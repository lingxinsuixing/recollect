//! 备份侧增量 apply：顺序、幂等、同 seq 冲突、跳号、CLI 接线；S1b-C1 起还钉
//! 会话归属 7 列——旧记录（无这 7 键）能 apply 且不算清空已归属行。

use hub::replicate::{apply_records, apply_status, records_since, ApplyError, ReplicaRecord};
use rc_contract::{compute_body_fp, compute_turn_uid, NormalizedTurn, SessionId, TurnTier};
use rc_ledger::transcript::{commit_sha256, insert_batch_committed, CommitTurn};
use rc_store::Root;
use serde_json::json;
use std::process::Command;

fn session(key: &str) -> SessionId {
    SessionId {
        session_key: key.into(),
        device_id: "dev-1".into(),
        source_agent: "qoder".into(),
        machine: Some("windows".into()),
        project_hint: Some("proj".into()),
        last_saved_at_ms: None,
        owner_principal_id: None,
        principal_name: None,
        principal_source: None,
    }
}

fn turn(uid: &str, body: &str, created_at_ms: i64) -> NormalizedTurn {
    NormalizedTurn {
        tier: TurnTier::ContentOnly,
        turn_uid: uid.into(),
        body: body.into(),
        body_fp: compute_body_fp(body),
        role: "human".into(),
        created_at_ms,
    }
}

fn center_with_two_commits(dir: &std::path::Path) -> Vec<hub::replicate::ReplicaRecord> {
    let mut root = Root::open(dir).unwrap();
    let s1 = session("s-1");
    let t1 = turn(
        &compute_turn_uid("dev-1", "s-1", "human", "alpha", 100),
        "alpha",
        100,
    );
    let first = insert_batch_committed(root.transcript_mut(), &s1, &[t1], 100, "import").unwrap();
    assert_eq!(first.commit_seq, Some(1));
    let s2 = session("s-2");
    let t2 = turn(
        &compute_turn_uid("dev-1", "s-2", "human", "beta", 200),
        "beta",
        200,
    );
    let second = insert_batch_committed(root.transcript_mut(), &s2, &[t2], 200, "import").unwrap();
    assert_eq!(second.commit_seq, Some(2));
    records_since(root.transcript(), 0, 10).unwrap()
}

#[test]
fn apply_is_sequential_idempotent_and_updates_status() {
    let dir = tempfile::tempdir().unwrap();
    let records = center_with_two_commits(&dir.path().join("center"));
    let backup_dir = dir.path().join("backup");
    let backup = Root::open(&backup_dir).unwrap();
    let out = apply_records(backup.transcript(), &records, 500).unwrap();
    assert_eq!((out.applied, out.skipped, out.last_applied_seq, out.last_epoch), (2, 0, 2, 1));
    assert_eq!(rc_ledger::count_turns_on(backup.transcript()), 2);
    assert_eq!(rc_ledger::count_sessions(backup.transcript()), 2);
    let status = apply_status(backup.transcript()).unwrap();
    assert_eq!(status.last_applied_seq, 2);
    assert_eq!(status.last_epoch, 1);
    assert_eq!(status.last_verified_at_ms, Some(500));

    let replay = apply_records(backup.transcript(), &records, 600).unwrap();
    assert_eq!((replay.applied, replay.skipped, replay.last_applied_seq), (0, 2, 2));
    assert_eq!(rc_ledger::count_turns_on(backup.transcript()), 2, "重放造出了第二行");
}

#[test]
fn apply_rejects_a_gap_and_a_same_seq_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let records = center_with_two_commits(&dir.path().join("center"));

    let gap_dir = dir.path().join("gap");
    let gap = Root::open(&gap_dir).unwrap();
    let error = apply_records(gap.transcript(), &records[1..], 500).unwrap_err();
    assert!(matches!(error, ApplyError::Gap { expected: 1, got: 2 }), "{error:?}");

    let conflict_dir = dir.path().join("conflict");
    let conflict = Root::open(&conflict_dir).unwrap();
    apply_records(conflict.transcript(), &records[..1], 500).unwrap();
    let mut tampered = records[0].clone();
    tampered.turns[0].body = "tampered".into();
    tampered.turns[0].body_fp = compute_body_fp("tampered");
    let commit_turn = CommitTurn {
        turn_uid: tampered.turns[0].turn_uid.as_str(),
        role: tampered.turns[0].role.as_str(),
        body: tampered.turns[0].body.as_str(),
        body_fp: tampered.turns[0].body_fp.as_str(),
        created_at_ms: tampered.turns[0].created_at_ms,
        ingested_at_ms: tampered.turns[0].ingested_at_ms,
    };
    tampered.content_sha256 = commit_sha256(&[commit_turn]);
    let error = apply_records(conflict.transcript(), &[tampered], 500).unwrap_err();
    assert!(matches!(error, ApplyError::Conflict(1)), "{error:?}");
}

