//! 2026-10-27 快照数据丢失的回归：退场锚、活 fact、卡与出处。
//!
//! 真机读数（中心 `memory.db` 134 fact / 134 card，seq7 快照 42 fact / 127 card）：
//! `prune` 删退场 anchor 时，`fact.subject_anchor_id ON DELETE CASCADE` 把中心仍持有的
//! fact 一起删掉，卡的 `pointers_json` 随之指向不存在的行；同一步又把 `fact` 的
//! `provenance` 整族删掉，于是卡沿指针也拿不到轮次出处。
//!
//! 三条判据（对应修复的三件事）：
//! 1. 被 fact 引用的退场锚与它的 fact/card 都留在副本里，fact 的 object/place 不被 SET NULL；
//! 2. 副本的 `provenance` 含 `mem_table='fact'` 的行（卡指针的出处来源）；
//! 3. 活卡的 `fact:` 指针落空 ⇒ `build` 拒绝发布，临时文件不留下。

use rc_memory::keys::{AnchorRole, Scope, ScopeKind};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::snapshot::{build, SnapshotError};
use rc_memory::writers_c::retire;
use rc_memory::writers_fact::{put_fact_rows, FactWriteItem};
use rc_memory::writers_x::{put_batch, BatchCtx, WriteItem};
use rc_memory::{MemKey, MemTable, Provenance};
use rc_store::Root;

const NOW: i64 = 1_700_000_999_000;
const SRC_MS: i64 = 1_700_000_000_000;

fn ctx() -> BatchCtx {
    BatchCtx { source_agent: "agent-snap-card".into(), device_id: "dev-snap-card".into() }
}

fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-snap-card".into()),
        owner_agent_id: "agent-snap-card".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "p-snap-card".into() }
}

fn turn(seed: char) -> String {
    seed.to_string().repeat(64)
}

fn anchor_item(title: &str, seed: char) -> WriteItem {
    WriteItem {
        table: MemTable::Anchor,
        kind: "concept".into(),
        title: title.into(),
        body: "快照事实夹具锚".into(),
        scope: scope(),
        valid_to_ms: None,
        aliases: vec![],
        prov: vec![Provenance { turn_uid: turn(seed), anchor_role: AnchorRole::Human }],
    }
}

fn card_item(title: &str, seed: char) -> WriteItem {
    WriteItem {
        table: MemTable::Card,
        kind: "lesson".into(),
        title: title.into(),
        body: "快照事实夹具卡".into(),
        scope: scope(),
        valid_to_ms: None,
        aliases: vec![],
        prov: vec![Provenance { turn_uid: turn(seed), anchor_role: AnchorRole::Human }],
    }
}

fn key(keys: &[Option<MemKey>], index: usize) -> MemKey {
    keys[index].clone().expect("fixture row must not be suppressed")
}

fn fact_item(subject: &MemKey, object: Option<&MemKey>) -> FactWriteItem {
    FactWriteItem {
        subject_anchor_id: subject.id.clone(),
        object_anchor_id: object.map(|k| k.id.clone()),
        predicate: "promise".into(),
        value: Some("把报价单发出去".into()),
        category: "promise".into(),
        mode: "stated".into(),
        time_text: String::new(),
        quote: "我明天把报价单发给你".into(),
        evidence: vec![(turn('a'), AnchorRole::Human), (turn('b'), AnchorRole::Agent)],
        text: "张三承诺明天把报价单发给李四，并在当天下午跟进确认一次。".into(),
        scope: scope(),
        source_ms: SRC_MS,
    }
}

fn count(conn: &rusqlite::Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap()
}

