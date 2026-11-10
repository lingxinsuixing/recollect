// crates/mcp/tests/recall_layers.rs
//! 派发档 I-5b：**mcp 工具面**接入 L2/L3 的三个可选参数，并钉住那两条命门。
//!
//! 这一份存在的理由只有三条，每条都对应派发档的一格：
//! 1. **§1 契约**：`recollect_recall` 多 `intent`/`depth`/`include_edges` 三个**可选**参数，
//!    校验点写在 `call_tool`（`tool_defs` 的 schema 是声明面、不执行），越界与未知值落
//!    `validation/bad_intent`、`validation/depth_out_of_range`，与既有 `bad_view` **同族**。
//! 2. **§2-A 命门**：`is_recall_empty_readout` 在 BASE 上只认 `no_rows`。库侧新增的 `no_episode`/
//!    `no_edges` 若照 `Empty` 一路标 `is_error=true`，"这次没有 episode"就变成一次**工具故障**
//!    （宿主会把它当传输问题重发）。这里把四码的可分性与 `isError` 那一维一起钉住，
//!    并钉 `row_absent` 仍标错误——判据不许放宽成"所有 Empty 都不算失败"。
//! 3. **§2-B 命门**：`no_episode`/`no_edges` **只在显式要那一层时报**（`intent=when|why`、
//!    `include_edges=true`；`depth>=2` 只加跳数、不单独触发）。默认 brief 绝不许因为
//!    "命中 L1 但没有 L2"而变成错误——那会把今天每一次正常召回都变成新错误。
//!
//! 播种是白盒的（真生产者 `put_episodes` / `put_edges`，不手搓 `card` 行）：本文件要证的是
//! **工具面把库侧读数透出去，并且只在被要求时才要那一层**。
//! 宿主主语与既有 mcp 夹具同族（`synthetic-agent`）：读闸按归属放行，名字不对就不是实现错
//! （`p7_state_view.rs` 的档头把这一格踩过一次，理由原样跟着搬）。
//!
//! T-5 的三段端到端（**副本目录**上的 stdio）住在最后一条用例里；`RECOLLECT_LAYERS_PRINT=1`
//! 时把三段原文打到 stdout（默认零输出，不给 `cargo test` 添噪声）。

mod common;
use common::{call, call_error_text, call_json, call_normal_state_text, init, tool_text, Session};
use rc_memory::keys::AnchorRole;
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_x::{
    put_card, put_edges, put_entity, put_episodes, BatchCtx, EdgeContent, EntityContent,
    EpisodeContent, FactContent,
};
use rc_memory::{put_fact_rows_with_extras, FactExtras, FactWriteItem, Provenance, Scope, ScopeKind};
use serde_json::{json, Value};
use std::path::Path;

const NOW: i64 = 1_700_000_000_000;
const MIN: i64 = 60_000;
/// 宿主自报的 agent 名 = 读闸里的读者。`common::seed` 那张卡与本文件所有派生行的归属都在它头上。
const AGENT: &str = "synthetic-agent";
/// L1 命中的词（`common::seed` 的标题里就有它）。
const TOK: &str = common::TOK;
/// L2/L3 那一族行的标题词：只出现在本文件自己播的卡上，与 `TOK` 那一族互不干扰。
const L2Q: &str = "邻域合成词";
/// 库里不存在的一张卡（`row_absent` 那一条臂要用一个**合法形状**的句柄，而不是坏句柄）。
const ABSENT: &str = "card:000000000000000000000000000000000000000000000000000000000000000f";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() }
}

fn prov(c: char) -> Vec<Provenance> {
    vec![Provenance { turn_uid: c.to_string().repeat(64), anchor_role: AnchorRole::Human }]
}

fn turn(index: usize) -> String {
    format!("{:064x}", 0x51 + index)
}

fn strip(handle: &str) -> String {
    handle.split_once(':').map(|(_, id)| id.to_string()).unwrap_or_else(|| handle.to_string())
}

fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-mcp".into()),
        owner_agent_id: AGENT.into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}

