//! 实体合并（`hub anchor merge` 的库面）：A/B/C 分类、方向、写动作、幂等、阴性。
//!
//! 口径分工：真把引用搬走的证据在这里（能直接造锚、mention、fact）；`hub` 那一侧的
//! CLI 测试只证接线、默认预演、JSON 形状与 `--only`。
use rc_memory::merge::{merge_apply, merge_plan, reference_counts, MergeAction, MergeClass};
use rc_memory::keys::AnchorRole;
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_fact::{put_fact_rows, FactWriteItem};
use rc_memory::writers_v4::{write_a1_with_owner, A1Anchor, A1Batch};
use rc_memory::{BatchCtx, Provenance, Scope, ScopeKind};
use rc_store::{MemoryHandle, Root};

const NOW: i64 = 1_700_000_900_000;
const SRC_MS: i64 = 1_700_000_000_000;
const SCOPE: &str = "proj-merge";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: SCOPE.into() }
}

fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-merge".into()),
        owner_agent_id: "agent-merge".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}

fn ctx() -> BatchCtx {
    BatchCtx { source_agent: "agent-merge".into(), device_id: "dev-merge".into() }
}

fn uid(index: usize) -> String {
    format!("{:064x}", 0xb0 + index)
}

/// 建一格锚（生产写入口）：kind/canonical/别名/出处/归属一次到位。
/// `stable=false` 落 `provisional`，`true` 落 `active`。
fn anchor(
    m: &MemoryHandle, kind: &str, canonical: &str, aliases: &[&str], stable: bool, turn: usize,
) -> String {
    let batch = A1Batch {
        anchors: vec![A1Anchor {
            reference: "a1".into(),
            kind: kind.into(),
            canonical: canonical.into(),
            aliases: aliases.iter().map(|a| (*a).to_string()).collect(),
            reality_status: "real".into(),
            evidence: vec![Provenance { turn_uid: uid(turn), anchor_role: AnchorRole::Human }],
            stable,
        }],
        mentions: Vec::new(),
        issues: Vec::new(),
        unresolved: Vec::new(),
    };
    let out =
        write_a1_with_owner(m, &batch, &ctx(), &owner(), &scope(), "run-merge", NOW).unwrap();
    out.by_ref["a1"].id.clone()
}

/// 直接落一格锚，绕过生产写入口：跨 kind 的同名对在写侧就会被合流（W15b1），
/// 而本文件要测的正是"库里已经存在这样一对"时合并器怎么分类——那一对只能预置。
fn seeded(m: &MemoryHandle, kind: &str, canonical: &str, status: &str) -> String {
    let id =
        rc_memory::compute_mem_id(rc_memory::MemTable::Anchor, kind, canonical, "", &scope());
    m.conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind, scope_key,
                                status, profile_text, created_at_ms, updated_at_ms, norm_version)
             VALUES(?1,?2,?3,'real',NULL,?4,?5,?6,NULL,?7,?7,?8)",
            rusqlite::params![
                id,
                kind,
                canonical,
                scope().kind.as_str(),
                scope().key,
                status,
                NOW,
                rc_store::capability::NORM_VERSION as i64
            ],
        )
        .unwrap();
    id
}

/// 在同一批里给某格锚落一条轮级指称（`mention.id` 含锚，换绑时会重键）。
fn mention_on(m: &MemoryHandle, anchor_id: &str, surface: &str, turn: usize) {
    let id = rc_memory::identity::compute_mention_id(&uid(turn), anchor_id, surface, None, None);
    m.conn()
        .execute(
            "INSERT INTO mention(id, anchor_id, turn_uid, surface, span_start, span_end,
                                 discourse_role, resolution, confidence, source_agent, device_id,
                                 created_at_ms, norm_version)
             VALUES(?1,?2,?3,?4,NULL,NULL,'mentioned','resolved',0.9,'agent-merge','dev-merge',?5,1)",
            rusqlite::params![id, anchor_id, uid(turn), surface, NOW],
        )
        .unwrap();
}

