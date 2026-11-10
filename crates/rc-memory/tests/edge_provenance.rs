//! C20 的三条读数：两端 + 第三依据都进出处表、不存在的依据整条拒收（C31 的生产面证明）、
//! 依据被库外删掉时读侧明写 missing（防御性读数保留）。

use rc_memory::keys::{AnchorRole, Scope, ScopeKind};
use rc_memory::writers_x::{put_card, put_edges, put_entity, EdgeContent, EntityContent, FactContent};
use rc_memory::{reader, BatchCtx, MemTable, Provenance};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

fn scope() -> Scope { Scope { kind: ScopeKind::Project, key: "p-edge".into() } }
fn prov(c: char, role: AnchorRole) -> Vec<Provenance> {
    vec![Provenance { turn_uid: c.to_string().repeat(64), anchor_role: role }]
}
fn entity(root: &Root, title: &str, c: char, role: AnchorRole) -> String {
    put_entity(root.memory(), &EntityContent {
        kind: "concept".into(), title: title.into(), scope: scope(), aliases: vec![],
    }, &prov(c, role), NOW).unwrap().id
}

#[test]
fn 边的两端与第三条依据都进出处表_而边仍不进事实面() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let a = entity(&root, "alpha", 'a', AnchorRole::Human);
    let b = entity(&root, "beta", 'b', AnchorRole::Agent);
    let c = put_card(m, &FactContent {
        kind: "decision".into(), title: "依据卡".into(), body: "依据。".into(),
        scope: scope(), valid_to_ms: None }, &prov('c', AnchorRole::Human), NOW).unwrap().id;
    let edge = put_edges(m, &[EdgeContent {
        from_id: a.clone(), to_id: b.clone(), kind: "part_of".into(),
        reason: "beta 是 alpha 的前置。".into(), scope: scope(), basis_ids: vec![c.clone()],
    }], &BatchCtx::synthetic(), NOW).unwrap();
    assert_eq!(edge, 1);

    let mut st = m.conn().prepare(
        "SELECT ref_kind, turn_uid, anchor_role FROM provenance
         WHERE mem_table='fact_link' ORDER BY turn_uid").unwrap();
    let got: Vec<(String, String, String)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap().map(|x| x.unwrap()).collect();
    assert_eq!(got.len(), 3, "from + to + 一条依据 = 三行：{got:?}");
    assert!(got.contains(&("anchor".into(), a.clone(), "human".into())), "from 端必须可回读");
    assert!(got.contains(&("anchor".into(), b.clone(), "human".into())),
            "to 端必须进出处表，且角色取两端最强（Human 压过 Agent）");
    assert!(got.contains(&("card".into(), c.clone(), "human".into())), "第三条依据必须进出处表");

    // 裁定 3：边不是事实行。写边不得改变 facts() 的行数。
    assert_eq!(reader::facts(m).unwrap().len(), 3,
               "两实体 + 一卡 = 3；边不许混进事实面（ROW_TABLES 保持四张）");
}

#[test]
fn 不存在的依据整条拒收_一行都不落() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let a = entity(&root, "alpha2", 'd', AnchorRole::Human);
    let b = entity(&root, "beta2", 'e', AnchorRole::Agent);
    let ghost = "f".repeat(64);
    let err = put_edges(m, &[EdgeContent {
        from_id: a, to_id: b, kind: "part_of".into(), reason: "x".into(),
        scope: scope(), basis_ids: vec![ghost],
    }], &BatchCtx::synthetic(), NOW);
    assert!(err.is_err(), "不存在的依据必须让整批失败");
    let rel: i64 = m.conn().query_row("SELECT count(*) FROM fact_link", [], |r| r.get(0)).unwrap();
    // 只数边那一族：本夹具的两个实体各带一行 `mem_table='entity'` 的出处行（`put_entity` 必带
    // 出处是既有形状），它们与"边有没有落半截"无关；全表 `count(*)` 恒 ≥ 2，断不出回滚。
    let prov: i64 = m.conn().query_row(
        "SELECT count(*) FROM provenance WHERE mem_table='fact_link'", [], |r| r.get(0)).unwrap();
    assert_eq!((rel, prov), (0, 0),
               "C31：生产路径造不出 dangling basis —— 表已在事务里回滚");
}

#[test]
fn 依据行被库外删掉后读侧明写missing_不渲第二跳() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let a = entity(&root, "alpha3", 'h', AnchorRole::Human);
    let b = entity(&root, "beta3", 'i', AnchorRole::Agent);
    let c = put_card(m, &FactContent {
        kind: "decision".into(), title: "会被删的依据".into(), body: "依据。".into(),
        scope: scope(), valid_to_ms: None }, &prov('j', AnchorRole::Human), NOW).unwrap().id;
    put_edges(m, &[EdgeContent {
        from_id: a, to_id: b, kind: "part_of".into(), reason: "x".into(),
        scope: scope(), basis_ids: vec![c.clone()],
    }], &BatchCtx::synthetic(), NOW).unwrap();
    let edge_id: String = m.conn().query_row("SELECT id FROM fact_link", [], |r| r.get(0)).unwrap();
    // 库外删除：配方/G4 之外的第二种来源，正是 C31 说的"防御性读数"。
    m.conn().execute("DELETE FROM card WHERE id=?1", [&c]).unwrap();
    let steps = reader::edge_provenance(m, &edge_id).unwrap();
    assert!(steps.iter().any(|s| matches!(s,
        rc_memory::ProvenanceStep::Basis { ref_kind, target_id, basis_exists: false, .. }
        if ref_kind == "card" && target_id == &c)), "依据不在必须明写 missing：{steps:?}");
    assert!(!steps.iter().any(|s| matches!(s,
        rc_memory::ProvenanceStep::BasisTurn { turn_uid, .. } if turn_uid == &"j".repeat(64))),
        "没有'然后'就不能报：missing 的依据不许渲第二跳");
    let _ = (MemTable::Card, NOW); // 保持 import 与常量在红灯阶段也被用到
}
