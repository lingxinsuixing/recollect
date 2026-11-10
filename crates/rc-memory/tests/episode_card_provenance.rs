//! 债 #23（派发档 I-9）：episode 镜像卡的出处链里，依据步 `ref_kind='fact'` 修前被
//! `MemTable::parse` 认不出（`keys.rs` 只有 `Anchor|Card` 两个变体）而落进兜底、恒报
//! `basis_exists=false`——依据步（真 fact 行）明明存在却自称 missing。
//!
//! 本文件钉三格：
//! 1. **正例**：真生产者 `put_fact_rows_with_extras` + `put_episodes` 落下的镜像卡，
//!    出处链里每条成员依据 `basis_exists=true`，且紧跟它自己的 `BasisTurn`
//!    （成员行自己的 `ref_kind='turn'` 出处；与 card/anchor 依据步同口径）；
//! 2. **阴性对照**：依据行真不在（库外删掉 fact 行，`edge_provenance.rs` 的
//!    `依据行被库外删掉后读侧明写missing_不渲第二跳` 用例的同形先例）
//!    ⇒ 仍 `false`、不渲第二跳——修法不许把"不存在"也读成 true；
//! 3. **既有形状不回退**：derive 链卡（basis=card）、锚依据（edge 的 anchor 端）的
//!    `basis_exists=true` 原样绿。
//!
//! 夹具纪律照 `tests/episode.rs`：行只走公开写入口；"依据不在"是库外删除
//!（生产写入口造不出 dangling basis——`put_episodes` 成员准入要求行在库）。

use rc_memory::identity::compute_fact_id;
use rc_memory::keys::{AnchorRole, MemKey, MemTable, Scope, ScopeKind};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::reader::{edge_provenance, provenance_chain};
use rc_memory::writers_x::{
    put_card, put_chains, put_edges, put_entity, put_episodes, BatchCtx, ChainContent, EdgeContent,
    EntityContent, EpisodeContent, FactContent,
};
use rc_memory::{put_fact_rows_with_extras, FactExtras, FactWriteItem, ProvenanceStep};
use rc_store::{MemoryHandle, Root};

const NOW: i64 = 1_700_000_000_000;
const SRC0: i64 = 1_699_999_000_000;

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-ep-prov".into() }
}

fn ctx() -> BatchCtx {
    BatchCtx { source_agent: "agent-ep-prov".into(), device_id: "dev-ep-prov".into() }
}

fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-ep-prov".into()),
        owner_agent_id: "agent-owner".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}

/// 锚行的 canonical：`seed_anchor` 的 INSERT 与句柄派生共用这一串。
const NAMES: [&str; 3] = ["甲某", "乙某", "丙某"];

fn anchor(index: usize) -> String {
    rc_memory::compute_mem_id(MemTable::Anchor, "person", NAMES[index], "", &scope())
}

fn turn(index: usize) -> String {
    format!("{:064x}", 0x31 + index)
}

fn seed_anchor(m: &MemoryHandle, id: &str, canonical: &str, scope_key: &str) {
    m.conn().execute(
        "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                            status, created_at_ms, updated_at_ms, norm_version)
         VALUES(?1, 'person', ?2, 'real', 'project', ?3, 'active', ?4, ?4, 1)",
        rusqlite::params![id, canonical, scope_key, SRC0],
    ).unwrap();
}

fn seeded() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    for (index, name) in NAMES.iter().enumerate() {
        seed_anchor(root.memory(), &anchor(index), name, "proj-ep-prov");
    }
    (dir, root)
}

