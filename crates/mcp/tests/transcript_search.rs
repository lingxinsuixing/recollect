//! W38a 刀 B 的工具面钉（设计 2026-11-08-w38a-transcript-search §五 + 工单 5）：
//! ① 三工具经 MCP `tools/call` 可调通且信封正确；② audit_log 留痕（search 每次调用
//! 一行、session/sessions 不写）；④ 闸拒与空读数在工具面可分（靠信封 `matched`）。
//! （③ replicate/restore 的 rebuild 接线钉在 hub 侧：`hub/tests/transcript_shadow_rebuild.rs`。）
//!
//! 夹具纪律：agent / principal / 会话名 / 正文全部自造合成词，不引用任何真实语料。

mod common;

use common::{call, call_error_text, call_json, call_normal_state_text, init, tool_text, Session};
use rc_contract::{compute_body_fp, NormalizedTurn, SessionId, TurnTier};
use rc_ledger::insert_batch;
use rc_store::Root;
use serde_json::{json, Value};
use std::path::Path;

/// 命中词（自造合成词，三个会话共享，让 principal 臂成为唯一区分）。
const MARKER: &str = "quokka";
/// 只在异 principal 会话里的命中词（闸拒读数的对照）。
const MARKER_OTHER: &str = "wallaby";
/// 哪里都没有的词（空读数的零对照）。
const ABSENT: &str = "ptarmigan";

/// reader A 的启动参数：agent 归自己、principal 声明 p-one（W37 已铺的启动项）。
const ARGS_A: [&str; 6] = [
    "--device-id", "dev-synthetic", "--agent-id", "agent-alpha", "--principal-id", "p-one",
];

fn session(key: &str, agent: &str, principal: &str) -> SessionId {
    SessionId {
        session_key: key.to_string(),
        device_id: "dev-synthetic".to_string(),
        source_agent: agent.to_string(),
        machine: None,
        project_hint: None,
        last_saved_at_ms: None,
        owner_principal_id: Some(principal.to_string()),
        principal_name: None,
        principal_source: None,
    }
}

fn turn(uid: &str, body: &str, created_ms: i64) -> NormalizedTurn {
    NormalizedTurn {
        tier: TurnTier::ContentOnly,
        turn_uid: uid.to_string(),
        body: body.to_string(),
        body_fp: compute_body_fp(body),
        role: "human".to_string(),
        created_at_ms: created_ms,
    }
}

/// 三会话五轮：甲（alpha/p-one，两轮命中）、丙（beta/p-one，同 principal 一轮命中）、
/// 乙（beta/p-two，异 principal，命中 + 独有的 wallaby 一轮）。
fn seed(data: &Path) {
    let root = Root::open(data).unwrap();
    insert_batch(
        root.transcript(),
        &session("ses-alpha", "agent-alpha", "p-one"),
        &[
            turn("t-alpha-1", "alpha plans to keep the quokka marker in view", 1000),
            turn("t-alpha-2", "alpha follows up on the quokka marker again", 1001),
        ],
        1001,
    )
    .unwrap();
    insert_batch(
        root.transcript(),
        &session("ses-gamma", "agent-beta", "p-one"),
        &[turn("t-gamma-1", "beta shares the quokka marker with the same principal", 1002)],
        1002,
    )
    .unwrap();
    insert_batch(
        root.transcript(),
        &session("ses-beta", "agent-beta", "p-two"),
        &[
            turn("t-beta-1", "beta keeps the quokka marker elsewhere", 1003),
            turn("t-beta-2", "beta keeps the wallaby marker private", 1004),
        ],
        1004,
    )
    .unwrap();
}

