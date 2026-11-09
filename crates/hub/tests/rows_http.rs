//! `POST /v1/rows` 的黑盒面：配对 → 签名 → 吸收 → 标记 → 下一轮不重蒸。
//! 夹具形状照 `ingest_http.rs`：同一个 `HubState` 直接 `handle`，不起监听端口。
//!
//! S3c-4：`fact` 行改走"程序闸 → 入队 → 同步窄审 → apply"，本文件后半段用
//! `HubState::with_backend_factory` 注入脚本后端（`Arc<Mutex<VecDeque>>`，Send+Sync）逐条考它。
use hub::hub_cfg::HubConfig;
use hub::serve::{BackendFactory, HubState};
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_net::http::{Handler, Request, Response};
use rc_net::key::{DeviceKey, HubKey};
use rc_store::Root;
use rusqlite::OptionalExtension;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

const NOW: i64 = 1_000;
const DATA: &str = "data";

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

/// 一发生签请求：签名盖 `path` 与 `body` 的原始字节（与 ingest 的 `authenticate_over` 同一形）。
fn signed_call(
    state: &mut HubState,
    dev: &DeviceKey,
    token: &str,
    path: &str,
    body: Vec<u8>,
) -> Response {
    let doc = challenge(state, dev, token);
    let nonce = doc["nonce"].as_str().unwrap();
    let canonical = rc_net::protocol::request_canonical(
        "POST",
        path,
        &rc_net::protocol::sha256_hex(&body),
        &dev.device_id(),
        nonce,
    );
    let headers = vec![
        ("Authorization".into(), format!("Bearer {token}")),
        ("X-Recollect-Device".into(), dev.device_id()),
        ("X-Recollect-Nonce".into(), nonce.into()),
        ("X-Recollect-Sig".into(), dev.sign_b64(canonical.as_bytes())),
    ];
    state.handle(&Request::new("POST", path, headers, body))
}

fn line(device: &str, body: &str) -> String {
    format!(
        r#"{{"format_version":1,"sessions":[{{"session_key":"s-rows","device_id":"{device}","source_agent":"qoder","turns":[{{"role":"human","body":"{body}","created_at_ms":1}}]}}]}}"#
    )
}

fn one_turn_uid(data: &Path) -> String {
    let root = Root::open(data).unwrap();
    root.transcript()
        .conn()
        .query_row("SELECT turn_uid FROM turns LIMIT 1", [], |r| r.get(0))
        .unwrap()
}

fn card_row(id: &str, body: &str) -> Value {
    json!({
        "id": id, "kind": "lesson", "title": "回并的卡片", "text": body,
        "scope_kind": "session", "scope_key": "dev-rows/s-rows",
        "as_of_ms": 500, "valid_to_ms": null, "superseded_by": null,
        "verdict": "passed", "cooldown_until_ms": null,
        "source_agent": "agent-rows", "device_id": "dev-rows",
        "created_at_ms": 600, "norm_version": 1
    })
}

fn provenance(mem_id: &str, uid: &str) -> Value {
    json!({
        "mem_table": "card", "mem_id": mem_id, "turn_uid": uid,
        "anchor_role": "human", "ref_kind": "turn", "ord": null, "role": "basis"
    })
}

fn rows_body(tables: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({"api_version": 1, "tables": tables})).unwrap()
}

fn card_count(data: &Path, id: &str) -> i64 {
    let root = Root::open(data).unwrap();
    root.memory()
        .conn()
        .query_row("SELECT count(*) FROM card WHERE id=?1", [id], |r| r.get(0))
        .unwrap()
}

fn marked_count(data: &Path, uid: &str) -> i64 {
    let root = Root::open(data).unwrap();
    root.ledger()
        .conn()
        .query_row(
            "SELECT count(*) FROM pre_distilled_turn WHERE turn_uid=?1",
            [uid],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn a_signed_row_batch_is_absorbed_and_the_turn_is_marked() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let ingest = line(&dev.device_id(), "CANARY-ROWS-11").into_bytes();
    let resp = signed_call(&mut state, &dev, &token, "/v1/ingest", ingest);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let uid = one_turn_uid(&data);

    let payload = rows_body(json!({
        "card": [card_row("card-1", "从节点回并的正文")],
        "provenance": [provenance("card-1", &uid)],
    }));
    let resp = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["api_version"], 1);
    assert_eq!(doc["rows_in"], 2);
    assert_eq!(doc["absorbed"], 2);
    assert_eq!(doc["skipped_duplicate"], 0);
    assert_eq!(doc["turns_marked"], 1);

    let root = Root::open(&data).unwrap();
    let body: String = root.memory()
        .conn()
        .query_row("SELECT text FROM card WHERE id='card-1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(body, "从节点回并的正文");
    assert_eq!(card_count(&data, "card-1"), 1);
    assert_eq!(marked_count(&data, &uid), 1);
    // 硬序的另一半：中心确认收下后必须立刻有一代新快照可下发，否则节点丢层后看不到这行。
    let snaps = std::fs::read_dir(data.join("snapshots")).expect("吸收后应有快照目录");
    assert!(
        snaps.flatten().any(|e| e.file_name().to_string_lossy().ends_with(".db")),
        "rows 吸收后没有发布新快照"
    );
}

#[test]
fn the_same_batch_replayed_is_all_duplicates() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let payload = rows_body(json!({
        "card": [card_row("card-1", "重放同一批")],
        "provenance": [provenance("card-1", "turn-replay")],
    }));
    let first = signed_call(&mut state, &dev, &token, "/v1/rows", payload.clone());
    assert_eq!(first.status, 200, "{}", String::from_utf8_lossy(&first.body));
    let second = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(second.status, 200, "{}", String::from_utf8_lossy(&second.body));
    let doc: Value = serde_json::from_slice(&second.body).unwrap();
    assert_eq!(doc["rows_in"], 2);
    assert_eq!(doc["absorbed"], 0);
    assert_eq!(doc["skipped_duplicate"], 2);
    assert_eq!(doc["turns_marked"], 1);
    assert_eq!(card_count(&data, "card-1"), 1, "重放不许长出第二行");
    let root = Root::open(&data).unwrap();
    let provs: i64 = root.memory()
        .conn()
        .query_row("SELECT count(*) FROM provenance", [], |r| r.get(0))
        .unwrap();
    assert_eq!(provs, 1);
    assert_eq!(marked_count(&data, "turn-replay"), 1);
}

