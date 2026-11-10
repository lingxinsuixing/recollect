//! G1 刀 B 第 2 件（设计 `2026-11-08-g1-reuse-lesson-design.md` §波次一第 2 层）：
//! **审核价值轴的阴性对照**。老规矩：闸没见过"拒绝"不算验过。
//!
//! 这一份钉五件事，每一件都对应一条语义裁定：
//! 1. 问句逐字在场、闭集两格在场（prompt 与常量同一个真相）；
//! 2. `value_axis` 闭集执法：闭集外的词硬拒（fact 与 event 两族 contract）；
//! 3. demote ⇒ 落卡事务里盖 `reason='value_axis'` 章、keep ⇒ 不盖章；
//! 4. correctness `Rejected` 优先：被 reject 的行不落库、也就没有章（demote 不能把它救回来）；
//! 5. 先写者赢与"跨路径继承"：程序章不被价值轴覆盖（rc-memory 的写点用例钉）、
//!    approved 写库时从已落库的判定里取回轴答案（本文件最后一条）。
//!
//! 阴性样本用的是**自编的流水句**（通用名，不抄语料原句）：一条"支持得住、但不改变以后
//! 的回答"的行必须 demote；一条 decision/promise 必须 keep。真模型在靶向小样里跑同一对样本
//! （`design/2026-11-08-g1b-value-axis-readings.md`）。
//!
//! 夹具风格与 `tests/review_run.rs` 相同：真 `Root::open(tempdir)` + `mod common`。

use rc_distill::pass_event::{run_event, select_event_batch};
use rc_distill::pass_event_contract::parse_event_review_full;
use rc_distill::pass_fact::{run_fact, select_fact_batch};
use rc_distill::pass_fact_contract::{parse_fact_review, FactPassError, ReviewTarget};
use rc_distill::pass_prompt::{
    event_review_system, fact_review_system, FactFamily, ValueAxis, VALUE_AXIS_QUESTION,
    VALUE_AXIS_VALUES,
};
use rc_distill::review::{parse_review_detail, GATE_RULES_VERSION};
use rc_distill::review::run::run_item;
use rc_distill::DistillError;
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::review::{
    record_decision, Decider, Decision, ReviewStage, ReviewState, ReviewVerdict,
};
use rc_memory::value_gate::VALUE_AXIS_REASON;
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{AnchorRole, Provenance, Scope, ScopeKind};
use rc_store::{MemoryHandle, Root};
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

mod common;
use common::{adopt_rows, seed_bodies};

const NOW: i64 = 1_700_000_000_000;
const MODEL: &str = "fake-axis-model";
const PROMISE: &str = "我下周三之前把对账单发给你";
const RECEIVED_BODY: &str = "张三已经收到对账单";
const CONFIG_BODY: &str = "昨晚在机房我把配置改回去，服务就恢复了";
const CONFIG_QUOTE: &str = "昨晚在机房我把配置改回去，服务就恢复了";

// ── 夹具（与 review_run.rs 同一副骨架）────────────────────────────────────────

fn set_owner(root: &mut Root) {
    root.transcript_mut()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id='p1', owner_agent_id='alice',
                    visibility='project', retention='keep',
                    scope_kind='project', scope_key='proj-x', ownership_src='operator'
             WHERE session_key='sess-batch-A' AND device_id='dev-batch-A'",
            [],
        )
        .unwrap();
}

fn add_anchor(root: &Root, scope: &Scope, kind: &str, canonical: &str, uid: &str) -> String {
    let key = put_entity(
        root.memory(),
        &EntityContent {
            kind: kind.into(),
            title: canonical.into(),
            scope: scope.clone(),
            aliases: vec![],
        },
        &[Provenance { turn_uid: uid.to_string(), anchor_role: AnchorRole::Human }],
        NOW - 1_000,
    )
    .unwrap();
    adopt_rows(root, "anchor", "alice", Some("p1"), "project", "project", "proj-x");
    key.id
}

/// fact：3 轮（人工承诺 + agent 回执 + 人工寒暄），张三 a2、李四 a1 两个活锚。
fn seeded_fact() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[PROMISE, RECEIVED_BODY, "谢谢"], 1_000);
    set_owner(&mut root);
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    add_anchor(&root, &scope, "person", "李四", &uids[2]);
    add_anchor(&root, &scope, "person", "张三", &uids[0]);
    (dir, root)
}

