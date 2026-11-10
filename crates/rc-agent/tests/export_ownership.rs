// crates/rc-agent/tests/export_ownership.rs
//! S1b-C4b1：`recollect_export` 是**人类面**，它过读闸（设计 ownership-persistence §4.2）。
//!
//! 这一层钉的是"接上了没有"与"接的形状对不对"，不是读闸规则本身（规则矩阵在
//! `rc-memory/tests/gate.rs`）：
//! ① 被拒的行**不出现、不解释**——导出件里既没有它的标题/正文，也没有"无权限"之类的说法；
//! ② `limit` 与"还有 N 行未列出"、"M rows listed of N"三处读数**全部按可见集算**：
//!    被闸掉的行不进"未列出"的账（那是"我能要却没给"，不是"我看不见"）；
//! ③ `viewer=None`（中心 CLI/旧测试）下旧行为一个字不变；
//! ④ public 行对另一个 agent 的 Human 面也可见（闸门不是全拒的反向读数）。
//!
//! 全部用真库：三行由 `put_batch` 分别以 agent-A / agent-B 署名写入，归属行随行落库
//! （C3 的写入继承），期望值不靠被过滤的那份输出反推。

use rc_memory::gate::Viewer;
use rc_memory::keys::{MemKey, MemTable};
use rc_memory::writers_x::{put_batch, BatchCtx, WriteItem};
use rc_memory::{AnchorRole, Provenance, Scope, ScopeKind};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

fn ctx(agent: &str) -> BatchCtx {
    BatchCtx { source_agent: agent.into(), device_id: format!("dev-{agent}") }
}

fn card(title: &str, body: &str) -> WriteItem {
    WriteItem {
        table: MemTable::Card,
        kind: "note".into(),
        title: title.into(),
        body: body.into(),
        // 同 scope 是刻意的：跨 scope 的可见性另有读者，这里要断的只有**归属**这一维。
        scope: Scope { kind: ScopeKind::Project, key: "proj-export-own".into() },
        valid_to_ms: None,
        aliases: vec![],
        prov: vec![Provenance { turn_uid: "e".repeat(64), anchor_role: AnchorRole::Human }],
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: Root,
    /// agent-A 的两行（`limit` 的账要两个数才分得清"可见集"与"全库"）。
    a: Vec<MemKey>,
    /// agent-B 的一行（本文件的"别人的 private 行"）。
    b: MemKey,
    /// 每行独有的正文金丝雀：断言"某个字出现了"与"某个字一个字都不出现"都要有牙。
    body_a: &'static str,
    body_b: &'static str,
}

fn seeded() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let body_a = "导出甲这一行独有的正文-KE71A";
    let body_b = "导出乙这一行独有的正文-KE71B";
    let a = put_batch(
        m,
        &[card("导出归属甲一", body_a), card("导出归属甲二", body_a)],
        &ctx("agent-A"),
        NOW,
    )
    .unwrap()
    .into_iter()
    .map(|k| k.expect("A 的行不被抑制"))
    .collect::<Vec<_>>();
    let b = put_batch(m, &[card("导出归属乙", body_b)], &ctx("agent-B"), NOW).unwrap()[0]
        .clone()
        .expect("B 的行不被抑制");
    assert_eq!(a.len(), 2, "前置：A 的两行都要在（内容寻址按标题区分）");
    Fixture { _dir: dir, root, a, b, body_a, body_b }
}

fn export(fx: &Fixture, limit: Option<usize>, viewer: Option<Viewer>) -> String {
    rc_agent::export::render_export(
        fx.root.memory(),
        &rc_agent::export::ExportRequest { now_ms: NOW, limit, viewer },
    )
    .unwrap()
    .text()
}

/// Human 面：A 的导出件里有自己的两行，**没有** B 的任何一个字，也不解释为什么没有。
#[test]
fn the_export_shows_only_the_viewers_rows() {
    let fx = seeded();
    let text = export(&fx, None, Some(Viewer::human("agent-A")));
    for k in &fx.a {
        assert!(text.contains(&k.key_string()), "自己的行没进导出件：{} ⇒ {text}", k.key_string());
    }
    assert!(text.contains("导出归属甲一") && text.contains("导出归属甲二"), "{text}");
    assert!(text.contains(fx.body_a), "自己的正文没进导出件：{text}");
    assert!(!text.contains("导出归属乙"), "被闸掉的行仍然出现在导出件里：{text}");
    assert!(!text.contains(fx.body_b), "被闸掉的正文仍然出现在导出件里：{text}");
    // **不解释**：被拒行不许在导出件里留下任何痕迹（它是"没有"，不是"不给你"）。
    assert!(!text.contains("无权限") && !text.contains("未授权") && !text.contains("无权"), "{text}");
    // 口径：总数与列出数都按**可见集**算（2，不是库里的 3）。
    assert!(text.contains("2 rows listed of 2."), "读数列的口径不是可见集：{text}");
    assert!(!text.contains("未列出"), "全量导出不该报未列出：{text}");
}

/// `limit` 与"还有 N 行未列出"按可见集算：可见 2 行、只给 1 行 ⇒ 未列出 1（不是 2）。
#[test]
fn the_limit_and_unlisted_counts_follow_the_visible_set() {
    let fx = seeded();
    let text = export(&fx, Some(1), Some(Viewer::human("agent-A")));
    assert!(text.contains("还有 1 行未列出"), "未列出数把看不见的也算进去了：{text}");
    assert!(text.contains("1 rows listed of 2."), "读数列的口径不是可见集：{text}");
    assert!(!text.contains("导出归属乙") && !text.contains(fx.body_b), "{text}");
}

