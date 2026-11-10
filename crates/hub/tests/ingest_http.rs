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

fn signed_headers(dev: &DeviceKey, token: &str, nonce: &str, signed_body: &[u8]) -> Vec<(String, String)> {
    let canonical = rc_net::protocol::request_canonical(
        "POST",
        "/v1/ingest",
        &rc_net::protocol::sha256_hex(signed_body),
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

/// `wire_body` 是线缆上的字节，`signed_body` 是签名覆盖的字节——gzip 那一格两者不同。
fn ingest_call(
    state: &mut HubState,
    dev: &DeviceKey,
    token: &str,
    wire_body: Vec<u8>,
    signed_body: &[u8],
    encoding: Option<&str>,
) -> Response {
    let doc = challenge(state, dev, token);
    let nonce = doc["nonce"].as_str().unwrap();
    let mut headers = signed_headers(dev, token, nonce, signed_body);
    if let Some(enc) = encoding {
        headers.push(("Content-Encoding".into(), enc.to_string()));
    }
    state.handle(&Request::new("POST", "/v1/ingest", headers, wire_body))
}

fn ingest_plain(state: &mut HubState, dev: &DeviceKey, token: &str, body: &[u8]) -> Response {
    ingest_call(state, dev, token, body.to_vec(), body, None)
}

fn line(session: &str, device: &str, body: &str) -> String {
    format!(
        r#"{{"format_version":1,"sessions":[{{"session_key":"{session}","device_id":"{device}","source_agent":"qoder","turns":[{{"role":"human","body":"{body}","created_at_ms":1}}]}}]}}"#
    )
}

/// 身份三格随会话上行：HTTP 侧必须在 `commit` 之后把它们变成 claim，再落主人锚 mention。
fn line_with_identity(
    session: &str,
    device: &str,
    principal_id: &str,
    principal_name: &str,
    principal_source: &str,
    body: &str,
) -> String {
    format!(
        r#"{{"format_version":1,"sessions":[{{"session_key":"{session}","device_id":"{device}","source_agent":"qoder","owner_principal_id":"{principal_id}","principal_name":"{principal_name}","principal_source":"{principal_source}","turns":[{{"role":"human","body":"{body}","created_at_ms":1}}]}}]}}"#
    )
}

fn mention_count(data: &std::path::Path) -> i64 {
    let root = Root::open(data).unwrap();
    root.memory()
        .conn()
        .query_row("SELECT count(*) FROM mention", [], |r| r.get(0))
        .unwrap()
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(bytes).unwrap();
    enc.finish().unwrap()
}

fn counter(data: &std::path::Path, device_id: &str, metric: &str) -> Option<i64> {
    let root = Root::open(data).unwrap();
    rc_ledger::counters::read_all(root.ledger())
        .unwrap()
        .into_iter()
        .find(|r| r.subject_id == device_id && r.metric == metric)
        .map(|r| r.count)
}

#[test]
fn ingest_accepts_ndjson_and_reports_counts_only() {
    let (dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let body = format!(
        "{}\n{}\n",
        line("s-1", &dev.device_id(), "CANARY-INGEST-11"),
        line("s-2", &dev.device_id(), "CANARY-INGEST-12"),
    );
    let resp = ingest_plain(&mut state, &dev, &token, body.as_bytes());
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["lines_in"], 2);
    assert_eq!(doc["lines_accepted"], 2);
    assert_eq!(doc["lines_rejected"], 0);
    assert_eq!(doc["turns_accepted"], 2);
    assert_eq!(doc["turns_rejected"], 0);
    assert_eq!(doc["batch_reject"], "");
    assert_eq!(doc["rejects_omitted"], 0);
    assert_eq!(doc["commit_seq"], 2, "两个会话各一条提交，响应回最大提交号");
    assert_eq!(doc["epoch"], 1);
    let text = String::from_utf8_lossy(&resp.body);
    assert!(!text.contains("CANARY-INGEST"), "响应泄露正文：{text}");
    assert!(!text.contains(&dir.path().display().to_string()), "响应泄露路径：{text}");
    let root = Root::open(&data).unwrap();
    assert_eq!(rc_ledger::count_turns_on(root.transcript()), 2);
    assert_eq!(rc_ledger::count_sessions(root.transcript()), 2);
}

#[test]
fn ingest_rejects_foreign_device_lines_without_storing_them() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let body = format!(
        "{}\n{}\n",
        line("s-1", "dev-someone-else", "CANARY-FOREIGN"),
        line("s-2", &dev.device_id(), "kept"),
    );
    let resp = ingest_plain(&mut state, &dev, &token, body.as_bytes());
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["lines_rejected"], 1);
    assert_eq!(doc["turns_accepted"], 1);
    assert_eq!(doc["turns_rejected"], 1);
    assert_eq!(doc["turns_rejected_signature"], 1);
    assert_eq!(doc["rejects"][0]["code"], "device_mismatch");
    assert_eq!(doc["rejects"][0]["line"], 1);
    let root = Root::open(&data).unwrap();
    assert_eq!(rc_ledger::count_turns_on(root.transcript()), 1, "外来设备的行落库了");
    assert_eq!(counter(&data, &dev.device_id(), "ingest_turns_rejected"), Some(1));
    assert_eq!(counter(&data, &dev.device_id(), "ingest_turns_rejected_signature"), Some(1));
    assert_eq!(counter(&data, "dev-someone-else", "ingest_turns_rejected"), None,
        "拒收记到了声明方而不是发送方头上");
    // 审计行与当场报告同一个口径：整行被拒的 turn 也算进 turns_rejected。
    let logged: i64 = root
        .ledger()
        .conn()
        .query_row("SELECT turns_rejected FROM import_log ORDER BY rowid DESC LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(logged, 1);
}

#[test]
fn ingest_commits_are_monotonic_and_replay_does_not_consume_one() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let first = format!("{}\n", line("s-1", &dev.device_id(), "first"));
    let resp = ingest_plain(&mut state, &dev, &token, first.as_bytes());
    assert_eq!(resp.status, 200);
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["commit_seq"], 1);

    let second = format!("{}\n", line("s-2", &dev.device_id(), "second"));
    let resp = ingest_plain(&mut state, &dev, &token, second.as_bytes());
    assert_eq!(resp.status, 200);
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["commit_seq"], 2);

    // 完全相同的第一批：幂等命中，不消耗提交号。
    let resp = ingest_plain(&mut state, &dev, &token, first.as_bytes());
    assert_eq!(resp.status, 200);
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["turns_accepted"], 0);
    assert_eq!(doc["skipped_duplicate"], 1);
    assert!(doc["commit_seq"].is_null(), "全重复批次不许占提交号");
    assert_eq!(doc["epoch"], 1, "重复批次仍要回当前纪元");

    let root = Root::open(&data).unwrap();
    let (logs, last): (i64, i64) = root
        .transcript()
        .conn()
        .query_row(
            "SELECT (SELECT count(*) FROM ingest_log), last_commit_seq FROM center_state WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((logs, last), (2, 2));
    let stamped: Vec<i64> = {
        let mut st = root
            .transcript()
            .conn()
            .prepare("SELECT commit_seq FROM turns ORDER BY commit_seq")
            .unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(stamped, vec![1, 2]);
}

#[test]
fn ingest_gzip_body_is_signed_after_decompression() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let body = format!("{}\n", line("s-1", &dev.device_id(), "gzipped"));
    let wire = gzip(body.as_bytes());
    assert_ne!(wire, body.as_bytes());
    let resp = ingest_call(&mut state, &dev, &token, wire, body.as_bytes(), Some("gzip"));
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["turns_accepted"], 1);
    let root = Root::open(&data).unwrap();
    assert_eq!(rc_ledger::count_turns_on(root.transcript()), 1);
}