/// event：1 轮（人工，配置恢复），张三 a2、地点 a1。
fn seeded_event() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[CONFIG_BODY], 1_000);
    set_owner(&mut root);
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    add_anchor(&root, &scope, "place", "某项目", &uids[0]);
    add_anchor(&root, &scope, "person", "张三", &uids[0]);
    (dir, root)
}

fn fact_plan(root: &Root) -> rc_distill::pass_fact::FactPlan {
    select_fact_batch(root.transcript(), root.ledger(), root.memory(), 0, FactFamily::Promise, NOW)
        .unwrap()
        .expect("夹具里 3 轮都还没跑 fact/promise")
}

fn event_plan(root: &Root) -> rc_distill::pass_event::EventPlan {
    select_event_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("夹具里的轮还没跑 event")
}

/// 抽取回复：t1 一条 promise（a2=张三、a1=李四）。
fn fact_reply() -> String {
    json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a2", "predicate": "promise",
            "object": "a1", "value": "把对账单发给你", "category": "promise",
            "mode": "stated", "time_text": "下周三之前", "quote": PROMISE,
            "evidence": ["t1"], "text": "张三承诺在下周三之前把对账单发给李四。"
        }],
        "empty_turns": ["t2", "t3"]
    })
    .to_string()
}

/// 两条草稿：f1 = 真承诺（人工轮），f2 = 那条自编流水句（agent 轮）。
fn fact_mixed_reply() -> String {
    json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a2", "predicate": "promise",
            "object": "a1", "value": "把对账单发给你", "category": "promise",
            "mode": "stated", "time_text": "下周三之前", "quote": PROMISE,
            "evidence": ["t1"], "text": "张三承诺在下周三之前把对账单发给李四。"
        }, {
            // 阴性样本：一条**被引文撑得住**的流水行（agent 轮的回执），价值轴答案是 demote。
            "ref": "f2", "turn": "t2", "subject": "a1", "predicate": "promise",
            "object": null, "value": "收到对账单", "category": "promise",
            "mode": "stated", "time_text": "", "quote": RECEIVED_BODY,
            "evidence": ["t2"], "text": "张三已经收到对账单。"
        }],
        "empty_turns": ["t3"]
    })
    .to_string()
}

fn event_reply() -> String {
    json!({
        "schema_version": 0, "pass": "event",
        "events": [{
            "ref": "e1", "turn": "t1", "category": "state_change", "actor": "a2",
            "trigger": "改回去", "action": "restore_config", "object": null,
            "object_text": "配置文件", "participants": ["a2"], "place": "a1",
            "result": "done", "mode": "stated", "time_text": "昨晚",
            "quote": CONFIG_QUOTE, "evidence": ["t1"],
            "text": "张三昨晚把配置改回去，服务恢复。"
        }],
        "empty_turns": []
    })
    .to_string()
}

/// fact 审核回复的一行：`(ref, verdict, code, evidence, value_axis)` 五格。
type AxisRow<'a> = (&'a str, &'a str, Option<&'a str>, Option<&'a str>, &'a str);

/// fact 审核回复：逐条 `ref/verdict/code/evidence/value_axis`（五键）。
fn fact_review_axis(rows: &[AxisRow<'_>]) -> String {
    let verdicts: Vec<Value> = rows
        .iter()
        .map(|(reference, verdict, code, evidence, axis)| {
            json!({
                "ref": reference, "verdict": verdict, "code": code,
                "evidence": evidence, "value_axis": axis,
            })
        })
        .collect();
    json!({"schema_version": 0, "pass": "fact_review", "family": "promise", "verdicts": verdicts})
        .to_string()
}

/// event 审核回复：逐条 `ref/verdict/code/value_axis`（四键）。
fn event_review_axis(rows: &[(&str, &str, Option<&str>, &str)]) -> String {
    let verdicts: Vec<Value> = rows
        .iter()
        .map(|(reference, verdict, code, axis)| {
            json!({"ref": reference, "verdict": verdict, "code": code, "value_axis": axis})
        })
        .collect();
    json!({"schema_version": 0, "pass": "event_review", "verdicts": verdicts}).to_string()
}

/// 回放 backend：按序给回复（脚本耗尽 = `Timeout`）。
struct ScriptBackend {
    replies: RefCell<VecDeque<Result<String, LlmFailure>>>,
    calls: Cell<usize>,
}

impl ScriptBackend {
    fn new(replies: Vec<Result<String, LlmFailure>>) -> Self {
        ScriptBackend { replies: RefCell::new(replies.into()), calls: Cell::new(0) }
    }
    /// fact 一片发两次（proposer + challenger），两档同形 ⇒ 同一份回复给两次。
    fn engine(review: String) -> Self {
        ScriptBackend::new(vec![Ok(review.clone()), Ok(review)])
    }
}

impl LlmBackend for ScriptBackend {
    fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
        self.calls.set(self.calls.get() + 1);
        self.replies.borrow_mut().pop_front().unwrap_or(Err(LlmFailure::Timeout))
    }
    fn slug(&self) -> Option<&str> {
        Some(MODEL)
    }
}

