//! 自动 pull 的一轮：status → records → ledger → memory；假源测试顺序/幂等/lag。

use hub::replicate::{pull_once, records_since, PullError, ReplicateSource};
use rc_contract::{compute_body_fp, compute_turn_uid, NormalizedTurn, SessionId, TurnTier};
use rc_ledger::transcript::insert_batch_committed;
use rc_memory::writers_x::put_card;
use rc_memory::{AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_net::key::{DeviceKey, HubKey};
use rc_net::snapshot::{Manifest, SnapshotState};
use rc_store::Root;
use serde_json::json;
use std::collections::HashMap;

struct FakeSource {
    responses: HashMap<String, Vec<u8>>,
    snapshot: std::path::PathBuf,
}

impl ReplicateSource for FakeSource {
    fn get(&mut self, target: &str) -> Result<Option<Vec<u8>>, PullError> {
        Ok(self.responses.get(target).cloned())
    }

    fn download(&mut self, _manifest: &Manifest, dest: &std::path::Path) -> Result<(), PullError> {
        std::fs::copy(&self.snapshot, dest)
            .map(|_| ())
            .map_err(|_| PullError::Source("fake snapshot copy failed".into()))
    }
}

fn pair_and_grant(root: &mut Root) {
    let dev = DeviceKey::generate().unwrap();
    let (code, _) = rc_net::pair::new_pair_code().unwrap();
    let hash = rc_net::pair::hash_pair_code(&code).unwrap();
    rc_ledger::device::issue_pair_code(root.ledger_mut(), &hash, 100, 10_000).unwrap();
    let (_token, token_hash) = rc_net::pair::new_token().unwrap();
    rc_ledger::device::pair(
        root.ledger_mut(),
        &hash,
        &dev.device_id(),
        &dev.public_b64(),
        &token_hash,
        101,
    )
    .unwrap();
    rc_ledger::device::grant_capability(
        root.ledger_mut(),
        &dev.device_id(),
        rc_ledger::device::CAPABILITY_REPLICATE,
        102,
    )
    .unwrap();
}

fn fixture(dir: &std::path::Path) -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, std::path::PathBuf) {
    let center_dir = dir.join("center");
    let mut center = Root::open(&center_dir).unwrap();
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
    insert_batch_committed(center.transcript_mut(), &session, &[turn], 100, "import").unwrap();
    pair_and_grant(&mut center);
    put_card(
        center.memory(),
        &FactContent {
            kind: "note".into(),
            title: "pull 卡".into(),
            body: "pull 正文".into(),
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
    let key = HubKey::load_or_create(&center_dir.join("hub.key")).unwrap();
    // H7b：standby 从备份通道取记忆；agent 通道的副本会被应用方当作完整记忆。
    let report = hub::snapshot::publish_channel(
        &mut center,
        &key,
        &center_dir,
        1_000,
        hub::snapshot::SnapshotChannel::Backup,
    )
    .unwrap();
    let snaps = hub::snapshot::channel_dir(&center_dir, hub::snapshot::SnapshotChannel::Backup);
    let state: SnapshotState =
        serde_json::from_slice(&std::fs::read(snaps.join("CURRENT")).unwrap()).unwrap();
    let snapshot_file = snaps.join(rc_net::snapshot::snapshot_name(
        report.manifest.epoch,
        report.manifest.seq,
    ));
    let status = json!({"last_commit_seq": 1, "epoch": 1}).to_string().into_bytes();
    let records = serde_json::to_vec(&json!({
        "records": records_since(center.transcript(), 0, 10).unwrap()
    }))
    .unwrap();
    let ledger = serde_json::to_vec(&hub::replicate::export_ledger(&center).unwrap()).unwrap();
    let state_json = serde_json::to_vec(&json!({
        "epoch": state.manifest.epoch,
        "seq": state.manifest.seq,
        "sha256": state.manifest.sha256,
        "bytes": state.manifest.bytes,
        "schema_version": state.manifest.schema_version,
        "norm_version": state.manifest.norm_version,
        "created_at_ms": state.manifest.created_at_ms,
        "signature": state.manifest.signature,
        "center_commit_seq": state.center_commit_seq,
    }))
    .unwrap();
    (status, records, ledger, state_json, snapshot_file)
}

#[test]
fn pull_once_applies_transcript_ledger_and_memory_in_one_round() {
    let dir = tempfile::tempdir().unwrap();
    let (status, records, ledger, state, snapshot) = fixture(dir.path());
    let mut responses = HashMap::new();
    responses.insert("/v1/replicate/status".into(), status);
    responses.insert("/v1/replicate?since=0&limit=10".into(), records);
    responses.insert("/v1/replicate?since=1&limit=10".into(), br#"{"records":[]}"#.to_vec());
    responses.insert("/v1/replicate/ledger".into(), ledger);
    responses.insert("/v1/state".into(), state);
    let mut source = FakeSource {
        responses,
        snapshot,
    };

    let backup_dir = dir.path().join("backup");
    let mut backup = Root::open(&backup_dir).unwrap();
    let out = pull_once(&mut backup, &backup_dir, &mut source, 10, 500).unwrap();
    assert_eq!(out.applied_commits, 1);
    assert!(out.ledger_applied);
    assert!(out.memory_applied);
    assert_eq!(out.lag, 0);
    assert_eq!(
        backup
            .transcript()
            .conn()
            .query_row("SELECT count(*) FROM turns", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        backup
            .memory()
            .conn()
            .query_row("SELECT count(*) FROM card", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let devices: i64 = backup
        .ledger()
        .conn()
        .query_row("SELECT count(*) FROM device", [], |r| r.get(0))
        .unwrap();
    assert_eq!(devices, 1);

    let second = pull_once(&mut backup, &backup_dir, &mut source, 10, 600).unwrap();
    assert_eq!(second.applied_commits, 0);
    assert_eq!(second.skipped_commits, 0);
    assert!(!second.ledger_applied);
    assert!(!second.memory_applied);
    assert_eq!(second.lag, 0);
}
