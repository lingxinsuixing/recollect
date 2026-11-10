//! C3：程序闸**逐 draft** 拒收的可审计性——一条被闸拒的草稿留一行 `review_decision`。
//!
//! 这一刀补的缺口：`review run` 报 `applied:4 / rejected:0` 的同时，载荷里 18 条 draft 只
//! 落库 7 条；逐条被闸拒的那 11 条，码只被折成批级计数（`review_detail` 的 `r=`）与逐轮
//! 终态（`rejected(<码>)`），事后查不到"是哪几条草稿、各是什么码"。
//!
//! 本文件钉五件事（每一题都能红）：
//! 1. state：1 条通过 + 2 条被**不同码**拒 ⇒ `review_decision` 恰多两行，`reason_code`
//!    逐条对上、`detail` 为 `ref=sN`；汇总 `review` 行的 `detail` 形状逐字未变；
//! 2. 阴性对照：全通过的 item ⇒ **零**新增 per-draft 行；
//! 3. fact / event 同形各一条（证明不是只接了 state）；
//! 4. 同一 item 重跑（quarantined 复审 → applied 重放）不产生重复行；
//! 5. approved 那一格（闸真的挡住写库）同样留一行。
//!
//! 夹具走真链路（`seed_bodies` 真导入转录、真锚 + 归属行），后端是脚本回放，
//! **不跑真实 LLM**。正反例只用通用占位名（张三/王五/某项目），不含语料专名。

use rc_distill::pass_event::{run_event_with, select_event_batch, EventPlan};
use rc_distill::pass_fact::{run_fact_with, select_fact_batch, FactPlan};
use rc_distill::pass_prompt::FactFamily;
use rc_distill::pass_state::{run_state_with, select_state_batch, StatePlan};
use rc_distill::review::run::{run_item, run_item_with_policy, RetryPolicy};
use rc_distill::review::{
    ReviewMode, PASS_EVENT_REVIEW_VERSION, PASS_FACT_REVIEW_VERSION, PASS_STATE_REVIEW_VERSION,
};
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::review::{
    record_decision, Decider, Decision, ReviewStage, ReviewState, ReviewVerdict,
};
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{AnchorRole, Provenance, Scope, ScopeKind};
use rc_store::{MemoryHandle, Root};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::VecDeque;

mod common;
use common::{adopt_rows, seed_bodies};

const NOW: i64 = 1_700_000_000_000;
const MODEL: &str = "fake-audit-model";

// ── 夹具 ──────────────────────────────────────────────────────────────────────

fn own_session(root: &mut Root) {
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
}

fn add_anchor(root: &Root, kind: &str, canonical: &str, uid: &str, created_at_ms: i64) {
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    put_entity(
        root.memory(),
        &EntityContent {
            kind: kind.into(),
            title: canonical.into(),
            scope,
            aliases: vec![],
        },
        &[Provenance { turn_uid: uid.to_string(), anchor_role: AnchorRole::Human }],
        created_at_ms,
    )
    .unwrap();
}

/// 会话 + 归属 + 两个 person 锚（王五较早 `a1`… 由候选表决定，草稿按位置引用）。
fn seeded(bodies: &[&str], anchors: &[(&str, &str)]) -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, bodies, 1_000);
    own_session(&mut root);
    // 序号口径与候选表一致：**列表末位最新**（`a1`）。夹具把这一条写死，草稿才能按位置引用。
    let total = anchors.len() as i64;
    for (index, (kind, canonical)) in anchors.iter().enumerate() {
        add_anchor(&root, kind, canonical, &uids[0], NOW - 1_000 * (total - index as i64));
    }
    adopt_rows(&root, "anchor", "alice", None, "project", "project", "proj-x");
    (dir, root, uids)
}

/// 回放后端：按序给回复（脚本耗尽 = `Timeout`）。
struct ScriptBackend {
    replies: RefCell<VecDeque<Result<String, LlmFailure>>>,
}

impl ScriptBackend {
    fn new(replies: Vec<Result<String, LlmFailure>>) -> Self {
        ScriptBackend { replies: RefCell::new(replies.into()) }
    }
}