#[test]
fn ingest_signature_over_compressed_bytes_is_refused() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let body = format!("{}\n", line("s-1", &dev.device_id(), "x"));
    let wire = gzip(body.as_bytes());
    // 客户端错误地把签名盖在压缩字节上：服务端按协议盖在解压后字节上 ⇒ 验签必红。
    let resp = ingest_call(&mut state, &dev, &token, wire.clone(), &wire, Some("gzip"));
    assert_eq!(resp.status, 403);
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["error"], "bad_signature");
}

#[test]
fn ingest_requires_auth_before_decompressing() {
    let (_dir, _data, mut state) = fixture();
    // 无 token + 坏 gzip：先解压的实现在这里回 400，先认证的必须回 401。
    let resp = state.handle(&Request::new(
        "POST",
        "/v1/ingest",
        vec![("Content-Encoding".into(), "gzip".into())],
        b"definitely not gzip".to_vec(),
    ));
    assert_eq!(resp.status, 401);
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["error"], "invalid_token");
}

#[test]
fn whole_batch_reject_is_422_and_stores_nothing() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let id = dev.device_id();
    let a = format!(r#"{{"format_version":1,"sessions":[{{"session_key":"s-1","device_id":"{id}","source_agent":"q","turns":[{{"role":"human","body":"a","created_at_ms":1,"tier":"id_bearing","turn_uid":"dup-1"}}]}}]}}"#);
    let b = format!(r#"{{"format_version":1,"sessions":[{{"session_key":"s-2","device_id":"{id}","source_agent":"q","turns":[{{"role":"human","body":"b","created_at_ms":2,"tier":"id_bearing","turn_uid":"dup-1"}}]}}]}}"#);
    let body = format!("{a}\n{b}\n");
    let resp = ingest_plain(&mut state, &dev, &token, body.as_bytes());
    assert_eq!(resp.status, 422, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["error"], "ingest_failed");
    assert_eq!(doc["batch_reject"], "duplicate_turn_uid");
    assert_eq!(doc["turns_accepted"], 0);
    let root = Root::open(&data).unwrap();
    assert_eq!(rc_ledger::count_turns_on(root.transcript()), 0);
}

#[test]
fn per_line_rejects_are_capped_and_counted() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let body = (0..150)
        .map(|i| format!("{{\"nope\":{i}}}"))
        .collect::<Vec<_>>()
        .join("\n");
    let resp = ingest_plain(&mut state, &dev, &token, body.as_bytes());
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["lines_in"], 150);
    assert_eq!(doc["lines_rejected"], 150);
    assert_eq!(doc["rejects"].as_array().unwrap().len(), 100);
    assert_eq!(doc["rejects_omitted"], 50);
}

