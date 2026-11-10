//! `recollect-mcp --replica` 的黑盒判据：节点模式读副本、写本机、拒表态。
//!
//! 夹具（P-65 的共享往返住在 `common`）：
//! - 节点目录 = `device.key` + 可选 `replica/memory.db` + 可选 `node-state.json`；
//! - 副本由中心库白盒播种后**拷文件**得到（`seed` 已建索引），不是让 mcp 自己造；
//! - `node-state.json` 只给 `as_of` 那一格，内容是一份**假的**清单（mcp 只读时刻，不验签）。
//!
//! 五条用例对应工单的五格判定面：①无副本说"无可用记忆"；②有副本自报出处与截至时刻；
//! ③表态被拒；④remember 只写本机转录层；⑤无副本时 remember 仍然可用（否则新节点
//! 永远攒不出第一行可上行的内容）。

mod common;
use common::{call_error_text, call_json, call_normal_state_text, init, Session, seed, TOK};
use serde_json::json;
use std::path::Path;

/// 节点宿主的**主语**。S1b-C4a 起 `recollect_recall` 的节点面过读闸（`RecallFilter.viewer`
/// = 宿主自报的 agent），而读闸的放行条件之一是"这一行是这位 agent 产的"（private_agent）。
/// 本文件的副本行全部由 `seed` / `seed_distinct` 经 `BatchCtx::synthetic` 署名
/// （`synthetic-agent`）⇒ 节点宿主的主语必须是同一个名字，否则夹具在讲
/// "A 读 B 的 private 行"——那是 C4a 明确要拒的形状，而不是本文件要测的副本读取。
/// 生产里也是同一个 client 在中心产出、在节点读回，所以这不是为测试放宽，而是夹具对齐生产。
/// （一处常量而不是九处字面量：名字漂了，本文件的每条副本用例都会一起红。）
const NODE_AGENT: &str = "synthetic-agent";

fn make_key(dir: &Path) -> String {
    let key = rc_net::key::DeviceKey::generate().unwrap();
    key.save(&dir.join("device.key")).unwrap();
    key.device_id()
}

/// 造一份"已经同步过"的节点目录，返回副本里那张卡的句柄。
fn with_replica(dir: &Path, as_of_ms: i64) -> String {
    let centre = tempfile::tempdir().unwrap();
    let handle = seed(centre.path(), as_of_ms);
    let replica = dir.join("replica");
    std::fs::create_dir_all(&replica).unwrap();
    std::fs::copy(centre.path().join("memory.db"), replica.join("memory.db")).unwrap();
    let mut state = node::state::SyncState::default();
    state.current_manifest = node::state::CurrentManifest {
        epoch: 1,
        seq: 1,
        sha256: "0".repeat(64),
        created_at_ms: as_of_ms,
    };
    node::state::save(&node::state::state_path(dir), &state).unwrap();
    make_key(dir);
    handle
}

/// 第二代副本要**内容不同**：`seed` 的卡片键由内容决定，同一份内容两次播种
/// 会得到同一个键，换代测试就分不清读的是哪一代。
fn seed_distinct(dir: &Path, now_ms: i64, title: &str) -> String {
    use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
    let root = rc_store::Root::open(dir).unwrap();
    let k = put_card(
        root.memory(),
        &FactContent {
            kind: "decision".into(),
            title: format!("{title} 的口径"),
            body: format!("{title} 的召回面。"),
            scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: "b".repeat(64), anchor_role: AnchorRole::Human }],
        now_ms,
    )
    .unwrap();
    rc_index::rebuild(root.memory(), now_ms).unwrap();
    k.key_string()
}

