//! P2 状态 pass 的库面（方案 `design/2026-10-31-state-pass-p2.md` §3–§7）。
//!
//! 这一层断七件事：选批只认 `(turn, 'state', '')` 且**不碰** event 的表态行；抽取一次调用、
//! 窄审是**另一次**调用；只有审核 pass 的行写进 `state_assertion`（+出处/提及/归属，不产卡）；
//! **阴性对照**——问句与纯宣布被程序闸挡在落库之前、且不调审核；推导值走隔离而不是落库；
//! 同维度异值按 D10 分流（更晚的那条放行给审核、审过顶掉旧值；更早的那条——迟到的旧话——进隔离）；
//! 三类失败的落点与重放的幂等。
//!
//! 正反例只用通用占位名（张三/小海/某网关），不含任何语料专名或 holdout。

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use rc_distill::pass_prompt::{PASS_STATE};
use rc_distill::pass_state::{run_state_with, select_state_batch, StateOutcome};
use rc_distill::review::ReviewMode;
use rc_distill::DistillError;
use rc_ledger::pass_state::{self, PassKey, PassRecord, PassState, REVIEW_PENDING_REASON};
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{AnchorRole, Provenance, Scope, ScopeKind};
use rc_store::{LedgerHandle, MemoryHandle, Root};
use serde_json::{json, Value};

mod common;
use common::{adopt_rows, seed_bodies};

const NOW: i64 = 1_700_000_000_000;
/// 两条直陈（主体名都在正文里 ⇒ 闸的"主语在证据里"天然成立）。
const ANXIOUS_BODY: &str = "张三这两天焦虑得很，晚上睡不着。";
const CAT_BODY: &str = "小海家的小花猫这两天不吃东西，蔫得很。";
/// D19 的两条夹具：同一形状（"我"字陈述），差别只在**是谁说的轮**。
const I_WORRIED_BODY: &str = "老师，我很担心自己跟不上，进度太快了。";
const I_RAN_SMOOTH_BODY: &str = "我这轮跑得挺顺，测试都过了。";
/// 同一主体同一维度的**另一个值**（值逐字在引文里，才轮得到冲突闸而不是隔离）。
const CALM_BODY: &str = "张三这两天平静得很，睡得不错。";
/// 阴性对照的两轮：一问一宣布。
const QUESTION_BODY: &str = "张三你是不是生气了？";
const ANNOUNCE_BODY: &str = "现在我去跑一遍全量测试";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-x".into() }
}

fn seeded(bodies: &[&str]) -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, bodies, 1_000);
    root.transcript_mut()
        .conn()
        .execute(
            "UPDATE sessions SET owner_agent_id='alice', visibility='project',
                    retention='keep', scope_kind='project', scope_key='proj-x',
                    ownership_src='operator'
             WHERE session_key='sess-batch-A' AND device_id='dev-batch-A'",
            [],
        )
        .unwrap();
    for (index, canonical) in ["张三", "小海", "小花猫", "某网关"].iter().enumerate() {
        put_entity(
            root.memory(),
            &EntityContent {
                kind: if *canonical == "小花猫" { "animal" } else { "person" }.into(),
                title: (*canonical).into(),
                scope: scope(),
                aliases: vec![],
            },
            &[Provenance { turn_uid: uids[0].clone(), anchor_role: AnchorRole::Human }],
            NOW - 1_000 * (index as i64 + 1),
        )
        .unwrap();
    }
    adopt_rows(&root, "anchor", "alice", None, "project", "project", "proj-x");
    (dir, root, uids)
}

/// 按名字取候选短引用（`a*` 的位置随证据预筛漂动，测试不许猜位置）。
fn aref(plan: &rc_distill::pass_state::StatePlan, canonical: &str) -> String {
    plan.candidates
        .iter()
        .find(|candidate| candidate.canonical == canonical)
        .unwrap_or_else(|| panic!("候选表里没有 {canonical}"))
        .reference
        .clone()
}

fn tref(plan: &rc_distill::pass_state::StatePlan, turn_uid: &str) -> String {
    plan.batch
        .rows
        .iter()
        .position(|row| row.turn_uid == turn_uid)
        .map(|index| format!("t{}", index + 1))
        .unwrap_or_else(|| panic!("批里没有这一轮"))
}

fn state_row(
    plan: &rc_distill::pass_state::StatePlan,
    reference: &str,
    turn_uid: &str,
    canonical: &str,
    dimension: &str,
    value: &str,
    quote: &str,
) -> Value {
    json!({
        "ref": reference, "turn": tref(plan, turn_uid), "subject": aref(plan, canonical),
        "dimension": dimension, "value_after": value, "quote": quote,
        "evidence": [tref(plan, turn_uid)]
    })
}

fn extract_reply(states: Vec<Value>, empty: Vec<&str>) -> String {
    json!({"schema_version": 0, "pass": "state", "states": states, "empty_turns": empty})
        .to_string()
}

/// 一条窄审回复。`code` 只在 reject 时必填（`pass` 与 `uncertain` 允许 null）。
fn review_reply(decisions: &[(&str, &str, Option<&str>)]) -> String {
    let verdicts: Vec<Value> = decisions
        .iter()
        .map(|(reference, verdict, code)| {
            json!({"ref": reference, "verdict": verdict, "code": code})
        })
        .collect();
    json!({"schema_version": 0, "pass": "state_review", "verdicts": verdicts}).to_string()
}

/// 窄审按片调用（一片 ≤2 条，见 `REVIEW_MAX_ITEMS`）：`n` 条草稿 ⇒ 若干份回复，
/// 每份按**片内位置**回答 `s1..sk`。
fn review_passes(n: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut left = n;
    while left > 0 {
        let size = left.min(2);
        let verdicts: Vec<(&str, &str, Option<&str>)> = match size {
            2 => vec![("s1", "pass", None), ("s2", "pass", None)],
            _ => vec![("s1", "pass", None)],
        };
        out.push(review_reply(&verdicts));
        left -= size;
    }
    out
}

