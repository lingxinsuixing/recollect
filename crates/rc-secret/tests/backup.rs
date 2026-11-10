//! 备份 key 文件与分块 AEAD 容器的往返/负例。

use rc_secret::backup::{decrypt_file, encrypt_file, BackupKey};
use rc_secret::SecretError;

#[test]
fn key_file_round_trips_and_refuses_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("backup.key");
    let key = BackupKey::generate().unwrap();
    key.save(&path).unwrap();
    let loaded = BackupKey::load(&path).unwrap();
    assert_eq!(loaded.key_id(), key.key_id());
    assert_eq!(key.key_id().len(), 16);
    assert!(matches!(key.save(&path), Err(SecretError::AlreadyExists)));
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"short").unwrap();
    assert!(matches!(BackupKey::load(&path), Err(SecretError::BackupKey)));
}

#[test]
fn file_encryption_round_trips_across_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let key = BackupKey::generate().unwrap();
    let plain = dir.path().join("plain.db");
    let data: Vec<u8> = (0..(2 * 1024 * 1024 + 37)).map(|i| (i % 251) as u8).collect();
    std::fs::write(&plain, &data).unwrap();
    let enc = dir.path().join("plain.db.enc");
    let out = dir.path().join("out.db");
    let info = encrypt_file(&plain, &enc, &key).unwrap();
    assert!(info.chunks >= 3, "大文件应分多块：{info:?}");
    let restored = decrypt_file(&enc, &out, &key).unwrap();
    assert_eq!(restored.plaintext_len, data.len() as u64);
    assert_eq!(std::fs::read(&out).unwrap(), data);
}

#[test]
fn wrong_key_and_tampering_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let key = BackupKey::generate().unwrap();
    let plain = dir.path().join("plain.db");
    std::fs::write(&plain, b"backup body").unwrap();
    let enc = dir.path().join("plain.db.enc");
    encrypt_file(&plain, &enc, &key).unwrap();

    let other = BackupKey::generate().unwrap();
    assert!(matches!(
        decrypt_file(&enc, &dir.path().join("out1.db"), &other),
        Err(SecretError::BackupAuth)
    ));

    let mut bytes = std::fs::read(&enc).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    std::fs::write(&enc, bytes).unwrap();
    assert!(matches!(
        decrypt_file(&enc, &dir.path().join("out2.db"), &key),
        Err(SecretError::BackupAuth)
    ));
}
