//! S3b-4b：`ReviewMode::EnqueueOnly`（`hub distill once --no-review` 的库面）。
//! 设计 `2026-10-26-review-queue-s3` §11 的 S3b 段：只抽取、只入队，窄审留给 `review run`。
//!
//! 断的是**这一模式自己的三件事**（不是闸门/引擎 / 既有 Inline 路径——那些各有自己的用例）：
//! 1. fact：有草稿的轮 `pending(review_pending)`、没有草稿的轮沿用既有终态
//!    （`empty` / `rejected(uncovered)`）、读数除轮/草稿四格全 0、`state=Pending`，
//!    且审核模型**一次都不开**（脚本只给抽取，第二次调用会耗尽成名 `Timeout` 并由断言捕获）；
//! 2. 同一条 item 之后由 `review::run::run_item`（Inline 引擎）补审 ⇒ applied、行 1、
//!    `mark_turns` 后该轮 `done`；
//! 3. `approved` 的 item 在 EnqueueOnly 下**仍走引擎**（那两支不问模型）——`--no-review`
//!    只跳过 "pending 的窄审"，不吞掉补写；
//! 4. event：与 fact 同形（有草稿/覆盖缺口两格）；
//! 5. naming：A1 非空 ⇒ 批内**每一轮** `pending(review_pending)`、0 锚、item pending；
//!    合法空批仍走既有 `empty`（不入队、不标 pending）。
//!
//! 夹具风格同 `review_wiring.rs`/`review_naming_run.rs`：真 `Root::open(tempdir)` +
//! `mod common`，正反例只用通用占位名（张三/李四），不含任何语料专名。

use rc_distill::pass_event::{run_event_with, select_event_batch, EventOutcome};
use rc_distill::pass_fact::{run_fact, run_fact_with, select_fact_batch, FactOutcome};
use rc_distill::pass_naming::{run_naming_with, select_naming_batch, NamingOutcome};
use rc_distill::pass_prompt::FactFamily;
use rc_distill::review::run::{mark_turns, run_item};
use rc_distill::review::ReviewMode;
use rc_ledger::pass_state::{self, PassKey, PassRecord, PassState, REVIEW_PENDING_REASON};
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::review::{
    get as queue_get, record_decision, Decider, Decision, ReviewStage, ReviewState, ReviewVerdict,
};
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{AnchorRole, Provenance, Scope, ScopeKind};
use rc_store::{MemoryHandle, Root};
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

mod common;
use common::{adopt_rows, seed_bodies};

const NOW: i64 = 1_700_000_000_000;
const PROMISE: &str = "我下周三之前把对账单发给你";
const CONFIG_BODY: &str = "昨晚在机房我把配置改回去，服务就恢复了";
const CONFIG_QUOTE: &str = "昨晚在机房我把配置改回去，服务就恢复了";
const RELEASE_BODY: &str = "今天上午我把发布记录更新了";

const NAMING_BODIES: [&str; 4] = [
    "张三说明天把对账单发给你",
    "老张说对账单后天到，张三负责跟进",
    "李四说预算表还没做完",
    "小结：老张把对账单交给了张三",
];

// ── 夹具 ──────────────────────────────────────────────────────────────────────

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

fn anchor(root: &Root, canonical: &str, turn: &str, created_at_ms: i64) {
    put_entity(
        root.memory(),
        &EntityContent {
            kind: "person".into(),
            title: canonical.into(),
            scope: Scope { kind: ScopeKind::Project, key: "proj-x".into() },
            aliases: vec![],
        },
        &[Provenance { turn_uid: turn.to_string(), anchor_role: AnchorRole::Human }],
        created_at_ms,
    )
    .unwrap();
    adopt_rows(root, "anchor", "alice", Some("p1"), "project", "project", "proj-x");
}

/// fact：3 轮（承诺 + 两句寒暄），张三 a1、李四 a2。
fn seeded_fact() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[PROMISE, "好的，收到", "谢谢"], 1_000);
    set_owner(&mut root);
    anchor(&root, "李四", &uids[2], NOW - 2_000);
    anchor(&root, "张三", &uids[0], NOW - 1_000);
    (dir, root, uids)
}