/// 抽取一次 + 窄审 `reviews.len()` 次的后端。
fn backend_of(extraction: String, reviews: Vec<String>) -> ScriptBackend {
    let mut script: Vec<Result<String, LlmFailure>> = vec![Ok(extraction)];
    script.extend(reviews.into_iter().map(Ok));
    ScriptBackend::new(script)
}

struct ScriptBackend {
    replies: RefCell<VecDeque<Result<String, LlmFailure>>>,
    calls: Cell<usize>,
}

impl ScriptBackend {
    fn new(replies: Vec<Result<String, LlmFailure>>) -> Self {
        ScriptBackend { replies: RefCell::new(replies.into()), calls: Cell::new(0) }
    }
    fn two(extraction: String, review: String) -> Self {
        ScriptBackend::new(vec![Ok(extraction), Ok(review)])
    }
}

impl LlmBackend for ScriptBackend {
    fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
        self.calls.set(self.calls.get() + 1);
        self.replies.borrow_mut().pop_front().unwrap_or(Err(LlmFailure::Timeout))
    }
    fn slug(&self) -> Option<&str> {
        Some("fake-state-model")
    }
}

fn plan_of(root: &Root) -> rc_distill::pass_state::StatePlan {
    select_state_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("夹具里的轮都还没跑过 state")
}

fn key_of(uid: &str) -> PassKey<'_> {
    PassKey { turn_uid: uid, pass_id: PASS_STATE, family: "" }
}

fn event_key_of(uid: &str) -> PassKey<'_> {
    PassKey { turn_uid: uid, pass_id: "event", family: "" }
}

fn record(l: &LedgerHandle, uid: &str) -> PassRecord {
    pass_state::get(l, key_of(uid))
        .unwrap()
        .unwrap_or_else(|| panic!("{uid} 没有 state 状态行"))
}

/// 全表计数（只用于 `card`/`fact` 这种"必须为 0"的断言；出处/提及/归属那一组按
/// `mem_table='state_assertion'` 过滤，见下面的用例）。
fn count(m: &MemoryHandle, table: &str) -> i64 {
    m.conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// `(维度, 值, 前值, 模式, 有效期止)`——冲突与覆盖语义的断言面。
type StateRow = (String, String, Option<String>, String, Option<i64>);

fn state_rows(m: &MemoryHandle) -> Vec<StateRow> {
    let mut st = m
        .conn()
        .prepare(
            "SELECT dimension, value_after, value_before, mode, valid_to_ms
             FROM state_assertion ORDER BY valid_from_ms, id",
        )
        .unwrap();
    st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<i64>>(4)?,
        ))
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

/// 跑一轮（默认 Inline 审核）。
fn run(root: &mut Root, plan: &rc_distill::pass_state::StatePlan, backend: &ScriptBackend)
    -> Result<StateOutcome, DistillError> {
    run_state_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        plan,
        backend,
        NOW,
        ReviewMode::Inline,
    )
}

// ── 1. 选批 ────────────────────────────────────────────────────────────────────

#[test]
fn select_uses_the_state_key_and_leaves_the_event_rows_alone() {
    let (_dir, root, uids) = seeded(&[ANXIOUS_BODY, CAT_BODY]);
    let plan = plan_of(&root);
    assert_eq!(plan.batch.rows.len(), 2);
    assert_eq!(plan.system_turns_skipped, 0);
    assert_eq!(plan.owner.owner_agent_id, "alice");
    assert_eq!(plan.scope.key, "proj-x");
    assert!(plan.candidates.iter().any(|c| c.canonical == "张三"));
    assert!(plan.existing.is_empty(), "库里还没有状态行");
    run_state_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &ScriptBackend::two(
            extract_reply(
                vec![state_row(&plan, "s1", &uids[0], "张三", "emotion", "焦虑得很", ANXIOUS_BODY)],
                vec![&tref(&plan, &uids[1])],
            ),
            review_reply(&[("s1", "pass", None)]),
        ),
        NOW,
        ReviewMode::Inline,
    )
    .unwrap();
    // state 的表态只落在 state 那一行；event 的键一格都没写过（双主链各说各话的前提）。
    assert!(record(root.ledger(), &uids[0]).state == PassState::Done);
    assert!(pass_state::get(root.ledger(), event_key_of(&uids[0])).unwrap().is_none());
    assert!(pass_state::get(root.ledger(), event_key_of(&uids[1])).unwrap().is_none());
}

/// 主体只能是有锚的实体：候选表里没有的名字，模型引用它就只能被拒。
#[test]
fn unknown_subject_reference_drops_that_row_only() {
    let (_dir, mut root, uids) = seeded(&[ANXIOUS_BODY]);
    let plan = plan_of(&root);
    // 单条坏不折整批：这条被宽松 parser 丢掉 ⇒ 批里没有可审的草稿 ⇒ 不调审核。
    let reply = extract_reply(
        vec![json!({
            "ref": "s1", "turn": "t1", "subject": "a99", "dimension": "emotion",
            "value_after": "焦虑得很", "quote": ANXIOUS_BODY, "evidence": ["t1"]
        })],
        Vec::new(),
    );
    let backend = ScriptBackend::new(vec![Ok(reply)]);
    let out = run(&mut root, &plan, &backend).unwrap();
    assert_eq!(backend.calls.get(), 1, "没有草稿就不该问第二次");
    assert_eq!(out.drafts, 0);
    assert_eq!(out.dropped, 1);
    assert_eq!(record(root.ledger(), &uids[0]).reason.as_deref(), Some("unknown_candidate"));
    assert_eq!(count(root.memory(), "state_assertion"), 0);
}

// ── 2. 落库 ────────────────────────────────────────────────────────────────────