/// `viewer=None`（中心 CLI/旧测试）：三行都出，旧读数一分不动。
#[test]
fn without_a_viewer_the_export_keeps_its_old_behaviour() {
    let fx = seeded();
    let text = export(&fx, None, None);
    for title in ["导出归属甲一", "导出归属甲二", "导出归属乙"] {
        assert!(text.contains(title), "viewer=None 时 `{title}` 不在导出件里：{text}");
    }
    assert!(text.contains(fx.body_a) && text.contains(fx.body_b), "{text}");
    assert!(text.contains("3 rows listed of 3."), "{text}");
    assert!(!text.contains("未列出"), "{text}");
}

/// 一条 `fact_link` 边：两端是 `"<table>:<id>"` 键。边**不是**事实行、没有归属行，
/// 它的可见性只能由两端的归属决定（C4b2b）。直接 SQL 插入——本切片不新增生产 API。
/// `rusqlite` 不在 rc-agent 的直接依赖里（守卫 11 的可数检查）⇒ 形参只递 `&str`，
/// 时间戳用常量拼进 SQL。
fn link(m: &rc_store::MemoryHandle, id: &str, from: &MemKey, to: &MemKey) {
    m.conn()
        .execute(
            &format!(
                "INSERT INTO fact_link(id, from_id, to_id, relation, basis, evidence_json, decided_by,
                                       scope_kind, scope_key, world_id, source_agent, device_id,
                                       created_at_ms, norm_version)
                 VALUES(?1, ?2, ?3, 'part_of', '导出边夹具理由', '[]', NULL, 'project',
                        'proj-export-own', NULL, 'agent-fixture', 'dev-fixture', {NOW}, 1)"
            ),
            [id, from.key_string().as_str(), to.key_string().as_str()],
        )
        .unwrap();
}

/// C4b2b：`## edges` 段也过读闸——两端**任一**不可见，整条边不出现（不解释、不留痕）。
#[test]
fn an_edge_with_an_invisible_endpoint_is_absent_from_the_export() {
    let fx = seeded();
    link(fx.root.memory(), "rel-a-b", &fx.a[0], &fx.b);
    let text = export(&fx, None, Some(Viewer::human("agent-A")));
    assert!(!text.contains("rel-a-b"), "一端不可见的边仍进了导出件：{text}");
    assert!(!text.contains("## edges"), "边被闸光后还留下了空的 `## edges` 段：{text}");
    assert!(!text.contains("导出边夹具理由"), "被拒边的理由被渲出来了：{text}");
    // 边过闸不影响事实行：A 自己的行与读数照旧。
    assert!(text.contains(&fx.a[0].key_string()), "{text}");
    assert!(text.contains("2 rows listed of 2."), "{text}");
}

/// `viewer=None`（中心 CLI/旧测试）：边照旧渲染，旧行为一个字不变。
#[test]
fn without_a_viewer_the_edges_section_keeps_its_old_behaviour() {
    let fx = seeded();
    link(fx.root.memory(), "rel-a-b", &fx.a[0], &fx.b);
    let text = export(&fx, None, None);
    assert!(text.contains("## edges"), "viewer=None 时边段消失了：{text}");
    assert!(text.contains("relationship:rel-a-b"), "viewer=None 时边被闸掉了：{text}");
    assert!(
        text.contains(&format!("{} -> {}", fx.a[0].key_string(), fx.b.key_string())),
        "边的两端渲染不对：{text}"
    );
    assert!(text.contains("导出边夹具理由"), "边的理由没渲出来：{text}");
}

/// 反向读数：两端都 public ⇒ 边对**另一个** agent 的 Human 面也可见（闸门不是"全拒"）。
#[test]
fn an_edge_between_public_rows_crosses_to_another_agents_human_face() {
    let fx = seeded();
    link(fx.root.memory(), "rel-a-b", &fx.a[0], &fx.b);
    for k in [&fx.a[0], &fx.b] {
        fx.root
            .memory()
            .conn()
            .execute(
                "UPDATE resource_owner SET visibility='public' WHERE mem_table='card' AND mem_id=?1",
                [k.id.as_str()],
            )
            .unwrap();
    }
    let text = export(&fx, None, Some(Viewer::human("agent-C")));
    assert!(text.contains("## edges"), "public 两端的边没有进边段：{text}");
    assert!(text.contains("relationship:rel-a-b"), "public 两端的边没跨过读闸：{text}");
    assert!(text.contains("导出边夹具理由"), "{text}");
    // 两端都可见 ⇒ 两行也都在可见集里（边可见与行可见是同一套规则的下游）。
    assert!(text.contains("2 rows listed of 2."), "{text}");
}

/// 反向读数：public 行对另一个 agent 的 Human 面也可见（闸门不是"只剩自己的"）。
/// 这里直接改归属行的 `visibility`：本用例要断的是**读侧**认这一列，
/// 写侧的归一在 `rc-memory` 那一侧。
#[test]
fn a_public_row_crosses_to_another_agents_human_face() {
    let fx = seeded();
    fx.root.memory()
        .conn()
        .execute(
            "UPDATE resource_owner SET visibility='public' WHERE mem_table='card' AND mem_id=?1",
            [fx.b.id.as_str()],
        )
        .unwrap();
    let text = export(&fx, None, Some(Viewer::human("agent-A")));
    assert!(text.contains("导出归属乙"), "public 行没跨过读闸：{text}");
    assert!(text.contains(fx.body_b), "public 行的正文被闸掉了：{text}");
    assert!(text.contains("3 rows listed of 3."), "{text}");
}