/// event：2 轮（配置恢复 + 更新发布记录），张三 a1。
fn seeded_event() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[CONFIG_BODY, RELEASE_BODY], 1_000);
    set_owner(&mut root);
    anchor(&root, "张三", &uids[0], NOW - 1_000);
    (dir, root, uids)
}

/// naming：四轮正文，无既有锚。
fn seeded_naming() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &NAMING_BODIES, 1_000);
    set_owner(&mut root);
    (dir, root, uids)
}

fn fact_plan(root: &Root) -> rc_distill::pass_fact::FactPlan {
    fact_plan_at(root, NOW)
}

/// 同一条选批，但"现在"由调用方给：退避到期后才重选得到同一批（S2a 的 60s）。
fn fact_plan_at(root: &Root, now_ms: i64) -> rc_distill::pass_fact::FactPlan {
    select_fact_batch(root.transcript(), root.ledger(), root.memory(), 0, FactFamily::Promise, now_ms)
        .unwrap()
        .expect("夹具里 3 轮都还没跑 fact/promise")
}

fn event_plan(root: &Root) -> rc_distill::pass_event::EventPlan {
    select_event_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("夹具里的轮还没跑 event")
}

fn naming_plan(root: &Root) -> rc_distill::pass_naming::NamingPlan {
    select_naming_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("夹具里 4 轮都还没跑 naming")
}

/// 抽取回复：t1 一条 promise（a1=张三、a2=李四），t2/t3 由 `empty_turns` 宣告空。
fn fact_reply() -> String {
    json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a1", "predicate": "promise",
            "object": "a2", "value": "把对账单发给你", "category": "promise",
            "mode": "stated", "time_text": "下周三之前", "quote": PROMISE,
            "evidence": ["t1"], "text": "张三承诺在下周三之前把对账单发给李四。"
        }],
        "empty_turns": ["t2", "t3"]
    })
    .to_string()
}

/// 同一批，但只宣告 t2 空：t3 是覆盖缺口（`rejected(uncovered)`）。
fn fact_reply_with_a_gap() -> String {
    json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a1", "predicate": "promise",
            "object": "a2", "value": "把对账单发给你", "category": "promise",
            "mode": "stated", "time_text": "下周三之前", "quote": PROMISE,
            "evidence": ["t1"], "text": "张三承诺在下周三之前把对账单发给李四。"
        }],
        "empty_turns": ["t2"]
    })
    .to_string()
}

fn fact_review() -> String {
    json!({
        "schema_version": 0, "pass": "fact_review", "family": "promise",
        "verdicts": [{"ref": "f1", "verdict": "pass", "code": null, "evidence": "把对账单发给你"}]
    })
    .to_string()
}

/// event 抽取回复：t1 一条 state_change（actor a1=张三）；t2 不宣告 ⇒ 覆盖缺口。
fn event_reply() -> String {
    json!({
        "schema_version": 0, "pass": "event",
        "events": [{
            "ref": "e1", "turn": "t1", "category": "state_change", "actor": "a1",
            "trigger": "改回去", "action": "restore_config", "object": null, "object_text": "配置文件",
            "participants": ["a1"], "place": null, "result": "done", "mode": "stated",
            "time_text": "昨晚", "quote": CONFIG_QUOTE, "evidence": ["t1"],
            "text": "张三昨晚把配置改回去，服务恢复。"
        }],
        "empty_turns": []
    })
    .to_string()
}

/// A1 回复：两个 person 锚（张三/老张、李四）+ 两条提及 + 两条 issue + 一条 unresolved。
fn a1_reply() -> String {
    json!({
        "schema_version": 4, "pass": "a1",
        "anchors": [{
            "ref": "a1", "kind": "person", "canonical": "张三", "aliases": ["老张"],
            "reality_status": "real", "evidence": ["t1", "t2"], "reason": "两轮点名"
        }, {
            "ref": "a2", "kind": "person", "canonical": "李四", "aliases": [],
            "reality_status": "real", "evidence": ["t3"], "reason": "一轮点名"
        }],
        "mentions": [{
            "anchor_ref": "a1", "surface": "老张", "turn_uid": "t2", "speaker_ref": null,
            "discourse_role": "mentioned", "resolution": "resolved", "confidence": 0.9
        }, {
            "anchor_ref": "a2", "surface": "李四", "turn_uid": "t3", "speaker_ref": "a1",
            "discourse_role": "mentioned", "resolution": "resolved", "confidence": 0.8
        }],
        "issues": [
            {"kind": "quote_mismatch", "refs": ["a1", "a2"], "reason": "两处口径不一致"},
            {"kind": "conflict", "refs": ["a2"], "reason": "只指向李四"}
        ],
        "unresolved_surfaces": [
            {"surface": "那台打印机", "turn_uid": "t4", "reason": "信息不足"}
        ]
    })
    .to_string()
}