#[test]
fn passed_rows_land_in_state_assertion_with_provenance_and_owner() {
    let (_dir, mut root, uids) = seeded(&[ANXIOUS_BODY, CAT_BODY]);
    let plan = plan_of(&root);
    let extraction = extract_reply(
        vec![
            state_row(&plan, "s1", &uids[0], "张三", "emotion", "焦虑得很", ANXIOUS_BODY),
            state_row(&plan, "s2", &uids[1], "小花猫", "health", "不吃东西", CAT_BODY),
        ],
        Vec::new(),
    );
    // 窄审按片调用：两条草稿落在同一片里 ⇒ 一次开口。
    let backend = backend_of(extraction, review_passes(2));
    let out = run(&mut root, &plan, &backend).unwrap();
    assert_eq!(backend.calls.get(), 2, "抽取一次 + 窄审一片一次");
    assert_eq!(out.drafts, 2);
    assert_eq!(out.written, 2);
    assert_eq!(out.states, 2);
    assert_eq!(out.duplicate, 0);
    assert_eq!(out.inferred, 0);
    assert_eq!(out.conflicts, 0);
    assert_eq!(out.state, PassState::Done);
    let got = state_rows(root.memory());
    assert_eq!(got.len(), 2);
    assert!(got.iter().all(|(_, _, before, _, to)| before.is_none() && to.is_none()));
    // 按**放行支**逐条断言（C1 之前这里是一条"全部 expressed"的笼统断言）：两行的主语名
    // 都落在承载值的那一句里 ⇒ 第 2 支的"值句共现" ⇒ `expressed`。
    let modes: Vec<&str> = got.iter().map(|(_, _, _, mode, _)| mode.as_str()).collect();
    assert_eq!(
        modes,
        vec!["expressed", "expressed"],
        "主语与值同句是这一格的唯一来源（无源行见第 9 节）"
    );
    for table in ["provenance", "memory_mention", "resource_owner"] {
        let n: i64 = root
            .memory()
            .conn()
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE mem_table='state_assertion'"),
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(n, 2, "{table} 里属于状态行的那几格");
    }
    // 不产卡、不写 fact：状态的消费面读的就是这张表。
    assert_eq!(count(root.memory(), "card"), 0);
    assert_eq!(count(root.memory(), "fact"), 0);
    assert_eq!(record(root.ledger(), &uids[0]).state, PassState::Done);
    assert_eq!(
        (record(root.ledger(), &uids[1]).state, record(root.ledger(), &uids[1]).reason.clone()),
        (PassState::Done, None),
        "第二轮的终态与理由"
    );
}

/// 重放：同一批再跑一次不写第二行（`done` 的轮不再被选中）。
#[test]
fn a_done_turn_is_not_reselected_under_the_same_prompt() {
    let (_dir, mut root, uids) = seeded(&[ANXIOUS_BODY]);
    let plan = plan_of(&root);
    let extraction = extract_reply(
        vec![state_row(&plan, "s1", &uids[0], "张三", "emotion", "焦虑得很", ANXIOUS_BODY)],
        Vec::new(),
    );
    run(&mut root, &plan, &backend_of(extraction, review_passes(1))).unwrap();
    assert!(
        select_state_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW).unwrap().is_none(),
        "同一判据下已完成的轮不许重抽"
    );
    assert_eq!(count(root.memory(), "state_assertion"), 1);
}

// ── 3. 阴性对照：闸必须拒 ──────────────────────────────────────────────────────
/// D19 的闸：human 轮说的"我"，**不许**记到本会话 agent 的自锚上。
///
/// 现场证据是 §10.133 的三路审计（21 行里 7 行错挂、锚表里 human 锚 0 条）：来访者说
/// "我很担心自己跟不上"，落库成 `cpsycoun(agent) / emotion =「很担心」`——消费面会把来访者的
/// 情绪当应用自己的状态端出来。6c 在"会话身份未知"时是 fail-open（那是 I2 的既有设计，
/// 不为它改），所以这一格必须由一条**只针对这一形状**的确定性检查补上。
///
/// 一条用例守一条规则，两脸都放进来：agent 自己说"我"时挂自锚仍必须放行——
/// 那 21 行里 8 行"合规"全是这一类，砍掉它就是砍能力。
#[test]
fn a_human_turn_never_borrows_the_sessions_own_agent_anchor() {
    let (_dir, mut root, uids) = seeded(&[I_WORRIED_BODY, I_RAN_SMOOTH_BODY]);
    let plan = plan_of(&root);
    let extraction = extract_reply(
        vec![
            state_row(&plan, "s1", &uids[0], "agent-batch-A", "emotion", "很担心", I_WORRIED_BODY),
            state_row(&plan, "s2", &uids[1], "agent-batch-A", "mood", "挺顺", I_RAN_SMOOTH_BODY),
        ],
        Vec::new(),
    );
    let backend = backend_of(extraction, review_passes(1));
    let out = run(&mut root, &plan, &backend).unwrap();
    // `drafts` 是**契约之后**的合法条数，程序闸在它之后判：两条都算草稿，其中一条被闸拒。
    assert_eq!(out.drafts, 2);
    assert_eq!(out.rejected, 1, "human 轮那条要被闸拦下，只剩 agent 轮那条进窄审");
    assert_eq!(out.states, 1, "agent 轮自挂自锚是正解，不许被同一格连带砍掉");
    assert_eq!(record(root.ledger(), &uids[0]).reason.as_deref(),
               Some("subject_self_on_human_turn"), "拒收理由要单列一格，不许混进 not_assertive");
    assert_eq!(record(root.ledger(), &uids[1]).state, PassState::Done);
    let got = state_rows(root.memory());
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1, "挺顺", "落库的只能是 agent 自己那一条");
}

