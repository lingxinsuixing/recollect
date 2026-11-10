//! 表名派生、key 文件与载荷 AEAD（设计 §10.1/§10.2）。

use rc_core::{
    aad, agent_table_suffix, is_valid_core_table, key_path, load_key, load_or_create_key, open,
    seal, table_names, AgentKey, CoreError, KEY_LEN, NONCE_LEN,
};
use std::collections::HashSet;
use std::path::Path;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn suffix_is_stable_16_lowercase_hex_and_tables_follow_it() {
    let a = agent_table_suffix("agent-a");
    assert_eq!(a, agent_table_suffix("agent-a"));
    assert_eq!(a.len(), 16);
    assert!(a
        .bytes()
        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
    assert_ne!(a, agent_table_suffix("agent-b"));

    let (doc, rev) = table_names("agent-a");
    assert_eq!(doc, format!("core_doc_{a}"));
    assert_eq!(rev, format!("core_rev_{a}"));
    assert!(is_valid_core_table(&doc));
    assert!(is_valid_core_table(&rev));
    // 原始 agent_id 不进表名。
    assert!(!doc.contains("agent-a") && !rev.contains("agent-a"));
}

#[test]
fn invalid_table_names_are_rejected() {
    for bad in [
        "",
        "core_agents",
        "core_doc_",
        "core_rev_",
        "core_doc_0123456789abcde",     // 15
        "core_doc_0123456789abcdef0",   // 17
        "core_doc_0123456789ABCDEF",    // 大写
        "core_doc_0123456789abcdeF",    // 混大写
        "core_doc_0123456789abcdeg",    // 非 hex
        "core_doc_0123456789abcdef; DROP TABLE core_agents",
        " core_doc_0123456789abcdef",
        "core_doc_0123456789abcdef ",
        "xcore_doc_0123456789abcdef",
        "core_other_0123456789abcdef",
        "core_doc_0123456789abcdef\"",
    ] {
        assert!(!is_valid_core_table(bad), "{bad:?} must be rejected");
    }
    assert!(is_valid_core_table("core_doc_0123456789abcdef"));
    assert!(is_valid_core_table("core_rev_0123456789abcdef"));
}

#[test]
fn key_file_is_created_with_exactly_32_bytes_and_reloads_stable() {
    let dir = tempfile::tempdir().unwrap();
    let k1 = load_or_create_key(dir.path(), "agent-a").unwrap();
    let path = key_path(dir.path(), "agent-a");
    assert!(path.ends_with(Path::new("keys").join(format!(
        "core_{}.key",
        agent_table_suffix("agent-a")
    ))));
    assert!(path.is_file());

    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes.len(), KEY_LEN);
    assert_eq!(k1.as_bytes(), bytes.as_slice());

    let k2 = load_or_create_key(dir.path(), "agent-a").unwrap();
    assert_eq!(k1.as_bytes(), k2.as_bytes(), "reload must not rotate the key");
    assert_eq!(std::fs::read(&path).unwrap(), bytes, "key file unchanged");

    // Debug 不许带出密钥字节。
    let shown = format!("{k1:?}");
    assert!(!shown.contains(&hex(k1.as_bytes())), "{shown}");

    // 另一个 agent 是另一把 key。
    let other = load_or_create_key(dir.path(), "agent-b").unwrap();
    assert_ne!(k1.as_bytes(), other.as_bytes());
}

#[test]
fn a_key_file_that_is_not_32_bytes_is_bad_key_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = key_path(dir.path(), "agent-a");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, [0u8; 31]).unwrap();
    assert!(matches!(
        load_or_create_key(dir.path(), "agent-a"),
        Err(CoreError::BadKeyFile)
    ));
    std::fs::write(&path, [0u8; 33]).unwrap();
    assert!(matches!(
        load_or_create_key(dir.path(), "agent-a"),
        Err(CoreError::BadKeyFile)
    ));
}

