//! G1 刀 A（设计 `2026-11-08-g1-reuse-lesson-design.md` §波次一）：程序资格预闸的写点钉。
//!
//! 判据（派发档语义裁定①②）：三者合取——主体/说话人 = agent 自锚 ∧ 出处无 human 轮参与
//! ∧ 谓词 ∈ 施工谓词闭集；豁免优先（human 出处 ∨ category∈decision/promise/plan
//! ∨ 谓词∈决策谓词闭集）。命中 ⇒ 镜像卡落 `card_sink(sink='archive')`：原文全留、照常进库、
//! 不改 verdict，只是召回主循环缺省不取（消费面的钉在 rc-search/rc-agent 的测试里）。
//!
//! 载体（G1 刀 B 第 0 件起）走 canonical 正路：DDL 住 `rc_store::ddl::CARD_SINK_SQL`
//! （`MEMORY_SQL` 重放列，开库即存在）、表名进 `MEMORY_TABLES` 与归属表，hub 的
//! rows/restore/backup 因此带着章走。读侧仍保留"缺表 = 空集"（刀 A 之前开的只读旧副本
//! 没有这张表，仍要能跑）；写路径的幂等补表留作双保险（同一份 DDL，不另写一遍）。
//!
//! 夹具全部合成（守卫 12）：通用占位名，没有语料专名、没有 CANARY、没有路径。

use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::value_gate::{
    archived_card_ids, human_provenance_exists, program_gate_hit, GateInput,
    CONSTRUCTION_PREDICATES, DECISION_PREDICATES, EXEMPT_CATEGORIES, PROGRAM_GATE_REASON,
    VALUE_AXIS_REASON,
};
use rc_memory::writers_fact::{put_fact_rows, put_fact_rows_with_extras, FactExtras, FactWriteItem};
use rc_memory::writers_v4::record_turn_speakers;
use rc_memory::{compute_mem_id, AnchorRole, BatchCtx, MemTable, Scope, ScopeKind};
use rc_store::Root;

const NOW: i64 = 1_700_000_999_000;
const SRC_MS: i64 = 1_700_000_000_000;
const AGENT: &str = "agent-g1a";
const OTHER: &str = "agent-other";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-g1a".into() }
}
fn ctx() -> BatchCtx {
    BatchCtx { source_agent: AGENT.into(), device_id: "dev-g1a".into() }
}
fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-g1a".into()),
        owner_agent_id: "agent-owner".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}
fn uid(index: usize) -> String {
    format!("{:064x}", 0x21 + index)
}
/// 自锚 id 与 `session_agent_anchor_id` 同一条算法（kind='agent'、canonical=source_agent）。
fn self_id() -> String {
    compute_mem_id(MemTable::Anchor, "agent", AGENT, "", &scope())
}
fn other_id() -> String {
    compute_mem_id(MemTable::Anchor, "agent", OTHER, "", &scope())
}

/// 铺一格活锚（外键目标）。kind/canonical 与上面两个 id 函数同源。
fn seed_anchor(m: &rc_store::MemoryHandle, id: &str, canonical: &str) {
    m.conn().execute(
        "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind,
                            scope_key, status, profile_text, created_at_ms, updated_at_ms,
                            norm_version)
         VALUES(?1,'agent',?2,'unknown',NULL,'project','proj-g1a','active',NULL,?3,?3,?4)",
        rusqlite::params![id, canonical, SRC_MS, rc_store::capability::NORM_VERSION as i64],
    ).unwrap();
}

/// 一条施工行：subject=自锚、证据全 agent、谓词 run_tests、category=observation。
/// 说话人由 C1 程序算：无 human 证据且无轮级 speaker ⇒ 会话自锚（两个自锚槽都命中）。
fn construction_item(index: usize, predicate: &str, category: &str, text: &str) -> FactWriteItem {
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

fn sink_rows(m: &rc_store::MemoryHandle) -> Vec<(String, String, String, i64)> {
    // 缺表 = 没有归档（载体走 review_verdict 先例：第一次盖章才建表）。
    let exists: i64 = m.conn().query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='card_sink'",
        [], |r| r.get(0)).unwrap();
    if exists == 0 {
        return Vec::new();
    }
    let mut st = m.conn().prepare(
        "SELECT card_id, sink, reason, at_ms FROM card_sink ORDER BY card_id").unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap().map(|r| r.unwrap()).collect()
}

fn card_count(m: &rc_store::MemoryHandle) -> i64 {
    m.conn().query_row("SELECT count(*) FROM card", [], |r| r.get(0)).unwrap()
}

// ── 纯判定：闭集与决策矩阵 ────────────────────────────────────────────