/// 判据 1 + 2：退场锚被 fact 引用 ⇒ 锚、fact、card 全留；fact 的 object 不被抹 NULL；
/// 卡指针指向的 fact 存在，且 fact 的两条轮次出处都在副本里。
#[test]
fn a_retired_anchor_referenced_by_a_fact_keeps_the_fact_and_its_card() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let keys = put_batch(m, &[anchor_item("张三", 'e'), anchor_item("李四", 'f')], &ctx(), NOW)
        .unwrap();
    let (subject, object) = (key(&keys, 0), key(&keys, 1));
    put_fact_rows(m, &[fact_item(&subject, Some(&object))], &ctx(), &owner(), NOW).unwrap();

    // 与真机同一时序：先有 fact，再退场锚（中心是软退场，行和 fact 都还在）。
    retire(m, &subject, "fixture", NOW + 1).unwrap();
    retire(m, &object, "fixture", NOW + 2).unwrap();
    assert_eq!(count(m.conn(), "fact"), 1, "前置：fact 没写进中心");
    assert_eq!(count(m.conn(), "card"), 1, "前置：卡没写进中心");

    let snaps = dir.path().join("snapshots");
    let built = build(m, &snaps, NOW + 10, true).unwrap();
    assert_eq!(
        built.pruned.dead_rows, 0,
        "被 fact 引用的退场锚不该被删（也不该连带删 fact）：{:?}",
        built.pruned
    );

    let copy = rusqlite::Connection::open(&built.path).unwrap();
    let fact_id: String = copy
        .query_row("SELECT id FROM fact", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count(&copy, "fact"), 1, "副本丢了被退场锚引用的 fact");
    assert_eq!(count(&copy, "card"), 1, "副本丢了 fact 对应的卡");
    assert_eq!(count(&copy, "anchor"), 2, "副本丢了作为外键目标的退场锚");

    let (subj, obj): (String, Option<String>) = copy
        .query_row(
            "SELECT subject_anchor_id, object_anchor_id FROM fact WHERE id=?1",
            [&fact_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(subj, subject.id, "fact 的 subject 指针变了");
    assert_eq!(
        obj.as_deref(),
        Some(object.id.as_str()),
        "退场锚让 fact.object_anchor_id 被 SET NULL（中心有值、副本变 NULL = 静默改内容）"
    );

    // 卡仍然指回这行 fact，且指针能落到真实行上。
    let (card_id, card_pointers): (String, String) = copy
        .query_row("SELECT id, pointers_json FROM card", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!(card_pointers, format!("[\"fact:{fact_id}\"]"), "卡的指针不再指回 fact");
    let resolved: i64 = copy
        .query_row("SELECT count(*) FROM fact WHERE id=?1", [&fact_id], |r| r.get(0))
        .unwrap();
    assert_eq!(resolved, 1, "卡指针指向的 fact 不在副本里（悬空卡）: {card_id}");

    // 判据 2：fact 的出处行随 fact 一起留在副本里；卡沿指针能读到两条轮次。
    let fact_prov: i64 = copy
        .query_row(
            "SELECT count(*) FROM provenance WHERE mem_table='fact' AND mem_id=?1",
            [&fact_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fact_prov, 2, "副本删掉了 fact 的 provenance（卡将无出处可读）");
    let turns: Vec<String> = copy
        .prepare("SELECT turn_uid FROM provenance WHERE mem_table='fact' AND mem_id=?1 ORDER BY 1")
        .unwrap()
        .query_map([&fact_id], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(turns, vec![turn('a'), turn('b')], "副本里的 fact 出处轮次不对");

    // 中心库一行不动。
    assert_eq!(count(m.conn(), "fact"), 1, "build 改了中心库的 fact");
    assert_eq!(count(m.conn(), "anchor"), 2, "build 改了中心库的 anchor");
}

/// 判据 1 的反面：没有被 fact 引用的退场锚照旧从副本里删掉（豁免不许变成"什么都不删"）。
#[test]
fn a_retired_anchor_without_fact_references_is_still_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let keys = put_batch(m, &[anchor_item("只作废的锚", 'c'), card_item("活卡", 'd')], &ctx(), NOW)
        .unwrap();
    retire(m, &key(&keys, 0), "fixture", NOW + 1).unwrap();

    let snaps = dir.path().join("snapshots");
    let built = build(m, &snaps, NOW + 10, true).unwrap();
    let copy = rusqlite::Connection::open(&built.path).unwrap();
    assert_eq!(count(&copy, "anchor"), 0, "没有 fact 引用的退场锚应当删掉");
    assert_eq!(count(&copy, "card"), 1, "活卡不该被锚的过滤带走");
    assert_eq!(built.pruned.dead_rows, 1, "退场锚的删除计数不对：{:?}", built.pruned);
}

/// 判据 3：活卡的 `fact:` 指针落空 ⇒ 拒绝发布，且不留临时文件。
#[test]
fn a_live_card_pointing_at_a_missing_fact_refuses_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let keys = put_batch(m, &[card_item("悬空卡", '9')], &ctx(), NOW).unwrap();
    let card = key(&keys, 0);
    let missing_fact = format!("fact:{}", "0".repeat(64));
    m.conn()
        .execute(
            "UPDATE card SET pointers_json=?1 WHERE id=?2",
            rusqlite::params![format!("[\"{missing_fact}\"]"), card.id],
        )
        .unwrap();

    let snaps = dir.path().join("snapshots");
    let err = build(m, &snaps, NOW + 10, true).unwrap_err();
    match err {
        SnapshotError::DanglingCard(n) => assert_eq!(n, 1, "坏指针条数不对"),
        other => panic!("悬空卡没有被一致性检查拦下：{other:?}"),
    }
    // 失败产物不许留给 retain 当历史：目标目录里一个文件都没有。
    let left = std::fs::read_dir(&snaps)
        .map(|it| it.count())
        .unwrap_or(0);
    assert_eq!(left, 0, "拒绝发布后还留下了临时/正式文件：{left}");

    // 中心库不受影响（卡还在，指针仍是那条坏指针——审计看得见）。
    assert_eq!(count(m.conn(), "card"), 1, "拒绝发布动了中心库");
    let pointers: String = m
        .conn()
        .query_row("SELECT pointers_json FROM card WHERE id=?1", [&card.id], |r| r.get(0))
        .unwrap();
    assert_eq!(pointers, format!("[\"{missing_fact}\"]"));
}
