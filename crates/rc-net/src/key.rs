use crate::error::NetError;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use std::path::Path;

const HUB_HEADER: &str = "recollect-key-v1";
const DEVICE_HEADER: &str = "recollect-device-key-v1";

pub struct HubKey {
    signing: SigningKey,
}

impl HubKey {
    /// 首次启动生成并落盘；已有文件则原样读回。**不覆盖、不轮换**：
    /// 中心换钥匙 = 所有节点验签失败，那是一次显式的阶段 5 动作，不是这里的兜底。
    pub fn load_or_create(path: &Path) -> Result<HubKey, NetError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut seed = [0u8; 32];
                getrandom::fill(&mut seed).map_err(|_| NetError::NoEntropy)?;
                let key = HubKey { signing: SigningKey::from_bytes(&seed) };
                let body = format!("{HUB_HEADER}\n{}\n", URL_SAFE_NO_PAD.encode(seed));
                write_private(path, body.as_bytes())?;
                Ok(key)
            }
            Err(e) => Err(NetError::Io(e.kind())),
        }
    }

    /// 只读加载：文件不在就是错误，**绝不新建**。
    ///
    /// 发现线程用它：它在 serve 已经读过钥匙之后运行，此刻"文件不在"只可能是有人在动
    /// 磁盘。这里若走 `load_or_create`，一次读失败就会凭空写出一把新钥匙，把全网节点钉住的
    /// `hub_pub` 一次性打碎——凭空造身份比发现失效坏得多。
    pub fn load(path: &Path) -> Result<HubKey, NetError> {
        let text = std::fs::read_to_string(path).map_err(|_| NetError::KeyFileUnreadable)?;
        Self::parse(&text)
    }

    fn parse(text: &str) -> Result<HubKey, NetError> {
        let mut lines = text.lines();
        if lines.next() != Some(HUB_HEADER) {
            return Err(NetError::KeyFileMalformed);
        }
        let seed = lines.next().ok_or(NetError::KeyFileMalformed)?;
        if lines.next().is_some() {
            return Err(NetError::KeyFileMalformed);
        }
        let bytes = URL_SAFE_NO_PAD.decode(seed).map_err(|_| NetError::KeyFileMalformed)?;
        let seed: [u8; 32] = bytes.try_into().map_err(|_| NetError::KeyFileMalformed)?;
        Ok(HubKey { signing: SigningKey::from_bytes(&seed) })
    }

    pub fn public_b64(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.signing.verifying_key().to_bytes())
    }

    pub fn sign_b64(&self, msg: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(self.signing.sign(msg).to_bytes())
    }
}

pub struct DeviceKey {
    signing: SigningKey,
}

impl DeviceKey {
    pub fn generate() -> Result<DeviceKey, NetError> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|_| NetError::NoEntropy)?;
        Ok(DeviceKey { signing: SigningKey::from_bytes(&seed) })
    }

    pub fn load(path: &Path) -> Result<DeviceKey, NetError> {
        let text = std::fs::read_to_string(path).map_err(|_| NetError::KeyFileUnreadable)?;
        let mut lines = text.lines();
        if lines.next() != Some(DEVICE_HEADER) {
            return Err(NetError::KeyFileMalformed);
        }
        let seed = lines.next().ok_or(NetError::KeyFileMalformed)?;
        if lines.next().is_some() {
            return Err(NetError::KeyFileMalformed);
        }
        let bytes = URL_SAFE_NO_PAD.decode(seed).map_err(|_| NetError::KeyFileMalformed)?;
        let seed: [u8; 32] = bytes.try_into().map_err(|_| NetError::KeyFileMalformed)?;
        Ok(DeviceKey { signing: SigningKey::from_bytes(&seed) })
    }

    pub fn save(&self, path: &Path) -> Result<(), NetError> {
        let body = format!(
            "{DEVICE_HEADER}\n{}\n",
            URL_SAFE_NO_PAD.encode(self.signing.to_bytes())
        );
        write_private(path, body.as_bytes())
    }

    /// 设备 id 由公钥导出，不单独存：换钥匙 = 换 id，两者不可能漂。
    /// 推导规则只有 [`device_id_of`] 一处；这里失败的唯一原因是自己的公钥编不成 base64，
    /// 而那在构造上不可能，所以 `expect` 不是对输入的赌。
    pub fn device_id(&self) -> String {
        device_id_of(&self.public_b64()).expect("own public key is always decodable")
    }

    pub fn public_b64(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.signing.verifying_key().to_bytes())
    }

    pub fn sign_b64(&self, msg: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(self.signing.sign(msg).to_bytes())
    }
}

/// 设备 id 的**唯一**导出规则。公钥必须恰好是 32 字节的 base64url：
/// 配对照表（`hub pair`）用它重算并和请求里的 `device_id` 对拍，因此它必须在
/// `rc-net` 里只有一份，不许 hub 再抄一遍 `sha256` 截断。
pub fn device_id_of(pubkey_b64: &str) -> Option<String> {
    let pk = URL_SAFE_NO_PAD.decode(pubkey_b64).ok()?;
    let pk: [u8; 32] = pk.try_into().ok()?;
    let h = crate::protocol::sha256_hex(&pk);
    Some(format!("dev-{}", &h[..16]))
}

/// 只验签，不泄露失败原因：调用方要区分"没身份"与"身份不对"时用 HTTP 码，不用错误串。
pub fn verify_b64(pubkey_b64: &str, msg: &[u8], sig_b64: &str) -> bool {
    let Ok(pk) = URL_SAFE_NO_PAD.decode(pubkey_b64) else { return false };
    let Ok(pk): Result<[u8; 32], _> = pk.try_into() else { return false };
    let Ok(vk) = VerifyingKey::from_bytes(&pk) else { return false };
    let Ok(sig) = URL_SAFE_NO_PAD.decode(sig_b64) else { return false };
    let Ok(sig): Result<[u8; 64], _> = sig.try_into() else { return false };
    vk.verify_strict(msg, &Signature::from_bytes(&sig)).is_ok()
}

/// 先写临时文件、再改权限、最后 rename。顺序不能反：先 rename 再改权限会留下
/// 一个"已经叫最终名字、但权限还没收紧"的窗口。
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), NetError> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| NetError::Io(e.kind()))?;
    restrict_to_owner(&tmp)?;
    std::fs::rename(&tmp, path).map_err(|e| NetError::Io(e.kind()))?;
    Ok(())
}

/// 把一段**私密**字节写到盘上，权限收到只有本人可读。
/// token 文件走这一条：它与私钥同级的敏感物，权限没收紧比没有更坏。
/// 顺序与 `write_private` 相同（临时名 → 收权限 → rename），失败即响亮失败。
pub fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), NetError> {
    write_private(path, bytes)
}

#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> Result<(), NetError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| NetError::Io(e.kind()))
}

/// Windows 用 `icacls`，零新依赖；`CREATE_NO_WINDOW` 守"运行期不弹前台控制台"。
/// 失败即响亮失败——一个权限没设上的私钥文件比没有私钥更坏。
#[cfg(windows)]
fn restrict_to_owner(path: &Path) -> Result<(), NetError> {
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let user = std::env::var("USERNAME").map_err(|_| NetError::KeyFileMalformed)?;
    let status = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r", &format!("{user}:F")])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| NetError::Io(e.kind()))?;
    if !status.success() {
        return Err(NetError::KeyFileMalformed);
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn restrict_to_owner(_path: &Path) -> Result<(), NetError> {
    Ok(())
}