#[test]
fn a_pre_distilled_turn_is_not_selected_by_the_next_cycle() {
    let (dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    // 先真 ingest 一条 turn：下一轮若不读 `pre_distilled_turn`，它就会被选中（变异会红在这条上）。
    let ingest = line(&dev.device_id(), "CANARY-PRE-31").into_bytes();
    let resp = signed_call(&mut state, &dev, &token, "/v1/ingest", ingest);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let uid = one_turn_uid(&data);
    // 只回并一行 provenance：它负责标记 turn；没有事实行 ⇒ 推导池为空，这一轮只考"不重蒸"。
    let payload = rows_body(json!({
        "provenance": [provenance("card-pre", &uid)],
    }));
    let resp = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["turns_marked"], 1);

    let replies = dir.path().join("replies.json");
    std::fs::write(&replies, "[]").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_recollect-hub"))
        .env_remove("RECOLLECT_DEVICE_ID")
        .args(["cycle", "once", "--replies", replies.to_str().unwrap(),
               "--data-dir", data.to_str().unwrap()])
        .output()
        .expect("hub binary");
    assert_eq!(
        out.status.code(),
        Some(0),
        "cycle 失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["turns_scanned"], 0, "已回并的 turn 被重新选中了");
    assert_eq!(doc["rows_written"], 0);

    let root = Root::open(&data).unwrap();
    // S3a-fix（fix round 1）：这一句的本意是"这一轮一次 LLM 口都没开"，所以只数 `llm_calls`
    // 那一支。S3a 之后 cycle 的收尾**无条件**写 `llm_job:cycle:quota_stopped`（"跑过且没触顶"
    // 必须落 0，不许留空冒充 null），那是运行记录不是调用记录——按主语整表数会把它当成
    // "开过口"，本条于是恒红。加 `metric` 这一格把口径退回它本来要断的那件事。
    let llm_job = rc_ledger::counters::read_all(root.ledger())
        .unwrap()
        .into_iter()
        .filter(|r| r.subject_kind == "llm_job" && r.metric == "llm_calls")
        .count();
    assert_eq!(llm_job, 0, "回并后的下一轮不该再开 LLM 口");
}

#[test]
fn unknown_tables_columns_and_attention_are_refused() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let cases = [
        ("unknown table", json!({"nope": []})),
        ("unknown column", json!({"card": [{"id": "c", "nope": 1}]})),
        ("attention", json!({"attention": []})),
        // S1b-C2a/C2b：归属侧表是"节点永不许上传归属"的伪造面，必须按未知表拒（400）。
        // 表名在 MEMORY_TABLES 里，所以这里考的是排除集而不是拼错名字。
        ("resource_owner", json!({"resource_owner": []})),
        ("resource_grant", json!({"resource_grant": []})),
    ];
    for (what, tables) in cases {
        let resp = signed_call(&mut state, &dev, &token, "/v1/rows", rows_body(tables));
        assert_eq!(resp.status, 400, "{what}: {}", String::from_utf8_lossy(&resp.body));
        let doc: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(doc["error"], "bad_request", "{what}");
        assert_eq!(card_count(&data, "c"), 0, "{what}: 拒收的载荷落了行");
    }
}

#[test]
fn an_unsigned_row_batch_is_refused() {
    let (_dir, data, mut state) = fixture();
    let payload = rows_body(json!({
        "card": [card_row("card-unsigned", "不该落库")],
        "provenance": [provenance("card-unsigned", "turn-unsigned")],
    }));
    let resp = state.handle(&Request::new("POST", "/v1/rows", Vec::new(), payload));
    assert!(resp.status == 401 || resp.status == 403, "{}", String::from_utf8_lossy(&resp.body));
    assert_eq!(card_count(&data, "card-unsigned"), 0);
    assert_eq!(marked_count(&data, "turn-unsigned"), 0);
}

#[test]
fn same_id_with_different_content_is_ignored() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let first = rows_body(json!({
        "card": [card_row("card-same", "原正文")],
        "provenance": [provenance("card-same", "turn-same")],
    }));
    let resp = signed_call(&mut state, &dev, &token, "/v1/rows", first);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));

    let second = rows_body(json!({
        "card": [card_row("card-same", "改过的正文")],
        "provenance": [provenance("card-same", "turn-same")],
    }));
    let resp = signed_call(&mut state, &dev, &token, "/v1/rows", second);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["absorbed"], 0, "同 id 不同内容不许覆盖");
    assert_eq!(doc["skipped_duplicate"], 2);

    let root = Root::open(&data).unwrap();
    let body: String = root.memory()
        .conn()
        .query_row("SELECT text FROM card WHERE id='card-same'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(body, "原正文", "幂等 ≠ 覆盖");
}

// ───────────────────────── S1b-C2b：吸收时推导 owner ─────────────────────────
// 判定面：`/v1/rows` 不信任载荷里的归属（载荷带 `resource_owner` 直接 400，见上一条用例），
// owner 只从 hub 自己的 `provenance.turn_uid → turns → sessions` 推导，
// 推不出时走"行 source_agent / unknown + private"的兜底，冲突逐行抑制并记账。

/// 带指定 session/agent 的一行 v1 载荷（与 `line` 同形，只多两个可调项）。
fn line_in(device: &str, session: &str, agent: &str, body: &str) -> String {
    format!(
        r#"{{"format_version":1,"sessions":[{{"session_key":"{session}","device_id":"{device}","source_agent":"{agent}","turns":[{{"role":"human","body":"{body}","created_at_ms":1}}]}}]}}"#
    )
}

fn turn_uid_of(data: &Path, session_key: &str) -> String {
    let root = Root::open(data).unwrap();
    root.transcript()
        .conn()
        .query_row(
            "SELECT turn_uid FROM turns WHERE session_key=?1",
            [session_key],
            |r| r.get(0),
        )
        .unwrap()
}

#[derive(Debug, PartialEq, Eq)]
struct Owner {
    principal: Option<String>,
    agent: String,
    created_by: String,
    visibility: String,
    retention: String,
    scope_kind: String,
    scope_key: String,
    source_agent: Option<String>,
    device_id: Option<String>,
    created_at_ms: i64,
}

fn owner_of(data: &Path, mem_table: &str, mem_id: &str) -> Option<Owner> {
    let root = Root::open(data).unwrap();
    root.memory()
        .conn()
        .query_row(
            "SELECT owner_principal_id, owner_agent_id, created_by_agent, visibility, retention,
                    scope_kind, scope_key, source_agent, device_id, created_at_ms
             FROM resource_owner WHERE mem_table=?1 AND mem_id=?2",
            [mem_table, mem_id],
            |r| {
                Ok(Owner {
                    principal: r.get(0)?,
                    agent: r.get(1)?,
                    created_by: r.get(2)?,
                    visibility: r.get(3)?,
                    retention: r.get(4)?,
                    scope_kind: r.get(5)?,
                    scope_key: r.get(6)?,
                    source_agent: r.get(7)?,
                    device_id: r.get(8)?,
                    created_at_ms: r.get(9)?,
                })
            },
        )
        .optional()
        .unwrap()
}

fn memory_op_count(data: &Path, mem_id: &str, reason: &str) -> i64 {
    let root = Root::open(data).unwrap();
    root.memory()
        .conn()
        .query_row(
            "SELECT count(*) FROM memory_op
             WHERE op='NOOP' AND mem_table='card' AND mem_id=?1 AND reason=?2 AND actor='program'",
            [mem_id, reason],
            |r| r.get(0),
        )
        .unwrap()
}

