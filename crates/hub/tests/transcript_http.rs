//! W39 刀 C 的中心转录只读路由（`/v1/transcript/*`）黑盒面：
//! 已配对设备经 `Trust::Lan` 臂读到跨 agent / 跨 principal 的会话原文；
//! 未配对/坏 token 401/403；参数形状与上限 400；每次成功读落一行 `transcript_read`。
//!
//! 夹具形状照 `rows_http.rs`：同一个 `HubState` 直接 `handle`，不起监听端口。
//! 种子数据用 `rc_ledger::insert_batch` 直接写转录库（agent / principal / 会话名 /
//! 正文全部自造合成词）。
use hub::hub_cfg::HubConfig;
use hub::serve::HubState;
use rc_contract::{NormalizedTurn, SessionId, TurnTier};
use rc_net::http::{Handler, Request, Response};
use rc_net::key::{DeviceKey, HubKey};
use rc_store::Root;
use serde_json::{json, Value};
use std::path::Path;

const NOW: i64 = 1_000;
const DATA: &str = "data";
/// 两个会话共享的命中词（自造合成词，不同归属都命中）。
const MARKER: &str = "zephyrquartz";
/// 哪里都没有的词（空读数的零对照）。
const ABSENT: &str = "ptarmigan";

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, HubState) {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join(DATA);
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

/// 64 位小写 hex 的 turn_uid（形状门要求）。
fn uid(n: u8) -> String {
    format!("{:064x}", n)
}

fn session(key: &str, device: &str, agent: &str, principal: &str) -> SessionId {
    SessionId {
        session_key: key.to_string(),
        device_id: device.to_string(),
        source_agent: agent.to_string(),
        machine: None,
        project_hint: None,
        last_saved_at_ms: None,
        owner_principal_id: Some(principal.to_string()),
        principal_name: None,
        principal_source: None,
    }
}

fn turn(uid: &str, role: &str, body: &str, created_ms: i64) -> NormalizedTurn {
    NormalizedTurn {
        tier: TurnTier::ContentOnly,
        turn_uid: uid.to_string(),
        body: body.to_string(),
        body_fp: format!("fp-{uid}"),
        role: role.to_string(),
        created_at_ms: created_ms,
    }
}

/// 两个会话：A（agent qoder / principal p1）3 轮，B（agent dsh / principal p2）2 轮。
/// 两个会话都带 [`MARKER`]，归属不同——`Trust::Lan` 臂必须两边都读得到（Ownership 臂
/// 只读得到 A 的那半，变异回 `Ownership` 时本文件的阳性断言会红）。
fn seed(data: &Path) {
    let mut root = Root::open(data).unwrap();
    let a = session("ses-alpha", "dev-a", "qoder", "p1");
    let turns_a = [
        turn(&uid(0x11), "human", &format!("{MARKER} amber opening words"), 1_000),
        turn(&uid(0x12), "agent", &format!("{MARKER} amber middle words"), 1_001),
        turn(&uid(0x13), "human", &format!("{MARKER} amber closing words"), 1_002),
    ];
    rc_ledger::insert_batch(root.transcript_mut(), &a, &turns_a, 1_100).unwrap();

    let b = session("ses-beta", "dev-b", "dsh", "p2");
    let turns_b = [
        turn(&uid(0x21), "human", &format!("{MARKER} cobalt first words"), 2_000),
        turn(&uid(0x22), "agent", &format!("{MARKER} cobalt second words"), 2_001),
    ];
    rc_ledger::insert_batch(root.transcript_mut(), &b, &turns_b, 2_100).unwrap();
}

fn issue_code(data: &Path, code: &str) {
    let mut root = Root::open(data).unwrap();
    let hash = rc_net::pair::hash_pair_code(code).unwrap();
    rc_ledger::device::issue_pair_code(root.ledger_mut(), &hash, NOW, NOW + 300_000).unwrap();
}

fn pair_one(state: &mut HubState, data: &Path) -> (DeviceKey, String) {
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

/// 一发生签 GET：签名盖 `target`（含 query）与空体，与 `authenticate` 同一形。
/// `extra` 是可选的自报头（`x-recollect-agent` / `x-recollect-principal`）。
fn signed_get(
    state: &mut HubState,
    dev: &DeviceKey,
    token: &str,
    target: &str,
    extra: &[(&str, &str)],
) -> Response {
    let doc = challenge(state, dev, token);
    let nonce = doc["nonce"].as_str().unwrap();
    let canonical = rc_net::protocol::request_canonical(
        "GET",
        target,
        &rc_net::protocol::sha256_hex(&[]),
        &dev.device_id(),
        nonce,
    );
    let mut headers = vec![
        ("Authorization".into(), format!("Bearer {token}")),
        ("X-Recollect-Device".into(), dev.device_id()),
        ("X-Recollect-Nonce".into(), nonce.into()),
        ("X-Recollect-Sig".into(), dev.sign_b64(canonical.as_bytes())),
    ];
    for (name, value) in extra {
        headers.push(((*name).to_string(), (*value).to_string()));
    }
    state.handle(&Request::new("GET", target, headers, Vec::new()))
}

fn json_body(resp: &Response) -> Value {
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    serde_json::from_slice(&resp.body).unwrap()
}

/// `transcript_read` 审计行：`(actor, subject, detail)`，按 id 升序。
fn audit_rows(data: &Path) -> Vec<(String, Option<String>, String)> {
    let root = Root::open(data).unwrap();
    let mut st = root
        .ledger()
        .conn()
        .prepare(
            "SELECT actor, subject, detail FROM audit_log
             WHERE action='transcript_read' ORDER BY id",
        )
        .unwrap();
    let rows = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap();
    rows.map(Result::unwrap).collect()
}

#[test]
fn paired_device_reads_both_sessions_across_owners() {
    let (_dir, data, mut state) = fixture();
    seed(&data);
    let (dev, token) = pair_one(&mut state, &data);

    // ① 会话清单：两个归属不同的会话都在（Lan 臂不做归属过滤）。
    let doc = json_body(&signed_get(
        &mut state,
        &dev,
        &token,
        "/v1/transcript/sessions",
        &[],
    ));
    assert_eq!(doc["source"], "center");
    assert!(doc["epoch"].as_u64().unwrap() >= 1, "{doc}");
    assert_eq!(doc["truncated"], false);
    let sessions = doc["sessions"].as_array().unwrap();
    let keys: Vec<&str> = sessions
        .iter()
        .map(|s| s["session_key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["ses-alpha", "ses-beta"], "{doc}");
    assert_eq!(sessions[0]["owner_agent_id"], "qoder");
    assert_eq!(sessions[0]["owner_principal_id"], "p1");
    assert_eq!(sessions[1]["owner_agent_id"], "dsh");
    assert_eq!(sessions[1]["owner_principal_id"], "p2");
    assert_eq!(sessions[1]["turns"], 2);

    // ② 回放：异归属会话的正文全文读得到（Ownership 臂下这一条会空）。
    let doc = json_body(&signed_get(
        &mut state,
        &dev,
        &token,
        "/v1/transcript/session?session_key=ses-beta",
        &[],
    ));
    assert_eq!(doc["source"], "center");
    assert_eq!(doc["session_key"], "ses-beta");
    // W39 刀 E：回放信封带 device 与归属两列（归属随原文走）。
    assert_eq!(doc["device_id"], "dev-b");
    assert_eq!(doc["owner_agent_id"], "dsh");
    assert_eq!(doc["owner_principal_id"], "p2");
    let turns = doc["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2, "{doc}");
    assert_eq!(turns[0]["turn_uid"], uid(0x21));
    assert_eq!(turns[0]["role"], "human");
    assert_eq!(turns[0]["body"], format!("{MARKER} cobalt first words"));
    assert_eq!(doc["truncated"], false);

    // ③ 检索：两个会话的命中都在。
    let doc = json_body(&signed_get(
        &mut state,
        &dev,
        &token,
        &format!("/v1/transcript/search?q={MARKER}"),
        &[],
    ));
    assert_eq!(doc["source"], "center");
    assert_eq!(doc["matched"], 5, "{doc}");
    assert_eq!(doc["truncated"], false);
    assert_eq!(doc["coverage"]["indexed"], 5);
    let hits = doc["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 5, "{doc}");
    let hit_sessions: Vec<&str> = hits
        .iter()
        .map(|h| h["session_key"].as_str().unwrap())
        .collect();
    assert!(hit_sessions.contains(&"ses-alpha") && hit_sessions.contains(&"ses-beta"), "{doc}");
    // 命中自动带归属（`TurnHit` 序列化，无手工字段清单）：逐条核它所在会话的 owner。
    for h in hits {
        let (agent, principal) = if h["session_key"] == "ses-alpha" { ("qoder", "p1") } else { ("dsh", "p2") };
        assert_eq!(h["owner_agent_id"], agent, "{h}");
        assert_eq!(h["owner_principal_id"], principal, "{h}");
    }

    // ④ 按轮取窗口：中间轮 + 前后各 1 条邻居，归属随原文走。
    let doc = json_body(&signed_get(
        &mut state,
        &dev,
        &token,
        &format!("/v1/transcript/turn?turn_uid={}&before=1&after=1", uid(0x12)),
        &[],
    ));
    assert_eq!(doc["source"], "center");
    assert_eq!(doc["session_key"], "ses-alpha");
    assert_eq!(doc["device_id"], "dev-a");
    assert_eq!(doc["owner_agent_id"], "qoder");
    assert_eq!(doc["owner_principal_id"], "p1");
    let uids: Vec<&str> = doc["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["turn_uid"].as_str().unwrap())
        .collect();
    assert_eq!(uids, vec![uid(0x11), uid(0x12), uid(0x13)], "{doc}");
    assert_eq!(doc["truncated_before"], false);
    assert_eq!(doc["truncated_after"], false);

    // ⑤ 分页：limit=1 只回一行、截断如实。
    let doc = json_body(&signed_get(
        &mut state,
        &dev,
        &token,
        "/v1/transcript/session?session_key=ses-alpha&limit=1",
        &[],
    ));
    assert_eq!(doc["turns"].as_array().unwrap().len(), 1, "{doc}");
    assert_eq!(doc["truncated"], true, "还有轮却不报截断：{doc}");
    let doc = json_body(&signed_get(
        &mut state,
        &dev,
        &token,
        "/v1/transcript/sessions?limit=1",
        &[],
    ));
    assert_eq!(doc["sessions"].as_array().unwrap().len(), 1, "{doc}");
    assert_eq!(doc["truncated"], true, "{doc}");
}

#[test]
fn empty_reads_are_200_and_audited() {
    let (_dir, data, mut state) = fixture();
    seed(&data);
    let (dev, token) = pair_one(&mut state, &data);

    // 不存在的会话 / 轮 / 无命中词：同为 200 空读数（不 404、不泄露存在性）。
    let doc = json_body(&signed_get(
        &mut state,
        &dev,
        &token,
        "/v1/transcript/session?session_key=ses-nope",
        &[],
    ));
    assert_eq!(doc["turns"], json!([]), "{doc}");
    // 空页与"查无"同形：归属三格也是 null（不编一个没有原文的归属）。
    assert_eq!(doc["device_id"], Value::Null, "{doc}");
    assert_eq!(doc["owner_agent_id"], Value::Null, "{doc}");
    assert_eq!(doc["owner_principal_id"], Value::Null, "{doc}");

    let absent_uid = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    assert_eq!(absent_uid.len(), 64);
    let doc = json_body(&signed_get(
        &mut state,
        &dev,
        &token,
        &format!("/v1/transcript/turn?turn_uid={absent_uid}"),
        &[],
    ));
    assert_eq!(doc["turns"], json!([]), "{doc}");
    assert_eq!(doc["session_key"], Value::Null, "{doc}");

    let doc = json_body(&signed_get(
        &mut state,
        &dev,
        &token,
        &format!("/v1/transcript/search?q={ABSENT}"),
        &[],
    ));
    assert_eq!(doc["matched"], 0, "{doc}");
    assert_eq!(doc["hits"], json!([]), "{doc}");

    // 空读数同样落审计（hits=0），subject 里只有 session_key、没有轮正文。
    let rows = audit_rows(&data);
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(rows.iter().all(|(_, _, detail)| {
        let doc: Value = serde_json::from_str(detail).unwrap();
        doc["hits"].as_i64() == Some(0)
    }));
}

#[test]
fn unpaired_or_bad_token_is_refused() {
    let (_dir, data, mut state) = fixture();
    seed(&data);
    let (dev, token) = pair_one(&mut state, &data);

    // 完全没有凭证 ⇒ 401。
    let resp = state.handle(&Request::new("GET", "/v1/transcript/sessions", Vec::new(), Vec::new()));
    assert_eq!(resp.status, 401, "{}", String::from_utf8_lossy(&resp.body));

    // 乱 token ⇒ 401（invalid_token）。
    let resp = state.handle(&Request::new(
        "GET",
        "/v1/transcript/sessions",
        vec![
            ("Authorization".into(), "Bearer not-a-token".into()),
            ("X-Recollect-Device".into(), dev.device_id()),
        ],
        Vec::new(),
    ));
    assert_eq!(resp.status, 401, "{}", String::from_utf8_lossy(&resp.body));

    // 合法 token 顶着别人的 device 头 ⇒ 403（device_mismatch）。
    let resp = state.handle(&Request::new(
        "GET",
        "/v1/transcript/sessions",
        vec![
            ("Authorization".into(), format!("Bearer {token}")),
            ("X-Recollect-Device".into(), "dev-somebody-else".into()),
        ],
        Vec::new(),
    ));
    assert_eq!(resp.status, 403, "{}", String::from_utf8_lossy(&resp.body));

    // 拒绝的请求一条审计都不落。
    assert!(audit_rows(&data).is_empty(), "失败请求不该写 transcript_read");
}

#[test]
fn malformed_parameters_are_400() {
    let (_dir, data, mut state) = fixture();
    seed(&data);
    let (dev, token) = pair_one(&mut state, &data);

    let cases = [
        // turn_uid：63 位、64 位大写、非 hex。
        "/v1/transcript/turn?turn_uid=abc".to_string(),
        format!("/v1/transcript/turn?turn_uid={}", "A".repeat(64)),
        format!("/v1/transcript/turn?turn_uid={}&before=11", uid(0x11)),
        format!("/v1/transcript/turn?turn_uid={}&after=eleven", uid(0x11)),
        // limit：0 / 越界 / 非数字。
        "/v1/transcript/sessions?limit=0".to_string(),
        "/v1/transcript/sessions?limit=201".to_string(),
        "/v1/transcript/session?session_key=ses-alpha&limit=0".to_string(),
        "/v1/transcript/session?session_key=ses-alpha&limit=nope".to_string(),
        format!("/v1/transcript/search?q={MARKER}&limit=21"),
        format!("/v1/transcript/search?q={MARKER}&limit=0"),
        // 必填缺席。
        "/v1/transcript/session".to_string(),
        "/v1/transcript/search".to_string(),
        "/v1/transcript/turn".to_string(),
    ];
    for target in cases {
        let resp = signed_get(&mut state, &dev, &token, &target, &[]);
        assert_eq!(resp.status, 400, "{target}: {}", String::from_utf8_lossy(&resp.body));
        let doc: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(doc["error"], "bad_request", "{target}");
    }
    assert!(audit_rows(&data).is_empty(), "400 不该写 transcript_read");

    // 未列方法进 405 清单。
    let resp = state.handle(&Request::new("POST", "/v1/transcript/sessions", Vec::new(), Vec::new()));
    assert_eq!(resp.status, 405, "{}", String::from_utf8_lossy(&resp.body));
}

#[test]
fn each_successful_read_appends_one_audit_row_without_body() {
    let (_dir, data, mut state) = fixture();
    seed(&data);
    let (dev, token) = pair_one(&mut state, &data);

    // 四次成功读，自报 agent/principal 只进审计。
    let extra = [("x-recollect-agent", "agent-probe"), ("x-recollect-principal", "p-probe")];
    json_body(&signed_get(&mut state, &dev, &token, "/v1/transcript/sessions", &extra));
    json_body(&signed_get(
        &mut state,
        &dev,
        &token,
        "/v1/transcript/session?session_key=ses-alpha",
        &extra,
    ));
    json_body(&signed_get(
        &mut state,
        &dev,
        &token,
        &format!("/v1/transcript/search?q={MARKER}&limit=2"),
        &extra,
    ));
    json_body(&signed_get(
        &mut state,
        &dev,
        &token,
        &format!("/v1/transcript/turn?turn_uid={}&before=1", uid(0x12)),
        &extra,
    ));

    let rows = audit_rows(&data);
    assert_eq!(rows.len(), 4, "{rows:?}");
    for (actor, _, _) in &rows {
        assert_eq!(actor, &dev.device_id(), "actor 必须是 device_id：{rows:?}");
    }
    // 形状：(route, agent, principal, hits)；detail 不含正文/命中词。
    let details: Vec<Value> = rows
        .iter()
        .map(|(_, _, detail)| {
            assert!(!detail.contains(MARKER), "审计 detail 泄漏正文：{detail}");
            assert!(!detail.contains("amber"), "审计 detail 泄漏正文：{detail}");
            serde_json::from_str(detail).unwrap()
        })
        .collect();
    assert_eq!(details[0]["route"], "/v1/transcript/sessions");
    assert_eq!(details[0]["hits"], 2);
    assert_eq!(details[1]["route"], "/v1/transcript/session");
    assert_eq!(details[1]["hits"], 3);
    assert_eq!(details[2]["route"], "/v1/transcript/search");
    assert_eq!(details[2]["hits"], 2, "limit=2 下的 hits 是本次返回条数：{details:?}");
    assert_eq!(details[3]["route"], "/v1/transcript/turn");
    assert_eq!(details[3]["hits"], 2);
    for detail in &details {
        assert_eq!(detail["agent"], "agent-probe");
        assert_eq!(detail["principal"], "p-probe");
    }
    // subject：会话/轮路由带 session_key，清单与检索路由为 None。
    assert_eq!(rows[0].1, None);
    assert_eq!(rows[1].1.as_deref(), Some("ses-alpha"));
    assert_eq!(rows[2].1, None);
    assert_eq!(rows[3].1.as_deref(), Some("ses-alpha"));

    // 缺省自报头 ⇒ "-"（不进准入，只进审计）。
    json_body(&signed_get(&mut state, &dev, &token, "/v1/transcript/sessions", &[]));
    let rows = audit_rows(&data);
    let last: Value = serde_json::from_str(&rows.last().unwrap().2).unwrap();
    assert_eq!(last["agent"], "-");
    assert_eq!(last["principal"], "-");
}
