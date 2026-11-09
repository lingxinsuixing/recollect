//! 快照清单与状态机。**这里不碰数据库**：摘要、文件 sha、字节数都由调用方给，
//! 本模块只回答"这份内容该不该发、发第几号、谁签的字、旧指针怎么换、旧文件删哪几份"。
//! 清单签名覆盖 `protocol::manifest_canonical` 的五个字段；`bytes`/`created_at_ms` 是未签名的
//! 信封元数据——节点只信被签到的那五个。
//!
//! `CURRENT` 用"临时名 → 旧指针挪到 .bak → 新指针改名"三步替换。Windows 的 `rename` 不覆盖
//! 已存在目标，所以这不是 POSIX 意义上的单步原子替换；换来的是崩在两步之间时
//! `CURRENT` 缺失、`.bak` 还在，`load_state` 会读回旧指针，而不是把一次事故读成"没有快照"
//! （那会让下一次发布换一个新 epoch，节点把一次回退当成合法的新纪元）。

use crate::error::NetError;
use crate::key::HubKey;
use crate::protocol::{manifest_canonical, SCHEMA_MAX, SCHEMA_MIN};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const CURRENT_STATE: &str = "CURRENT";
pub const CURRENT_BACKUP: &str = "CURRENT.bak";
const CURRENT_TEMP: &str = "CURRENT.tmp";
/// 中心保留的快照份数。当前那份永不删（`retain` 的硬约束），
/// 所以磁盘上可能是 `RETAIN_COUNT` 份而不是"最新 N 份"。
pub const RETAIN_COUNT: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub epoch: u64,
    pub seq: u64,
    pub sha256: String,
    pub schema_version: i32,
    pub norm_version: u32,
    pub bytes: u64,
    pub created_at_ms: i64,
    /// base64url(no pad) 的 Ed25519 签名，签的是 `manifest_canonical` 的五个字段。
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotState {
    pub manifest: Manifest,
    /// 逻辑内容摘要：只在中心侧决定 seq，**不进清单、不上线**。
    pub digest: String,
    /// 文件名，不是路径（守卫 12：状态文件也不是吐路径的地方）。
    pub file: String,
    /// 产出这一份记忆快照时，中心的 transcript commit_seq。旧状态文件没有这一格 ⇒ 0。
    #[serde(default)]
    pub center_commit_seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publish {
    Fresh,
    Unchanged,
}

#[derive(Debug)]
pub struct SnapshotBuild<'a> {
    pub temp_db: &'a Path,
    pub sha256: &'a str,
    pub digest: &'a str,
    pub schema_version: i32,
    pub norm_version: u32,
    pub now_ms: i64,
    pub center_commit_seq: u64,
}

#[derive(Debug)]
pub enum PublishOutcome {
    Published(Manifest),
    Unchanged(Manifest),
}

pub fn snapshot_name(epoch: u64, seq: u64) -> String {
    format!("snapshot-{epoch}-{seq}.db")
}

/// 状态里的 `file` 只接受本份清单对应的规范文件名（`snapshot-<epoch>-<seq>.db`）。
/// 任何路径形状（`../`、绝对路径、别的名字）一律不解析：`CURRENT` 是本机状态文件，
/// 但被改写时不许把读取面扩成"任意文件"。`None` ⇒ 调用方按快照缺失处理。
pub fn resolved_file_name(state: &SnapshotState) -> Option<&str> {
    (state.file == snapshot_name(state.manifest.epoch, state.manifest.seq))
        .then_some(state.file.as_str())
}

fn parse_snapshot_name(name: &str) -> Option<(u64, u64)> {
    let stem = name.strip_prefix("snapshot-")?.strip_suffix(".db")?;
    let (epoch, seq) = stem.split_once('-')?;
    Some((epoch.parse().ok()?, seq.parse().ok()?))
}

/// `epoch` 初值随机（`restore` 时 +1 是 T5 的事）。保留低 63 位：JSON 里当整数读、
/// 不碰符号位；0 重抽一次。
pub fn new_epoch() -> Result<u64, NetError> {
    loop {
        let mut raw = [0u8; 8];
        getrandom::fill(&mut raw).map_err(|_| NetError::NoEntropy)?;
        let value = u64::from_le_bytes(raw) & 0x7fff_ffff_ffff_ffff;
        if value != 0 {
            return Ok(value);
        }
    }
}

/// 内容摘要相同、且 schema/norm 都没变 ⇒ 不发新快照。
/// schema/norm 要一起比：内容没变但产物换了一代 schema 时，旧文件对新一代节点是不可读的。
pub fn decide(
    prev: Option<&SnapshotState>,
    digest: &str,
    schema_version: i32,
    norm_version: u32,
) -> Publish {
    match prev {
        Some(s)
            if s.digest == digest
                && s.manifest.schema_version == schema_version
                && s.manifest.norm_version == norm_version =>
        {
            Publish::Unchanged
        }
        _ => Publish::Fresh,
    }
}

