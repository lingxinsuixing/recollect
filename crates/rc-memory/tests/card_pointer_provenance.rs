//! 召回可追溯性（审计 2026-10-27）：`card.pointers_json` → `fact` → `turn` 的读侧兜底。
//!
//! 2026-10-30 起**镜像卡自带出处**（P0-2）：`put_fact_rows` 除 `mem_table='fact'` 外，
//! 还给卡写同一组证据轮（`mem_table='card'`，`ref_kind='turn'`）。理由是推导池的口径
//! "行自身要有 turn 出处"——只写 fact 名下时，推导看不见任何事实/事件行。
//! 因此指针兜底今天是**旧形状/半写库**的读路：只在卡自己零出处时才走。
//! - 优先读卡自己的 provenance 行，按 turn_uid 升序；
//! - 零行时沿 `pointers_json` 读 fact 的 provenance；再没有就用 `fact.evidence_turn_uid` 兜底；
//! - 只产 `Turn` 步骤，不编造 basis；坏指针 / 无指针 ⇒ 空链，不 panic。
//!
//! 夹具只走公开写入口（`put_entity` / `put_card` / `put_fact_rows`）；只有"制造坏指针"
//! 与"删掉 fact 出处行"两处用 `m.conn()`（`recall_provenance.rs` 的同形先例）。

use rc_memory::keys::{AnchorRole, MemKey, MemTable, Scope, ScopeKind};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::reader::{provenance_chain, row_participants, rows_since};
use rc_memory::writers_fact::{put_fact_rows, FactWriteItem};
use rc_memory::writers_x::{put_card, put_entity, BatchCtx, EntityContent, FactContent};
use rc_memory::{Provenance, ProvenanceStep};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;
const SRC_MS: i64 = 1_699_999_000_000;

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "p-card-prov".into() }
}

fn uid(index: usize) -> String {
    format!("{:064x}", 0x21 + index)
}

fn human(turn: char) -> Vec<Provenance> {
    vec![Provenance { turn_uid: turn.to_string().repeat(64), anchor_role: AnchorRole::Human }]
}

fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-card-prov".into()),
        owner_agent_id: "agent-owner".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}

fn entity(root: &Root, title: &str, turn: char) -> String {
    put_entity(root.memory(), &EntityContent {
        kind: "person".into(), title: title.into(), scope: scope(), aliases: vec![],
    }, &human(turn), NOW).unwrap().id
}

fn fact_item(anchor_id: &str, text: &str, evidence: Vec<(String, AnchorRole)>) -> FactWriteItem {
    FactWriteItem {
        subject_anchor_id: anchor_id.to_string(),
        object_anchor_id: None,
        predicate: "note".into(),
        value: Some("某件事".into()),
        category: "observation".into(),
        mode: "stated".into(),
        time_text: String::new(),
        quote: String::new(),
        evidence,
        text: text.to_string(),
        scope: scope(),
        source_ms: SRC_MS,
    }
}

