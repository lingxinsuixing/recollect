// crates/rc-agent/tests/recall_principal_gate.rs
//! W37-A1（G12 跨 agent 读闸）的 recall 集成臂：agent-A 写卡（principal p1），
//! viewer = agent-B `with_principal(p1)` ⇒ recall 可见（4b 在候选层放行）；
//! viewer = agent-C(p2) 与无 principal 的 agent-B ⇒ 不可见，且与"查不到"同形
//! （查询臂报 `no_rows`/`visible_none_under_identity` 分型照旧、句柄臂进 `unresolved_handles`）。
//!
//! probe 半边（hook probe 补 viewer，W37-A1 条目④）：`probe_via_search` 带 viewer 时
//! 读者看不见的行不进候选池（`candidates` 变小），`None` 与改动前逐位相同。
//!
//! 夹具抄 `tests/recall_identity_silence.rs`：`put_batch_with_owner` 以 agent-A 落卡，
//! 归属带 principal；`rc_index::rebuild` 后命中。

use rc_agent::recall::{recall, RecallFilter, RecallRequest, Viewer};
use rc_memory::keys::{AnchorRole, MemTable, Scope, ScopeKind};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_x::{put_batch_with_owner, BatchCtx, WriteItem};
use rc_memory::Provenance;
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;
const QUERY: &str = "跨agent读闸合成词";

fn seeded() -> (tempfile::TempDir, Root, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let items = vec![WriteItem {
        table: MemTable::Card,
        kind: "note".into(),
        title: "读闸甲卡".into(),
        body: format!("{QUERY} 的一句合成正文"),
        scope: Scope { kind: ScopeKind::Project, key: "proj-gate".into() },
        valid_to_ms: None,
        aliases: vec![],
        prov: vec![Provenance { turn_uid: "w".repeat(64), anchor_role: AnchorRole::Human }],
    }];
    let k = put_batch_with_owner(
        root.memory(),
        &items,
        &BatchCtx { source_agent: "agent-A".into(), device_id: "dev-A".into() },
        &OwnerCtx {
            owner_principal_id: Some("p1".into()),
            owner_agent_id: "agent-A".into(),
            visibility: Visibility::PrivateAgent,
            retention: "default".into(),
        },
        NOW,
        &[],
    ).unwrap().remove(0).expect("夹具行不被抑制");
    rc_index::rebuild(root.memory(), NOW).unwrap();
    (dir, root, k.key_string())
}

fn filter(viewer: Option<Viewer>) -> RecallFilter {
    RecallFilter { viewer, ..Default::default() }
}

fn rows_of(v: &serde_json::Value) -> Vec<String> {
    v["rows"].as_array().unwrap().iter()
        .map(|r| r["id"].as_str().unwrap().to_owned()).collect()
}

fn unresolved_of(v: &serde_json::Value) -> Vec<String> {
    v["unresolved_handles"].as_array().unwrap().iter()
        .map(|x| x.as_str().unwrap().to_owned())
        .filter(|s| !s.starts_with('+'))
        .collect()
}

fn query(root: &Root, viewer: Option<Viewer>) -> serde_json::Value {
    let req = RecallRequest::query(QUERY).unwrap();
    let json = recall(root.memory(), &req, &filter(viewer), NOW).unwrap().text();
    serde_json::from_str(&json).unwrap()
}

/// 钉①·recall 集成臂：同 principal 异 agent 可见。
/// 钉②·阴性对照：异 principal 与无 principal 都不可见——且不是折成"查不到"：
/// D32 的分型照旧工作（候选在场、读闸全拒 ⇒ `visible_none_under_identity`），
/// 这同时证明 rank 在这条臂上没有提前过闸（否则这个码永远到不了）。
#[test]
fn recall同principal异agent可见_阴性对照与无principal() {
    let (_d, root, k) = seeded();

    // 阴性前置：agent-C(p2) 看不见——4b 只认同 principal。空读数走 D32 的身份分型。
    let req = RecallRequest::query(QUERY).unwrap();
    let err = recall(root.memory(), &req,
                     &filter(Some(Viewer::agent("agent-C").with_principal("p2"))), NOW)
        .expect_err("异 principal 该得到身份沉默分型");
    assert_eq!((err.kind, err.code),
               (rc_agent::AgentErrorKind::Authorization, "visible_none_under_identity"), "{err:?}");

    // 无 principal ⇒ 4b 永不命中，同样不可见（钉④：fail-closed 方向不变）。
    let err = recall(root.memory(), &req, &filter(Some(Viewer::agent("agent-B"))), NOW)
        .expect_err("无 principal 该得到同样的身份沉默分型");
    assert_eq!((err.kind, err.code),
               (rc_agent::AgentErrorKind::Authorization, "visible_none_under_identity"), "{err:?}");

    let v = query(&root, Some(Viewer::agent("agent-B").with_principal("p1")));
    assert_eq!(rows_of(&v), vec![k.clone()], "同 principal 异 agent 没经 4b 读到：{v}");
    assert!(unresolved_of(&v).is_empty(), "可见行不该出现在 unresolved_handles：{v}");
}

/// 钉②·句柄臂：被拒与查不到同形——都进 `unresolved_handles`，不报"无权限"。
/// "查不到"那一半用一个形状合法但库里没有的句柄（64 hex）。
#[test]
fn 句柄臂被拒与查不到同形进_unresolved_handles() {
    let (_d, root, k) = seeded();
    let ghost = format!("card:{}", "f".repeat(64));
    let (req, _dropped) = RecallRequest::handles(&[k.clone(), ghost]).unwrap();
    let json = recall(root.memory(), &req,
                      &filter(Some(Viewer::agent("agent-C").with_principal("p2"))), NOW)
        .unwrap().text();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(rows_of(&v).is_empty(), "异 principal 的行被句柄臂读出来了：{v}");
    let unresolved = unresolved_of(&v);
    assert!(unresolved.contains(&k), "被拒句柄与查不到不同形（没进 unresolved_handles）：{v}");
    assert_eq!(unresolved.len(), 2, "两种缺席必须同形：{v}");
}

/// hook probe 补 viewer（W37-A1 条目④）：带 viewer 时读者看不见的行不进候选池；
/// `None` 与改动前逐位相同（候选池里有）。
#[test]
fn probe带viewer时读者看不见的行不进候选池() {
    let (_d, root, _k) = seeded();

    let no_viewer = rc_agent::surface::probe_via_search(root.memory(), None, QUERY, NOW).unwrap();
    assert_eq!(no_viewer.candidates, 1, "无 viewer 的旧形状变了：{no_viewer:?}");

    let stranger = rc_agent::surface::probe_via_search(
        root.memory(), Some(&Viewer::agent("agent-C").with_principal("p2")), QUERY, NOW).unwrap();
    assert_eq!(stranger.candidates, 0, "异 principal 的 private 行进了 probe 候选池：{stranger:?}");
    assert!(stranger.forms.is_empty(), "看不见的行开了口：{stranger:?}");

    let mate = rc_agent::surface::probe_via_search(
        root.memory(), Some(&Viewer::agent("agent-B").with_principal("p1")), QUERY, NOW).unwrap();
    assert_eq!(mate.candidates, 1, "同 principal 异 agent 的行没进 probe 候选池：{mate:?}");
}