/// 合法空批：六键齐全、四张列表全空。
fn empty_a1_reply() -> String {
    json!({
        "schema_version": 4, "pass": "a1",
        "anchors": [], "mentions": [], "issues": [], "unresolved_surfaces": []
    })
    .to_string()
}

/// 回放 backend：按序给回复，记录调用次数（脚本耗尽 = `Timeout`）。
struct ScriptBackend {
    replies: RefCell<VecDeque<Result<String, LlmFailure>>>,
    calls: Cell<usize>,
}

impl ScriptBackend {
    fn new(replies: Vec<Result<String, LlmFailure>>) -> Self {
        ScriptBackend { replies: RefCell::new(replies.into()), calls: Cell::new(0) }
    }
    fn one(reply: String) -> Self {
        ScriptBackend::new(vec![Ok(reply)])
    }
    fn two(first: String, second: String) -> Self {
        ScriptBackend::new(vec![Ok(first), Ok(second)])
    }
}

impl LlmBackend for ScriptBackend {
    fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
        self.calls.set(self.calls.get() + 1);
        self.replies.borrow_mut().pop_front().unwrap_or(Err(LlmFailure::Timeout))
    }
    fn slug(&self) -> Option<&str> {
        Some("fake-no-review-model")
    }
}

// ── 读数 ──────────────────────────────────────────────────────────────────────

fn count(m: &MemoryHandle, table: &str) -> i64 {
    m.conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn only_item(m: &MemoryHandle) -> (i64, String, Option<String>) {
    m.conn()
        .query_row("SELECT id, state, program_code FROM review_item", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap()
}

fn decision_stages(m: &MemoryHandle, id: i64) -> Vec<String> {
    let mut st = m
        .conn()
        .prepare("SELECT stage FROM review_decision WHERE item_id=?1 ORDER BY id")
        .unwrap();
    st.query_map([id], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn fact_key<'a>(uid: &'a str) -> PassKey<'a> {
    PassKey { turn_uid: uid, pass_id: "fact", family: "promise" }
}

fn event_key(uid: &str) -> PassKey<'_> {
    PassKey { turn_uid: uid, pass_id: "event", family: "" }
}

fn naming_key(uid: &str) -> PassKey<'_> {
    PassKey { turn_uid: uid, pass_id: "naming", family: "" }
}

fn record(root: &Root, key: PassKey<'_>) -> PassRecord {
    pass_state::get(root.ledger(), key).unwrap().expect("状态行不存在")
}

fn assert_awaiting_review(root: &Root, key: PassKey<'_>) {
    let r = record(root, key);
    assert_eq!(r.state, PassState::Pending);
    assert_eq!(r.reason.as_deref(), Some(REVIEW_PENDING_REASON));
    assert_eq!(r.next_retry_at_ms, None, "等审核不是等退避");
    assert!(r.fingerprint.is_some(), "抽取跑过 ⇒ 指纹必须留着：{r:?}");
    assert!(r.run_id.is_some(), "抽取跑过 ⇒ run_id 必须留着：{r:?}");
}

// ── fact ──────────────────────────────────────────────────────────────────────