fn scalar(m: &MemoryHandle, sql: &str) -> i64 {
    m.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

/// 还活着的引用（attention 是"已解决"账，不算悬空）。
fn live_refs(m: &MemoryHandle, id: &str) -> i64 {
    let c = reference_counts(m, id).unwrap();
    (c.mention + c.memory_mention + c.state + c.fact + c.provenance + c.owner) as i64
}

/// 1：A 类（同 kind 同名）可合并；mention/provenance/归属搬走、别名归并、审计行就位。
#[test]
fn a_same_kind_pair_merges_and_moves_every_live_reference() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let to = anchor(m, "org", "bilibili", &[], true, 1);
    let from = anchor(m, "org", "B 站", &["Bilibili"], false, 2);
    mention_on(m, &from, "B站", 3);

    let plan = merge_plan(m).unwrap();
    assert_eq!(plan.len(), 1, "只应发现这一组：{plan:#?}");
    let c = &plan[0];
    assert_eq!(c.class, MergeClass::A);
    assert_eq!(c.action, MergeAction::Merge);
    assert_eq!((c.from_id.as_str(), c.to_id.as_str()), (from.as_str(), to.as_str()),
               "active 的一格留作 to：{c:#?}");
    assert!(c.refs >= 3, "from 上至少 1 mention + 出处 + 归属：{c:#?}");
    // 预演一格不写。
    assert_eq!(scalar(m, "SELECT count(*) FROM anchor_merge"), 0);
    assert_eq!(scalar(m, &format!("SELECT count(*) FROM mention WHERE anchor_id='{from}'")), 1);

    let report = merge_apply(m, "同一个人", "user", NOW, MergeClass::A).unwrap();
    assert_eq!(report.merged_count(), 1);
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    let applied = &report.merged[0];
    assert!(applied.run_id.starts_with("merge-"));
    assert_eq!(applied.rebind.mentions_moved, 1);
    assert_eq!(applied.rebind.memory_mentions_moved, 0);

    // 旧锚：不删、盖章；新锚仍是自己。
    let status: String = m
        .conn()
        .query_row("SELECT status FROM anchor WHERE id=?1", [&from], |r| r.get(0))
        .unwrap();
    assert_eq!(status, "merged");
    let to_status: String = m
        .conn()
        .query_row("SELECT status FROM anchor WHERE id=?1", [&to], |r| r.get(0))
        .unwrap();
    assert_eq!(to_status, "active");
    assert_eq!(scalar(m, "SELECT count(*) FROM anchor"), 2, "不许物理删");

    // 引用零悬空：mention 改指 to，出处/归属跟着搬。
    assert_eq!(live_refs(m, &from), 0, "旧锚不再有任何活引用");
    let mention_to: i64 = m
        .conn()
        .query_row("SELECT count(*) FROM mention WHERE anchor_id=?1", [&to], |r| r.get(0))
        .unwrap();
    assert_eq!(mention_to, 1);

    // 别名归并：旧别名 + 旧正名都成为 to 的称呼。
    let aliases: Vec<String> = {
        let mut st = m
            .conn()
            .prepare("SELECT alias FROM alias WHERE anchor_id=?1 ORDER BY alias")
            .unwrap();
        st.query_map([&to], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(aliases, vec!["B 站".to_string(), "Bilibili".to_string()]);

    // 审计行：from/to/run_id/reason 一格不少。
    let row: (String, String, String, String) = m
        .conn()
        .query_row(
            "SELECT from_id, to_id, run_id, reason FROM anchor_merge",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!((row.0.as_str(), row.1.as_str()), (from.as_str(), to.as_str()));
    assert_eq!(row.2, applied.run_id);
    assert_eq!(row.3, "同一个人");

    // memory_op 的 MERGE 台账仍在（旧锚"出声不消失"）。
    assert_eq!(
        scalar(m, "SELECT count(*) FROM memory_op WHERE op='MERGE' AND mem_table='anchor'"),
        1
    );
}

/// 2：幂等——第二次运行 `merged=0`，清单里不再出现已合并的 from，审计行不长第二条。
#[test]
fn a_second_run_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let _to = anchor(m, "org", "bilibili", &[], true, 1);
    let _from = anchor(m, "org", "B 站", &["Bilibili"], false, 2);

    assert_eq!(merge_apply(m, "r1", "user", NOW, MergeClass::A).unwrap().merged_count(), 1);
    assert!(merge_plan(m).unwrap().is_empty(), "已合并的 from 不许再出现");
    let again = merge_apply(m, "r1", "user", NOW + 1, MergeClass::A).unwrap();
    assert_eq!(again.merged_count(), 0);
    assert_eq!(scalar(m, "SELECT count(*) FROM anchor_merge"), 1);
}

/// 3：跨 kind 同名是 B 类——只出清单，apply 一行不写。
#[test]
fn cross_kind_duplicates_are_reported_but_never_merged() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let work = anchor(m, "work", "d3", &[], false, 1);
    let agent = seeded(m, "agent", "d3", "provisional");

    let plan = merge_plan(m).unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].class, MergeClass::B);
    assert_eq!(plan[0].action, MergeAction::Manual);
    assert_eq!(plan[0].reason, "cross_kind");

    let report = merge_apply(m, "r", "user", NOW, MergeClass::A).unwrap();
    assert_eq!(report.merged_count(), 0);
    assert_eq!(scalar(m, "SELECT count(*) FROM anchor_merge"), 0);
    for id in [&work, &agent] {
        let status: String = m
            .conn()
            .query_row("SELECT status FROM anchor WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "provisional", "B 类不许被顺手合掉");
    }
}