/// 方案 §7 的阴性对照：**纯提问与纯宣布**的引文进不了库，而且一次审核都不问。
#[test]
fn questions_and_announcements_are_refused_before_any_review_call() {
    let (_dir, mut root, uids) = seeded(&[QUESTION_BODY, ANNOUNCE_BODY]);
    let plan = plan_of(&root);
    let extraction = extract_reply(
        vec![
            state_row(&plan, "s1", &uids[0], "张三", "emotion", "生气", QUESTION_BODY),
            state_row(&plan, "s2", &uids[1], "agent-batch-A", "progress", "跑一遍全量测试", ANNOUNCE_BODY),
        ],
        Vec::new(),
    );
    let backend = ScriptBackend::new(vec![Ok(extraction)]);
    let out = run(&mut root, &plan, &backend).unwrap();
    assert_eq!(backend.calls.get(), 1, "没有可审的草稿 ⇒ 窄审一次都不该开口");
    assert_eq!(out.drafts, 0);
    assert_eq!(out.dropped, 2);
    assert_eq!(out.states, 0);
    assert_eq!(out.state, PassState::Rejected);
    for uid in &uids {
        assert_eq!(record(root.ledger(), uid).state, PassState::Rejected);
        assert_eq!(record(root.ledger(), uid).reason.as_deref(), Some("not_assertive"));
    }
    assert_eq!(count(root.memory(), "state_assertion"), 0);
}

/// 反面对照的对照：同一批里真陈述的那条照常落库（闸不是"整批拒"）。
#[test]
fn a_question_next_to_a_statement_does_not_block_the_statement() {
    let (_dir, mut root, uids) = seeded(&[QUESTION_BODY, ANXIOUS_BODY]);
    let plan = plan_of(&root);
    let extraction = extract_reply(
        vec![
            state_row(&plan, "s1", &uids[0], "张三", "emotion", "生气", QUESTION_BODY),
            state_row(&plan, "s2", &uids[1], "张三", "emotion", "焦虑得很", ANXIOUS_BODY),
        ],
        Vec::new(),
    );
    let backend = ScriptBackend::two(extraction, review_reply(&[("s1", "pass", None)]));
    let out = run(&mut root, &plan, &backend).unwrap();
    assert_eq!(out.drafts, 1, "问句那条被丢掉，剩下的一条进审核");
    assert_eq!(out.states, 1);
    assert_eq!(record(root.ledger(), &uids[0]).reason.as_deref(), Some("not_assertive"));
    let second = record(root.ledger(), &uids[1]);
    assert_eq!(
        (second.state, second.reason.clone(), out.state, out.rejected, out.uncertain),
        (PassState::Done, None, PassState::Done, 0, 0),
        "同一批里真陈述的那条不该被问句连坐"
    );
}

// ── 4. inferred 的隔离 ─────────────────────────────────────────────────────────

#[test]
fn a_reworded_value_is_quarantined_and_never_lands() {
    let (_dir, mut root, uids) = seeded(&[ANXIOUS_BODY]);
    let plan = plan_of(&root);
    // 值换成库里没有的词："压力"没出现在引文里 ⇒ 程序判它是推导。
    let extraction = extract_reply(
        vec![state_row(&plan, "s1", &uids[0], "张三", "emotion", "压力很大", ANXIOUS_BODY)],
        Vec::new(),
    );
    let backend = ScriptBackend::new(vec![Ok(extraction)]);
    let out = run(&mut root, &plan, &backend).unwrap();
    assert_eq!(out.inferred, 1);
    assert_eq!(out.drafts, 0);
    assert_eq!(out.dropped, 0, "推导不是拒收");
    assert_eq!(out.states, 0);
    assert_eq!(backend.calls.get(), 1, "隔离的条不进审核");
    assert_eq!(record(root.ledger(), &uids[0]).state, PassState::Quarantined);
    assert_eq!(record(root.ledger(), &uids[0]).reason.as_deref(), Some("inferred_deferred"));
    assert_eq!(count(root.memory(), "state_assertion"), 0);
}

// ── 5. 同维度异值 = D10 的两条确定性规则 ──────────────────────────────────────

/// **更新的那一条不再被闸拦下**（D10 §4 第四行）：它照样进窄审，审过之后由写库层在同一事务里
/// 顶掉旧值。这一条证的是"只拦不裁会把当前状态冻结在第一条上"这个缺陷被修掉了。
#[test]
fn a_later_value_on_the_same_dimension_is_reviewed_and_supersedes() {
    let (_dir, mut root, uids) = seeded(&[ANXIOUS_BODY, CALM_BODY]);
    let plan = plan_of(&root);
    let first = extract_reply(
        vec![state_row(&plan, "s1", &uids[0], "张三", "emotion", "焦虑得很", ANXIOUS_BODY)],
        vec![&tref(&plan, &uids[1])],
    );
    run(&mut root, &plan, &backend_of(first, review_passes(1))).unwrap();
    assert_eq!(state_rows(root.memory()).len(), 1);

    root.ledger_mut()
        .conn()
        .execute("UPDATE extract_pass_state SET state='stale' WHERE pass_id='state'", [])
        .unwrap();
    let plan2 = plan_of(&root);
    let second = extract_reply(
        vec![state_row(&plan2, "s1", &uids[1], "张三", "emotion", "平静得很", CALM_BODY)],
        vec![&tref(&plan2, &uids[0])],
    );
    let backend = backend_of(second, review_passes(1));
    let out = run(&mut root, &plan2, &backend).unwrap();
    assert_eq!(out.conflicts, 0, "更晚的那一条不是\"待裁的冲突\"，它是来顶掉旧值的");
    assert_eq!(out.drafts, 1);
    assert_eq!(out.states, 1, "审过就要落库");
    assert_eq!(backend.calls.get(), 2, "抽取 + 窄审，两次都要开");
    assert_eq!(record(root.ledger(), &uids[1]).state, PassState::Done);

    let got = state_rows(root.memory());
    assert_eq!(got.len(), 2, "旧行不删：只关有效期");
    assert_eq!(got[0].1, "焦虑得很");
    assert!(got[0].4.is_some(), "旧行被关掉：终点=新断言的证据轮时刻（逐格数值见 writers_state）");
    assert_eq!(got[1].1, "平静得很");
    assert_eq!(got[1].4, None, "当前值是后来那一条");
    assert_eq!(got[1].2.as_deref(), Some("焦虑得很"), "新行带上前值");
}