/// ledger.db 里 action='search' 的审计行（actor, subject, detail），按写入序。
/// 子进程还活着时并读打开：WAL 下并发读者合法，判据只 SELECT 不写。
fn search_audit_rows(data: &Path) -> Vec<(String, String, String)> {
    let conn = rusqlite::Connection::open(data.join("ledger.db")).unwrap();
    let mut st = conn
        .prepare("SELECT actor, subject, detail FROM audit_log WHERE action='search' ORDER BY id")
        .unwrap();
    let rows = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

fn list_tools(s: &mut Session, id: i64) -> Value {
    let r = s.ask(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/list"}));
    assert!(r["error"].is_null(), "tools/list 不许走 JSON-RPC error：{r}");
    r["result"]["tools"].clone()
}

// ── 工单 2：四处同步的对撞面（tools/list 的定义表随 capabilities 走） ──────────

#[test]
fn transcript_tools_are_listed_with_schemas() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let mut s = Session::start_with(dir.path(), &ARGS_A);
    s.ask(&init(1, "agent-alpha"));
    let tools = list_tools(&mut s, 2);
    let find = |name: &str| -> Value {
        tools
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == json!(name))
            .unwrap_or_else(|| panic!("tools/list 缺 {name}：{tools}"))
            .clone()
    };
    let def = find("recollect_search");
    assert_eq!(def["inputSchema"]["required"], json!(["query"]));
    assert_eq!(def["inputSchema"]["properties"]["limit"]["maximum"], json!(20));
    let def = find("recollect_session");
    assert_eq!(def["inputSchema"]["required"], json!(["session_key"]));
    assert_eq!(def["inputSchema"]["properties"]["limit"]["maximum"], json!(200));
    let def = find("recollect_sessions");
    assert_eq!(def["inputSchema"]["properties"]["limit"]["maximum"], json!(200));
    // W39 刀 B：按轮取原文的 schema 与别名（清单/描述/schema 三处同源，缺一即启动失败）。
    let def = find("recollect_turn");
    assert_eq!(def["inputSchema"]["required"], json!(["turn_uid"]));
    assert_eq!(def["inputSchema"]["properties"]["before"]["maximum"], json!(10));
    assert_eq!(def["inputSchema"]["properties"]["after"]["maximum"], json!(10));
    let def = find("recollect_search");
    assert!(
        def["description"].as_str().unwrap().contains("independent of recall"),
        "描述必须写明独立于 recall：{}",
        def["description"]
    );
}

// ── 工单 1 / 5①：信封形状与闸 ─────────────────────────────────────────────

#[test]
fn search_returns_envelope_and_the_gate_holds_on_the_tool_face() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let mut s = Session::start_with(dir.path(), &ARGS_A);
    s.ask(&init(1, "agent-alpha"));

    let env: Value = call_json(&mut s, 2, "recollect_search", json!({ "query": MARKER }));
    let hits = env["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 3, "自己两轮 + 同 principal 一轮：{env}");
    let keys: Vec<&str> = hits.iter().map(|h| h["session_key"].as_str().unwrap()).collect();
    assert!(keys.contains(&"ses-alpha") && keys.contains(&"ses-gamma"), "{keys:?}");
    assert!(
        !keys.contains(&"ses-beta"),
        "红线：异 principal 的会话从工具面漏进来了：{keys:?}"
    );
    // 信封四格（细案 §二.4/§二.6）：hits 由库层排（created_at_ms 降序）。
    // `matched` 是**闸前**命中数——乙会话那轮 quokka 命中了但被闸关在外面，所以是 4 不是 3。
    assert_eq!(env["matched"], json!(4));
    assert_eq!(env["truncated"], json!(false));
    assert_eq!(env["coverage"], json!({"indexed": 5, "total": 5}));
    for h in hits {
        for k in ["turn_uid", "session_key", "role", "created_at_ms", "snippet"] {
            assert!(h.get(k).is_some(), "命中轮缺 {k} 键：{h}");
        }
        assert!(
            h["snippet"].as_str().unwrap().contains(MARKER),
            "snippet 必须含命中词：{h}"
        );
        // W39 §二.4：归属随原文走——每条命中带所在会话的 owner 两列。
        let agent = if h["session_key"] == json!("ses-alpha") { "agent-alpha" } else { "agent-beta" };
        assert_eq!(h["owner_agent_id"], json!(agent), "命中的归属 agent：{h}");
        assert_eq!(h["owner_principal_id"], json!("p-one"), "命中的归属 principal：{h}");
    }

    // 工单 5④：闸拒与空读数都是 isError=false，靠 matched 分型。
    let denied: Value = call_json(&mut s, 3, "recollect_search", json!({ "query": MARKER_OTHER }));
    assert!(
        denied["hits"].as_array().unwrap().is_empty(),
        "闸外的命中不许回来：{denied}"
    );
    assert!(
        denied["matched"].as_i64().unwrap() > 0,
        "闸拒必须可分型（matched 是闸前命中数）：{denied}"
    );
    let empty: Value = call_json(&mut s, 4, "recollect_search", json!({ "query": ABSENT }));
    assert!(empty["hits"].as_array().unwrap().is_empty());
    assert_eq!(empty["matched"], json!(0), "空读数的 matched 必须是 0：{empty}");
    assert_ne!(denied["matched"], empty["matched"], "两个空必须可分");
}