impl LlmBackend for ScriptBackend {
    fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
        self.replies.borrow_mut().pop_front().unwrap_or(Err(LlmFailure::Timeout))
    }
    fn slug(&self) -> Option<&str> {
        Some(MODEL)
    }
}

// ── 读数 ──────────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct Row {
    id: i64,
    stage: String,
    verdict: String,
    reason_code: String,
    detail: String,
    decider: String,
    model: String,
    prompt_version: String,
    input_sha256: String,
    created_at_ms: i64,
}

fn decisions(root: &Root, item_id: i64) -> Vec<Row> {
    let mut st = root
        .memory()
        .conn()
        .prepare(
            "SELECT id, stage, verdict, reason_code, detail, decider, model, prompt_version,
                    input_sha256, created_at_ms
               FROM review_decision WHERE item_id=?1 ORDER BY id",
        )
        .unwrap();
    st.query_map([item_id], |r| {
        Ok(Row {
            id: r.get(0)?,
            stage: r.get(1)?,
            verdict: r.get(2)?,
            reason_code: r.get(3)?,
            detail: r.get(4)?,
            decider: r.get(5)?,
            model: r.get(6)?,
            prompt_version: r.get(7)?,
            input_sha256: r.get(8)?,
            created_at_ms: r.get(9)?,
        })
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

/// per-draft 审计行：`stage=program / verdict=reject`（闸对单条草稿的裁定）。
fn audit_rows(root: &Root, item_id: i64) -> Vec<Row> {
    decisions(root, item_id)
        .into_iter()
        .filter(|row| row.stage == "program" && row.verdict == "reject")
        .collect()
}

/// `(码, detail)` 的逐条读数——审计行的语义面。
fn audit_pairs(rows: &[Row]) -> Vec<(String, String)> {
    rows.iter()
        .map(|row| (row.reason_code.clone(), row.detail.clone()))
        .collect()
}

fn last_review(root: &Root, item_id: i64) -> Row {
    decisions(root, item_id)
        .into_iter()
        .rfind(|row| row.stage == "review")
        .expect("引擎应当记过一条 review 判定")
}

fn item_payload_sha(root: &Root, item_id: i64) -> String {
    root.memory()
        .conn()
        .query_row("SELECT payload_sha256 FROM review_item WHERE id=?1", [item_id], |r| {
            r.get(0)
        })
        .unwrap()
}

fn only_item(root: &Root) -> i64 {
    root.memory()
        .conn()
        .query_row("SELECT id FROM review_item", [], |r| r.get(0))
        .unwrap()
}

fn item_state(root: &Root, id: i64) -> String {
    root.memory()
        .conn()
        .query_row("SELECT state FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap()
}

fn count(m: &MemoryHandle, table: &str) -> i64 {
    m.conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// 人工放行：追加一条 `stage=human, verdict=pass`（从 pending_review 合法）。
fn human_pass(root: &Root, id: i64) {
    let decision = Decision {
        stage: ReviewStage::Human,
        verdict: ReviewVerdict::Pass,
        reason_code: "human_approved".into(),
        rules_version: "review-gate-test".into(),
        model: String::new(),
        prompt_version: String::new(),
        input_sha256: String::new(),
        review_input_sha256: String::new(),
        decider: Decider::Human,
        detail: String::new(),
    };
    record_decision(root.memory(), id, &decision, NOW + 500).unwrap();
}

// ── state 夹具 ────────────────────────────────────────────────────────────────

/// human 轮里把"我"挂到本会话自锚 ⇒ 闸的 D19 码。
const I_WORRIED: &str = "老师，我很担心自己跟不上，进度太快了。";
/// agent 轮里同一挂法**合规**（agent 说"我"）⇒ 这条过闸。
const I_SMOOTH: &str = "我这轮跑得挺顺，测试都过了。";
/// 主体名在正文里 ⇒ 主语闸放行；这里只用来当"agent 轮里点不到小海"的证据轮。
const ANXIOUS: &str = "张三这两天焦虑得很，晚上睡不着。";
/// 凑批的 human 轮（既不承载草稿，也不参与下面的断言）。
const FILLER: &str = "接下来看一下进度。";

fn state_plan(root: &Root) -> StatePlan {
    select_state_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("夹具里的轮都还没跑 state")
}

fn state_aref(plan: &StatePlan, canonical: &str) -> String {
    plan.candidates
        .iter()
        .find(|candidate| candidate.canonical == canonical)
        .unwrap_or_else(|| panic!("候选表里没有 {canonical}"))
        .reference
        .clone()
}

fn state_tref(plan: &StatePlan, turn_uid: &str) -> String {
    plan.batch
        .rows
        .iter()
        .position(|row| row.turn_uid == turn_uid)
        .map(|index| format!("t{}", index + 1))
        .unwrap_or_else(|| panic!("批里没有这一轮"))
}

#[allow(clippy::too_many_arguments)]
fn state_row(
    plan: &StatePlan,
    reference: &str,
    turn_uid: &str,
    subject: &str,
    dimension: &str,
    value: &str,
    quote: &str,
) -> Value {
    json!({
        "ref": reference, "turn": state_tref(plan, turn_uid),
        "subject": state_aref(plan, subject),
        "dimension": dimension, "value_after": value, "quote": quote,
        "evidence": [state_tref(plan, turn_uid)]
    })
}

fn state_extract(rows: Vec<Value>) -> String {
    json!({"schema_version": 0, "pass": "state", "states": rows, "empty_turns": []}).to_string()
}

fn state_review(decisions: &[(&str, &str)]) -> String {
    let verdicts: Vec<Value> = decisions
        .iter()
        .map(|(reference, verdict)| json!({"ref": reference, "verdict": verdict, "code": null}))
        .collect();
    json!({"schema_version": 0, "pass": "state_review", "verdicts": verdicts}).to_string()
}

/// 只入队（`EnqueueOnly`）：抽出队列 item、**不**问窄审 ⇒ per-draft 审计行还没落，
/// 于是"写入点"只可能是后面那次显式的引擎调用。
fn queue_state(root: &mut Root, plan: &StatePlan, extraction: String) -> i64 {
    run_state_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        plan,
        &ScriptBackend::new(vec![Ok(extraction)]),
        NOW,
        ReviewMode::EnqueueOnly,
    )
    .unwrap();
    let id = only_item(root);
    assert_eq!(item_state(root, id), "pending_review");
    id
}

// ── fact 夹具 ─────────────────────────────────────────────────────────────────

const FAMILY: FactFamily = FactFamily::Decision;
/// human 轮（第 0 条）：主语"张三"逐字在正文里。
const DECIDE_BODY: &str = "张三决定把发布推迟，等测试全过。";
/// agent 轮（第 1 条）：正文里没有"王五" ⇒ 引用王五做主语必判 `subject_not_in_evidence`。
const ACK_BODY: &str = "好的，收到，我会跟进";
/// 阴性对照的 agent 轮：正文明写"张三"，两条草稿都能过闸。
const ACK_WITH_SUBJECT: &str = "张三已经收到，我会跟进";

fn fact_plan(root: &Root) -> FactPlan {
    select_fact_batch(root.transcript(), root.ledger(), root.memory(), 0, FAMILY, NOW)
        .unwrap()
        .expect("夹具里的轮都还没跑 fact/decision")
}

/// 一条 fact 草稿：`subject` 是候选短引用（`a1`/`a2`），`turn` 是批内位置（`t1`…）。
fn fact_draft(
    reference: &str,
    turn: &str,
    subject: &str,
    value: &str,
    quote: &str,
    text: &str,
) -> Value {
    json!({
        "ref": reference, "turn": turn, "subject": subject,
        "predicate": "decide", "object": null, "value": value,
        "category": "decision", "mode": "stated", "time_text": "",
        "quote": quote, "evidence": [turn], "text": text
    })
}

fn fact_extract(drafts: Vec<Value>, empty_turns: &[&str]) -> String {
    json!({
        "schema_version": 0, "pass": "fact", "family": FAMILY.slug(),
        "facts": drafts, "empty_turns": empty_turns,
    })
    .to_string()
}

/// 一条 fact 窄审回复：`pass`/`reject` 必须带 quote 里的一段原文（§7.2 的证据闸）。
fn fact_review(decisions: &[(&str, &str, &str)]) -> String {
    let verdicts: Vec<Value> = decisions
        .iter()
        .map(|(reference, verdict, evidence)| {
            json!({"ref": reference, "verdict": verdict, "code": null, "evidence": evidence})
        })
        .collect();
    json!({"schema_version": 0, "pass": "fact_review", "family": FAMILY.slug(),
           "verdicts": verdicts})
    .to_string()
}

fn queue_fact(root: &mut Root, plan: &FactPlan, extraction: String) -> i64 {
    run_fact_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        plan,
        &ScriptBackend::new(vec![Ok(extraction)]),
        NOW,
        ReviewMode::EnqueueOnly,
    )
    .unwrap();
    let id = only_item(root);
    assert_eq!(item_state(root, id), "pending_review");
    id
}

/// fact 一片 = proposer + challenger 两次调用，两档同形 ⇒ 同一份回复塞两份。
fn engine(review: String) -> ScriptBackend {
    ScriptBackend::new(vec![Ok(review.clone()), Ok(review)])
}

// ── event 夹具 ────────────────────────────────────────────────────────────────

const CONFIG_BODY: &str = "昨晚在机房我把配置改回去，服务就恢复了";

fn event_plan(root: &Root) -> EventPlan {
    select_event_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("夹具里的轮还没跑 event")
}

/// 两条事件草稿都在同一轮上：`e1` 只引 actor，`e2` 多引一个对象锚（`李四`）——评审前
/// 把那个锚退场，只有 `e2` 被闸拒。
fn event_extract() -> String {
    let one = |reference: &str, object: Value, object_text: Value| {
        json!({
            "ref": reference, "turn": "t1", "category": "state_change", "actor": "a1",
            "trigger": "改回去", "action": "restore_config", "object": object,
            // `object` 与 `object_text` 二者只能给一个（同给是契约的 `ObjectBoth`）。
            "object_text": object_text, "participants": ["a1"], "place": null,
            "result": "done", "mode": "stated", "time_text": "",
            "quote": CONFIG_BODY, "evidence": ["t1"],
            "text": "张三昨晚把配置改回去，服务恢复。"
        })
    };
    json!({
        "schema_version": 0, "pass": "event",
        "events": [
            one("e1", Value::Null, json!("配置文件")),
            one("e2", json!("a2"), Value::Null),
        ],
        "empty_turns": []
    })
    .to_string()
}

fn event_review(decisions: &[(&str, &str)]) -> String {
    let verdicts: Vec<Value> = decisions
        .iter()
        .map(|(reference, verdict)| json!({"ref": reference, "verdict": verdict, "code": null}))
        .collect();
    json!({"schema_version": 0, "pass": "event_review", "verdicts": verdicts}).to_string()
}

fn queue_event(root: &mut Root, plan: &EventPlan, extraction: String) -> i64 {
    run_event_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        plan,
        &ScriptBackend::new(vec![Ok(extraction)]),
        NOW,
        ReviewMode::EnqueueOnly,
    )
    .unwrap();
    let id = only_item(root);
    assert_eq!(item_state(root, id), "pending_review");
    id
}

// ── 1. state：1 通过 + 2 条不同码拒收 ⇒ 恰多两行 ─────────────────────────────

#[test]
fn state_two_gate_rejects_land_one_audit_row_each_and_leave_the_summary_shape() {
    let (_dir, mut root, uids) = seeded(
        &[I_WORRIED, I_SMOOTH, FILLER, ANXIOUS],
        &[("person", "张三"), ("animal", "小花猫"), ("person", "小海")],
    );
    let plan = state_plan(&root);
    let extraction = state_extract(vec![
        // s1：human 轮的"我"挂到本会话自锚 ⇒ D19 码。
        state_row(&plan, "s1", &uids[0], "agent-batch-A", "emotion", "很担心", I_WORRIED),
        // s2：agent 轮的同一挂法合规 ⇒ 过闸、进审核。
        state_row(&plan, "s2", &uids[1], "agent-batch-A", "mood", "挺顺", I_SMOOTH),
        // s3：agent 轮的正文里没有"小海" ⇒ 另一个码（`subject_not_in_evidence`）。
        state_row(&plan, "s3", &uids[3], "小海", "progress", "焦虑得很", ANXIOUS),
    ]);
    let id = queue_state(&mut root, &plan, extraction);
    assert!(audit_rows(&root, id).is_empty(), "只入队、没跑闸 ⇒ 还没有 per-draft 行");

    // 引擎这一次只看到 s2（被闸拒的两条不进审核输入，片内重新编号为 s1）。
    let out = run_item(
        root.memory(),
        root.transcript(),
        root.ledger(),
        id,
        &ScriptBackend::new(vec![Ok(state_review(&[("s1", "pass")]))]),
        NOW + 1,
    )
    .unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);
    assert_eq!(count(root.memory(), "state_assertion"), 1, "通过的那条照常落库");

    let rows = audit_rows(&root, id);
    assert_eq!(
        audit_pairs(&rows),
        vec![
            ("subject_self_on_human_turn".to_string(), "ref=s1".to_string()),
            ("subject_not_in_evidence".to_string(), "ref=s3".to_string()),
        ],
        "每一条被闸拒的 draft 恰一行，码与 ref 逐条对上"
    );
    // 四格与汇总判定同源：同一个载荷指纹、同一份 model/prompt 版本、同一个 decider。
    let sha = item_payload_sha(&root, id);
    for row in &rows {
        assert_eq!(row.decider, "program");
        assert_eq!(row.model, MODEL);
        assert_eq!(row.prompt_version, PASS_STATE_REVIEW_VERSION);
        assert_eq!(row.input_sha256, sha);
        assert_eq!(row.created_at_ms, NOW + 1);
    }
    // 汇总行形状逐字未变（`p/r/u/pass/uncertain` 五格，坏条只计进 `r=`）。
    let summary = last_review(&root, id);
    assert_eq!(summary.detail, "p=1;r=2;u=0;pass=s2;uncertain=-");
    assert_eq!(summary.reason_code, "review_mixed");
}

// ── 2. 阴性对照：全通过 ⇒ 零新增 per-draft 行 ─────────────────────────────────

#[test]
fn an_all_pass_item_writes_no_per_draft_row() {
    let (_dir, mut root, _uids) = seeded(
        &[DECIDE_BODY, ACK_WITH_SUBJECT],
        &[("person", "王五"), ("person", "张三")],
    );
    let plan = fact_plan(&root);
    let id = queue_fact(
        &mut root,
        &plan,
        fact_extract(
            vec![
                fact_draft("f1", "t1", "a1", "把发布推迟", DECIDE_BODY, "张三决定把发布推迟。"),
                fact_draft("f2", "t2", "a1", "已经收到", "张三已经收到", "张三已经收到并跟进。"),
            ],
            &[],
        ),
    );

    let out = run_item(
        root.memory(),
        root.transcript(),
        root.ledger(),
        id,
        &engine(fact_review(&[
            ("f1", "pass", "把发布推迟"),
            ("f2", "pass", "已经收到"),
        ])),
        NOW + 1,
    )
    .unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);
    assert_eq!(out.facts, 2, "两条都过闸、都过审、都写库");
    assert!(
        audit_rows(&root, id).is_empty(),
        "全通过的 item 不许长出 per-draft 行：{:?}",
        audit_pairs(&audit_rows(&root, id))
    );
    let summary = last_review(&root, id);
    assert_eq!(summary.detail, "p=2;r=0;u=0;pass=f1,f2;uncertain=-");
}