/// 一次白盒播种的全部 L1/L2/L3，返回四个句柄：
/// `(成员卡, 经历镜像卡, 边的甲端卡, 只有 L1 的那张卡)`。
///
/// 四个句柄是**故意分家**的，每一格对应一条命门：
/// - 成员卡：一条 fact 的陈述面消费卡，它**有**经历（`intent=when` 要读出 `episode_id`/`mates`）；
/// - 甲端卡：两条真边的一条端点，它**没有**经历（`include_edges=true` 要读出 `edges`）；
/// - 最后一张是 `common::seed` 的 L1 卡，经历与边两头都不沾——"命中 L1 但没有 L2/L3"那一格
///   就靠它验默认不报错、显式要才报。
fn seed_layers(data: &Path) -> (String, String, String, String) {
    let root = rc_store::Root::open(data).unwrap();
    let m = root.memory();
    let ctx = BatchCtx::synthetic();

    // ── L2：两个成员 fact（各有陈述面卡）⇒ 一段真生产者写的经历 ──
    let subject = put_entity(m, &EntityContent {
        kind: "person".into(), title: "邻域主语".into(), scope: scope(), aliases: vec![],
    }, &prov('e'), NOW).unwrap().id;
    let member = |i: usize, value: &str| -> String {
        let item = FactWriteItem {
            subject_anchor_id: subject.clone(),
            object_anchor_id: None,
            predicate: "做了".into(),
            value: Some(value.into()),
            category: "event".into(),
            mode: "stated".into(),
            time_text: String::new(),
            quote: value.into(),
            evidence: vec![(turn(i), AnchorRole::Human)],
            text: format!("{L2Q}的甲某{value}"),
            scope: scope(),
            source_ms: NOW + i as i64 * MIN,
        };
        let extras = FactExtras { participants: vec![], ..Default::default() };
        let out = put_fact_rows_with_extras(m, &[item], &[extras], &ctx, &owner(), NOW).unwrap();
        assert_eq!(out.facts, 1, "夹具的 fact 必须真的落库：{value}");
        format!("card:{}", out.card_ids[0].id)
    };
    let member_card = member(0, "调通闸");
    let second = member(1, "补了桩");
    // 成员卡 ⇒ fact id（经历点名的是 fact，不是卡）。
    let fact_ids = fact_ids_of(m, &[member_card.clone(), second.clone()]);
    assert_eq!(fact_ids.len(), 2, "两张成员卡必须各指回一条 fact");
    let written = put_episodes(m, &[EpisodeContent {
        title: "那段经历".into(),
        narrative: "先调通闸，再补了桩。".into(),
        fact_ids,
    }], &ctx, NOW).unwrap();
    assert_eq!(written.episodes, 1, "经历必须真落库：{written:?}");
    // 镜像卡句柄从库里读（不重算哈希：要证的正是生产者落下的那一张）。
    let episode_id: String = m.conn().query_row(
        "SELECT id FROM card WHERE kind='episode' ORDER BY created_at_ms, id LIMIT 1",
        [], |r| r.get(0)).unwrap();
    let mirror = format!("card:{episode_id}");

    // ── L3：两条真边（甲→乙 一跳、乙→丙 二跳），basis 有值 ⇒ 边可渲染 ──
    let endpoint = |title: &str, body: &str, c: char| -> String {
        put_card(m, &FactContent {
            kind: "decision".into(), title: format!("{L2Q}的{title}"), body: body.into(),
            scope: scope(), valid_to_ms: None,
        }, &prov(c), NOW).unwrap().key_string()
    };
    let hop_a = endpoint("甲端", "甲端落地。", 'a');
    let hop_b = endpoint("乙端", "乙端落地。", 'b');
    let hop_c = endpoint("丙端", "丙端落地。", 'c');
    for (from, to, reason, at) in [
        (hop_a.clone(), hop_b.clone(), "甲端等乙端。", NOW),
        (hop_b.clone(), hop_c.clone(), "乙端在丙端里。", NOW + MIN),
    ] {
        assert_eq!(put_edges(m, &[EdgeContent {
            from_id: strip(&from), to_id: strip(&to), kind: "depends_on".into(),
            reason: reason.into(), scope: scope(), basis_ids: vec![],
        }], &ctx, at).unwrap(), 1, "两条边都要真落库");
    }

    let l1_only = common::seed(data, NOW);
    rc_index::rebuild(m, NOW).unwrap();
    (member_card, mirror, hop_a, l1_only)
}

/// 卡的 `pointers_json` ⇒ 它点名的 fact id（数组序 = 写入序）。
fn fact_ids_of(m: &rc_store::MemoryHandle, card_handles: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for h in card_handles {
        let json_str: String = m.conn().query_row(
            "SELECT pointers_json FROM card WHERE id=?1", [&strip(h)], |r| r.get(0)).unwrap();
        for part in json_str.trim_matches(|c| c == '[' || c == ']').split(',') {
            if let Some(f) = part.trim().trim_matches('"').strip_prefix("fact:") {
                out.push(f.to_string());
            }
        }
    }
    out
}

