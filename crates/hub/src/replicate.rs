//! 中心增量复制：按 `commit_seq` 拉取提交记录。**只读**，不分配提交号。
//!
//! 一条记录 = 一个 `ingest_log` 行 + 它对应的 session 元数据 + 那一批 turn 行。
//! 记录自带 `content_sha256`，备份侧 apply 时会重算；导出侧也会先自检一次，
//! 防止把“日志说 N 行、实际只有 M 行”的损坏记录交给备份。

use rc_ledger::transcript::{commit_sha256, tombstone_sha256, CommitTurn};
use rc_net::snapshot::Manifest;
use rc_store::{LedgerHandle, Root, StoreError, TranscriptHandle};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaTurn {
    pub turn_uid: String,
    pub role: String,
    pub body: String,
    pub body_fp: String,
    pub turn_key_tier: u8,
    pub created_at_ms: i64,
    pub ingested_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaRecord {
    pub commit_seq: u64,
    pub epoch: u64,
    pub op: String,
    pub session_key: String,
    pub device_id: String,
    pub source_agent: String,
    pub machine: Option<String>,
    pub project_hint: Option<String>,
    pub last_saved_at_ms: Option<i64>,
    // S1b-C1：会话归属 7 列里除 `scope_kind`/`scope_key` 之外的 5 格（`sessions` v5）。
    // **全部 `#[serde(default)]`**：旧中心写出的记录 JSON 没有这些键 ⇒ `None`，
    // 备份侧的 upsert 走 `COALESCE`，不清空已归属行。
    // 另两格复用下面既有的 `scope_kind`/`scope_key`：一条记录的 `scope_*` 就是"本记录作用域"——
    // import 记录里它是会话归属的 scope，purge 记录里它是被删的作用域（同一对键，JSON 里
    // 不能出现两份同名列）；apply 按 `op` 分派，import 分支不读它们做墓碑判定。
    #[serde(default)]
    pub owner_principal_id: Option<String>,
    #[serde(default)]
    pub owner_agent_id: Option<String>,
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub retention: Option<String>,
    #[serde(default)]
    pub ownership_src: Option<String>,
    pub turns: Vec<ReplicaTurn>,
    pub content_sha256: String,
    pub created_at_ms: i64,
    /// 本记录的作用域：purge 记录必有；import 记录是会话归属的 scope（S1b-C1 起带上）。
    #[serde(default)]
    pub scope_kind: Option<String>,
    #[serde(default)]
    pub scope_key: Option<String>,
    #[serde(default)]
    pub turns_removed: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ReplicaStatus {
    pub epoch: u64,
    pub last_commit_seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ApplyStatus {
    pub last_applied_seq: u64,
    pub last_epoch: u64,
    pub last_verified_at_ms: Option<i64>,
}

/// 回执超过这个时长不再作保。备份静默死掉（没被撤销、也没再回执）时，
/// 设备不该继续拿一条旧回执删本地副本；调度器每轮回执，正常备份不会过期。
pub const ACK_FRESH_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// 一条备份回执。`active=false` 点名的是“这台设备已经不在编”（撤销或查不到），
/// `fresh=false` 点名的是“还在编，但已经超过 `ACK_FRESH_MS` 没有回执”。
/// 两者都不参与 `replicated_seq`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplicaAckView {
    pub device_id: String,
    pub applied_seq: u64,
    pub updated_at_ms: i64,
    pub active: bool,
    pub fresh: bool,
}

/// 中心对“备份落到了哪一格”的读数。`replicated_seq` 只取**当前 epoch、仍在编、
/// 且回执未过期**的最大值：一份备份曾经落地过，就算它后来停摆，那批数据在它盘上仍然存在；
/// 但设备被撤销（机器丢失/退役）或长期没有回执后，不能再拿它的旧回执让设备删本地副本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AckStatus {
    pub epoch: u64,
    pub last_commit_seq: u64,
    pub replicated_seq: u64,
    pub acks: Vec<ReplicaAckView>,
}

#[derive(Debug)]
pub enum AckError {
    Store(StoreError),
    /// 回执声称落地了中心还没提交的一格：这不是“落后”，是读数自相矛盾。
    BeyondCenter { applied_seq: u64, last_commit_seq: u64 },
}

impl From<StoreError> for AckError {
    fn from(error: StoreError) -> Self {
        AckError::Store(error)
    }
}

impl From<rusqlite::Error> for AckError {
    fn from(error: rusqlite::Error) -> Self {
        AckError::Store(StoreError::from(error))
    }
}

#[derive(Debug)]
pub enum ApplyError {
    Store(StoreError),
    /// 记录自身不自洽（哈希/字段/已有 turn 内容冲突）。
    BadRecord(&'static str),
    /// 同一个 commit_seq 已落地过，但内容哈希不同。
    Conflict(u64),
    /// 备份还缺前一提交；不跳号应用。
    Gap { expected: u64, got: u64 },
    /// 这条导入落在一条更早的 purge 墓碑之后：拒绝复活，停止应用等人看。
    Tombstoned(u64),
}

impl From<StoreError> for ApplyError {
    fn from(error: StoreError) -> Self {
        ApplyError::Store(error)
    }
}

impl From<rusqlite::Error> for ApplyError {
    fn from(error: rusqlite::Error) -> Self {
        ApplyError::Store(StoreError::from(error))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplyOutcome {
    pub applied: usize,
    pub skipped: usize,
    pub last_applied_seq: u64,
    pub last_epoch: u64,
}

/// 备份侧才需要这两张表：中心库不需要，故不放进全局 transcript DDL。
/// `apply`/`status` 都会幂等建表；建表本身不分配提交号、不动中心数据。
pub fn ensure_apply_tables(t: &TranscriptHandle) -> Result<(), StoreError> {
    t.conn().execute_batch(
        "CREATE TABLE IF NOT EXISTS replication_state(
            id INTEGER PRIMARY KEY CHECK(id=1),
            last_applied_seq INTEGER NOT NULL DEFAULT 0,
            last_epoch INTEGER NOT NULL DEFAULT 0,
            last_verified_at_ms INTEGER);
         INSERT OR IGNORE INTO replication_state(id, last_applied_seq, last_epoch)
            VALUES(1, 0, 0);
         CREATE TABLE IF NOT EXISTS replicated_commit(
            commit_seq INTEGER PRIMARY KEY,
            epoch INTEGER NOT NULL,
            content_sha256 TEXT NOT NULL,
            applied_at_ms INTEGER NOT NULL);",
    )?;
    Ok(())
}

pub fn apply_status(t: &TranscriptHandle) -> Result<ApplyStatus, StoreError> {
    ensure_apply_tables(t)?;
    let (last_applied_seq, last_epoch, last_verified_at_ms): (i64, i64, Option<i64>) =
        t.conn().query_row(
            "SELECT last_applied_seq, last_epoch, last_verified_at_ms
             FROM replication_state WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
    Ok(ApplyStatus {
        last_applied_seq: last_applied_seq as u64,
        last_epoch: last_epoch as u64,
        last_verified_at_ms,
    })
}

fn validate_record(record: &ReplicaRecord) -> Result<(), ApplyError> {
    match record.op.as_str() {
        "import" => validate_import(record),
        "purge" => {
            let kind = record.scope_kind.as_deref().ok_or(ApplyError::BadRecord("scope_kind"))?;
            let key = record.scope_key.as_deref().ok_or(ApplyError::BadRecord("scope_key"))?;
            if !matches!(kind, "session" | "device") || key.is_empty() {
                return Err(ApplyError::BadRecord("scope"));
            }
            if record.turns_removed == 0 || !record.turns.is_empty() {
                return Err(ApplyError::BadRecord("tombstone"));
            }
            if tombstone_sha256(kind, key, record.turns_removed) != record.content_sha256 {
                return Err(ApplyError::BadRecord("content_sha256"));
            }
            Ok(())
        }
        _ => Err(ApplyError::BadRecord("op")),
    }
}

fn validate_import(record: &ReplicaRecord) -> Result<(), ApplyError> {
    if record.turns.is_empty() {
        return Err(ApplyError::BadRecord("turns"));
    }
    for turn in &record.turns {
        if turn.turn_uid.is_empty()
            || !matches!(turn.role.as_str(), "human" | "agent" | "system")
            || !(1..=3).contains(&turn.turn_key_tier)
            || turn.body_fp.is_empty()
        {
            return Err(ApplyError::BadRecord("turn"));
        }
    }
    let commit_turns: Vec<CommitTurn<'_>> = record
        .turns
        .iter()
        .map(|turn| CommitTurn {
            turn_uid: turn.turn_uid.as_str(),
            role: turn.role.as_str(),
            body: turn.body.as_str(),
            body_fp: turn.body_fp.as_str(),
            created_at_ms: turn.created_at_ms,
            ingested_at_ms: turn.ingested_at_ms,
        })
        .collect();
    if commit_sha256(&commit_turns) != record.content_sha256 {
        return Err(ApplyError::BadRecord("content_sha256"));
    }
    Ok(())
}

/// 幂等 apply：按 commit_seq 严格顺序落地；同 seq 同 sha 跳过，同 seq 不同 sha 冲突，
/// 跳号拒绝。每条记录在自己的事务里，成功一条推进一格 `last_applied_seq`。
pub fn apply_records(
    t: &TranscriptHandle,
    records: &[ReplicaRecord],
    now_ms: i64,
) -> Result<ApplyOutcome, ApplyError> {
    ensure_apply_tables(t)?;
    let (mut last_applied, mut last_epoch): (u64, u64) = {
        let (seq, epoch): (i64, i64) = t.conn().query_row(
            "SELECT last_applied_seq, last_epoch FROM replication_state WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        (seq as u64, epoch as u64)
    };
    let mut applied = 0usize;
    let mut skipped = 0usize;

    for record in records {
        validate_record(record)?;
        if record.commit_seq <= last_applied {
            let existing: Option<(String, i64)> = {
                let mut st = t.conn().prepare(
                    "SELECT content_sha256, epoch FROM replicated_commit WHERE commit_seq=?1",
                )?;
                let mut rows = st.query_map(rusqlite::params![record.commit_seq as i64], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?;
                match rows.next() {
                    Some(Ok(row)) => Some(row),
                    Some(Err(error)) => return Err(error.into()),
                    None => None,
                }
            };
            match existing {
                Some((sha, _)) if sha == record.content_sha256 => {
                    skipped += 1;
                    continue;
                }
                Some(_) => return Err(ApplyError::Conflict(record.commit_seq)),
                None => return Err(ApplyError::BadRecord("replication state")),
            }
        }
        if record.commit_seq != last_applied + 1 {
            return Err(ApplyError::Gap {
                expected: last_applied + 1,
                got: record.commit_seq,
            });
        }

        if record.op == "purge" {
            apply_purge(t, record, now_ms)?;
            last_applied = record.commit_seq;
            last_epoch = record.epoch;
            applied += 1;
            continue;
        }
        // 先前的 purge 挡住后到的同 scope 导入：备份不能把已删会话从旧记录里复活。
        if rc_ledger::transcript::tombstone_blocks(
            t,
            &record.session_key,
            &record.device_id,
            Some(record.commit_seq),
        )? {
            return Err(ApplyError::Tombstoned(record.commit_seq));
        }

        let conn = t.conn();
        let tx = conn.unchecked_transaction()?;
        // S1b-C1：归属 7 列随记录落地。旧记录（7 格全 None）插入新会话时落 NULL，
        // 读侧默认语义见设计 §1.2；冲突分支逐列 `COALESCE(旧, 新)` ⇒ 旧记录不清空已归属行。
        tx.execute(
            "INSERT INTO sessions(session_key, device_id, source_agent, machine, project_hint,
                                  last_saved_at_ms, owner_principal_id, owner_agent_id,
                                  scope_kind, scope_key, visibility, retention, ownership_src)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
             ON CONFLICT(session_key, device_id) DO UPDATE SET
               source_agent=excluded.source_agent,
               machine=excluded.machine,
               project_hint=excluded.project_hint,
               last_saved_at_ms=max(last_saved_at_ms, excluded.last_saved_at_ms),
               owner_principal_id=COALESCE(sessions.owner_principal_id, excluded.owner_principal_id),
               owner_agent_id=COALESCE(sessions.owner_agent_id, excluded.owner_agent_id),
               scope_kind=COALESCE(sessions.scope_kind, excluded.scope_kind),
               scope_key=COALESCE(sessions.scope_key, excluded.scope_key),
               visibility=COALESCE(sessions.visibility, excluded.visibility),
               retention=COALESCE(sessions.retention, excluded.retention),
               ownership_src=COALESCE(sessions.ownership_src, excluded.ownership_src)",
            rusqlite::params![
                record.session_key,
                record.device_id,
                record.source_agent,
                record.machine,
                record.project_hint,
                record.last_saved_at_ms.unwrap_or(record.created_at_ms),
                record.owner_principal_id,
                record.owner_agent_id,
                record.scope_kind,
                record.scope_key,
                record.visibility,
                record.retention,
                record.ownership_src
            ],
        )?;
        for turn in &record.turns {
            let changed = tx.execute(
                "INSERT OR IGNORE INTO turns(turn_uid, session_key, device_id, role, body, body_fp,
                                            turn_key_tier, project_hint, created_at_ms, ingested_at_ms,
                                            commit_seq)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                rusqlite::params![
                    turn.turn_uid,
                    record.session_key,
                    record.device_id,
                    turn.role,
                    turn.body,
                    turn.body_fp,
                    turn.turn_key_tier,
                    record.project_hint,
                    turn.created_at_ms,
                    turn.ingested_at_ms,
                    record.commit_seq as i64
                ],
            )?;
            if changed == 0 {
                let existing: (String, String, i64, u8) = tx.query_row(
                    "SELECT role, body_fp, created_at_ms, turn_key_tier
                     FROM turns WHERE turn_uid=?1",
                    rusqlite::params![turn.turn_uid],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )?;
                if existing
                    != (
                        turn.role.clone(),
                        turn.body_fp.clone(),
                        turn.created_at_ms,
                        turn.turn_key_tier,
                    )
                {
                    return Err(ApplyError::BadRecord("turn exists with different content"));
                }
            }
        }
        tx.execute(
            "INSERT INTO replicated_commit(commit_seq, epoch, content_sha256, applied_at_ms)
             VALUES(?1,?2,?3,?4)",
            rusqlite::params![
                record.commit_seq as i64,
                record.epoch as i64,
                record.content_sha256,
                now_ms
            ],
        )?;
        // 备份接管后要能继续作为中心导出：import 记录也补进 ingest_log。
        tx.execute(
            "INSERT OR IGNORE INTO ingest_log(commit_seq, epoch, op, session_key, device_id,
                                              turns_count, content_sha256, created_at_ms)
             VALUES(?1,?2,'import',?3,?4,?5,?6,?7)",
            rusqlite::params![
                record.commit_seq as i64,
                record.epoch as i64,
                record.session_key,
                record.device_id,
                record.turns.len() as i64,
                record.content_sha256,
                record.created_at_ms
            ],
        )?;
        tx.execute(
            "UPDATE replication_state
             SET last_applied_seq=?1, last_epoch=?2, last_verified_at_ms=?3
             WHERE id=1",
            rusqlite::params![record.commit_seq as i64, record.epoch as i64, now_ms],
        )?;
        // 备份自己也持有一份中心态：接管时从同一格继续，不从 0 重来。
        tx.execute(
            "UPDATE center_state SET epoch=?1, last_commit_seq=?2 WHERE id=1",
            rusqlite::params![record.epoch as i64, record.commit_seq as i64],
        )?;
        tx.commit()?;
        last_applied = record.commit_seq;
        last_epoch = record.epoch;
        applied += 1;
    }

    Ok(ApplyOutcome {
        applied,
        skipped,
        last_applied_seq: last_applied,
        last_epoch,
    })
}

fn apply_purge(
    t: &TranscriptHandle,
    record: &ReplicaRecord,
    now_ms: i64,
) -> Result<(), ApplyError> {
    let kind = record
        .scope_kind
        .as_deref()
        .ok_or(ApplyError::BadRecord("scope_kind"))?;
    let key = record
        .scope_key
        .as_deref()
        .ok_or(ApplyError::BadRecord("scope_key"))?;
    let column = match kind {
        "session" => "session_key",
        "device" => "device_id",
        _ => return Err(ApplyError::BadRecord("scope_kind")),
    };
    let conn = t.conn();
    let tx = conn.unchecked_transaction()?;
    let deleted = tx.execute(
        &format!("DELETE FROM turns WHERE {column}=?1"),
        rusqlite::params![key],
    )?;
    if deleted as u64 != record.turns_removed {
        return Err(ApplyError::BadRecord("tombstone count"));
    }
    tx.execute(
        "INSERT INTO tombstone(commit_seq, epoch, scope_kind, scope_key,
                               turns_removed, content_sha256, created_at_ms)
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
        rusqlite::params![
            record.commit_seq as i64,
            record.epoch as i64,
            kind,
            key,
            record.turns_removed as i64,
            record.content_sha256,
            record.created_at_ms
        ],
    )?;
    tx.execute(
        "INSERT INTO replicated_commit(commit_seq, epoch, content_sha256, applied_at_ms)
         VALUES(?1,?2,?3,?4)",
        rusqlite::params![
            record.commit_seq as i64,
            record.epoch as i64,
            record.content_sha256,
            now_ms
        ],
    )?;
    tx.execute(
        "UPDATE replication_state
         SET last_applied_seq=?1, last_epoch=?2, last_verified_at_ms=?3
         WHERE id=1",
        rusqlite::params![record.commit_seq as i64, record.epoch as i64, now_ms],
    )?;
    tx.execute(
        "UPDATE center_state SET epoch=?1, last_commit_seq=?2 WHERE id=1",
        rusqlite::params![record.epoch as i64, record.commit_seq as i64],
    )?;
    tx.commit()?;
    Ok(())
}

pub const LEDGER_SNAPSHOT_FORMAT_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceExport {
    pub device_id: String,
    pub pubkey: String,
    pub token_hash: String,
    pub paired_at_ms: i64,
    pub revoked_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityExport {
    pub device_id: String,
    pub capability: String,
    pub granted_at_ms: i64,
    pub revoked_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditExport {
    pub id: i64,
    pub at_ms: i64,
    pub actor: String,
    pub action: String,
    pub subject: Option<String>,
    pub detail: String,
}

/// 处理水位：接管后必须知道“蒸到哪/推到哪”。只带游标，不带正文。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatermarkExport {
    pub family: String,
    pub scope_key: String,
    pub cursor_at_ms: i64,
    pub cursor_turn_uid: String,
    pub attempts: i64,
    pub last_success_at_ms: Option<i64>,
    pub halted_at_ms: Option<i64>,
}

/// 每 turn 的抽取态：`extracted` 的 turn 不重抽，`rejected/worthless` 不复活。
///
/// S1：`extraction_fingerprint` 是"这一轮是谁产出的"（64 位 hex 或 `None`）。
/// `skip_serializing_if` 是**升级兼容的开关**：旧快照（全 `None`）序列化出来的字节与升级前
/// **逐字节相同**，于是 `ledger_snapshot_sha256` 的输入算出来的还是同一个 sha——
/// 旧快照仍过校验。`default` 让没有这个键的旧 JSON 反序列化得出来。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractStateExport {
    pub turn_uid: String,
    pub state: String,
    pub reason: Option<String>,
    pub at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extraction_fingerprint: Option<String>,
}

/// H7c2：审查轴水位（每个 pass 一行）。**不是**处理游标（[`WatermarkExport`] 是"蒸到哪"），
/// 而是"这个 pass 审到哪一条判定 + 三态计数"：`max_decision_id` 与本地
/// `review_decision` 的最大 id 一比，接管后"审查差多少条"就有数了。
///
/// `updated_at_ms` 也带：接管时能看出这份水位有多旧，不必猜。
/// 载荷/判定 detail/原因码一个字节都不在这里（正文不进 ledger，见 `rc_ledger::review_mark`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewWatermarkExport {
    pub pass_id: String,
    pub max_decision_id: i64,
    pub pending_review: i64,
    pub quarantined: i64,
    pub applied: i64,
    pub updated_at_ms: i64,
}

/// 授权/审计的复制快照。**不含 raw token**：只有 token 哈希与公钥。
/// `commit_seq` 是中心转录轴；`snapshot_sha256` 覆盖内容，使“同 seq 但能力变了”
/// 也能被 apply 识别，而不是因为 seq 相同被跳过。
/// v2 起带 `watermarks` 与 `extract_states`：接管后不重蒸/不跳过。
/// H7c2 再加 `review_watermarks`——同样 `default` + 空则不出键，旧快照的 JSON 与 sha
/// **逐字节不变**（见 [`ledger_snapshot_sha256`]）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerSnapshot {
    pub format_version: u32,
    pub commit_seq: u64,
    pub epoch: u64,
    pub devices: Vec<DeviceExport>,
    pub capabilities: Vec<CapabilityExport>,
    pub audits: Vec<AuditExport>,
    #[serde(default)]
    pub watermarks: Vec<WatermarkExport>,
    #[serde(default)]
    pub extract_states: Vec<ExtractStateExport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub review_watermarks: Vec<ReviewWatermarkExport>,
    pub snapshot_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LedgerApplyStatus {
    pub commit_seq: u64,
    pub snapshot_sha256: String,
    pub last_verified_at_ms: Option<i64>,
}

#[derive(Debug)]
pub enum LedgerApplyError {
    Store(StoreError),
    BadSnapshot(&'static str),
    Conflict(&'static str),
}

impl From<StoreError> for LedgerApplyError {
    fn from(error: StoreError) -> Self {
        LedgerApplyError::Store(error)
    }
}

impl From<rusqlite::Error> for LedgerApplyError {
    fn from(error: rusqlite::Error) -> Self {
        LedgerApplyError::Store(StoreError::from(error))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerApplyOutcome {
    pub applied: bool,
    pub devices: usize,
    pub capabilities: usize,
    pub audits: usize,
    pub watermarks: usize,
    pub extract_states: usize,
    /// H7c2：本次写入的审查轴水位行数（`DELETE` 后逐行插入的条数）。
    pub review_watermarks: usize,
    pub commit_seq: u64,
}

/// 快照内容哈希。
///
/// H7c2 的兼容纪律：`review_watermarks` **只在非空时**进 preimage。空时走的是与 H7c2
/// 之前逐字节相同的那 8 元组——旧备份/旧快照的 `snapshot_sha256` 仍过校验。
fn ledger_snapshot_sha256(snapshot: &LedgerSnapshot) -> Result<String, LedgerApplyError> {
    let bytes = if snapshot.review_watermarks.is_empty() {
        serde_json::to_vec(&(
            snapshot.format_version,
            snapshot.commit_seq,
            snapshot.epoch,
            &snapshot.devices,
            &snapshot.capabilities,
            &snapshot.audits,
            &snapshot.watermarks,
            &snapshot.extract_states,
        ))
    } else {
        serde_json::to_vec(&(
            snapshot.format_version,
            snapshot.commit_seq,
            snapshot.epoch,
            &snapshot.devices,
            &snapshot.capabilities,
            &snapshot.audits,
            &snapshot.watermarks,
            &snapshot.extract_states,
            &snapshot.review_watermarks,
        ))
    }
    .map_err(|_| LedgerApplyError::BadSnapshot("encode"))?;
    Ok(rc_net::protocol::sha256_hex(&bytes))
}

pub fn export_ledger(root: &Root) -> Result<LedgerSnapshot, StoreError> {
    let status = status(root.transcript())?;
    let devices = {
        let mut st = root.ledger().conn().prepare(
            "SELECT device_id, pubkey, token_hash, paired_at_ms, revoked_at_ms
             FROM device ORDER BY device_id",
        )?;
        let rows = st.query_map([], |r| {
            Ok(DeviceExport {
                device_id: r.get(0)?,
                pubkey: r.get(1)?,
                token_hash: r.get(2)?,
                paired_at_ms: r.get(3)?,
                revoked_at_ms: r.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let capabilities = {
        let mut st = root.ledger().conn().prepare(
            "SELECT device_id, capability, granted_at_ms, revoked_at_ms
             FROM device_capability ORDER BY device_id, capability",
        )?;
        let rows = st.query_map([], |r| {
            Ok(CapabilityExport {
                device_id: r.get(0)?,
                capability: r.get(1)?,
                granted_at_ms: r.get(2)?,
                revoked_at_ms: r.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let audits = {
        let mut st = root.ledger().conn().prepare(
            "SELECT id, at_ms, actor, action, subject, detail FROM audit_log ORDER BY id",
        )?;
        let rows = st.query_map([], |r| {
            Ok(AuditExport {
                id: r.get(0)?,
                at_ms: r.get(1)?,
                actor: r.get(2)?,
                action: r.get(3)?,
                subject: r.get(4)?,
                detail: r.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let watermarks = {
        let mut st = root.ledger().conn().prepare(
            "SELECT family, scope_key, cursor_at_ms, cursor_turn_uid, attempts,
                    last_success_at_ms, halted_at_ms
             FROM job_watermark ORDER BY family, scope_key",
        )?;
        let rows = st.query_map([], |r| {
            Ok(WatermarkExport {
                family: r.get(0)?,
                scope_key: r.get(1)?,
                cursor_at_ms: r.get(2)?,
                cursor_turn_uid: r.get(3)?,
                attempts: r.get(4)?,
                last_success_at_ms: r.get(5)?,
                halted_at_ms: r.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let extract_states = {
        let mut st = root.ledger().conn().prepare(
            "SELECT turn_uid, state, reason, at_ms, extraction_fingerprint
             FROM extract_state ORDER BY turn_uid",
        )?;
        let rows = st.query_map([], |r| {
            Ok(ExtractStateExport {
                turn_uid: r.get(0)?,
                state: r.get(1)?,
                reason: r.get(2)?,
                at_ms: r.get(3)?,
                extraction_fingerprint: r.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    // H7c2：审查轴水位随账本快照走（只读，顺序与 `review_mark::all` 同一把尺子）。
    let review_watermarks: Vec<ReviewWatermarkExport> =
        rc_ledger::review_mark::all(root.ledger())?
            .into_iter()
            .map(|w| ReviewWatermarkExport {
                pass_id: w.pass_id,
                max_decision_id: w.max_decision_id,
                pending_review: w.pending_review,
                quarantined: w.quarantined,
                applied: w.applied,
                updated_at_ms: w.updated_at_ms,
            })
            .collect();
    let mut snapshot = LedgerSnapshot {
        format_version: LEDGER_SNAPSHOT_FORMAT_VERSION,
        commit_seq: status.last_commit_seq,
        epoch: status.epoch,
        devices,
        capabilities,
        audits,
        watermarks,
        extract_states,
        review_watermarks,
        snapshot_sha256: String::new(),
    };
    snapshot.snapshot_sha256 = ledger_snapshot_sha256(&snapshot)
        .map_err(|_| StoreError::Io("the ledger snapshot could not be hashed".to_string()))?;
    Ok(snapshot)
}

fn ensure_ledger_apply_state(l: &LedgerHandle) -> Result<(), StoreError> {
    l.conn().execute_batch(
        "CREATE TABLE IF NOT EXISTS ledger_replication_state(
            id INTEGER PRIMARY KEY CHECK(id=1),
            last_commit_seq INTEGER NOT NULL DEFAULT 0,
            last_snapshot_sha256 TEXT NOT NULL DEFAULT '',
            last_verified_at_ms INTEGER);
         INSERT OR IGNORE INTO ledger_replication_state(id, last_commit_seq, last_snapshot_sha256)
            VALUES(1, 0, '');",
    )?;
    Ok(())
}

pub fn ledger_apply_status(l: &LedgerHandle) -> Result<LedgerApplyStatus, StoreError> {
    ensure_ledger_apply_state(l)?;
    let (commit_seq, snapshot_sha256, last_verified_at_ms): (i64, String, Option<i64>) =
        l.conn().query_row(
            "SELECT last_commit_seq, last_snapshot_sha256, last_verified_at_ms
             FROM ledger_replication_state WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
    Ok(LedgerApplyStatus {
        commit_seq: commit_seq as u64,
        snapshot_sha256,
        last_verified_at_ms,
    })
}

pub fn apply_ledger(
    l: &mut LedgerHandle,
    snapshot: &LedgerSnapshot,
    now_ms: i64,
) -> Result<LedgerApplyOutcome, LedgerApplyError> {
    if snapshot.format_version != LEDGER_SNAPSHOT_FORMAT_VERSION {
        return Err(LedgerApplyError::BadSnapshot("format_version"));
    }
    if ledger_snapshot_sha256(snapshot)? != snapshot.snapshot_sha256 {
        return Err(LedgerApplyError::BadSnapshot("snapshot_sha256"));
    }
    ensure_ledger_apply_state(l)?;
    let (last_seq, last_sha): (i64, String) = l.conn().query_row(
        "SELECT last_commit_seq, last_snapshot_sha256 FROM ledger_replication_state WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if snapshot.commit_seq < last_seq as u64
        || (snapshot.commit_seq == last_seq as u64 && snapshot.snapshot_sha256 == last_sha)
    {
        return Ok(LedgerApplyOutcome {
            applied: false,
            devices: 0,
            capabilities: 0,
            audits: 0,
            watermarks: 0,
            extract_states: 0,
            review_watermarks: 0,
            commit_seq: last_seq as u64,
        });
    }

    let tx = l.conn().unchecked_transaction()?;
    for device in &snapshot.devices {
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT pubkey, token_hash FROM device WHERE device_id=?1",
                [&device.device_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((pubkey, token_hash)) = existing {
            if pubkey != device.pubkey || token_hash != device.token_hash {
                return Err(LedgerApplyError::Conflict("device"));
            }
        } else {
            let owner: Option<String> = tx
                .query_row(
                    "SELECT device_id FROM device WHERE token_hash=?1",
                    [&device.token_hash],
                    |r| r.get(0),
                )
                .optional()?;
            if owner.is_some() {
                return Err(LedgerApplyError::Conflict("token_hash"));
            }
        }
        tx.execute(
            "INSERT INTO device(device_id, pubkey, token_hash, paired_at_ms, revoked_at_ms)
             VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(device_id) DO UPDATE SET
               pubkey=excluded.pubkey,
               token_hash=excluded.token_hash,
               paired_at_ms=excluded.paired_at_ms,
               revoked_at_ms=excluded.revoked_at_ms",
            rusqlite::params![
                device.device_id,
                device.pubkey,
                device.token_hash,
                device.paired_at_ms,
                device.revoked_at_ms
            ],
        )?;
    }
    for capability in &snapshot.capabilities {
        tx.execute(
            "INSERT INTO device_capability(device_id, capability, granted_at_ms, revoked_at_ms)
             VALUES(?1,?2,?3,?4)
             ON CONFLICT(device_id, capability) DO UPDATE SET
               granted_at_ms=excluded.granted_at_ms,
               revoked_at_ms=excluded.revoked_at_ms",
            rusqlite::params![
                capability.device_id,
                capability.capability,
                capability.granted_at_ms,
                capability.revoked_at_ms
            ],
        )?;
    }
    for audit in &snapshot.audits {
        tx.execute(
            "INSERT OR IGNORE INTO audit_log(id, at_ms, actor, action, subject, detail)
             VALUES(?1,?2,?3,?4,?5,?6)",
            rusqlite::params![
                audit.id,
                audit.at_ms,
                audit.actor,
                audit.action,
                audit.subject,
                audit.detail
            ],
        )?;
    }
    for watermark in &snapshot.watermarks {
        tx.execute(
            "INSERT INTO job_watermark(family, scope_key, cursor_at_ms, cursor_turn_uid,
                                      attempts, last_success_at_ms, halted_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(family, scope_key) DO UPDATE SET
               cursor_at_ms=excluded.cursor_at_ms,
               cursor_turn_uid=excluded.cursor_turn_uid,
               attempts=excluded.attempts,
               last_success_at_ms=excluded.last_success_at_ms,
               halted_at_ms=excluded.halted_at_ms",
            rusqlite::params![
                watermark.family,
                watermark.scope_key,
                watermark.cursor_at_ms,
                watermark.cursor_turn_uid,
                watermark.attempts,
                watermark.last_success_at_ms,
                watermark.halted_at_ms
            ],
        )?;
    }
    for state in &snapshot.extract_states {
        // S1：`COALESCE(excluded, existing)` —— 快照里这一格是 NULL（旧快照 / 中心没盖过章）
        // 时**保留备份自己已有的指纹**；旧快照因此不该把备份的身份抹掉。反过来，快照带值就覆盖。
        tx.execute(
            "INSERT INTO extract_state(turn_uid, state, reason, at_ms, extraction_fingerprint)
             VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(turn_uid) DO UPDATE SET
               state=excluded.state,
               reason=excluded.reason,
               at_ms=excluded.at_ms,
               extraction_fingerprint=coalesce(excluded.extraction_fingerprint,
                                               extract_state.extraction_fingerprint)",
            rusqlite::params![
                state.turn_uid,
                state.state,
                state.reason,
                state.at_ms,
                state.extraction_fingerprint
            ],
        )?;
    }
    // H7c2：审查轴水位是**整表替换**，不是逐行 upsert——中心那边删掉/换掉的 pass 行，
    // 备份跟着删；否则水位只增不减，"差多少条"会被陈旧行带偏。与其它表同事务。
    tx.execute("DELETE FROM review_watermark", [])?;
    for w in &snapshot.review_watermarks {
        tx.execute(
            "INSERT INTO review_watermark(
                pass_id, max_decision_id, pending_review, quarantined, applied, updated_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6)",
            rusqlite::params![
                w.pass_id,
                w.max_decision_id,
                w.pending_review,
                w.quarantined,
                w.applied,
                w.updated_at_ms
            ],
        )?;
    }
    tx.execute(
        "UPDATE ledger_replication_state
         SET last_commit_seq=?1, last_snapshot_sha256=?2, last_verified_at_ms=?3
         WHERE id=1",
        rusqlite::params![
            snapshot.commit_seq as i64,
            snapshot.snapshot_sha256,
            now_ms
        ],
    )?;
    tx.commit()?;
    Ok(LedgerApplyOutcome {
        applied: true,
        devices: snapshot.devices.len(),
        capabilities: snapshot.capabilities.len(),
        audits: snapshot.audits.len(),
        watermarks: snapshot.watermarks.len(),
        extract_states: snapshot.extract_states.len(),
        review_watermarks: snapshot.review_watermarks.len(),
        commit_seq: snapshot.commit_seq,
    })
}

#[derive(Debug)]
pub enum MemoryApplyError {
    Store(StoreError),
    BadSnapshot(&'static str),
}

impl From<StoreError> for MemoryApplyError {
    fn from(error: StoreError) -> Self {
        MemoryApplyError::Store(error)
    }
}

impl From<rusqlite::Error> for MemoryApplyError {
    fn from(error: rusqlite::Error) -> Self {
        MemoryApplyError::Store(StoreError::from(error))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryApplyStatus {
    pub commit_seq: u64,
    pub snapshot_sha256: String,
    pub last_verified_at_ms: Option<i64>,
    /// 这台机器是否**真的应用过**至少一份记忆快照。`ensure_memory_apply_state` 会给
    /// 每个库建一行 `(0, '', NULL)`；只看 `commit_seq` 分不出"中心本地库"与"还没拉过
    /// 快照的备份"。`sha` 非空或验证时刻在场 ⇒ 是备份侧。
    pub applied_any: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MemoryApplyOutcome {
    pub applied: bool,
    pub commit_seq: u64,
    pub memory_rows: i64,
}

fn ensure_memory_apply_state(l: &LedgerHandle) -> Result<(), StoreError> {
    l.conn().execute_batch(
        "CREATE TABLE IF NOT EXISTS memory_replication_state(
            id INTEGER PRIMARY KEY CHECK(id=1),
            last_commit_seq INTEGER NOT NULL DEFAULT 0,
            last_snapshot_sha256 TEXT NOT NULL DEFAULT '',
            last_verified_at_ms INTEGER);
         INSERT OR IGNORE INTO memory_replication_state(id, last_commit_seq, last_snapshot_sha256)
            VALUES(1, 0, '');",
    )?;
    Ok(())
}

pub fn memory_apply_status(l: &LedgerHandle) -> Result<MemoryApplyStatus, StoreError> {
    ensure_memory_apply_state(l)?;
    let (commit_seq, snapshot_sha256, last_verified_at_ms): (i64, String, Option<i64>) =
        l.conn().query_row(
            "SELECT last_commit_seq, last_snapshot_sha256, last_verified_at_ms
             FROM memory_replication_state WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
    Ok(MemoryApplyStatus {
        commit_seq: commit_seq as u64,
        applied_any: !snapshot_sha256.is_empty() || last_verified_at_ms.is_some(),
        snapshot_sha256,
        last_verified_at_ms,
    })
}

/// 把一份中心 memory 快照整体落到备份：先验文件 sha 与 memory schema，再整替记忆库、
/// 重建索引、记录 `commit_seq`。同一 seq 同 sha 幂等跳过；同 seq 不同 sha 允许覆盖
/// （记忆内容可能在同一个 transcript commit 上变化）；比已应用水位旧则拒绝。
pub fn apply_memory(
    root: &mut Root,
    snapshot_db: &Path,
    expect_sha256: &str,
    expect_commit_seq: u64,
    now_ms: i64,
) -> Result<MemoryApplyOutcome, MemoryApplyError> {
    let sha = rc_store::snapshot::file_sha256(snapshot_db)?;
    if sha != expect_sha256 {
        return Err(MemoryApplyError::BadSnapshot("sha256"));
    }
    // 只读打开一次：坏 schema/坏文件的失败必须发生在整替之前。
    let probe = rc_store::open_replica(snapshot_db)?;
    drop(probe);

    let status = memory_apply_status(root.ledger())?;
    if expect_commit_seq < status.commit_seq {
        return Err(MemoryApplyError::BadSnapshot("commit_seq"));
    }
    if expect_commit_seq == status.commit_seq && sha == status.snapshot_sha256 {
        return Ok(MemoryApplyOutcome {
            applied: false,
            commit_seq: status.commit_seq,
            memory_rows: crate::backup::memory_rows(root.memory().conn())?,
        });
    }

    crate::restore::overwrite_memory(root.memory(), snapshot_db)?;
    rc_index::rebuild(root.memory(), now_ms)
        .map_err(|_| StoreError::Sql("the memory index could not be rebuilt".to_string()))?;
    // W38a 刀 B（工单 4）：转录影子层与记忆索引同刻重建——快照应用的转录内容若来自
    // 刀 A 之前的旧备份/旧导入，影子是空的，检索面只剩 coverage 缺口。
    rc_ledger::rebuild_turn_shadow(root.transcript())
        .map_err(|_| StoreError::Sql("the transcript shadow index could not be rebuilt".to_string()))?;
    let rows = crate::backup::memory_rows(root.memory().conn())?;
    ensure_memory_apply_state(root.ledger())?;
    root.ledger().conn().execute(
        "UPDATE memory_replication_state
         SET last_commit_seq=?1, last_snapshot_sha256=?2, last_verified_at_ms=?3
         WHERE id=1",
        rusqlite::params![expect_commit_seq as i64, sha, now_ms],
    )?;
    Ok(MemoryApplyOutcome {
        applied: true,
        commit_seq: expect_commit_seq,
        memory_rows: rows,
    })
}

#[derive(Debug)]
pub enum PullError {
    Source(String),
    BadResponse(&'static str),
    Store(StoreError),
    Apply(ApplyError),
    Ledger(LedgerApplyError),
    Memory(MemoryApplyError),
}

impl From<StoreError> for PullError {
    fn from(error: StoreError) -> Self {
        PullError::Store(error)
    }
}
impl From<ApplyError> for PullError {
    fn from(error: ApplyError) -> Self {
        PullError::Apply(error)
    }
}
impl From<LedgerApplyError> for PullError {
    fn from(error: LedgerApplyError) -> Self {
        PullError::Ledger(error)
    }
}
impl From<MemoryApplyError> for PullError {
    fn from(error: MemoryApplyError) -> Self {
        PullError::Memory(error)
    }
}

/// 拉取源：`get` 返回 None 表示 404（例如中心还没发布 memory 快照）。
pub trait ReplicateSource {
    fn get(&mut self, target: &str) -> Result<Option<Vec<u8>>, PullError>;
    fn download(&mut self, _manifest: &Manifest, _dest: &Path) -> Result<(), PullError> {
        Err(PullError::Source("memory download is not supported by this source".into()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PullReport {
    pub center_commit_seq: u64,
    pub applied_commits: usize,
    pub skipped_commits: usize,
    pub ledger_applied: bool,
    pub memory_applied: bool,
    /// 这一轮结束后本地已应用到的水位；`--ack` 上报的就是它。
    pub applied_seq: u64,
    pub lag: u64,
}

/// 拉一轮：status → records → ledger → memory（若有新快照）。每段都幂等，
/// 进程被调度器反复调用不会重复落行；lag 是这一轮结束后的中心-本地差。
pub fn pull_once(
    root: &mut Root,
    data_dir: &Path,
    source: &mut dyn ReplicateSource,
    limit: usize,
    now_ms: i64,
) -> Result<PullReport, PullError> {
    let status_bytes = source
        .get("/v1/replicate/status")?
        .ok_or(PullError::BadResponse("status"))?;
    let status: Value = serde_json::from_slice(&status_bytes)
        .map_err(|_| PullError::BadResponse("status"))?;
    let center_commit_seq = status
        .get("last_commit_seq")
        .and_then(Value::as_u64)
        .ok_or(PullError::BadResponse("status"))?;

    let local = apply_status(root.transcript())?;
    let records_target = format!(
        "/v1/replicate?since={}&limit={}",
        local.last_applied_seq,
        limit.max(1)
    );
    let records_bytes = source
        .get(&records_target)?
        .ok_or(PullError::BadResponse("records"))?;
    let records_doc: Value =
        serde_json::from_slice(&records_bytes).map_err(|_| PullError::BadResponse("records"))?;
    let records_value = records_doc
        .get("records")
        .cloned()
        .ok_or(PullError::BadResponse("records"))?;
    let records: Vec<ReplicaRecord> = serde_json::from_value(records_value)
        .map_err(|_| PullError::BadResponse("records"))?;
    let commits = apply_records(root.transcript(), &records, now_ms)?;

    let ledger_bytes = source
        .get("/v1/replicate/ledger")?
        .ok_or(PullError::BadResponse("ledger"))?;
    let ledger: LedgerSnapshot =
        serde_json::from_slice(&ledger_bytes).map_err(|_| PullError::BadResponse("ledger"))?;
    let ledger_applied = apply_ledger(root.ledger_mut(), &ledger, now_ms)?.applied;

    let mut memory_applied = false;
    if let Some(state_bytes) = source.get("/v1/state")? {
        let state: Value =
            serde_json::from_slice(&state_bytes).map_err(|_| PullError::BadResponse("state"))?;
        let center_mem_seq = state.get("center_commit_seq").and_then(Value::as_u64);
        let sha = state.get("sha256").and_then(Value::as_str);
        if let (Some(center_mem_seq), Some(sha)) = (center_mem_seq, sha) {
            let mem_status = memory_apply_status(root.ledger())?;
            if center_mem_seq > mem_status.commit_seq || sha != mem_status.snapshot_sha256 {
                let manifest = Manifest {
                    epoch: state.get("epoch").and_then(Value::as_u64).unwrap_or(0),
                    seq: state.get("seq").and_then(Value::as_u64).unwrap_or(0),
                    sha256: sha.to_string(),
                    schema_version: state
                        .get("schema_version")
                        .and_then(Value::as_i64)
                        .unwrap_or(0) as i32,
                    norm_version: state
                        .get("norm_version")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u32,
                    bytes: state.get("bytes").and_then(Value::as_u64).unwrap_or(0),
                    created_at_ms: state.get("created_at_ms").and_then(Value::as_i64).unwrap_or(0),
                    signature: state
                        .get("signature")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                };
                let dest = data_dir.join(".replicate-memory.tmp");
                let _ = std::fs::remove_file(&dest);
                if let Err(error) = source.download(&manifest, &dest) {
                    let _ = std::fs::remove_file(&dest);
                    return Err(error);
                }
                let outcome = match apply_memory(root, &dest, sha, center_mem_seq, now_ms) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        let _ = std::fs::remove_file(&dest);
                        return Err(PullError::Memory(error));
                    }
                };
                let _ = std::fs::remove_file(&dest);
                memory_applied = outcome.applied;
            }
        }
    }

    let final_status = apply_status(root.transcript())?;
    Ok(PullReport {
        center_commit_seq,
        applied_commits: commits.applied,
        skipped_commits: commits.skipped,
        ledger_applied,
        memory_applied,
        applied_seq: final_status.last_applied_seq,
        lag: center_commit_seq.saturating_sub(final_status.last_applied_seq),
    })
}

pub fn status(t: &TranscriptHandle) -> Result<ReplicaStatus, StoreError> {
    let (epoch, last_commit_seq): (i64, i64) = t.conn().query_row(
        "SELECT epoch, last_commit_seq FROM center_state WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(ReplicaStatus {
        epoch: epoch as u64,
        last_commit_seq: last_commit_seq as u64,
    })
}

/// 备份上报“我落地到哪一格”。同一设备重复上报**只前进不后退**（乱序请求不让水位倒退），
/// epoch 变了则整行换新——提升前的回执对新中心不作数。
///
/// 回执不得超前中心自己的 `last_commit_seq`：那说明备份读到的记录比中心日志还新，
/// 两边的读数已经不自洽，记下来只会让设备提前删本地副本。
pub fn record_ack(
    t: &TranscriptHandle,
    device_id: &str,
    applied_seq: u64,
    now_ms: i64,
) -> Result<(), AckError> {
    let status = status(t)?;
    if applied_seq > status.last_commit_seq {
        return Err(AckError::BeyondCenter {
            applied_seq,
            last_commit_seq: status.last_commit_seq,
        });
    }
    t.conn().execute(
        "INSERT INTO replica_ack(device_id, applied_seq, epoch, updated_at_ms)
         VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(device_id) DO UPDATE SET
            applied_seq = CASE WHEN replica_ack.epoch = excluded.epoch
                               THEN MAX(replica_ack.applied_seq, excluded.applied_seq)
                               ELSE excluded.applied_seq END,
            updated_at_ms = CASE WHEN replica_ack.epoch = excluded.epoch
                                 THEN MAX(replica_ack.updated_at_ms, excluded.updated_at_ms)
                                 ELSE excluded.updated_at_ms END,
            epoch = excluded.epoch",
        rusqlite::params![
            device_id,
            applied_seq as i64,
            status.epoch as i64,
            now_ms
        ],
    )?;
    Ok(())
}

/// 当前 epoch 的回执 + 一个合并水位。`replicated_seq` 只数**仍在编且未过期**的设备：
/// 撤销之后不再让它的旧回执替别的设备作保；回执过期同理（备份静默死掉）。
pub fn ack_status(
    t: &TranscriptHandle,
    l: &LedgerHandle,
    now_ms: i64,
) -> Result<AckStatus, StoreError> {
    let status = status(t)?;
    let rows: Vec<(String, i64, i64)> = {
        let mut st = t.conn().prepare(
            "SELECT device_id, applied_seq, updated_at_ms
             FROM replica_ack WHERE epoch = ?1 ORDER BY device_id",
        )?;
        let mapped = st.query_map(rusqlite::params![status.epoch as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        mapped.collect::<Result<Vec<_>, _>>()?
    };
    let fresh_floor = now_ms.saturating_sub(ACK_FRESH_MS);
    let mut acks = Vec::with_capacity(rows.len());
    let mut replicated_seq = 0u64;
    for (device_id, applied_seq, updated_at_ms) in rows {
        let active = rc_ledger::device::find_by_id(l, &device_id)?
            .map(|row| row.revoked_at_ms.is_none())
            .unwrap_or(false);
        let fresh = updated_at_ms >= fresh_floor;
        let applied_seq = applied_seq as u64;
        if active && fresh {
            replicated_seq = replicated_seq.max(applied_seq);
        }
        acks.push(ReplicaAckView {
            device_id,
            applied_seq,
            updated_at_ms,
            active,
            fresh,
        });
    }
    Ok(AckStatus {
        epoch: status.epoch,
        last_commit_seq: status.last_commit_seq,
        replicated_seq,
        acks,
    })
}

/// 按 `commit_seq > since` 顺序取最多 `limit` 条。`limit` 由 HTTP 层夹到安全上界。
pub fn records_since(
    t: &TranscriptHandle,
    since: u64,
    limit: usize,
) -> Result<Vec<ReplicaRecord>, StoreError> {
    let conn = t.conn();
    let logs: Vec<(i64, i64, String, String, String, String, i64)> = {
        let mut st = conn.prepare(
            "SELECT commit_seq, epoch, op, session_key, device_id, content_sha256, created_at_ms
             FROM ingest_log WHERE commit_seq > ?1 ORDER BY commit_seq LIMIT ?2",
        )?;
        let rows = st.query_map(rusqlite::params![since as i64, limit as i64], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };

    let mut out = Vec::with_capacity(logs.len());
    for (commit_seq, epoch, op, session_key, device_id, content_sha256, created_at_ms) in logs {
        let session = {
            let mut st = conn.prepare(
                "SELECT source_agent, machine, project_hint, last_saved_at_ms,
                        owner_principal_id, owner_agent_id, scope_kind, scope_key,
                        visibility, retention, ownership_src
                 FROM sessions WHERE session_key=?1 AND device_id=?2",
            )?;
            let mut rows = st.query_map(rusqlite::params![session_key, device_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    r.get::<_, Option<String>>(10)?,
                ))
            })?;
            match rows.next() {
                Some(Ok(row)) => row,
                Some(Err(error)) => return Err(error.into()),
                None => {
                    return Err(StoreError::Io(
                        "an ingest log row has no matching session".to_string(),
                    ))
                }
            }
        };

        let turns: Vec<ReplicaTurn> = {
            let mut st = conn.prepare(
                "SELECT turn_uid, role, body, body_fp, turn_key_tier, created_at_ms, ingested_at_ms
                 FROM turns WHERE commit_seq=?1 ORDER BY ingested_at_ms, turn_uid",
            )?;
            let rows = st.query_map(rusqlite::params![commit_seq], |r| {
                Ok(ReplicaTurn {
                    turn_uid: r.get(0)?,
                    role: r.get(1)?,
                    body: r.get(2)?,
                    body_fp: r.get(3)?,
                    turn_key_tier: r.get(4)?,
                    created_at_ms: r.get(5)?,
                    ingested_at_ms: r.get(6)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        let commit_turns: Vec<CommitTurn<'_>> = turns
            .iter()
            .map(|turn| CommitTurn {
                turn_uid: turn.turn_uid.as_str(),
                role: turn.role.as_str(),
                body: turn.body.as_str(),
                body_fp: turn.body_fp.as_str(),
                created_at_ms: turn.created_at_ms,
                ingested_at_ms: turn.ingested_at_ms,
            })
            .collect();
        if commit_sha256(&commit_turns) != content_sha256 {
            return Err(StoreError::Io(
                "an ingest log row does not match its turns".to_string(),
            ));
        }

        out.push(ReplicaRecord {
            commit_seq: commit_seq as u64,
            epoch: epoch as u64,
            op,
            session_key,
            device_id,
            source_agent: session.0,
            machine: session.1,
            project_hint: session.2,
            last_saved_at_ms: session.3,
            owner_principal_id: session.4,
            owner_agent_id: session.5,
            // import 记录的 `scope_*` 就是会话归属的 scope（purge 记录另有其值）。
            scope_kind: session.6,
            scope_key: session.7,
            visibility: session.8,
            retention: session.9,
            ownership_src: session.10,
            turns,
            content_sha256,
            created_at_ms,
            turns_removed: 0,
        });
    }

    // purge 的墓碑与 import 共用 center_state 的 commit_seq；导出时按 seq 合并。
    let tombstones: Vec<(i64, i64, String, String, i64, String, i64)> = {
        let mut st = conn.prepare(
            "SELECT commit_seq, epoch, scope_kind, scope_key, turns_removed,
                    content_sha256, created_at_ms
             FROM tombstone WHERE commit_seq > ?1 ORDER BY commit_seq LIMIT ?2",
        )?;
        let rows = st.query_map(rusqlite::params![since as i64, limit as i64], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for (commit_seq, epoch, scope_kind, scope_key, turns_removed, content_sha256, created_at_ms)
        in tombstones
    {
        let (session_key, device_id) = if scope_kind == "session" {
            (scope_key.clone(), String::new())
        } else {
            (String::new(), scope_key.clone())
        };
        out.push(ReplicaRecord {
            commit_seq: commit_seq as u64,
            epoch: epoch as u64,
            op: "purge".into(),
            session_key,
            device_id,
            source_agent: String::new(),
            machine: None,
            project_hint: None,
            last_saved_at_ms: None,
            owner_principal_id: None,
            owner_agent_id: None,
            visibility: None,
            retention: None,
            ownership_src: None,
            turns: Vec::new(),
            content_sha256,
            created_at_ms,
            scope_kind: Some(scope_kind),
            scope_key: Some(scope_key),
            turns_removed: turns_removed as u64,
        });
    }
    out.sort_by_key(|record| record.commit_seq);
    out.truncate(limit);
    Ok(out)
}