/// 一条真 fact 行（走真生产者 ⇒ 自带 `ref_kind='turn'` 出处），返回它的 id。
/// 事件时间不回填：成员准入只看出处与死活，`event_order` 的序不影响本文件的断言
///（断言按 `target_id` 找 Basis，不按位置）。
fn fact(m: &MemoryHandle, index: usize, value: &str) -> String {
    let text = format!("甲某{value}");
    let item = FactWriteItem {
        subject_anchor_id: anchor(0),
        object_anchor_id: None,
        predicate: "做了".into(),
        value: Some(value.into()),
        category: "event".into(),
        mode: "stated".into(),
        time_text: String::new(),
        quote: value.into(),
        evidence: vec![(turn(index), AnchorRole::Human)],
        text: text.clone(),
        scope: scope(),
        source_ms: SRC0,
    };
    let id = compute_fact_id(&anchor(0), "做了", None, Some(value), "event", "", &text, &scope());
    let out = put_fact_rows_with_extras(
        m, &[item], &[FactExtras::default()], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(out.facts, 1, "夹具的 fact 必须真的落库：{value}");
    id
}

/// 两成员的经历，返回（镜像卡 id，两个成员 fact id）。
fn episode_with(m: &MemoryHandle, values: [&str; 2]) -> (String, [String; 2]) {
    let ids = [fact(m, 0, values[0]), fact(m, 1, values[1])];
    put_episodes(m, &[EpisodeContent {
        title: "一段经历".into(),
        narrative: "甲某先调通闸，乙某补了桩。".into(),
        fact_ids: ids.to_vec(),
    }], &ctx(), NOW).unwrap();
    let card_id: String = m.conn().query_row(
        "SELECT id FROM card WHERE kind='episode'", [], |r| r.get(0)).unwrap();
    (card_id, ids)
}

/// 正例：镜像卡出处链里每条成员依据 `basis_exists=true`，
/// 且紧跟它自己的 `BasisTurn`（成员行自己的 `ref_kind='turn'` 出处）。
#[test]
fn 镜像卡的fact依据步报存在并紧跟成员自己的出处轮() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let (card_id, ids) = episode_with(m, ["调通了闸", "补了桩"]);

    let steps = provenance_chain(m, &MemKey { table: MemTable::Card, id: card_id }).unwrap();
    assert_eq!(steps.len(), 4, "两成员 = 两条 Basis + 各一条 BasisTurn：{steps:?}");
    for (fact_id, evidence_uid) in ids.iter().zip([turn(0), turn(1)]) {
        let pos = steps.iter().position(|s| matches!(s,
            ProvenanceStep::Basis { ref_kind, target_id, basis_exists: true, .. }
            if ref_kind == "fact" && target_id == fact_id))
            .unwrap_or_else(|| panic!("成员 {fact_id} 的依据步必须报 basis_exists=true：{steps:?}"));
        assert!(matches!(&steps[pos],
            ProvenanceStep::Basis { role: AnchorRole::Human, .. }),
            "依据步的锚角色取成员行自己出处的最强值（夹具里是 human）：{steps:?}");
        assert!(matches!(&steps[pos + 1],
            ProvenanceStep::BasisTurn { turn_uid, role }
            if turn_uid == &evidence_uid && *role == AnchorRole::Human),
            "Basis 必须紧跟成员自己的证据轮（没有'然后'才不报）：{steps:?}");
    }
}

/// 阴性对照：依据行真的不在（库外删掉 fact 行）⇒ 仍 `false`、不渲第二跳。
/// `provenance` 没有指向 fact 的外键，删除后依据步还在链里、只是它指的行没了。
#[test]
fn 依据行真不在时镜像卡仍报missing且不渲第二跳() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let (card_id, ids) = episode_with(m, ["会被删的一条", "留下的一条"]);
    let gone = &ids[0];
    m.conn().execute("DELETE FROM fact WHERE id=?1", [gone]).unwrap();

    let steps = provenance_chain(m, &MemKey { table: MemTable::Card, id: card_id }).unwrap();
    // 本条只守"missing 语义"这一格（变异证明要求：fact 分支改回兜底时恰红的是正例那条，
    // 本条必须原样绿）——幸存成员的形状归正例守，这里不碰。
    assert!(steps.iter().any(|s| matches!(s,
        ProvenanceStep::Basis { ref_kind, target_id, basis_exists: false, .. }
        if ref_kind == "fact" && target_id == gone)),
        "不在的成员必须仍明写 basis_exists=false：{steps:?}");
    assert!(!steps.iter().any(|s| matches!(s,
        ProvenanceStep::BasisTurn { turn_uid, .. } if turn_uid == &turn(0))),
        "missing 的依据不许渲第二跳（被删成员的证据轮是 {}）：{steps:?}", turn(0));
}