fn conflict_counter(data: &Path) -> Option<i64> {
    let root = Root::open(data).unwrap();
    root.ledger()
        .conn()
        .query_row(
            "SELECT count FROM usage_counter
             WHERE subject_kind='maintenance' AND subject_id='ownership'
               AND metric='ownership_conflicts'",
            [],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
}

/// anchor 行没有 `source_agent`/`device_id` 列（设计 §2.1 的"anchor 无该列 ⇒ NULL"）。
fn anchor_row(id: &str, scope_key: &str, created_at_ms: i64) -> Value {
    json!({
        "id": id, "kind": "concept", "canonical": format!("回并锚-{id}"),
        "reality_status": "unknown", "world_id": null,
        "scope_kind": "project", "scope_key": scope_key,
        "status": "active", "profile_text": null,
        "created_at_ms": created_at_ms, "updated_at_ms": created_at_ms, "norm_version": 1
    })
}

/// card + provenance 吸收后，`resource_owner` 的每一列都来自 hub 自己的 transcript：
/// owner=session 的 `COALESCE(owner_agent_id,source_agent)`（=qoder），
/// produced-by / scope / 时刻来自**行载荷**（只用于填充，不当权限来源），
/// `scope_kind` 由载荷的契约五值 `session` 归一成记忆行的 `project`。
#[test]
fn an_absorbed_card_gets_its_owner_from_the_hub_transcript() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let ingest = line(&dev.device_id(), "CANARY-OWNER-1").into_bytes();
    let resp = signed_call(&mut state, &dev, &token, "/v1/ingest", ingest);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let uid = one_turn_uid(&data);

    let payload = rows_body(json!({
        "card": [card_row("card-own", "归属来自中心的会话库")],
        "provenance": [provenance("card-own", &uid)],
    }));
    let resp = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));

    assert_eq!(
        owner_of(&data, "card", "card-own"),
        Some(Owner {
            principal: None,
            agent: "qoder".into(),
            created_by: "agent-rows".into(),
            visibility: "private_agent".into(),
            retention: "default".into(),
            // 载荷写的是 "session"（契约五值），落库必须是记忆行词表的 project。
            scope_kind: "project".into(),
            // 行自己的 scope_key 优先（不是 session 的 's-rows'）。
            scope_key: "dev-rows/s-rows".into(),
            source_agent: Some("agent-rows".into()),
            device_id: Some("dev-rows".into()),
            created_at_ms: 600,
        })
    );
}

/// 没有 provenance 的 anchor：`payload_owner`（整批唯一 session owner）补齐它；
/// 整批解析不出 session（只剩 anchor）或解析出多个，则落 `unknown + private_agent`。
///
/// 口径：owner 只能从 `provenance→turns→sessions` 推，**没有**其它来源；
/// 所以"只有 anchor 的一批"里没有任何 turn，恒落 unknown（见报告里登记的失效句）。
#[test]
fn an_anchor_without_provenance_falls_back_from_the_payload_owner_to_unknown() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let ingest = line(&dev.device_id(), "CANARY-OWNER-2").into_bytes();
    let resp = signed_call(&mut state, &dev, &token, "/v1/ingest", ingest);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let uid1 = turn_uid_of(&data, "s-rows");

    // ① 整批只有一个 session owner（card-a 的 provenance 指到它）⇒ anchor 也拿它。
    let payload = rows_body(json!({
        "anchor": [anchor_row("a-own", "dev-rows/s-rows", 700)],
        "card": [card_row("card-a", "带出处的卡")],
        "provenance": [provenance("card-a", &uid1)],
    }));
    let resp = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let anchor = owner_of(&data, "anchor", "a-own").expect("anchor 没拿到 payload_owner");
    assert_eq!(anchor.agent, "qoder", "anchor 没继承整批唯一 owner");
    assert_eq!(anchor.created_by, "qoder", "anchor 没有行 source_agent ⇒ produced-by 退回 session");
    assert_eq!(anchor.visibility, "private_agent");
    assert_eq!(anchor.scope_kind, "project");
    assert_eq!(anchor.scope_key, "dev-rows/s-rows");
    assert_eq!(anchor.source_agent, None, "anchor 没有 source_agent 列");
    assert_eq!(anchor.device_id, None, "anchor 没有 device_id 列");
    assert_eq!(anchor.created_at_ms, 700);

    // ② 只剩 anchor（载荷里没有任何 turn）⇒ 推不出任何 session ⇒ unknown + private_agent。
    let payload = rows_body(json!({
        "anchor": [anchor_row("a-lone", "dev-rows/s-rows", 710)],
    }));
    let resp = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let lone = owner_of(&data, "anchor", "a-lone").expect("孤立 anchor 没写归属行");
    assert_eq!(lone.agent, "unknown");
    assert_eq!(lone.created_by, "unknown");
    assert_eq!(lone.visibility, "private_agent");
    assert_eq!(lone.retention, "default");
    assert_eq!(lone.created_at_ms, 710);

    // ③ 整批解析出两个 session owner ⇒ payload_owner 不成立，anchor 仍是 unknown。
    let ingest2 =
        line_in(&dev.device_id(), "s-rows-2", "agent-other", "CANARY-OWNER-2B").into_bytes();
    let resp = signed_call(&mut state, &dev, &token, "/v1/ingest", ingest2);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let uid2 = turn_uid_of(&data, "s-rows-2");
    let payload = rows_body(json!({
        "anchor": [anchor_row("a-multi", "dev-rows/s-rows", 720)],
        "card": [card_row("card-m1", "第一处出处")],
        "provenance": [provenance("card-m1", &uid1), provenance("card-m1", &uid2)],
    }));
    let resp = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    assert_eq!(
        owner_of(&data, "anchor", "a-multi").unwrap().agent,
        "unknown",
        "两个 session owner 时 anchor 不该随便挑一个"
    );
    // 同一行 provenance 跨两个 session ⇒ 它自己也 unresolved（行 source_agent 兜底）。
    assert_eq!(
        owner_of(&data, "card", "card-m1").unwrap().agent,
        "agent-rows",
        "跨 session 的行没有按 unresolved 兜底"
    );
}

/// 冲突：同一 `mem_id` 第二次从另一个 session/owner 上行 ⇒ 不覆盖已有 owner、逐行抑制、
/// 留 `memory_op(reason='owner_conflict')` + 计数一次。
///
/// `absorbed=1` 而不是 0：这次载荷新增了一条 provenance 行（首次吸收没有那条出处），
/// 被抑制的是 card 那一行本身——`skipped_duplicate=1`、行数与正文都不动。
#[test]
fn a_second_owner_for_the_same_mem_id_is_suppressed_and_counted() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let resp = signed_call(
        &mut state,
        &dev,
        &token,
        "/v1/ingest",
        line(&dev.device_id(), "CANARY-CONFLICT-1").into_bytes(),
    );
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let resp = signed_call(
        &mut state,
        &dev,
        &token,
        "/v1/ingest",
        line_in(&dev.device_id(), "s-rows-2", "agent-other", "CANARY-CONFLICT-2").into_bytes(),
    );
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let uid1 = turn_uid_of(&data, "s-rows");
    let uid2 = turn_uid_of(&data, "s-rows-2");

    let payload = rows_body(json!({
        "card": [card_row("card-cc", "先写者的正文")],
        "provenance": [provenance("card-cc", &uid1)],
    }));
    let resp = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let before = owner_of(&data, "card", "card-cc").expect("第一次吸收没有写 owner");
    assert_eq!(before.agent, "qoder");
    assert_eq!(conflict_counter(&data), None, "第一次吸收不该记冲突");

    // 第二个 session（source_agent=agent-other）带着同一 id、改过的正文上行。
    let payload = rows_body(json!({
        "card": [card_row("card-cc", "后来者的正文")],
        "provenance": [provenance("card-cc", &uid2)],
    }));
    let resp = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["absorbed"], 1, "只有新出处落库，被抑制的 card 行不许计数：{doc}");
    assert_eq!(doc["skipped_duplicate"], 1, "{doc}");

    // 记忆行没长第二行、正文没被覆盖。
    assert_eq!(card_count(&data, "card-cc"), 1);
    let root = Root::open(&data).unwrap();
    let text: String = root
        .memory()
        .conn()
        .query_row("SELECT text FROM card WHERE id='card-cc'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(text, "先写者的正文", "冲突不许走覆盖路径");

    // 归属行逐列不变（先写者赢）。
    assert_eq!(owner_of(&data, "card", "card-cc"), Some(before), "已有 owner 被第二次上行改写了");
    // 逐行抑制的审计与计数各一格。
    assert_eq!(memory_op_count(&data, "card-cc", "owner_conflict"), 1, "没留 owner_conflict 记账");
    assert_eq!(conflict_counter(&data), Some(1), "冲突计数不是 1");
}

