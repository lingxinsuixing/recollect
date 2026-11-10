//! S4a1：TLS 地基的用例面。
//!
//! 覆盖三件事：身份文件的生命周期（生成/加载/半套不重建）、正确指纹能跑通一次
//! 请求、错误指纹与明文都**不能**拿到正常响应。

use rc_net::http::{serve_tls, Handler, Limits, Request, Response};
use rc_net::tls::{self, TlsError};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);
const SHORT: &str = "0123456789abcdef";

struct Echo;

impl Handler for Echo {
    fn handle(&mut self, req: &Request) -> Response {
        Response::bytes(
            200,
            "text/plain",
            format!("{} {}", req.method, req.target).into_bytes(),
        )
    }
}

struct TlsServer {
    addr: SocketAddr,
    pin: String,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl TlsServer {
    fn start() -> TlsServer {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let id = tls::ensure_identity(dir.path(), addr).unwrap();
        let pin = id.fingerprint.clone();
        let config = tls::server_config(&id).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let join = std::thread::spawn(move || {
            let mut handler = Echo;
            serve_tls(listener, &mut handler, &stop2, Limits::default(), 0, config);
        });
        TlsServer { addr, pin, stop, join: Some(join) }
    }
}

impl Drop for TlsServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // 唤醒 accept：这条连接会在 `stop` 检查处被丢掉（不会进握手）。
        let _ = TcpStream::connect(self.addr);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// 一个测试自己的请求：写一行、读回状态行 + body。
fn get(addr: SocketAddr, pin: &str, target: &str) -> Result<(String, Vec<u8>), TlsError> {
    let mut stream = tls::connect(addr, pin, TIMEOUT)?;
    let req = format!("GET {target} HTTP/1.1\r\nHost: recollect-hub\r\n\r\n");
    stream.write_all(req.as_bytes()).map_err(|_| TlsError::Io)?;
    stream.flush().map_err(|_| TlsError::Io)?;
    let (head, body) = read_response(&mut stream);
    Ok((head, body))
}

/// 按 `Content-Length` 精确读：TLS 层没有 `close_notify` 时不能用 `read_to_end`
/// 判断"响应读完了"。
fn read_response<R: Read>(stream: &mut R) -> (String, Vec<u8>) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
        assert!(head.len() < 64 * 1024, "response head is too large");
    }
    let text = String::from_utf8(head).unwrap();
    let len = text
        .split("\r\n")
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .map(|v| v.parse::<usize>().unwrap())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).unwrap();
    (text, body)
}

#[test]
fn ensure_identity_generates_then_reloads() {
    let dir = tempfile::tempdir().unwrap();
    let bind: SocketAddr = "127.0.0.1:7420".parse().unwrap();

    let first = tls::ensure_identity(dir.path(), bind).unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    assert!(cert_path.is_file(), "首次调用必须落 cert.pem");
    assert!(key_path.is_file(), "首次调用必须落 key.pem");
    assert_eq!(first.fingerprint, tls::fingerprint_der(&first.cert_der));

    let second = tls::ensure_identity(dir.path(), bind).unwrap();
    assert_eq!(first.fingerprint, second.fingerprint, "二次调用指纹必须不变");
    assert_eq!(first.cert_der.as_ref(), second.cert_der.as_ref());

    // 两文件都是 PEM，且能被 rustls-pemfile 读回同一张证书。
    let cert_pem = std::fs::read(&cert_path).unwrap();
    assert!(String::from_utf8_lossy(&cert_pem).contains("BEGIN CERTIFICATE"));
    let mut cursor = std::io::Cursor::new(&cert_pem[..]);
    let certs: Vec<_> = rustls_pemfile::certs(&mut cursor)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(certs.len(), 1);
    assert_eq!(certs[0].as_ref(), first.cert_der.as_ref());

    let key_pem = std::fs::read(&key_path).unwrap();
    let mut cursor = std::io::Cursor::new(&key_pem[..]);
    assert!(rustls_pemfile::private_key(&mut cursor).unwrap().is_some());
}

#[test]
fn half_identity_is_reported_and_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let bind: SocketAddr = "127.0.0.1:7420".parse().unwrap();
    tls::ensure_identity(dir.path(), bind).unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    let cert_before = std::fs::read(&cert_path).unwrap();

    std::fs::remove_file(&key_path).unwrap();
    assert_eq!(tls::ensure_identity(dir.path(), bind).unwrap_err(), TlsError::Key);
    assert_eq!(std::fs::read(&cert_path).unwrap(), cert_before, "已有 cert 不许被覆盖");
    assert!(!key_path.exists(), "半套不许静默重建");

    // 反向的一半：只有 key 没有 cert ⇒ Cert。
    let dir2 = tempfile::tempdir().unwrap();
    tls::ensure_identity(dir2.path(), bind).unwrap();
    std::fs::remove_file(dir2.path().join("cert.pem")).unwrap();
    assert_eq!(
        tls::ensure_identity(dir2.path(), bind).unwrap_err(),
        TlsError::Cert
    );
    assert!(!dir2.path().join("cert.pem").exists());
}