/// 4：同一名字被三格认领 ⇒ C 类（目标不唯一），不自动。
#[test]
fn an_alias_claimed_by_three_anchors_is_class_c() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let a = anchor(m, "org", "shared", &[], true, 1);
    let b = anchor(m, "org", "b", &[], false, 2);
    let c = anchor(m, "org", "c", &[], false, 3);
    // 生产写入口会拒绝"歧义别名硬挂"，所以这里直接落 alias 行来造 C 类的真实形状：
    // 同一个称呼被三格认领，目标自然不唯一。
    for id in [&b, &c] {
        m.conn()
            .execute(
                "INSERT INTO alias(anchor_id, alias) VALUES(?1, 'shared')",
                [id],
            )
            .unwrap();
    }

    let plan = merge_plan(m).unwrap();
    assert_eq!(plan.len(), 2, "两个非幸存者各一行：{plan:#?}");
    for row in &plan {
        assert_eq!(row.class, MergeClass::C);
        assert_eq!(row.action, MergeAction::Manual);
        assert_eq!(row.reason, "ambiguous_target");
        assert_eq!(row.to_id, a);
    }
    assert_eq!(merge_apply(m, "r", "user", NOW, MergeClass::A).unwrap().merged_count(), 0);
    assert_eq!(scalar(m, "SELECT count(*) FROM anchor_merge"), 0);
    for id in [&b, &c] {
        let status: String = m
            .conn()
            .query_row("SELECT status FROM anchor WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "provisional");
    }
}

/// 5：A 类但 fact 主体/客体参与 id 且 id 输入未落库 ⇒ 预演就降级为人工，不假装能合。
#[test]
fn a_pair_with_unmovable_fact_refs_is_manual_even_when_same_kind() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let to = anchor(m, "person", "张三", &[], true, 1);
    let from = anchor(m, "person", "老张", &[], false, 2);
    // 名字靠别名连起来（写入口会拒绝歧义别名，这里直接落行造形状）。
    m.conn()
        .execute("INSERT INTO alias(anchor_id, alias) VALUES(?1, '张三')", [&from])
        .unwrap();
    put_fact_rows(
        m,
        &[FactWriteItem {
            subject_anchor_id: from.clone(),
            object_anchor_id: None,
            predicate: "met".into(),
            value: Some("done".into()),
            category: "event".into(),
            mode: rc_memory::FACT_MODE_STATED.into(),
            time_text: String::new(),
            quote: "见过".into(),
            evidence: vec![(uid(3), AnchorRole::Human)],
            text: "见过张三".into(),
            scope: scope(),
            source_ms: SRC_MS,
        }],
        &ctx(),
        &owner(),
        NOW,
    )
    .unwrap();

    let plan = merge_plan(m).unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].class, MergeClass::A);
    assert_eq!(plan[0].action, MergeAction::Manual, "{plan:#?}");
    assert!(
        plan[0].reason.starts_with("blocked:") && plan[0].reason.contains("fact_id_inputs_not_stored"),
        "原因码要指出搬不动的那一格：{}",
        plan[0].reason
    );
    assert_eq!(merge_apply(m, "r", "user", NOW, MergeClass::A).unwrap().merged_count(), 0);
    let status: String = m
        .conn()
        .query_row("SELECT status FROM anchor WHERE id=?1", [&from], |r| r.get(0))
        .unwrap();
    assert_eq!(status, "provisional");
    assert_eq!(plan[0].to_id, to);
}

/// 6：阴性——没有同名双格的锚不许被凑成一组（单端、无别名交叉都不算重复）。
#[test]
fn anchors_without_a_name_twin_are_not_grouped() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let _a = anchor(m, "work", "solo", &["alone"], true, 1);
    let _b = anchor(m, "work", "other", &[], true, 2);
    let _c = anchor(m, "person", "solo-person", &[], true, 3); // 名字不连任何一格
    assert!(merge_plan(m).unwrap().is_empty(), "只有单边匹配不算重复组");
    assert_eq!(merge_apply(m, "r", "user", NOW, MergeClass::A).unwrap().merged_count(), 0);
}

/// 7：同义别名去重——两格都叫同一个别名，合并不许在 to 上长出第二行同名列。
#[test]
fn duplicate_aliases_are_not_doubled() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let to = anchor(m, "org", "B 站", &["Bilibili"], true, 1);
    let _from = anchor(m, "org", "bilibili", &[], false, 2);
    // 造"两格都叫同一个别名"的碰撞：写入口会因歧义不落第二条，这里直接补。
    m.conn()
        .execute(
            "INSERT INTO alias(anchor_id, alias) VALUES(?1, 'Bilibili')",
            [&_from],
        )
        .unwrap();
    let report = merge_apply(m, "r", "user", NOW, MergeClass::A).unwrap();
    assert_eq!(report.merged_count(), 1);
    let n: i64 = m
        .conn()
        .query_row(
            "SELECT count(*) FROM alias WHERE anchor_id=?1 AND alias='Bilibili'",
            [&to],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "同名别名只留一行");
    let total: i64 = m
        .conn()
        .query_row("SELECT count(*) FROM alias WHERE anchor_id=?1", [&to], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 2, "Bilibili + 旧正名 bilibili");
}