// ── 3a. fact：闸拒的草稿留一行（证明不是只接了 state）───────────────────────

#[test]
fn fact_gate_reject_lands_one_audit_row_with_the_gate_code() {
    let (_dir, mut root, _uids) = seeded(
        &[DECIDE_BODY, ACK_BODY, "谢谢"],
        &[("person", "王五"), ("person", "张三")],
    );
    let plan = fact_plan(&root);
    let id = queue_fact(
        &mut root,
        &plan,
        fact_extract(
            vec![
                fact_draft("f1", "t1", "a1", "把发布推迟", DECIDE_BODY, "张三决定把发布推迟。"),
                // a2 = 王五：agent 轮正文里没有这个名字 ⇒ `subject_not_in_evidence`。
                fact_draft("f2", "t2", "a2", "我会跟进", ACK_BODY, "王五决定跟进发布。"),
            ],
            &["t3"],
        ),
    );

    let out = run_item(
        root.memory(),
        root.transcript(),
        root.ledger(),
        id,
        &engine(fact_review(&[("f1", "pass", "把发布推迟")])),
        NOW + 1,
    )
    .unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);
    assert_eq!(out.facts, 1, "只有过闸的那条写库");

    let rows = audit_rows(&root, id);
    assert_eq!(
        audit_pairs(&rows),
        vec![("subject_not_in_evidence".to_string(), "ref=f2".to_string())]
    );
    assert_eq!(rows[0].model, MODEL);
    assert_eq!(rows[0].prompt_version, PASS_FACT_REVIEW_VERSION);
    assert_eq!(rows[0].input_sha256, item_payload_sha(&root, id));
    let summary = last_review(&root, id);
    assert_eq!(summary.detail, "p=1;r=1;u=0;pass=f1;uncertain=-");
}