/// 1. fact `EnqueueOnly`：只给抽取的脚本 ⇒ 0 行、item `pending_review`、判定只有 program pass；
///    有草稿的轮 `pending(review_pending)`、被 `empty_turns` 覆盖的轮仍是 `empty`。
#[test]
fn fact_enqueue_only_marks_the_draft_turn_and_keeps_empty_turns() {
    let (_dir, root, uids) = seeded_fact();
    let plan = fact_plan(&root);
    let backend = ScriptBackend::one(fact_reply());

    let out: FactOutcome = run_fact_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
        ReviewMode::EnqueueOnly,
    )
    .unwrap();

    assert_eq!(backend.calls.get(), 1, "EnqueueOnly 只问抽取，审核模型一次都不开");
    assert_eq!(
        (out.turns, out.drafts, out.dropped, out.uncovered),
        (3, 1, 0, 0),
        "轮/草稿/丢弃/覆盖缺口照实"
    );
    assert_eq!(
        (
            out.written,
            out.duplicate,
            out.rejected,
            out.uncertain,
            out.facts,
            out.cards,
            out.mentions
        ),
        (0, 0, 0, 0, 0, 0, 0),
        "没审核就没写库：七格读数全 0"
    );
    assert_eq!(out.state, PassState::Pending, "待审不是失败，也不是 done");
    assert_eq!(count(root.memory(), "fact"), 0, "EnqueueOnly 不许写记忆行");

    let (id, state, program_code) = only_item(root.memory());
    assert_eq!(state, "pending_review", "产出留在队列里等 review run");
    assert_eq!(program_code, None, "闸 pass 时 program_code 为 NULL");
    assert_eq!(decision_stages(root.memory(), id), vec!["program".to_string()],
               "判定只有程序闸一条，没有 review/apply");
    let verdict: String = root
        .memory()
        .conn()
        .query_row("SELECT verdict FROM review_decision WHERE item_id=?1", [id], |r| r.get(0))
        .unwrap();
    assert_eq!(verdict, "pass");

    assert_awaiting_review(&root, fact_key(&uids[0]));
    for uid in &uids[1..] {
        let r = record(&root, fact_key(uid));
        assert_eq!(r.state, PassState::Empty, "被 empty_turns 覆盖的轮沿用既有终态：{uid}");
        assert_eq!(r.reason, None, "{uid}");
    }
}

/// 2. fact `EnqueueOnly` 的覆盖缺口：没有草稿且没被宣告的轮仍落 `rejected(uncovered)`。
#[test]
fn fact_enqueue_only_keeps_the_uncovered_terminal_state() {
    let (_dir, root, uids) = seeded_fact();
    let plan = fact_plan(&root);
    let backend = ScriptBackend::one(fact_reply_with_a_gap());

    let out = run_fact_with(
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
    assert_eq!((out.turns, out.drafts, out.uncovered), (3, 1, 1));
    assert_eq!(out.state, PassState::Pending);
    assert_awaiting_review(&root, fact_key(&uids[0]));
    assert_eq!(record(&root, fact_key(&uids[1])).state, PassState::Empty);
    let gap = record(&root, fact_key(&uids[2]));
    assert_eq!(gap.state, PassState::Rejected);
    assert_eq!(gap.reason.as_deref(), Some("uncovered"), "模型真漏了那一轮");
    assert_eq!(count(root.memory(), "fact"), 0);
}

/// 3. 同一条 item 再走 `review::run::run_item`（Inline 引擎）+ pass 回复 ⇒ applied、行 1；
///    `mark_turns` 后那一轮 `done`（与 `hub review run` 同一条引擎 + 同一条铺终态）。
#[test]
fn an_enqueued_fact_item_is_written_by_the_engine() {
    let (_dir, root, uids) = seeded_fact();
    let plan = fact_plan(&root);
    let extraction = ScriptBackend::one(fact_reply());
    run_fact_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &extraction,
        NOW,
        ReviewMode::EnqueueOnly,
    )
    .unwrap();
    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "pending_review");
    let item = queue_get(root.memory(), id).unwrap().expect("item 必须在");

    // §7.2：一片 fact 审核 = proposer + challenger 两次调用（同形回复）。
    let reply = fact_review();
    let review = ScriptBackend::new(vec![Ok(reply.clone()), Ok(reply)]);
    let run = run_item(root.memory(), root.transcript(), root.ledger(), id, &review, NOW + 10)
        .unwrap();
    assert_eq!(review.calls.get(), 2, "补审问 proposer + challenger 两次");
    assert_eq!(run.item_state, ReviewState::Applied);
    mark_turns(root.ledger(), &item, &run, NOW + 10).unwrap();

    assert_eq!(count(root.memory(), "fact"), 1, "补写必须落一行 fact");
    assert_eq!(only_item(root.memory()).1, "applied");
    assert_eq!(record(&root, fact_key(&uids[0])).state, PassState::Done);
    assert_eq!(
        decision_stages(root.memory(), id),
        vec!["program".to_string(), "review".to_string(), "apply".to_string()],
        "判定链补齐为 program/review/apply"
    );
}