#[test]
fn search_validates_args_and_honors_limit() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let mut s = Session::start_with(dir.path(), &ARGS_A);
    s.ask(&init(1, "agent-alpha"));

    // 缺 query 与越界 limit 都是响亮的 isError（错误文本是 kind:code hint 三段）。
    let t = call_error_text(&mut s, 2, "recollect_search", json!({}));
    assert!(t.contains("missing_argument"), "{t}");
    let t = call_error_text(&mut s, 3, "recollect_search", json!({ "query": MARKER, "limit": 21 }));
    assert!(t.contains("bad_limit"), "{t}");

    // limit=1 只回一行，且如实报截断（G7）。
    let env: Value = call_json(&mut s, 4, "recollect_search", json!({ "query": MARKER, "limit": 1 }));
    assert_eq!(env["hits"].as_array().unwrap().len(), 1, "{env}");
    assert_eq!(env["truncated"], json!(true));
}

// ── 工单 1：回放与清单 ─────────────────────────────────────────────────────

#[test]
fn session_replays_visible_turns_and_hides_invisible_ones() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let mut s = Session::start_with(dir.path(), &ARGS_A);
    s.ask(&init(1, "agent-alpha"));

    let env: Value = call_json(&mut s, 2, "recollect_session", json!({ "session_key": "ses-alpha" }));
    // W39 刀 E：回放信封带 device 与归属两列（"知道跟谁说话"的落点）。
    assert_eq!(env["session_key"], json!("ses-alpha"));
    assert_eq!(env["device_id"], json!("dev-synthetic"), "{env}");
    assert_eq!(env["owner_agent_id"], json!("agent-alpha"), "{env}");
    assert_eq!(env["owner_principal_id"], json!("p-one"), "{env}");
    let turns = env["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2, "{env}");
    assert_eq!(turns[0]["turn_uid"], json!("t-alpha-1"), "回放按时间升序：{env}");
    assert_eq!(turns[0]["role"], json!("human"));
    assert_eq!(
        turns[0]["body"],
        json!("alpha plans to keep the quokka marker in view"),
        "回放要带正文全文：{env}"
    );

    // 游标：严格排在该轮之后。
    let env: Value = call_json(
        &mut s,
        3,
        "recollect_session",
        json!({ "session_key": "ses-alpha", "after_uid": "t-alpha-1" }),
    );
    let uids: Vec<&str> =
        env["turns"].as_array().unwrap().iter().map(|t| t["turn_uid"].as_str().unwrap()).collect();
    assert_eq!(uids, vec!["t-alpha-2"], "游标之后应只剩后一轮：{env}");

    // limit 截断。
    let env: Value = call_json(
        &mut s,
        4,
        "recollect_session",
        json!({ "session_key": "ses-alpha", "limit": 1 }),
    );
    assert_eq!(env["turns"].as_array().unwrap().len(), 1, "{env}");

    // 不可见 ⇒ no_rows（isError=false，与"查无"同形，不泄露存在性）。
    let t = call_normal_state_text(&mut s, 5, "recollect_session", json!({ "session_key": "ses-beta" }));
    assert!(t.contains("no_rows"), "{t}");
}

#[test]
fn sessions_lists_only_visible_sessions_with_counts_and_range() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let mut s = Session::start_with(dir.path(), &ARGS_A);
    s.ask(&init(1, "agent-alpha"));

    let env: Value = call_json(&mut s, 2, "recollect_sessions", json!({}));
    let list = env["sessions"].as_array().unwrap();
    let keys: Vec<&str> = list.iter().map(|x| x["session_key"].as_str().unwrap()).collect();
    assert!(keys.contains(&"ses-alpha") && keys.contains(&"ses-gamma"), "{keys:?}");
    assert!(!keys.contains(&"ses-beta"), "闸外的会话不许出现在清单里：{keys:?}");
    let a = list.iter().find(|x| x["session_key"] == json!("ses-alpha")).unwrap();
    assert_eq!(a["source_agent"], json!("agent-alpha"));
    assert_eq!(a["turns"], json!(2), "轮数读数：{a}");
    assert_eq!(a["first_ms"], json!(1000), "时间范围读数：{a}");
    assert_eq!(a["last_ms"], json!(1001), "时间范围读数：{a}");
    assert!(a["project_hint"].is_null(), "没写 project_hint 就是 null：{a}");
    assert_eq!(env["truncated"], json!(false));

    // 可选 limit 收短清单并如实报截断。
    let env: Value = call_json(&mut s, 3, "recollect_sessions", json!({ "limit": 1 }));
    assert_eq!(env["sessions"].as_array().unwrap().len(), 1, "{env}");
    assert_eq!(env["truncated"], json!(true));
}