// ── 3b. event：同形一条 ───────────────────────────────────────────────────────

#[test]
fn event_gate_reject_lands_one_audit_row_with_the_gate_code() {
    let (_dir, mut root, _uids) = seeded(
        &[CONFIG_BODY],
        &[("place", "某项目"), ("person", "李四"), ("person", "张三")],
    );
    let plan = event_plan(&root);
    let id = queue_event(&mut root, &plan, event_extract());
    // 入队之后、评审之前，e2 引用的对象锚退场了：抽取期它还在池里（引文/候选都合法），
    // 只有闸的第 6 步能看到 `ref_retired`——这正是"码只活在闸里"的那一类。
    let retired = root
        .memory()
        .conn()
        .execute("UPDATE anchor SET status='retired' WHERE canonical='李四'", [])
        .unwrap();
    assert_eq!(retired, 1, "夹具必须真的退掉一个锚");

    let out = run_item(
        root.memory(),
        root.transcript(),
        root.ledger(),
        id,
        &ScriptBackend::new(vec![Ok(event_review(&[("e1", "pass")]))]),
        NOW + 1,
    )
    .unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);
    assert_eq!(count(root.memory(), "fact"), 1, "只有过闸的那条写库（事件行落在 fact 表）");

    let rows = audit_rows(&root, id);
    assert_eq!(
        audit_pairs(&rows),
        vec![("ref_retired".to_string(), "ref=e2".to_string())]
    );
    assert_eq!(rows[0].prompt_version, PASS_EVENT_REVIEW_VERSION);
    assert_eq!(rows[0].input_sha256, item_payload_sha(&root, id));
    let summary = last_review(&root, id);
    assert_eq!(summary.detail, "p=1;r=1;u=0;pass=e1;uncertain=-");
}