// ──────────────────── S3c-4：fact 行 = 闸 → 入队 → 同步窄审 → apply ────────────────────

/// 一条 `pass` 判定（`rows_review` 的输出契约）。
const PASS_ONE: &str = r#"{"schema_version":0,"pass":"rows_review","verdicts":[{"ref":"r1","verdict":"pass","code":null}]}"#;
/// 一条 `reject` 判定（码取 `unsupported`，与 prompt 反例同一套）。
const REJECT_ONE: &str = r#"{"schema_version":0,"pass":"rows_review","verdicts":[{"ref":"r1","verdict":"reject","code":"unsupported"}]}"#;

/// 脚本后端：脚本与收到的 prompt 都放在 `Arc<Mutex<…>>` 里（`BackendFactory` 要求 Send+Sync）。
#[derive(Clone)]
struct Script {
    replies: Arc<Mutex<VecDeque<Result<String, LlmFailure>>>>,
    prompts: Arc<Mutex<Vec<Prompt>>>,
}

impl Script {
    fn new(replies: Vec<Result<String, LlmFailure>>) -> Script {
        Script {
            replies: Arc::new(Mutex::new(replies.into())),
            prompts: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn factory(&self) -> BackendFactory {
        let script = self.clone();
        Box::new(move || Ok(Box::new(script.clone()) as Box<dyn LlmBackend>))
    }

    fn prompts(&self) -> Vec<Prompt> {
        self.prompts.lock().unwrap().clone()
    }
}

impl LlmBackend for Script {
    fn complete(&self, prompt: &Prompt) -> Result<String, LlmFailure> {
        self.prompts.lock().unwrap().push(prompt.clone());
        // 脚本耗尽 ⇒ Timeout（与 hub 的 `Seat` 同一条语义），调用方按 LLM 故障 503。
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Err(LlmFailure::Timeout))
    }

    fn slug(&self) -> Option<&str> {
        Some("script")
    }
}

/// 一行合法 fact（已过闸的形状）：证据轮、闭集内的 category/mode、不带 ASCII 数字的 text。
fn fact_row(id: &str, subject: &str, predicate: &str, value: &str, category: &str, uid: &str) -> Value {
    json!({
        "id": id,
        "subject_anchor_id": subject,
        "predicate": predicate,
        "value_text": value,
        "category": category,
        "mode": "stated",
        "text": "上线时间另行通知",
        "evidence_turn_uid": uid,
        "scope_kind": "session",
        "scope_key": "dev-rows/s-rows",
        "source_agent": "agent-rows",
        "device_id": "dev-rows",
        "created_at_ms": 600,
        "norm_version": 1
    })
}

/// fact 行的出处（`provenance.mem_table='fact'`）；闸/水位都按它找轮。
fn fact_provenance(mem_id: &str, uid: &str) -> Value {
    json!({
        "mem_table": "fact", "mem_id": mem_id, "turn_uid": uid,
        "anchor_role": "human", "ref_kind": "turn", "ord": null, "role": "basis"
    })
}

fn fact_count(data: &Path, id: &str) -> i64 {
    let root = Root::open(data).unwrap();
    root.memory()
        .conn()
        .query_row("SELECT count(*) FROM fact WHERE id=?1", [id], |r| r.get(0))
        .unwrap()
}

/// `pass_id=rows` 的全部 item：`(id, state, program_code)`，按 id 升序。
fn rows_items(data: &Path) -> Vec<(i64, String, Option<String>)> {
    let root = Root::open(data).unwrap();
    let mut st = root
        .memory()
        .conn()
        .prepare("SELECT id, state, program_code FROM review_item WHERE pass_id='rows' ORDER BY id")
        .unwrap();
    let rows = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap();
    rows.map(Result::unwrap).collect()
}

/// 一条 item 的判定链：`stage:verdict`，按 id 升序。
fn decision_chain(data: &Path, item_id: i64) -> Vec<String> {
    let root = Root::open(data).unwrap();
    let mut st = root
        .memory()
        .conn()
        .prepare("SELECT stage, verdict FROM review_decision WHERE item_id=?1 ORDER BY id")
        .unwrap();
    let rows = st
        .query_map([item_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap();
    rows.map(|row| {
        let (stage, verdict) = row.unwrap();
        format!("{stage}:{verdict}")
    })
    .collect()
}

/// 一次真实 ingest 落一条轮，返回它的 `turn_uid`。
fn ingest_turn(state: &mut HubState, data: &Path, dev: &DeviceKey, token: &str, body: &str) -> String {
    let resp = signed_call(state, dev, token, "/v1/ingest", line(&dev.device_id(), body).into_bytes());
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    one_turn_uid(data)
}

/// fact 通过：200、`rows_absorbed=1`、fact 一行且 owner 来自会话、item applied、
/// 判定链 program/review/apply、水位一条。
#[test]
fn a_gated_fact_row_is_reviewed_applied_and_marked() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let uid = ingest_turn(&mut state, &data, &dev, &token, "CANARY-ROWS-FACT-1");
    let script = Script::new(vec![Ok(PASS_ONE.to_string())]);
    let mut state = state.with_backend_factory(script.factory());

    let payload = rows_body(json!({
        "anchor": [anchor_row("a-subject", "dev-rows/s-rows", 700)],
        "fact": [fact_row("fact-1", "a-subject", "status", "上线时间另行通知", "decision", &uid)],
        "provenance": [fact_provenance("fact-1", &uid)],
    }));
    let resp = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["rows_reviewed"], 1, "{doc}");
    assert_eq!(doc["rows_absorbed"], 1, "{doc}");
    assert_eq!(doc["rows_rejected"], 0, "{doc}");
    assert_eq!(doc["rows_quarantined"], 0, "{doc}");
    assert_eq!(doc["rows_duplicate"], 0, "首轮没有撞 id：{doc}");
    assert_eq!(doc["rows_direct"], 2, "anchor+provenance 应照旧直写：{doc}");
    assert_eq!(doc["turns_marked"], 1, "{doc}");

    assert_eq!(fact_count(&data, "fact-1"), 1, "过闸的行没有写进 fact");
    let owner = owner_of(&data, "fact", "fact-1").expect("apply 没写归属行");
    assert_eq!(owner.agent, "qoder", "owner 没有来自会话");
    assert_eq!(owner.created_by, "qoder", "created_by 应取会话的 source_agent");

    let items = rows_items(&data);
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].1, "applied");
    assert_eq!(decision_chain(&data, items[0].0), ["program:pass", "review:pass", "apply:pass"]);
    assert_eq!(marked_count(&data, &uid), 1);
    // 窄审确实问了模型一次（不是"闸过即写库"）。
    assert_eq!(script.prompts().len(), 1);
}

/// 窄审 reject：200、0 fact、item rejected、水位仍标记（这一批审完了）。
#[test]
fn a_review_rejected_fact_row_is_counted_and_not_written() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let uid = ingest_turn(&mut state, &data, &dev, &token, "CANARY-ROWS-FACT-2");
    let script = Script::new(vec![Ok(REJECT_ONE.to_string())]);
    let mut state = state.with_backend_factory(script.factory());

    let payload = rows_body(json!({
        "anchor": [anchor_row("a-subject", "dev-rows/s-rows", 700)],
        "fact": [fact_row("fact-2", "a-subject", "status", "上线时间另行通知", "decision", &uid)],
        "provenance": [fact_provenance("fact-2", &uid)],
    }));
    let resp = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["rows_reviewed"], 1, "{doc}");
    assert_eq!(doc["rows_absorbed"], 0, "{doc}");
    assert_eq!(doc["rows_rejected"], 1, "{doc}");

    assert_eq!(fact_count(&data, "fact-2"), 0, "reject 的行不许写库");
    let items = rows_items(&data);
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].1, "rejected");
    assert_eq!(decision_chain(&data, items[0].0), ["program:pass", "review:reject"]);
    assert_eq!(marked_count(&data, &uid), 1, "审完的批仍要打水位");
}

