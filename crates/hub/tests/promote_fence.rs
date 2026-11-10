//! 提升证书、epoch 递增、旧中心 fencing 与新中心继续接写的演练。

use hub::hub_cfg::HubConfig;
use hub::promote::{verify_cert, write_fence, PromoteError, PromotionCert};
use hub::serve::HubState;
use rc_contract::{compute_body_fp, compute_turn_uid, NormalizedTurn, SessionId, TurnTier};
use rc_ledger::transcript::insert_batch_committed;
use rc_net::http::{Handler, Request, Response};
use rc_net::key::{DeviceKey, HubKey};
use rc_store::Root;
use serde_json::{json, Value};
use std::process::Command;

const NOW: i64 = 1_000;

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
    insert_batch_committed(root.transcript_mut(), &s1, &[t1], 100, "import").unwrap();
    let s2 = session("s-2");
    let t2 = turn(
        &compute_turn_uid("dev-1", "s-2", "human", "beta", 200),
        "beta",
        200,
    );
    insert_batch_committed(root.transcript_mut(), &s2, &[t2], 200, "import").unwrap();
    hub::replicate::records_since(root.transcript(), 0, 10).unwrap()
}

fn cfg() -> HubConfig {
    HubConfig {
        bind: "127.0.0.1:7420".into(),
        endpoints: vec!["http://127.0.0.1:7420".into()],
        max_snapshot_bps: 4096,
    }
}

fn issue_code(data: &std::path::Path, code: &str) {
    let mut root = Root::open(data).unwrap();
    let hash = rc_net::pair::hash_pair_code(code).unwrap();
    rc_ledger::device::issue_pair_code(root.ledger_mut(), &hash, NOW, NOW + 300_000).unwrap();
}