#[test]
fn 闭集词表与归一化口径被钉住() {
    // 施工闭集：现网分布定词 + 计划例词（中英）。抽样钉几个代表，全词表在报告里列全。
    for w in ["run_tests", "stop_monitor", "attach_listener", "deploy_release",
              "check_db_stats", "verify_fix", "dispatch_subagents", "rebuild_fts",
              "部署", "重启", "监听", "提交", "构建", "运行", "测试",
              "deploy", "restart", "monitor", "commit", "build", "run", "test"] {
        assert!(CONSTRUCTION_PREDICATES.contains(&w), "施工闭集少了 {w}");
    }
    // 宁漏勿错的排除项：里程碑/发现/决策类不在闭集里。
    for w in ["land", "land_w31", "land_identity_model", "accept_t2", "decide_manual_maintenance",
              "complete_w32_stopgap_wave", "confirm_design", "find_issue", "observe_watermark_drift",
              "process_died", "decide", "plan", "promise"] {
        assert!(!CONSTRUCTION_PREDICATES.contains(&w), "施工闭集误收了 {w}（宁漏勿错）");
    }
    // 决策谓词闭集 = 派发档逐字五词；豁免类别 = 三值。
    assert_eq!(DECISION_PREDICATES.to_vec(),
               vec!["决定", "拍板", "裁定", "要求", "承诺"]);
    assert_eq!(EXEMPT_CATEGORIES.to_vec(), vec!["decision", "promise", "plan"]);
    // 归一化：trim + ASCII  lowercase 后精确相等（与 check_alias 同一把尺子）。
    let input = GateInput {
        subject_anchor_id: &self_id(), speaker_anchor_id: Some(&self_id()),
        self_anchor_id: Some(&self_id()), human_in_provenance: false,
        category: "observation", predicate: "  Run_Tests  ",
    };
    assert!(program_gate_hit(&input), "归一化后精确相等必须命中");
}

#[test]
fn 判定矩阵_豁免优先与三者合取() {
    let self_id = self_id();
    let other = other_id();
    let base = GateInput {
        subject_anchor_id: &self_id, speaker_anchor_id: Some(&self_id),
        self_anchor_id: Some(&self_id), human_in_provenance: false,
        category: "observation", predicate: "run_tests",
    };
    assert!(program_gate_hit(&base), "三者合取命中");
    // 豁免①：出处任一环有 human。
    assert!(!program_gate_hit(&GateInput { human_in_provenance: true, ..base }));
    // 豁免②：category 三值（即使谓词撞施工词）。
    for c in ["decision", "promise", "plan"] {
        assert!(!program_gate_hit(&GateInput { category: c, ..base }), "category={c} 必须豁免");
    }
    // 豁免③：决策谓词五词（即使 category 不是豁免类）。
    for p in ["决定", "拍板", "裁定", "要求", "承诺"] {
        assert!(!program_gate_hit(&GateInput { predicate: p, ..base }), "谓词={p} 必须豁免");
    }
    // 合取①：主体/说话人两槽都不是自锚 ⇒ 不命中（测试钉⑤）。
    assert!(!program_gate_hit(&GateInput {
        subject_anchor_id: &other, speaker_anchor_id: Some(&other), ..base }));
    // 说话人槽命中也算（派发档"主体/说话人"的析取读法）：主体是别的锚、但本 agent 说的。
    assert!(program_gate_hit(&GateInput { subject_anchor_id: &other, ..base }));
    // 说话人缺席、主体命中 ⇒ 命中。
    assert!(program_gate_hit(&GateInput { speaker_anchor_id: None, ..base }));
    // 自锚解析不出来（锚不在库）⇒ 不命中：不猜。
    assert!(!program_gate_hit(&GateInput { self_anchor_id: None, ..base }));
    // 合取③：谓词不在闭集 ⇒ 不命中（宁漏勿错，刀 B 兜底）。
    assert!(!program_gate_hit(&GateInput { predicate: "land_w31", ..base }));
    assert!(!program_gate_hit(&GateInput { predicate: "run_tests_extra", ..base }),
            "精确相等：前缀撞词不算命中");
}

// ── 写点：落卡事务内盖章 ─────────────────────────────────────────────