/// 程序闸拒（category 不在闭集）：200、0 fact、item rejected 且 program_code 是闸码、
/// 水位有标记；**一次模型都不问**。
#[test]
fn a_gate_rejected_fact_row_is_queued_with_the_gate_code() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let uid = ingest_turn(&mut state, &data, &dev, &token, "CANARY-ROWS-FACT-3");
    // 空脚本：闸拒的行一旦被送审就会撞 Timeout → 503，这条用例因此在"问了模型"时变红。
    let script = Script::new(Vec::new());
    let mut state = state.with_backend_factory(script.factory());

    let payload = rows_body(json!({
        "anchor": [anchor_row("a-subject", "dev-rows/s-rows", 700)],
        "fact": [fact_row("fact-3", "a-subject", "status", "上线时间另行通知", "nonsense", &uid)],
        "provenance": [fact_provenance("fact-3", &uid)],
    }));
    let resp = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["rows_reviewed"], 1, "{doc}");
    assert_eq!(doc["rows_absorbed"], 0, "{doc}");
    assert_eq!(doc["rows_rejected"], 1, "{doc}");
    assert_eq!(doc["turns_marked"], 1, "{doc}");

    assert_eq!(fact_count(&data, "fact-3"), 0, "闸拒的行不许写库");
    let items = rows_items(&data);
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].1, "rejected");
    assert_eq!(items[0].2.as_deref(), Some("category"), "program_code 不是闸码");
    assert_eq!(decision_chain(&data, items[0].0), ["program:reject"]);
    assert!(script.prompts().is_empty(), "闸拒的行不该问模型");
    assert_eq!(marked_count(&data, &uid), 1);
}

/// 没有后端：fact 行请求 ⇒ 503 `review_unavailable`、0 fact、0 item、水位 0；
/// 同一 body 再带 factory 重试 ⇒ 200、1 行（结构行已直写，重试幂等）。
#[test]
fn a_fact_row_without_a_backend_is_503_and_retries_idempotently() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let uid = ingest_turn(&mut state, &data, &dev, &token, "CANARY-ROWS-FACT-4");

    let payload = rows_body(json!({
        "anchor": [anchor_row("a-subject", "dev-rows/s-rows", 700)],
        "fact": [fact_row("fact-4", "a-subject", "status", "上线时间另行通知", "decision", &uid)],
        "provenance": [fact_provenance("fact-4", &uid)],
    }));
    let first = signed_call(&mut state, &dev, &token, "/v1/rows", payload.clone());
    assert_eq!(first.status, 503, "{}", String::from_utf8_lossy(&first.body));
    let doc: Value = serde_json::from_slice(&first.body).unwrap();
    assert_eq!(doc["error"], "review_unavailable");
    assert_eq!(fact_count(&data, "fact-4"), 0);
    assert!(rows_items(&data).is_empty(), "无后端时不该在 fact 入队之后才 503");
    assert_eq!(marked_count(&data, &uid), 0, "503 之前一个水位都不许打");

    let script = Script::new(vec![Ok(PASS_ONE.to_string())]);
    let mut state = state.with_backend_factory(script.factory());
    let second = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(second.status, 200, "{}", String::from_utf8_lossy(&second.body));
    let doc: Value = serde_json::from_slice(&second.body).unwrap();
    assert_eq!(doc["rows_absorbed"], 1, "{doc}");
    assert_eq!(fact_count(&data, "fact-4"), 1);
    assert_eq!(marked_count(&data, &uid), 1);
}

/// LLM 失败：503、item 留 pending、0 fact、水位 0；换正常脚本重试 ⇒ 200、1 行、item applied。
#[test]
fn an_llm_failure_is_503_and_the_same_body_retries_to_applied() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let uid = ingest_turn(&mut state, &data, &dev, &token, "CANARY-ROWS-FACT-5");
    let broken = Script::new(vec![Err(LlmFailure::BackendError)]);
    let mut state = state.with_backend_factory(broken.factory());

    let payload = rows_body(json!({
        "anchor": [anchor_row("a-subject", "dev-rows/s-rows", 700)],
        "fact": [fact_row("fact-5", "a-subject", "status", "上线时间另行通知", "decision", &uid)],
        "provenance": [fact_provenance("fact-5", &uid)],
    }));
    let first = signed_call(&mut state, &dev, &token, "/v1/rows", payload.clone());
    assert_eq!(first.status, 503, "{}", String::from_utf8_lossy(&first.body));
    let doc: Value = serde_json::from_slice(&first.body).unwrap();
    assert_eq!(doc["error"], "store_unavailable");
    assert_eq!(fact_count(&data, "fact-5"), 0);
    assert_eq!(marked_count(&data, &uid), 0, "LLM 失败那一发不许打水位");
    let items = rows_items(&data);
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].1, "pending_review", "LLM 故障时 item 必须留 pending");
    assert_eq!(decision_chain(&data, items[0].0), ["program:pass"]);

    let good = Script::new(vec![Ok(PASS_ONE.to_string())]);
    let mut state = state.with_backend_factory(good.factory());
    let second = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(second.status, 200, "{}", String::from_utf8_lossy(&second.body));
    let doc: Value = serde_json::from_slice(&second.body).unwrap();
    assert_eq!(doc["rows_absorbed"], 1, "{doc}");
    assert_eq!(fact_count(&data, "fact-5"), 1);
    let items = rows_items(&data);
    assert_eq!(items.len(), 1, "重试不许长出第二条 item：{items:?}");
    assert_eq!(items[0].1, "applied");
    assert_eq!(marked_count(&data, &uid), 1);
}

/// 冲突候选：库内先有同 subject+predicate 的异值 ⇒ 审核 user 里出现 `conflict:` 与旧值。
#[test]
fn an_existing_conflicting_value_reaches_the_review_prompt() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let uid = ingest_turn(&mut state, &data, &dev, &token, "CANARY-ROWS-FACT-6");

    // 先造"既有事实"：锚 + 一行旧值（直接落库——本轮考的是上传面的冲突候选）。
    {
        let root = Root::open(&data).unwrap();
        root.memory()
            .conn()
            .execute(
                "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                                    status, created_at_ms, updated_at_ms, norm_version)
                 VALUES('a-subject','concept','既有主语','unknown','project','dev-rows/s-rows',
                        'active',700,700,1)",
                [],
            )
            .unwrap();
        // 归属行：闸门要求引用锚对行 owner 可见（生产里的既有行由归属回填盖章）。
        // 这里的"既有事实"是直插的库外形状，夹具自己补 owner=qoder。
        root.memory()
            .conn()
            .execute(
                "INSERT INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                            created_by_agent, visibility, retention, scope_kind,
                                            scope_key, source_agent, device_id, created_at_ms)
                 VALUES('anchor','a-subject',NULL,'qoder','agent-old','private_agent','default',
                        'project','dev-rows/s-rows','agent-old','dev-rows',700)",
                [],
            )
            .unwrap();
        root.memory()
            .conn()
            .execute(
                "INSERT INTO fact(id, subject_anchor_id, predicate, value_text, category, mode,
                                  text, evidence_turn_uid, scope_kind, scope_key, source_agent,
                                  device_id, created_at_ms, norm_version)
                 VALUES('fact-old','a-subject','status','旧值','decision','stated','旧的事实',?1,
                        'project','dev-rows/s-rows','agent-old','dev-rows',600,1)",
                [&uid],
            )
            .unwrap();
    }

    let script = Script::new(vec![Ok(PASS_ONE.to_string())]);
    let mut state = state.with_backend_factory(script.factory());
    let payload = rows_body(json!({
        "fact": [fact_row("fact-new", "a-subject", "status", "新值", "decision", &uid)],
        "provenance": [fact_provenance("fact-new", &uid)],
    }));
    let resp = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    assert_eq!(fact_count(&data, "fact-new"), 1);

    let prompts = script.prompts();
    assert_eq!(prompts.len(), 1, "冲突行应当被窄审一次");
    assert!(
        prompts[0].user.contains("conflict:"),
        "审核 user 没有 conflict 行：{}",
        prompts[0].user
    );
    assert!(
        prompts[0].user.contains("旧值"),
        "审核 user 没带旧值：{}",
        prompts[0].user
    );
}

