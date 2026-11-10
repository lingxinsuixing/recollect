//! G1 刀 A：archive 在**单一读口**（`rank`）的消费（派发档语义裁定⑤⑥）。
//!
//! hook probe（`gate::probe_view`）与 mcp recall 共享 `rank` 这一个候选池入口：
//! archive 过滤落在这里，两个消费者不许各自再滤一遍。计数与过滤同一处产生
//! （`rank_counted` 的第二返回值），`rank` 是它的薄包装（先例：put_timeline_rows_counted）。
//!
//! 顺序钉：读闸在前、archive 在后——被身份闸拒的行静默缺席，**不算**归档计数
//! （价值降档与身份沉默是两族，混计会让 value_mix 泄露存在性）。
//!
//! 夹具全部合成（守卫 12）。

use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_fact::{put_fact_rows, FactWriteItem};
use rc_memory::{compute_mem_id, AnchorRole, BatchCtx, MemTable, Scope, ScopeKind};
use rc_search::gate::probe_view;
use rc_search::rank::{rank, rank_counted};
use rc_store::Root;

const NOW: i64 = 1_700_000_999_000;
const SRC_MS: i64 = 1_700_000_000_000;
const AGENT: &str = "agent-g1a-search";
const QUERY: &str = "归档消费合成词";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-g1a-search".into() }
}
fn ctx() -> BatchCtx {
    BatchCtx { source_agent: AGENT.into(), device_id: "dev-g1a-search".into() }
}
fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-g1a-search".into()),
        owner_agent_id: AGENT.into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}
fn self_id() -> String {
    compute_mem_id(MemTable::Anchor, "agent", AGENT, "", &scope())
}
fn uid(index: usize) -> String {
    format!("{:064x}", 0x41 + index)
}

fn seed_anchor(m: &rc_store::MemoryHandle) {
    m.conn().execute(
        "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind,
                            scope_key, status, profile_text, created_at_ms, updated_at_ms,
                            norm_version)
         VALUES(?1,'agent',?2,'unknown',NULL,'project','proj-g1a-search','active',NULL,?3,?3,?4)",
        rusqlite::params![self_id(), AGENT, SRC_MS, rc_store::capability::NORM_VERSION as i64],
    ).unwrap();
}

fn item(index: usize, predicate: &str, category: &str, text: &str) -> FactWriteItem {
    FactWriteItem {
        subject_anchor_id: self_id(),
        object_anchor_id: None,
        predicate: predicate.into(),
        value: Some("已执行".into()),
        category: category.into(),
        mode: "stated".into(),
        time_text: String::new(),
        quote: text.into(),
        evidence: vec![(uid(index), AnchorRole::Agent)],
        text: text.into(),
        scope: scope(),
        source_ms: SRC_MS,
    }
}

/// 一条施工行（归档）+ 一条豁免类别行（main），正文共用查询词。
fn seed_pair(m: &rc_store::MemoryHandle) {
    seed_anchor(m);
    put_fact_rows(m, &[
        item(1, "run_tests", "observation", &format!("{QUERY} 的施工流水正文。")),
        item(2, "run_tests", "decision", &format!("{QUERY} 的决策正文。")),
    ], &ctx(), &owner(), NOW).unwrap();
    rc_index::rebuild(m, NOW).unwrap();
}

#[test]
fn rank_counted_过滤与计数同一处_rank_是薄包装() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_pair(m);
    let (rows, archived) = rank_counted(m, None, NOW, QUERY).unwrap();
    assert_eq!(archived, 1, "本查询候选集里恰一条被归档挡下");
    assert_eq!(rows.len(), 1, "主循环只剩决策卡");
    assert_eq!(rows[0].facts.kind, "decision");
    // 薄包装与计数版逐位同序。
    let plain = rank(m, None, NOW, QUERY).unwrap();
    assert_eq!(plain.len(), rows.len());
    for (a, b) in plain.iter().zip(&rows) {
        assert_eq!(a.key.key_string(), b.key.key_string());
        assert_eq!(a.score, b.score);
    }
    // 原文全留：两张卡都在库里，archive 不是删除。
    let cards: i64 = m.conn().query_row("SELECT count(*) FROM card", [], |r| r.get(0)).unwrap();
    assert_eq!(cards, 2);
}

#[test]
fn 库里另有归档行_不进本查询候选集就不计数_测试钉七() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_pair(m);
    // 第三条施工行：正文不含查询词 ⇒ 归档了也不进这个查询的候选集。
    put_fact_rows(m, &[item(3, "stop_monitor", "state_change", "无关的监听停止流水。")],
        &ctx(), &owner(), NOW).unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    let sink_total: i64 = m.conn().query_row(
        "SELECT count(*) FROM card_sink", [], |r| r.get(0)).unwrap();
    assert_eq!(sink_total, 2, "全库两条归档（夹具体检）");
    let (rows, archived) = rank_counted(m, None, NOW, QUERY).unwrap();
    assert_eq!(archived, 1, "只数本查询候选集被挡的那一条，不是全库总数");
    assert_eq!(rows.len(), 1);
}

#[test]
fn probe_view_与_rank_同享归档过滤() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_pair(m);
    let view = probe_view(m, None, QUERY, NOW).unwrap();
    assert_eq!(view.candidates, 1, "归档行不进注入候选（hook 与 mcp 同一把尺子）");
    for d in &view.forms {
        assert_eq!(d.key.table, MemTable::Card);
        assert_eq!(d.facts.kind, "decision", "出声的只许是决策卡");
    }
}

#[test]
fn 读闸先于归档计数_被身份拒的行不算归档() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchor(m);
    // private_agent 的行：陌生 agent 读不到（读闸拒），即使它也被归档了。
    let private_owner = OwnerCtx { visibility: Visibility::PrivateAgent, ..owner() };
    put_fact_rows(m, &[item(1, "run_tests", "observation", &format!("{QUERY} 的私有流水。"))],
        &ctx(), &private_owner, NOW).unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    let sink: i64 = m.conn().query_row(
        "SELECT count(*) FROM card_sink", [], |r| r.get(0)).unwrap();
    assert_eq!(sink, 1, "写点照盖章（归档与身份是两轴）");
    let stranger = rc_memory::gate::Viewer::agent("stranger");
    let (rows, archived) = rank_counted(m, Some(&stranger), NOW, QUERY).unwrap();
    assert!(rows.is_empty(), "身份闸拒的行静默缺席");
    assert_eq!(archived, 0, "被身份闸拒的行不算归档计数（不泄露存在性）");
    // 无身份面（None）才看得到归档计数。
    let (rows_all, archived_all) = rank_counted(m, None, NOW, QUERY).unwrap();
    assert!(rows_all.is_empty());
    assert_eq!(archived_all, 1);
}