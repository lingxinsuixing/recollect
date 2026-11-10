//! 中心增量复制导出：按 commit_seq 取记录、status/lag 读数与认证边界。

use hub::hub_cfg::HubConfig;
use hub::serve::HubState;
use rc_net::http::{Handler, Request, Response};
use rc_net::key::{DeviceKey, HubKey};
use rc_store::Root;
use serde_json::{json, Value};
use std::process::Command;

const NOW: i64 = 1_000;

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, HubState) {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let root = Root::open(&data).unwrap();
    let key = HubKey::load_or_create(&dir.path().join("hub.key")).unwrap();
    let cfg = HubConfig {
        bind: "127.0.0.1:7420".into(),
        endpoints: vec!["http://127.0.0.1:7420".into()],
        max_snapshot_bps: 4096,
    };
    let state = HubState::new(root, key, cfg, &data, Box::new(|| NOW));
    (dir, data, state)
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

fn challenge(state: &mut HubState, dev: &DeviceKey, token: &str) -> String {
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
    doc["nonce"].as_str().unwrap().to_string()
}

fn signed_get(state: &mut HubState, dev: &DeviceKey, token: &str, target: &str) -> Response {
    let nonce = challenge(state, dev, token);
    let canonical = rc_net::protocol::request_canonical(
        "GET",
        target,
        &rc_net::protocol::sha256_hex(b""),
        &dev.device_id(),
        &nonce,
    );
    let headers = vec![
        ("Authorization".into(), format!("Bearer {token}")),
        ("X-Recollect-Device".into(), dev.device_id()),
        ("X-Recollect-Nonce".into(), nonce),
        ("X-Recollect-Sig".into(), dev.sign_b64(canonical.as_bytes())),
    ];
    state.handle(&Request::new("GET", target, headers, Vec::new()))
}

fn signed_ingest(state: &mut HubState, dev: &DeviceKey, token: &str, target: &str, body: &[u8]) -> Response {
    let nonce = challenge(state, dev, token);
    let canonical = rc_net::protocol::request_canonical(
        "POST",
        target,
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
    state.handle(&Request::new("POST", target, headers, body.to_vec()))
}

fn line(session: &str, device: &str, body: &str) -> String {
    format!(
        r#"{{"format_version":1,"sessions":[{{"session_key":"{session}","device_id":"{device}","source_agent":"qoder","turns":[{{"role":"human","body":"{body}","created_at_ms":1}}]}}]}}"#
    )
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

fn import(state: &mut HubState, dev: &DeviceKey, token: &str, session: &str, body: &str) -> Value {
    let payload = format!("{}\n", line(session, &dev.device_id(), body));
    let resp = signed_ingest(state, dev, token, "/v1/ingest", payload.as_bytes());
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    serde_json::from_slice(&resp.body).unwrap()
}

#[test]
fn replicate_returns_records_in_commit_order_and_status() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    grant_replicate(&data, &dev.device_id());
    import(&mut state, &dev, &token, "s-1", "first");
    import(&mut state, &dev, &token, "s-2", "second");

    let resp = signed_get(&mut state, &dev, &token, "/v1/replicate?since=0&limit=10");
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["epoch"], 1);
    assert_eq!(doc["last_commit_seq"], 2);
    assert_eq!(doc["next_since"], 2);
    let records = doc["records"].as_array().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["commit_seq"], 1);
    assert_eq!(records[0]["session_key"], "s-1");
    assert_eq!(records[0]["turns"][0]["body"], "first");
    assert_eq!(records[1]["commit_seq"], 2);
    assert_eq!(records[1]["turns"][0]["body"], "second");

    let resp = signed_get(&mut state, &dev, &token, "/v1/replicate?since=1&limit=10");
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["records"].as_array().unwrap().len(), 1);
    assert_eq!(doc["records"][0]["commit_seq"], 2);

    let resp = signed_get(&mut state, &dev, &token, "/v1/replicate?since=2&limit=10");
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert!(doc["records"].as_array().unwrap().is_empty());
    assert_eq!(doc["next_since"], 2);

    let resp = signed_get(&mut state, &dev, &token, "/v1/replicate/status");
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["epoch"], 1);
    assert_eq!(doc["last_commit_seq"], 2);
}

