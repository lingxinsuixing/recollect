//! TLS 传输层的身份与配置（S4a1）。
//!
//! 分工：TLS 只负责"链路不被旁听、不被改道"；**身份**由本模块的指纹钉死
//! 与既有的请求签名共同承担。所以这里不接公共 CA、不做主机名校验——
//! 终端证书的 sha256 就是中心在网络上的名字。
//!
//! 私钥与设备签名密钥是两把：TLS 私钥只用来握手，丢了就重新生成一对证书，
//! 不影响任何已发出的 token 与签名。

use crate::protocol::sha256_hex;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, Error, ServerConfig};
use rustls::{ClientConnection, SignatureScheme, StreamOwned};
use std::fs;
use std::io::{Cursor, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// 证书文件名。目录由调用方给（hub 侧是 `<data-dir>/tls`）。
const CERT_FILE: &str = "cert.pem";
const KEY_FILE: &str = "key.pem";
/// 自签证书的 CN 与唯一 DNS SAN。客户端 `ServerName` 用同一个固定串——
/// 身份不靠主机名，但 rustls 需要一个名字来搭 SNI 与签名上下文。
pub const SERVER_NAME: &str = "recollect-hub";

/// 错误面全静态短语：路径、证书正文、指纹一个字节都不进来（守卫 12）。
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TlsError {
    #[error("tls io")]
    Io,
    #[error("tls certificate is unusable")]
    Cert,
    #[error("tls private key is unusable")]
    Key,
    #[error("tls certificate does not match the pinned fingerprint")]
    Pin,
    #[error("tls pinned fingerprint is empty")]
    EmptyPin,
}

pub struct TlsIdentity {
    pub cert_der: CertificateDer<'static>,
    pub key_der: PrivateKeyDer<'static>,
    /// sha256(cert_der)，小写 64 hex。钉死与展示都用它。
    pub fingerprint: String,
}

impl std::fmt::Debug for TlsIdentity {
    /// 私钥不进 Debug。证书 DER 也不打——错误报告里出现它没有用处。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsIdentity")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

/// `<dir>/cert.pem` + `<dir>/key.pem`：都在就加载；都不在就生成；只在一半就是错误。
///
/// **半套不静默重建**：已经钉过旧指纹的节点会永远连不上，而"证书换了"
/// 这件事必须由人看见（`hub tls show`），不能由一次启动顺手抹掉。
pub fn ensure_identity(dir: &Path, bind: SocketAddr) -> Result<TlsIdentity, TlsError> {
    let cert_path = dir.join(CERT_FILE);
    let key_path = dir.join(KEY_FILE);
    match (cert_path.is_file(), key_path.is_file()) {
        (true, true) => load_identity(&cert_path, &key_path),
        (false, false) => generate_identity(dir, bind),
        (false, true) => Err(TlsError::Cert),
        (true, false) => Err(TlsError::Key),
    }
}

fn load_identity(cert_path: &Path, key_path: &Path) -> Result<TlsIdentity, TlsError> {
    let cert_pem = fs::read(cert_path).map_err(|_| TlsError::Cert)?;
    let key_pem = fs::read(key_path).map_err(|_| TlsError::Key)?;
    parse_identity(&cert_pem, &key_pem)
}

fn parse_identity(cert_pem: &[u8], key_pem: &[u8]) -> Result<TlsIdentity, TlsError> {
    let cert_der = parse_cert(cert_pem)?;
    let key_der = rustls_pemfile::private_key(&mut Cursor::new(key_pem))
        .map_err(|_| TlsError::Key)?
        .ok_or(TlsError::Key)?;
    let fingerprint = fingerprint_der(&cert_der);
    Ok(TlsIdentity { cert_der, key_der, fingerprint })
}

/// 证书 PEM → DER。自签链只有一张；多张意味着文件被人工改过，按"读不懂"处理。
/// `parse_identity` 与 [`fingerprint_at`] 共用这一份：两条路对"什么算一张好证书"
/// 的答案必须相同，否则 `tls show` 的指纹与票里的指纹会对不上。
fn parse_cert(cert_pem: &[u8]) -> Result<CertificateDer<'static>, TlsError> {
    let mut cursor = Cursor::new(cert_pem);
    let mut certs = rustls_pemfile::certs(&mut cursor);
    let cert_der = certs.next().ok_or(TlsError::Cert)?.map_err(|_| TlsError::Cert)?;
    if certs.next().is_some() {
        return Err(TlsError::Cert);
    }
    Ok(cert_der)
}

/// 读 `<dir>/cert.pem` 并返回 DER 的 sha256 hex（与 `ensure_identity` 同公式）。
///
/// **只读不生成**：没有证书是一件运维事实（先跑一次 TLS serve），
/// 顺手生成一对会让"指纹是什么"由一次查询决定，而它本该由证书决定。
pub fn fingerprint_at(dir: &Path) -> Result<String, TlsError> {
    let cert_pem = fs::read(dir.join(CERT_FILE)).map_err(|_| TlsError::Cert)?;
    Ok(fingerprint_der(&parse_cert(&cert_pem)?))
}

/// 生成一对自签证书：CN=`recollect-hub`，SAN 含固定 DNS 名与 `bind.ip()`。
///
/// 密钥算法用 rcgen 的默认（ECDSA P-256，PKCS#8）；有效期也用 rcgen 默认。
/// 不做 `tls rotate` 的双钉窗口——那是后续切片的事。
fn generate_identity(dir: &Path, bind: SocketAddr) -> Result<TlsIdentity, TlsError> {
    fs::create_dir_all(dir).map_err(|_| TlsError::Io)?;
    let mut params = rcgen::CertificateParams::default();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, SERVER_NAME);
    let dns = rcgen::Ia5String::try_from(SERVER_NAME).map_err(|_| TlsError::Cert)?;
    params.subject_alt_names.push(rcgen::SanType::DnsName(dns));
    // SAN 里的 IP 是给人看的第二重线索（真正钉的是指纹）。
    params.subject_alt_names.push(rcgen::SanType::IpAddress(bind.ip()));
    let key_pair = rcgen::KeyPair::generate().map_err(|_| TlsError::Key)?;
    let cert = params.self_signed(&key_pair).map_err(|_| TlsError::Cert)?;
    write_atomic(&dir.join(CERT_FILE), cert.pem().as_bytes(), false)?;
    write_atomic(&dir.join(KEY_FILE), key_pair.serialize_pem().as_bytes(), true)?;
    // 回读而不是直接用内存里的 DER：写盘这一环也要证明是对的，
    // 而且二次调用走的正是加载路径，指纹当然一致。
    load_identity(&dir.join(CERT_FILE), &dir.join(KEY_FILE))
}

