// crates/rc-agent/tests/recall_identity_silence.rs
//! D32：读侧身份静默空读的分型。三种沉默必须**能自证是哪种沉默**（台账 §10.151）：
//!
//! - 查询本身没命中 / 内容过滤（app、时间窗）筛空 / 库是空的 ⇒ `empty:no_rows`（原文不动）；
//! - 候选存在、内容过滤后仍存在、读闸把**这条身份**能看到的全部拒掉 ⇒
//!   `authorization:visible_none_under_identity` + hint 点名 `--agent-id`。
//!
//! 判据是 `gather` 查询臂里**读闸之前**通过 `recall_filter_match` 的候选数（`content_passed`），
//! 不是"库里有没有行"（`views.rs::entity_summary` / 全库行数当尺子会把"内容筛空"也算成身份）。
//!
//! 夹具抄 `tests/recall_ownership.rs`：`put_batch` 以 agent-A / agent-B 各写一张卡，两行正文
//! 共用同一个查询词，`rc_index::rebuild` 后命中两行；归属行随行落库。**全部断真读者出口**
//! `rc_agent::recall::{recall, recall_narrative}`，不自造 SQL 尺。

use rc_agent::recall::{
    recall, recall_narrative, NarrativeView, RecallFilter, RecallRequest, Viewer,
};
use rc_agent::{AgentErrorKind, ToolError};
use rc_memory::keys::{AnchorRole, MemTable, Scope, ScopeKind};
use rc_memory::writers_x::{put_batch, BatchCtx, WriteItem};
use rc_memory::Provenance;
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;
/// 两张卡共用的查询词：命中它就是命中两行，读者差异只来自读闸。
const QUERY: &str = "分叉身份静默合成词";
/// D32 的新闭集码（派发档 §二逐字）。
const NEW_CODE: &str = "visible_none_under_identity";
/// 裁定 R101：负样本必须是 ASCII 连续串——中文在 jieba 下必然切出与夹具共享的词元。
const NO_HIT_QUERY: &str = "zzqqxxsynthetic";

fn ctx(agent: &str) -> BatchCtx {
    BatchCtx { source_agent: agent.into(), device_id: format!("dev-{agent}") }
}

fn card(title: &str) -> WriteItem {
    WriteItem {
        table: MemTable::Card,
        kind: "note".into(),
        title: title.into(),
        body: format!("{QUERY} 的一句合成正文"),
        scope: Scope { kind: ScopeKind::Project, key: "proj-silence".into() },
        valid_to_ms: None,
        aliases: vec![],
        prov: vec![Provenance { turn_uid: "e".repeat(64), anchor_role: AnchorRole::Human }],
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: Root,
    a: String,
    b: String,
}

/// 同库两行：A 署名一行、B 署名一行（private_agent 归属由 `put_batch` 的合成默认给出）。
fn seeded() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let a = put_batch(m, &[card("静默甲卡")], &ctx("agent-A"), NOW).unwrap()[0]
        .clone().expect("A 的行不被抑制");
    let b = put_batch(m, &[card("静默乙卡")], &ctx("agent-B"), NOW).unwrap()[0]
        .clone().expect("B 的行不被抑制");
    assert_ne!(a, b, "前置：两行的句柄必须不同（内容寻址含标题）");
    rc_index::rebuild(m, NOW).unwrap();
    Fixture { _dir: dir, root, a: a.key_string(), b: b.key_string() }
}

fn filter(viewer: Option<Viewer>) -> RecallFilter {
    RecallFilter { viewer, ..Default::default() }
}

fn rows_of(v: &serde_json::Value) -> Vec<String> {
    v["rows"].as_array().expect("rows 必须是数组")
        .iter().map(|r| r["id"].as_str().expect("每行都带句柄").to_owned()).collect()
}

fn unresolved_of(v: &serde_json::Value) -> Vec<String> {
    v["unresolved_handles"].as_array().expect("unresolved_handles 必须是数组")
        .iter().map(|x| x.as_str().expect("每一项都是字符串").to_owned())
        .filter(|s| !s.starts_with('+'))
        .collect()
}

fn query(fx: &Fixture, viewer: Option<Viewer>) -> serde_json::Value {
    let req = RecallRequest::query(QUERY).unwrap();
    let json = recall(fx.root.memory(), &req, &filter(viewer), NOW).unwrap().text();
    serde_json::from_str(&json).unwrap()
}

fn query_err(fx: &Fixture, filter: &RecallFilter) -> ToolError {
    let req = RecallRequest::query(QUERY).unwrap();
    recall(fx.root.memory(), &req, filter, NOW)
        .expect_err("这个身份该得到空读数分型，不该拿到 Ok")
}

fn assert_no_rows(err: &ToolError) {
    assert_eq!((err.kind, err.code), (AgentErrorKind::Empty, "no_rows"),
               "这一种沉默必须仍是 no_rows 原文：{err:?}");
    assert_eq!(
        err.hint, "recall found nothing for this request",
        "no_rows 的 hint 原文也不许改：{err:?}"
    );
}

// —— 1. 错身份：候选在、内容过滤后仍在、读闸全拒 ⇒ 换码 ——