pub fn load_state(dir: &Path) -> Result<Option<SnapshotState>, NetError> {
    match read_state(&dir.join(CURRENT_STATE))? {
        Some(s) => Ok(Some(s)),
        None => read_state(&dir.join(CURRENT_BACKUP)),
    }
}

fn read_state(path: &Path) -> Result<Option<SnapshotState>, NetError> {
    let text = match std::fs::read(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(NetError::Io(e.kind())),
    };
    serde_json::from_slice(&text)
        .map(Some)
        .map_err(|_| NetError::SnapshotState)
}

pub fn save_state(dir: &Path, state: &SnapshotState) -> Result<(), NetError> {
    std::fs::create_dir_all(dir).map_err(|e| NetError::Io(e.kind()))?;
    let text = serde_json::to_vec(state).map_err(|_| NetError::SnapshotState)?;
    let temp = dir.join(CURRENT_TEMP);
    std::fs::write(&temp, &text).map_err(|e| NetError::Io(e.kind()))?;
    let current = dir.join(CURRENT_STATE);
    let backup = dir.join(CURRENT_BACKUP);
    if current.exists() {
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(&current, &backup).map_err(|e| NetError::Io(e.kind()))?;
    }
    std::fs::rename(&temp, &current).map_err(|e| NetError::Io(e.kind()))?;
    Ok(())
}

/// 发布一份已经产好的临时文件。返回的清单是**这一份**的清单；`Unchanged` 时返回旧清单，
/// 临时文件当场删掉（内容没变就不该留第二份字节）。
pub fn publish(
    dir: &Path,
    build: SnapshotBuild<'_>,
    key: &HubKey,
    keep: usize,
) -> Result<PublishOutcome, NetError> {
    let prev = load_state(dir)?;
    if matches!(
        decide(
            prev.as_ref(),
            build.digest,
            build.schema_version,
            build.norm_version
        ),
        Publish::Unchanged
    ) {
        let _ = std::fs::remove_file(build.temp_db);
        let mut previous = prev.expect("unchanged implies a previous state");
        // 记忆内容没变，但中心提交轴可能前进了：状态里的 commit_seq 要跟上，
        // 否则备份会以为这份快照仍是旧水位。
        if previous.center_commit_seq != build.center_commit_seq {
            previous.center_commit_seq = build.center_commit_seq;
            save_state(dir, &previous)?;
        }
        return Ok(PublishOutcome::Unchanged(previous.manifest));
    }
    let epoch = match prev.as_ref() {
        Some(s) => s.manifest.epoch,
        None => new_epoch()?,
    };
    let seq = match prev.as_ref() {
        Some(s) => s
            .manifest
            .seq
            .checked_add(1)
            .ok_or(NetError::SnapshotState)?,
        None => 1,
    };
    let bytes = std::fs::metadata(build.temp_db)
        .map_err(|e| NetError::Io(e.kind()))?
        .len();
    std::fs::create_dir_all(dir).map_err(|e| NetError::Io(e.kind()))?;
    let file = snapshot_name(epoch, seq);
    let final_path = dir.join(&file);
    if final_path.exists() {
        // 上一轮在 save_state 失败后留下的孤儿；它没有被 CURRENT 指过，删掉是安全的。
        std::fs::remove_file(&final_path).map_err(|e| NetError::Io(e.kind()))?;
    }
    std::fs::rename(build.temp_db, &final_path).map_err(|e| NetError::Io(e.kind()))?;
    let signature = key.sign_b64(
        manifest_canonical(epoch, seq, build.sha256, build.schema_version, build.norm_version)
            .as_bytes(),
    );
    let manifest = Manifest {
        epoch,
        seq,
        sha256: build.sha256.to_string(),
        schema_version: build.schema_version,
        norm_version: build.norm_version,
        bytes,
        created_at_ms: build.now_ms,
        signature,
    };
    let state = SnapshotState {
        manifest: manifest.clone(),
        digest: build.digest.to_string(),
        file,
        center_commit_seq: build.center_commit_seq,
    };
    if let Err(e) = save_state(dir, &state) {
        let _ = std::fs::remove_file(&final_path);
        return Err(e);
    }
    // 轮转失败不改写这次发布的成功：新指针已经落盘，旧文件删不掉只是磁盘多占一份。
    let _ = retain(dir, keep, &state.file);
    Ok(PublishOutcome::Published(manifest))
}