/// 中心面的一次会话（`initialize` 已经把 `AGENT` 绑成读者）。
fn centre(data: &Path) -> Session {
    let mut s = Session::start(data);
    let _ = s.ask(&init(1, AGENT));
    s
}

/// 成功那一条路，但返回**文本**而不是 JSON（`brief`/`card` 是文本面，程序面才解 JSON）。
fn ok_text(s: &mut Session, id: i64, args: Value) -> String {
    let r = s.ask(&call(id, "recollect_recall", args));
    assert!(r["error"].is_null(), "走了 JSON-RPC error 而不是 isError：{r}");
    assert_eq!(r["result"]["isError"], json!(false), "期望成功，回执却标了失败：{r}");
    tool_text(&r)
}

/// 一次 `view=json` 的行数组（`call_json` 已经把"失败必须走 isError"那条协议纪律一起断了）。
fn json_rows(s: &mut Session, id: i64, args: Value) -> Vec<Value> {
    let mut v = args;
    v["view"] = json!("json");
    let doc = call_json(s, id, "recollect_recall", v);
    doc["rows"].as_array().cloned().unwrap_or_default()
}

/// 只回一句"这次是拒了还是收了"，不要求正文是 JSON：参数校验那一族用它，
/// 因为合法参数在别的夹具上仍可能落 `no_edges` 那类正常读数（那不是 JSON）。
fn recall_echo(s: &mut Session, id: i64, args: Value) -> (bool, String) {
    let r = s.ask(&call(id, "recollect_recall", args));
    assert!(r["error"].is_null(), "走了 JSON-RPC error 而不是 isError：{r}");
    (r["result"]["isError"].as_bool().unwrap(), tool_text(&r))
}

/// T-5 的取证口：默认**零输出**，只在 `RECOLLECT_LAYERS_PRINT=1` 时把原文打到 stdout。
/// 三段端到端原文要进报告，而 `cargo test` 的默认噪声不许涨。
fn show(label: &str, text: &str) {
    if std::env::var("RECOLLECT_LAYERS_PRINT").is_ok() {
        println!("──── {label} ────\n{text}");
    }
}

// ── §1 契约：三个可选参数的校验点（与 bad_view 同族，都写在 call_tool 里） ──────────

#[test]
fn intent_闭集外的值被拒_与_bad_view_同族() {
    let dir = tempfile::tempdir().unwrap();
    seed_layers(dir.path());
    let mut s = centre(dir.path());
    // 未知值：拒，且落 `validation` 型（改输入就对了，不是运维动作）。
    let (is_error, text) = recall_echo(&mut s, 2, json!({"query": TOK, "intent": "because"}));
    assert!(is_error, "坏 intent 是输入不合法，不是正常读数：{text}");
    assert!(text.contains("validation:bad_intent"), "闭集外要落 bad_intent：{text}");
    assert!(!text.contains("because"), "不许回显调用方给的原串：{text}");
    // 五个合法值全过：任何一个被 `bad_intent` 拒掉，就是白名单没开全。
    for intent in ["auto", "why", "when", "who", "what"] {
        let (_, text) = recall_echo(&mut s, 3, json!({"query": TOK, "intent": intent}));
        assert!(!text.contains("bad_intent"), "{intent} 是合法值却被拒：{text}");
    }
    // 非字符串的 intent 走 `as_str` 的宽容读法 = 缺省 `auto`（与既有 `view` 同一族），不报错。
    let (_, text) = recall_echo(&mut s, 4, json!({"query": TOK, "intent": 7}));
    assert!(!text.contains("bad_intent"), "非字符串 intent 按缺省读而不是报错：{text}");
}