/// **迟到的旧话**仍然进隔离（D10 §3 第二支的闸侧那一半）：它不改写现在，是"补一段历史"，
/// 由人（或 `maintain`）看，而不是让写库层拿导入顺序当事件顺序。
#[test]
fn a_late_arrival_is_isolated_not_written() {
    let (_dir, mut root, uids) = seeded(&[ANXIOUS_BODY, CALM_BODY]);
    let plan = plan_of(&root);
    // 先落的是**更晚**那一轮说的话。
    let first = extract_reply(
        vec![state_row(&plan, "s1", &uids[1], "张三", "emotion", "平静得很", CALM_BODY)],
        vec![&tref(&plan, &uids[0])],
    );
    run(&mut root, &plan, &backend_of(first, review_passes(1))).unwrap();
    assert_eq!(state_rows(root.memory()).len(), 1);

    root.ledger_mut()
        .conn()
        .execute("UPDATE extract_pass_state SET state='stale' WHERE pass_id='state'", [])
        .unwrap();
    let plan2 = plan_of(&root);
    // 再导入的是**更早**那一轮的同一维度异值。
    let second = extract_reply(
        vec![state_row(&plan2, "s1", &uids[0], "张三", "emotion", "焦虑得很", ANXIOUS_BODY)],
        vec![&tref(&plan2, &uids[1])],
    );
    let backend = backend_of(second, Vec::new());
    let out = run(&mut root, &plan2, &backend).unwrap();
    assert_eq!(out.conflicts, 1, "迟到的同维度异值要被闸数出来");
    assert_eq!(out.drafts, 1);
    assert_eq!(out.states, 0, "隔离的条一行都不写");
    assert_eq!(backend.calls.get(), 1, "没有可审的好草稿 ⇒ 不问审核");
    assert_eq!(record(root.ledger(), &uids[0]).state, PassState::Quarantined);
    assert_eq!(
        record(root.ledger(), &uids[0]).reason.as_deref(),
        Some("state_conflict_same_dim")
    );
    let got = state_rows(root.memory());
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1, "平静得很", "当前值不许被一句旧话顶掉");
    assert_eq!(got[0].4, None);
}

/// 同维度**同值**换一轮再说一遍：幂等跳过并计数，不写第二行。
#[test]
fn repeating_the_same_value_from_another_turn_counts_dedup() {
    let (_dir, mut root, uids) = seeded(&[ANXIOUS_BODY]);
    let plan = plan_of(&root);
    let extraction = extract_reply(
        vec![state_row(&plan, "s1", &uids[0], "张三", "emotion", "焦虑得很", ANXIOUS_BODY)],
        Vec::new(),
    );
    run(&mut root, &plan, &ScriptBackend::two(extraction, review_reply(&[("s1", "pass", None)]))).unwrap();
    root.ledger_mut()
        .conn()
        .execute("UPDATE extract_pass_state SET state='stale' WHERE pass_id='state'", [])
        .unwrap();
    let plan2 = plan_of(&root);
    let again = extract_reply(
        vec![state_row(&plan2, "s1", &uids[0], "张三", "emotion", "焦虑得很", ANXIOUS_BODY)],
        Vec::new(),
    );
    let out = run(&mut root, &plan2, &ScriptBackend::two(again, review_reply(&[("s1", "pass", None)]))).unwrap();
    assert_eq!(out.written, 1, "交出去写的条数是一");
    assert_eq!(out.duplicate, 1, "写库层的第二道幂等把它吞了");
    assert_eq!(out.states, 0);
    assert_eq!(count(root.memory(), "state_assertion"), 1);
}

// ── 6. 审核判决 ────────────────────────────────────────────────────────────────

#[test]
fn review_reject_drops_the_row_and_marks_the_turn() {
    let (_dir, mut root, uids) = seeded(&[ANXIOUS_BODY]);
    let plan = plan_of(&root);
    let extraction = extract_reply(
        vec![state_row(&plan, "s1", &uids[0], "张三", "emotion", "焦虑得很", ANXIOUS_BODY)],
        Vec::new(),
    );
    let out = run(
        &mut root,
        &plan,
        &ScriptBackend::two(extraction, review_reply(&[("s1", "reject", Some("value_drift"))])),
    )
    .unwrap();
    assert_eq!(out.rejected, 1);
    assert_eq!(out.states, 0);
    assert_eq!(out.state, PassState::Rejected);
    assert_eq!(record(root.ledger(), &uids[0]).reason.as_deref(), Some("review_reject"));
    assert_eq!(count(root.memory(), "state_assertion"), 0);
}

#[test]
fn review_uncertain_quarantines_the_turn() {
    let (_dir, mut root, uids) = seeded(&[ANXIOUS_BODY]);
    let plan = plan_of(&root);
    let extraction = extract_reply(
        vec![state_row(&plan, "s1", &uids[0], "张三", "emotion", "焦虑得很", ANXIOUS_BODY)],
        Vec::new(),
    );
    let verdicts = json!({"schema_version": 0, "pass": "state_review", "verdicts": [
        {"ref": "s1", "verdict": "uncertain", "code": null}]}).to_string();
    let out = run(&mut root, &plan, &ScriptBackend::two(extraction, verdicts)).unwrap();
    assert_eq!(out.uncertain, 1);
    assert_eq!(out.states, 0);
    assert_eq!(record(root.ledger(), &uids[0]).state, PassState::Quarantined);
    assert_eq!(record(root.ledger(), &uids[0]).reason.as_deref(), Some("review_uncertain"));
}

