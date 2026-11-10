// crates/rc-agent/tests/recall_ownership.rs
//! S1b-C4a：`recollect_recall` 的**两条臂**都过读闸（设计 ownership-persistence §4.2）。
//!
//! 这一层钉的是"接上了没有"与"接的形状对不对"，不是读闸规则本身（规则矩阵在
//! `rc-memory/tests/gate.rs`）：
//! ① 查询臂：别的 agent 的 private 行不进候选集，**也不进 `unlisted` 口径**（过滤后计数）；
//! ② 句柄臂：被拒的句柄与查不到的句柄**同形**——都进 `unresolved_handles`、都不报"无权限"、
//!    都不回正文（不泄露存在性）；
//! ③ `viewer=None`（中心 CLI/旧测试）下面两条臂的旧行为一个字不变；
//! ④ public 行对另一个 agent 也可见（"闸门不是全拒"的反向读数）。
//!
//! 全部用真库：两行由 `put_batch` 分别以 agent-A / agent-B 署名写入，归属行随行落库
//! （C3 的写入继承），再用 `Viewer` 换读者；期望值不靠被过滤的那份输出反推。

use rc_agent::recall::{recall, RecallFilter, RecallRequest, Viewer};
use rc_memory::keys::{AnchorRole, MemTable, Scope, ScopeKind};
use rc_memory::writers_x::{put_batch, BatchCtx, WriteItem};
use rc_memory::Provenance;
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;
/// 两张卡共用的查询词：命中它就是命中两行，读者差异只来自读闸。
const QUERY: &str = "分叉归属合成词";

fn ctx(agent: &str) -> BatchCtx {
    BatchCtx { source_agent: agent.into(), device_id: format!("dev-{agent}") }
}