/// 4. `approved` 的 item 在 `EnqueueOnly` 下**仍走引擎**：`--no-review` 只跳过 pending 的窄审。
///
/// 场景就是设计 §11 的"LLM 不可用 ⇒ pending，恢复后补写"的同一格：Inline 跑一遍、窄审
/// 开不了口 ⇒ 三轮全 pending、item pending_review；人工放行 ⇒ approved；退避到期后
/// `--no-review` 重跑同一批（同载荷命中**同一个** item）⇒ 引擎按台账补写。
#[test]
fn an_approved_item_still_runs_the_engine_under_enqueue_only() {
    let (_dir, root, uids) = seeded_fact();
    // 先选一次批：`select_fact_batch` 会给会话自锚落一行（S2e-7），之后候选表才稳定。
    // 不这样做，第二次选批的候选比第一次多一条，`fact_user` 变 ⇒ 指纹变 ⇒ 载荷换 sha ⇒
    // 会新开一条 item，测不到"approved 的同一个 item 仍走引擎"。
    let _ = fact_plan(&root);
    let plan = fact_plan(&root);
    // ① Inline（既有签名 = `Inline`）：抽取成功、窄审 `backend_error` ⇒ 轮全 pending。
    let failing = ScriptBackend::new(vec![Ok(fact_reply()), Err(LlmFailure::BackendError)]);
    let err = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &failing, NOW)
        .unwrap_err();
    assert!(matches!(err, rc_distill::DistillError::Llm(_)), "窄审开不了口是 LLM 侧故障：{err}");
    assert_eq!(count(root.memory(), "fact"), 0);
    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "pending_review");
    let item = queue_get(root.memory(), id).unwrap().expect("item 必须在");
    for uid in &uids {
        let r = record(&root, fact_key(uid));
        assert_eq!(r.state, PassState::Pending, "LLM 故障那一支是 pending + 退避：{uid}");
        assert!(r.next_retry_at_ms.is_some(), "{uid}");
    }

    // ② 等价于 `hub review decide <id> --approve --reason operator`：只推进状态，不写库。
    let approved = record_decision(
        root.memory(),
        id,
        &Decision {
            stage: ReviewStage::Human,
            verdict: ReviewVerdict::Pass,
            reason_code: "operator".into(),
            rules_version: "review-gate-v0.1".into(),
            model: String::new(),
            prompt_version: String::new(),
            input_sha256: item.payload_sha256.clone(),
            review_input_sha256: String::new(),
            decider: Decider::Human,
            detail: "operator".into(),
        },
        NOW + 1,
    )
    .unwrap();
    assert_eq!(approved, ReviewState::Approved);
    assert_eq!(count(root.memory(), "fact"), 0, "放行本身不写库");

    // ③ 退避到期后以 `EnqueueOnly` 重跑同一批：载荷不变 ⇒ 命中同一个 item（approved），
    //    引擎按台账补写。脚本只给抽取：approved 若又问了窄审，第二次调用会拿到 `Timeout`。
    let later = NOW + 61_000;
    let plan = fact_plan_at(&root, later);
    assert_eq!(plan.batch.rows.len(), 3, "退避到期后同一批三轮都可重选");
    let backend = ScriptBackend::one(fact_reply());
    let out = run_fact_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        later,
        ReviewMode::EnqueueOnly,
    )
    .unwrap();

    assert_eq!(
        backend.calls.get(),
        1,
        "approved 的判决从台账重建：这一次只问抽取，不再有窄审调用"
    );
    assert_eq!(out.state, PassState::Done, "引擎说 Applied ⇒ done");
    assert_eq!(out.written, 1);
    assert_eq!(out.facts, 1);
    assert_eq!(count(root.memory(), "fact"), 1, "补写必须落一行 fact");
    assert_eq!(only_item(root.memory()).1, "applied");
    assert_eq!(record(&root, fact_key(&uids[0])).state, PassState::Done);
    assert_eq!(
        decision_stages(root.memory(), id),
        vec![
            "program".to_string(),
            "human".to_string(),
            "apply".to_string()
        ],
        "人工放行一条 + 引擎补写的 apply（approved 不重记 review）"
    );
}