/// `--no-review`：入队但不问模型、不写行。
#[test]
fn enqueue_only_mode_leaves_the_row_unwritten() {
    let (_dir, root, uids) = seeded(&[ANXIOUS_BODY]);
    let plan = plan_of(&root);
    let extraction = extract_reply(
        vec![state_row(&plan, "s1", &uids[0], "张三", "emotion", "焦虑得很", ANXIOUS_BODY)],
        Vec::new(),
    );
    let backend = ScriptBackend::new(vec![Ok(extraction)]);
    let out = run_state_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
        ReviewMode::EnqueueOnly,
    )
    .unwrap();
    assert_eq!(backend.calls.get(), 1);
    assert_eq!(out.state, PassState::Pending);
    assert_eq!(out.states, 0);
    assert_eq!(record(root.ledger(), &uids[0]).reason.as_deref(), Some(REVIEW_PENDING_REASON));
    assert_eq!(count(root.memory(), "state_assertion"), 0);
    let queued: i64 = root
        .memory()
        .conn()
        .query_row("SELECT count(*) FROM review_item WHERE pass_id='state'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(queued, 1, "入队了却没问模型");
}

/// 隔离的 item 由 `hub review run` 补审（同一条载荷，不重抽）。
#[test]
fn the_enqueued_item_can_be_drained_by_the_review_queue() {
    let (_dir, root, uids) = seeded(&[ANXIOUS_BODY]);
    let plan = plan_of(&root);
    let extraction = extract_reply(
        vec![state_row(&plan, "s1", &uids[0], "张三", "emotion", "焦虑得很", ANXIOUS_BODY)],
        Vec::new(),
    );
    run_state_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &ScriptBackend::new(vec![Ok(extraction)]),
        NOW,
        ReviewMode::EnqueueOnly,
    )
    .unwrap();
    let item_id: i64 = root
        .memory()
        .conn()
        .query_row("SELECT id FROM review_item WHERE pass_id='state'", [], |r| r.get(0))
        .unwrap();
    let backend = ScriptBackend::new(vec![Ok(review_reply(&[("s1", "pass", None)]))]);
    rc_distill::review::run::run_item(
        root.memory(),
        root.transcript(),
        root.ledger(),
        item_id,
        &backend,
        NOW + 1,
    )
    .unwrap();
    assert_eq!(count(root.memory(), "state_assertion"), 1, "补审后行才落库");
}

// ── 7. 失败面 ──────────────────────────────────────────────────────────────────

#[test]
fn llm_failure_backs_off_without_writing_rows() {
    let (_dir, mut root, uids) = seeded(&[ANXIOUS_BODY]);
    let plan = plan_of(&root);
    let backend = ScriptBackend::new(vec![Err(LlmFailure::BackendError)]);
    let err = run(&mut root, &plan, &backend).unwrap_err();
    assert!(matches!(err, DistillError::Llm(LlmFailure::BackendError)));
    assert_eq!(record(root.ledger(), &uids[0]).state, PassState::Pending);
    assert_eq!(count(root.memory(), "state_assertion"), 0);
    // 退避未到期不可重选；到期后可。
    assert!(select_state_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW - 1)
        .unwrap()
        .is_none());
    assert!(select_state_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW + 61_000)
        .unwrap()
        .is_some());
}