#[test]
fn ingest_writes_one_import_log_row_and_device_counters() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let body = format!(
        "{}\n{}\n",
        line("s-1", &dev.device_id(), "kept"),
        line("s-2", &dev.device_id(), "   "),
    );
    let resp = ingest_plain(&mut state, &dev, &token, body.as_bytes());
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["turns_accepted"], 1);
    assert_eq!(doc["turns_rejected"], 1);
    assert_eq!(doc["rejects"][0]["line"], 2, "turn 拒收没映射回它所在的行");
    assert_eq!(doc["rejects"][0]["turn"], 0);
    assert_eq!(doc["rejects"][0]["code"], "body_empty");
    let root = Root::open(&data).unwrap();
    let (sessions_in, turns_accepted, turns_rejected, batch): (i64, i64, i64, String) = root
        .ledger()
        .conn()
        .query_row(
            "SELECT sessions_in, turns_accepted, turns_rejected, batch_reject_code
             FROM import_log ORDER BY rowid DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!((sessions_in, turns_accepted, turns_rejected), (1, 1, 1));
    assert_eq!(batch, "");
    assert_eq!(counter(&data, &dev.device_id(), "ingest_turns_accepted"), Some(1));
    assert_eq!(counter(&data, &dev.device_id(), "ingest_turns_rejected"), Some(1));
    assert_eq!(counter(&data, &dev.device_id(), "ingest_turns_rejected_signature"), None);
}