// ── 4. 同一 item 重跑 ⇒ 不产生重复行 ──────────────────────────────────────────

#[test]
fn rerunning_the_same_item_never_duplicates_the_per_draft_rows() {
    let (_dir, mut root, _uids) = seeded(
        &[DECIDE_BODY, ACK_BODY, "谢谢"],
        &[("person", "王五"), ("person", "张三")],
    );
    let plan = fact_plan(&root);
    let id = queue_fact(
        &mut root,
        &plan,
        fact_extract(
            vec![
                fact_draft("f1", "t1", "a1", "把发布推迟", DECIDE_BODY, "张三决定把发布推迟。"),
                fact_draft("f2", "t2", "a2", "我会跟进", ACK_BODY, "王五决定跟进发布。"),
            ],
            &["t3"],
        ),
    );

    // 第一次：f1 uncertain ⇒ item 停在 quarantined，闸的拒收已经可查。
    let out = run_item(
        root.memory(),
        root.transcript(),
        root.ledger(),
        id,
        &engine(json!({"schema_version": 0, "pass": "fact_review", "family": FAMILY.slug(),
                       "verdicts": [{"ref": "f1", "verdict": "uncertain", "code": null,
                                     "evidence": null}]})
        .to_string()),
        NOW + 1,
    )
    .unwrap();
    assert_eq!(out.item_state, ReviewState::Quarantined);
    let first = audit_rows(&root, id);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].detail, "ref=f2");

    // 第二次：同一条 item 的复审走同一条 pending 路径（`Quarantined` 策略），闸重算一遍。
    let out = run_item_with_policy(
        root.memory(),
        root.transcript(),
        root.ledger(),
        id,
        &engine(json!({"schema_version": 0, "pass": "fact_review", "family": FAMILY.slug(),
                       "verdicts": [{"ref": "f1", "verdict": "uncertain", "code": null,
                                     "evidence": null}]})
        .to_string()),
        NOW + 2,
        RetryPolicy::Quarantined,
    )
    .unwrap();
    assert_eq!(out.item_state, ReviewState::Quarantined);
    let second = audit_rows(&root, id);
    assert_eq!(second.len(), 1, "复审不许追加第二条同样的 per-draft 行");
    assert_eq!(second[0].id, first[0].id, "命中的是同一行，不是新写一行");
    assert_eq!(second[0].created_at_ms, NOW + 1, "那一行没被改写");

    // 第三次：同一条 item 收口成 applied，闸结论没变 ⇒ 行数仍是一。
    let out = run_item_with_policy(
        root.memory(),
        root.transcript(),
        root.ledger(),
        id,
        &engine(fact_review(&[("f1", "pass", "把发布推迟")])),
        NOW + 3,
        RetryPolicy::Quarantined,
    )
    .unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);
    assert_eq!(audit_rows(&root, id).len(), 1);

    // 第四次：已 applied 的重放（终态只读）——一行都不许新增。
    let out = run_item(
        root.memory(),
        root.transcript(),
        root.ledger(),
        id,
        &engine(fact_review(&[("f1", "pass", "把发布推迟")])),
        NOW + 4,
    )
    .unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);
    assert_eq!(audit_rows(&root, id).len(), 1);
}

