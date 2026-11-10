//! C3：候选渲染的 `[self]` / `[principal]` 标记，与「名字出现在本批证据」的预筛排序。
//!
//! 这一层只断四件事，且都不跑真实 LLM：
//! 1. 会话自锚渲染成 `- aN | agent | <source_agent> [self]`、主人锚渲染成
//!    `- aN | person | <canonical> [principal]`（fact 与 event 同一个渲染点）；
//! 2. canonical（或该锚的别名）出现在**目标轮正文**的候选排到前面，其余保持
//!    `created_at DESC` 的原序；
//! 3. 预筛**只重排、不删**：同一批的候选总数与名字没出现时一致，非命中候选一个不少；
//! 4. `a1..aN` 按渲染位置编号，与 `plan.candidates` 的顺序一一对应。
//!
//! 正反例只用通用占位名（张三/李四/老四），不含任何语料专名。

use rc_distill::pass_fact::{select_fact_batch, FactPlan};
use rc_distill::pass_prompt::{
    event_user, fact_user, FactCandidate, FactFamily, MARK_PRINCIPAL, MARK_SELF,
};
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{AnchorRole, Provenance, Scope, ScopeKind};
use rc_store::Root;

mod common;
use common::{adopt_rows, seed_bodies};

const NOW: i64 = 1_700_000_000_000;

/// 两条目标轮 + 两个实体锚：张三较新（无正文命中时排 a1）、李四较早（别名「老四」）。
///
/// 会话归属改成 `alice / project/proj-x`（与命名 pass 的真实链路同向），主人身份由调用方给：
/// `Some("p1")` 建主人锚，`None` 则一个都不建。
fn fixture(bodies: &[&str], principal: Option<&str>) -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, bodies, 1_000);
    root.transcript_mut()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id=?1, owner_agent_id='alice',
                    visibility='project', retention='keep',
                    scope_kind='project', scope_key='proj-x', ownership_src='operator'
             WHERE session_key='sess-batch-A' AND device_id='dev-batch-A'",
            rusqlite::params![principal],
        )
        .unwrap();
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    for (canonical, created_at_ms, aliases) in [
        ("张三", NOW - 1_000, vec![]),
        ("李四", NOW - 2_000, vec!["老四".to_string()]),
    ] {
        put_entity(
            root.memory(),
            &EntityContent {
                kind: "person".into(),
                title: canonical.into(),
                scope: scope.clone(),
                aliases,
            },
            &[Provenance { turn_uid: uids[0].clone(), anchor_role: AnchorRole::Human }],
            created_at_ms,
        )
        .unwrap();
    }
    // 候选池按会话归属过滤：夹具的锚改挂会话 owner（真实链路由命名 pass 做）。
    adopt_rows(&root, "anchor", "alice", principal, "project", "project", "proj-x");
    (dir, root)
}

fn plan_of(root: &Root) -> FactPlan {
    select_fact_batch(root.transcript(), root.ledger(), root.memory(), 0, FactFamily::Promise, NOW)
        .unwrap()
        .expect("夹具里的轮都还没跑过 fact/promise")
}

fn canonicals(plan: &FactPlan) -> Vec<&str> {
    plan.candidates.iter().map(|candidate| candidate.canonical.as_str()).collect()
}

fn references(plan: &FactPlan) -> Vec<&str> {
    plan.candidates.iter().map(|candidate| candidate.reference.as_str()).collect()
}

/// 渲染出来的候选行（`- a1 | ...`），按出现顺序。
fn rendered_candidates(user: &str) -> Vec<&str> {
    user.lines().filter(|line| line.starts_with("- a")).collect()
}

/// 标记恰一格：`mark` 是该值的候选有且只有一个。
fn only_marked<'a>(plan: &'a FactPlan, mark: &str) -> &'a FactCandidate {
    let mut found: Vec<&FactCandidate> =
        plan.candidates.iter().filter(|candidate| candidate.mark == Some(mark)).collect();
    assert_eq!(found.len(), 1, "标记 [{mark}] 应当恰有一个候选：{found:?}");
    found.pop().unwrap()
}

/// 1a. 自锚渲染带 `[self]`、主人锚带 `[principal]`；其余候选不多一个字符。
///
/// 正文不点任何候选名 ⇒ 顺序仍是 `created_at DESC`（张三、李四），自锚与主人锚接在末尾。
#[test]
fn self_and_principal_candidates_render_with_their_marks() {
    let (_dir, root) = fixture(&["我下周三之前把对账单发给你", "好的，收到"], Some("p1"));
    let plan = plan_of(&root);

    let self_anchor = only_marked(&plan, MARK_SELF);
    assert_eq!(self_anchor.kind, "agent");
    assert_eq!(self_anchor.canonical, "agent-batch-A", "自锚的 canonical = 会话 source_agent");
    let principal = only_marked(&plan, MARK_PRINCIPAL);
    assert_eq!(principal.kind, "person");
    assert_eq!(principal.canonical, "p1", "没有可读名 ⇒ canonical = principal_id");

    let fact = fact_user(&plan.batch, &plan.candidates, FactFamily::Promise, &plan.ctx_turns);
    let event = event_user(&plan.batch, &plan.candidates, &plan.ctx_turns);
    for user in [&fact, &event] {
        let want_self = format!("- {} | agent | agent-batch-A [self]", self_anchor.reference);
        let want_principal = format!("- {} | person | p1 [principal]", principal.reference);
        assert!(user.contains(&want_self), "自锚必须带 [self]：{user}");
        assert!(user.contains(&want_principal), "主人锚必须带 [principal]：{user}");
        assert_eq!(user.matches("[self]").count(), 1, "标记只出现一次：{user}");
        assert_eq!(user.matches("[principal]").count(), 1, "标记只出现一次：{user}");
        // 其余候选（张三/李四）行尾一个字符都不多。
        assert!(user.contains("- a1 | person | 张三\n"), "未标记的候选渲染不变：{user}");
        assert!(user.contains("- a2 | person | 李四\n"), "未标记的候选渲染不变：{user}");
    }
}

