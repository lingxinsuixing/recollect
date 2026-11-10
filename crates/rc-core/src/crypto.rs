//! 每 agent 独立密钥与载荷 AEAD。
//!
//! 三条不变式：
//! - 表名只由 `sha256(agent_id)` 的前 16 个 hex 派生，原始 `agent_id` 不进标识符；
//! - key 文件恰好 32B，权限走 `rc_secret::write_private_file`（Unix 0600 / Windows ACL 只此一份）；
//! - nonce 每条消息重新 `getrandom::fill`，24B（XChaCha20）；AAD 绑定 `agent_id|version|section`
//!   （载荷）与 `agent_id|notebook|id`（小本本行，W36）。

use crate::CoreError;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 24;
/// 表名后缀的 hex 长度：`sha256` 前 8 字节。
const SUFFIX_HEX_LEN: usize = 16;

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let mut s = String::with_capacity(64);
    for b in h.finalize() {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// `sha256(agent_id)` 的前 16 个 hex。
pub fn agent_table_suffix(agent_id: &str) -> String {
    let mut h = Sha256::new();
    h.update(agent_id.as_bytes());
    let digest = h.finalize();
    let mut s = String::with_capacity(SUFFIX_HEX_LEN);
    for b in digest.iter().take(SUFFIX_HEX_LEN / 2) {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 该 agent 的两张载荷表：`core_doc_<h>` / `core_rev_<h>`。
/// 第三张 per-agent 表（小本本 `core_note_<h>`）由 [`note_table_name`] 单独给出：
/// `table_names` 的二元组形状被注册表两列与既有测试钉住，这里不改签名。
pub fn table_names(agent_id: &str) -> (String, String) {
    let h = agent_table_suffix(agent_id);
    (format!("core_doc_{h}"), format!("core_rev_{h}"))
}

/// 该 agent 的小本本表：`core_note_<h>`（W36，append-only 的第三张 per-agent 动态表）。
pub fn note_table_name(agent_id: &str) -> String {
    format!("core_note_{}", agent_table_suffix(agent_id))
}

/// 动态表名的唯一闸：`core_doc_`/`core_rev_`/`core_note_` + 恰好 16 位小写 hex。
/// 等价于 `^core_(doc|rev|note)_[0-9a-f]{16}$`；SQL 里还须用双引号包裹（调用点在 `store`）。
pub fn is_valid_core_table(name: &str) -> bool {
    let Some(suffix) = name
        .strip_prefix("core_doc_")
        .or_else(|| name.strip_prefix("core_rev_"))
        .or_else(|| name.strip_prefix("core_note_"))
    else {
        return false;
    };
    suffix.len() == SUFFIX_HEX_LEN
        && suffix.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `<data_dir>/keys/core_<h>.key`。
pub fn key_path(data_dir: &Path, agent_id: &str) -> PathBuf {
    data_dir
        .join("keys")
        .join(format!("core_{}.key", agent_table_suffix(agent_id)))
}

/// 32B 会话密钥；drop 时零化，`Debug` 不打印内容。
pub struct AgentKey {
    key: Zeroizing<[u8; KEY_LEN]>,
}

impl AgentKey {
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        AgentKey { key: Zeroizing::new(bytes) }
    }

    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.key
    }
}

impl fmt::Debug for AgentKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AgentKey(<redacted>)")
    }
}

fn secret_error(e: rc_secret::SecretError) -> CoreError {
    match e {
        rc_secret::SecretError::Io(kind) => CoreError::Io(std::io::Error::from(kind)),
        _ => CoreError::BadKeyFile,
    }
}

/// 读已存在的 key 文件，**从不写文件**。缺失 ⇒ `KeyMissing`；不是恰好 32B ⇒ `BadKeyFile`。
fn read_key_file(path: &Path) -> Result<AgentKey, CoreError> {
    match std::fs::read(path) {
        Ok(bytes) => {
            if bytes.len() != KEY_LEN {
                return Err(CoreError::BadKeyFile);
            }
            let mut key = Zeroizing::new([0u8; KEY_LEN]);
            key.copy_from_slice(&bytes);
            Ok(AgentKey { key })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(CoreError::KeyMissing),
        Err(e) => Err(CoreError::Io(e)),
    }
}

/// 只加载已存在的 key（读写路径用）。缺失 ⇒ `KeyMissing`；绝不新造一把 key——
/// 那会让已有密文永久不可读，而错误却只在解密时以 `Crypto` 冒出来。
pub fn load_key(data_dir: &Path, agent_id: &str) -> Result<AgentKey, CoreError> {
    read_key_file(&key_path(data_dir, agent_id))
}

/// 读 key 文件；不存在则生成 32B 随机字节并用 `write_private_file` 落盘（仅注册路径用）。
/// 已有文件不是恰好 32B ⇒ `BadKeyFile`（不静默重建：那会把已加密的载荷变成永久不可读）。
pub fn load_or_create_key(data_dir: &Path, agent_id: &str) -> Result<AgentKey, CoreError> {
    let path = key_path(data_dir, agent_id);
    match read_key_file(&path) {
        Ok(key) => Ok(key),
        Err(CoreError::KeyMissing) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut key = Zeroizing::new([0u8; KEY_LEN]);
            getrandom::fill(key.as_mut_slice()).map_err(|_| CoreError::Crypto)?;
            rc_secret::write_private_file(&path, key.as_slice()).map_err(secret_error)?;
            Ok(AgentKey { key })
        }
        Err(e) => Err(e),
    }
}

/// AAD = `core|<agent_id>|<version>|<section>` 的字节。版本与分区都进 AAD：
/// 同一段密文换一个 `doc_version` 或换一个 `section` 读都必须 AEAD 失败。
pub fn aad(agent_id: &str, version: u32, section: &str) -> Vec<u8> {
    format!("core|{agent_id}|{version}|{section}").into_bytes()
}

/// 小本本一行的 AAD = `core|<agent_id>|notebook|<id>` 的字节（W36）。行级 AEAD 绑行身份：
/// 同一段密文换一个行 id 读必须 AEAD 失败（换 id 重放必失败）。
pub fn aad_notebook(agent_id: &str, id: &str) -> Vec<u8> {
    format!("core|{agent_id}|notebook|{id}").into_bytes()
}

/// 加密；nonce 每次重新取随机。
pub fn seal(
    key: &AgentKey,
    aad: &[u8],
    plaintext: &[u8],
) -> Result<([u8; NONCE_LEN], Vec<u8>), CoreError> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|_| CoreError::Crypto)?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key.as_bytes()));
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: plaintext, aad })
        .map_err(|_| CoreError::Crypto)?;
    Ok((nonce, ciphertext))
}

/// 解密；错 key、错 AAD 与任何篡改都归 `CoreError::Crypto`。
pub fn open(
    key: &AgentKey,
    aad: &[u8],
    nonce: &[u8; NONCE_LEN],
    ciphertext: &[u8],
) -> Result<Vec<u8>, CoreError> {
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key.as_bytes()));
    cipher
        .decrypt(XNonce::from_slice(nonce), Payload { msg: ciphertext, aad })
        .map_err(|_| CoreError::Crypto)
}

/// `aad_hash` / `payload_sha256` 两列共用的十六进制摘要。
pub(crate) fn sha256_hex_of(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}