/// 临时文件 + rename：半个文件不能被下一次启动当成"已有证书"。
/// Unix 上私钥 0600（证书 0644，指纹本来就要给人看）。
fn write_atomic(path: &Path, bytes: &[u8], private: bool) -> Result<(), TlsError> {
    let tmp = temp_path(path)?;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(if private { 0o600 } else { 0o644 });
    }
    let _ = private;
    let written = (|| -> std::io::Result<()> {
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
        return Err(TlsError::Io);
    }
    fs::rename(&tmp, path).map_err(|_| {
        let _ = fs::remove_file(&tmp);
        TlsError::Io
    })
}

fn temp_path(path: &Path) -> Result<PathBuf, TlsError> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path.file_name().and_then(|s| s.to_str()).ok_or(TlsError::Io)?;
    let tmp = format!(".{name}.{}.{n}.tmp", std::process::id());
    Ok(path.with_file_name(tmp))
}

/// sha256(cert DER)，小写 64 hex。
pub fn fingerprint_der(cert: &[u8]) -> String {
    sha256_hex(cert)
}

/// 展示用的短指纹：前 16 个 hex 字符。短于 16 就原样返回（不 panic）。
pub fn short_fingerprint(fp: &str) -> String {
    fp.chars().take(16).collect()
}

pub fn server_config(id: &TlsIdentity) -> Result<Arc<ServerConfig>, TlsError> {
    let cfg = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|_| TlsError::Cert)?
        .with_no_client_auth()
        .with_single_cert(vec![id.cert_der.clone()], id.key_der.clone_key())
        .map_err(|_| TlsError::Cert)?;
    Ok(Arc::new(cfg))
}

