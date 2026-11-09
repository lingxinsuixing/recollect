//! C20 的导出面：边段里两端与第三条依据都读得回来，且边的行记号不搅浑事实行计数。

use rc_agent::export::{render_export, ExportRequest};
use rc_memory::keys::{AnchorRole, Scope, ScopeKind};
use rc_memory::writers_x::{put_card, put_edges, put_entity, EdgeContent, EntityContent, FactContent};
use rc_memory::{BatchCtx, Provenance};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

#[test]
fn 导出件_边的两端与依据都读得回来() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let scope = Scope { kind: ScopeKind::Project, key: "p-export-edge".into() };
    let prov = |c: char, role| vec![Provenance { turn_uid: c.to_string().repeat(64), anchor_role: role }];
    let a = put_entity(m, &EntityContent { kind: "concept".into(), title: "alpha".into(),
        scope: scope.clone(), aliases: vec![] }, &prov('a', AnchorRole::Human), NOW).unwrap().id;
    let b = put_entity(m, &EntityContent { kind: "concept".into(), title: "beta".into(),
        scope: scope.clone(), aliases: vec![] }, &prov('b', AnchorRole::Agent), NOW).unwrap().id;
    let c = put_card(m, &FactContent { kind: "decision".into(), title: "依据卡".into(),
        body: "依据。".into(), scope: scope.clone(), valid_to_ms: None },
        &prov('c', AnchorRole::Human), NOW).unwrap().id;
    put_edges(m, &[EdgeContent { from_id: a.clone(), to_id: b.clone(), kind: "part_of".into(),
        reason: "beta 是 alpha 的前置。".into(), scope, basis_ids: vec![c.clone()] }],
        &BatchCtx::synthetic(), NOW).unwrap();
    let edge_id: String = m.conn().query_row("SELECT id FROM fact_link", [], |r| r.get(0)).unwrap();

    let text = render_export(m, &ExportRequest::all(NOW).unwrap()).unwrap().text();
    assert!(text.contains("## edges"), "边段没渲出来：{text}");
    assert!(text.contains(&format!("* `relationship:{}` anchor:{} -> anchor:{} (part_of",
                                   edge_id, a, b)), "{text}");
    assert!(text.contains(&format!("    ← anchor:{a} (human)")), "from 端没渲：{text}");
    assert!(text.contains(&format!("    ← anchor:{b} (human)")),
            "to 端没渲（角色取两端最强 Human）：{text}");
    assert!(text.contains(&format!("    ← card:{c} (human)")), "第三条依据没渲：{text}");
    assert!(text.contains("因为 beta 是 alpha 的前置。"), "{text}");
}