fn pair_one(state: &mut HubState, data: &std::path::Path) -> (DeviceKey, String) {
    let dev = DeviceKey::generate().unwrap();
    let (code, _) = rc_net::pair::new_pair_code().unwrap();
    issue_code(data, &code);
    let body = serde_json::to_vec(&json!({
        "code": code,
        "device_id": dev.device_id(),
        "pubkey": dev.public_b64(),
    }))
    .unwrap();
    let resp = state.handle(&Request::new("POST", "/v1/pair", Vec::new(), body));
    assert_eq!(resp.status, 200, "pair failed: {}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    (dev, doc["token"].as_str().unwrap().to_string())
}

fn challenge(state: &mut HubState, dev: &DeviceKey, token: &str) -> (String, u64) {
    let resp = state.handle(&Request::new(
        "POST",
        "/v1/challenge",
        vec![
            ("Authorization".into(), format!("Bearer {token}")),
            ("X-Recollect-Device".into(), dev.device_id()),
        ],
        Vec::new(),
    ));
    assert_eq!(resp.status, 200);
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    (
        doc["nonce"].as_str().unwrap().to_string(),
        doc["epoch"].as_u64().unwrap(),
    )
}

fn signed_ingest_epoch(
    state: &mut HubState,
    dev: &DeviceKey,
    token: &str,
    body: &[u8],
    epoch: u64,
) -> Response {
    let (nonce, _) = challenge(state, dev, token);
    let canonical = rc_net::protocol::request_canonical_v2(
        "POST",
        "/v1/ingest",
        &rc_net::protocol::sha256_hex(body),
        &dev.device_id(),
        &nonce,
        epoch,
    );
    let headers = vec![
        ("Authorization".into(), format!("Bearer {token}")),
        ("X-Recollect-Device".into(), dev.device_id()),
        ("X-Recollect-Nonce".into(), nonce),
        ("X-Recollect-Epoch".into(), epoch.to_string()),
        ("X-Recollect-Sig".into(), dev.sign_b64(canonical.as_bytes())),
    ];
    state.handle(&Request::new("POST", "/v1/ingest", headers, body.to_vec()))
}

fn signed_ingest(state: &mut HubState, dev: &DeviceKey, token: &str, body: &[u8]) -> Response {
    let (_, epoch) = challenge(state, dev, token);
    signed_ingest_epoch(state, dev, token, body, epoch)
}

fn signed_ingest_v1(state: &mut HubState, dev: &DeviceKey, token: &str, body: &[u8]) -> Response {
    let (nonce, _) = challenge(state, dev, token);
    let canonical = rc_net::protocol::request_canonical(
        "POST",
        "/v1/ingest",
        &rc_net::protocol::sha256_hex(body),
        &dev.device_id(),
        &nonce,
    );
    let headers = vec![
        ("Authorization".into(), format!("Bearer {token}")),
        ("X-Recollect-Device".into(), dev.device_id()),
        ("X-Recollect-Nonce".into(), nonce),
        ("X-Recollect-Sig".into(), dev.sign_b64(canonical.as_bytes())),
    ];
    state.handle(&Request::new("POST", "/v1/ingest", headers, body.to_vec()))
}

fn line(session: &str, device: &str, body: &str) -> String {
    format!(
        r#"{{"format_version":1,"sessions":[{{"session_key":"{session}","device_id":"{device}","source_agent":"qoder","turns":[{{"role":"human","body":"{body}","created_at_ms":1}}]}}]}}"#
    )
}

#[test]
fn promotion_is_lag_checked_signed_and_moves_the_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let center_dir = dir.path().join("center");
    let records = center_with_two_commits(&center_dir);
    let backup_dir = dir.path().join("backup");
    let backup = Root::open(&backup_dir).unwrap();
    hub::replicate::apply_records(backup.transcript(), &records, 500).unwrap();
    let old_key = HubKey::load_or_create(&center_dir.join("hub.key")).unwrap();
    let new_key = HubKey::load_or_create(&backup_dir.join("hub.key")).unwrap();

    let report = hub::promote::promote(
        &backup,
        &backup_dir,
        hub::promote::PromoteRequest {
            prev_key: &old_key,
            new_key: &new_key,
            center_last_seq: 2,
            center_epoch: 1,
            now_ms: 1_000,
            force: false,
        },
    )
    .unwrap();
    assert_eq!(report.new_epoch, 2);
    assert_eq!(report.last_commit_seq, 2);
    assert_eq!(report.cert_file, "promotion-2.json");
    assert!(report.signed_by_prev_center);
    assert_eq!(
        rc_ledger::transcript::center_epoch(backup.transcript()).unwrap(),
        2
    );

    let cert_path = backup_dir.join("promotions").join("promotion-2.json");
    let cert: PromotionCert =
        serde_json::from_slice(&std::fs::read(&cert_path).unwrap()).unwrap();
    assert!(verify_cert(&cert, &old_key.public_b64()));
    let mut tampered = cert.clone();
    tampered.new_epoch += 1;
    assert!(!verify_cert(&tampered, &old_key.public_b64()));

    let second = hub::promote::promote(
        &backup,
        &backup_dir,
        hub::promote::PromoteRequest {
            prev_key: &old_key,
            new_key: &new_key,
            center_last_seq: 2,
            center_epoch: 1,
            now_ms: 1_100,
            force: false,
        },
    );
    assert!(matches!(second, Err(PromoteError::AlreadyPromoted)), "{second:?}");
}

#[test]
fn lagged_promotion_is_refused_without_force() {
    let dir = tempfile::tempdir().unwrap();
    let center_dir = dir.path().join("center");
    let records = center_with_two_commits(&center_dir);
    let backup_dir = dir.path().join("backup");
    let backup = Root::open(&backup_dir).unwrap();
    hub::replicate::apply_records(backup.transcript(), &records[..1], 500).unwrap();
    let old_key = HubKey::load_or_create(&center_dir.join("hub.key")).unwrap();
    let new_key = HubKey::load_or_create(&backup_dir.join("hub.key")).unwrap();
    let error = hub::promote::promote(
        &backup,
        &backup_dir,
        hub::promote::PromoteRequest {
            prev_key: &old_key,
            new_key: &new_key,
            center_last_seq: 2,
            center_epoch: 1,
            now_ms: 1_000,
            force: false,
        },
    )
    .unwrap_err();
    assert!(matches!(
        error,
        PromoteError::Lag {
            applied_seq: 1,
            expected_seq: 2,
            ..
        }
    ), "{error:?}");
    assert_eq!(
        rc_ledger::transcript::center_epoch(backup.transcript()).unwrap(),
        1,
        "拒绝的提升不许改 epoch"
    );
}

