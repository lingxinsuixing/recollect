use hub::hub_cfg::HubConfig;
use hub::serve::HubState;
use rc_net::http::{Handler, Request};
use rc_net::key::{DeviceKey, HubKey};
use rc_store::Root;
use serde_json::{json, Value};

const NOW: i64 = 1_000;

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, HubState, String) {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let root = Root::open(&data).unwrap();
    let key_path = dir.path().join("hub.key");
    let key = HubKey::load_or_create(&key_path).unwrap();
    let hub_pub = key.public_b64();
    let cfg = HubConfig {
        bind: "127.0.0.1:7420".into(),
        endpoints: vec!["http://127.0.0.1:7420".into()],
        max_snapshot_bps: 4096,
    };
    let state = HubState::new(root, key, cfg, &data, Box::new(|| NOW));
    (dir, data, state, hub_pub)
}

fn issue_code(data: &std::path::Path, code: &str, expires_at_ms: i64) {
    let mut root = Root::open(data).unwrap();
    let hash = rc_net::pair::hash_pair_code(code).unwrap();
    rc_ledger::device::issue_pair_code(root.ledger_mut(), &hash, NOW, expires_at_ms).unwrap();
}

fn pair_body(code: &str, dev: &DeviceKey) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "code": code,
        "device_id": dev.device_id(),
        "pubkey": dev.public_b64(),
    }))
    .unwrap()
}

fn post(state: &mut HubState, path: &str, body: Vec<u8>) -> rc_net::http::Response {
    state.handle(&Request::new(
        "POST",
        path,
        vec![("Content-Type".into(), "application/json".into())],
        body,
    ))
}

fn pair_one(state: &mut HubState, data: &std::path::Path) -> (DeviceKey, String) {
    let dev = DeviceKey::generate().unwrap();
    let (code, _hash) = rc_net::pair::new_pair_code().unwrap();
    issue_code(data, &code, NOW + 300_000);
    let resp = post(state, "/v1/pair", pair_body(&code, &dev));
    assert_eq!(resp.status, 200, "pair failed: {}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    (dev, doc["token"].as_str().unwrap().to_string())
}

fn challenge(state: &mut HubState, dev: &DeviceKey, token: &str) -> Value {
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
    serde_json::from_slice(&resp.body).unwrap()
}

fn signed_headers(
    dev: &DeviceKey,
    token: &str,
    nonce: &str,
    method: &str,
    target: &str,
    body: &[u8],
) -> Vec<(String, String)> {
    let canonical = rc_net::protocol::request_canonical(
        method,
        target,
        &rc_net::protocol::sha256_hex(body),
        &dev.device_id(),
        nonce,
    );
    vec![
        ("Authorization".into(), format!("Bearer {token}")),
        ("X-Recollect-Device".into(), dev.device_id()),
        ("X-Recollect-Nonce".into(), nonce.into()),
        ("X-Recollect-Sig".into(), dev.sign_b64(canonical.as_bytes())),
    ]
}

#[test]
fn pair_returns_token_hub_pub_schema_and_endpoints() {
    let (_dir, data, mut state, hub_pub) = fixture();
    let dev = DeviceKey::generate().unwrap();
    let (code, _) = rc_net::pair::new_pair_code().unwrap();
    issue_code(&data, &code, NOW + 300_000);
    let resp = post(&mut state, "/v1/pair", pair_body(&code, &dev));
    assert_eq!(resp.status, 200);
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["api_version"], 1);
    assert!(!doc["token"].as_str().unwrap().is_empty());
    assert_eq!(doc["hub_pub"], hub_pub);
    assert_eq!(doc["schema"]["min"], 6);
    assert_eq!(doc["schema"]["max"], 6);
    assert_eq!(doc["endpoints"], json!(["http://127.0.0.1:7420"]));
}