#[test]
fn depth_越界被拒_一与二之外都不收货() {
    let dir = tempfile::tempdir().unwrap();
    seed_layers(dir.path());
    let mut s = centre(dir.path());
    for depth in [json!(0), json!(3), json!(-1)] {
        let (is_error, text) = recall_echo(&mut s, 2, json!({"query": TOK, "depth": depth}));
        assert!(is_error, "depth={depth} 越界要响亮拒绝：{text}");
        assert!(text.contains("validation:depth_out_of_range"),
            "越界要落 depth_out_of_range（与 bad_view 同族）：{text}");
    }
    // 1 与 2 收货；非整数（字符串）按缺省 1 宽容读，与既有 `as_i64` 一族一致。
    for depth in [json!(1), json!(2), json!("2")] {
        let rows = json_rows(&mut s, 3, json!({"query": TOK, "depth": depth}));
        assert!(!rows.is_empty(), "depth={depth} 应当被接受而不是报错");
    }
}

/// C6：`time_basis` 的闭集校验与既有三支同族（`call_tool` 里判，schema 只是声明面）。
/// 这一条落在中心面：中心面不应用内容过滤，但**参数合法性照样要拒**——静默忽略一个拼错的
/// 参数，会让调用方以为"按事件窗筛过了"。
#[test]
fn time_basis_闭集外的值被拒_两个合法值都收货() {
    let dir = tempfile::tempdir().unwrap();
    seed_layers(dir.path());
    let mut s = centre(dir.path());
    let (is_error, text) = recall_echo(&mut s, 2, json!({"query": TOK, "time_basis": "occurred"}));
    assert!(is_error, "坏 time_basis 是输入不合法：{text}");
    assert!(text.contains("validation:bad_time_basis"), "闭集外要落 bad_time_basis：{text}");
    assert!(!text.contains("occurred"), "不许回显调用方给的原串：{text}");
    for basis in ["as_of", "event"] {
        let (is_error, text) = recall_echo(&mut s, 3, json!({"query": TOK, "time_basis": basis}));
        assert!(!is_error, "{basis} 是合法值却被拒：{text}");
        assert!(!text.contains("bad_time_basis"), "{basis} 是合法值却被拒：{text}");
    }
    // 非字符串按缺省读（与 `intent`/`view` 的 `as_str` 一族一致），不报错。
    let (is_error, text) = recall_echo(&mut s, 4, json!({"query": TOK, "time_basis": 7}));
    assert!(!is_error && !text.contains("bad_time_basis"),
            "非字符串 time_basis 按缺省读而不是报错：{text}");
}

#[test]
fn 非布尔的_include_edges_按_false_宽容读() {    let dir = tempfile::tempdir().unwrap();
    let (member_card, _, hop_a, _) = seed_layers(dir.path());
    let mut s = centre(dir.path());
    // 给了字符串 "true" ⇒ 按 false 读 ⇒ 甲端那张卡的 edges 是空的（库里有边，但这一面没要）。
    let rows = json_rows(&mut s, 2,
        json!({"handles": [hop_a.clone(), member_card], "include_edges": "true"}));
    for row in &rows {
        assert_eq!(row["edges"].as_array().unwrap().len(), 0,
            "非布尔的 include_edges 不许被当成 true：{row}");
    }
    // 真布尔才要边（同一份库、同一张卡 ⇒ 差别只能由这一个参数产生）。
    let rows = json_rows(&mut s, 3, json!({"handles": [hop_a], "include_edges": true}));
    assert!(!rows[0]["edges"].as_array().unwrap().is_empty(),
        "布尔 true 才要边：{}", rows[0]);
}

// ── §2-B 命门：默认参数绝不因为"没有那一层"变成错误 ────────────────────────────────

#[test]
fn 默认参数下_命中_l1_而没有_l2_不是错误_也不带邻域() {
    let dir = tempfile::tempdir().unwrap();
    let _ = seed_layers(dir.path());
    let mut s = centre(dir.path());
    // 库里**有**经历也有边，而这次命中的那张卡两头都不沾。默认一次调用不许报任何新码。
    let text = ok_text(&mut s, 2, json!({"view": "brief", "query": TOK}));
    assert!(!text.contains("no_episode"), "默认 brief 不许冒出 no_episode：{text}");
    assert!(!text.contains("no_edges"), "默认 brief 不许冒出 no_edges：{text}");
    assert!(!text.contains("（L2）"), "默认 brief 不许把 L2 那一小段拼进来：{text}");
    let rows = json_rows(&mut s, 3, json!({"query": TOK}));
    let hit = rows.iter().find(|r| r["title"].as_str().is_some_and(|t| t.contains(TOK)))
        .expect("L1 命中行要在");
    assert_eq!(hit["layer"], json!("l1"), "默认读数只到 L1：{hit}");
    assert_eq!(hit["episode_id"], Value::Null, "默认臂一次都不碰经历：{hit}");
    assert_eq!(hit["mates"].as_array().unwrap().len(), 0, "默认臂不带成员：{hit}");
    assert_eq!(hit["edges"].as_array().unwrap().len(), 0, "默认臂不带边：{hit}");
}