fn only_item(m: &MemoryHandle) -> (i64, String) {
    m.conn()
        .query_row("SELECT id, state FROM review_item", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
}

/// 造一条停在 `pending_review` 的 fact item（抽取成功、审核调用失败）。
fn pending_fact(root: &Root, mixed: bool) -> i64 {
    let plan = fact_plan(root);
    let reply = if mixed { fact_mixed_reply() } else { fact_reply() };
    let backend = ScriptBackend::new(vec![Ok(reply), Err(LlmFailure::BackendError)]);
    let err = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW)
        .unwrap_err();
    assert!(matches!(err, DistillError::Llm(LlmFailure::BackendError)), "{err}");
    let (id, state) = only_item(root.memory());
    assert_eq!(state, "pending_review");
    id
}

fn pending_event(root: &Root) -> i64 {
    let plan = event_plan(root);
    let backend = ScriptBackend::new(vec![Ok(event_reply()), Err(LlmFailure::BackendError)]);
    run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap_err();
    let (id, state) = only_item(root.memory());
    assert_eq!(state, "pending_review");
    id
}

fn count(m: &MemoryHandle, table: &str) -> i64 {
    m.conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// `(card_id, reason)`：全库归档章。
fn sink_rows(m: &MemoryHandle) -> Vec<(String, String)> {
    let mut st = m.conn().prepare("SELECT card_id, reason FROM card_sink ORDER BY card_id").unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn review_detail_of(m: &MemoryHandle, id: i64) -> String {
    m.conn()
        .query_row(
            "SELECT detail FROM review_decision WHERE item_id=?1 AND stage='review' ORDER BY id DESC LIMIT 1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
}

// ── 1. 问句逐字 + 闭集两格 ────────────────────────────────────────────────────

#[test]
fn the_value_axis_question_is_verbatim_in_both_review_prompts() {
    // 常量自身就是派发档那句，一个字都不许漂。
    assert_eq!(VALUE_AXIS_QUESTION, "这一条会改变以后的回答吗？");
    assert_eq!(VALUE_AXIS_VALUES.to_vec(), vec!["keep", "demote"]);

    for (name, system) in [
        ("fact review", fact_review_system(FactFamily::Promise)),
        ("event review", event_review_system()),
    ] {
        assert!(system.contains(VALUE_AXIS_QUESTION), "{name} 必须逐字带上那句问句");
        assert!(system.matches(VALUE_AXIS_QUESTION).count() >= 1);
        for value in VALUE_AXIS_VALUES {
            assert!(system.contains(value), "{name} 的契约缺闭集值：{value}");
        }
        assert!(system.contains("value_axis"), "{name} 的契约缺 value_axis 键名");
        assert!(system.contains("a missing value_axis is read as keep"),
                "{name} 必须说清缺键读成 keep");
    }
}

// ── 2. 闭集执法：闭集外的值硬拒 ──────────────────────────────────────────────

#[test]
fn the_contract_rejects_an_unknown_value_axis() {
    let targets = vec![ReviewTarget::new("f1", PROMISE)];
    // 缺键 ⇒ keep（challenger 的回复就不带这一格）。
    let no_axis = json!({"schema_version": 0, "pass": "fact_review", "family": "promise",
        "verdicts": [{"ref": "f1", "verdict": "pass", "code": null, "evidence": "对账单"}]})
        .to_string();
    let report = parse_fact_review(&no_axis, &targets, FactFamily::Promise).unwrap();
    assert_eq!(report.value_axis, vec![("f1".to_string(), ValueAxis::Keep)]);

    // 闭集外 ⇒ 整批硬拒，载荷只带 ref。
    let bad = json!({"schema_version": 0, "pass": "fact_review", "family": "promise",
        "verdicts": [{"ref": "f1", "verdict": "pass", "code": null, "evidence": "对账单",
                      "value_axis": "maybe"}]})
        .to_string();
    assert_eq!(
        parse_fact_review(&bad, &targets, FactFamily::Promise),
        Err(FactPassError::BadValueAxis("f1".into()))
    );
    // 非字符串（数字/布尔/对象）是形状错，不是"另一个闭集值"。
    let wrong_type = bad.replace("\"maybe\"", "1");
    assert_eq!(
        parse_fact_review(&wrong_type, &targets, FactFamily::Promise),
        Err(FactPassError::Json)
    );

    // event 同一条纪律。
    let refs = vec!["e1".to_string()];
    let ok = event_review_axis(&[("e1", "pass", None, "demote")]);
    let report = parse_event_review_full(&ok, &refs).unwrap();
    assert_eq!(report.value_axis, vec![("e1".to_string(), ValueAxis::Demote)]);
    let bad = event_review_axis(&[("e1", "pass", None, "hold")]);
    assert!(parse_event_review_full(&bad, &refs).is_err(), "闭集外的值必须拒");
}

// ── 3. demote ⇒ 落卡盖章；keep ⇒ 不盖章 ─────────────────────────────────────

#[test]
fn a_demoted_row_lands_its_card_archived_with_the_value_axis_reason() {
    let (_dir, root) = seeded_fact();
    let id = pending_fact(&root, false);
    let engine = ScriptBackend::engine(fact_review_axis(&[
        ("f1", "pass", None, Some("把对账单发给你"), "keep"),
    ]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);
    assert_eq!(count(root.memory(), "fact"), 1);
    assert!(sink_rows(root.memory()).is_empty(), "keep ⇒ 不盖章");
    assert_eq!(review_detail_of(root.memory(), id), "p=1;r=0;u=0;pass=f1;uncertain=-",
               "keep 时 detail 不多一个字节（既有形状不动）");

    // 同一条流水句换成 demote：卡照常进库、章是 value_axis。
    let (_dir2, root2) = seeded_fact();
    let id = pending_fact(&root2, true);
    let engine = ScriptBackend::engine(fact_review_axis(&[
        ("f1", "pass", None, Some("把对账单发给你"), "keep"),
        ("f2", "pass", None, Some("收到对账单"), "demote"),
    ]));
    let out = run_item(root2.memory(), root2.transcript(), root2.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);
    assert_eq!(count(root2.memory(), "fact"), 2, "demote 不是不写：原文全留、照常进库");
    let rows = sink_rows(root2.memory());
    assert_eq!(rows.len(), 1, "只有 demote 那一行盖章");
    assert_eq!(rows[0].1, VALUE_AXIS_REASON);
    let archived_text: String = root2
        .memory()
        .conn()
        .query_row("SELECT text FROM card WHERE id=?1", [&rows[0].0], |r| r.get(0))
        .unwrap();
    assert_eq!(archived_text, "张三已经收到对账单。", "章指向的正是流水那一行的镜像卡");
    assert_eq!(
        review_detail_of(root2.memory(), id),
        "p=2;r=0;u=0;pass=f1,f2;uncertain=-;demote=f2",
        "轴答案随判定落库（approved 路径据此重建）"
    );
    let rebuilt = parse_review_detail(&review_detail_of(root2.memory(), id)).unwrap();
    assert_eq!(rebuilt.demote, ["f2".to_string()].into_iter().collect());
}

// ── 4. correctness Rejected 优先：不落库 ⇒ 无章可盖 ─────────────────────────

#[test]
fn a_rejected_row_is_never_written_and_never_stamped_even_when_demoted() {
    let (_dir, root) = seeded_fact();
    let id = pending_fact(&root, true);
    let engine = ScriptBackend::engine(fact_review_axis(&[
        ("f1", "pass", None, Some("把对账单发给你"), "keep"),
        ("f2", "reject", Some("not_durable"), Some("收到对账单"), "demote"),
    ]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);
    assert_eq!(count(root.memory(), "fact"), 1, "被 reject 的行不落库");
    assert!(sink_rows(root.memory()).is_empty(),
            "没有行就没有卡，也就没有章：demote 不能把 reject 救回来");
    // 全 reject：连写库都不进，章当然也没有。
    let (_dir2, root2) = seeded_fact();
    let id = pending_fact(&root2, false);
    let engine = ScriptBackend::engine(fact_review_axis(&[
        ("f1", "reject", Some("quote_mismatch"), Some("把对账单发给你"), "demote"),
    ]));
    let out = run_item(root2.memory(), root2.transcript(), root2.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(out.item_state, ReviewState::Rejected);
    assert_eq!(count(root2.memory(), "fact"), 0);
    assert_eq!(count(root2.memory(), "card_sink"), 0);
}

// ── 5. event 支：同一条轴、同一处章 ──────────────────────────────────────────

#[test]
fn an_event_demote_archives_its_card_with_the_same_reason() {
    let (_dir, root) = seeded_event();
    let id = pending_event(&root);
    let engine = ScriptBackend::engine(event_review_axis(&[("e1", "pass", None, "demote")]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);
    assert_eq!(count(root.memory(), "fact"), 1);
    let rows = sink_rows(root.memory());
    assert_eq!(rows.len(), 1, "event 支也走同一处章");
    assert_eq!(rows[0].1, VALUE_AXIS_REASON);
    assert_eq!(
        review_detail_of(root.memory(), id),
        "p=1;r=0;u=0;pass=e1;uncertain=-;demote=e1"
    );
}

// ── 6. 跨路径继承：approved 写库时取回轴答案 ────────────────────────────────

#[test]
fn an_approved_item_inherits_the_demote_from_the_persisted_decision() {
    let (_dir, root) = seeded_fact();
    let id = pending_fact(&root, false);
    // 第一次审核：对错判 uncertain（不写库、章无处可盖），价值轴却已经给了 demote。
    let engine = ScriptBackend::engine(fact_review_axis(&[
        ("f1", "uncertain", Some("quote_mismatch"), None, "demote"),
    ]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(out.item_state, ReviewState::Quarantined);
    assert_eq!(count(root.memory(), "fact"), 0);
    assert_eq!(review_detail_of(root.memory(), id), "p=0;r=0;u=1;pass=-;uncertain=f1;demote=f1");

    // 人工放行（对错轴），再跑 approved：写库时章从已落库的判定里取回。
    let decision = Decision {
        stage: ReviewStage::Human,
        verdict: ReviewVerdict::Pass,
        reason_code: "human_approved".into(),
        rules_version: GATE_RULES_VERSION.into(),
        model: String::new(),
        prompt_version: String::new(),
        input_sha256: String::new(),
        review_input_sha256: String::new(),
        decider: Decider::Human,
        detail: String::new(),
    };
    record_decision(root.memory(), id, &decision, NOW + 1_000).unwrap();
    assert_eq!(only_item(root.memory()).1, "approved");

    // approved 分支不调模型：脚本空的 backend 只要被调用就会 `Timeout`。
    let engine = ScriptBackend::new(vec![]);
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW + 2_000)
        .unwrap();
    assert_eq!(package_state(out.item_state), "applied");
    assert_eq!(engine.calls.get(), 0, "approved 不许再问模型");
    assert_eq!(count(root.memory(), "fact"), 1);
    let rows = sink_rows(root.memory());
    assert_eq!(rows.len(), 1, "价值轴不随 human pass 清零");
    assert_eq!(rows[0].1, VALUE_AXIS_REASON);
}

fn package_state(state: ReviewState) -> &'static str {
    match state {
        ReviewState::Applied => "applied",
        ReviewState::Approved => "approved",
        ReviewState::PendingReview => "pending_review",
        ReviewState::Quarantined => "quarantined",
        ReviewState::Rejected => "rejected",
        ReviewState::Stale => "stale",
    }
}