/// 写一条 fact（同时落一张最小 card），返回 `(fact_id, card_id)`。
fn write_fact(root: &Root, anchor_id: &str, text: &str,
              evidence: Vec<(String, AnchorRole)>) -> (String, String) {
    let m = root.memory();
    let item = fact_item(anchor_id, text, evidence);
    let out = put_fact_rows(m, std::slice::from_ref(&item), &BatchCtx::synthetic(),
                            &owner(), NOW).unwrap();
    assert_eq!((out.facts, out.cards), (1, 1), "一条 fact 必须落 fact + card 各一行");
    let (fact_id, pointers): (String, String) = m.conn().query_row(
        "SELECT f.id, c.pointers_json FROM fact AS f JOIN card AS c
           ON c.pointers_json = '[\"fact:' || f.id || '\"]' WHERE f.text=?1",
        [text], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    let card_id: String = m.conn().query_row(
        "SELECT id FROM card WHERE pointers_json=?1", [&pointers], |r| r.get(0)).unwrap();
    (fact_id, card_id)
}

fn card_key(card_id: &str) -> MemKey {
    MemKey { table: MemTable::Card, id: card_id.to_string() }
}

#[test]
fn card_chain_walks_pointers_to_fact_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let subject = entity(&root, "指针卡主体", 'a');
    let (fact_id, card_id) = write_fact(&root, &subject, "指针卡正文甲",
        vec![(uid(0), AnchorRole::Human), (uid(1), AnchorRole::Agent)]);

    let steps = provenance_chain(m, &card_key(&card_id)).unwrap();
    assert_eq!(steps.len(), 2, "card 的出处必须沿 pointers_json 走一跳：{steps:?}");
    let got: Vec<(String, AnchorRole)> = steps.iter().map(|s| match s {
        ProvenanceStep::Turn { turn_uid, role } => (turn_uid.clone(), *role),
        other => panic!("card 兜底只许产 Turn，不许编造 basis：{other:?}"),
    }).collect();
    assert_eq!(got, vec![(uid(0), AnchorRole::Human), (uid(1), AnchorRole::Agent)],
               "fact 的 provenance 行按 turn_uid 升序读回：{fact_id}");

    // 指针里指的不是这张卡自己的 fact 时也不许串行。
    assert!(steps.iter().all(|s| matches!(s, ProvenanceStep::Turn { .. })));
}

#[test]
fn card_chain_dedupes_shared_turns_across_pointed_facts() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let subject = entity(&root, "去重卡主体", 'b');
    let (fact_a, card_id) = write_fact(&root, &subject, "去重卡正文甲",
        vec![(uid(2), AnchorRole::Human), (uid(3), AnchorRole::Agent)]);
    let (fact_b, _) = write_fact(&root, &subject, "去重卡正文乙",
        vec![(uid(3), AnchorRole::Agent), (uid(4), AnchorRole::Human)]);
    // 这一支测的是**旧形状**（卡自己零出处）的指针兜底；新写入的镜像卡自带出处，
    // 所以先删掉它，才能走到 pointers 那一跳。
    m.conn().execute(
        "DELETE FROM provenance WHERE mem_table='card' AND mem_id=?1", [&card_id]).unwrap();
    // 公开写入口只写单 fact 指针；多指针形状由审计报告给出，这里直接改成它。
    m.conn().execute(
        "UPDATE card SET pointers_json=?1 WHERE id=?2",
        rusqlite::params![format!("[\"fact:{fact_a}\",\"fact:{fact_b}\"]"), card_id],
    ).unwrap();

    let steps = provenance_chain(m, &card_key(&card_id)).unwrap();
    let uids: Vec<&str> = steps.iter().map(|s| match s {
        ProvenanceStep::Turn { turn_uid, .. } => turn_uid.as_str(),
        other => panic!("只许 Turn：{other:?}"),
    }).collect();
    assert_eq!(uids, vec![uid(2).as_str(), uid(3).as_str(), uid(4).as_str()],
               "跨 fact 的同一轮只留一次，且保持指针顺序：{steps:?}");
}

#[test]
fn card_chain_falls_back_to_evidence_turn_when_fact_has_no_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let subject = entity(&root, "兜底卡主体", 'c');
    let (fact_id, card_id) = write_fact(&root, &subject, "兜底卡正文",
        vec![(uid(5), AnchorRole::Human)]);
    // 制造"fact 有 evidence_turn_uid、但 fact 与 card 的 provenance 都是零行"的形状
    // （审计里 134 行全有 provenance，这一支是历史库/半写库的防御性读数）。
    m.conn().execute(
        "DELETE FROM provenance WHERE mem_table='card' AND mem_id=?1", [&card_id]).unwrap();
    m.conn().execute(
        "DELETE FROM provenance WHERE mem_table='fact' AND mem_id=?1", [&fact_id]).unwrap();

    let steps = provenance_chain(m, &card_key(&card_id)).unwrap();
    assert_eq!(steps.len(), 1, "没有 provenance 行时必须用 evidence_turn_uid 兜底：{steps:?}");
    match &steps[0] {
        ProvenanceStep::Turn { turn_uid, role } => {
            assert_eq!(turn_uid, &uid(5));
            // fact 的 speaker_anchor_id 是 NULL ⇒ 推不出角色，落 Agent（兜底，不是事实）。
            assert_eq!(*role, AnchorRole::Agent, "推不出角色时的兜底值是 Agent");
        }
        other => panic!("只许 Turn：{other:?}"),
    }
}

