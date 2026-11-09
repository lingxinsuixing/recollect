//! 备份回执：`POST /v1/replicate/ack` 与 `GET /v1/replicate/acked`。
//!
//! 这两条回答的是同一个问题——“至少有一份备份落到了哪一格”。设备据此清理本地原始副本，
//! 所以边界比普通读面更硬：只有复制能力能写回执、回执不许超前中心、撤销的备份不再作保。

use hub::hub_cfg::HubConfig;
use hub::serve::HubState;
use rc_net::http::{Handler, Request, Response};
use rc_net::key::{DeviceKey, HubKey};
use rc_store::Root;
use serde_json::{json, Value};

const NOW: i64 = 1_000;

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, HubState) {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let root = Root::open(&data).unwrap();
    let key = HubKey::load_or_create(&dir.path().join("hub.key")).unwrap();
    let cfg = HubConfig {
        bind: "127.0.0.1:7421".into(),
        endpoints: vec!["http://127.0.0.1:7421".into()],
        max_snapshot_bps: 4096,
    };
    let state = HubState::new(root, key, cfg, &data, Box::new(|| NOW));
    (dir, data, state)
}

fn pair_one(state: &mut HubState, data: &std::path::Path) -> (DeviceKey, String) {
    let dev = DeviceKey::generate().unwrap();
    let (code, _) = rc_net::pair::new_pair_code().unwrap();
    {
        let mut root = Root::open(data).unwrap();
        let hash = rc_net::pair::hash_pair_code(&code).unwrap();
        rc_ledger::device::issue_pair_code(root.ledger_mut(), &hash, NOW, NOW + 300_000).unwrap();
    }
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
    assert_eq!(resp.status, 200, "challenge failed: {}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    (
        doc["nonce"].as_str().unwrap().to_string(),
        doc["epoch"].as_u64().expect("challenge 必须回 epoch"),
    )
}

fn signed(
    state: &mut HubState,
    dev: &DeviceKey,
    token: &str,
    method: &str,
    target: &str,
    body: &[u8],
) -> Response {
    let (nonce, epoch) = challenge(state, dev, token);
    let canonical = rc_net::protocol::request_canonical_v2(
        method,
        target,
        &rc_net::protocol::sha256_hex(body),
        &dev.device_id(),
        &nonce,
        epoch,
    );
    let headers = vec![
        ("Authorization".into(), format!("Bearer {token}")),
        ("X-Recollect-Device".into(), dev.device_id()),
        ("X-Recollect-Nonce".into(), nonce),
        ("X-Recollect-Sig".into(), dev.sign_b64(canonical.as_bytes())),
        ("X-Recollect-Epoch".into(), epoch.to_string()),
    ];
    state.handle(&Request::new(method, target, headers, body.to_vec()))
}

fn ack(state: &mut HubState, dev: &DeviceKey, token: &str, applied_seq: Value) -> Response {
    let body = serde_json::to_vec(&json!({ "applied_seq": applied_seq })).unwrap();
    signed(state, dev, token, "POST", "/v1/replicate/ack", &body)
}

fn acked(state: &mut HubState, dev: &DeviceKey, token: &str) -> Value {
    let resp = signed(state, dev, token, "GET", "/v1/replicate/acked", b"");
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    serde_json::from_slice(&resp.body).unwrap()
}

fn line(session: &str, device: &str, body: &str) -> String {
    format!(
        r#"{{"format_version":1,"sessions":[{{"session_key":"{session}","device_id":"{device}","source_agent":"qoder","turns":[{{"role":"human","body":"{body}","created_at_ms":1}}]}}]}}"#
    )
}

fn import(state: &mut HubState, dev: &DeviceKey, token: &str, session: &str, body: &str) -> Value {
    let payload = format!("{}\n", line(session, &dev.device_id(), body));
    let resp = signed(state, dev, token, "POST", "/v1/ingest", payload.as_bytes());
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    serde_json::from_slice(&resp.body).unwrap()
}

fn grant_replicate(data: &std::path::Path, device_id: &str) {
    let mut root = Root::open(data).unwrap();
    rc_ledger::device::grant_capability(
        root.ledger_mut(),
        device_id,
        rc_ledger::device::CAPABILITY_REPLICATE,
        NOW,
    )
    .unwrap();
}