#[test]
fn http_ingest_with_identity_writes_principal_mention_and_replay_is_idempotent() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let body = format!(
        "{}\n",
        line_with_identity(
            "s-p",
            &dev.device_id(),
            "p-http",
            "某人",
            "agent_runtime",
            "我明天把报价单发你",
        )
    );
    let resp = ingest_plain(&mut state, &dev, &token, body.as_bytes());
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["turns_accepted"], 1);

    {
        let root = Root::open(&data).unwrap();
        let n: i64 = root
            .memory()
            .conn()
            .query_row("SELECT count(*) FROM mention", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "http 上行带身份的人轮「我」应落 1 行 mention");
        let (surface, role, resolution, anchor_id, speaker, span_start, span_end): (
            String,
            String,
            String,
            String,
            Option<String>,
            Option<i64>,
            Option<i64>,
        ) = root
            .memory()
            .conn()
            .query_row(
                "SELECT surface, discourse_role, resolution, anchor_id, speaker_anchor_id,
                        span_start, span_end FROM mention",
                [],
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
            .unwrap();
        assert_eq!(surface, "我");
        assert_eq!(role, "speaker");
        assert_eq!(resolution, "resolved");
        assert_eq!(speaker.as_deref(), Some(anchor_id.as_str()), "「我」就是主人锚在说");
        assert_eq!((span_start, span_end), (None, None));
        let kind: String = root
            .memory()
            .conn()
            .query_row("SELECT kind FROM anchor WHERE id=?1", [&anchor_id], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, "person");
        let claims: i64 = root
            .transcript()
            .conn()
            .query_row(
                "SELECT count(*) FROM session_principal_claim
                  WHERE session_key='s-p' AND device_id=?1",
                [dev.device_id()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(claims, 1, "身份三格应落成一条 claim");
        let memory_mentions: i64 = root
            .memory()
            .conn()
            .query_row("SELECT count(*) FROM memory_mention", [], |r| r.get(0))
            .unwrap();
        assert_eq!(memory_mentions, 0, "memory_mention 不是这条规则的判据");
    }

    // 同一 body 重放（新 nonce）：转录层幂等命中，mention 也不许长第二行。
    let resp = ingest_plain(&mut state, &dev, &token, body.as_bytes());
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["turns_accepted"], 0);
    assert_eq!(doc["skipped_duplicate"], 1);
    assert_eq!(mention_count(&data), 1, "重放不许新增 mention");

    // 同会话再发一条**不带身份三格**的新 human 轮：claim 从库里回读。
    let second = format!("{}\n", line("s-p", &dev.device_id(), "我下周再补一版"));
    let resp = ingest_plain(&mut state, &dev, &token, second.as_bytes());
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["turns_accepted"], 1);
    assert_eq!(mention_count(&data), 2, "claim 从 sessions/claim 表回读，不要求每条都带身份");
}

#[test]
fn ingest_replayed_request_is_refused() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let body = format!("{}\n", line("s-1", &dev.device_id(), "once"));
    let doc = challenge(&mut state, &dev, &token);
    let nonce = doc["nonce"].as_str().unwrap().to_string();
    let headers = signed_headers(&dev, &token, &nonce, body.as_bytes());
    let req = Request::new("POST", "/v1/ingest", headers.clone(), body.clone().into_bytes());
    assert_eq!(state.handle(&req).status, 200);
    let again = Request::new("POST", "/v1/ingest", headers, body.into_bytes());
    let resp = state.handle(&again);
    assert_eq!(resp.status, 401);
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["error"], "bad_nonce");
}

#[test]
fn ingest_refuses_unknown_content_encoding() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let body = format!("{}\n", line("s-1", &dev.device_id(), "x"));
    let resp = ingest_call(
        &mut state, &dev, &token, body.clone().into_bytes(), body.as_bytes(), Some("br"),
    );
    assert_eq!(resp.status, 400);
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["error"], "bad_request");
}