// ── 工单 3 / 5②：audit_log 留痕（只钉检索这一口） ──────────────────────────

#[test]
fn audit_log_pins_search_and_only_search() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let mut s = Session::start_with(dir.path(), &ARGS_A);
    s.ask(&init(1, "agent-alpha"));
    assert!(search_audit_rows(dir.path()).is_empty(), "夹具不许自带审计行");

    // 一次 search 一行：actor=agent、subject=principal、detail 带 q/hits/truncated。
    let _: Value = call_json(&mut s, 2, "recollect_search", json!({ "query": MARKER }));
    let rows = search_audit_rows(dir.path());
    assert_eq!(rows.len(), 1, "一次调用恰好一行：{rows:?}");
    assert_eq!(rows[0].0, "agent-alpha", "actor=agent_id：{rows:?}");
    assert_eq!(rows[0].1, "p-one", "subject=viewer principal：{rows:?}");
    let detail: Value = serde_json::from_str(&rows[0].2).unwrap();
    assert_eq!(detail["q"], json!(MARKER), "{detail}");
    assert_eq!(detail["hits"], json!(3), "{detail}");
    assert_eq!(detail["truncated"], json!(false), "{detail}");

    // 其余两工具不写（读面留痕只钉检索这一口）。
    let _: Value = call_json(&mut s, 3, "recollect_session", json!({ "session_key": "ses-alpha" }));
    let _: Value = call_json(&mut s, 4, "recollect_sessions", json!({}));
    assert_eq!(search_audit_rows(dir.path()).len(), 1, "session/sessions 不许留审计行");

    // 闸拒的那一次也是一次调用：照写一行，hits=0。
    let _: Value = call_json(&mut s, 5, "recollect_search", json!({ "query": MARKER_OTHER }));
    let rows = search_audit_rows(dir.path());
    assert_eq!(rows.len(), 2, "{rows:?}");
    let detail: Value = serde_json::from_str(&rows[1].2).unwrap();
    assert_eq!(detail["hits"], json!(0), "{detail}");

    // q 截 120 字符（细案 §二.5）。
    let _: Value = call_json(&mut s, 6, "recollect_search", json!({ "query": "z".repeat(300) }));
    let rows = search_audit_rows(dir.path());
    let detail: Value = serde_json::from_str(&rows[2].2).unwrap();
    assert_eq!(detail["q"].as_str().unwrap().chars().count(), 120, "{detail}");
}

// ── 协议纪律的顺带对账：工具面文本必须还是单段 text（与既有五工具同形状） ────

#[test]
fn search_readout_is_a_single_text_block() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let mut s = Session::start_with(dir.path(), &ARGS_A);
    s.ask(&init(1, "agent-alpha"));
    // 本进程首次工具调用会附一段 bootstrap 状态块（P1 既有行为），先烧掉它。
    let _: Value = call_json(&mut s, 2, "recollect_sessions", json!({}));
    let r = s.ask(&call(3, "recollect_search", json!({ "query": MARKER })));
    assert_eq!(r["result"]["content"].as_array().unwrap().len(), 1, "{r}");
    let _: Value = serde_json::from_str(&tool_text(&r)).expect("回执文本必须是 JSON 信封");
}

// ── W39 刀 B（细案 §3.4）：按轮取原文 ──────────────────────────────────────
//
// 形状门要 64 位小写 hex，所以这一组夹具的 turn_uid 用自造序号补零成 64 位；
// 会话甲（qoder / p1，五轮）与启动参数同归属，会话乙（dsh / p2，三轮）两条判据都吃不到。

/// 启动参数：agent 归自己（qoder）、principal 声明 p1（与甲会话同主人）。
const ARGS_TURN: [&str; 6] = [
    "--device-id", "dev-synthetic", "--agent-id", "qoder", "--principal-id", "p1",
];

/// 64 位小写 hex 的 turn_uid（自造序号补零——工具面形状门要的正是这个形状）。
fn hex_uid(n: u64) -> String {
    format!("{n:064x}")
}