/// 既有形状不回退：derive 链卡（basis=card）与锚依据（edge 的 anchor 端）修前后同绿。
#[test]
fn 链卡与锚依据的basis_exists仍为true() {
    let (_dir, root) = seeded();
    let m = root.memory();
    // ① derive 链卡：一环节一依据，依据是张 card（`put_chains` 的 `basis_ids` 收裸 id）。
    let link = put_card(m, &FactContent {
        kind: "decision".into(), title: "环节卡".into(), body: "环节这一行。".into(),
        scope: scope(), valid_to_ms: None,
    }, &[rc_memory::Provenance {
        turn_uid: turn(0), anchor_role: AnchorRole::Human }], NOW).unwrap().id;
    let basis = put_card(m, &FactContent {
        kind: "decision".into(), title: "依据卡".into(), body: "凭什么这一行。".into(),
        scope: scope(), valid_to_ms: None,
    }, &[rc_memory::Provenance {
        turn_uid: turn(1), anchor_role: AnchorRole::Human }], NOW).unwrap().id;
    put_chains(m, &[ChainContent {
        mechanism: "环节先于依据是这条链的机制句".into(),
        link_ids: vec![link.clone()],
        basis_ids: vec![basis.clone()],
        causal_type: "causes".into(),
        scope: scope(),
    }], &BatchCtx::synthetic(), NOW).unwrap();
    let head: String = m.conn().query_row(
        "SELECT id FROM card WHERE kind='causal'", [], |r| r.get(0)).unwrap();
    let steps = provenance_chain(m, &MemKey { table: MemTable::Card, id: head }).unwrap();
    let pos = steps.iter().position(|s| matches!(s,
        ProvenanceStep::Basis { ref_kind, target_id, basis_exists: true, .. }
        if ref_kind == "card" && target_id == &basis))
        .unwrap_or_else(|| panic!("card 依据步必须仍是 basis_exists=true：{steps:?}"));
    assert!(matches!(&steps[pos + 1],
        ProvenanceStep::BasisTurn { turn_uid, .. } if turn_uid == &turn(1)),
        "card 依据的第二跳照旧：{steps:?}");
    // ② 锚依据：边两端是 anchor，`edge_provenance` 的 Basis 步 `ref_kind='anchor'`。
    let human = |c: char| vec![rc_memory::Provenance {
        turn_uid: c.to_string().repeat(64), anchor_role: AnchorRole::Human }];
    let a = put_entity(m, &EntityContent {
        kind: "concept".into(), title: "锚甲".into(), scope: scope(), aliases: vec![],
    }, &human('p'), NOW).unwrap().id;
    let b = put_entity(m, &EntityContent {
        kind: "concept".into(), title: "锚乙".into(), scope: scope(), aliases: vec![],
    }, &human('q'), NOW).unwrap().id;
    put_edges(m, &[EdgeContent {
        from_id: a.clone(), to_id: b.clone(), kind: "part_of".into(),
        reason: "锚乙是锚甲的前置。".into(), scope: scope(), basis_ids: vec![],
    }], &BatchCtx::synthetic(), NOW).unwrap();
    let edge_id: String = m.conn().query_row("SELECT id FROM fact_link", [], |r| r.get(0)).unwrap();
    let steps = edge_provenance(m, &edge_id).unwrap();
    for anchor_id in [&a, &b] {
        assert!(steps.iter().any(|s| matches!(s,
            ProvenanceStep::Basis { ref_kind, target_id, basis_exists: true, .. }
            if ref_kind == "anchor" && *target_id == *anchor_id)),
            "anchor 依据步必须仍是 basis_exists=true：{anchor_id} ⇒ {steps:?}");
    }
}