#[test]
fn replicate_requires_authentication_and_rejects_bad_query() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    grant_replicate(&data, &dev.device_id());

    let resp = state.handle(&Request::new("GET", "/v1/replicate?since=0", Vec::new(), Vec::new()));
    assert_eq!(resp.status, 401);
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["error"], "invalid_token");

    let resp = signed_get(&mut state, &dev, &token, "/v1/replicate?since=oops");
    assert_eq!(resp.status, 400);
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["error"], "bad_request");

    let resp = signed_get(&mut state, &dev, &token, "/v1/replicate?limit=0");
    assert_eq!(resp.status, 400);
}

#[test]
fn replicate_grant_and_revoke_cli_control_the_endpoint() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let binary = env!("CARGO_BIN_EXE_recollect-hub");
    let out = Command::new(binary)
        .args([
            "replicate",
            "grant",
            "--data-dir",
            data.to_str().unwrap(),
            "--device-id",
            &dev.device_id(),
            "--now-ms",
            "1000",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let resp = signed_get(&mut state, &dev, &token, "/v1/replicate?since=0");
    assert_eq!(resp.status, 200);

    let out = Command::new(binary)
        .args([
            "replicate",
            "revoke",
            "--data-dir",
            data.to_str().unwrap(),
            "--device-id",
            &dev.device_id(),
            "--now-ms",
            "1001",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let resp = signed_get(&mut state, &dev, &token, "/v1/replicate?since=0");
    assert_eq!(resp.status, 403);
}

#[test]
fn replicate_requires_an_explicit_capability_and_revoke_takes_effect() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);

    let resp = signed_get(&mut state, &dev, &token, "/v1/replicate?since=0");
    assert_eq!(resp.status, 403);
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["error"], "replicate_denied");
    let resp = signed_get(&mut state, &dev, &token, "/v1/replicate/ledger");
    assert_eq!(resp.status, 403, "ledger 快照也必须过能力闸");

    grant_replicate(&data, &dev.device_id());
    let resp = signed_get(&mut state, &dev, &token, "/v1/replicate?since=0");
    assert_eq!(resp.status, 200);
    let resp = signed_get(&mut state, &dev, &token, "/v1/replicate/ledger");
    assert_eq!(resp.status, 200);
    let ledger: Value = serde_json::from_slice(&resp.body).unwrap();
    assert!(ledger["devices"].as_array().is_some());

    let mut root = Root::open(&data).unwrap();
    assert!(rc_ledger::device::revoke_capability(
        root.ledger_mut(),
        &dev.device_id(),
        rc_ledger::device::CAPABILITY_REPLICATE,
        NOW + 1,
    )
    .unwrap());
    let resp = signed_get(&mut state, &dev, &token, "/v1/replicate?since=0");
    assert_eq!(resp.status, 403);
}

#[test]
fn replicate_limit_caps_the_page_and_next_since_advances() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    grant_replicate(&data, &dev.device_id());
    for i in 0..3 {
        import(&mut state, &dev, &token, &format!("s-{i}"), &format!("body-{i}"));
    }
    let resp = signed_get(&mut state, &dev, &token, "/v1/replicate?since=0&limit=1");
    assert_eq!(resp.status, 200);
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["records"].as_array().unwrap().len(), 1);
    assert_eq!(doc["next_since"], 1);
    let resp = signed_get(&mut state, &dev, &token, "/v1/replicate?since=1&limit=1");
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["records"].as_array().unwrap().len(), 1);
    assert_eq!(doc["records"][0]["commit_seq"], 2);
    assert_eq!(doc["next_since"], 2);
}