#[test]
fn include_edges_为_true_时_一跳与两跳读到的边数不同() {
    let dir = tempfile::tempdir().unwrap();
    let (_, _, hop_a, _) = seed_layers(dir.path());
    let mut s = centre(dir.path());
    let one = json_rows(&mut s, 2, json!({"handles": [hop_a.clone()], "include_edges": true}));
    let two = json_rows(&mut s, 3,
        json!({"handles": [hop_a], "include_edges": true, "depth": 2}));
    let n1 = one[0]["edges"].as_array().unwrap().len();
    let n2 = two[0]["edges"].as_array().unwrap().len();
    assert!(n1 >= 1, "一跳要读到甲→乙那条：{}", one[0]);
    assert!(n2 > n1, "depth=2 要读出第二跳（乙→丙），跳数参数不许是摆设：{n1} vs {n2}");
    assert!(n2 <= rc_agent::recall::MAX_L3_EDGES, "两跳合计也不许越过库侧的帽：{n2}");
    // `depth>=2` 单独出现（没有 include_edges、不是 intent=why/when）时**不触发**新空读数：
    // 派发档 §2 那三格里只有 depth 这一格落在"要边"之外，它只加跳数。
    let (is_error, text) = recall_echo(&mut s, 4, json!({"query": TOK, "depth": 2}));
    assert!(!is_error, "depth=2 单独出现不许变成失败：{text}");
    assert!(!text.contains("no_edges"), "depth=2 单独出现不许报 no_edges：{text}");
}

#[test]
fn intent_when_时_brief_带_经历一跳_json_带_episode_id_与_mates() {
    let dir = tempfile::tempdir().unwrap();
    let (member_card, mirror, _, _) = seed_layers(dir.path());
    let mut s = centre(dir.path());
    let rows = json_rows(&mut s, 2, json!({"handles": [member_card.clone()], "intent": "when"}));
    let row = &rows[0];
    assert!(row["episode_id"].is_string(), "intent=when 要把经历 id 透出来：{row}");
    assert!(!row["mates"].as_array().unwrap().is_empty(), "intent=when 要带成员：{row}");
    assert_eq!(row["layer"], json!("l1"), "命中行自己仍是 L1，经历是它的邻域：{row}");
    // brief 面：邻域那一小段拼进来了（默认面没有它，见上面那条用例）。
    let brief = ok_text(&mut s, 3,
        json!({"view": "brief", "handles": [member_card], "intent": "when"}));
    assert!(brief.contains("（L2）"), "brief 没把 L2 那一跳渲出来：{brief}");
    assert!(brief.contains(&mirror), "brief 的 L2 块要报镜像卡句柄：{brief}");
    show("intent=when 的 brief 原文", &brief);
}

#[test]
fn intent_why_时读到边_而不是经历() {
    let dir = tempfile::tempdir().unwrap();
    let (_, _, hop_a, _) = seed_layers(dir.path());
    let mut s = centre(dir.path());
    let rows = json_rows(&mut s, 2, json!({"handles": [hop_a], "intent": "why"}));
    assert!(!rows[0]["edges"].as_array().unwrap().is_empty(),
        "intent=why 就是\"凭什么\"，要读到边：{}", rows[0]);
    assert_eq!(rows[0]["mates"].as_array().unwrap().len(), 0,
        "why 不读经历那一层：{}", rows[0]);
    // card 面的邻域块把边渲出来（brief 面不渲边，见 `recall.rs` 的 `neighbourhood_block` 只挂
    // 在 card/expand 上）——这一格钉的是"mcp 把库侧文本原样透出，不另算一套"。
    let card = ok_text(&mut s, 3, json!({"view": "card", "handles": [hop_a], "intent": "why"}));
    assert!(card.contains("（L3）"), "card 面没把 L3 的边渲出来：{card}");
}

// ── §2-A 命门：空读数四码可分、新码不标 isError ──────────────────────────────────