#[test]
fn a_broken_envelope_rejects_every_turn_and_keeps_the_artifact() {
    let (_dir, mut root, uids) = seeded(&[ANXIOUS_BODY]);
    let plan = plan_of(&root);
    for (raw, slug) in [
        (json!({"schema_version": 9, "pass": "state", "states": [], "empty_turns": []}).to_string(), "version"),
        (json!({"schema_version": 0, "pass": "event", "states": [], "empty_turns": []}).to_string(), "pass"),
        ("不是 json".to_string(), "json"),
    ] {
        root.ledger_mut()
            .conn()
            .execute("DELETE FROM extract_pass_state WHERE pass_id='state'", [])
            .unwrap();
        let err = run(&mut root, &plan, &ScriptBackend::new(vec![Ok(raw)])).unwrap_err();
        assert!(matches!(err, DistillError::State(_)), "{slug} 该走契约族");
        let rec = record(root.ledger(), &uids[0]);
        assert_eq!(rec.state, PassState::Rejected);
        assert_eq!(rec.reason.as_deref(), Some(slug));
    }
    let artifacts: i64 = root
        .ledger()
        .conn()
        .query_row(
            "SELECT count(*) FROM extract_pass_artifact WHERE pass_id='state'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    assert!(artifacts >= 1, "信封坏要留得下原文：{artifacts}");
    assert_eq!(count(root.memory(), "state_assertion"), 0);
}

/// 空产出（全是 `empty_turns`）是合法结果：不调审核、逐轮 `empty`。
#[test]
fn an_all_empty_answer_is_a_legal_batch() {
    let (_dir, mut root, uids) = seeded(&[ANXIOUS_BODY, CAT_BODY]);
    let plan = plan_of(&root);
    let extraction = extract_reply(
        Vec::new(),
        vec![&tref(&plan, &uids[0]), &tref(&plan, &uids[1])],
    );
    let backend = ScriptBackend::new(vec![Ok(extraction)]);
    let out = run(&mut root, &plan, &backend).unwrap();
    assert_eq!(backend.calls.get(), 1);
    assert_eq!(out.turns, 2);
    assert_eq!(out.drafts, 0);
    assert_eq!(out.state, PassState::Empty);
    for uid in &uids {
        assert_eq!(record(root.ledger(), uid).state, PassState::Empty);
        assert_eq!(record(root.ledger(), uid).reason.as_deref(), None);
    }
}

// ── 8. 整批程序拒收不留 running（债 #15）──────────────────────────────────────
/// D19 同型的第二条 human"我"陈述（正文不同、形状相同，凑出"两条都被闸拒"的整批）。
const I_DRAINED_BODY: &str = "老师，我觉得自己效率很低，老走神。";

/// **整批草稿被程序闸逐条拒**（批级前置没命中、`all_failed`）：被拒草稿的轮按各自的码落，
/// **其余批内轮也必须到终态**。它们已经 `mark_running`，早退不推就停 `running`、
/// `reason=NULL`，只能等 `reclaim_running` 兜底——债 #15 在 I-0b 实测的 4-running 形状。
/// 这里三种残留形状各钉一格：推导轮、漏写轮（uncovered）、被 `empty_turns` 宣告的轮。
#[test]
fn whole_batch_program_reject_leaves_no_turn_running() {
    let (_dir, mut root, uids) = seeded(&[
        I_WORRIED_BODY,
        ANXIOUS_BODY,
        I_DRAINED_BODY,
        ANNOUNCE_BODY,
        QUESTION_BODY,
    ]);
    let plan = plan_of(&root);
    let extraction = extract_reply(
        vec![
            // 两条直陈都把 human 轮的"我"挂到本会话 agent 自锚 ⇒ 程序闸逐条拒。
            state_row(&plan, "s1", &uids[0], "agent-batch-A", "emotion", "很担心", I_WORRIED_BODY),
            // 值不在引文里 ⇒ 推导：占住这一轮的覆盖，不进审核、不落库。
            state_row(&plan, "s2", &uids[1], "张三", "emotion", "压力很大", ANXIOUS_BODY),
            state_row(&plan, "s3", &uids[2], "agent-batch-A", "emotion", "很低", I_DRAINED_BODY),
        ],
        vec![&tref(&plan, &uids[4])],
    );
    // uids[3] 既没草稿也没进 empty_turns ⇒ uncovered（模型漏写）。
    let backend = ScriptBackend::new(vec![Ok(extraction)]);
    let out = run(&mut root, &plan, &backend).unwrap();
    assert_eq!(backend.calls.get(), 1, "整批被闸拒 ⇒ 窄审一次都不开");
    assert_eq!(out.drafts, 2);
    assert_eq!(out.inferred, 1);
    assert_eq!(out.uncovered, 1);
    assert_eq!(out.conflicts, 0);
    assert_eq!(out.state, PassState::Rejected);
    // 逐格终态断言：被拒草稿的轮带各自的码；推导轮隔离；漏写轮 rejected(uncovered)；
    // 宣告空的轮 empty。
    let rec = record(root.ledger(), &uids[0]);
    assert_eq!(rec.state, PassState::Rejected);
    assert_eq!(rec.reason.as_deref(), Some("subject_self_on_human_turn"));
    let rec = record(root.ledger(), &uids[2]);
    assert_eq!(rec.state, PassState::Rejected);
    assert_eq!(rec.reason.as_deref(), Some("subject_self_on_human_turn"));
    let rec = record(root.ledger(), &uids[1]);
    assert_eq!(rec.state, PassState::Quarantined);
    assert_eq!(rec.reason.as_deref(), Some("inferred_deferred"));
    let rec = record(root.ledger(), &uids[3]);
    assert_eq!(rec.state, PassState::Rejected);
    assert_eq!(rec.reason.as_deref(), Some("uncovered"));
    let rec = record(root.ledger(), &uids[4]);
    assert_eq!(rec.state, PassState::Empty);
    assert_eq!(rec.reason.as_deref(), None);
    // running 残留 = 0：八轮里四轮停摆的那一格，在这里一格都不许有。
    for uid in &uids {
        let rec = record(root.ledger(), uid);
        assert_ne!(rec.state, PassState::Running, "{uid} 停在 running（债 #15 的泄漏形状）");
    }
    assert_eq!(count(root.memory(), "state_assertion"), 0);
}

/// **阳性对照（部分通过）**：同一条闸只拒 human"我"那一条，agent 轮的直陈照常过闸、
/// 过审、落库——证明修的不是"把所有轮硬推成 rejected"。混合批在修复前后都必须是
/// 这个形状（它走的是既有正常路径，不经过整批早退支）。
#[test]
fn a_partial_program_reject_still_lands_the_good_draft() {
    let (_dir, mut root, uids) =
        seeded(&[I_WORRIED_BODY, I_RAN_SMOOTH_BODY, ANXIOUS_BODY, CAT_BODY]);
    let plan = plan_of(&root);
    let extraction = extract_reply(
        vec![
            state_row(&plan, "s1", &uids[0], "agent-batch-A", "emotion", "很担心", I_WORRIED_BODY),
            state_row(&plan, "s2", &uids[1], "agent-batch-A", "mood", "挺顺", I_RAN_SMOOTH_BODY),
        ],
        Vec::new(),
    );
    // uids[2]/uids[3] 既没草稿也没进 empty_turns ⇒ 正常路径里它们落 rejected(uncovered)。
    let backend = backend_of(extraction, review_passes(1));
    let out = run(&mut root, &plan, &backend).unwrap();
    assert_eq!(backend.calls.get(), 2, "有一条好草稿 ⇒ 抽取 + 窄审两次开口");
    assert_eq!(out.drafts, 2);
    assert_eq!(out.states, 1, "只有 agent 轮那条落库");
    let rec = record(root.ledger(), &uids[0]);
    assert_eq!(rec.state, PassState::Rejected);
    assert_eq!(rec.reason.as_deref(), Some("subject_self_on_human_turn"));
    let rec = record(root.ledger(), &uids[1]);
    assert_eq!(rec.state, PassState::Done);
    assert_eq!(rec.reason.as_deref(), None);
    let rec = record(root.ledger(), &uids[2]);
    assert_eq!(rec.state, PassState::Rejected);
    assert_eq!(rec.reason.as_deref(), Some("uncovered"));
    let rec = record(root.ledger(), &uids[3]);
    assert_eq!(rec.state, PassState::Rejected);
    assert_eq!(rec.reason.as_deref(), Some("uncovered"));
    assert_eq!(count(root.memory(), "state_assertion"), 1);
}

// ── 9. C1：主语轴（无源行不许以 `expressed` 落库）──────────────────────────────
//
// 判据只有一条：**承载值的那一句**与主语同一句才算有源。三支说话人豁免（说话人=该锚 /
// 主人锚 / 本会话自锚）另要值句里有第一人称，否则只是"说清了谁在说"，没说清"哪一句承载值"。

/// 值句里既没有自锚的名字、也没有第一人称：只能以 `inferred` 落库。
const STALE_BODY: &str = "二进制是 10-28 的，HEAD 已到 11-02 ——stale，真实消费者测试要先重编。";
/// 主语名只在**另一句**里（值句里没有它）⇒ 跨句借名。
const BORROW_BODY: &str = "张三这两天平静得很。这轮跑完了，测试全过，他状态不错。";
/// 自锚行 + 值句里的第一人称：这一支必须仍是有源的。
const SELF_SMOOTH_BODY: &str = "我这轮跑得挺顺，测试都过了。";
/// 与非直陈轮拼批（`seed_bodies` 按 i 交替 human/agent，第 1 轮才是 agent）。
const FILLER_BODY: &str = "先对一下本周的进度";

/// 落库行的 `mode`（按 `valid_from_ms, id` 的顺序，与 `state_rows` 同序）。
fn landed_modes(root: &Root) -> Vec<String> {
    state_rows(root.memory())
        .into_iter()
        .map(|(_, _, _, mode, _)| mode)
        .collect()
}

/// 硬判据：主语是**本会话自锚**、agent 轮说话人也是它，但承载值的那一句里既没有名字也没有
/// 第一人称 ⇒ 这一行不许以 `expressed` 落库。
///
/// 现场形状就是台账 §V-1 复算的 `fca2a8f0`（「二进制 10-28、HEAD 到 11-02 ⇒ stale」挂到
/// 锚 `qoder` 的 `progress`）：闸放行（第 5 支），但它没有源句，只能是推断。
#[test]
fn an_unsourced_self_anchor_row_lands_as_inferred() {
    let (_dir, mut root, uids) = seeded(&[FILLER_BODY, STALE_BODY]);
    let plan = plan_of(&root);
    let extraction = extract_reply(
        vec![state_row(
            &plan, "s1", &uids[1], "agent-batch-A", "progress", "stale", STALE_BODY,
        )],
        Vec::new(),
    );
    let backend = backend_of(extraction, review_passes(1));
    let out = run(&mut root, &plan, &backend).unwrap();
    assert_eq!(out.states, 1, "闸放行 + 审核 pass ⇒ 这一行照常落库（降级不是拒收）");
    assert_eq!(landed_modes(&root), vec!["inferred".to_string()]);
    // 硬判据的反面写死：这一格一旦回到 expressed，本轮修复就被抹掉了。
    assert!(
        !landed_modes(&root).iter().any(|mode| mode == "expressed"),
        "无源行以 expressed 落库了"
    );
    assert_eq!(record(root.ledger(), &uids[1]).state, PassState::Done);
}

/// 跨句借名：名字在证据轮里出现（第 2 支放行），但不在承载值的那一句 ⇒ `inferred`。
#[test]
fn a_name_borrowed_from_another_sentence_lands_as_inferred() {
    let (_dir, mut root, uids) = seeded(&[FILLER_BODY, BORROW_BODY]);
    let plan = plan_of(&root);
    let extraction = extract_reply(
        vec![state_row(
            &plan, "s1", &uids[1], "张三", "emotion", "状态不错", BORROW_BODY,
        )],
        Vec::new(),
    );
    let backend = backend_of(extraction, review_passes(1));
    let out = run(&mut root, &plan, &backend).unwrap();
    assert_eq!(out.states, 1);
    assert_eq!(landed_modes(&root), vec!["inferred".to_string()]);
}

/// 阳性对照：值与主语同句 ⇒ `expressed`（收紧不许把这一支一起打掉）。
#[test]
fn a_value_in_the_same_sentence_as_its_subject_lands_as_expressed() {
    let (_dir, mut root, uids) = seeded(&[FILLER_BODY, ANXIOUS_BODY]);
    let plan = plan_of(&root);
    let extraction = extract_reply(
        vec![state_row(
            &plan, "s1", &uids[1], "张三", "emotion", "焦虑得很", ANXIOUS_BODY,
        )],
        Vec::new(),
    );
    let backend = backend_of(extraction, review_passes(1));
    let out = run(&mut root, &plan, &backend).unwrap();
    assert_eq!(out.states, 1);
    assert_eq!(landed_modes(&root), vec!["expressed".to_string()]);
}

/// 阳性对照（自锚那一支）：值句里有第一人称 ⇒ 仍 `expressed`。
///
/// 这一条钉的是"agent 自己说我…"那类能力（D19 的 21 行里 8 行合规的就是它）：收紧只针对
/// "值句里连第一人称都没有"的自锚行，不许把这一整支打成推断。
#[test]
fn a_self_anchor_row_speaking_in_the_first_person_stays_expressed() {
    let (_dir, mut root, uids) = seeded(&[FILLER_BODY, SELF_SMOOTH_BODY]);
    let plan = plan_of(&root);
    let extraction = extract_reply(
        vec![state_row(
            &plan, "s1", &uids[1], "agent-batch-A", "mood", "挺顺", SELF_SMOOTH_BODY,
        )],
        Vec::new(),
    );
    let backend = backend_of(extraction, review_passes(1));
    let out = run(&mut root, &plan, &backend).unwrap();
    assert_eq!(out.states, 1);
    assert_eq!(landed_modes(&root), vec!["expressed".to_string()]);
}
