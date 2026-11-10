//! 备份 key 与分块 AEAD 文件容器。
//!
//! 威胁模型（用户 2026-10-25 选定）：备份副本只防机器/磁盘丢失、误删、异地副本被随手拷走，
//! **不防本机 principal / 机器管理员**。因此 key 文件由操作员单独保管，不跟 data_dir 同放；
//! 值只做 XChaCha20-Poly1305 认证加密，不做 KMS/角色隔离。
//!
//! 文件容器按 1 MiB 分块，每块独立 nonce/counter；大库不需要整份读进内存。

use crate::{write_private_file, SecretError};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use zeroize::Zeroizing;

const KEY_MAGIC: &[u8] = b"RCBACKUPKEY1";
const BACKUP_MAGIC: &[u8] = b"RCBACKUP1";
const BACKUP_VERSION: u8 = 1;
const KEY_LEN: usize = 32;
const KEY_ID_LEN: usize = 8;
const NONCE_PREFIX_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const DEFAULT_CHUNK: u32 = 1024 * 1024;
const MAX_CHUNK: u32 = 16 * 1024 * 1024;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 备份 key（32 字节），文件格式 `RCBACKUPKEY1 || key`。drop 时零化。
pub struct BackupKey {
    key: Zeroizing<[u8; KEY_LEN]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupFileInfo {
    pub key_id: String,
    pub plaintext_len: u64,
    pub chunks: u64,
}

impl BackupKey {
    pub fn generate() -> Result<Self, SecretError> {
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        getrandom::fill(&mut *key).map_err(|_| SecretError::Io(std::io::ErrorKind::Other))?;
        Ok(BackupKey { key })
    }

    pub fn load(path: &Path) -> Result<Self, SecretError> {
        let bytes = std::fs::read(path).map_err(|e| SecretError::Io(e.kind()))?;
        if bytes.len() != KEY_MAGIC.len() + KEY_LEN || &bytes[..KEY_MAGIC.len()] != KEY_MAGIC {
            return Err(SecretError::BackupKey);
        }
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        key.copy_from_slice(&bytes[KEY_MAGIC.len()..]);
        Ok(BackupKey { key })
    }

    /// 已存在 ⇒ `AlreadyExists`，不覆盖旧 key。
    pub fn save(&self, path: &Path) -> Result<(), SecretError> {
        if path.exists() {
            return Err(SecretError::AlreadyExists);
        }
        let mut bytes = Vec::with_capacity(KEY_MAGIC.len() + KEY_LEN);
        bytes.extend_from_slice(KEY_MAGIC);
        bytes.extend_from_slice(&*self.key);
        write_private_file(path, &bytes)
    }

    /// 16 位十六进制指纹：显示/清单用，不是 key 本身。
    pub fn key_id(&self) -> String {
        hex(&self.key_id_bytes())
    }

    fn key_id_bytes(&self) -> [u8; KEY_ID_LEN] {
        let digest = Sha256::digest(*self.key);
        let mut out = [0u8; KEY_ID_LEN];
        out.copy_from_slice(&digest[..KEY_ID_LEN]);
        out
    }
}

fn nonce(prefix: &[u8; NONCE_PREFIX_LEN], counter: u64) -> [u8; NONCE_LEN] {
    let mut out = [0u8; NONCE_LEN];
    out[..NONCE_PREFIX_LEN].copy_from_slice(prefix);
    out[NONCE_PREFIX_LEN..].copy_from_slice(&counter.to_be_bytes());
    out
}

fn cipher(key: &BackupKey) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(Key::from_slice(&*key.key))
}

/// 加密 `input` 到 `output`。调用方负责在失败时删掉半成品。
pub fn encrypt_file(
    input: &Path,
    output: &Path,
    key: &BackupKey,
) -> Result<BackupFileInfo, SecretError> {
    let plaintext_len = std::fs::metadata(input)
        .map_err(|e| SecretError::Io(e.kind()))?
        .len();
    let mut prefix = [0u8; NONCE_PREFIX_LEN];
    getrandom::fill(&mut prefix).map_err(|_| SecretError::Io(std::io::ErrorKind::Other))?;
    let out = std::fs::File::create(output).map_err(|e| SecretError::Io(e.kind()))?;
    let mut writer = BufWriter::new(out);
    writer
        .write_all(BACKUP_MAGIC)
        .and_then(|_| writer.write_all(&[BACKUP_VERSION]))
        .and_then(|_| writer.write_all(&key.key_id_bytes()))
        .and_then(|_| writer.write_all(&DEFAULT_CHUNK.to_le_bytes()))
        .and_then(|_| writer.write_all(&plaintext_len.to_le_bytes()))
        .and_then(|_| writer.write_all(&prefix))
        .map_err(|e| SecretError::Io(e.kind()))?;

    let mut reader = BufReader::new(
        std::fs::File::open(input).map_err(|e| SecretError::Io(e.kind()))?,
    );
    let aead = cipher(key);
    let mut buf = vec![0u8; DEFAULT_CHUNK as usize];
    let mut counter = 0u64;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| SecretError::Io(e.kind()))?;
        if n == 0 {
            break;
        }
        let nonce = nonce(&prefix, counter);
        let ciphertext = aead
            .encrypt(XNonce::from_slice(&nonce), &buf[..n])
            .map_err(|_| SecretError::BackupFormat)?;
        writer
            .write_all(&(ciphertext.len() as u32).to_le_bytes())
            .and_then(|_| writer.write_all(&ciphertext))
            .map_err(|e| SecretError::Io(e.kind()))?;
        counter += 1;
    }
    writer.flush().map_err(|e| SecretError::Io(e.kind()))?;
    let written = std::fs::metadata(output)
        .map_err(|e| SecretError::Io(e.kind()))?
        .len();
    let expected = BACKUP_MAGIC.len() as u64
        + 1
        + KEY_ID_LEN as u64
        + 4
        + 8
        + NONCE_PREFIX_LEN as u64
        + plaintext_len
        + counter * 16
        + counter * 4;
    if written != expected {
        return Err(SecretError::BackupFormat);
    }
    Ok(BackupFileInfo {
        key_id: key.key_id(),
        plaintext_len,
        chunks: counter,
    })
}