#[test]
fn 错身份换码_visible_none_under_identity() {
    let fx = seeded();
    let err = query_err(&fx, &filter(Some(Viewer::agent("agent-C"))));
    assert_eq!(err.kind, AgentErrorKind::Authorization, "错身份不在 Authorization 这一族：{err:?}");
    assert_eq!(err.code, NEW_CODE, "错身份没有换成新码：{err:?}");
    assert!(!err.to_string().contains("no_rows"), "新码的文本里不许再出现 no_rows：{err}");
    assert!(err.hint.contains("--agent-id"), "hint 要点名 --agent-id：{err}");
}

// —— 2. 双向对照：同一夹具换对身份仍走正常读数 ——

#[test]
fn 对身份照常读数_双向对照() {
    let fx = seeded();
    let v = query(&fx, Some(Viewer::agent("agent-A")));
    assert_eq!(rows_of(&v), vec![fx.a.clone()], "对身份拿到的行不对（或错误地换了码）");
    assert!(unresolved_of(&v).is_empty(), "查询臂不该有 unresolved_handles：{v}");
    assert_eq!(v["truncated"], serde_json::json!(false), "对身份被读成截断：{v}");

    let v = query(&fx, Some(Viewer::agent("agent-B")));
    assert_eq!(rows_of(&v), vec![fx.b.clone()], "B 那一侧不是镜像读数");
}

// —— 3. 叙事视图（brief）走同一支空读数判定 ——

#[test]
fn 叙事视图也换码() {
    let fx = seeded();
    let req = RecallRequest::query(QUERY).unwrap();

    let err = recall_narrative(
        fx.root.memory(), &req, &filter(Some(Viewer::agent("agent-C"))), NOW, NarrativeView::Brief,
    ).expect_err("叙事视图的错身份该得到分型错误");
    assert_eq!(err.kind, AgentErrorKind::Authorization, "叙事视图没换分型：{err:?}");
    assert_eq!(err.code, NEW_CODE, "叙事视图没换码：{err:?}");

    let ok = recall_narrative(
        fx.root.memory(), &req, &filter(Some(Viewer::agent("agent-A"))), NOW, NarrativeView::Brief,
    ).expect("对身份在叙事视图上不该报错");
    assert!(ok.text().contains("静默甲卡"), "对身份的简报里没有自己那一行：{}", ok.text());
    assert!(!ok.text().contains("静默乙卡"), "对身份的简报里漏进了别人的行：{}", ok.text());
}

// —— 4. 查询本身没命中任何候选 ⇒ 仍是 no_rows ——

#[test]
fn 查询无命中仍是_no_rows() {
    let fx = seeded();
    let req = RecallRequest::query(NO_HIT_QUERY).unwrap();
    let err = recall(fx.root.memory(), &req, &filter(Some(Viewer::agent("agent-C"))), NOW)
        .expect_err("无命中该得到空读数错误");
    assert_no_rows(&err);
}

// —— 5. 候选命中但内容过滤筛空 ⇒ 仍是 no_rows（不是身份沉默） ——

#[test]
fn 内容过滤筛空仍是_no_rows() {
    let fx = seeded();

    let by_app = RecallFilter {
        app: Some("no-such-app".into()),
        viewer: Some(Viewer::agent("agent-C")),
        ..Default::default()
    };
    assert_no_rows(&query_err(&fx, &by_app));

    // 时间窗外（`time_window` 闭区间，取 NOW-1 上界即把两行都排除）。
    let by_window = RecallFilter {
        time_window: Some((0, NOW - 1)),
        viewer: Some(Viewer::agent("agent-C")),
        ..Default::default()
    };
    assert_no_rows(&query_err(&fx, &by_window));
}

// —— 6. 空库 ⇒ 仍是 no_rows（不是新码） ——

#[test]
fn 空库仍是_no_rows() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    // 索引层必须建过：没建是 `index_not_built`（Unavailable），那是第四种沉默，不在本用例判据内。
    rc_index::rebuild(root.memory(), NOW).unwrap();

    let req = RecallRequest::query(QUERY).unwrap();
    let err = recall(root.memory(), &req, &filter(Some(Viewer::agent("agent-C"))), NOW)
        .expect_err("空库该得到空读数错误");
    assert_no_rows(&err);
}

// —— 7. 部分可见：命中两行、只看得见自己那一行 ⇒ Ok，不换码、不报错 ——

#[test]
fn 部分可见照常返回可见行_不换码() {
    let fx = seeded();

    // 前置（独立读数）：这份夹具的两行都被查询命中——空过滤器下两行都在。
    let all = query(&fx, None);
    let mut got = rows_of(&all);
    got.sort();
    let mut want = vec![fx.a.clone(), fx.b.clone()];
    want.sort();
    assert_eq!(got, want, "前置：同库两行必须都被查询命中（否则本用例对'部分可见'无判别力）");

    // 两个身份各自只看见自己那一行，都不换码。
    assert_eq!(rows_of(&query(&fx, Some(Viewer::agent("agent-A")))), vec![fx.a.clone()]);
    assert_eq!(rows_of(&query(&fx, Some(Viewer::agent("agent-B")))), vec![fx.b.clone()]);
}
