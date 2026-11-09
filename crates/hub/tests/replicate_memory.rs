//! memory 快照带 commit_seq 的备份落地：验 sha/schema、整替、重建索引、幂等。

use hub::replicate::{apply_memory, memory_apply_status, MemoryApplyError};
use rc_contract::{compute_body_fp, compute_turn_uid, NormalizedTurn, SessionId, TurnTier};
use rc_ledger::transcript::insert_batch_committed;
use rc_memory::writers_x::put_card;
use rc_memory::{AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_net::key::HubKey;
use rc_net::snapshot::SnapshotState;
use rc_store::Root;
use std::process::Command;

fn one_commit(root: &mut Root) {
    let session = SessionId {
        session_key: "s-1".into(),
        device_id: "dev-1".into(),
        source_agent: "qoder".into(),
        machine: None,
        project_hint: None,
        last_saved_at_ms: None,
        owner_principal_id: None,
        principal_name: None,
        principal_source: None,
    };
    let body = "alpha";
    let turn = NormalizedTurn {
        tier: TurnTier::ContentOnly,
        turn_uid: compute_turn_uid("dev-1", "s-1", "human", body, 100),
        body: body.into(),
        body_fp: compute_body_fp(body),
        role: "human".into(),
        created_at_ms: 100,
    };
    insert_batch_committed(root.transcript_mut(), &session, &[turn], 100, "import").unwrap();
}

fn center_with_card(dir: &std::path::Path) -> (std::path::PathBuf, String, u64) {
    std::fs::create_dir_all(dir).unwrap();
    let mut root = Root::open(dir).unwrap();
    one_commit(&mut root);
    put_card(
        root.memory(),
        &FactContent {
            kind: "note".into(),
            title: "快照卡".into(),
            body: "快照正文".into(),
            scope: Scope {
                kind: ScopeKind::Project,
                key: "proj".into(),
            },
            valid_to_ms: None,
        },
        &[Provenance {
            turn_uid: "u-1".into(),
            anchor_role: AnchorRole::Human,
        }],
        100,
    )
    .unwrap();
    let key = HubKey::load_or_create(&dir.join("hub.key")).unwrap();
    // H7b：standby 从**备份通道**取记忆（完整、含审查两表）；agent 通道是给普通设备的下发面。
    let report = hub::snapshot::publish_channel(
        &mut root,
        &key,
        dir,
        1_000,
        hub::snapshot::SnapshotChannel::Backup,
    )
    .unwrap();
    let snaps = hub::snapshot::channel_dir(dir, hub::snapshot::SnapshotChannel::Backup);
    let state: SnapshotState =
        serde_json::from_slice(&std::fs::read(snaps.join("CURRENT")).unwrap()).unwrap();
    let file = snaps.join(rc_net::snapshot::snapshot_name(
        report.manifest.epoch,
        report.manifest.seq,
    ));
    (file, report.manifest.sha256, state.center_commit_seq)
}

fn card_count(root: &Root) -> i64 {
    root.memory()
        .conn()
        .query_row("SELECT count(*) FROM card", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn memory_snapshot_applies_with_sha_and_commit_seq() {
    let dir = tempfile::tempdir().unwrap();
    let (snapshot, sha, commit_seq) = center_with_card(&dir.path().join("center"));
    let copied = dir.path().join("memory-snapshot.db");
    std::fs::copy(&snapshot, &copied).unwrap();
    let mut backup = Root::open(&dir.path().join("backup")).unwrap();
    let out = apply_memory(&mut backup, &copied, &sha, commit_seq, 500).unwrap();
    assert!(out.applied);
    assert_eq!(out.commit_seq, commit_seq);
    assert!(out.memory_rows >= 2);
    assert_eq!(card_count(&backup), 1);
    let status = memory_apply_status(backup.ledger()).unwrap();
    assert_eq!(status.commit_seq, commit_seq);
    assert_eq!(status.snapshot_sha256, sha);

    let replay = apply_memory(&mut backup, &copied, &sha, commit_seq, 600).unwrap();
    assert!(!replay.applied);
    assert_eq!(card_count(&backup), 1);
}

#[test]
fn a_wrong_sha_or_stale_commit_is_rejected_before_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let (snapshot, sha, commit_seq) = center_with_card(&dir.path().join("center"));
    let copied = dir.path().join("memory-snapshot.db");
    std::fs::copy(&snapshot, &copied).unwrap();
    let mut backup = Root::open(&dir.path().join("backup")).unwrap();
    let wrong = "0".repeat(64);
    let error = apply_memory(&mut backup, &copied, &wrong, commit_seq, 500).unwrap_err();
    assert!(matches!(error, MemoryApplyError::BadSnapshot("sha256")), "{error:?}");
    assert_eq!(card_count(&backup), 0, "坏 sha 不许动备份记忆库");

    apply_memory(&mut backup, &copied, &sha, commit_seq, 600).unwrap();
    let error = apply_memory(&mut backup, &copied, &sha, commit_seq.saturating_sub(1), 700)
        .unwrap_err();
    assert!(matches!(error, MemoryApplyError::BadSnapshot("commit_seq")), "{error:?}");
    assert_eq!(card_count(&backup), 1, "过期快照不许回退备份");
}

#[test]
fn memory_apply_cli_wires_through() {
    let dir = tempfile::tempdir().unwrap();
    let (snapshot, sha, commit_seq) = center_with_card(&dir.path().join("center"));
    let copied = dir.path().join("memory-snapshot.db");
    std::fs::copy(&snapshot, &copied).unwrap();
    let binary = env!("CARGO_BIN_EXE_recollect-hub");
    let backup = dir.path().join("backup");
    let out = Command::new(binary)
        .args([
            "replicate",
            "apply-memory",
            "--data-dir",
            backup.to_str().unwrap(),
            "--snapshot",
            copied.to_str().unwrap(),
            "--expect-sha256",
            &sha,
            "--expect-commit-seq",
            &commit_seq.to_string(),
            "--now-ms",
            "500",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["applied"], true);

    let out = Command::new(binary)
        .args([
            "replicate",
            "memory-status",
            "--data-dir",
            backup.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["commit_seq"], commit_seq);
}