#[test]
fn no_episode_与_no_edges_是正常读数_is_error_为_false() {
    let dir = tempfile::tempdir().unwrap();
    let (member_card, _, _, l1_only) = seed_layers(dir.path());
    let mut s = centre(dir.path());
    // 显式要 L2，而命中的那张卡没有经历 ⇒ `empty:no_episode`，但它**不是**故障。
    let text = call_normal_state_text(&mut s, 2, "recollect_recall",
        json!({"view": "json", "handles": [l1_only.clone()], "intent": "when"}));
    assert!(text.contains("empty:no_episode"), "要的是 no_episode 那一码：{text}");
    // 显式要 L3，而那张卡身上没有边 ⇒ `empty:no_edges`，同样不是故障。
    let text = call_normal_state_text(&mut s, 3, "recollect_recall",
        json!({"view": "json", "handles": [l1_only.clone()], "include_edges": true}));
    assert!(text.contains("empty:no_edges"), "要的是 no_edges 那一码：{text}");
    // 同一次调用里只要**有一行**带着那一层，就不是"没有那一层"（整体不许报空读数）。
    let rows = json_rows(&mut s, 4,
        json!({"handles": [l1_only, member_card], "intent": "when"}));
    assert!(rows.iter().any(|r| r["episode_id"].is_string()),
        "有一行带着经历时不许整体报 no_episode：{rows:?}");
    // 两码互不串门：要经历给的是 no_episode，要边给的是 no_edges（四码可分这一格的第二半）。
    let text = call_normal_state_text(&mut s, 5, "recollect_recall",
        json!({"view": "json", "handles": [l1_only], "intent": "when"}));
    assert!(!text.contains("no_edges"), "no_episode 那一支不许冒出 no_edges：{text}");
}

#[test]
fn row_absent_仍是错误_判据没放宽成所有_empty() {
    let dir = tempfile::tempdir().unwrap();
    seed_layers(dir.path());
    let mut s = centre(dir.path());
    // `recollect_vote` 指认一条不存在的行 ⇒ 同为 `Empty` 族，但那**是**故障（调用方的动作错了）。
    let text = call_error_text(&mut s, 2, "recollect_vote",
        json!({"target": ABSENT, "stance": "confirmed"}));
    assert!(text.contains("empty:row_absent"), "这条臂要还认 row_absent：{text}");
}

#[test]
fn tools_list_把三个新参数声明出来_描述带_include_edges_的用法提示() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = centre(dir.path());
    let r = s.ask(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
    let tool = r["result"]["tools"].as_array().unwrap().iter()
        .find(|t| t["name"] == json!("recollect_recall")).expect("recollect_recall 在清单里");
    let props = &tool["inputSchema"]["properties"];
    for key in ["intent", "depth", "include_edges"] {
        assert!(props.get(key).is_some(), "{key} 没进 schema（声明面）：{props}");
    }
    assert_eq!(props["intent"]["enum"].as_array().unwrap().len(), 5, "intent 的闭集是五值");
    assert_eq!(props["depth"]["minimum"], json!(1), "depth 的下界要声明出来");
    assert_eq!(props["depth"]["maximum"], json!(2), "depth 的上界要声明出来");
    assert_eq!(props["include_edges"]["type"], json!("boolean"), "{props}");
    assert!(tool["description"].as_str().unwrap().contains("include_edges"),
        "描述要把 include_edges 的用法提示教出去：{}", tool["description"]);
}

// ── T-5：端到端真实消费者（**副本目录**上的 stdio 三段） ───────────────────────────

/// 节点目录 = `device.key` + `node-state.json` + `replica/memory.db`。
/// 副本文件走 `rc_store::snapshot::vacuum_into`（一致快照），**不是** `cp *.db`：本机是 WAL 模式，
/// 裸拷拿到的是 WAL 之外的旧库。全程只在临时目录里读写，绝不碰 `<data-dir>`。
fn node_dir_with_replica(src_dir: &Path, node: &Path, as_of_ms: i64) {
    let src = rc_store::Root::open(src_dir).unwrap();
    std::fs::create_dir_all(node.join("replica")).unwrap();
    rc_store::snapshot::vacuum_into(src.memory().conn(), &node.join("replica").join("memory.db"))
        .unwrap();
    let key = rc_net::key::DeviceKey::generate().unwrap();
    key.save(&node.join("device.key")).unwrap();
    let mut state = node::state::SyncState::default();
    state.current_manifest = node::state::CurrentManifest {
        epoch: 1, seq: 1, sha256: "0".repeat(64), created_at_ms: as_of_ms,
    };
    node::state::save(&node::state::state_path(node), &state).unwrap();
}

