//! G1 刀 A：存量卡程序回填（派发档语义裁定④、测试钉⑥）。
//!
//! 回填与写点**同一判定函数同一把尺子**（`rc_memory::value_gate::program_gate_hit`）：
//! 先删掉写点盖的章模拟"功能上线前的老库"，回填必须盖回同一批卡；第二遍零新增（幂等）。
//! 豁免行（human 出处 / 豁免类别）两遍都不许被盖章。
//!
//! 夹具全部合成（守卫 12）。

use rc_distill::value_backfill::backfill_program_archive;
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::value_gate::PROGRAM_GATE_REASON;
use rc_memory::writers_fact::{put_fact_rows, FactWriteItem};
use rc_memory::{compute_mem_id, AnchorRole, BatchCtx, MemTable, Scope, ScopeKind};
use rc_store::Root;

const NOW: i64 = 1_700_000_999_000;
const SRC_MS: i64 = 1_700_000_000_000;
const AGENT: &str = "agent-g1a-bf";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-g1a-bf".into() }
}
fn ctx() -> BatchCtx {
    BatchCtx { source_agent: AGENT.into(), device_id: "dev-g1a-bf".into() }
}
fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-g1a-bf".into()),
        owner_agent_id: "agent-owner".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}
fn uid(index: usize) -> String {
    format!("{:064x}", 0x31 + index)
}
fn self_id() -> String {
    compute_mem_id(MemTable::Anchor, "agent", AGENT, "", &scope())
}

fn seed_self_anchor(m: &rc_store::MemoryHandle) {
    m.conn().execute(
        "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind,
                            scope_key, status, profile_text, created_at_ms, updated_at_ms,
                            norm_version)
         VALUES(?1,'agent',?2,'unknown',NULL,'project','proj-g1a-bf','active',NULL,?3,?3,?4)",
        rusqlite::params![self_id(), AGENT, SRC_MS, rc_store::capability::NORM_VERSION as i64],
    ).unwrap();
}

fn item(predicate: &str, category: &str, text: &str,
        evidence: Vec<(String, AnchorRole)>) -> FactWriteItem {
    FactWriteItem {
        subject_anchor_id: self_id(),
        object_anchor_id: None,
        predicate: predicate.into(),
        value: Some("已执行".into()),
        category: category.into(),
        mode: "stated".into(),
        time_text: String::new(),
        quote: text.into(),
        evidence,
        text: text.into(),
        scope: scope(),
        source_ms: SRC_MS,
    }
}

/// 四条行：两条施工（run_tests / stop_monitor）、一条豁免类别撞施工词、一条 human 出处。
fn seed_four(m: &rc_store::MemoryHandle) {
    let items = vec![
        item("run_tests", "observation", "回填用例：跑了一遍测试。",
             vec![(uid(1), AnchorRole::Agent)]),
        item("stop_monitor", "state_change", "回填用例：监听任务已停止。",
             vec![(uid(2), AnchorRole::Agent)]),
        item("run_tests", "decision", "回填用例：决定以后每天都跑测试。",
             vec![(uid(3), AnchorRole::Agent)]),
        item("run_tests", "observation", "回填用例：用户在場时跑了一遍测试。",
             vec![(uid(4), AnchorRole::Agent), (uid(5), AnchorRole::Human)]),
    ];
    put_fact_rows(m, &items, &ctx(), &owner(), NOW).unwrap();
}

fn sink_card_ids(m: &rc_store::MemoryHandle) -> Vec<String> {
    let mut st = m.conn().prepare(
        "SELECT card_id FROM card_sink WHERE sink='archive' ORDER BY card_id").unwrap();
    st.query_map([], |r| r.get::<_, String>(0)).unwrap().map(|r| r.unwrap()).collect()
}

#[test]
fn 回填与写点同一把尺子_两遍幂等_测试钉六() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_self_anchor(m);
    seed_four(m);
    let write_point_marks = sink_card_ids(m);
    assert_eq!(write_point_marks.len(), 2, "写点先盖两条施工行（夹具体检）");

    // 模拟功能上线前的老库：章不在，行都在。
    m.conn().execute("DELETE FROM card_sink", []).unwrap();
    assert!(sink_card_ids(m).is_empty());

    let first = backfill_program_archive(m, NOW).unwrap();
    assert_eq!(first.scanned, 4, "四条 stated fact 都过一遍判定");
    assert_eq!(first.hits, 2, "命中的恰是那两条施工行");
    assert_eq!(first.newly_marked, 2);
    assert_eq!(sink_card_ids(m), write_point_marks,
        "回填盖回的卡与写点逐 id 相同（同一判定函数同一把尺子）");
    // reason 同源；回填钟是回填时刻（不是源轮钟——章是现在盖的）。
    let reasons: Vec<String> = {
        let mut st = m.conn().prepare("SELECT reason FROM card_sink").unwrap();
        st.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    };
    assert!(reasons.iter().all(|r| r == PROGRAM_GATE_REASON));

    let second = backfill_program_archive(m, NOW + 1).unwrap();
    assert_eq!(second.scanned, first.scanned);
    assert_eq!(second.hits, first.hits);
    assert_eq!(second.newly_marked, 0, "第二遍零新增（幂等）");
    assert_eq!(sink_card_ids(m), write_point_marks, "两遍结果一致");
}

#[test]
fn 回填不碰豁免行与卡面() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_self_anchor(m);
    seed_four(m);
    m.conn().execute("DELETE FROM card_sink", []).unwrap();
    backfill_program_archive(m, NOW).unwrap();
    // 四张卡一张不少、verdict 一个不改。
    let cards: i64 = m.conn().query_row("SELECT count(*) FROM card", [], |r| r.get(0)).unwrap();
    assert_eq!(cards, 4);
    let unreviewed: i64 = m.conn().query_row(
        "SELECT count(*) FROM card WHERE verdict='unreviewed'", [], |r| r.get(0)).unwrap();
    assert_eq!(unreviewed, 4, "回填不改 verdict");
    // 豁免的两条（decision 类别 + human 出处）不在章里。
    let exempt_texts = ["回填用例：决定以后每天都跑测试。", "回填用例：用户在場时跑了一遍测试。"];
    for text in exempt_texts {
        let marked: i64 = m.conn().query_row(
            "SELECT count(*) FROM card c JOIN card_sink s ON s.card_id=c.id WHERE c.text=?1",
            [text], |r| r.get(0)).unwrap();
        assert_eq!(marked, 0, "豁免行被回填误盖：{text}");
    }
}

#[test]
fn 只读库拒回填() {
    let dir = tempfile::tempdir().unwrap();
    {
        let root = Root::open(dir.path()).unwrap();
        let m = root.memory();
        seed_self_anchor(m);
        seed_four(m);
    }
    let ro = Root::open_readonly(dir.path()).unwrap();
    let err = backfill_program_archive(ro.memory(), NOW).unwrap_err();
    assert!(matches!(err, rc_memory::MemError::ReadOnly), "只读面必须拒写：{err:?}");
}