#[test]
fn load_key_refuses_a_missing_file_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = key_path(dir.path(), "agent-a");
    assert!(matches!(
        load_key(dir.path(), "agent-a"),
        Err(CoreError::KeyMissing)
    ));
    assert!(!path.exists(), "load_key must not mint a key");
    assert!(!dir.path().join("keys").exists(), "not even the directory");

    // 注册路径才造 key，之后 load_key 读回同一把。
    let created = load_or_create_key(dir.path(), "agent-a").unwrap();
    assert!(path.is_file());
    let loaded = load_key(dir.path(), "agent-a").unwrap();
    assert_eq!(created.as_bytes(), loaded.as_bytes());

    // 删掉之后仍然是 `KeyMissing`，且不会重建。
    std::fs::remove_file(&path).unwrap();
    assert!(matches!(
        load_key(dir.path(), "agent-a"),
        Err(CoreError::KeyMissing)
    ));
    assert!(!path.exists(), "load_key must not recreate the deleted key");
}

#[test]
fn load_key_rejects_a_wrong_length_without_rewriting_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = key_path(dir.path(), "agent-a");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, [0u8; 31]).unwrap();
    assert!(matches!(
        load_key(dir.path(), "agent-a"),
        Err(CoreError::BadKeyFile)
    ));
    assert_eq!(std::fs::read(&path).unwrap(), vec![0u8; 31], "file untouched");
}

#[test]
fn seal_open_round_trips() {
    let key = AgentKey::from_bytes([7u8; KEY_LEN]);
    let aad = aad("agent-a", 3, "doc");
    let plain = b"{\"agent_id\":\"agent-a\",\"core_text\":\"hello\"}";
    let (nonce, ciphertext) = seal(&key, &aad, plain).unwrap();
    assert_eq!(nonce.len(), NONCE_LEN);
    assert_ne!(&ciphertext[..], &plain[..]);
    let opened = open(&key, &aad, &nonce, &ciphertext).unwrap();
    assert_eq!(opened, plain);
}

#[test]
fn a_wrong_key_cannot_open() {
    let k1 = AgentKey::from_bytes([1u8; KEY_LEN]);
    let k2 = AgentKey::from_bytes([2u8; KEY_LEN]);
    let aad = aad("agent-a", 1, "doc");
    let (nonce, ciphertext) = seal(&k1, &aad, b"secret").unwrap();
    assert!(matches!(
        open(&k2, &aad, &nonce, &ciphertext),
        Err(CoreError::Crypto)
    ));
}

#[test]
fn a_tampered_aad_or_ciphertext_is_crypto_failure() {
    let key = AgentKey::from_bytes([9u8; KEY_LEN]);
    let (nonce, ciphertext) = seal(&key, &aad("agent-a", 1, "doc"), b"secret").unwrap();

    // 换版本
    assert!(matches!(
        open(&key, &aad("agent-a", 2, "doc"), &nonce, &ciphertext),
        Err(CoreError::Crypto)
    ));
    // 换分区
    assert!(matches!(
        open(&key, &aad("agent-a", 1, "persona"), &nonce, &ciphertext),
        Err(CoreError::Crypto)
    ));
    // 换 agent
    assert!(matches!(
        open(&key, &aad("agent-b", 1, "doc"), &nonce, &ciphertext),
        Err(CoreError::Crypto)
    ));
    // 翻一个密文字节
    let mut flipped = ciphertext.clone();
    let last = flipped.len() - 1;
    flipped[last] ^= 0x01;
    assert!(matches!(
        open(&key, &aad("agent-a", 1, "doc"), &nonce, &flipped),
        Err(CoreError::Crypto)
    ));
}

#[test]
fn a_thousand_seals_never_reuse_a_nonce() {
    let key = AgentKey::from_bytes([3u8; KEY_LEN]);
    let aad = aad("agent-a", 1, "doc");
    let mut seen = HashSet::new();
    for _ in 0..1000 {
        let (nonce, _) = seal(&key, &aad, b"same plaintext").unwrap();
        assert!(seen.insert(nonce), "nonce reused");
    }
    assert_eq!(seen.len(), 1000);
}