fn turn_count(dir: &Path) -> i64 {
    let root = rc_store::NodeRoot::open_node(dir, None).unwrap();
    root.transcript()
        .conn()
        .query_row("SELECT count(*) FROM turns", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_replica_less_node_says_no_memory_not_no_rows() {
    let dir = tempfile::tempdir().unwrap();
    make_key(dir.path());
    let mut s = Session::start_with(dir.path(), &["--replica"]);
    let _ = s.ask(&init(1, NODE_AGENT));
    let text = call_error_text(&mut s, 2, "recollect_recall", json!({"view": "json", "query": "工具面合成词"}));
    assert!(text.contains("no_memory"), "副本不在时要说无可用记忆：{text}");
    assert!(!text.contains("no_rows"), "不许把'还没有副本'折成'库里没有'：{text}");
}

#[test]
fn the_node_recall_says_source_replica_and_carries_as_of() {
    let dir = tempfile::tempdir().unwrap();
    let as_of = 1_700_000_000_000;
    let handle = with_replica(dir.path(), as_of);
    let mut s = Session::start_with(dir.path(), &["--replica"]);
    let _ = s.ask(&init(1, NODE_AGENT));
    let v = call_json(&mut s, 2, "recollect_recall", json!({"view": "json", "handles": [handle]}));
    assert_eq!(v["source"], json!("replica"), "{v}");
    assert_eq!(v["as_of"], json!(as_of), "{v}");
    assert!(v["lag_ms"].as_i64().is_some_and(|ms| ms > 0), "{v}");
    assert!(!v["rows"].as_array().unwrap().is_empty(), "副本里的行要读得到：{v}");
}

#[test]
fn the_node_refuses_to_vote_on_a_read_only_replica() {
    let dir = tempfile::tempdir().unwrap();
    let handle = with_replica(dir.path(), 1_700_000_000_000);
    let mut s = Session::start_with(dir.path(), &["--replica"]);
    let _ = s.ask(&init(1, NODE_AGENT));
    let text = call_error_text(
        &mut s,
        2,
        "recollect_vote",
        json!({"target": handle, "stance": "confirmed"}),
    );
    assert!(text.contains("write_refused"), "表态要响亮拒绝：{text}");
}

#[test]
fn the_node_recall_filter_honours_app_and_window_but_never_device() {
    let dir = tempfile::tempdir().unwrap();
    let as_of = 1_700_000_000_000;
    with_replica(dir.path(), as_of);
    let mut s = Session::start_with(dir.path(), &["--replica"]);
    let _ = s.ask(&init(1, NODE_AGENT));
    // 句柄臂不过滤（recall 的两种请求形状里只有查询臂过 `RecallFilter`），
    // 所以这一格必须走 query，否则三条断言全是空过的。
    let v = call_json(
        &mut s,
        2,
        "recollect_recall",
        json!({"view": "json", "query": TOK, "device_id": "some-other-device"}),
    );
    assert!(!v["rows"].as_array().unwrap().is_empty(), "节点面不许按 device 筛：{v}");
    let v = call_json(
        &mut s,
        3,
        "recollect_recall",
        json!({"view": "json", "query": TOK, "app": "synthetic-agent"}),
    );
    assert!(!v["rows"].as_array().unwrap().is_empty(), "app 命中时行要在：{v}");
    // Task 10 之后这两格走 `call_normal_state_text`：内容过滤筛空得到的是 `no_rows`，
    // 那是**正常状态**（`isError: false`），而文本仍是 `empty:no_rows …` 三段原样。
    // 用例要证的"被筛掉了"那一维由文本里的码继续钉着，翻的只有 `isError`。
    let text = call_normal_state_text(
        &mut s,
        4,
        "recollect_recall",
        json!({"view": "json", "query": TOK, "app": "no-such-app"}),
    );
    assert!(text.contains("no_rows"), "app 不命中时要被筛掉：{text}");
    let text = call_normal_state_text(
        &mut s,
        5,
        "recollect_recall",
        json!({"view": "json", "query": TOK, "time_window": {"from": as_of + 1, "to": as_of + 2}}),
    );
    assert!(text.contains("no_rows"), "窗口外要被筛掉：{text}");
    let v = call_json(
        &mut s,
        6,
        "recollect_recall",
        json!({"view": "json", "query": TOK, "time_window": {"from": as_of, "to": as_of}}),
    );
    assert!(!v["rows"].as_array().unwrap().is_empty(), "闭区间端点要算在窗口内：{v}");
}

#[test]
fn the_node_remember_writes_the_local_transcript_only() {
    let dir = tempfile::tempdir().unwrap();
    with_replica(dir.path(), 1_700_000_000_000);
    let replica = dir.path().join("replica").join("memory.db");
    let before = std::fs::read(&replica).unwrap();
    let mut s = Session::start_with(dir.path(), &["--replica"]);
    let _ = s.ask(&init(1, NODE_AGENT));
    let v = call_json(&mut s, 2, "recollect_remember", json!({"body": "CANARY-NODE-BODY-7714"}));
    assert_eq!(v["turn_uid"].as_str().unwrap().len(), 64, "{v}");
    assert!(s.drain().is_empty(), "一次往返只许一条回包");
    assert_eq!(std::fs::read(&replica).unwrap(), before, "副本文件一个字节都不许动");
    assert_eq!(turn_count(dir.path()), 1, "写要落在本机转录层");
}

#[test]
fn a_replica_less_node_still_remembers_locally() {
    let dir = tempfile::tempdir().unwrap();
    make_key(dir.path());
    let mut s = Session::start_with(dir.path(), &["--replica"]);
    let _ = s.ask(&init(1, NODE_AGENT));
    let v = call_json(&mut s, 2, "recollect_remember", json!({"body": "CANARY-NODE-BODY-8825"}));
    assert_eq!(v["turn_uid"].as_str().unwrap().len(), 64, "{v}");
    assert!(v["similar_n"].is_null(), "没有记忆库时'这次没比'：{v}");
    assert!(s.drain().is_empty(), "一次往返只许一条回包");
    assert_eq!(turn_count(dir.path()), 1, "没有副本也要能写本机转录层");
}

#[test]
fn the_node_switches_to_a_new_generation_while_running() {
    let dir = tempfile::tempdir().unwrap();
    let old_as_of = 1_700_000_000_000;
    let old_handle = with_replica(dir.path(), old_as_of);
    let mut s = Session::start_with(dir.path(), &["--replica"]);
    let _ = s.ask(&init(1, NODE_AGENT));
    let v = call_json(&mut s, 2, "recollect_recall", json!({"view": "json", "handles": [old_handle]}));
    assert_eq!(v["as_of"], json!(old_as_of), "{v}");

    // 第二次同步：新一代落到代际名下，状态推进。mcp 还在跑。
    let newer = tempfile::tempdir().unwrap();
    let new_as_of = old_as_of + 86_400_000;
    let new_handle = seed_distinct(newer.path(), new_as_of, "GEN-TWO");
    let replica = dir.path().join("replica");
    std::fs::copy(newer.path().join("memory.db"), replica.join("snapshot-2-1.db")).unwrap();
    let mut state =
        node::state::load(&node::state::state_path(dir.path())).unwrap().unwrap();
    state.current_manifest = node::state::CurrentManifest {
        epoch: 2,
        seq: 1,
        sha256: "1".repeat(64),
        created_at_ms: new_as_of,
    };
    node::state::save(&node::state::state_path(dir.path()), &state).unwrap();

    let v = call_json(&mut s, 3, "recollect_recall", json!({"view": "json", "handles": [new_handle]}));
    assert_eq!(v["as_of"], json!(new_as_of), "{v}");
    assert!(!v["rows"].as_array().unwrap().is_empty(), "长驻的 mcp 要看见换代后的副本：{v}");
}

#[test]
fn the_staging_layer_marks_pending_rows_and_flips_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let as_of = 1_700_000_000_000;
    let base_handle = with_replica(dir.path(), as_of);
    let manifest = node::state::CurrentManifest {
        epoch: 1,
        seq: 1,
        sha256: "0".repeat(64),
        created_at_ms: as_of,
    };
    node::staging::prepare(dir.path(), &manifest, as_of).unwrap();
    let staged = node::staging::path(dir.path());
    let pending_handle = {
        use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
        let root = rc_store::AgentRoot::open_replica_rw(dir.path(), &staged).unwrap();
        let key = put_card(
            root.memory(),
            &FactContent {
                kind: "decision".into(),
                title: format!("{TOK} pending"),
                body: format!("{TOK} pending body"),
                scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
                valid_to_ms: None,
            },
            &[Provenance { turn_uid: "c".repeat(64), anchor_role: AnchorRole::Human }],
            as_of + 10,
        )
        .unwrap();
        rc_index::rebuild(root.memory(), as_of + 10).unwrap();
        root.memory()
            .conn()
            .execute(
                "INSERT INTO staging_new_row(id, table_name) VALUES(?1,'card')",
                [key.key_string()],
            )
            .unwrap();
        key.key_string()
    };

    let mut s = Session::start_with(dir.path(), &["--replica"]);
    let _ = s.ask(&init(1, NODE_AGENT));
    let v = call_json(&mut s, 2, "recollect_recall", json!({"view": "json", "query": TOK}));
    assert_eq!(v["source"], json!("pending_reconcile"), "{v}");
    let rows = v["rows"].as_array().unwrap();
    let base_row = rows.iter().find(|r| r["id"] == base_handle).expect("base row");
    assert!(base_row.get("pending").is_none(), "已对账的行不许标 pending：{v}");
    let pending_row = rows.iter().find(|r| r["id"] == pending_handle).expect("pending row");
    assert_eq!(pending_row["pending"], json!(true), "{v}");
    drop(s);

    node::staging::drop_layer(dir.path()).unwrap();
    let mut s2 = Session::start_with(dir.path(), &["--replica"]);
    let _ = s2.ask(&init(1, NODE_AGENT));
    let v2 = call_json(&mut s2, 2, "recollect_recall", json!({"view": "json", "query": TOK}));
    assert_eq!(v2["source"], json!("replica"), "{v2}");
    assert!(
        v2["rows"].as_array().unwrap().iter().all(|r| r["id"] != pending_handle),
        "丢掉暂存层后新行不该在：{v2}"
    );
}

// ── I-5b 补的一格：空读数四码可分，而新加的两码不是故障 ──────────────────────────
// 派发档 §2-A 的命门在**节点面**的形状：`is_recall_empty_readout` 在 BASE 上只认 `no_rows`，
// 库侧 Phase 4 新加的 `no_episode` / `no_edges` 若照 `Empty` 一路标 `is_error=true`，
// "这台机器的副本里没有这一段经历"就变成一次工具故障——而宿主对故障的动作是重发。
// 四码各一例、各归各的下一步：`no_rows` 改词 / `no_episode` 与 `no_edges` 什么都不用做 /
// `no_memory` 是运维态，必须仍然响亮（接入文档 §5：要报给操作员）。

#[test]
fn 空读数四码可分_新两码与_no_rows_一样不标_is_error() {
    let dir = tempfile::tempdir().unwrap();
    let handle = with_replica(dir.path(), 1_700_000_000_000);
    let mut s = Session::start_with(dir.path(), &["--replica"]);
    let _ = s.ask(&init(1, NODE_AGENT));
    // ① 有副本、这次没命中 ⇒ `no_rows`（既有那一格，Task 10 已钉）。
    let text = call_normal_state_text(&mut s, 2, "recollect_recall",
        json!({"view": "json", "query": "no-such-synthetic-word"}));
    assert!(text.contains("empty:no_rows"), "没命中要说 no_rows：{text}");
    assert!(!text.contains("no_episode") && !text.contains("no_edges"),
        "三码不许串门（可分性判据的一半）：{text}");
    // ② 显式要经历，而副本里那张卡不属于任何经历 ⇒ `no_episode`，正常读数。
    let text = call_normal_state_text(&mut s, 3, "recollect_recall",
        json!({"view": "json", "handles": [handle.clone()], "intent": "when"}));
    assert!(text.contains("empty:no_episode"), "{text}");
    assert!(!text.contains("no_rows"), "命中的是行、缺的是那一层，两码不同：{text}");
    // ③ 显式要边 ⇒ `no_edges`，同样正常。
    let text = call_normal_state_text(&mut s, 4, "recollect_recall",
        json!({"view": "json", "handles": [handle.clone()], "include_edges": true}));
    assert!(text.contains("empty:no_edges"), "{text}");
    // ④ 默认那一次照旧是一次成功读数（副本里那张卡读得到；上面三个空读数都没把它改成故障）。
    let v = call_json(&mut s, 5, "recollect_recall", json!({"view": "json", "handles": [handle]}));
    assert!(!v["rows"].as_array().unwrap().is_empty(), "{v}");
    assert_eq!(v["rows"].as_array().unwrap()[0]["edges"].as_array().unwrap().len(), 0,
        "默认面不点层 ⇒ 边数组恒空：{v}");
    // ⑤ 没有副本的目录：`no_memory` 仍是一次**故障**（运维态），而且它不被那两个新码顶掉。
    let bare = tempfile::tempdir().unwrap();
    make_key(bare.path());
    let mut s2 = Session::start_with(bare.path(), &["--replica"]);
    let _ = s2.ask(&init(1, NODE_AGENT));
    let text = call_error_text(&mut s2, 2, "recollect_recall",
        json!({"view": "json", "query": TOK, "intent": "when", "include_edges": true}));
    assert!(text.contains("no_memory"), "无副本要说不等价于无那一层：{text}");
    assert!(!text.contains("no_episode") && !text.contains("no_edges"),
        "副本都不在时不许冒邻域那两码：{text}");
}

// ── W39 刀 D：边缘转录四件的**远端优先 / 本地回落** ─────────────────────────────
// 两臂各钉一格来源标（细案 2026-11-09-w39-remote-transcript §3.2/§五.6）：
// ① 还没有 `node-state.json`（未配对）⇒ 本地读数、`source:"local"`、`degraded:false`，
//    **不带** `remote_error`（不是故障）；
// ② 有 state 但端点一条都不通 ⇒ 同样本地读数、`source:"local"`、`degraded:true`、
//    `remote_error:"unreachable"`（显式降级，不假装中心零命中）。
// 两臂都不许出现 `source:"center"`：这里根本没有中心可读。

/// 边缘本地转录层的命中词（自造合成词；中心那边不存在，命中只可能来自本机）。
const EDGE_MARKER: &str = "wombatglint";
const EDGE_SESSION: &str = "ses-edge-transcript";

/// 往边缘本机转录层写两轮。uid 用 64 位小写 hex——`recollect_turn` 的形状门要求。
fn seed_local_turns(dir: &Path, device_id: &str) -> Vec<String> {
    use rc_contract::{compute_body_fp, NormalizedTurn, SessionId, TurnTier};
    let root = rc_store::NodeRoot::open_node(dir, None).unwrap();
    let sid = SessionId {
        session_key: EDGE_SESSION.to_string(),
        device_id: device_id.to_string(),
        source_agent: NODE_AGENT.to_string(),
        machine: None,
        project_hint: None,
        last_saved_at_ms: None,
        owner_principal_id: None,
        principal_name: None,
        principal_source: None,
    };
    let bodies = [
        format!("{EDGE_MARKER} edge opening words"),
        format!("{EDGE_MARKER} edge closing words"),
    ];
    let turns: Vec<NormalizedTurn> = bodies
        .iter()
        .enumerate()
        .map(|(i, body)| NormalizedTurn {
            turn_uid: format!("{:064x}", 0x51 + i as u8),
            body_fp: compute_body_fp(body),
            tier: TurnTier::ContentOnly,
            body: body.clone(),
            role: "agent".into(),
            created_at_ms: 100 + i as i64,
        })
        .collect();
    let uids = turns.iter().map(|t| t.turn_uid.clone()).collect();
    rc_ledger::insert_batch(root.transcript(), &sid, &turns, 200).unwrap();
    uids
}

/// 伪造"已配对但端点不通"的材料：指纹形状合法、地址指向一个刚放掉的回环端口。
/// token 与中心公钥只要非空即可——远端在第一跳（challenge）就失败，两者都不会被真的用到。
fn write_unreachable_pairing(dir: &Path) {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let state = node::state::SyncState {
        paired_hub_pub: "not-a-real-hub-pub".into(),
        paired_tls_fp: Some("ab".repeat(32)),
        paired_endpoints: vec![format!("https://127.0.0.1:{port}")],
        ..node::state::SyncState::default()
    };
    node::state::save(&node::state::state_path(dir), &state).unwrap();
    std::fs::write(node::state::token_path(dir), "not-a-real-token\n").unwrap();
}

#[test]
fn an_unpaired_edge_reads_the_local_transcript_and_marks_source_local() {
    let dir = tempfile::tempdir().unwrap();
    let device = make_key(dir.path());
    let uids = seed_local_turns(dir.path(), &device);
    let mut s = Session::start_with(dir.path(), &["--replica"]);
    let _ = s.ask(&init(1, NODE_AGENT));

    let v = call_json(&mut s, 2, "recollect_sessions", json!({}));
    assert_eq!(v["source"], json!("local"), "未配对时读本机：{v}");
    assert_eq!(v["degraded"], json!(false), "未配对不是故障：{v}");
    assert!(v.get("remote_error").is_none(), "未配对不带 remote_error：{v}");
    assert!(
        v["sessions"].as_array().unwrap().iter().any(|r| r["session_key"] == json!(EDGE_SESSION)),
        "本机会话要在清单里：{v}"
    );

    let v = call_json(&mut s, 3, "recollect_search", json!({"query": EDGE_MARKER}));
    assert_eq!(v["source"], json!("local"), "{v}");
    assert_eq!(v["degraded"], json!(false), "{v}");
    assert!(v.get("remote_error").is_none(), "{v}");
    assert!(!v["hits"].as_array().unwrap().is_empty(), "本机命中要在：{v}");

    let v = call_json(&mut s, 4, "recollect_session", json!({"session_key": EDGE_SESSION}));
    assert_eq!(v["source"], json!("local"), "{v}");
    assert_eq!(v["degraded"], json!(false), "{v}");
    assert_eq!(v["turns"].as_array().unwrap().len(), 2, "两轮正文要在：{v}");

    let v = call_json(&mut s, 5, "recollect_turn", json!({"turn_uid": uids[1], "before": 1}));
    assert_eq!(v["source"], json!("local"), "{v}");
    assert_eq!(v["degraded"], json!(false), "{v}");
    assert_eq!(v["turns"].as_array().unwrap().len(), 2, "目标轮 + 前一邻居：{v}");
}

#[test]
fn an_unreachable_centre_falls_back_to_local_and_says_degraded() {
    let dir = tempfile::tempdir().unwrap();
    let device = make_key(dir.path());
    let uids = seed_local_turns(dir.path(), &device);
    write_unreachable_pairing(dir.path());
    let mut s = Session::start_with(dir.path(), &["--replica"]);
    let _ = s.ask(&init(1, NODE_AGENT));

    let v = call_json(&mut s, 2, "recollect_sessions", json!({}));
    assert_eq!(v["source"], json!("local"), "降级读数仍来自本机：{v}");
    assert_eq!(v["degraded"], json!(true), "中心不可达要显式降级：{v}");
    assert_eq!(v["remote_error"], json!("unreachable"), "{v}");

    let v = call_json(&mut s, 3, "recollect_search", json!({"query": EDGE_MARKER}));
    assert_eq!(v["source"], json!("local"), "{v}");
    assert_eq!(v["degraded"], json!(true), "{v}");
    assert_eq!(v["remote_error"], json!("unreachable"), "{v}");
    assert!(!v["hits"].as_array().unwrap().is_empty(), "回落仍要读到本机：{v}");

    let v = call_json(&mut s, 4, "recollect_session", json!({"session_key": EDGE_SESSION}));
    assert_eq!(v["source"], json!("local"), "{v}");
    assert_eq!(v["degraded"], json!(true), "{v}");
    assert_eq!(v["remote_error"], json!("unreachable"), "{v}");
    assert_eq!(v["turns"].as_array().unwrap().len(), 2, "{v}");

    let v = call_json(&mut s, 5, "recollect_turn", json!({"turn_uid": uids[1], "before": 1}));
    assert_eq!(v["source"], json!("local"), "{v}");
    assert_eq!(v["degraded"], json!(true), "{v}");
    assert_eq!(v["remote_error"], json!("unreachable"), "{v}");
    assert_eq!(v["turns"].as_array().unwrap().len(), 2, "{v}");
}