fn card(title: &str) -> WriteItem {
    WriteItem {
        table: MemTable::Card,
        kind: "note".into(),
        title: title.into(),
        body: format!("{QUERY} 的一句合成正文"),
        // 同 scope 是刻意的：跨 scope 的过滤另有读者（`recall_filter_wiring.rs`），
        // 这里要断的只有**归属**这一维。
        scope: Scope { kind: ScopeKind::Project, key: "proj-own".into() },
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
    let a = put_batch(m, &[card("归属甲卡")], &ctx("agent-A"), NOW).unwrap()[0]
        .clone().expect("A 的行不被抑制");
    let b = put_batch(m, &[card("归属乙卡")], &ctx("agent-B"), NOW).unwrap()[0]
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

/// `+Nnot_listed` 不是句柄：口径同 `handles.rs` 的同一支过滤。
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

fn handle(fx: &Fixture, handles: &[&str], viewer: Option<Viewer>) -> serde_json::Value {
    let raw: Vec<String> = handles.iter().map(|s| s.to_string()).collect();
    let (req, dropped) = RecallRequest::handles(&raw).unwrap();
    assert_eq!(dropped, 0, "前置：句柄现算自库里那一行，不该有条被拒");
    let json = recall(fx.root.memory(), &req, &filter(viewer), NOW).unwrap().text();
    serde_json::from_str(&json).unwrap()
}

/// 查询臂：A 只看到自己写的；B 只看到自己写的；没有 viewer 的人两行都看到。
#[test]
fn the_query_arm_returns_only_the_viewers_rows() {
    let fx = seeded();
    assert_eq!(rows_of(&query(&fx, Some(Viewer::agent("agent-A")))), vec![fx.a.clone()],
               "A 的查询臂读到了 B 的 private 行");
    assert_eq!(rows_of(&query(&fx, Some(Viewer::agent("agent-B")))), vec![fx.b.clone()],
               "B 的查询臂读到了 A 的 private 行");

    let mut all = rows_of(&query(&fx, None));
    all.sort();
    let mut want = vec![fx.a.clone(), fx.b.clone()];
    want.sort();
    assert_eq!(all, want, "viewer=None 改变了旧行为（两行都该在）");
}

/// 权限过滤算进"过滤后候选集"：A 的查询下面，被闸掉的 B **不进** `unlisted`，
/// 所以 `truncated` 必须是 false——否则"看不到的"会被说成"没列出来的"。
#[test]
fn denied_rows_do_not_show_up_as_truncation() {
    let fx = seeded();
    let v = query(&fx, Some(Viewer::agent("agent-A")));
    assert_eq!(v["truncated"], serde_json::json!(false),
               "被闸掉的行被记成了截断（`unlisted` 的口径漂了）");
    assert!(unresolved_of(&v).is_empty(), "查询臂不该有 unresolved_handles");
}

/// 句柄臂：点名 B 的句柄给 A ⇒ 缺席；点名自己的 ⇒ 读得回来。
#[test]
fn the_handle_arm_denies_another_agents_handle() {
    let fx = seeded();
    let denied = handle(&fx, &[&fx.b], Some(Viewer::agent("agent-A")));
    assert!(rows_of(&denied).is_empty(), "被拒句柄仍然带回了正文行");
    assert_eq!(unresolved_of(&denied), vec![fx.b.clone()], "被拒句柄没有出现在 unresolved_handles");

    let own = handle(&fx, &[&fx.a], Some(Viewer::agent("agent-A")));
    assert_eq!(rows_of(&own), vec![fx.a.clone()], "自己的句柄被闸掉了");
    assert!(unresolved_of(&own).is_empty());
}

/// 不泄露存在性：被拒的句柄与**根本不存在**的句柄，返回形状逐键相同
/// （只差 `unresolved_handles` 里那一串本身，而那一串是调用方递进来的）。
#[test]
fn a_denied_handle_looks_exactly_like_an_absent_one() {
    let fx = seeded();
    let ghost = format!("card:{}", "f".repeat(64));
    let denied = handle(&fx, &[&fx.b], Some(Viewer::agent("agent-A")));
    let absent = handle(&fx, &[&ghost], Some(Viewer::agent("agent-A")));

    let denied_keys: Vec<&String> = denied.as_object().unwrap().keys().collect();
    let absent_keys: Vec<&String> = absent.as_object().unwrap().keys().collect();
    assert_eq!(denied_keys, absent_keys, "两种缺席的 JSON 键集不同 ⇒ 形状本身泄露了存在性");
    assert_eq!(denied["truncated"], absent["truncated"]);
    assert_eq!(rows_of(&denied).len(), rows_of(&absent).len(), "两种缺席的行数不同");
    assert_eq!(unresolved_of(&absent), vec![ghost], "前置：幽灵句柄必须原样缺席");
}

/// `viewer=None` 下句柄臂的旧行为：点名谁给谁（含别人写的行）。
#[test]
fn without_a_viewer_the_handle_arm_keeps_its_old_behaviour() {
    let fx = seeded();
    let got = handle(&fx, &[&fx.a, &fx.b], None);
    assert_eq!(rows_of(&got), vec![fx.a.clone(), fx.b.clone()], "viewer=None 的句柄臂被闸住了");
    assert!(unresolved_of(&got).is_empty());
}

/// 反向读数：public 行对另一个 agent 也可见（闸门不是"只剩自己的"）。
/// 这里直接改归属行的 `visibility`：本用例要断的是**读侧**认这一列，
/// 写侧的归一（`owner::effective`）在 `owner_writes.rs` 那一侧。
#[test]
fn a_public_row_crosses_to_another_agent() {
    let fx = seeded();
    let id = fx.b.strip_prefix("card:").expect("句柄形状是 card:<id>");
    fx.root.memory().conn().execute(
        "UPDATE resource_owner SET visibility='public' WHERE mem_table='card' AND mem_id=?1",
        [id],
    ).unwrap();

    let got = query(&fx, Some(Viewer::agent("agent-A")));
    let mut ids = rows_of(&got);
    ids.sort();
    let mut want = vec![fx.a.clone(), fx.b.clone()];
    want.sort();
    assert_eq!(ids, want, "public 行没有跨过读闸");

    let by_handle = handle(&fx, &[&fx.b], Some(Viewer::agent("agent-A")));
    assert_eq!(rows_of(&by_handle), vec![fx.b.clone()], "public 行的句柄臂被闸掉了");
}
