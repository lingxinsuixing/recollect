//! 可同步的密文箱：`secrets.enc` 的文件格式、Argon2id 派生与 ChaCha20-Poly1305。
//!
//! 不变式：**值只经过不留下**。箱在打开期间持有值（`Zeroizing`），调用方拿到后应只传给
//! 执行面（`Live` 座位 / 节点自蒸），不写进配置、日志、返回值；关闭即零化。
//! 文件里没有明文、没有口令；口令由调用方从受 ACL 保护的口令文件读入。

pub mod backup;

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;
use zeroize::Zeroizing;

pub const MAGIC: &[u8] = b"RECOLLECT-SECRETS\x01";
const MAGIC_LEN: usize = MAGIC.len();
const PARAM_LEN: usize = 12;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const HEADER_PREFIX: usize = MAGIC_LEN + PARAM_LEN + SALT_LEN;
const HEADER_LEN: usize = HEADER_PREFIX + NONCE_LEN;

const DEFAULT_M_COST: u32 = 19_456;
const DEFAULT_T_COST: u32 = 2;
const DEFAULT_P_COST: u32 = 1;

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("secret box io: {0:?}")]
    Io(std::io::ErrorKind),
    #[error("the secret box has an unsupported format")]
    BadFormat,
    #[error("the secret box cannot be opened (wrong passphrase or tampered file)")]
    OpenFailed,
    #[error("a secret box already exists")]
    AlreadyExists,
    #[error("the slot name is not a safe token")]
    BadSlot,
    #[error("the KDF parameters are invalid")]
    BadParams,
    #[error("the private-file permission could not be set")]
    Permission,
    /// 读箱约定里的"文件不在"：`.enc` 缺席。与 `Io(NotFound)` 分开是为了让调用方
    /// 说得出"箱还没建"与"箱读不动"是两件事（前者请操作员跑 doctor，后者是故障）。
    #[error("the secret box file does not exist")]
    BoxMissing,
    /// 读箱约定里的"文件不在"：`.pass` 缺席（箱在、口令文件没落）。
    #[error("the secret box passphrase file does not exist")]
    PassMissing,
    /// 箱开得了、格不在。与 `BadSlot`（格名非法）是两件事。
    #[error("the secret box holds no such slot")]
    MissingSlot,
    /// 箱名是文件名 stem：必须是 safe token 且不含 `:`（Windows 文件名不许冒号）。
    #[error("the secret box name is not a safe file stem")]
    BadBox,
    /// 备份 key 文件损坏/长度不对。
    #[error("the backup key file is malformed")]
    BackupKey,
    /// 备份容器头损坏/分块长度不对。
    #[error("the backup file is malformed")]
    BackupFormat,
    /// 备份容器没有通过认证：错 key 或密文被改。
    #[error("the backup file did not authenticate")]
    BackupAuth,
}

/// 一个打开着的箱。`slots` 里的值在 drop 时零化。
pub struct SecretsBox {
    path: PathBuf,
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
    salt: [u8; SALT_LEN],
    nonce: [u8; NONCE_LEN],
    key: Zeroizing<[u8; KEY_LEN]>,
    slots: BTreeMap<String, Zeroizing<String>>,
}

impl SecretsBox {
    /// 新建空箱；已存在 ⇒ `AlreadyExists`（不覆盖）。
    pub fn create(path: &Path, passphrase: &[u8]) -> Result<Self, SecretError> {
        if path.exists() {
            return Err(SecretError::AlreadyExists);
        }
        let mut salt = [0u8; SALT_LEN];
        getrandom::fill(&mut salt).map_err(|_| SecretError::Io(std::io::ErrorKind::Other))?;
        let params = derive_params(DEFAULT_M_COST, DEFAULT_T_COST, DEFAULT_P_COST)?;
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(passphrase, &salt, &mut *key)
            .map_err(|_| SecretError::BadParams)?;
        let mut b = SecretsBox {
            path: path.to_path_buf(),
            m_cost: DEFAULT_M_COST,
            t_cost: DEFAULT_T_COST,
            p_cost: DEFAULT_P_COST,
            salt,
            nonce: [0u8; NONCE_LEN],
            key,
            slots: BTreeMap::new(),
        };
        b.save()?;
        Ok(b)
    }