/// 按轮取窗口的夹具。**与 [`seed`] 分开**：`seed` 的轮数与命中数被检索/回放那几条
/// 用例逐格钉着，往它里面加轮会让那些判据翻红。
fn seed_turn_fixture(data: &Path) -> (Vec<String>, Vec<String>) {
    let a: Vec<String> = (1u64..=5).map(hex_uid).collect();
    let b: Vec<String> = (0xb1u64..=0xb3).map(hex_uid).collect();
    let root = Root::open(data).unwrap();
    insert_batch(
        root.transcript(),
        &session("ses-orca", "qoder", "p1"),
        &[
            turn(&a[0], "orca drafts the granite ribbon", 2000),
            turn(&a[1], "orca answers with a slate quill", 2001),
            turn(&a[2], "orca pins the copper lantern", 2002),
            turn(&a[3], "orca shelves the violet ledger", 2003),
            turn(&a[4], "orca closes the granite ribbon", 2004),
        ],
        2004,
    )
    .unwrap();
    insert_batch(
        root.transcript(),
        &session("ses-lynx", "dsh", "p2"),
        &[
            turn(&b[0], "lynx hides the amber shuttle", 2010),
            turn(&b[1], "lynx counts the silver pebbles", 2011),
            turn(&b[2], "lynx seals the amber shuttle", 2012),
        ],
        2012,
    )
    .unwrap();
    (a, b)
}

#[test]
fn turn_returns_the_target_between_its_neighbours_with_ownership() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = seed_turn_fixture(dir.path());
    let mut s = Session::start_with(dir.path(), &ARGS_TURN);
    s.ask(&init(1, "qoder"));

    // 第 3 轮 + 前后各 1 轮：升序、目标居中，信封带归属两列。
    let env: Value = call_json(
        &mut s,
        2,
        "recollect_turn",
        json!({ "turn_uid": a[2], "before": 1, "after": 1 }),
    );
    assert_eq!(env["turn_uid"], json!(a[2]));
    assert_eq!(env["session_key"], json!("ses-orca"));
    assert_eq!(env["device_id"], json!("dev-synthetic"));
    assert_eq!(env["owner_agent_id"], json!("qoder"));
    assert_eq!(env["owner_principal_id"], json!("p1"), "归属必须随原文走：{env}");
    let turns = env["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 3, "前后各 1 轮 + 目标 = 3：{env}");
    let uids: Vec<&str> = turns.iter().map(|t| t["turn_uid"].as_str().unwrap()).collect();
    assert_eq!(uids, vec![a[1].as_str(), a[2].as_str(), a[3].as_str()], "升序且目标居中：{env}");
    assert_eq!(turns[1]["role"], json!("human"));
    assert_eq!(turns[1]["body"], json!("orca pins the copper lantern"), "正文逐字：{env}");
    assert!(turns[1].get("created_at_ms").is_some(), "每轮带时间：{env}");
    // 两侧各还剩一轮没取回来 ⇒ 截断如实为 true（不是"到头了"）。
    assert_eq!(env["truncated_before"], json!(true), "{env}");
    assert_eq!(env["truncated_after"], json!(true), "{env}");
}

