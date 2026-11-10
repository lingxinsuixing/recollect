//! T10 四列的读回收据。`filter_tests` 用的是 `RowFacts` 字面量，这里用的是真库读出的行。
//! 四个值互不相同 ⇒ SELECT 列序与 `Raw` 元组序任何一次相邻同型列对调都会让某一条断言红。

use rc_memory::keys::{MemTable, Scope, ScopeKind};
use rc_memory::reader::{entity_ids_for_name, facts};
use rc_memory::writers_x::{put_batch, put_entity, BatchCtx, EntityContent, WriteItem};
use rc_memory::{AnchorRole, Provenance};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

#[test]
fn 四列从真库读回的就是写进去的那四个值() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let ctx = BatchCtx { source_agent: "agent-beta".into(), device_id: "dev-gamma".into() };
    let items = [WriteItem {
        table: MemTable::Card, kind: "note".into(), title: "四列读回".into(),
        body: "一条合成正文，不含任何生产文本".into(),
        scope: Scope { kind: ScopeKind::Person, key: "person-4col".into() },
        valid_to_ms: None, aliases: vec![],
        prov: vec![Provenance { turn_uid: "t-4col".into(), anchor_role: AnchorRole::Human }],
    }];
    let keys = put_batch(m, &items, &ctx, NOW).unwrap();
    let id = keys[0].as_ref().expect("写入口该给出句柄").id.clone();

    let row = facts(m).unwrap().into_iter().find(|f| f.key.id == id).expect("那行读得回来");
    assert_eq!(row.scope.kind, ScopeKind::Person, "scope_kind 读回的不是写进去的那一值");
    assert_eq!(row.scope.key, "person-4col", "scope_key 读回的不是写进去的那一值");
    assert_eq!(row.source_agent, "agent-beta", "source_agent 读回的不是写进去的那一值");
    assert_eq!(row.device_id, "dev-gamma", "device_id 读回的不是写进去的那一值");
}

/// 同一个实体既能按别名命中又能按标题命中时，去重支必须让它只出一个 id。
/// 牙：撤掉 `entity_ids_for_name` 里的 `if !ids.contains(&id)` ⇒ 这里数到 2。
#[test]
fn 别名与标题同指一个实体时只出一个_id() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    put_entity(m, &EntityContent {
        kind: "person".into(), title: "老王".into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-x".into() },
        aliases: vec!["老王".into()],
    }, &[Provenance { turn_uid: "t-alias".into(), anchor_role: AnchorRole::Human }], NOW).unwrap();
    let ids = entity_ids_for_name(m, "老王").unwrap();
    assert_eq!(ids.len(), 1, "两条查询各回一次同一个 id ⇒ 去重必须收成一格，实得 {ids:?}");
}

/// 3E-C21 的剩余半笔：**两个不同实体同名**（A39 那一型：kind 写法抖动 ⇒ 同题两行）。
/// 上一支钉的是"同一个 id 被两条查询各报一次 ⇒ 收成一条"，本支钉的是它的反面：
/// 去重支收成的是 **id**，不是**名字**——两个不同实体的同名必须两个都报出来。
/// 牙：把 `entity_ids_for_name` 标题那一趟的去重支换成"只补第一个就收工"（`push` 后 `break`）
/// ⇒ 本条数到 1 而红（上一支照样绿，两支各守一面）。
#[test]
fn 同名两个实体时两个_id_都要报出来() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let mk = |kind: &'static str, turn: &'static str| put_entity(m, &EntityContent {
        kind: kind.into(), title: "陈姐".into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-twin".into() },
        aliases: vec![],
    }, &[Provenance { turn_uid: turn.into(), anchor_role: AnchorRole::Human }], NOW);
    let a = mk("person", "t-twin-a").unwrap();
    let b = mk("agent", "t-twin-b").unwrap();
    assert_ne!(a.id, b.id, "夹具自己没造出两行 ⇒ 这条用例是空过的");
    let mut ids = entity_ids_for_name(m, "陈姐").unwrap();
    ids.sort();
    let mut want = vec![a.id.clone(), b.id.clone()];
    want.sort();
    assert_eq!(ids, want, "两个同名实体要报两个 id，收成一个是假合并");
}