#[test]
fn fenced_old_center_rejects_writes_and_promoted_center_continues() {
    let dir = tempfile::tempdir().unwrap();
    let center_dir = dir.path().join("center");
    let records = center_with_two_commits(&center_dir);
    let backup_dir = dir.path().join("backup");
    let backup = Root::open(&backup_dir).unwrap();
    hub::replicate::apply_records(backup.transcript(), &records, 500).unwrap();
    let old_key = HubKey::load_or_create(&center_dir.join("hub.key")).unwrap();
    let new_key = HubKey::load_or_create(&backup_dir.join("hub.key")).unwrap();
    hub::promote::promote(
        &backup,
        &backup_dir,
        hub::promote::PromoteRequest {
            prev_key: &old_key,
            new_key: &new_key,
            center_last_seq: 2,
            center_epoch: 1,
            now_ms: 1_000,
            force: false,
        },
    )
    .unwrap();

    let old_root = Root::open(&center_dir).unwrap();
    let mut old_state = HubState::new(old_root, old_key, cfg(), &center_dir, Box::new(|| NOW));
    let (old_dev, old_token) = pair_one(&mut old_state, &center_dir);
    write_fence(&center_dir, 2, 1_100).unwrap();
    let body = format!("{}\n", line("s-old", &old_dev.device_id(), "after fence"));
    let resp = signed_ingest(&mut old_state, &old_dev, &old_token, body.as_bytes());
    assert_eq!(resp.status, 409);
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["error"], "center_fenced");

    let mut new_state = HubState::new(backup, new_key, cfg(), &backup_dir, Box::new(|| 2_000));
    let (new_dev, new_token) = pair_one(&mut new_state, &backup_dir);
    let body = format!("{}\n", line("s-new", &new_dev.device_id(), "after promotion"));
    let resp = signed_ingest(&mut new_state, &new_dev, &new_token, body.as_bytes());
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["commit_seq"], 3, "新中心必须从已应用的 2 继续");
    assert_eq!(doc["epoch"], 2);
}

#[test]
fn device_epoch_rejects_a_stale_center_without_the_fence_file() {
    let dir = tempfile::tempdir().unwrap();
    let center_dir = dir.path().join("center");
    let records = center_with_two_commits(&center_dir);
    let backup_dir = dir.path().join("backup");
    let backup = Root::open(&backup_dir).unwrap();
    hub::replicate::apply_records(backup.transcript(), &records, 500).unwrap();
    let old_key = HubKey::load_or_create(&center_dir.join("hub.key")).unwrap();
    let new_key = HubKey::load_or_create(&backup_dir.join("hub.key")).unwrap();
    hub::promote::promote(
        &backup,
        &backup_dir,
        hub::promote::PromoteRequest {
            prev_key: &old_key,
            new_key: &new_key,
            center_last_seq: 2,
            center_epoch: 1,
            now_ms: 1_000,
            force: false,
        },
    )
    .unwrap();

    // 旧中心仍在 epoch 1，且**没有** FENCED；设备已拿到 epoch 2。
    let old_root = Root::open(&center_dir).unwrap();
    let mut old_state = HubState::new(old_root, old_key, cfg(), &center_dir, Box::new(|| NOW));
    let (old_dev, old_token) = pair_one(&mut old_state, &center_dir);
    let body = format!("{}\n", line("stale", &old_dev.device_id(), "new epoch"));
    let resp = signed_ingest_epoch(&mut old_state, &old_dev, &old_token, body.as_bytes(), 2);
    assert_eq!(resp.status, 409, "旧中心接受了新 epoch 的写");
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["error"], "stale_epoch");

    // 新中心 epoch 2：带 epoch 2 可写；带 epoch 1 是 stale；完全不带 epoch 是 epoch_required。
    let mut new_state = HubState::new(backup, new_key, cfg(), &backup_dir, Box::new(|| 2_000));
    let (new_dev, new_token) = pair_one(&mut new_state, &backup_dir);
    let body = format!("{}\n", line("fresh", &new_dev.device_id(), "epoch two"));
    let resp = signed_ingest(&mut new_state, &new_dev, &new_token, body.as_bytes());
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));

    let body = format!("{}\n", line("stale", &new_dev.device_id(), "old epoch"));
    let resp = signed_ingest_epoch(&mut new_state, &new_dev, &new_token, body.as_bytes(), 1);
    assert_eq!(resp.status, 409);
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["error"], "stale_epoch");

    let resp = signed_ingest_v1(&mut new_state, &new_dev, &new_token, body.as_bytes());
    assert_eq!(resp.status, 409);
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["error"], "epoch_required");
}