#[test]
fn turn_with_zero_window_returns_only_the_target() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = seed_turn_fixture(dir.path());
    let mut s = Session::start_with(dir.path(), &ARGS_TURN);
    s.ask(&init(1, "qoder"));

    let env: Value = call_json(
        &mut s,
        2,
        "recollect_turn",
        json!({ "turn_uid": a[2], "before": 0, "after": 0 }),
    );
    let turns = env["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 1, "before=0 after=0 只回目标轮：{env}");
    assert_eq!(turns[0]["turn_uid"], json!(a[2]), "{env}");

    // 缺省与 0 同义（不为省参数另开一条缺省路径）。
    let env: Value = call_json(&mut s, 3, "recollect_turn", json!({ "turn_uid": a[2] }));
    let turns = env["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 1, "缺省就是 0：{env}");
    assert_eq!(turns[0]["turn_uid"], json!(a[2]), "{env}");
}

#[test]
fn turn_absent_and_gated_are_the_same_no_rows_and_shape_errors_are_validation() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = seed_turn_fixture(dir.path());
    let mut s = Session::start_with(dir.path(), &ARGS_TURN);
    s.ask(&init(1, "qoder"));

    // 不存在的 64 hex ⇒ no_rows 且 isError=false（正常状态，不是故障）。
    let missing = call_normal_state_text(
        &mut s,
        2,
        "recollect_turn",
        json!({ "turn_uid": hex_uid(0xdead) }),
    );
    assert!(missing.contains("no_rows"), "{missing}");
    // 会话乙的轮（异 agent、异 principal）⇒ **同一段文本**：被闸挡与不存在同形。
    let gated = call_normal_state_text(
        &mut s,
        3,
        "recollect_turn",
        json!({ "turn_uid": b[0] }),
    );
    assert_eq!(gated, missing, "闸挡与查无必须同形（不泄露存在性）");

    // 形状不对（63 位 / 含大写）⇒ validation，isError=true。
    let t = call_error_text(&mut s, 4, "recollect_turn", json!({ "turn_uid": "a".repeat(63) }));
    assert!(t.contains("bad_turn_handle"), "{t}");
    let t = call_error_text(&mut s, 5, "recollect_turn", json!({ "turn_uid": "A".repeat(64) }));
    assert!(t.contains("bad_turn_handle"), "{t}");

    // 越界 before ⇒ validation（不 panic、不静默截成 10）。
    let t = call_error_text(
        &mut s,
        6,
        "recollect_turn",
        json!({ "turn_uid": a[2], "before": 11 }),
    );
    assert!(t.contains("bad_window"), "{t}");
}

// ── W39 刀 E（细案 §二.4）：remember 的 agent 轮写 startup principal ─────────
//
// 仿 `g19_selfserve.rs` 的 "带 `--principal-id` 起进程 + `recollect_remember`" 夹具；
// 判据的两格分别是 `sessions.owner_principal_id` 与 `session_principal_claim` 行数。

/// 该会话的归属镜像列（`sessions.owner_principal_id`），没有行也算一个读数。
fn session_owner(data: &Path, session_key: &str) -> Option<String> {
    let root = Root::open(data).unwrap();
    root.transcript()
        .conn()
        .query_row(
            "SELECT owner_principal_id FROM sessions
             WHERE session_key=?1 AND device_id='dev-synthetic'",
            [session_key],
            |r| r.get::<_, Option<String>>(0),
        )
        .unwrap()
}

#[test]
fn remember_writes_the_startup_principal_and_stays_null_without_it() {
    // 有 `--principal-id`：同事务盖章 + 镜像列都写上。
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::start_with(dir.path(), &ARGS_A);
    s.ask(&init(1, "agent-alpha"));
    let out: Value = call_json(
        &mut s,
        2,
        "recollect_remember",
        json!({ "body": "the quokka ledger note", "session_key": "ses-stamped" }),
    );
    assert!(out["turn_uid"].is_string(), "remember 必须回轮句柄：{out}");
    assert_eq!(
        session_owner(dir.path(), "ses-stamped").as_deref(),
        Some("p-one"),
        "带 principal 启动时，agent 轮所在会话必须有主人"
    );
    let root = Root::open(dir.path()).unwrap();
    let claims =
        rc_ledger::transcript::principal_claims(root.transcript(), Some("ses-stamped")).unwrap();
    assert_eq!(claims.len(), 1, "同事务应恰好盖一章：{claims:?}");
    assert_eq!(claims[0].principal_id, "p-one");
    assert_eq!(claims[0].source, "agent_runtime", "缺省章源：{claims:?}");
    assert_eq!(claims[0].claimed_by, "agent-alpha", "章的主体是自报 agent：{claims:?}");
    drop(root);

    // 对照臂：没有 `--principal-id` ⇒ 该列仍 NULL、章表不长行。
    let dir = tempfile::tempdir().unwrap();
    let mut bare = Session::start_with(
        dir.path(),
        &["--device-id", "dev-synthetic", "--agent-id", "agent-alpha"],
    );
    bare.ask(&init(1, "agent-alpha"));
    let out: Value = call_json(
        &mut bare,
        2,
        "recollect_remember",
        json!({ "body": "the wallaby ledger note", "session_key": "ses-bare" }),
    );
    assert!(out["turn_uid"].is_string(), "{out}");
    assert_eq!(
        session_owner(dir.path(), "ses-bare"),
        None,
        "没给 principal 就不许编一个主人"
    );
    let root = Root::open(dir.path()).unwrap();
    assert!(
        rc_ledger::transcript::principal_claims(root.transcript(), Some("ses-bare"))
            .unwrap()
            .is_empty(),
        "没给 principal 时章表不许长行"
    );
}