    /// 用口令打开；错口令与篡改都归 `OpenFailed`（AEAD 认证失败无法区分两者）。
    pub fn open(path: &Path, passphrase: &[u8]) -> Result<Self, SecretError> {
        let bytes = std::fs::read(path).map_err(|e| SecretError::Io(e.kind()))?;
        if bytes.len() < HEADER_LEN || &bytes[..MAGIC_LEN] != MAGIC {
            return Err(SecretError::BadFormat);
        }
        let m_cost = u32::from_le_bytes(bytes[MAGIC_LEN..MAGIC_LEN + 4].try_into().unwrap());
        let t_cost =
            u32::from_le_bytes(bytes[MAGIC_LEN + 4..MAGIC_LEN + 8].try_into().unwrap());
        let p_cost =
            u32::from_le_bytes(bytes[MAGIC_LEN + 8..MAGIC_LEN + 12].try_into().unwrap());
        let salt: [u8; SALT_LEN] = bytes[MAGIC_LEN + PARAM_LEN..HEADER_PREFIX]
            .try_into()
            .map_err(|_| SecretError::BadFormat)?;
        let nonce: [u8; NONCE_LEN] = bytes[HEADER_PREFIX..HEADER_LEN]
            .try_into()
            .map_err(|_| SecretError::BadFormat)?;
        let params = derive_params(m_cost, t_cost, p_cost)?;
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(passphrase, &salt, &mut *key)
            .map_err(|_| SecretError::BadParams)?;
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&*key));
        let plain = cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload { msg: &bytes[HEADER_LEN..], aad: &bytes[..HEADER_PREFIX] },
            )
            .map_err(|_| SecretError::OpenFailed)?;
        let plain = Zeroizing::new(plain);
        let parsed: BTreeMap<String, String> =
            serde_json::from_slice(&plain).map_err(|_| SecretError::BadFormat)?;
        let slots = parsed
            .into_iter()
            .map(|(k, v)| (k, Zeroizing::new(v)))
            .collect();
        Ok(SecretsBox {
            path: path.to_path_buf(),
            m_cost,
            t_cost,
            p_cost,
            salt,
            nonce,
            key,
            slots,
        })
    }

    pub fn get(&self, slot: &str) -> Option<&str> {
        self.slots.get(slot).map(|v| v.as_str())
    }

    pub fn list(&self) -> Vec<&str> {
        self.slots.keys().map(String::as_str).collect()
    }

    /// 写入并立即保存；slot 必须是 safe token。
    pub fn put(&mut self, slot: &str, value: &str) -> Result<(), SecretError> {
        if rc_contract::safe_token(slot) != slot {
            return Err(SecretError::BadSlot);
        }
        self.slots.insert(slot.to_owned(), Zeroizing::new(value.to_owned()));
        self.save()
    }

    /// 删除并保存；返回原本是否存在。
    pub fn remove(&mut self, slot: &str) -> Result<bool, SecretError> {
        let existed = self.slots.remove(slot).is_some();
        if existed {
            self.save()?;
        }
        Ok(existed)
    }

    fn aad(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(HEADER_PREFIX);
        v.extend_from_slice(MAGIC);
        v.extend_from_slice(&self.m_cost.to_le_bytes());
        v.extend_from_slice(&self.t_cost.to_le_bytes());
        v.extend_from_slice(&self.p_cost.to_le_bytes());
        v.extend_from_slice(&self.salt);
        v
    }

    fn save(&mut self) -> Result<(), SecretError> {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(|_| SecretError::Io(std::io::ErrorKind::Other))?;
        self.nonce = nonce;
        let aad = self.aad();
        let plain: BTreeMap<&str, &str> =
            self.slots.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let plain_bytes =
            Zeroizing::new(serde_json::to_vec(&plain).map_err(|_| SecretError::BadFormat)?);
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&*self.key));
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&self.nonce),
                Payload { msg: &plain_bytes, aad: &aad },
            )
            .map_err(|_| SecretError::Io(std::io::ErrorKind::Other))?;
        let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
        out.extend_from_slice(&aad);
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&ciphertext);
        let tmp = self.path.with_extension("enc.tmp");
        write_private(&tmp, &out)?;
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(SecretError::Io(e.kind()));
        }
        Ok(())
    }
}

