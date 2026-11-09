//! 中心密钥的加密归档：导出到密文箱、从密文箱导入。
//!
//! 两件事只做一次：`hub.key` 的原字节进 `SecretsBox` 的 `hub.key` 格（十六进制，箱本身是
//! ChaCha20-Poly1305 密文），导入时原样写回并**验形**。归档里没有明文私钥，口令只从 stdin 来
//! （CLI 那一侧拦 TTY/空值），本模块的错误面全部是静态短语：零路径、零口令、零私钥。
//!
//! doctor 的指纹读数走 `<data-dir>/hub.pub`：它是本模块（以及 `serve`/`snapshot publish`）
//! 用 `HubKey::load_or_create(..).public_b64()` 算出的 `device_id_of` 结果，连同 `hub.key` 的
//! (字节数, mtime) 一起落盘。doctor 只在戳对得上时采用——密钥被换掉而缓存没刷新时它读作
//! null，而不是把上一把钥匙的指纹念成这一把的。这条缓存让 doctor 不需要 Ed25519 依赖
//! （doctor 的清单里没有 `rc-net`，本批文件集也不许动它），而且 doctor 永远不打开私钥本体。

use rc_net::key::HubKey;
use rc_store::Root;
use std::path::Path;

const KEY_FILE: &str = "hub.key";
const ARCHIVE_SLOT: &str = "hub.key";
const EXPORTED_AT_METRIC: &str = "key_archive_exported_at_ms";
const PUBLIC_CACHE: &str = "hub.pub";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyArchiveFault {
    Io,
    KeyMissing,
    AlreadyExists,
    BadArchive,
    BadPassphrase,
    ExistingKey,
    BadKey,
}

impl KeyArchiveFault {
    /// 运维面的唯一文案来源：一个变体一句静态短语，零插值、零路径。
    pub fn message(self) -> &'static str {
        match self {
            KeyArchiveFault::Io => "key archive io failed",
            KeyArchiveFault::KeyMissing => "hub key is missing — nothing to export",
            KeyArchiveFault::AlreadyExists => "key archive already exists — refusing to overwrite",
            KeyArchiveFault::BadArchive => "the key archive is malformed",
            KeyArchiveFault::BadPassphrase => {
                "the key archive cannot be opened (wrong passphrase or tampered file)"
            }
            KeyArchiveFault::ExistingKey => "hub key already exists — refusing to overwrite",
            KeyArchiveFault::BadKey => "the archived hub key is unreadable or malformed",
        }
    }
}

/// 导出：读 `hub.key` 原字节 → 十六进制进密文箱的 `hub.key` 格 → 记导出时刻 → 回公钥指纹。
///
/// 顺序里有两个刻意的前置：`out` 的存在性在**写任何字节之前**判（拒绝覆盖不是写完再删）；
/// 私钥解析在**建箱之前**跑（一份读不动的私钥不该先产出一个“成功”的归档）。
/// 归档写成而计数写失败时删掉归档：命令回 3 之后重试撞上 `AlreadyExists` 比不写更坏。
pub fn export(
    data_dir: &Path,
    out: &Path,
    passphrase: &[u8],
    now_ms: i64,
) -> Result<String, KeyArchiveFault> {
    let key_path = data_dir.join(KEY_FILE);
    if !key_path.is_file() {
        return Err(KeyArchiveFault::KeyMissing);
    }
    if out.exists() {
        return Err(KeyArchiveFault::AlreadyExists);
    }
    // `rc-net` 只公开 `load_or_create`（`parse` 是私有的，本批文件集不许动 `rc-net`）。
    // 这里先用 `is_file` 挡掉缺失、读原字节、解析后再逐字节对拍：`load_or_create` 的
    // “创建”那一支只可能在读与解析之间有人删掉文件的竞态里发生，对拍把那一种变成
    // 响亮失败，而不是悄悄用一把新钥匙导出。
    let raw = std::fs::read(&key_path).map_err(|_| KeyArchiveFault::Io)?;
    let key = HubKey::load_or_create(&key_path).map_err(|_| KeyArchiveFault::BadKey)?;
    if std::fs::read(&key_path).map_err(|_| KeyArchiveFault::Io)? != raw {
        return Err(KeyArchiveFault::Io);
    }
    let fingerprint = fingerprint_of(&key)?;
    // 账本在写归档之前打开：库坏了就不该留下一个“导出成功”的文件。
    let root = Root::open(data_dir).map_err(|_| KeyArchiveFault::Io)?;
    let mut archive =
        rc_secret::SecretsBox::create(out, passphrase).map_err(create_fault)?;
    if let Err(e) = archive.put(ARCHIVE_SLOT, &hex_encode(&raw)) {
        drop(archive);
        let _ = std::fs::remove_file(out);
        return Err(create_fault(e));
    }
    drop(archive);
    if rc_ledger::counters::set_one(
        root.ledger(),
        "maintenance",
        "key_archive",
        EXPORTED_AT_METRIC,
        now_ms,
        now_ms,
    )
    .is_err()
    {
        let _ = std::fs::remove_file(out);
        return Err(KeyArchiveFault::Io);
    }
    record_fingerprint(data_dir, &key_path, &key);
    Ok(fingerprint)
}

