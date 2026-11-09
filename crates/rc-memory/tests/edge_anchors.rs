//! I4：derive 建边的锚门槛与归属统一。
//!
//! ① 共享 mention ⇒ 可建；`part_of` 只认这一种（审计 `c18cd965` 的收紧）；
//! ② 纯共现（无共享 mention、正文也无共同实体）⇒ 不建，返回 dropped；
//! ③ 别名文本锚（`agnes-ai` ↔ `Agnes AI`）⇒ 可建非 `part_of` 边；
//! ④ 边的 scope_kind 一律 project（即使调用方递了 entity scope）。

use rc_memory::keys::{Scope, ScopeKind};
use rc_memory::writers_x::{put_card, put_edges_anchored, put_entity, put_mentions, BatchCtx,
                           EdgeContent, EntityContent, FactContent, MentionContent};
use rc_memory::{AnchorRole, Provenance};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

fn scope() -> Scope { Scope { kind: ScopeKind::Project, key: "proj-anchor".into() } }
fn prov(c: char) -> Vec<Provenance> {
    vec![Provenance { turn_uid: c.to_string().repeat(64), anchor_role: AnchorRole::Human }]
}
fn ctx() -> BatchCtx { BatchCtx { source_agent: "rc-derive".into(), device_id: "unknown".into() } }

fn card(m: &rc_store::MemoryHandle, title: &str, body: &str, c: char) -> String {
    put_card(m, &FactContent {
        kind: "decision".into(), title: title.into(), body: body.into(),
        scope: scope(), valid_to_ms: None,
    }, &prov(c), NOW).unwrap().id
}

fn entity(m: &rc_store::MemoryHandle, title: &str, aliases: &[&str], c: char) -> String {
    put_entity(m, &EntityContent {
        kind: "concept".into(), title: title.into(), scope: scope(),
        aliases: aliases.iter().map(|a| a.to_string()).collect(),
    }, &prov(c), NOW).unwrap().id
}

fn edge(from: &str, to: &str, kind: &str) -> EdgeContent {
    EdgeContent {
        from_id: from.into(), to_id: to.into(), kind: kind.into(),
        reason: "合成理由".into(),
        // 故意递 entity 归属：写入口必须把 kind 强制成 project（I4 的统一 scope）。
        scope: Scope { kind: ScopeKind::Entity, key: "entity-scope-leak".into() },
        basis_ids: vec![],
    }
}

#[test]
fn a_shared_mention_is_an_anchor_and_the_edge_scope_is_forced_to_project() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let e = entity(m, "Agnes AI", &[], 'e');
    let a = card(m, "视觉绑定", "Agnes AI 提供视觉能力", 'a');
    let b = card(m, "回退链", "Agnes AI 仍在回退链里", 'b');
    // 共享 mention：两端都指向同一个实体（本批 mention 必须先落库——derive 侧已按此排序）。
    put_mentions(m, &[MentionContent { row_id: a.clone(), name: "Agnes AI".into() },
                      MentionContent { row_id: b.clone(), name: "Agnes AI".into() }],
                 &ctx(), NOW).unwrap();

    let (written, dropped) = put_edges_anchored(m, &[edge(&a, &b, "part_of")], &ctx(), NOW).unwrap();
    assert_eq!((written, dropped), (1, 0), "共享 mention ⇒ part_of 可建");
    let (sk, key): (String, String) = m.conn().query_row(
        "SELECT scope_kind, scope_key FROM fact_link", [],
        |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(sk, "project", "I4：边归属统一 project（调用方递 entity 也不许漏进来）");
    assert_eq!(key, "entity-scope-leak", "key 由调用方给，kind 由写入口盖章");
    // 收紧的第二层：目标是实体行时，from 必须直接提到它。两端都提到同一个第三方实体
    // 不够——审计 c18cd965 的定价卡只提到 `Agnes 3.0 Flash`，目标却是「…官方文档」。
    let doc = entity(m, "Agnes AI 官方文档", &[], 'd');
    put_mentions(m, &[MentionContent { row_id: a.clone(), name: "Agnes AI".into() },
                      MentionContent { row_id: doc.clone(), name: "Agnes AI".into() }],
                 &ctx(), NOW).unwrap();
    let (written2, dropped2) = put_edges_anchored(m, &[edge(&a, &doc, "part_of")], &ctx(), NOW).unwrap();
    assert_eq!((written2, dropped2), (0, 1),
        "只共享第三方实体、from 没有直接提到目标实体 ⇒ part_of 仍拒");
    let _ = e;
}

#[test]
fn pure_cooccurrence_is_dropped_not_written() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let a = card(m, "阈值已提高", "从 0.5 提高到 0.75", 'a');
    let b = card(m, "触发点", "约 384K 触发压缩", 'b');
    // 无实体、无 mention：同轮相邻句不是关系（审计 a844e223/d315d482 的那一型）。
    let (written, dropped) = put_edges_anchored(m, &[edge(&a, &b, "depends_on")], &ctx(), NOW).unwrap();
    assert_eq!((written, dropped), (0, 1), "纯共现不建边，dropped 必须可数");
    let n: i64 = m.conn().query_row("SELECT count(*) FROM fact_link", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "门槛之外的边一个字节都不许落");
}

#[test]
fn an_alias_text_anchor_is_recognized_but_does_not_lift_part_of() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let e = entity(m, "Agnes AI", &["agnes-ai"], 'e');
    // 正文只出现别名 `agnes-ai`，实体标题是 `Agnes AI` ⇒ 归一锚键必须认得出来。
    let a = card(m, "定价为零", "agnes-ai 的输入输出全免费", 'a');
    let b = card(m, "可达性", "agnes-ai 直连接得上", 'b');

    let (written, dropped) = put_edges_anchored(m, &[edge(&a, &b, "depends_on")], &ctx(), NOW).unwrap();
    assert_eq!((written, dropped), (1, 0), "别名文本锚 ⇒ depends_on 可建");
    let (written2, dropped2) = put_edges_anchored(m, &[edge(&e, &b, "part_of")], &ctx(), NOW).unwrap();
    assert_eq!((written2, dropped2), (0, 1),
        "part_of 只认共享 mention；仅靠标题包含（审计 c18cd965）不许过门槛");
}