/// 已 applied 的 item 再上行同一批：200、`rows_absorbed` 照计、**不重写**、不再问模型。
#[test]
fn an_applied_fact_row_upload_is_counted_but_not_rewritten() {    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let uid = ingest_turn(&mut state, &data, &dev, &token, "CANARY-ROWS-FACT-7");
    let script = Script::new(vec![Ok(PASS_ONE.to_string())]);
    let mut state = state.with_backend_factory(script.factory());

    let payload = rows_body(json!({
        "anchor": [anchor_row("a-subject", "dev-rows/s-rows", 700)],
        "fact": [fact_row("fact-7", "a-subject", "status", "上线时间另行通知", "decision", &uid)],
        "provenance": [fact_provenance("fact-7", &uid)],
    }));
    let first = signed_call(&mut state, &dev, &token, "/v1/rows", payload.clone());
    assert_eq!(first.status, 200, "{}", String::from_utf8_lossy(&first.body));
    let first_doc: Value = serde_json::from_slice(&first.body).unwrap();
    assert_eq!(first_doc["rows_duplicate"], 0, "首轮没有撞 id：{first_doc}");
    let items = rows_items(&data);
    assert_eq!(items[0].1, "applied");
    let chain = decision_chain(&data, items[0].0);
    assert_eq!(chain, ["program:pass", "review:pass", "apply:pass"]);

    let second = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(second.status, 200, "{}", String::from_utf8_lossy(&second.body));
    let doc: Value = serde_json::from_slice(&second.body).unwrap();
    assert_eq!(doc["rows_reviewed"], 1, "{doc}");
    assert_eq!(doc["rows_absorbed"], 1, "已 applied 的重试要照计行数：{doc}");
    // S3c-fix2：已 applied 的重试**不**算撞 id（它连 apply 都没再跑），这一格必须是 0。
    assert_eq!(doc["rows_duplicate"], 0, "已 applied 的重试不该记 duplicate：{doc}");
    assert_eq!(fact_count(&data, "fact-7"), 1, "重试不许写第二行");
    let items = rows_items(&data);
    assert_eq!(items.len(), 1, "重试不许长出第二条 item：{items:?}");
    assert_eq!(decision_chain(&data, items[0].0), chain, "重试不许补判定");
    assert_eq!(script.prompts().len(), 1, "已 applied 的 item 不该再问模型");
}

/// S3c-fix2：窄审 pass 但 `id` 已在 `fact` 里（不是已 applied 的 item）⇒ 行落在
/// `rows_duplicate`（`ON CONFLICT DO NOTHING` 没写进去），响应必须把这一格亮出来，
/// 否则节点只看到 `rows_absorbed=0` 会以为行被拒。
#[test]
fn a_fact_row_whose_id_already_exists_is_counted_as_duplicate_in_the_response() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let uid = ingest_turn(&mut state, &data, &dev, &token, "CANARY-ROWS-FACT-DUP");
    // 先造"同一 id 的既有行"：锚 + fact 直接落库，审查队列里没有它的 item。
    {
        let root = Root::open(&data).unwrap();
        root.memory()
            .conn()
            .execute(
                "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                                    status, created_at_ms, updated_at_ms, norm_version)
                 VALUES('a-subject','concept','既有主语','unknown','project','dev-rows/s-rows',
                        'active',700,700,1)",
                [],
            )
            .unwrap();
        // 归属行：闸门要求引用锚对行 owner 可见（生产里的既有行由归属回填盖章）。
        // 这里的"既有事实"是直插的库外形状，夹具自己补 owner=qoder。
        root.memory()
            .conn()
            .execute(
                "INSERT INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                            created_by_agent, visibility, retention, scope_kind,
                                            scope_key, source_agent, device_id, created_at_ms)
                 VALUES('anchor','a-subject',NULL,'qoder','agent-old','private_agent','default',
                        'project','dev-rows/s-rows','agent-old','dev-rows',700)",
                [],
            )
            .unwrap();
        root.memory()
            .conn()
            .execute(
                "INSERT INTO fact(id, subject_anchor_id, predicate, value_text, category, mode,
                                  text, evidence_turn_uid, scope_kind, scope_key, source_agent,
                                  device_id, created_at_ms, norm_version)
                 VALUES('fact-dup','a-subject','status','旧值','decision','stated','旧的事实',?1,
                        'project','dev-rows/s-rows','agent-old','dev-rows',600,1)",
                [&uid],
            )
            .unwrap();
    }

    let script = Script::new(vec![Ok(PASS_ONE.to_string())]);
    let mut state = state.with_backend_factory(script.factory());
    let payload = rows_body(json!({
        "fact": [fact_row("fact-dup", "a-subject", "status", "新值", "decision", &uid)],
        "provenance": [fact_provenance("fact-dup", &uid)],
    }));
    let resp = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["rows_reviewed"], 1, "{doc}");
    assert_eq!(doc["rows_absorbed"], 0, "撞 id 的行没有写进去：{doc}");
    assert_eq!(doc["rows_duplicate"], 1, "撞 id 的行必须落在 duplicate：{doc}");
    assert_eq!(doc["rows_rejected"], 0, "{doc}");
    assert_eq!(doc["rows_quarantined"], 0, "{doc}");
    assert_self_consistent(&doc);
    assert_eq!(fact_count(&data, "fact-dup"), 1, "重放同一 id 不许长出第二行");
    let items = rows_items(&data);
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].1, "applied");
    assert_eq!(decision_chain(&data, items[0].0), ["program:pass", "review:pass", "apply:pass"]);
}

// ─────────────────── S3c-fix：P0 两片回归、计数与"未推进" ───────────────────

/// 逐行 `pass` 的 `rows_review` 回复（ref 从 `r1` 到 `r{count}`）。
fn pass_all(count: usize) -> String {
    let verdicts: Vec<Value> = (1..=count)
        .map(|index| json!({"ref": format!("r{index}"), "verdict": "pass", "code": null}))
        .collect();
    json!({"schema_version": 0, "pass": "rows_review", "verdicts": verdicts}).to_string()
}

/// 逐行判决的 `rows_review` 回复：`(ref, verdict, code)`。
fn verdicts_reply(rows: &[(&str, &str, Option<&str>)]) -> String {
    let verdicts: Vec<Value> = rows
        .iter()
        .map(|(reference, verdict, code)| {
            json!({"ref": reference, "verdict": verdict, "code": code})
        })
        .collect();
    json!({"schema_version": 0, "pass": "rows_review", "verdicts": verdicts}).to_string()
}