/// 只保留最近 `keep` 份，**当前那份永不删**（哪怕它按序号是最旧的）。
/// 文件名认不出的条目一律不碰（临时文件、人的杂物都不归它管）。
pub fn retain(dir: &Path, keep: usize, current: &str) -> Result<Vec<String>, NetError> {
    let mut files: Vec<(u64, u64, String)> = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(NetError::Io(e.kind())),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some((epoch, seq)) = parse_snapshot_name(&name) {
            files.push((epoch, seq, name));
        }
    }
    files.sort();
    let total = files.len();
    let mut removed = Vec::new();
    for (i, (_, _, name)) in files.into_iter().enumerate() {
        let in_newest = i + keep >= total;
        if in_newest || name == current {
            continue;
        }
        match std::fs::remove_file(dir.join(&name)) {
            Ok(()) => removed.push(name),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(NetError::Io(e.kind())),
        }
    }
    Ok(removed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptOutcome {
    /// 这一份比本地新，要下载。
    New,
    /// 与本地完全相同：不下载，也不算错。
    AlreadyCurrent,
}

/// 节点侧的清单接受判定。四道闸的顺序是判定的一部分：
/// 1. **先验签**——schema/norm 也在签名覆盖的五个字段里，先比它们等于让未签名字段探路；
/// 2. schema 必须落在本产物支持的区间；3. norm 必须与本产物一致；
/// 4. 最后才比单调性：(epoch, seq) 只许前进，同号只许同字节。
///
/// 同号的两种结局必须分开：同 sha ⇒ `AlreadyCurrent`（幂等）；
/// 同号不同字节 ⇒ `EpochSeqConflict`——中心重签了一份内容不同的快照，
/// 节点不能自己挑一个，也不能静默覆盖。
pub fn accept_manifest(
    hub_pub: &str,
    manifest: &Manifest,
    current: Option<&Manifest>,
    expected_norm_version: u32,
) -> Result<AcceptOutcome, NetError> {
    let canonical = manifest_canonical(
        manifest.epoch,
        manifest.seq,
        &manifest.sha256,
        manifest.schema_version,
        manifest.norm_version,
    );
    if !crate::key::verify_b64(hub_pub, canonical.as_bytes(), &manifest.signature) {
        return Err(NetError::Signature);
    }
    // sha 是被签过的字段，但"签过"不等于"形状对"。小写 64 hex 是唯一合法形；
    // 大写或长度不对的串会让下游的十六进制比较静默失配，所以在进状态机之前拦掉。
    if !is_lower_hex(&manifest.sha256) {
        return Err(NetError::SnapshotState);
    }
    if manifest.schema_version < SCHEMA_MIN || manifest.schema_version > SCHEMA_MAX {
        return Err(NetError::Schema);
    }
    if manifest.norm_version != expected_norm_version {
        return Err(NetError::Norm);
    }
    let Some(cur) = current else {
        return Ok(AcceptOutcome::New);
    };
    if manifest.epoch > cur.epoch {
        return Ok(AcceptOutcome::New);
    }
    if manifest.epoch < cur.epoch {
        return Err(NetError::StaleEpoch);
    }
    if manifest.seq > cur.seq {
        return Ok(AcceptOutcome::New);
    }
    if manifest.seq < cur.seq {
        return Err(NetError::StaleSeq);
    }
    if manifest.sha256 == cur.sha256
        && manifest.schema_version == cur.schema_version
        && manifest.norm_version == cur.norm_version
    {
        Ok(AcceptOutcome::AlreadyCurrent)
    } else {
        Err(NetError::EpochSeqConflict)
    }
}

fn is_lower_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod resolve_file_tests {
    use super::*;

    fn state(file: &str, epoch: u64, seq: u64) -> SnapshotState {
        SnapshotState {
            manifest: Manifest {
                epoch,
                seq,
                sha256: "0".repeat(64),
                schema_version: 1,
                norm_version: 1,
                bytes: 1,
                created_at_ms: 0,
                signature: String::new(),
            },
            digest: String::new(),
            file: file.to_string(),
            center_commit_seq: 0,
        }
    }

    #[test]
    fn resolved_file_name_accepts_only_the_manifest_name() {
        assert_eq!(
            resolved_file_name(&state("snapshot-7-3.db", 7, 3)),
            Some("snapshot-7-3.db")
        );
        for bad in [
            "../hub.key",
            "snapshot-7-3.db/../../hub.key",
            "C:/x.db",
            "snapshot-7-4.db",
            "snapshot-7-3.dbx",
            "",
        ] {
            assert_eq!(resolved_file_name(&state(bad, 7, 3)), None, "{bad}");
        }
    }
}