// ── event ─────────────────────────────────────────────────────────────────────

/// 5. event `EnqueueOnly`：有草稿的轮 `pending(review_pending)`、覆盖缺口的轮
///    `rejected(uncovered)`、0 行、item pending、只调一次模型。
#[test]
fn event_enqueue_only_marks_the_draft_turn_and_keeps_the_uncovered_state() {
    let (_dir, root, uids) = seeded_event();
    let plan = event_plan(&root);
    let backend = ScriptBackend::one(event_reply());

    let out: EventOutcome = run_event_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
        ReviewMode::EnqueueOnly,
    )
    .unwrap();

    assert_eq!(backend.calls.get(), 1, "审核模型一次都不开");
    assert_eq!((out.turns, out.drafts, out.dropped, out.uncovered), (2, 1, 0, 1));
    assert_eq!(
        (
            out.written,
            out.rejected,
            out.uncertain,
            out.facts,
            out.cards,
            out.mentions
        ),
        (0, 0, 0, 0, 0, 0)
    );
    assert_eq!(out.state, PassState::Pending);
    assert_eq!(count(root.memory(), "fact"), 0);
    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "pending_review");
    assert_eq!(decision_stages(root.memory(), id), vec!["program".to_string()]);

    assert_awaiting_review(&root, event_key(&uids[0]));
    let gap = record(&root, event_key(&uids[1]));
    assert_eq!(gap.state, PassState::Rejected);
    assert_eq!(gap.reason.as_deref(), Some("uncovered"));
}

// ── naming ────────────────────────────────────────────────────────────────────

/// 6. naming `EnqueueOnly`：A1 非空 ⇒ 0 锚、批内**每一轮** `pending(review_pending)`、
///    item pending；A1 双跑两次调用，审核模型一次都不开。
#[test]
fn naming_enqueue_only_marks_every_turn_pending() {
    let (_dir, root, uids) = seeded_naming();
    let plan = naming_plan(&root);
    let backend = ScriptBackend::two(a1_reply(), a1_reply());

    let out: NamingOutcome = run_naming_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
        ReviewMode::EnqueueOnly,
    )
    .unwrap();

    assert_eq!(backend.calls.get(), 2, "A1 双跑各一次，窄审一次都不开");
    assert_eq!((out.turns, out.anchors, out.mentions, out.filtered), (4, 0, 0, 0));
    assert_eq!(out.state, PassState::Pending);
    assert_eq!(count(root.memory(), "anchor"), 0, "EnqueueOnly 不许写锚");
    assert_eq!(count(root.memory(), "mention"), 0);

    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "pending_review");
    assert_eq!(decision_stages(root.memory(), id), vec!["program".to_string()]);

    for uid in &uids {
        assert_awaiting_review(&root, naming_key(uid));
    }
}

/// 7. naming 的合法空批（`anchors+mentions` 为空）与本模式无关：仍走既有 `empty` 路径，
///    不入队、不标 pending。
#[test]
fn naming_enqueue_only_leaves_a_legal_empty_batch_empty() {
    let (_dir, root, uids) = seeded_naming();
    let plan = naming_plan(&root);
    let backend = ScriptBackend::two(empty_a1_reply(), empty_a1_reply());

    let out = run_naming_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
        ReviewMode::EnqueueOnly,
    )
    .unwrap();

    assert_eq!(backend.calls.get(), 2, "空批连审核都不该问");
    assert_eq!(out.state, PassState::Empty);
    assert_eq!((out.anchors, out.mentions), (0, 0));
    assert_eq!(count(root.memory(), "review_item"), 0, "零产出不入队");
    for uid in &uids {
        let r = record(&root, naming_key(uid));
        assert_eq!(r.state, PassState::Empty, "{uid}");
        assert_eq!(r.reason, None, "{uid}");
    }
}
