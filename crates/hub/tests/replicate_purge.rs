//! purge 墓碑随 commit_seq 复制：中心导出、备份 apply、重放幂等、计数冲突回滚。

use hub::purge::{purge, PurgeScope};
use hub::replicate::{
    apply_records, apply_status, records_since, ApplyError, ReplicaRecord, ReplicaTurn,
};
use rc_contract::{compute_body_fp, compute_turn_uid, NormalizedTurn, SessionId, TurnTier};
use rc_ledger::transcript::{
    commit_sha256, insert_batch_committed, tombstone_sha256, CommitTurn,
};
use rc_store::Root;

fn session(key: &str, device: &str) -> SessionId {
    SessionId {
        session_key: key.into(),
        device_id: device.into(),
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

fn center_with_two_commits(dir: &std::path::Path) -> Root {
    let mut root = Root::open(dir).unwrap();
    let s1 = session("s-1", "dev-1");
    let t1 = turn(
        &compute_turn_uid("dev-1", "s-1", "human", "alpha", 100),
        "alpha",
        100,
    );
    insert_batch_committed(root.transcript_mut(), &s1, &[t1], 100, "import").unwrap();
    let s2 = session("s-2", "dev-1");
    let t2 = turn(
        &compute_turn_uid("dev-1", "s-2", "human", "beta", 200),
        "beta",
        200,
    );
    insert_batch_committed(root.transcript_mut(), &s2, &[t2], 200, "import").unwrap();
    root
}

fn count_turns(root: &Root) -> i64 {
    root.transcript()
        .conn()
        .query_row("SELECT count(*) FROM turns", [], |r| r.get(0))
        .unwrap()
}

fn count_tombstones(root: &Root) -> i64 {
    root.transcript()
        .conn()
        .query_row("SELECT count(*) FROM tombstone", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn purge_is_exported_and_applied_as_a_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let mut center = center_with_two_commits(&dir.path().join("center"));
    let backup_dir = dir.path().join("backup");
    let backup = Root::open(&backup_dir).unwrap();
    let imports = records_since(center.transcript(), 0, 10).unwrap();
    apply_records(backup.transcript(), &imports, 500).unwrap();
    assert_eq!(count_turns(&backup), 2);

    let removed = purge(
        &mut center,
        &PurgeScope::Session("s-1".into()),
        300,
    )
    .unwrap();
    assert_eq!(removed, 1);
    assert_eq!(count_turns(&center), 1);
    assert_eq!(count_tombstones(&center), 1);

    let records = records_since(center.transcript(), 2, 10).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].op, "purge");
    assert_eq!(records[0].scope_kind.as_deref(), Some("session"));
    assert_eq!(records[0].scope_key.as_deref(), Some("s-1"));
    assert_eq!(records[0].turns_removed, 1);
    assert!(records[0].turns.is_empty());

    let out = apply_records(backup.transcript(), &records, 600).unwrap();
    assert_eq!(out.applied, 1);
    assert_eq!(count_turns(&backup), 1, "备份侧 s-1 没删掉");
    assert_eq!(count_tombstones(&backup), 1);
    assert_eq!(apply_status(backup.transcript()).unwrap().last_applied_seq, 3);
    let remaining: String = backup
        .transcript()
        .conn()
        .query_row("SELECT session_key FROM turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining, "s-2");

    // 重放整段：同 seq 同 sha 跳过，不重复删、不重复写墓碑。
    let replay = apply_records(backup.transcript(), &records, 700).unwrap();
    assert_eq!((replay.applied, replay.skipped), (0, 1));
    assert_eq!(count_turns(&backup), 1);
    assert_eq!(count_tombstones(&backup), 1);
}

#[test]
fn a_promoted_backup_can_reexport_later_commits() {
    let dir = tempfile::tempdir().unwrap();
    let mut center = center_with_two_commits(&dir.path().join("center"));
    let records = records_since(center.transcript(), 0, 10).unwrap();
    let backup = Root::open(&dir.path().join("backup")).unwrap();
    apply_records(backup.transcript(), &records, 500).unwrap();
    purge(&mut center, &PurgeScope::Session("s-1".into()), 600).unwrap();
    let purge_records = records_since(center.transcript(), 2, 10).unwrap();
    apply_records(backup.transcript(), &purge_records, 700).unwrap();

    // 从已应用点之后导出：import 2 的 turn 还在，purge 3 也在。
    // 从 0 重放被 purge 掉的 import 需要先走全量快照引导；这里不假装能从零重建。
    let reexported = records_since(backup.transcript(), 1, 10).unwrap();
    assert_eq!(reexported.len(), 2);
    assert_eq!(reexported[0].op, "import");
    assert_eq!(reexported[0].commit_seq, 2);
    assert_eq!(reexported[1].op, "purge");
    assert_eq!(reexported[1].commit_seq, 3);
}

#[test]
fn device_purge_removes_all_device_turns_on_the_backup() {
    let dir = tempfile::tempdir().unwrap();
    let mut center = center_with_two_commits(&dir.path().join("center"));
    let backup = Root::open(&dir.path().join("backup")).unwrap();
    apply_records(
        backup.transcript(),
        &records_since(center.transcript(), 0, 10).unwrap(),
        500,
    )
    .unwrap();
    purge(&mut center, &PurgeScope::Device("dev-1".into()), 600).unwrap();
    let records = records_since(center.transcript(), 2, 10).unwrap();
    assert_eq!(records[0].scope_kind.as_deref(), Some("device"));
    apply_records(backup.transcript(), &records, 700).unwrap();
    assert_eq!(count_turns(&backup), 0);
    assert_eq!(count_tombstones(&backup), 1);
}

#[test]
fn a_zero_turn_purge_does_not_allocate_a_commit_or_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let mut center = center_with_two_commits(&dir.path().join("center"));
    let before: (i64, i64) = center
        .transcript()
        .conn()
        .query_row(
            "SELECT last_commit_seq, epoch FROM center_state WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let removed = purge(
        &mut center,
        &PurgeScope::Session("does-not-exist".into()),
        300,
    )
    .unwrap();
    assert_eq!(removed, 0);
    let after: (i64, i64) = center
        .transcript()
        .conn()
        .query_row(
            "SELECT last_commit_seq, epoch FROM center_state WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(count_tombstones(&center), 0);
}

#[test]
fn a_tombstone_count_mismatch_rolls_back_the_backup_delete() {
    let dir = tempfile::tempdir().unwrap();
    let mut center = center_with_two_commits(&dir.path().join("center"));
    let backup = Root::open(&dir.path().join("backup")).unwrap();
    apply_records(
        backup.transcript(),
        &records_since(center.transcript(), 0, 10).unwrap(),
        500,
    )
    .unwrap();
    purge(&mut center, &PurgeScope::Session("s-1".into()), 600).unwrap();
    let mut records = records_since(center.transcript(), 2, 10).unwrap();
    records[0].turns_removed = 99;
    records[0].content_sha256 = tombstone_sha256("session", "s-1", 99);
    let error = apply_records(backup.transcript(), &records, 700).unwrap_err();
    assert!(matches!(error, ApplyError::BadRecord("tombstone count")), "{error:?}");
    assert_eq!(count_turns(&backup), 2, "冲突的墓碑不许留下半删状态");
    assert_eq!(count_tombstones(&backup), 0);
}

#[test]
fn a_later_import_of_a_purged_scope_is_refused_by_apply() {
    let dir = tempfile::tempdir().unwrap();
    let mut center = center_with_two_commits(&dir.path().join("center"));
    let backup = Root::open(&dir.path().join("backup")).unwrap();
    apply_records(
        backup.transcript(),
        &records_since(center.transcript(), 0, 10).unwrap(),
        500,
    )
    .unwrap();

    purge(&mut center, &PurgeScope::Session("s-1".into()), 300).unwrap();
    let purge_record = records_since(center.transcript(), 2, 10).unwrap();

    // 人为构造一条“purge 之后又导入同 scope”的记录：真实中心会拦在 ingest，
    // 这里直接喂给 apply，验证备份侧也不会复活。
    let turn = ReplicaTurn {
        turn_uid: "revived-turn".into(),
        role: "human".into(),
        body: "revived".into(),
        body_fp: compute_body_fp("revived"),
        turn_key_tier: 1,
        created_at_ms: 400,
        ingested_at_ms: 400,
    };
    let sha = commit_sha256(&[CommitTurn {
        turn_uid: &turn.turn_uid,
        role: &turn.role,
        body: &turn.body,
        body_fp: &turn.body_fp,
        created_at_ms: turn.created_at_ms,
        ingested_at_ms: turn.ingested_at_ms,
    }]);
    let record = ReplicaRecord {
        commit_seq: 4,
        epoch: 1,
        op: "import".into(),
        session_key: "s-1".into(),
        device_id: "dev-1".into(),
        source_agent: "qoder".into(),
        machine: Some("windows".into()),
        project_hint: Some("proj".into()),
        last_saved_at_ms: None,
        // S1b-C1：会话归属 5 格（`scope_*` 由下面既有的 purge 两格承担）。
        owner_principal_id: None,
        owner_agent_id: None,
        visibility: None,
        retention: None,
        ownership_src: None,
        turns: vec![turn],
        content_sha256: sha,
        created_at_ms: 400,
        scope_kind: None,
        scope_key: None,
        turns_removed: 0,
    };
    let mut records = purge_record;
    records.push(record);
    let error = apply_records(backup.transcript(), &records, 600).unwrap_err();
    assert!(
        matches!(error, ApplyError::Tombstoned(4)),
        "purge 之后的导入必须被拒：{error:?}"
    );
    assert_eq!(count_turns(&backup), 1, "被拒的导入不许复活 s-1");
    assert_eq!(
        apply_status(backup.transcript()).unwrap().last_applied_seq,
        3,
        "只推进到 purge 那一条"
    );
}
