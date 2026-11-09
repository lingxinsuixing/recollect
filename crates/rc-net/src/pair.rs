use crate::error::NetError;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// 去掉了 0/O/1/I 的 32 字符表。32 整除 256 ⇒ `& 31` 无模偏差，50 bit 熵。
const CODE_ALPHABET: &[u8; 32] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

pub fn new_pair_code() -> Result<(String, String), NetError> {
    let mut raw = [0u8; 10];
    getrandom::fill(&mut raw).map_err(|_| NetError::NoEntropy)?;
    let chars: String = raw.iter().map(|b| CODE_ALPHABET[(*b as usize) & 31] as char).collect();
    let display = format!("{}-{}", &chars[..5], &chars[5..]);
    let hash = hash_pair_code(&display).expect("generated code is valid by construction");
    Ok((display, hash))
}

pub fn normalize_pair_code(input: &str) -> Option<String> {
    let cleaned: String = input
        .chars()
        .filter(|c| *c != '-' && !c.is_whitespace())
        .collect::<String>()
        .to_ascii_uppercase();
    if cleaned.len() != 10 || !cleaned.bytes().all(|b| CODE_ALPHABET.contains(&b)) {
        return None;
    }
    Some(cleaned)
}

pub fn hash_pair_code(input: &str) -> Option<String> {
    let code = normalize_pair_code(input)?;
    let mut h = Sha256::new();
    h.update(b"recollect-pair-code-v1\n");
    h.update(code.as_bytes());
    Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

pub fn new_token() -> Result<(String, String), NetError> {
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|_| NetError::NoEntropy)?;
    let token = URL_SAFE_NO_PAD.encode(raw);
    let hash = hash_token(&token);
    Ok((token, hash))
}

pub fn hash_token(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"recollect-token-v1\n");
    h.update(token.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

struct NonceEntry {
    device_id: String,
    expires_at_ms: i64,
}

/// 纯内存、短 TTL、单次使用。**重启后旧 nonce 失效是可接受的**：
/// 重放窗口由 TTL 决定，不由持久化决定；为它开第四张 ledger 表是把"临时挑战"
/// 写进永不出中心的账本，代价大于收益。
pub struct NonceStore {
    ttl_ms: i64,
    capacity: usize,
    entries: HashMap<String, NonceEntry>,
}

impl NonceStore {
    pub fn new(ttl_ms: i64) -> NonceStore {
        Self::with_capacity(ttl_ms, 1024)
    }

    pub fn with_capacity(ttl_ms: i64, capacity: usize) -> NonceStore {
        NonceStore { ttl_ms, capacity, entries: HashMap::new() }
    }

    pub fn issue(&mut self, device_id: &str, now_ms: i64) -> Result<(String, i64), NetError> {
        self.gc(now_ms);
        if self.entries.len() >= self.capacity {
            return Err(NetError::NonceCapacity);
        }
        let mut raw = [0u8; 18];
        getrandom::fill(&mut raw).map_err(|_| NetError::NoEntropy)?;
        let nonce = URL_SAFE_NO_PAD.encode(raw);
        let expires_at_ms = now_ms + self.ttl_ms;
        self.entries.insert(
            nonce.clone(),
            NonceEntry { device_id: device_id.to_string(), expires_at_ms },
        );
        Ok((nonce, expires_at_ms))
    }

    /// 命中即删除 ⇒ 同一个 nonce 第二次进来必为 false。设备不匹配同样删除：
    /// 一个被别人试过的 nonce 不该继续可用。
    pub fn consume(&mut self, device_id: &str, nonce: &str, now_ms: i64) -> bool {
        let Some(entry) = self.entries.get(nonce) else { return false };
        let ok = entry.device_id == device_id && entry.expires_at_ms >= now_ms;
        self.entries.remove(nonce);
        ok
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 与 `len()` 配对存在（`clippy::len_without_is_empty` 是 `-D warnings` 下的硬杠）。
    /// 语义只是"没有活着的 nonce"，不是"没有容量"——容量满的判据在 `issue` 里。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn gc(&mut self, now_ms: i64) {
        self.entries.retain(|_, e| e.expires_at_ms >= now_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_hash_is_not_the_token() {
        let (token, hash) = new_token().unwrap();
        assert_eq!(token.len(), 43, "32 字节 base64url：{token}");
        assert_eq!(hash.len(), 64);
        assert_ne!(token, hash);
        assert_eq!(hash_token(&token), hash);
    }

    #[test]
    fn pair_code_hash_is_case_and_dash_insensitive_but_shape_strict() {
        let (display, hash) = new_pair_code().unwrap();
        assert_eq!(display.len(), 11);
        assert_eq!(hash_pair_code(&display.to_lowercase()).as_deref(), Some(hash.as_str()));
        let no_dash = display.replace('-', " ");
        assert_eq!(hash_pair_code(&no_dash).as_deref(), Some(hash.as_str()));
        assert!(hash_pair_code("SHORT").is_none());
        assert!(hash_pair_code("00000-00000").is_none(), "0 不在字母表里");
    }
}