/// epoch 1 的 v1 兼容窗口由 `REQUIRE_EPOCH` 标记关闭：带 epoch 的客户端不受影响，
/// 标记撤掉后旧客户端又能进来——升级窗口的开关不该需要重新编译。
#[test]
fn epoch_marker_closes_the_v1_window_at_epoch_one() {
    let dir = tempfile::tempdir().unwrap();
    let center_dir = dir.path().join("center");
    center_with_two_commits(&center_dir);
    let key = HubKey::load_or_create(&center_dir.join("hub.key")).unwrap();
    let root = Root::open(&center_dir).unwrap();
    let mut state = HubState::new(root, key, cfg(), &center_dir, Box::new(|| NOW));
    let (dev, token) = pair_one(&mut state, &center_dir);
    let marker = center_dir.join(hub::serve::REQUIRE_EPOCH_FILE);

    let body = format!("{}\n", line("s-v1", &dev.device_id(), "one"));
    assert_eq!(
        signed_ingest_v1(&mut state, &dev, &token, body.as_bytes()).status,
        200,
        "没有标记时 v1 客户端仍可写（升级窗口）"
    );

    std::fs::write(&marker, b"{\"required\":true}\n").unwrap();
    let body = format!("{}\n", line("s-v1", &dev.device_id(), "two"));
    let resp = signed_ingest_v1(&mut state, &dev, &token, body.as_bytes());
    assert_eq!(resp.status, 409);
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["error"], "epoch_required");

    // 带 epoch 头的客户端照常工作。
    let body = format!("{}\n", line("s-v2", &dev.device_id(), "three"));
    assert_eq!(
        signed_ingest(&mut state, &dev, &token, body.as_bytes()).status,
        200
    );

    std::fs::remove_file(&marker).unwrap();
    let body = format!("{}\n", line("s-v1", &dev.device_id(), "four"));
    assert_eq!(
        signed_ingest_v1(&mut state, &dev, &token, body.as_bytes()).status,
        200,
        "撤掉标记后旧客户端恢复可用（回滚路径）"
    );
}


#[test]
fn fence_is_idempotent_and_does_not_downgrade() {
    let dir = tempfile::tempdir().unwrap();
    let first = write_fence(dir.path(), 4, 1_000).unwrap();
    assert_eq!(first.fenced_epoch, 4);
    let again = write_fence(dir.path(), 4, 1_100).unwrap();
    assert_eq!(again.fenced_at_ms, 1_000, "同 epoch 重写必须幂等");
    let lower = write_fence(dir.path(), 3, 1_200).unwrap();
    assert_eq!(lower.fenced_epoch, 4, "fence 不许降级");
    let record = hub::promote::read_fence(dir.path()).unwrap().unwrap();
    assert_eq!(record.fenced_epoch, 4);
}

#[test]
fn promote_and_fence_cli_wire_up_without_echoing_paths_or_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let center_dir = dir.path().join("center");
    let records = center_with_two_commits(&center_dir);
    let _ = HubKey::load_or_create(&center_dir.join("hub.key")).unwrap();
    let backup_dir = dir.path().join("backup");
    let backup = Root::open(&backup_dir).unwrap();
    hub::replicate::apply_records(backup.transcript(), &records, 500).unwrap();
    drop(backup);
    let binary = env!("CARGO_BIN_EXE_recollect-hub");
    let out = Command::new(binary)
        .args([
            "promote",
            "--data-dir",
            backup_dir.to_str().unwrap(),
            "--prev-key",
            center_dir.join("hub.key").to_str().unwrap(),
            "--center-last-seq",
            "2",
            "--center-epoch",
            "1",
            "--now-ms",
            "1000",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!text.contains(&backup_dir.display().to_string()), "输出泄露路径：{text}");
    for canary in ["alpha", "beta"] {
        assert!(!text.contains(canary), "输出泄露正文 {canary}: {text}");
    }
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["promoted"], true);
    assert_eq!(doc["new_epoch"], 2);

    let out = Command::new(binary)
        .args([
            "fence",
            "--data-dir",
            center_dir.to_str().unwrap(),
            "--epoch",
            "2",
            "--now-ms",
            "1100",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["fenced"], false);
    assert!(hub::promote::read_fence(&center_dir).unwrap().is_none());

    let out = Command::new(binary)
        .args([
            "fence",
            "--data-dir",
            center_dir.to_str().unwrap(),
            "--epoch",
            "2",
            "--now-ms",
            "1100",
            "--confirm",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["fenced"], true);
    assert_eq!(
        hub::promote::read_fence(&center_dir)
            .unwrap()
            .unwrap()
            .fenced_epoch,
        2
    );
}