/// 导入：`hub.key` 已在 ⇒ 拒绝；开箱、取格、十六进制解码；
/// `write_private_file`（临时名 + rename）落盘，再用 `HubKey::load_or_create` 验形；坏形即删。
pub fn import(
    data_dir: &Path,
    from: &Path,
    passphrase: &[u8],
) -> Result<String, KeyArchiveFault> {
    let key_path = data_dir.join(KEY_FILE);
    if key_path.exists() {
        return Err(KeyArchiveFault::ExistingKey);
    }
    let archive = rc_secret::SecretsBox::open(from, passphrase).map_err(open_fault)?;
    let encoded = archive.get(ARCHIVE_SLOT).ok_or(KeyArchiveFault::BadArchive)?;
    let raw = hex_decode(encoded).ok_or(KeyArchiveFault::BadArchive)?;
    std::fs::create_dir_all(data_dir).map_err(|_| KeyArchiveFault::Io)?;
    rc_net::key::write_private_file(&key_path, &raw).map_err(|_| KeyArchiveFault::Io)?;
    // 同上：`load_or_create` 在文件在时只解析不覆盖；对拍把“写后文件被换掉/删掉”变成
    // BadKey 并删掉那一份（导入的契约就是盘上等于归档里那串字节）。
    match HubKey::load_or_create(&key_path) {
        Ok(key) => {
            let written = std::fs::read(&key_path).map_err(|_| KeyArchiveFault::Io)?;
            if written != raw {
                let _ = std::fs::remove_file(&key_path);
                return Err(KeyArchiveFault::BadKey);
            }
            let fingerprint = fingerprint_of(&key)?;
            record_fingerprint(data_dir, &key_path, &key);
            Ok(fingerprint)
        }
        Err(_) => {
            let _ = std::fs::remove_file(&key_path);
            Err(KeyArchiveFault::BadKey)
        }
    }
}

/// 每一次“hub 自己读或建了中心私钥”之后刷新公开指纹缓存；失败只是 doctor 少一格读数，
/// 不让它反过来把 serve / snapshot / export / import 判死。调用方手里已经有 `HubKey`，
/// 因此这里不再打开私钥文件（少一次 `load_or_create`，也就少一条“缺文件会被创建”的支）。
pub fn record_fingerprint(data_dir: &Path, key_path: &Path, key: &HubKey) {
    let Ok(fingerprint) = fingerprint_of(key) else { return };
    write_public_stamp(data_dir, key_path, &fingerprint);
}

fn fingerprint_of(key: &HubKey) -> Result<String, KeyArchiveFault> {
    rc_net::key::device_id_of(&key.public_b64()).ok_or(KeyArchiveFault::BadKey)
}

#[derive(serde::Serialize)]
struct PublicStamp<'a> {
    key_size: u64,
    key_mtime_ms: i64,
    fingerprint: &'a str,
}

/// 指纹缓存以 `hub.key` 的 (字节数, mtime 毫秒) 为戳。时间取毫秒是为了与 doctor 那一侧
/// 用同一种截断精度比较：`SystemTime` 在文件系统上的精度因平台而异，纳秒级尾数不进戳。
fn write_public_stamp(data_dir: &Path, key_path: &Path, fingerprint: &str) {
    let Ok(meta) = std::fs::metadata(key_path) else { return };
    let Ok(modified) = meta.modified() else { return };
    let Ok(since_epoch) = modified.duration_since(std::time::UNIX_EPOCH) else { return };
    let stamp = PublicStamp {
        key_size: meta.len(),
        key_mtime_ms: since_epoch.as_millis() as i64,
        fingerprint,
    };
    let Ok(body) = serde_json::to_vec(&stamp) else { return };
    let _ = rc_net::key::write_private_file(&data_dir.join(PUBLIC_CACHE), &body);
}

/// `SecretError` 增变体时这里编译不过，逼着表态；建箱只会产出 Io/Permission/AlreadyExists。
fn create_fault(e: rc_secret::SecretError) -> KeyArchiveFault {
    use rc_secret::SecretError as E;
    match e {
        E::AlreadyExists => KeyArchiveFault::AlreadyExists,
        E::Io(_) | E::Permission | E::BadParams => KeyArchiveFault::Io,
        E::BadFormat | E::OpenFailed | E::BadSlot | E::MissingSlot | E::BoxMissing
        | E::PassMissing | E::BadBox | E::BackupKey | E::BackupFormat | E::BackupAuth => {
            KeyArchiveFault::BadArchive
        }
    }
}

/// 开箱的失败面：错口令与篡改都落在 `OpenFailed`（AEAD 认证失败无法区分两者），
/// 归 `BadPassphrase`；格式/参数坏归 `BadArchive`；读文件失败归 `Io`。
fn open_fault(e: rc_secret::SecretError) -> KeyArchiveFault {
    use rc_secret::SecretError as E;
    match e {
        E::OpenFailed => KeyArchiveFault::BadPassphrase,
        E::BadFormat | E::BadParams | E::MissingSlot | E::BadSlot | E::BoxMissing
        | E::PassMissing | E::BadBox | E::BackupKey | E::BackupFormat | E::BackupAuth => {
            KeyArchiveFault::BadArchive
        }
        E::Io(_) | E::Permission => KeyArchiveFault::Io,
        E::AlreadyExists => KeyArchiveFault::BadArchive,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    let (pairs, rest) = text.as_bytes().as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(pairs.len());
    for pair in pairs {
        out.push((hex_digit(pair[0])? << 4) | hex_digit(pair[1])?);
    }
    Some(out)
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}