/// 私有落盘的唯一公开入口：口令文件这类"只有本账户可读"的文件也走这里，
/// 让 Unix 的 0600 与 Windows 的 ACL 只存在一份实现（调用方不许另写一套权限规则）。
pub fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), SecretError> {
    write_private(path, bytes)
}

/// `<data_dir>/<box>.enc` + `<data_dir>/<box>.pass` 的约定读法。
///
/// 箱名是文件名 stem：必须是 safe token 且不含 `:`（Windows 文件名不许冒号），否则 `BadBox`；
/// 格名过 safe token，否则 `BadSlot`。两个文件都用 `is_file()` 先判存在性——`.enc` 缺是
/// `BoxMissing`、`.pass` 缺是 `PassMissing`，不把 `Io(NotFound)` 混进前者。
/// 口令文件按字节读成 UTF-8、只去掉一个行尾换行；`SecretsBox::open` 的错原样传
/// （错口令/篡改 = `OpenFailed`）。
///
/// 返回的 `Zeroizing<String>` 由调用方只交给执行面（`Live` 座位 / 节点自蒸），
/// 不写进配置、日志或返回值；与箱里的值同一份零化纪律。
pub fn read_secret(
    data_dir: &Path,
    box_name: &str,
    slot: &str,
) -> Result<Zeroizing<String>, SecretError> {
    if rc_contract::safe_token(box_name) != box_name || box_name.contains(':') {
        return Err(SecretError::BadBox);
    }
    if rc_contract::safe_token(slot) != slot {
        return Err(SecretError::BadSlot);
    }
    let enc = data_dir.join(format!("{box_name}.enc"));
    if !enc.is_file() {
        return Err(SecretError::BoxMissing);
    }
    let pass_path = data_dir.join(format!("{box_name}.pass"));
    if !pass_path.is_file() {
        return Err(SecretError::PassMissing);
    }
    let raw = std::fs::read(&pass_path).map_err(|e| SecretError::Io(e.kind()))?;
    let phrase = strip_line_ending(&raw);
    let phrase = std::str::from_utf8(phrase).map_err(|_| SecretError::BadFormat)?;
    let b = SecretsBox::open(&enc, phrase.as_bytes())?;
    match b.get(slot) {
        Some(value) => Ok(Zeroizing::new(value.to_owned())),
        None => Err(SecretError::MissingSlot),
    }
}

/// 恰好一个行尾换行；它前面若是 `\r`，属于同一个行尾（口令文件的写侧就是这么落的）。
fn strip_line_ending(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    if end > 0 && bytes[end - 1] == b'\n' {
        end -= 1;
        if end > 0 && bytes[end - 1] == b'\r' {
            end -= 1;
        }
    }
    &bytes[..end]
}

fn derive_params(m_cost: u32, t_cost: u32, p_cost: u32) -> Result<Params, SecretError> {
    Params::new(m_cost, t_cost, p_cost, Some(KEY_LEN)).map_err(|_| SecretError::BadParams)
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), SecretError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| SecretError::Io(e.kind()))?;
    f.write_all(bytes).map_err(|e| SecretError::Io(e.kind()))?;
    f.sync_all().map_err(|e| SecretError::Io(e.kind()))?;
    Ok(())
}

/// Windows 用 `icacls`（零新依赖）；`CREATE_NO_WINDOW` 守“运行期不弹前台控制台”。
#[cfg(windows)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), SecretError> {
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|e| SecretError::Io(e.kind()))?;
    f.write_all(bytes).map_err(|e| SecretError::Io(e.kind()))?;
    f.sync_all().map_err(|e| SecretError::Io(e.kind()))?;
    drop(f);
    let user = std::env::var("USERNAME").map_err(|_| SecretError::Permission)?;
    let grant = format!("{user}:F");
    let status = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r", grant.as_str()])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| SecretError::Io(e.kind()))?;
    if !status.success() {
        return Err(SecretError::Permission);
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn write_private(_path: &Path, _bytes: &[u8]) -> Result<(), SecretError> {
    Err(SecretError::Permission)
}