#[test]
fn apply_rejects_a_record_whose_hash_does_not_match_its_turns() {
    let dir = tempfile::tempdir().unwrap();
    let records = center_with_two_commits(&dir.path().join("center"));
    let backup = Root::open(&dir.path().join("backup")).unwrap();
    let mut bad = records[0].clone();
    bad.turns[0].body = "changed without hashing".into();
    let error = apply_records(backup.transcript(), &[bad], 500).unwrap_err();
    assert!(matches!(error, ApplyError::BadRecord("content_sha256")), "{error:?}");
}

#[test]
fn replicate_cli_applies_status_and_lag_without_leaking_body() {
    let dir = tempfile::tempdir().unwrap();
    let records = center_with_two_commits(&dir.path().join("center"));
    let input = dir.path().join("records.json");
    let payload = json!({"records": records});
    std::fs::write(&input, serde_json::to_vec(&payload).unwrap()).unwrap();
    let backup = dir.path().join("backup");
    let binary = env!("CARGO_BIN_EXE_recollect-hub");
    let out = Command::new(binary)
        .args([
            "replicate",
            "apply",
            "--input",
            input.to_str().unwrap(),
            "--data-dir",
            backup.to_str().unwrap(),
            "--now-ms",
            "500",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    for canary in ["alpha", "beta"] {
        assert!(!text.contains(canary), "apply 输出泄露正文 {canary}: {text}");
    }
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["last_applied_seq"], 2);

    let out = Command::new(binary)
        .args(["replicate", "status", "--data-dir", backup.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["last_applied_seq"], 2);

    let out = Command::new(binary)
        .args([
            "replicate",
            "lag",
            "--data-dir",
            backup.to_str().unwrap(),
            "--center-last-seq",
            "3",
            "--center-epoch",
            "1",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["lag"], 1);
    assert_eq!(doc["epoch_match"], true);
}

// ── S1b-C1：会话归属 7 列的复制兼容 ────────────────────────────────────────
// 判据来自设计 §8-C1 第 4 条：旧记录能 apply、已归属会话再 apply 旧记录后归属不变、
// 新记录带归属时逐列一致。

const OWNERSHIP_KEYS: [&str; 7] = [
    "owner_principal_id",
    "owner_agent_id",
    "scope_kind",
    "scope_key",
    "visibility",
    "retention",
    "ownership_src",
];

type Ownership = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn owned(
    principal: Option<&str>,
    agent: &str,
    kind: &str,
    key: &str,
    visibility: &str,
    retention: &str,
    src: &str,
) -> Ownership {
    (
        principal.map(str::to_string),
        Some(agent.to_string()),
        Some(kind.to_string()),
        Some(key.to_string()),
        Some(visibility.to_string()),
        Some(retention.to_string()),
        Some(src.to_string()),
    )
}

fn session_ownership(root: &Root, session: &str, device: &str) -> Ownership {
    root.transcript()
        .conn()
        .query_row(
            "SELECT owner_principal_id, owner_agent_id, scope_kind, scope_key,
                    visibility, retention, ownership_src
             FROM sessions WHERE session_key=?1 AND device_id=?2",
            rusqlite::params![session, device],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .unwrap()
}

/// 把一条记录退回 v5 之前的 JSON 形状：7 个归属键整键消失。
/// 这正是旧中心写进 `records.json` / 旧备份快照里的样子（结构体无 `deny_unknown_fields`，
/// 反向也安全）；走 `serde_json::Value` 是为了让"缺键"真的发生在 JSON 层而不是靠字段赋值。
fn legacy_record(record: &ReplicaRecord) -> ReplicaRecord {
    let mut value = serde_json::to_value(record).unwrap();
    let obj = value.as_object_mut().unwrap();
    for key in OWNERSHIP_KEYS {
        assert!(obj.remove(key).is_some(), "记录形状里没有 {key}，夹具前提失效");
    }
    serde_json::from_value(value).unwrap()
}

/// 同一会话的两条提交：用来在"已归属"之后仍能再 apply 一条记录。
fn center_with_two_commits_in_one_session(dir: &std::path::Path) -> Vec<ReplicaRecord> {
    let mut root = Root::open(dir).unwrap();
    let s = session("s-1");
    let alpha = turn(&compute_turn_uid("dev-1", "s-1", "human", "alpha", 100), "alpha", 100);
    assert_eq!(
        insert_batch_committed(root.transcript_mut(), &s, &[alpha], 100, "import")
            .unwrap()
            .commit_seq,
        Some(1)
    );
    let beta = turn(&compute_turn_uid("dev-1", "s-1", "human", "beta", 200), "beta", 200);
    assert_eq!(
        insert_batch_committed(root.transcript_mut(), &s, &[beta], 200, "import")
            .unwrap()
            .commit_seq,
        Some(2)
    );
    records_since(root.transcript(), 0, 10).unwrap()
}

#[test]
fn a_legacy_record_without_ownership_still_applies() {
    let dir = tempfile::tempdir().unwrap();
    let records = center_with_two_commits(&dir.path().join("center"));
    let legacy: Vec<ReplicaRecord> = records.iter().map(legacy_record).collect();
    for record in &legacy {
        assert!(
            record.owner_principal_id.is_none()
                && record.owner_agent_id.is_none()
                && record.scope_kind.is_none()
                && record.scope_key.is_none()
                && record.visibility.is_none()
                && record.retention.is_none()
                && record.ownership_src.is_none(),
            "旧 JSON 里缺 7 个键 ⇒ 必须反序列化成 None"
        );
    }

    let backup = Root::open(&dir.path().join("backup")).unwrap();
    let out = apply_records(backup.transcript(), &legacy, 500).unwrap();
    assert_eq!((out.applied, out.skipped), (2, 0), "旧记录必须能 apply");
    assert_eq!(rc_ledger::count_sessions(backup.transcript()), 2);
    let null_owned: i64 = backup
        .transcript()
        .conn()
        .query_row(
            "SELECT count(*) FROM sessions
             WHERE owner_principal_id IS NULL AND owner_agent_id IS NULL
               AND scope_kind IS NULL AND scope_key IS NULL AND visibility IS NULL
               AND retention IS NULL AND ownership_src IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        null_owned, 2,
        "旧记录的插入不许给归属列编默认值：NULL 才是读侧的\"未知 ⇒ 最保守\""
    );
}

#[test]
fn a_legacy_record_does_not_clear_an_owned_session() {
    let dir = tempfile::tempdir().unwrap();
    let records = center_with_two_commits_in_one_session(&dir.path().join("center"));
    let backup = Root::open(&dir.path().join("backup")).unwrap();
    apply_records(backup.transcript(), &records[..1], 500).unwrap();

    // 把落地行改成"已归属"的非默认值（运维 CLI / 将来的 v3 适配器会这么写）。
    let explicit = owned(Some("p-1"), "agent-owner", "person", "person:k", "person", "keep", "operator");
    backup
        .transcript()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id='p-1', owner_agent_id='agent-owner',
                    scope_kind='person', scope_key='person:k', visibility='person',
                    retention='keep', ownership_src='operator'
             WHERE session_key='s-1'",
            [],
        )
        .unwrap();

    // 第二条记录退回旧的 JSON 形状（7 键缺失 ⇒ 全 None），再 apply。
    let legacy = legacy_record(&records[1]);
    let out = apply_records(backup.transcript(), &[legacy], 600).unwrap();
    assert_eq!((out.applied, out.skipped), (1, 0));
    assert_eq!(
        session_ownership(&backup, "s-1", "dev-1"),
        explicit,
        "旧记录把已归属会话的归属列清空了（冲突分支必须 COALESCE(旧, 新)）"
    );
    assert_eq!(rc_ledger::count_turns_on(backup.transcript()), 2, "第二条提交的两轮都该落地");
}

#[test]
fn a_record_with_ownership_lands_column_by_column() {
    let dir = tempfile::tempdir().unwrap();
    let records = center_with_two_commits(&dir.path().join("center"));
    // 导出侧（records_since）已经把中心写下的保守默认带上。
    assert_eq!(
        (
            records[1].owner_principal_id.clone(),
            records[1].owner_agent_id.clone(),
            records[1].scope_kind.clone(),
            records[1].scope_key.clone(),
            records[1].visibility.clone(),
            records[1].retention.clone(),
            records[1].ownership_src.clone(),
        ),
        owned(None, "qoder", "project", "proj", "private_agent", "default", "default"),
        "records_since 没有把 sessions 的 7 个归属列带进记录"
    );

    let backup = Root::open(&dir.path().join("backup")).unwrap();
    apply_records(backup.transcript(), &records, 500).unwrap();
    assert_eq!(
        session_ownership(&backup, "s-2", "dev-1"),
        owned(None, "qoder", "project", "proj", "private_agent", "default", "default"),
        "apply 没有逐列写归属"
    );
}