#[test]
fn pair_code_is_one_time_and_hashing_never_stores_the_code() {
    let (_dir, data, mut state, _) = fixture();
    let dev = DeviceKey::generate().unwrap();
    let (code, _) = rc_net::pair::new_pair_code().unwrap();
    issue_code(&data, &code, NOW + 300_000);

    let first = post(&mut state, "/v1/pair", pair_body(&code, &dev));
    assert_eq!(first.status, 200);
    let second = post(&mut state, "/v1/pair", pair_body(&code, &dev));
    assert_eq!(second.status, 403);
    assert_eq!(
        serde_json::from_slice::<Value>(&second.body).unwrap()["error"],
        "pair_code_used"
    );

    let root = Root::open(&data).unwrap();
    let raw: i64 = root
        .ledger()
        .conn()
        .query_row("SELECT count(*) FROM pair_code WHERE code_hash=?1", [&code], |r| r.get(0))
        .unwrap();
    assert_eq!(raw, 0, "配对码的原码进了库");
    let (used, stored): (Option<i64>, String) = root
        .ledger()
        .conn()
        .query_row("SELECT used_at_ms, code_hash FROM pair_code", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert!(used.is_some());
    assert_ne!(stored, code);
}

#[test]
fn pair_rejects_a_device_id_not_derived_from_the_pubkey() {
    let (_dir, data, mut state, _) = fixture();
    let dev = DeviceKey::generate().unwrap();
    let (code, _) = rc_net::pair::new_pair_code().unwrap();
    issue_code(&data, &code, NOW + 300_000);
    let body = serde_json::to_vec(&json!({
        "code": code,
        "device_id": "dev-0000000000000000",
        "pubkey": dev.public_b64(),
    }))
    .unwrap();
    let resp = post(&mut state, "/v1/pair", body);
    assert_eq!(resp.status, 400);
    let root = Root::open(&data).unwrap();
    let used: Option<i64> = root
        .ledger()
        .conn()
        .query_row("SELECT used_at_ms FROM pair_code", [], |r| r.get(0))
        .unwrap();
    assert!(used.is_none(), "坏请求把码消费掉了");
}

#[test]
fn challenge_response_verifies_with_hub_pub_and_nonce_is_single_use() {
    let (_dir, data, mut state, hub_pub) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let doc = challenge(&mut state, &dev, &token);
    let nonce = doc["nonce"].as_str().unwrap();
    let expires = doc["expires_at_ms"].as_i64().unwrap();
    let canonical = rc_net::protocol::challenge_canonical(&dev.device_id(), nonce, expires);
    assert!(rc_net::key::verify_b64(
        &hub_pub,
        canonical.as_bytes(),
        doc["sig"].as_str().unwrap()
    ));

    let body = b"payload";
    let req = Request::new(
        "POST",
        "/v1/ingest",
        signed_headers(&dev, &token, nonce, "POST", "/v1/ingest", body),
        body.to_vec(),
    );
    assert!(state.authenticate(&req).is_ok());
    assert_eq!(state.authenticate(&req).unwrap_err().code, "bad_nonce");
}

#[test]
fn revoked_device_is_refused_with_device_revoked() {
    let (_dir, data, mut state, _) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let mut root = Root::open(&data).unwrap();
    rc_ledger::device::revoke(root.ledger_mut(), &dev.device_id(), NOW + 1).unwrap();
    let resp = state.handle(&Request::new(
        "POST",
        "/v1/challenge",
        vec![
            ("Authorization".into(), format!("Bearer {token}")),
            ("X-Recollect-Device".into(), dev.device_id()),
        ],
        Vec::new(),
    ));
    assert_eq!(resp.status, 403);
    assert_eq!(serde_json::from_slice::<Value>(&resp.body).unwrap()["error"], "device_revoked");
}

#[test]
fn device_header_mismatch_is_refused() {
    let (_dir, data, mut state, _) = fixture();
    let (_dev, token) = pair_one(&mut state, &data);
    let other = DeviceKey::generate().unwrap();
    let resp = state.handle(&Request::new(
        "POST",
        "/v1/challenge",
        vec![
            ("Authorization".into(), format!("Bearer {token}")),
            ("X-Recollect-Device".into(), other.device_id()),
        ],
        Vec::new(),
    ));
    assert_eq!(resp.status, 403);
    assert_eq!(serde_json::from_slice::<Value>(&resp.body).unwrap()["error"], "device_mismatch");
}

#[test]
fn bad_signature_does_not_consume_the_nonce() {
    let (_dir, data, mut state, _) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let doc = challenge(&mut state, &dev, &token);
    let nonce = doc["nonce"].as_str().unwrap().to_string();

    let body = b"x";
    let mut headers = signed_headers(&dev, &token, &nonce, "POST", "/v1/ingest", body);
    let sig_at = headers.iter().position(|(k, _)| k.eq_ignore_ascii_case("x-recollect-sig")).unwrap();
    let first = headers[sig_at].1.remove(0);
    headers[sig_at].1.insert(0, if first == 'A' { 'B' } else { 'A' });
    let bad = Request::new("POST", "/v1/ingest", headers, body.to_vec());
    assert_eq!(state.authenticate(&bad).unwrap_err().code, "bad_signature");

    let good = Request::new(
        "POST",
        "/v1/ingest",
        signed_headers(&dev, &token, &nonce, "POST", "/v1/ingest", body),
        body.to_vec(),
    );
    assert!(state.authenticate(&good).is_ok(), "坏签名把 nonce 烧掉了");
}

#[test]
fn replayed_request_is_refused_with_bad_nonce() {
    let (_dir, data, mut state, _) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let doc = challenge(&mut state, &dev, &token);
    let nonce = doc["nonce"].as_str().unwrap();
    let body = b"same";
    let req = Request::new(
        "POST",
        "/v1/ingest",
        signed_headers(&dev, &token, nonce, "POST", "/v1/ingest", body),
        body.to_vec(),
    );
    assert!(state.authenticate(&req).is_ok());
    assert_eq!(state.authenticate(&req).unwrap_err().status, 401);
}

#[test]
fn nonce_capacity_is_a_loud_429() {
    let (_dir, data, mut state, _) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    for _ in 0..1024 {
        let _ = challenge(&mut state, &dev, &token);
    }
    let resp = state.handle(&Request::new(
        "POST",
        "/v1/challenge",
        vec![
            ("Authorization".into(), format!("Bearer {token}")),
            ("X-Recollect-Device".into(), dev.device_id()),
        ],
        Vec::new(),
    ));
    assert_eq!(resp.status, 429);
    assert_eq!(serde_json::from_slice::<Value>(&resp.body).unwrap()["error"], "nonce_capacity");
}

#[test]
fn audit_log_records_pair_code_issued_pair_and_revoke() {
    let (_dir, data, mut state, _) = fixture();
    let (dev, _token) = pair_one(&mut state, &data);
    let mut root = Root::open(&data).unwrap();
    rc_ledger::device::revoke(root.ledger_mut(), &dev.device_id(), NOW + 1).unwrap();

    let actions: Vec<String> = {
        let mut st = root.ledger().conn().prepare("SELECT action FROM audit_log ORDER BY id").unwrap();
        let rows = st.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    };
    for want in ["pair_code_issued", "pair", "revoke"] {
        assert!(actions.iter().any(|a| a == want), "缺审计动作 {want}: {actions:?}");
    }
    let details: Vec<Option<String>> = {
        let mut st = root.ledger().conn().prepare("SELECT detail FROM audit_log").unwrap();
        let rows = st.query_map([], |r| r.get::<_, Option<String>>(0)).unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    };
    assert!(details.iter().all(|d| d.as_deref().unwrap_or("").is_empty()));
}