#[test]
fn card_chain_without_pointers_or_with_bad_pointers_is_empty_and_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let card = put_card(m, &FactContent {
        kind: "note".into(), title: "无指针卡".into(), body: "这张卡没有 fact 指针。".into(),
        scope: scope(), valid_to_ms: None,
    }, &human('d'), NOW).unwrap();
    // put_card 会写直接出处；删掉它，让兜底成为唯一读路（pointers_json 的 DEFAULT 是 '[]'）。
    m.conn().execute(
        "DELETE FROM provenance WHERE mem_table='card' AND mem_id=?1", [&card.id]).unwrap();
    assert!(provenance_chain(m, &card).unwrap().is_empty(),
            "无 pointers 的 card 必须返回空链");

    for bad in ["[oops", "[\"fact:\"]", "[\"card:abc\"]", "[\"fact:not-a-real-id\"]", "{}"] {
        m.conn().execute("UPDATE card SET pointers_json=?1 WHERE id=?2",
                         rusqlite::params![bad, card.id]).unwrap();
        let steps = provenance_chain(m, &card).unwrap();
        assert!(steps.is_empty(), "坏 pointers {bad:?} 必须返回空链、不许 panic：{steps:?}");
    }
}

/// P0-2（2026-10-30）：镜像卡自带 turn 出处，并且因此进入推导池。
///
/// 改前：fact/event 的镜像卡在 `provenance` 里零行（出处只写在 `mem_table='fact'` 名下），
/// 而推导池要求"行自身有 `ref_kind='turn'` 出处" ⇒ 推导看不见任何事实/事件行。
#[test]
fn mirror_card_carries_its_own_provenance_and_enters_the_derive_pool() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let subject = entity(&root, "推导池主体", 'e');
    let (_fact_id, card_id) = write_fact(&root, &subject, "推导池正文",
        vec![(uid(6), AnchorRole::Human)]);

    let owned: i64 = m.conn().query_row(
        "SELECT count(*) FROM provenance
          WHERE mem_table='card' AND mem_id=?1 AND ref_kind='turn'",
        [&card_id], |r| r.get(0)).unwrap();
    assert_eq!(owned, 1, "镜像卡必须带自己的 turn 出处（不再只靠 pointers 兜底）");

    let rows = rows_since(m, None, 100).unwrap();
    assert!(
        rows.iter().any(|r| r.key.table == MemTable::Card && r.key.id == card_id),
        "推导池必须包含事实/事件的镜像卡：{rows:?}"
    );
}

/// P0-3b（2026-10-30）：推导面的参与者正式名从 `fact.participants_json` 映射而来。
#[test]
fn participants_of_a_mirror_card_resolve_to_anchor_names() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let subject = entity(&root, "参与者甲", 'f');
    let other = entity(&root, "参与者乙", 'g');
    let (fact_id, card_id) = write_fact(&root, &subject, "参与者正文",
        vec![(uid(7), AnchorRole::Human)]);
    m.conn().execute(
        "UPDATE fact SET participants_json=?1 WHERE id=?2",
        rusqlite::params![format!("[\"{subject}\",\"{other}\",\"missing-anchor\"]"), fact_id]).unwrap();

    let row = rows_since(m, None, 100).unwrap()
        .into_iter().find(|r| r.key.id == card_id).expect("镜像卡进池");
    let names = row_participants(m, &row).unwrap();
    assert_eq!(names, vec!["参与者甲".to_string(), "参与者乙".to_string()],
               "按 participants_json 顺序映射正式名，坏 id 少不多");
}