// ── 5. approved：闸真的挡住写库的那一格也留一行 ───────────────────────────────

#[test]
fn approved_gate_reject_is_audited_before_the_write() {
    let (_dir, mut root, _uids) = seeded(
        &[DECIDE_BODY, ACK_BODY, "谢谢"],
        &[("person", "王五"), ("person", "张三")],
    );
    let plan = fact_plan(&root);
    let id = queue_fact(
        &mut root,
        &plan,
        fact_extract(
            vec![
                fact_draft("f1", "t1", "a1", "把发布推迟", DECIDE_BODY, "张三决定把发布推迟。"),
                fact_draft("f2", "t2", "a2", "我会跟进", ACK_BODY, "王五决定跟进发布。"),
            ],
            &["t3"],
        ),
    );
    human_pass(&root, id);
    assert_eq!(item_state(&root, id), "approved");

    let out = run_item(
        root.memory(),
        root.transcript(),
        root.ledger(),
        id,
        // 已 approved 的重放一次模型都不问：脚本为空，真开口就会拿到 `Timeout` 而失败。
        &ScriptBackend::new(Vec::new()),
        NOW + 1,
    )
    .unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);
    assert_eq!(out.facts, 1, "human pass 也不放行被闸拒的那条");
    let rows = audit_rows(&root, id);
    assert_eq!(
        audit_pairs(&rows),
        vec![("subject_not_in_evidence".to_string(), "ref=f2".to_string())]
    );
    assert_eq!(rows[0].prompt_version, PASS_FACT_REVIEW_VERSION);
}