#[test]
fn 施工行落_archive_卡照常进库_verdict_不动() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchor(m, &self_id(), AGENT);
    let out = put_fact_rows(m, &[construction_item(1, "run_tests", "observation",
        "监听任务已停止，施工流水一句。")], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(out.cards, 1, "卡照常进库（archive 不是不写）");
    assert_eq!(card_count(m), 1);
    // verdict 不冒充、不被改：仍是写卡时的 unreviewed。
    let verdict: String = m.conn().query_row(
        "SELECT verdict FROM card", [], |r| r.get(0)).unwrap();
    assert_eq!(verdict, "unreviewed", "archive 不改 verdict");
    let rows = sink_rows(m);
    assert_eq!(rows.len(), 1, "施工行必须落一条 archive");
    let (card_id, sink, reason, at_ms) = &rows[0];
    assert_eq!(sink, "archive");
    assert_eq!(reason, PROGRAM_GATE_REASON);
    assert_eq!(*at_ms, SRC_MS, "盖章钟取源轮时刻（I3），不是导入钟 NOW");
    // 载体指向的正是那张镜像卡。
    let pointed: String = m.conn().query_row(
        "SELECT id FROM card", [], |r| r.get(0)).unwrap();
    assert_eq!(card_id, &pointed);
    // 读口同一把尺子。
    let archived = archived_card_ids(m).unwrap();
    assert!(archived.contains(&pointed));
}

#[test]
fn human_出处照旧_main_测试钉二() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchor(m, &self_id(), AGENT);
    let mut item = construction_item(1, "run_tests", "observation", "用户在场时跑了一次测试。");
    item.evidence.push((uid(2), AnchorRole::Human));
    put_fact_rows(m, &[item], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(card_count(m), 1, "卡照常进库");
    assert!(sink_rows(m).is_empty(), "human 出处豁免：不落 archive");
    assert!(archived_card_ids(m).unwrap().is_empty());
}

#[test]
fn 豁免类别撞施工词也_main_测试钉三() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchor(m, &self_id(), AGENT);
    let items: Vec<FactWriteItem> = ["decision", "promise", "plan"].iter().enumerate()
        .map(|(i, c)| construction_item(i + 1, "run_tests", c,
            &format!("豁免类别 {c} 的施工词用例正文。")))
        .collect();
    put_fact_rows(m, &items, &ctx(), &owner(), NOW).unwrap();
    assert_eq!(card_count(m), 3);
    assert!(sink_rows(m).is_empty(), "category∈decision/promise/plan 豁免优先");
}

#[test]
fn 决策谓词照旧_main_测试钉四() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchor(m, &self_id(), AGENT);
    put_fact_rows(m, &[construction_item(1, "决定", "observation",
        "决定下周上线价值闸。")], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(card_count(m), 1);
    assert!(sink_rows(m).is_empty(), "决策谓词豁免：不落 archive");
}

#[test]
fn 非自锚照旧_main_测试钉五() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchor(m, &self_id(), AGENT);
    // 轮级 speaker = 另一个 agent（C1 优先取 mention 的 speaker）：说话人槽也不是自锚。
    record_turn_speakers(m, &ctx(), &owner(), &scope(),
        &[(uid(1), OTHER.into())], NOW).unwrap();
    let mut item = construction_item(1, "run_tests", "observation", "子代理跑了一次测试。");
    item.subject_anchor_id = other_id();
    put_fact_rows(m, &[item], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(card_count(m), 1);
    // 说话人确实解析到了另一个 agent（夹具自身的体检）。
    let speaker: Option<String> = m.conn().query_row(
        "SELECT speaker_anchor_id FROM fact", [], |r| r.get(0)).unwrap();
    assert_eq!(speaker.as_deref(), Some(other_id().as_str()));
    assert!(sink_rows(m).is_empty(), "主体/说话人都不是自锚 ⇒ main");
}

#[test]
fn 写点重放幂等_不盖第二次() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchor(m, &self_id(), AGENT);
    let items = [construction_item(1, "run_tests", "observation", "重放幂等用例正文。")];
    put_fact_rows(m, &items, &ctx(), &owner(), NOW).unwrap();
    let first = sink_rows(m);
    let replay = put_fact_rows(m, &items, &ctx(), &owner(), NOW).unwrap();
    assert_eq!(replay.cards, 0, "重放不增卡");
    assert_eq!(sink_rows(m), first, "重放不增 archive 行、不改既有盖章");
}