/// 空 pin 是配置错误，不是"不校验"——**没有 pin 就没有客户端**。
pub fn client_config(pinned_fp: &str) -> Result<Arc<ClientConfig>, TlsError> {
    let pin = normalize_pin(pinned_fp)?;
    let cfg = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|_| TlsError::Pin)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier { pin }))
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

/// pin 是外部输入（配置、票），先归一再比：64 个 hex，大小写不敏感。
fn normalize_pin(raw: &str) -> Result<String, TlsError> {
    let pin = raw.trim();
    if pin.is_empty() {
        return Err(TlsError::EmptyPin);
    }
    if pin.len() != 64 || !pin.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(TlsError::Pin);
    }
    Ok(pin.to_ascii_lowercase())
}

/// 建 TCP + TLS 连接，**握手在这里走完**：错指纹必须在 `connect` 就失败，
/// 而不是拖到第一次读请求时才暴露。`ServerName` 用固定串，不做主机名校验。
pub fn connect(
    addr: SocketAddr,
    pinned_fp: &str,
    timeout: Duration,
) -> Result<StreamOwned<ClientConnection, TcpStream>, TlsError> {
    let config = client_config(pinned_fp)?;
    let name = ServerName::try_from(SERVER_NAME).map_err(|_| TlsError::Cert)?;
    let mut conn = ClientConnection::new(config, name).map_err(|_| TlsError::Cert)?;
    let mut sock = TcpStream::connect_timeout(&addr, timeout).map_err(|_| TlsError::Io)?;
    sock.set_read_timeout(Some(timeout)).map_err(|_| TlsError::Io)?;
    sock.set_write_timeout(Some(timeout)).map_err(|_| TlsError::Io)?;
    while conn.is_handshaking() {
        conn.complete_io(&mut sock).map_err(|e| {
            // rustls 把"证书被本地拒了"折成 InvalidData：那是钉死失败，
            // 其余（EOF、超时、reset）都是链路问题。
            if e.kind() == std::io::ErrorKind::InvalidData {
                TlsError::Pin
            } else {
                TlsError::Io
            }
        })?;
    }
    Ok(StreamOwned::new(conn, sock))
}

#[derive(Debug)]
struct PinnedVerifier {
    pin: String,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        if fingerprint_der(end_entity) == self.pin {
            return Ok(ServerCertVerified::assertion());
        }
        // 不回显指纹或证文：错误面只说明"不是这枚"。
        Err(Error::InvalidCertificate(
            CertificateError::ApplicationVerificationFailure,
        ))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// 只装一次：`ring` 的 provider 建一次就够，每次握手重建会白拷一堆套件表。
fn provider() -> Arc<CryptoProvider> {
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| Arc::new(rustls::crypto::ring::default_provider()))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> SocketAddr {
        "127.0.0.1:7420".parse().unwrap()
    }

    /// 票里的指纹与 `tls show`/握手用的指纹必须**逐字节**相同：
    /// 两条路各读一次 PEM，公式只能有一份。
    #[test]
    fn fingerprint_at_matches_ensure_identity() {
        let dir = tempfile::tempdir().unwrap();
        let id = ensure_identity(dir.path(), addr()).unwrap();
        assert_eq!(fingerprint_at(dir.path()).unwrap(), id.fingerprint);
        assert_eq!(fingerprint_at(dir.path()).unwrap().len(), 64);
    }

    /// 没有证书是运维事实，不是"顺手生成一对"的理由：目录得原样空着。
    #[test]
    fn fingerprint_at_never_generates_a_certificate() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(fingerprint_at(dir.path()), Err(TlsError::Cert)));
        assert!(!dir.path().join(CERT_FILE).exists());
    }

    #[test]
    fn fingerprint_at_rejects_an_unreadable_certificate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CERT_FILE), b"-----BEGIN CERTIFICATE-----\n").unwrap();
        assert!(matches!(fingerprint_at(dir.path()), Err(TlsError::Cert)));
    }
}
