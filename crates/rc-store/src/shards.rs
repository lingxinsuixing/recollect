//! V4 的会话分片载体：`<data-dir>/inject/session-<sha256 hex>.ids`，一行一个注入过的句柄。
//!
//! 三件事各自只有一处实现：
//! - **名字不泄露原串**：文件名只含 `sha256(session)` 的小写十六进制，原会话串不出现在盘上。
//! - **幂等**：同会话重复 `record` 得到同样的内容（`BTreeSet` 去重 + 升序）。
//! - **上限 64**：`read_dir` 里 `session-*.ids` 按 mtime 升序，超出 `RETAIN_SESSIONS` 的删最旧；
//!   每删一个把 `inject/evicted` 的十进制计数 +1（读-改-写；文件不存在按 0）。
//!
//! `reading` 只读、永不失败：目录不在就是 `{shards:0, evicted:0}`，坏计数按 0。
//! 淘汰计数**走文件、不进 `usage_counter`**——本模块不认识 ledger，也不改 metric 词表。
//!
//! 保留排序以**文件 mtime** 为准（`now_ms` 只负责把刚落盘的分片盖成调用方给的时刻），
//! 因此重放旧会话不会把它顶成"最新"以外的形状：调用方给什么时刻，它就排什么位置。

use crate::StoreError;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 保留的会话分片数。V4 的 64 是上限；证据见 `docs/2026-10-22-inject-shards.md`。
pub const RETAIN_SESSIONS: usize = 64;

/// 淘汰计数的文件名（住在 `inject/` 里，与分片同目录）。
pub const EVICTED_FILE: &str = "evicted";

/// `doctor` 的读数形状：现存分片数与累计淘汰数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShardReading {
    pub shards: usize,
    pub evicted: u64,
}

/// 分片目录：`<data_dir>/inject`。
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("inject")
}

/// 写入一次注入读数：整份重写该会话的分片、盖 mtime、再按上限淘汰。
/// 返回操作之后的 [`ShardReading`]（与 `reading` 同一口径，累计淘汰数来自文件）。
pub fn record(
    data_dir: &Path,
    session: &str,
    keys: &[String],
    now_ms: i64,
) -> Result<ShardReading, StoreError> {
    if session.trim().is_empty() {
        // 空会话是**调用方错误**，不是库坏了。`StoreError` 的变体表与 `rc-agent` 的
        // 逐变体映射共用（那边会在新增变体时当场 E0004），本批不改那个 crate ⇒ 不扩变体表，
        // 用带常量消息的 `Io` 类别；调用方（hook）只把它折成一条类别 note，原文不外传。
        return Err(StoreError::Io("session key is empty".to_string()));
    }
    let d = dir(data_dir);
    fs::create_dir_all(&d)?;
    let dest = d.join(shard_name(session));
    // 升序去重：同会话重复写、同一句柄重复给，落盘字节都一样。
    let mut unique: BTreeSet<&str> = BTreeSet::new();
    for k in keys {
        unique.insert(k.as_str());
    }
    let mut body = String::new();
    for k in &unique {
        body.push_str(k);
        body.push('\n');
    }
    write_replacing(&dest, body.as_bytes())?;
    set_mtime(&dest, now_ms)?;
    retain(&d)?;
    Ok(reading(data_dir))
}

/// 只读读数：目录不在 ⇒ 零；只数 `session-*.ids`，`evicted` 解析不出按 0。
pub fn reading(data_dir: &Path) -> ShardReading {
    let d = dir(data_dir);
    let shards = match fs::read_dir(&d) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| is_shard_file(&e.file_name()))
            .count(),
        Err(_) => 0,
    };
    ShardReading { shards, evicted: read_evicted(&d) }
}

/// 文件名：`session-<sha256(session) 小写 hex>.ids`。原会话串只进哈希，不进名字。
fn shard_name(session: &str) -> String {
    let mut h = Sha256::new();
    h.update(session.as_bytes());
    let mut hex = String::with_capacity(64);
    for b in h.finalize() {
        hex.push_str(&format!("{b:02x}"));
    }
    format!("session-{hex}.ids")
}

fn is_shard_file(name: &OsStr) -> bool {
    let s = name.to_string_lossy();
    s.starts_with("session-") && s.ends_with(".ids")
}

/// 临时名 + rename。Windows 上 rename 撞到已存在的目标会拒绝，所以退一步走
/// "旧文件先挪到备份名 → rename → 删备份"；第二步失败就把旧文件挪回去
/// （**失败保留旧文件**不是口号，是这条恢复路径）。
fn write_replacing(dest: &Path, body: &[u8]) -> Result<(), StoreError> {
    let tmp = sibling_with_suffix(dest, ".tmp")?;
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(body)?;
        f.sync_all()?;
    }
    let outcome = match fs::rename(&tmp, dest) {
        Ok(()) => Ok(()),
        Err(_) if dest.exists() => replace_over_existing(&tmp, dest),
        Err(e) => Err(e),
    };
    if outcome.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    outcome.map_err(StoreError::from)
}

fn replace_over_existing(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    let backup = sibling_with_suffix(dest, ".old")?;
    fs::rename(dest, &backup)?;
    match fs::rename(tmp, dest) {
        Ok(()) => {
            let _ = fs::remove_file(&backup);
            Ok(())
        }
        Err(e) => {
            // 新文件没落上：把旧文件挪回原位，别让一次失败的写入删掉上一次的读数。
            let _ = fs::rename(&backup, dest);
            Err(e)
        }
    }
}

fn sibling_with_suffix(path: &Path, suffix: &str) -> std::io::Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "shard path has no file name")
    })?;
    let mut tmp = name.to_os_string();
    tmp.push(format!("{suffix}-{}", std::process::id()));
    Ok(path.with_file_name(tmp))
}

fn set_mtime(path: &Path, now_ms: i64) -> Result<(), StoreError> {
    let at = UNIX_EPOCH + Duration::from_millis(now_ms.max(0) as u64);
    // Windows 的 SetFileTime 要 FILE_WRITE_ATTRIBUTES；`File::open` 只给读权限。
    let f = fs::OpenOptions::new().write(true).open(path)?;
    f.set_modified(at)?;
    Ok(())
}

fn retain(d: &Path) -> Result<(), StoreError> {
    let mut found: Vec<(SystemTime, std::ffi::OsString, PathBuf)> = Vec::new();
    for entry in fs::read_dir(d)? {
        let entry = entry?;
        let name = entry.file_name();
        if !is_shard_file(&name) {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        found.push((mtime, name, entry.path()));
    }
    if found.len() <= RETAIN_SESSIONS {
        return Ok(());
    }
    // mtime 相同时用文件名定序：同一毫秒里写 100 个会话也不会删到哪一个是随机的。
    found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let excess = found.len() - RETAIN_SESSIONS;
    let mut evicted = read_evicted(d);
    for (_, _, path) in found.into_iter().take(excess) {
        fs::remove_file(&path)?;
        evicted += 1;
        // 每删一个 +1：先落盘再删下一个，中途失败时计数不会把没删的算进去。
        fs::write(d.join(EVICTED_FILE), evicted.to_string())?;
    }
    Ok(())
}

fn read_evicted(d: &Path) -> u64 {
    fs::read_to_string(d.join(EVICTED_FILE))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}