#[test]
fn ack_needs_the_capability_and_never_runs_ahead_of_the_center() {
    let (_dir, data, mut state) = fixture();
    let (writer, writer_token) = pair_one(&mut state, &data);
    let (backup, backup_token) = pair_one(&mut state, &data);
    grant_replicate(&data, &backup.device_id());

    import(&mut state, &writer, &writer_token, "s1", "alpha");
    import(&mut state, &writer, &writer_token, "s2", "beta");

    // 没授权的那台不能写回执：否则任何设备都能替备份作保。
    let denied = ack(&mut state, &writer, &writer_token, json!(2));
    assert_eq!(denied.status, 403);
    assert_eq!(
        serde_json::from_slice::<Value>(&denied.body).unwrap()["error"],
        "replicate_denied"
    );

    let ok = ack(&mut state, &backup, &backup_token, json!(2));
    assert_eq!(ok.status, 200, "{}", String::from_utf8_lossy(&ok.body));
    let doc: Value = serde_json::from_slice(&ok.body).unwrap();
    assert_eq!(doc["applied_seq"], 2);
    assert_eq!(doc["replicated_seq"], 2);
    assert_eq!(doc["epoch"], 1);

    // 普通设备（无复制能力）也能读到合并水位：它才是清理本地副本的消费者。
    let seen = acked(&mut state, &writer, &writer_token);
    assert_eq!(seen["epoch"], 1);
    assert_eq!(seen["last_commit_seq"], 2);
    assert_eq!(seen["replicated_seq"], 2);

    // 超前中心一格不是“落后”，是两边读数不自洽。
    let ahead = ack(&mut state, &backup, &backup_token, json!(3));
    assert_eq!(ahead.status, 409);
    assert_eq!(
        serde_json::from_slice::<Value>(&ahead.body).unwrap()["error"],
        "ack_beyond_center"
    );

    // 缺字段/类型不对是坏请求，不是 0。
    let missing = signed(&mut state, &backup, &backup_token, "POST", "/v1/replicate/ack", b"{}");
    assert_eq!(missing.status, 400);
}

#[test]
fn ack_only_moves_forward_and_a_revoked_backup_stops_covering_devices() {
    let (_dir, data, mut state) = fixture();
    let (writer, writer_token) = pair_one(&mut state, &data);
    let (backup, backup_token) = pair_one(&mut state, &data);
    grant_replicate(&data, &backup.device_id());
    import(&mut state, &writer, &writer_token, "s1", "alpha");
    import(&mut state, &writer, &writer_token, "s2", "beta");

    assert_eq!(ack(&mut state, &backup, &backup_token, json!(2)).status, 200);
    // 乱序到达的旧回执不许把水位拉回去。
    assert_eq!(ack(&mut state, &backup, &backup_token, json!(1)).status, 200);
    assert_eq!(acked(&mut state, &writer, &writer_token)["replicated_seq"], 2);

    {
        let mut root = Root::open(&data).unwrap();
        rc_ledger::device::revoke(root.ledger_mut(), &backup.device_id(), NOW + 1).unwrap();
    }
    // 备份被撤销（机器丢失/退役）后，它的旧回执不再替设备作保。
    let after = acked(&mut state, &writer, &writer_token);
    assert_eq!(after["replicated_seq"], 0, "撤销后的备份不能再覆盖设备");
}

#[test]
fn a_stale_ack_stops_covering_devices() {
    let (_dir, data, mut state) = fixture();
    let (writer, writer_token) = pair_one(&mut state, &data);
    let (backup, backup_token) = pair_one(&mut state, &data);
    grant_replicate(&data, &backup.device_id());
    import(&mut state, &writer, &writer_token, "s1", "alpha");
    assert_eq!(ack(&mut state, &backup, &backup_token, json!(1)).status, 200);
    assert_eq!(acked(&mut state, &writer, &writer_token)["replicated_seq"], 1);

    // 备份静默死掉：还在编，但超过窗口没有回执。旧回执不许继续替设备作保。
    {
        let root = Root::open(&data).unwrap();
        root.transcript()
            .conn()
            .execute(
                "UPDATE replica_ack SET updated_at_ms = ?1 WHERE device_id = ?2",
                rusqlite::params![
                    NOW - hub::replicate::ACK_FRESH_MS - 1,
                    backup.device_id()
                ],
            )
            .unwrap();
    }
    assert_eq!(acked(&mut state, &writer, &writer_token)["replicated_seq"], 0);

    // 操作面能看到“还在编、但回执过期”，而不是只剩一个 0。
    let resp = signed(&mut state, &backup, &backup_token, "GET", "/v1/replicate/status", b"");
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["replicated_seq"], 0);
    assert_eq!(doc["acks"][0]["active"], true);
    assert_eq!(doc["acks"][0]["fresh"], false);
}

#[test]
fn acks_from_an_earlier_epoch_do_not_count() {
    let (_dir, data, mut state) = fixture();
    let (writer, writer_token) = pair_one(&mut state, &data);
    let (backup, backup_token) = pair_one(&mut state, &data);
    grant_replicate(&data, &backup.device_id());
    import(&mut state, &writer, &writer_token, "s1", "alpha");
    assert_eq!(ack(&mut state, &backup, &backup_token, json!(1)).status, 200);
    assert_eq!(acked(&mut state, &writer, &writer_token)["replicated_seq"], 1);

    // 提升把 epoch 推到 2；旧回执在提升前落地，不能替新中心作保。
    {
        let root = Root::open(&data).unwrap();
        root.transcript()
            .conn()
            .execute("UPDATE center_state SET epoch=2 WHERE id=1", [])
            .unwrap();
    }
    let seen = acked(&mut state, &writer, &writer_token);
    assert_eq!(seen["epoch"], 2);
    assert_eq!(seen["replicated_seq"], 0);
}