fn fact_total(data: &Path) -> i64 {
    let root = Root::open(data).unwrap();
    root.memory()
        .conn()
        .query_row("SELECT count(*) FROM fact", [], |r| r.get(0))
        .unwrap()
}

/// `absorbed+rejected+quarantined+duplicate == reviewed`（派发档 §5 的自洽断言；
/// S3c-fix2 起 `rows_duplicate` 进了响应，撞 id 的行也必须落在某一格）。
fn assert_self_consistent(doc: &Value) {
    let reviewed = doc["rows_reviewed"].as_u64().expect("rows_reviewed");
    let counted = doc["rows_absorbed"].as_u64().expect("rows_absorbed")
        + doc["rows_rejected"].as_u64().expect("rows_rejected")
        + doc["rows_quarantined"].as_u64().expect("rows_quarantined")
        + doc["rows_duplicate"].as_u64().expect("rows_duplicate");
    assert_eq!(
        counted, reviewed,
        "absorbed+rejected+quarantined+duplicate 必须等于 reviewed：{doc}"
    );
}

/// **P0 回归钉**：同一请求 12 条同轮 fact ⇒ 组内 `chunks(10)` 切两片 ⇒ 两片行集合不同
/// ⇒ `rows_batch_key` 不同 ⇒ 谁也不顶替谁。200、12 行全入库、`rows_absorbed=12`、
/// 两条 item 都 applied。
///
/// 修复前：两片的 `batch_key` 只由轮集合构成（同一轮），入队第二片时第一片被顶成
/// `stale` ⇒ 前 10 行不审不写、`rows_absorbed=2`、HTTP 仍 200 + 打水（静默永久丢行）。
#[test]
fn twelve_same_turn_facts_split_into_two_slices_and_all_twelve_are_written() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let uid = ingest_turn(&mut state, &data, &dev, &token, "CANARY-ROWS-FACT-12");
    // 两片各问一次模型：片 1 = 10 行、片 2 = 2 行。
    let script = Script::new(vec![Ok(pass_all(10)), Ok(pass_all(2))]);
    let mut state = state.with_backend_factory(script.factory());

    let facts: Vec<Value> = (1..=12)
        .map(|index| {
            fact_row(
                &format!("fact-{index:02}"),
                "a-subject",
                "status",
                "上线时间另行通知",
                "decision",
                &uid,
            )
        })
        .collect();
    let provenances: Vec<Value> = (1..=12)
        .map(|index| fact_provenance(&format!("fact-{index:02}"), &uid))
        .collect();
    let payload = rows_body(json!({
        "anchor": [anchor_row("a-subject", "dev-rows/s-rows", 700)],
        "fact": facts,
        "provenance": provenances,
    }));
    let resp = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["rows_reviewed"], 12, "{doc}");
    assert_eq!(doc["rows_absorbed"], 12, "前一片的行被顶掉了：{doc}");
    assert_eq!(doc["rows_rejected"], 0, "{doc}");
    assert_eq!(doc["rows_quarantined"], 0, "{doc}");
    assert_self_consistent(&doc);
    assert_eq!(doc["turns_marked"], 1, "{doc}");

    assert_eq!(fact_total(&data), 12, "12 行必须全入库（P0：前 ≤10 行静默丢失）");
    let items = rows_items(&data);
    assert_eq!(items.len(), 2, "≤10 行/片 ⇒ 恰好两条 item：{items:?}");
    assert!(
        items.iter().all(|(_, state, _)| state == "applied"),
        "两片都必须 applied（没有 stale）：{items:?}"
    );
    assert_eq!(script.prompts().len(), 2, "两片各问一次模型");
    assert_eq!(marked_count(&data, &uid), 1);
}

/// 混合 item：一 pass 一 reject ⇒ `rows_absorbed=1`、`rows_rejected=1`（复审 P1-d：
/// 被拒行原先从读数里蒸发），只写 pass 行，200。
#[test]
fn a_mixed_item_counts_the_rejected_row_too() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let uid = ingest_turn(&mut state, &data, &dev, &token, "CANARY-ROWS-FACT-MIX");
    let script = Script::new(vec![Ok(verdicts_reply(&[
        ("r1", "pass", None),
        ("r2", "reject", Some("unsupported")),
    ]))]);
    let mut state = state.with_backend_factory(script.factory());

    let payload = rows_body(json!({
        "anchor": [anchor_row("a-subject", "dev-rows/s-rows", 700)],
        "fact": [
            fact_row("fact-mix-1", "a-subject", "status", "上线时间另行通知", "decision", &uid),
            fact_row("fact-mix-2", "a-subject", "status", "上线时间另行通知", "decision", &uid),
        ],
        "provenance": [
            fact_provenance("fact-mix-1", &uid),
            fact_provenance("fact-mix-2", &uid),
        ],
    }));
    let resp = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["rows_reviewed"], 2, "{doc}");
    assert_eq!(doc["rows_absorbed"], 1, "{doc}");
    assert_eq!(doc["rows_rejected"], 1, "被拒的那一行必须计数：{doc}");
    assert_eq!(doc["rows_quarantined"], 0, "{doc}");
    assert_self_consistent(&doc);
    assert_eq!(fact_total(&data), 1, "只写 pass 行");

    let items = rows_items(&data);
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].1, "applied", "混合 item 仍按 applied 推进");
}

/// uncertain 一条 ⇒ item quarantined、`rows_quarantined` 计数非 0、200（审完了、打水）。
#[test]
fn an_uncertain_row_is_counted_as_quarantined() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let uid = ingest_turn(&mut state, &data, &dev, &token, "CANARY-ROWS-FACT-UNC");
    // 五码闭集里挑一个：uncertain 允许带码（S3c-fix 的 prompt 也要求这样填）。
    let script = Script::new(vec![Ok(verdicts_reply(&[("r1", "uncertain", Some("unsupported"))]))]);
    let mut state = state.with_backend_factory(script.factory());

    let payload = rows_body(json!({
        "anchor": [anchor_row("a-subject", "dev-rows/s-rows", 700)],
        "fact": [fact_row("fact-unc", "a-subject", "status", "上线时间另行通知", "decision", &uid)],
        "provenance": [fact_provenance("fact-unc", &uid)],
    }));
    let resp = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["rows_reviewed"], 1, "{doc}");
    assert_eq!(doc["rows_absorbed"], 0, "{doc}");
    assert_eq!(doc["rows_rejected"], 0, "{doc}");
    assert_eq!(doc["rows_quarantined"], 1, "{doc}");
    assert_self_consistent(&doc);
    assert_eq!(fact_total(&data), 0, "隔离的行不许写库");

    let items = rows_items(&data);
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].1, "quarantined");
    assert_eq!(marked_count(&data, &uid), 1, "隔离也算审完（节点不必重发）");
}