#[test]
fn 开库即有表_旧副本缺表读作空集() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    // G1 刀 B 第 0 件：card_sink 升 canonical 清单 ⇒ **开库即存在**。MEMORY_TABLES 的
    // 备份行数读数是严格制（清单有而库里没表会报错），懒补表撑不住升清单。
    let exists: i64 = m.conn().query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='card_sink'",
        [], |r| r.get(0)).unwrap();
    assert_eq!(exists, 1, "DDL 必须在 MEMORY_SQL 重放列里：开库即有 card_sink");
    assert!(rc_memory::schema::MEMORY_TABLES.contains(&"card_sink"),
            "card_sink 必须进 canonical 清单（hub rows/restore/backup 才带着章走）");
    assert!(archived_card_ids(m).unwrap().is_empty(), "没盖过章 ⇒ 空集");
    // 只读副本上同一读口也必须能跑（现网库只读测量的前提）。
    let ro = Root::open_readonly(dir.path()).unwrap();
    assert!(archived_card_ids(ro.memory()).unwrap().is_empty());
    drop(ro);
    // 真旧副本形状：刀 A 之前开的库没有这张表。DROP 之后只读打开，读口仍按"没有归档"
    // 处理而不是报错——读侧的缺表兼容不随升清单消失。
    m.conn().execute_batch("DROP TABLE card_sink").unwrap();
    let old = Root::open_readonly(dir.path()).unwrap();
    assert!(archived_card_ids(old.memory()).unwrap().is_empty(), "缺表 = 空集，不是错误");
}

#[test]
fn human_出处判定与写点共用一个入口() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchor(m, &self_id(), AGENT);
    let mut item = construction_item(1, "run_tests", "observation", "混证据用例正文。");
    item.evidence.push((uid(2), AnchorRole::Human));
    let out = put_fact_rows(m, &[item], &ctx(), &owner(), NOW).unwrap();
    let card_id = &out.card_ids[0].id;
    let fact_id: String = m.conn().query_row(
        "SELECT id FROM fact", [], |r| r.get(0)).unwrap();
    // 同一入口：fact 与 card 两侧的出处角色逐条同形。
    assert!(human_provenance_exists(m.conn(), "fact", &fact_id).unwrap());
    assert!(human_provenance_exists(m.conn(), "card", card_id).unwrap());
    assert!(!human_provenance_exists(m.conn(), "card", &"0".repeat(64)).unwrap());
}
// ── G1 刀 B：价值轴章与程序章同表同待遇 ─────────────────────────────

/// 价值轴判 demote 的 extras（其余三格默认）。
fn demote_extra() -> FactExtras {
    FactExtras { value_demote: true, ..Default::default() }
}

#[test]
fn 价值轴_demote_盖价值轴章_keep_不盖() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchor(m, &self_id(), AGENT);
    // 谓词故意**不在**施工闭集（程序闸不命中），于是这一行只可能被价值轴盖章。
    let demoted = construction_item(1, "confirm_plan_review", "observation", "价值轴 demote 用例正文。");
    put_fact_rows_with_extras(m, &[demoted], &[demote_extra()], &ctx(), &owner(), NOW).unwrap();
    let kept = construction_item(2, "confirm_plan_review", "observation", "价值轴 keep 用例正文。");
    put_fact_rows(m, &[kept], &ctx(), &owner(), NOW).unwrap();

    let rows = sink_rows(m);
    assert_eq!(rows.len(), 1, "只有 demote 那一行盖章；keep 不盖章");
    let (card_id, sink, reason, at_ms) = &rows[0];
    assert_eq!(sink, "archive", "价值轴章与程序章同一格 sink");
    assert_eq!(reason, VALUE_AXIS_REASON);
    assert_eq!(*at_ms, SRC_MS, "章钟取源轮时刻（I3），不是导入钟");
    // 载体指向的那张卡就是 demote 那一行的镜像卡（不是 keep 的）。
    let demoted_text: String = m.conn().query_row(
        "SELECT text FROM card WHERE id=?1", [card_id], |r| r.get(0)).unwrap();
    assert_eq!(demoted_text, "价值轴 demote 用例正文。");
    assert!(archived_card_ids(m).unwrap().contains(card_id), "读口同一把尺子");
    assert_eq!(card_count(m), 2, "两行都照常进库（archive 不是不写）");
}

#[test]
fn 两轴同命中_程序章优先_且先写者赢() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchor(m, &self_id(), AGENT);
    // 施工谓词（程序闸命中）+ 价值轴 demote：一次调用里两轴都成立 ⇒ 程序章优先。
    let both = construction_item(1, "run_tests", "observation", "两轴同命中用例正文。");
    put_fact_rows_with_extras(m, std::slice::from_ref(&both), &[demote_extra()], &ctx(), &owner(), NOW).unwrap();
    let rows = sink_rows(m);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2, PROGRAM_GATE_REASON, "同一次调用里程序章优先");

    // 跨调用：同一张卡重放（程序章已在）⇒ 价值轴章 INSERT OR IGNORE 返回 0，不改既有章。
    let replay = put_fact_rows_with_extras(m, &[both], &[demote_extra()], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(replay.cards, 0, "同址卡重放不增卡");
    let after = sink_rows(m);
    assert_eq!(after, rows, "先写者赢：既有程序章不被价值轴覆盖");
}