#[test]
fn correct_pin_round_trips_a_request() {
    let server = TlsServer::start();
    let (head, body) = get(server.addr, &server.pin, "/v1/status?x=1").unwrap();
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "head: {head}");
    assert_eq!(body, b"GET /v1/status?x=1".to_vec());
}

#[test]
fn wrong_pin_fails_without_echoing_material() {
    let server = TlsServer::start();
    let wrong = "0".repeat(64);
    assert_ne!(wrong, server.pin);
    let err = match tls::connect(server.addr, &wrong, TIMEOUT) {
        Ok(_) => panic!("错误指纹不许握手成功"),
        Err(e) => e,
    };
    assert_eq!(err, TlsError::Pin);
    let msg = err.to_string();
    assert_eq!(msg, "tls certificate does not match the pinned fingerprint");
    // 静态短语：没有 PEM 头、没有 base64 尾巴、没有路径。
    assert!(!msg.contains("BEGIN"), "msg: {msg}");
    assert!(!msg.contains("MII"), "msg: {msg}");
    assert!(msg.len() < 128, "msg: {msg}");

    // 错指纹的那台机器没把服务端带坏：正常指纹仍然能通。
    let (head, _) = get(server.addr, &server.pin, "/v1/status").unwrap();
    assert!(head.starts_with("HTTP/1.1 200 OK"));
}

#[test]
fn plaintext_on_the_tls_port_gets_no_http_response() {
    let server = TlsServer::start();
    let mut plain = TcpStream::connect(server.addr).unwrap();
    plain.set_read_timeout(Some(TIMEOUT)).unwrap();
    plain
        .write_all(b"GET /v1/status HTTP/1.1\r\nHost: recollect-hub\r\n\r\n")
        .unwrap();
    let mut out = Vec::new();
    // 服务端可以一个字节都不回，也可以回一个 TLS alert；两种都算"没给正常响应"。
    let _ = plain.read_to_end(&mut out);
    assert!(
        !out.windows(8).any(|w| w == b"HTTP/1.1"),
        "明文打 TLS 端口拿到了 HTTP 响应: {:?}",
        String::from_utf8_lossy(&out)
    );

    // accept 循环没退出：紧接着的正常连接仍然服务。
    let (head, body) = get(server.addr, &server.pin, "/after-plaintext").unwrap();
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"));
    assert_eq!(body, b"GET /after-plaintext".to_vec());
}

#[test]
fn empty_pin_and_short_fingerprint() {
    assert_eq!(tls::client_config("").unwrap_err(), TlsError::EmptyPin);
    assert_eq!(tls::client_config("   ").unwrap_err(), TlsError::EmptyPin);
    // 非 64 hex 的 pin 是配置错误，不是"跳过校验"。
    assert_eq!(tls::client_config("abc").unwrap_err(), TlsError::Pin);

    let fp = SHORT.repeat(4);
    assert_eq!(fp.len(), 64);
    assert_eq!(tls::short_fingerprint(&fp), SHORT);
    assert_eq!(tls::short_fingerprint("abcd").as_str(), "abcd");
}

#[test]
fn generated_der_parses_and_carries_the_bind_ip() {
    let dir = tempfile::tempdir().unwrap();
    let bind: SocketAddr = "192.0.2.7:7420".parse().unwrap();
    let id = tls::ensure_identity(dir.path(), bind).unwrap();

    // 结构上真能被 webpki 解析（rustls 自己用的那套）。
    rustls::server::ParsedCertificate::try_from(&id.cert_der).unwrap();

    assert_eq!(id.fingerprint.len(), 64);
    assert!(
        id.fingerprint
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "指纹必须是小写 hex: {}",
        id.fingerprint
    );

    let der = id.cert_der.as_ref();
    // SAN 扩展的 OID：2.5.29.17（06 03 55 1D 11）。
    assert!(
        der.windows(5).any(|w| w == &b"\x06\x03\x55\x1d\x11"[..]),
        "证书里没有 subjectAltName 扩展"
    );
    // iPAddress GeneralName：context tag 7 + 长度 4 + 地址四字节。
    assert!(
        der.windows(6).any(|w| w == &[0x87, 0x04, 192, 0, 2, 7][..]),
        "SAN 里没有 bind.ip()"
    );
    assert!(
        der.windows(tls::SERVER_NAME.len())
            .any(|w| w == tls::SERVER_NAME.as_bytes()),
        "CN/SAN 里没有固定名"
    );
}