/// `Stale` 分支（人为制造：把 item 直接改成 `stale`）：本请求的行**没有结论**
/// ⇒ 503 `store_unavailable`、水位 0（复审 P1-2：200 + 打水会让节点永不重发）。
#[test]
fn a_stale_item_is_503_and_marks_no_watermark() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let uid = ingest_turn(&mut state, &data, &dev, &token, "CANARY-ROWS-FACT-STALE");
    // 第一次：LLM 故障 ⇒ 503、item 留 pending、水位 0。
    let broken = Script::new(vec![Err(LlmFailure::BackendError)]);
    let mut state = state.with_backend_factory(broken.factory());
    let payload = rows_body(json!({
        "anchor": [anchor_row("a-subject", "dev-rows/s-rows", 700)],
        "fact": [fact_row("fact-stale", "a-subject", "status", "上线时间另行通知", "decision", &uid)],
        "provenance": [fact_provenance("fact-stale", &uid)],
    }));
    let first = signed_call(&mut state, &dev, &token, "/v1/rows", payload.clone());
    assert_eq!(first.status, 503, "{}", String::from_utf8_lossy(&first.body));
    assert_eq!(marked_count(&data, &uid), 0);

    // 人为制造顶替终态：同一条 item 被另一片换掉（`stale`）。
    {
        let root = Root::open(&data).unwrap();
        let changed = root
            .memory()
            .conn()
            .execute("UPDATE review_item SET state='stale' WHERE pass_id='rows'", [])
            .unwrap();
        assert_eq!(changed, 1, "夹具必须恰好改到一条 rows item");
    }

    let good = Script::new(vec![Ok(PASS_ONE.to_string())]);
    let mut state = state.with_backend_factory(good.factory());
    let second = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(second.status, 503, "{}", String::from_utf8_lossy(&second.body));
    let doc: Value = serde_json::from_slice(&second.body).unwrap();
    assert_eq!(doc["error"], "store_unavailable", "{doc}");
    assert_eq!(marked_count(&data, &uid), 0, "没有结论的分支一个水位都不许打");
    assert_eq!(fact_total(&data), 0, "stale 的 item 一行都没写");
    assert!(good.prompts().is_empty(), "stale 的 item 不该再问模型");
}

/// 冲突候选只看**同 scope**（复审 P1-c）：别 scope 的旧值不进 prompt；
/// 同 scope 的旧值仍进（后者由 `an_existing_conflicting_value_reaches_the_review_prompt` 守）。
#[test]
fn an_existing_value_in_another_scope_stays_out_of_the_review_prompt() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let uid = ingest_turn(&mut state, &data, &dev, &token, "CANARY-ROWS-FACT-SCOPE");
    {
        let root = Root::open(&data).unwrap();
        root.memory()
            .conn()
            .execute(
                "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                                    status, created_at_ms, updated_at_ms, norm_version)
                 VALUES('a-subject','concept','既有主语','unknown','project','other-project',
                        'active',700,700,1)",
                [],
            )
            .unwrap();
        // 归属行：引用锚必须对行 owner 可见（owner=qoder，与 fact 行同一 owner）。
        root.memory()
            .conn()
            .execute(
                "INSERT INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                            created_by_agent, visibility, retention, scope_kind,
                                            scope_key, source_agent, device_id, created_at_ms)
                 VALUES('anchor','a-subject',NULL,'qoder','agent-old','private_agent','default',
                        'project','other-project','agent-old','dev-rows',700)",
                [],
            )
            .unwrap();
        // 同一 subject + predicate + 异值，但落在另一个 project scope。
        root.memory()
            .conn()
            .execute(
                "INSERT INTO fact(id, subject_anchor_id, predicate, value_text, category, mode,
                                  text, evidence_turn_uid, scope_kind, scope_key, source_agent,
                                  device_id, created_at_ms, norm_version)
                 VALUES('fact-elsewhere','a-subject','status','别处的旧值','decision','stated',
                        '别处的事实',?1,'project','other-project','agent-old','dev-rows',600,1)",
                [&uid],
            )
            .unwrap();
    }

    let script = Script::new(vec![Ok(PASS_ONE.to_string())]);
    let mut state = state.with_backend_factory(script.factory());
    let payload = rows_body(json!({
        "fact": [fact_row("fact-scoped", "a-subject", "status", "新值", "decision", &uid)],
        "provenance": [fact_provenance("fact-scoped", &uid)],
    }));
    let resp = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    assert_eq!(fact_count(&data, "fact-scoped"), 1);

    let prompts = script.prompts();
    assert_eq!(prompts.len(), 1, "这一行仍要窄审");
    assert!(
        !prompts[0].user.contains("conflict:"),
        "别 scope 的旧值进了 prompt：{}",
        prompts[0].user
    );
    assert!(
        !prompts[0].user.contains("别处的旧值"),
        "别 scope 的旧值正文外泄了：{}",
        prompts[0].user
    );
}

/// 程序阶段已被**另一写者**记过：handler 不重记、不折 503（复审 P1-3）。
///
/// 单线程里造不出"两次 `record_program` 真正交错"（`record_program` 与 `program_decided`
/// 之间的窗口没有锁），所以这条用例把它拆成两半：
/// ① 程序判定已经落库的同一 body 再走 handler ⇒ 只跑引擎、200（不是 503）；
/// ② 结构上那条"另一写者已记"的错误面确实是队列竞态（`is_review_conflict()`），
///    handler 因此忽略它而不是把它当存储故障——这里用"终态上再表态"造一个真竞态来钉住分类。
#[test]
fn a_program_pass_recorded_by_another_writer_does_not_become_a_503() {
    let (_dir, data, mut state) = fixture();
    let (dev, token) = pair_one(&mut state, &data);
    let uid = ingest_turn(&mut state, &data, &dev, &token, "CANARY-ROWS-FACT-RACE");
    let broken = Script::new(vec![Err(LlmFailure::BackendError)]);
    let mut state = state.with_backend_factory(broken.factory());
    let payload = rows_body(json!({
        "anchor": [anchor_row("a-subject", "dev-rows/s-rows", 700)],
        "fact": [fact_row("fact-race", "a-subject", "status", "上线时间另行通知", "decision", &uid)],
        "provenance": [fact_provenance("fact-race", &uid)],
    }));
    let first = signed_call(&mut state, &dev, &token, "/v1/rows", payload.clone());
    assert_eq!(first.status, 503, "{}", String::from_utf8_lossy(&first.body));
    let item_id = rows_items(&data)[0].0;
    assert_eq!(decision_chain(&data, item_id), ["program:pass"], "程序阶段已表态");

    // ① 程序阶段已表态 ⇒ 再走 handler 只跑引擎，不重记、不 503。
    let good = Script::new(vec![Ok(PASS_ONE.to_string())]);
    let mut state = state.with_backend_factory(good.factory());
    let second = signed_call(&mut state, &dev, &token, "/v1/rows", payload);
    assert_eq!(
        second.status,
        200,
        "程序阶段已有判定不该折成 503：{}",
        String::from_utf8_lossy(&second.body)
    );
    let doc: Value = serde_json::from_slice(&second.body).unwrap();
    assert_eq!(doc["rows_absorbed"], 1, "{doc}");
    assert_eq!(fact_count(&data, "fact-race"), 1);
    let items = rows_items(&data);
    assert_eq!(items.len(), 1, "重试不许长出第二条 item：{items:?}");
    assert_eq!(items[0].1, "applied");
    assert_eq!(
        decision_chain(&data, items[0].0),
        ["program:pass", "review:pass", "apply:pass"],
        "程序判定只许一条"
    );

    // ② 另一次表态（终态上再记 program/reject）是队列竞态，不是存储故障。
    let root = Root::open(&data).unwrap();
    let error = rc_distill::review::record_program(
        root.memory(),
        item_id,
        rc_memory::review::ReviewVerdict::Reject,
        "program_reject",
        &"0".repeat(64),
        NOW + 1,
    )
    .unwrap_err();
    assert!(error.is_review_conflict(), "队列竞态必须可判别：{error}");
}