/// 解密 `input` 到 `output`。错 key、被篡改、截断都归 `BackupAuth`/`BackupFormat`；
/// 调用方必须把 `output` 当临时文件，失败即删。
pub fn decrypt_file(
    input: &Path,
    output: &Path,
    key: &BackupKey,
) -> Result<BackupFileInfo, SecretError> {
    let mut reader = BufReader::new(
        std::fs::File::open(input).map_err(|e| SecretError::Io(e.kind()))?,
    );
    let mut magic = [0u8; 9];
    reader
        .read_exact(&mut magic)
        .map_err(|_| SecretError::BackupFormat)?;
    if magic.as_slice() != BACKUP_MAGIC {
        return Err(SecretError::BackupFormat);
    }
    let mut version = [0u8; 1];
    reader
        .read_exact(&mut version)
        .map_err(|_| SecretError::BackupFormat)?;
    if version[0] != BACKUP_VERSION {
        return Err(SecretError::BackupFormat);
    }
    let mut key_id = [0u8; KEY_ID_LEN];
    reader
        .read_exact(&mut key_id)
        .map_err(|_| SecretError::BackupFormat)?;
    if key_id != key.key_id_bytes() {
        return Err(SecretError::BackupAuth);
    }
    let mut chunk_raw = [0u8; 4];
    reader
        .read_exact(&mut chunk_raw)
        .map_err(|_| SecretError::BackupFormat)?;
    let chunk_size = u32::from_le_bytes(chunk_raw);
    if chunk_size == 0 || chunk_size > MAX_CHUNK {
        return Err(SecretError::BackupFormat);
    }
    let mut len_raw = [0u8; 8];
    reader
        .read_exact(&mut len_raw)
        .map_err(|_| SecretError::BackupFormat)?;
    let plaintext_len = u64::from_le_bytes(len_raw);
    let mut prefix = [0u8; NONCE_PREFIX_LEN];
    reader
        .read_exact(&mut prefix)
        .map_err(|_| SecretError::BackupFormat)?;

    let out = std::fs::File::create(output).map_err(|e| SecretError::Io(e.kind()))?;
    let mut writer = BufWriter::new(out);
    let aead = cipher(key);
    let mut counter = 0u64;
    let mut total = 0u64;
    while total < plaintext_len {
        let mut clen_raw = [0u8; 4];
        reader
            .read_exact(&mut clen_raw)
            .map_err(|_| SecretError::BackupFormat)?;
        let clen = u32::from_le_bytes(clen_raw) as usize;
        if clen < 16 || clen > chunk_size as usize + 16 {
            return Err(SecretError::BackupFormat);
        }
        let mut ciphertext = vec![0u8; clen];
        reader
            .read_exact(&mut ciphertext)
            .map_err(|_| SecretError::BackupFormat)?;
        let nonce = nonce(&prefix, counter);
        let plaintext = aead
            .decrypt(XNonce::from_slice(&nonce), ciphertext.as_ref())
            .map_err(|_| SecretError::BackupAuth)?;
        if plaintext.is_empty() || total + plaintext.len() as u64 > plaintext_len {
            return Err(SecretError::BackupFormat);
        }
        writer
            .write_all(&plaintext)
            .map_err(|e| SecretError::Io(e.kind()))?;
        total += plaintext.len() as u64;
        counter += 1;
    }
    writer.flush().map_err(|e| SecretError::Io(e.kind()))?;
    // 容器末尾不许有多余块。
    let mut extra = [0u8; 1];
    if reader
        .read(&mut extra)
        .map_err(|e| SecretError::Io(e.kind()))?
        != 0
    {
        return Err(SecretError::BackupFormat);
    }
    Ok(BackupFileInfo {
        key_id: key.key_id(),
        plaintext_len,
        chunks: counter,
    })
}