/// 2b. 名字在目标轮正文里的候选排到前面；其余保持 `created_at DESC`。
///
/// 张三比李四新（不预筛时张三 a1），但正文点的是李四 ⇒ 李四先；自锚/主人锚不命中，接在最后。
#[test]
fn a_candidate_named_in_a_target_turn_moves_ahead_and_the_rest_keep_created_at_order() {
    let (_dir, root) = fixture(&["李四拆开清理后恢复", "好的，收到"], Some("p1"));
    let plan = plan_of(&root);

    assert_eq!(
        canonicals(&plan),
        vec!["李四", "张三", "agent-batch-A", "p1"],
        "命中的李四在前，未命中的张三保持原序，自锚与主人锚仍接在末尾"
    );
    assert_eq!(references(&plan), vec!["a1", "a2", "a3", "a4"], "重排后按位置重编号");
    assert_eq!(plan.candidates[0].canonical, "李四");
    assert_eq!(plan.candidates[1].canonical, "张三", "未命中的两格相对顺序不变");
}

/// 2b-bis. 命中不只看 canonical：别名「老四」出现在正文里也把李四提到前面。
#[test]
fn an_alias_found_in_the_target_turn_also_moves_the_candidate_forward() {
    let (_dir, root) = fixture(&["老四拆开清理后恢复", "好的，收到"], Some("p1"));
    let plan = plan_of(&root);

    assert_eq!(
        canonicals(&plan),
        vec!["李四", "张三", "agent-batch-A", "p1"],
        "canonical 不在正文里，但别名在 ⇒ 同样命中"
    );
}

/// 3c. 预筛不删候选：名字出现与否，同一夹具的候选总数与名字集合一致，只是顺序不同。
#[test]
fn prescreening_reorders_without_dropping_a_single_candidate() {
    let (_hit_dir, hit_root) = fixture(&["李四拆开清理后恢复", "好的，收到"], Some("p1"));
    let (_plain_dir, plain_root) = fixture(&["我下周三之前把对账单发给你", "好的，收到"], Some("p1"));
    let hit = plan_of(&hit_root);
    let plain = plan_of(&plain_root);

    assert_eq!(hit.candidates.len(), plain.candidates.len(), "预筛不删：候选总数一致");
    let mut hit_names: Vec<&str> = canonicals(&hit);
    let mut plain_names: Vec<&str> = canonicals(&plain);
    hit_names.sort_unstable();
    plain_names.sort_unstable();
    assert_eq!(hit_names, plain_names, "同一批候选，只有顺序不同");
    // 反向对照：不预筛时张三排第一；命中时李四排第一。总数却一样。
    assert_eq!(plain.candidates[0].canonical, "张三");
    assert_eq!(hit.candidates[0].canonical, "李四");
}

/// 4d. `a1..aN` 与渲染位置一一对应：第 i 行就是 `a{i+1}`，且与 `plan.candidates` 同序。
#[test]
fn candidate_references_follow_the_rendered_positions() {
    let (_dir, root) = fixture(&["李四拆开清理后恢复", "好的，收到"], Some("p1"));
    let plan = plan_of(&root);
    let fact = fact_user(&plan.batch, &plan.candidates, FactFamily::Promise, &plan.ctx_turns);
    let lines = rendered_candidates(&fact);

    assert_eq!(lines.len(), plan.candidates.len(), "每一格候选都渲染且只渲染一次");
    for (index, (line, candidate)) in lines.iter().zip(plan.candidates.iter()).enumerate() {
        let reference = format!("a{}", index + 1);
        assert!(line.starts_with(&format!("- {reference} | ")), "第 {index} 行必须是 {reference}：{line}");
        assert_eq!(candidate.reference, reference, "plan 的编号与渲染位置同源");
    }
    // 同一批内编号唯一（位置即契约）。
    let unique: std::collections::BTreeSet<&str> = references(&plan).into_iter().collect();
    assert_eq!(unique.len(), plan.candidates.len(), "同一批内 a* 不重复");
}

/// 5e. 没有主人身份的会话：不出现 `[principal]`，也不建主人锚（自锚照旧带 `[self]`）。
#[test]
fn a_session_without_a_principal_renders_no_principal_mark() {
    let (_dir, root) = fixture(&["我下周三之前把对账单发给你", "好的，收到"], None);
    let plan = plan_of(&root);

    assert!(
        plan.candidates.iter().all(|candidate| candidate.mark != Some(MARK_PRINCIPAL)),
        "没有主人锚就没有 [principal]：{:?}",
        canonicals(&plan)
    );
    assert_eq!(canonicals(&plan), vec!["张三", "李四", "agent-batch-A"], "主人锚一条都不加");
    let fact = fact_user(&plan.batch, &plan.candidates, FactFamily::Promise, &plan.ctx_turns);
    assert!(!fact.contains("[principal]"), "渲染里不许出现 [principal]：{fact}");
    assert_eq!(fact.matches("[self]").count(), 1, "自锚仍带 [self]：{fact}");
}