#[test]
fn 副本面三段_默认不带邻域_include_edges_带边_intent_when_带一跳() {
    let src = tempfile::tempdir().unwrap();
    let (member_card, _, hop_a, _) = seed_layers(src.path());
    let node = tempfile::tempdir().unwrap();
    node_dir_with_replica(src.path(), node.path(), NOW);

    let mut s = Session::start_with(node.path(), &["--replica"]);
    let _ = s.ask(&init(1, AGENT));
    // 副本身份自报：这一段说明三段读的都是**副本**而不是中心（`replica_mode.rs` 的既有判据）。
    let doc = call_json(&mut s, 2, "recollect_recall",
        json!({"view": "json", "handles": [member_card.clone()]}));
    assert_eq!(doc["source"], json!("replica"), "{doc}");

    // 第一段：默认 brief——一行 L1 命中，不带经历那一小段、不带边、不报新码。
    let first = ok_text(&mut s, 3, json!({"view": "brief", "handles": [member_card.clone()]}));
    show("① 默认 brief（不带 L2/L3）", &first);
    assert!(!first.contains("no_episode") && !first.contains("no_edges"),
        "默认读数不许冒新码：{first}");
    assert!(!first.contains("（L2）"), "默认 brief 不许拼 L2 那一跳：{first}");

    // 第二段：include_edges=true——甲端那张卡把边带出来（json 的数组 + card 面的 L3 块两格都读）。
    let second = ok_text(&mut s, 4,
        json!({"view": "card", "handles": [hop_a.clone()], "include_edges": true}));
    show("② include_edges=true（带边）", &second);
    assert!(second.contains("（L3）"), "副本面的 card 没渲出边：{second}");
    let rows = json_rows(&mut s, 5, json!({"handles": [hop_a.clone()], "include_edges": true}));
    assert!(!rows[0]["edges"].as_array().unwrap().is_empty(),
        "副本面的 include_edges 要真读到边：{}", rows[0]);

    // 第三段：intent=when——成员卡带经历一跳（episode id + 成员）。
    let third = ok_text(&mut s, 6,
        json!({"view": "brief", "handles": [member_card.clone()], "intent": "when"}));
    show("③ intent=when（带经历一跳）", &third);
    assert!(third.contains("（L2）"), "副本面的 intent=when 要渲出 L2 那一跳：{third}");
    let rows = json_rows(&mut s, 7, json!({"handles": [member_card], "intent": "when"}));
    assert!(rows[0]["episode_id"].is_string(), "第三段要带 episode_id：{}", rows[0]);
    assert!(!rows[0]["mates"].as_array().unwrap().is_empty(), "第三段要带成员：{}", rows[0]);
    // 第一段与第三段是**同一次会话、同一张卡**，只差一个参数 ⇒ 差别只可能由 `intent` 产生。
    assert!(third.len() > first.len(), "显式要那一层的那一段应当更长（同一库、同一张卡）：\
        {} vs {}", first.len(), third.len());
}

/// **T-4 的取证夹具**（`#[ignore]`，默认**不跑**；`cargo test -p mcp --test recall_layers
/// t4_播种 -- --ignored` 才跑）：把本文件那一份播种写进 `RECOLLECT_T4_SEED_DIR` 指定的目录，
/// 并把四个句柄落到同目录的 `t4-handles.txt`。
/// 它存在的理由与 `recall-contract.md` §7 那条"`#[ignore]` 的计时 smoke"同一族：
/// "默认逐字节不变"要在 **mcp 这一层**证，就要**两个二进制读同一份库**，而这份库只能由
/// 真生产者播出来（手搓 SQL 不算证据，同 §T-6 那条纪律）。播种内容与本文件全部用例同源，
/// 所以它不是一条新的数据形状，只是把同一份夹具换到一个能被两个进程共用的目录里。
#[test]
#[ignore]
fn t4_播种_给_base_worktree_对拍用() {
    let dir = std::env::var("RECOLLECT_T4_SEED_DIR").expect("要 RECOLLECT_T4_SEED_DIR");
    let path = std::path::Path::new(&dir);
    std::fs::create_dir_all(path).unwrap();
    let (member_card, mirror, hop_a, l1_only) = seed_layers(path);
    std::fs::write(path.join("t4-handles.txt"),
        format!("{member_card}\n{mirror}\n{hop_a}\n{l1_only}\n")).unwrap();
}
