//! S3b-2a：`review::run` 引擎的 E2E（设计 `2026-10-26-review-queue-s3` §4/§5/§6/§8）。
//!
//! 断的是引擎本身，不是 runner 的接线：每条用例先用 `run_fact`/`run_event` 配一个
//! "第一次合法抽取、第二次 `BackendError`" 的 mock 后端，造出一条停在 `pending_review`
//! 的真 item（0 行），再让 [`run_item`] 只凭载荷 + 转录把复核、判定、写库跑完。
//!
//! 覆盖（与派发档 §5 的清单一一对应）：
//! 1. fact happy path：三段判定 + 1 行 + prompt 与 runner 那次失败调用逐字节相同；
//! 2. fact mixed：只写 pass 那条，verdicts 逐条；
//! 3. fact 全 reject：0 行、rejected；
//! 4. fact 全 uncertain：0 行、quarantined；
//! 5. 审核输出坏 JSON：Err(Review)、quarantined、artifact(stage=review) 一行；
//! 6. 审核 LLM 故障：Err(Llm)、item 仍 pending、0 行、无 review 判定；
//! 7. 幂等：applied 后再跑 ⇒ written=0、applied_now=false、行数与判定数不变；
//! 8. 载荷缺新键 ⇒ Ok、quarantined、`PAYLOAD_GAP`、0 行；
//! 9. event happy path：extras 的 participants/place/value_json 落到事件行；
//! 10. `pass_id=time` ⇒ `Err(Unsupported)`（naming 自 S3b-3b、rows 自 S3c-3b 起受支持，
//!     钉子在 time——这台引擎没有 time 的重放与判定实现）。
//!
//! 夹具风格与 `tests/review_wiring.rs` 相同：真 `Root::open(tempdir)` + `mod common`，
//! 正反例只用通用占位名（张三/李四/某项目），不含任何语料专名。

use rc_distill::pass_event::{run_event, select_event_batch};
use rc_distill::pass_fact::{
    run_fact, run_fact_with, select_fact_batch, FACT_PIPELINE_PREFIX,
};
use rc_distill::pass_fact_contract::{FactDraft, FactPassError};
use rc_distill::pass_prompt::{
    fact_review_challenge_system, fact_review_system, fact_review_user, FactFamily,
    ReviewInputContext,
};
use rc_distill::review::run::{
    run_item, RunError, PAYLOAD_GAP, REVIEW_INPUT_MAX_BYTES, REVIEW_INPUT_TOO_LARGE,
};
use rc_distill::review::{
    ReviewMode, PASS_EVENT_REVIEW_VERSION, PASS_FACT_REVIEW_VERSION, REVIEW_VERDICTS_MISSING,
    GATE_RULES_VERSION,
};
use rc_distill::DistillError;
use rc_distill::{Batch, TurnRow};
use rc_ledger::pass_state::{self, PassState};
use rc_ledger::review_mark;
use rc_ledger::water;
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::review::{
    enqueue, payload_sha256, record_decision, Decider, Decision, NewItem, ReviewStage, ReviewState,
    ReviewVerdict,
};
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{AnchorRole, Provenance, Scope, ScopeKind};
use rc_store::{MemoryHandle, Root};
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

mod common;
use common::{adopt_rows, seed_bodies};

const NOW: i64 = 1_700_000_000_000;
const MODEL: &str = "fake-review-model";
const PROMISE: &str = "我下周三之前把对账单发给你";
/// t2 是 **agent** 轮：正文要真写着主语"张三"（旧夹具的"好的，收到"里没有主语名，
/// 却把主语挂在张三上——新硬检下那是不真实的形状）。
const RECEIVED_BODY: &str = "张三已经收到对账单";
const CONFIG_BODY: &str = "昨晚在机房我把配置改回去，服务就恢复了";
const CONFIG_QUOTE: &str = "昨晚在机房我把配置改回去，服务就恢复了";
const REJECTED_FACT_VALUE: &str = "收到对账单";

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

fn add_anchor(
    root: &Root,
    scope: &Scope,
    kind: &str,
    canonical: &str,
    uid: &str,
    created_at_ms: i64,
) -> String {
    let key = put_entity(
        root.memory(),
        &EntityContent {
            kind: kind.into(),
            title: canonical.into(),
            scope: scope.clone(),
            aliases: vec![],
        },
        &[Provenance { turn_uid: uid.to_string(), anchor_role: AnchorRole::Human }],
        created_at_ms,
    )
    .unwrap();
    adopt_rows(root, "anchor", "alice", Some("p1"), "project", "project", "proj-x");
    key.id
}

/// fact：3 轮（承诺 + 两句寒暄），张三 a1、李四 a2 两个活锚。
fn seeded_fact() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[PROMISE, RECEIVED_BODY, "谢谢"], 1_000);
    set_owner(&mut root);
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    add_anchor(&root, &scope, "person", "李四", &uids[2], NOW - 2_000);
    add_anchor(&root, &scope, "person", "张三", &uids[0], NOW - 1_000);
    (dir, root, uids)
}

/// event：1 轮（配置恢复），张三 a1、地点「某项目」a2 两个活锚。
fn seeded_event() -> (tempfile::TempDir, Root, Vec<String>, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[CONFIG_BODY], 1_000);
    set_owner(&mut root);
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    let place = add_anchor(&root, &scope, "place", "某项目", &uids[0], NOW - 2_000);
    let zhang = add_anchor(&root, &scope, "person", "张三", &uids[0], NOW - 1_000);
    (dir, root, uids, zhang, place)
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

/// 抽取回复：t1 一条 promise（a1=张三、a2=李四），t2/t3 空覆盖。
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

/// mixed：t1 一条（审核 pass）+ t2 一条（审核 reject），t3 空覆盖。
fn fact_mixed_reply() -> String {
    json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a1", "predicate": "promise",
            "object": "a2", "value": "把对账单发给你", "category": "promise",
            "mode": "stated", "time_text": "下周三之前", "quote": PROMISE,
            "evidence": ["t1"], "text": "张三承诺在下周三之前把对账单发给李四。"
        }, {
            "ref": "f2", "turn": "t2", "subject": "a1", "predicate": "promise",
            "object": null, "value": "收到对账单", "category": "promise",
            "mode": "stated", "time_text": "", "quote": RECEIVED_BODY,
            "evidence": ["t2"], "text": "张三收到对账单。"
        }],
        "empty_turns": ["t3"]
    })
    .to_string()
}

/// fact 审核回复（两档同形）：每条判决带 `evidence`——夹具引文里都有「对账单」三个字，
/// 所以 pass/reject 统一用它当证据；uncertain 允许 null（工单 §7.2 四键）。
fn fact_review(decisions: &[(&str, &str, Option<&str>)]) -> String {
    let with_evidence: Vec<(&str, &str, Option<&str>, Option<&str>)> = decisions
        .iter()
        .map(|(reference, verdict, code)| {
            let evidence = if *verdict == "uncertain" { None } else { Some("对账单") };
            (*reference, *verdict, *code, evidence)
        })
        .collect();
    fact_review_with_evidence(&with_evidence)
}

/// 证据可定制的一版：降级用例要构造"证据不在 quote 里"。
fn fact_review_with_evidence(
    decisions: &[(&str, &str, Option<&str>, Option<&str>)],
) -> String {
    let verdicts: Vec<Value> = decisions
        .iter()
        .map(|(reference, verdict, code, evidence)| {
            json!({
                "ref": reference,
                "verdict": verdict,
                "code": code,
                "evidence": evidence,
            })
        })
        .collect();
    json!({"schema_version": 0, "pass": "fact_review", "family": "promise", "verdicts": verdicts})
        .to_string()
}

/// event 抽取回复：t1 一条 state_change（actor a1=张三、place a2=某项目）。
fn event_reply() -> String {
    json!({
        "schema_version": 0, "pass": "event",
        "events": [{
            "ref": "e1", "turn": "t1", "category": "state_change", "actor": "a1",
            "trigger": "改回去", "action": "restore_config", "object": null, "object_text": "配置文件",
            "participants": ["a1"], "place": "a2", "result": "done", "mode": "stated",
            "time_text": "昨晚", "quote": CONFIG_QUOTE, "evidence": ["t1"],
            "text": "张三昨晚把配置改回去，服务恢复。"
        }],
        "empty_turns": []
    })
    .to_string()
}

fn event_review(decisions: &[(&str, &str, Option<&str>)]) -> String {
    let verdicts: Vec<Value> = decisions
        .iter()
        .map(|(reference, verdict, code)| json!({"ref": reference, "verdict": verdict, "code": code}))
        .collect();
    json!({"schema_version": 0, "pass": "event_review", "verdicts": verdicts}).to_string()
}

/// 回放 backend：按序给回复，记录每次调用拿到的 Prompt（脚本耗尽 = `Timeout`）。
struct ScriptBackend {
    replies: RefCell<VecDeque<Result<String, LlmFailure>>>,
    calls: Cell<usize>,
    prompts: RefCell<Vec<Prompt>>,
}

impl ScriptBackend {
    fn new(replies: Vec<Result<String, LlmFailure>>) -> Self {
        ScriptBackend {
            replies: RefCell::new(replies.into()),
            calls: Cell::new(0),
            prompts: RefCell::new(Vec::new()),
        }
    }
    /// 引擎对一片 fact 发两次审核调用（proposer + challenger）；两档同形，
    /// 用同一份判决回复即可（challenger 的"找反证"角色在 prompt 里，不在回复形状里）。
    /// event/naming/rows 只消费第一条，多出来的一条不会被读到。
    fn engine(review: String) -> Self {
        ScriptBackend::new(vec![Ok(review.clone()), Ok(review)])
    }
}

impl LlmBackend for ScriptBackend {
    fn complete(&self, prompt: &Prompt) -> Result<String, LlmFailure> {
        self.calls.set(self.calls.get() + 1);
        self.prompts.borrow_mut().push(prompt.clone());
        self.replies.borrow_mut().pop_front().unwrap_or(Err(LlmFailure::Timeout))
    }
    fn slug(&self) -> Option<&str> {
        Some(MODEL)
    }
}

/// 造一条停在 `pending_review` 的 fact item，返回 `(id, runner 那次失败审核调用捕获的 Prompt)`。
fn pending_fact(root: &Root) -> (i64, Prompt) {
    let plan = fact_plan(root);
    let backend = ScriptBackend::new(vec![Ok(fact_reply()), Err(LlmFailure::BackendError)]);
    let err = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW)
        .unwrap_err();
    assert!(matches!(err, DistillError::Llm(LlmFailure::BackendError)), "{err}");
    assert_eq!(count(root.memory(), "fact"), 0, "审核失败不许写行");
    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "pending_review");
    let prompts = backend.prompts.borrow();
    assert_eq!(prompts.len(), 2, "抽取一次 + 审核一次（第二次失败）");
    (id, prompts[1].clone())
}

/// 同 [`pending_fact`]，但用 mixed 抽取（2 条草稿）。
fn pending_fact_mixed(root: &Root) -> i64 {
    let plan = fact_plan(root);
    let backend = ScriptBackend::new(vec![Ok(fact_mixed_reply()), Err(LlmFailure::BackendError)]);
    run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap_err();
    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "pending_review");
    id
}

fn pending_event(root: &Root) -> i64 {
    let plan = event_plan(root);
    let backend = ScriptBackend::new(vec![Ok(event_reply()), Err(LlmFailure::BackendError)]);
    let err =
        run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap_err();
    assert!(matches!(err, DistillError::Llm(LlmFailure::BackendError)), "{err}");
    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "pending_review");
    id
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

fn item_state(m: &MemoryHandle, id: i64) -> String {
    m.conn()
        .query_row("SELECT state FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap()
}

struct DecRow {
    stage: String,
    verdict: String,
    reason_code: String,
    model: String,
    prompt_version: String,
    input_sha256: String,
    review_input_sha256: String,
    decider: String,
    detail: String,
}

fn decisions(m: &MemoryHandle, item_id: i64) -> Vec<DecRow> {
    let mut st = m
        .conn()
        .prepare(
            "SELECT stage, verdict, reason_code, model, prompt_version, input_sha256,
                    review_input_sha256, decider, detail
               FROM review_decision WHERE item_id=?1 ORDER BY id",
        )
        .unwrap();
    let rows = st
        .query_map([item_id], |r| {
            Ok(DecRow {
                stage: r.get(0)?,
                verdict: r.get(1)?,
                reason_code: r.get(2)?,
                model: r.get(3)?,
                prompt_version: r.get(4)?,
                input_sha256: r.get(5)?,
                review_input_sha256: r.get(6)?,
                decider: r.get(7)?,
                detail: r.get(8)?,
            })
        })
        .unwrap();
    rows.collect::<Result<_, _>>().unwrap()
}

fn review_artifacts(root: &Root) -> i64 {
    root.ledger()
        .conn()
        .query_row(
            "SELECT count(*) FROM extract_pass_artifact WHERE stage='review'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

/// `(pass_id, run_id, turn_uid)`：审核解析失败的 artifact 行，按落库顺序。
fn review_artifact_rows(root: &Root) -> Vec<(String, String, String)> {
    let mut st = root
        .ledger()
        .conn()
        .prepare(
            "SELECT pass_id, run_id, turn_uid FROM extract_pass_artifact
              WHERE stage='review' ORDER BY id",
        )
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// 测试专用：把 item 从 applied 拨回 approved，模拟"上次写库失败/重试"。
fn force_approved(m: &MemoryHandle, id: i64) {
    m.conn()
        .execute(
            "UPDATE review_item SET state='approved', applied_at_ms=NULL WHERE id=?1",
            [id],
        )
        .unwrap();
    assert_eq!(item_state(m, id), "approved");
}

/// 合法改写载荷：同步用 `enqueue` 的同一条公式重算列指纹。
///
/// 只改 `payload_json` 而不改 `payload_sha256` 就是篡改（H4a 的引擎在 replay 之前拦下）；
/// 想测"载荷形状"的分支必须走这条"重新落章"的路径。
fn rewrite_payload(m: &MemoryHandle, id: i64, mutate: impl FnOnce(&mut Value)) {
    let (payload_json, owner_json, pass_id, family): (String, String, String, String) = m
        .conn()
        .query_row(
            "SELECT payload_json, owner_json, pass_id, family FROM review_item WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    let mut payload: Value = serde_json::from_str(&payload_json).unwrap();
    mutate(&mut payload);
    let payload_json = payload.to_string();
    let sha = payload_sha256(&pass_id, &family, &owner_json, &payload_json);
    m.conn()
        .execute(
            "UPDATE review_item SET payload_json=?2, payload_sha256=?3 WHERE id=?1",
            rusqlite::params![id, payload_json, sha],
        )
        .unwrap();
}

/// 篡改载荷：只改 `payload_json`，**不动**列里的 `payload_sha256`。
fn tamper_payload(m: &MemoryHandle, id: i64, mutate: impl FnOnce(&mut Value)) {
    let payload_json: String = m
        .conn()
        .query_row("SELECT payload_json FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap();
    let mut payload: Value = serde_json::from_str(&payload_json).unwrap();
    mutate(&mut payload);
    m.conn()
        .execute(
            "UPDATE review_item SET payload_json=?2 WHERE id=?1",
            rusqlite::params![id, payload.to_string()],
        )
        .unwrap();
}

/// 人工放行：追加一条 `stage=human, verdict=pass` 的判定（从 approved 合法）。
fn human_pass(root: &Root, id: i64) {
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
}

// ── fact ──────────────────────────────────────────────────────────────────────

/// 1. happy path：三段判定、1 行、item=applied；审核输入与 runner 那次失败调用逐字节相同。
#[test]
fn fact_happy_path_applies_and_matches_the_runner_prompt_byte_for_byte() {
    let (_dir, root, _uids) = seeded_fact();
    let (id, runner_prompt) = pending_fact(&root);
    let engine = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));

    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);
    assert!(out.applied_now);
    assert_eq!(out.written, 1);
    assert_eq!((out.facts, out.cards, out.mentions), (1, 1, 2));
    assert_eq!(out.verdicts, Some(vec![("f1".to_string(), ReviewVerdict::Pass)]));
    assert_eq!(out.fail_closed_reason, None);
    assert_eq!(count(root.memory(), "fact"), 1);

    // prompt 等价：第一次 backend 调用（proposer）的 system/user 与 runner 第二次调用逐字节
    // 相等；第二次调用是 challenger，读**同一片 user**（同一 chunk、同一套 ref，工单 §2.1）。
    let prompts = engine.prompts.borrow();
    assert_eq!(prompts.len(), 2, "一片 fact 审核 = proposer + challenger 两次调用");
    assert_eq!(prompts[0].system, runner_prompt.system, "system 必须逐字节相同");
    assert_eq!(prompts[0].user, runner_prompt.user, "user 必须逐字节相同");
    assert_eq!(
        prompts[1].system,
        fact_review_challenge_system(FactFamily::Promise),
        "第二次调用是 challenger 的 system"
    );
    assert_eq!(prompts[1].user, runner_prompt.user, "两档读同一片、同一套 ref");
    assert!(!prompts[0].derive && !prompts[1].derive);

    let rows = decisions(root.memory(), id);
    assert_eq!(
        rows.iter().map(|r| r.stage.as_str()).collect::<Vec<_>>(),
        vec!["program", "review", "apply"]
    );
    assert_eq!((rows[1].verdict.as_str(), rows[1].reason_code.as_str()), ("pass", "review_pass"));
    assert_eq!(rows[1].model, MODEL, "review 的 model 有值");
    assert_eq!(rows[1].prompt_version, PASS_FACT_REVIEW_VERSION, "review 的 prompt_version 有值");
    assert_eq!(rows[1].decider, "center");
    assert_eq!(rows[2].reason_code, "applied");
    assert_eq!(rows[2].prompt_version, PASS_FACT_REVIEW_VERSION);
    let payload_sha: String = root
        .memory()
        .conn()
        .query_row("SELECT payload_sha256 FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap();
    assert_eq!(rows[2].input_sha256, payload_sha, "apply 记的是载荷 sha");
    assert_eq!(rows[2].review_input_sha256, "", "apply 没有送审输入");

    // input_sha 与 runner 同一指纹式：proposer system 是第二格，challenger system 与
    // 这一片 user 依次进第四格（§7.2 两次调用都覆盖）。
    let expected = water::extraction_fingerprint(
        &format!("{FACT_PIPELINE_PREFIX}/promise"),
        MODEL,
        &runner_prompt.system,
        &format!(
            "{}\n\u{1e}\n{}",
            fact_review_challenge_system(FactFamily::Promise),
            runner_prompt.user
        ),
    );
    // H4b：载荷指纹进 input_sha256，送审输入指纹进 review_input_sha256。
    assert_eq!(rows[1].input_sha256, payload_sha, "review 行的 input_sha256 是载荷指纹");
    assert_eq!(
        rows[1].review_input_sha256, expected,
        "review_input_sha256 与 runner 的第二次调用同源"
    );
}

/// 2. mixed（1 pass + 1 reject）：只写 pass 那条，verdicts 逐条，p>0 ⇒ applied。
#[test]
fn fact_mixed_writes_only_the_passing_draft() {
    let (_dir, root, _uids) = seeded_fact();
    let id = pending_fact_mixed(&root);
    let engine = ScriptBackend::engine(fact_review(&[
        ("f1", "pass", None),
        ("f2", "reject", Some("not_durable")),
    ]));

    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);
    assert!(out.applied_now);
    assert_eq!(out.written, 1);
    assert_eq!(out.duplicate, 0);
    assert_eq!(
        out.verdicts,
        Some(vec![
            ("f1".to_string(), ReviewVerdict::Pass),
            ("f2".to_string(), ReviewVerdict::Reject),
        ])
    );
    assert_eq!(count(root.memory(), "fact"), 1);
    let leaked: i64 = root
        .memory()
        .conn()
        .query_row(
            "SELECT count(*) FROM fact WHERE COALESCE(value_text,'')=?1",
            [REJECTED_FACT_VALUE],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leaked, 0, "被 reject 的草稿不许写进记忆表");

    let rows = decisions(root.memory(), id);
    assert_eq!(rows[1].reason_code, "review_mixed");
    assert_eq!(rows[1].detail, "p=1;r=1;u=0;pass=f1;uncertain=-");
    assert_eq!(rows[2].detail, "facts=1;cards=1;mentions=2");
}

/// 3. 全 reject：0 行、item=rejected、verdicts 逐条。
#[test]
fn fact_all_reject_writes_nothing() {
    let (_dir, root, _uids) = seeded_fact();
    let (id, _) = pending_fact(&root);
    let engine =
        ScriptBackend::engine(fact_review(&[("f1", "reject", Some("not_durable"))]));

    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(out.item_state, ReviewState::Rejected);
    assert!(!out.applied_now);
    assert_eq!(out.written, 0);
    assert_eq!(out.verdicts, Some(vec![("f1".to_string(), ReviewVerdict::Reject)]));
    assert_eq!(count(root.memory(), "fact"), 0);
    let rows = decisions(root.memory(), id);
    assert_eq!(rows.iter().map(|r| r.stage.as_str()).collect::<Vec<_>>(), vec!["program", "review"]);
    assert_eq!(rows[1].reason_code, "review_reject");
}

/// 4. 全 uncertain：0 行、item=quarantined、verdicts 逐条。
#[test]
fn fact_all_uncertain_quarantines_without_writing() {
    let (_dir, root, _uids) = seeded_fact();
    let (id, _) = pending_fact(&root);
    let engine =
        ScriptBackend::engine(fact_review(&[("f1", "uncertain", Some("quote_mismatch"))]));

    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(out.item_state, ReviewState::Quarantined);
    assert!(!out.applied_now);
    assert_eq!(out.written, 0);
    assert_eq!(out.verdicts, Some(vec![("f1".to_string(), ReviewVerdict::Uncertain)]));
    assert_eq!(count(root.memory(), "fact"), 0);
    let rows = decisions(root.memory(), id);
    assert_eq!(rows[1].reason_code, "review_uncertain");
}

// ── §7.2 正反双判：裁决矩阵 + 证据闸 + 失败面 ─────────────────────────────────

/// 一次单草稿 fact 审核的读数（root 在运行结束后即可释放）。
struct PairRun {
    state: ReviewState,
    adjudications: Vec<(String, ReviewVerdict, Option<String>)>,
    downgrades: Vec<(String, &'static str)>,
    written: usize,
}

/// 一条草稿，两次调用各给一份回复（proposer 在前、challenger 在后）。
fn run_pair(proposer: String, challenger: String) -> PairRun {
    let (_dir, root, _uids) = seeded_fact();
    let (id, _) = pending_fact(&root);
    let engine = ScriptBackend::new(vec![Ok(proposer), Ok(challenger)]);
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    PairRun {
        state: out.item_state,
        adjudications: out.adjudications,
        downgrades: out.evidence_downgrades,
        written: out.written,
    }
}

fn verdict_pair<'a>(verdict: &'a str, code: Option<&'a str>) -> Vec<(&'a str, &'a str, Option<&'a str>)> {
    vec![("f1", verdict, code)]
}

/// 28. 裁决矩阵（工单 §2.4）：两侧都 pass 才 pass；challenger reject 的 code 胜出；
///     否则 proposer reject 的 code 胜出；任一侧 uncertain ⇒ uncertain。
#[test]
fn the_two_sided_ruling_matrix_is_fixed() {
    let pass = || fact_review(&verdict_pair("pass", None));

    // pass / pass ⇒ pass
    let run = run_pair(pass(), pass());
    assert_eq!(
        run.adjudications,
        vec![("f1".to_string(), ReviewVerdict::Pass, None)],
        "两侧都 pass 才是 pass"
    );
    assert_eq!((run.state, run.written), (ReviewState::Applied, 1));

    // pass / reject ⇒ reject，code 取 challenger 的
    let run = run_pair(pass(), fact_review(&verdict_pair("reject", Some("not_durable"))));
    assert_eq!(
        run.adjudications,
        vec![(
            "f1".to_string(),
            ReviewVerdict::Reject,
            Some("not_durable".to_string())
        )],
        "challenger reject 的 code 胜出"
    );
    assert_eq!((run.state, run.written), (ReviewState::Rejected, 0));

    // reject / pass ⇒ reject，code 取 proposer 的
    let run = run_pair(
        fact_review(&verdict_pair("reject", Some("missing_subject"))),
        pass(),
    );
    assert_eq!(
        run.adjudications,
        vec![(
            "f1".to_string(),
            ReviewVerdict::Reject,
            Some("missing_subject".to_string())
        )],
        "challenger 没找到反证时，proposer reject 的 code 必须留下"
    );
    assert_eq!((run.state, run.written), (ReviewState::Rejected, 0));

    // reject / reject ⇒ reject，码仍取 challenger 的
    let run = run_pair(
        fact_review(&verdict_pair("reject", Some("not_durable"))),
        fact_review(&verdict_pair("reject", Some("quote_mismatch"))),
    );
    assert_eq!(
        run.adjudications,
        vec![(
            "f1".to_string(),
            ReviewVerdict::Reject,
            Some("quote_mismatch".to_string())
        )]
    );
    assert_eq!(run.state, ReviewState::Rejected);

    // pass / uncertain ⇒ uncertain（不做"少数服从多数"）
    let run = run_pair(pass(), fact_review(&verdict_pair("uncertain", None)));
    assert_eq!(
        run.adjudications,
        vec![("f1".to_string(), ReviewVerdict::Uncertain, None)]
    );
    assert_eq!(run.state, ReviewState::Quarantined);

    // uncertain / reject ⇒ uncertain（任一侧 uncertain 就压过反证）
    let run = run_pair(
        fact_review(&verdict_pair("uncertain", Some("quote_mismatch"))),
        fact_review(&verdict_pair("reject", Some("not_durable"))),
    );
    assert_eq!(
        run.adjudications,
        vec![("f1".to_string(), ReviewVerdict::Uncertain, None)]
    );
    assert_eq!(run.state, ReviewState::Quarantined);
}

/// 29. 证据闸（工单 §2.3）：缺证据 / 证据越界都逐条降 uncertain，降级数与原因进返回值；
///     降级后不存在任何"无证据的 pass 或 reject"（验收口径"无证据判决 = 0"）。
#[test]
fn the_evidence_gate_downgrades_and_reports_the_count() {
    // proposer 的证据不在 quote 内（PROMISE 里没有「李四」）；challenger 证据合格。
    let run = run_pair(
        fact_review_with_evidence(&[("f1", "pass", None, Some("李四"))]),
        fact_review(&verdict_pair("pass", None)),
    );
    assert_eq!(
        run.adjudications,
        vec![("f1".to_string(), ReviewVerdict::Uncertain, None)],
        "越界证据的 pass 降 uncertain，不许当 pass 用"
    );
    assert_eq!(
        run.downgrades,
        vec![("f1".to_string(), "evidence_not_in_quote")],
        "降级原因必须读得出来"
    );
    assert_eq!(run.state, ReviewState::Quarantined);

    // 两侧都缺证据：两条降级都记下；无证据的 pass/reject 一条都不剩。
    let run = run_pair(
        fact_review_with_evidence(&[("f1", "pass", None, None)]),
        fact_review_with_evidence(&[("f1", "reject", Some("not_durable"), None)]),
    );
    assert_eq!(
        run.adjudications,
        vec![("f1".to_string(), ReviewVerdict::Uncertain, None)]
    );
    assert_eq!(
        run.downgrades,
        vec![
            ("f1".to_string(), "evidence_missing"),
            ("f1".to_string(), "evidence_missing"),
        ]
    );
    assert!(
        run.adjudications
            .iter()
            .all(|(_, verdict, _)| *verdict == ReviewVerdict::Uncertain),
        "降级之后不许留下任何无证据的 pass/reject"
    );
}

/// 30. 失败面（工单 §4.4）：proposer 解析失败但 challenger 判 reject ⇒ 该条 reject（不漏）；
///     两侧都失败 ⇒ 全部 uncertain（与今天同一条路：artifact 照落、item 隔离）。
#[test]
fn a_failed_proposer_still_lets_the_challenger_reject() {
    let (_dir, root, _uids) = seeded_fact();
    let (id, _) = pending_fact(&root);
    let engine = ScriptBackend::new(vec![
        Ok("{}".to_string()),
        Ok(fact_review(&verdict_pair("reject", Some("not_durable")))),
    ]);
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(
        out.adjudications,
        vec![(
            "f1".to_string(),
            ReviewVerdict::Reject,
            Some("not_durable".to_string())
        )],
        "proposer 坏掉不是 pass：challenger 找到反证就 reject"
    );
    assert_eq!((out.item_state, out.written), (ReviewState::Rejected, 0));
    assert_eq!(review_artifacts(&root), 1, "proposer 的坏原文仍要留证");
    let rows = decisions(root.memory(), id);
    assert_eq!(
        (rows[1].stage.as_str(), rows[1].verdict.as_str(), rows[1].reason_code.as_str()),
        ("review", "reject", "review_reject")
    );

    // proposer 坏 + challenger 没找到反证 ⇒ 不许因此 pass，一律 uncertain。
    let run = run_pair("{".to_string(), fact_review(&verdict_pair("pass", None)));
    assert_eq!(
        run.adjudications,
        vec![("f1".to_string(), ReviewVerdict::Uncertain, None)]
    );
    assert_eq!(run.state, ReviewState::Quarantined);

    // 两侧都失败 ⇒ 现有那条路：Err(Review)、item 隔离、review 判定 uncertain。
    let (_dir, root, _uids) = seeded_fact();
    let (id, _) = pending_fact(&root);
    let engine = ScriptBackend::new(vec![Ok("{}".to_string()), Ok("{}".to_string())]);
    let err = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW)
        .unwrap_err();
    assert!(matches!(err, RunError::Review(DistillError::Fact(FactPassError::Json))));
    assert_eq!(item_state(root.memory(), id), "quarantined");
    let rows = decisions(root.memory(), id);
    assert_eq!(rows[1].stage, "review");
    assert_eq!(rows[1].verdict, "uncertain", "两侧都失败 ⇒ 全部 uncertain");
    assert_eq!(review_artifacts(&root), 2);
}

/// 5. 审核输出不是判决：先落 artifact，再记 uncertain；返回 Err(Review)。
#[test]
fn bad_review_json_records_the_artifact_and_quarantines() {
    let (_dir, root, _uids) = seeded_fact();
    let (id, _) = pending_fact(&root);
    let engine = ScriptBackend::engine("{}".to_string());

    let err = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW)
        .unwrap_err();
    assert!(
        matches!(err, RunError::Review(DistillError::Fact(FactPassError::Json))),
        "{err:?}"
    );
    assert_eq!(item_state(root.memory(), id), "quarantined");
    assert_eq!(count(root.memory(), "fact"), 0);
    assert_eq!(
        review_artifacts(&root),
        2,
        "proposer 与 challenger 各留一份原文（两次都失败，artifact 照落）"
    );
    let rows = decisions(root.memory(), id);
    assert_eq!((rows[1].stage.as_str(), rows[1].verdict.as_str()), ("review", "uncertain"));
    assert_eq!(rows[1].reason_code, "review_json");
}

/// 13. artifact 的 `run_id` 取状态行那一个（`fact-promise-<ms>`），不取载荷里的抽取指纹：
///     仍返回 `Err(Review)`、item 仍 quarantined，artifact 是补件不掩盖解析错误。
#[test]
fn bad_review_artifact_uses_the_pass_state_run_id() {
    let (_dir, root, uids) = seeded_fact();
    let (id, _) = pending_fact(&root);
    let engine = ScriptBackend::engine("{}".to_string());

    let err = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW)
        .unwrap_err();
    assert!(
        matches!(err, RunError::Review(DistillError::Fact(FactPassError::Json))),
        "{err:?}"
    );
    assert_eq!(item_state(root.memory(), id), "quarantined");

    let record = pass_state::get(
        root.ledger(),
        pass_state::PassKey { turn_uid: &uids[0], pass_id: "fact", family: "promise" },
    )
    .unwrap()
    .expect("runner 第一次调用给批内每轮落了 running 行");
    let expected = record.run_id.expect("running 行的 run_id");
    assert_eq!(expected, format!("fact-promise-{NOW}"), "与旧 runner 的 run_id 同一形状");

    let rows = review_artifact_rows(&root);
    assert_eq!(rows.len(), 2, "proposer + challenger 各一份");
    for row in &rows {
        assert_eq!(row.0, "fact");
        assert_eq!(row.1, expected, "artifact 的 run_id 取状态行，不取载荷指纹");
        assert_ne!(row.1.len(), 64, "不是 64 位的载荷/抽取指纹");
        assert_eq!(row.2, uids[0], "turn_uid 是批内第一轮");
    }
}

/// 6. 审核 LLM 故障：Err(Llm)、item 仍 pending、0 行、无 review 判定、无 artifact。
#[test]
fn review_llm_failure_leaves_the_item_pending() {
    let (_dir, root, _uids) = seeded_fact();
    let (id, _) = pending_fact(&root);
    let engine = ScriptBackend::new(vec![Err(LlmFailure::BackendError)]);

    let err = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW)
        .unwrap_err();
    assert!(matches!(err, RunError::Llm(LlmFailure::BackendError)), "{err:?}");
    assert_eq!(item_state(root.memory(), id), "pending_review");
    assert_eq!(count(root.memory(), "fact"), 0);
    assert_eq!(review_artifacts(&root), 0);
    let rows = decisions(root.memory(), id);
    assert_eq!(rows.len(), 1, "只应有 program pass 判定");
    assert_eq!(rows[0].stage, "program");
}

/// 7. 幂等：applied 后再跑引擎 ⇒ written=0、applied_now=false、行数与判定数不变。
#[test]
fn second_run_on_an_applied_item_is_a_no_op() {
    let (_dir, root, _uids) = seeded_fact();
    let (id, _) = pending_fact(&root);
    let first = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    run_item(root.memory(), root.transcript(), root.ledger(), id, &first, NOW).unwrap();
    assert_eq!(item_state(root.memory(), id), "applied");
    let facts_before = count(root.memory(), "fact");
    let decisions_before = decisions(root.memory(), id).len();

    let again = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &again, NOW + 5_000)
        .unwrap();
    assert_eq!(again.calls.get(), 0, "applied 不再调模型");
    assert_eq!(out.item_state, ReviewState::Applied);
    assert!(!out.applied_now);
    assert_eq!(out.written, 0);
    assert_eq!((out.facts, out.cards, out.mentions), (0, 0, 0));
    assert_eq!(out.verdicts, Some(vec![("f1".to_string(), ReviewVerdict::Pass)]));
    assert_eq!(count(root.memory(), "fact"), facts_before);
    assert_eq!(decisions(root.memory(), id).len(), decisions_before, "apply 判定不增加");
}

/// 14. Applied 重放的 fact 读数按旧 runner "组装但不写库"的口径：去重循环照跑，
///     命中计 `duplicate`（`written=0`），插入读数恒 0、`applied_now=false`、行数不变。
#[test]
fn applied_fact_replay_counts_the_dedup_hit_without_writing() {
    let (_dir, root, uids) = seeded_fact();
    let (id, _) = pending_fact(&root);
    let first = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    run_item(root.memory(), root.transcript(), root.ledger(), id, &first, NOW).unwrap();
    assert_eq!(count(root.memory(), "fact"), 1);

    let again = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    let out = run_item(
        root.memory(),
        root.transcript(),
        root.ledger(),
        id,
        &again,
        NOW + 5_000,
    )
    .unwrap();
    assert_eq!(again.calls.get(), 0, "applied 不再调模型");
    assert_eq!(out.item_state, ReviewState::Applied);
    assert!(!out.applied_now);
    assert_eq!(out.written, 0, "已在库 ⇒ 去重后没有行可交");
    assert_eq!(out.duplicate, 1, "重复的那一条按旧 runner 口径计数");
    assert_eq!(out.duplicate_uids.iter().collect::<Vec<_>>(), vec![&uids[0]]);
    assert_eq!((out.facts, out.cards, out.mentions), (0, 0, 0), "重放不新增插入行");
    assert_eq!(out.verdicts, Some(vec![("f1".to_string(), ReviewVerdict::Pass)]));
    assert_eq!(out.fail_closed_reason, None);
    assert_eq!(count(root.memory(), "fact"), 1, "行数不变");
}

/// 14b. C5：同一条证据轮上的**两条不同主语**的事实都要落库。
///     旧键只有 `(证据轮, value)`，而 `value` 为空是常态 ⇒ 第二条被当成重复静默丢掉。
///     两条草稿同轮、同类别、同谓词、都没有 value，只有主语不同——新键必须放过它们。
#[test]
fn two_facts_on_one_turn_with_an_empty_value_both_land() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let both = "张三和李四都说下周三之前把对账单发出去";
    let uids = seed_bodies(&mut root, &[both, RECEIVED_BODY, "谢谢"], 1_000);
    set_owner(&mut root);
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    add_anchor(&root, &scope, "person", "李四", &uids[0], NOW - 2_000);
    add_anchor(&root, &scope, "person", "张三", &uids[0], NOW - 1_000);

    let plan = fact_plan(&root);
    let reply = json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a1", "predicate": "promise",
            "object": "a2", "value": null, "category": "promise",
            "mode": "stated", "time_text": "", "quote": both,
            "evidence": ["t1"], "text": "张三承诺把对账单发出去。"
        }, {
            "ref": "f2", "turn": "t1", "subject": "a2", "predicate": "promise",
            "object": "a1", "value": null, "category": "promise",
            "mode": "stated", "time_text": "", "quote": both,
            "evidence": ["t1"], "text": "李四承诺把对账单发出去。"
        }],
        "empty_turns": ["t2", "t3"]
    })
    .to_string();
    let backend = ScriptBackend::new(vec![
        Ok(reply),
        Ok(fact_review(&[("f1", "pass", None), ("f2", "pass", None)])),
        Ok(fact_review(&[("f1", "pass", None), ("f2", "pass", None)])),
    ]);
    let out = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(out.duplicate, 0, "同轮不同主语不是重复：{out:?}");
    assert_eq!(out.written, 2, "两条槽位都要落库：{out:?}");
    assert_eq!(count(root.memory(), "fact"), 2);
}

/// 15. Applied 重放的 mixed 读数：verdicts 逐条重建，`written` 只计 pass 草稿
///     （fact 已被去重 ⇒ 0），reject 不进引擎计数、也不借重放写进记忆表。
#[test]
fn applied_mixed_replay_counts_only_the_passing_draft() {
    let (_dir, root, uids) = seeded_fact();
    let id = pending_fact_mixed(&root);
    let first = ScriptBackend::engine(fact_review(&[
        ("f1", "pass", None),
        ("f2", "reject", Some("not_durable")),
    ]));
    let applied =
        run_item(root.memory(), root.transcript(), root.ledger(), id, &first, NOW).unwrap();
    assert_eq!((applied.written, applied.duplicate), (1, 0), "首次只有 f1 写库");
    assert_eq!(count(root.memory(), "fact"), 1);

    let again = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    let out = run_item(
        root.memory(),
        root.transcript(),
        root.ledger(),
        id,
        &again,
        NOW + 5_000,
    )
    .unwrap();
    assert_eq!(again.calls.get(), 0, "脚本只剩一条：重放若问模型会耗尽成 Timeout");
    assert_eq!(out.item_state, ReviewState::Applied);
    assert!(!out.applied_now);
    assert_eq!(
        out.verdicts,
        Some(vec![
            ("f1".to_string(), ReviewVerdict::Pass),
            ("f2".to_string(), ReviewVerdict::Reject),
        ]),
        "混合批逐条判决照旧重建"
    );
    assert_eq!(out.written, 0, "f1 被去重命中；f2 是 reject，不计入引擎的写入读数");
    assert_eq!(out.duplicate, 1);
    assert_eq!(out.duplicate_uids.iter().collect::<Vec<_>>(), vec![&uids[0]]);
    assert_eq!((out.facts, out.cards, out.mentions), (0, 0, 0));
    assert_eq!(count(root.memory(), "fact"), 1, "行数不变");
    let leaked: i64 = root
        .memory()
        .conn()
        .query_row(
            "SELECT count(*) FROM fact WHERE COALESCE(value_text,'')=?1",
            [REJECTED_FACT_VALUE],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leaked, 0, "被 reject 的草稿不许借重放写进记忆表");
}

/// 8. 载荷缺新键（载荷与指纹**自洽**）：replay 判 gap ⇒ 记 `uncertain(review_payload_gap)`、
///    0 行、不调模型。改写走 [`rewrite_payload`]（同步重算列指纹），否则先被 H4a 的
///    防篡改闸拦下，测不到 replay 的 gap 分支。
#[test]
fn payload_gap_quarantines_without_calling_the_model() {
    let (_dir, root, _uids) = seeded_fact();
    let (id, _) = pending_fact(&root);
    rewrite_payload(root.memory(), id, |payload| {
        payload["items"][0].as_object_mut().unwrap().remove("turn_uid");
    });

    let engine = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(engine.calls.get(), 0, "gap 不调模型");
    assert_eq!(out.item_state, ReviewState::Quarantined);
    assert_eq!(out.verdicts, None);
    assert_eq!(out.fail_closed_reason, Some(PAYLOAD_GAP));
    assert_eq!(out.written, 0);
    assert_eq!(count(root.memory(), "fact"), 0);

    let rows = decisions(root.memory(), id);
    assert_eq!(rows.len(), 2, "program pass + review uncertain");
    assert_eq!(
        (rows[1].stage.as_str(), rows[1].verdict.as_str(), rows[1].reason_code.as_str()),
        ("review", "uncertain", PAYLOAD_GAP)
    );
    assert_eq!((rows[1].decider.as_str(), rows[1].model.as_str()), ("program", ""));
    assert_eq!(rows[1].prompt_version, "");
    assert_eq!(rows[1].input_sha256.len(), 64);
    assert_eq!(
        rows[1].review_input_sha256, "",
        "gap 是程序裁定、没有送审：送审指纹为空"
    );
}

// ── H5a：超长审核输入 ─────────────────────────────────────────────────────────

/// 长正文批的抽取回复：t1 一条 promise（subject a1=张三），批里只有这一轮。
fn large_fact_reply() -> String {
    json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a1", "predicate": "promise",
            "object": null, "value": "把对账单发给你", "category": "promise",
            "mode": "stated", "time_text": "下周三之前", "quote": PROMISE,
            "evidence": ["t1"], "text": "张三承诺在下周三之前把对账单发给你。"
        }],
        "empty_turns": []
    })
    .to_string()
}

/// H5a-1. 目标轮正文 ≥ 300 KiB ⇒ 审核输入 `system + user` 超过
/// [`REVIEW_INPUT_MAX_BYTES`]：不调模型、不写库、`program/uncertain(review_input_too_large)`、
/// item → `quarantined`。
///
/// 造法走真路径：`EnqueueOnly` 只抽取（`--no-review`），长正文从导入起就在目标轮里；
/// `review run` 在调模型**之前**按字节数拦下。截断再送模型只会走到解析失败与无限退避。
#[test]
fn oversized_review_input_is_quarantined_without_calling_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    // 3 字节/汉字 ⇒ 120k 字 = 360 KB；前缀给引文留逐字出现的位置。
    let body = format!("张三说：{PROMISE}{}", "补".repeat(120_000));
    assert!(
        body.len() >= 300 * 1024,
        "夹具正文必须 ≥ 300 KiB，实际 {} 字节",
        body.len()
    );
    let uids = seed_bodies(&mut root, &[body.as_str()], 1_000);
    set_owner(&mut root);
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    let subject = add_anchor(&root, &scope, "person", "张三", &uids[0], NOW - 1_000);

    let plan = select_fact_batch(
        root.transcript(),
        root.ledger(),
        root.memory(),
        0,
        FactFamily::Promise,
        NOW,
    )
    .unwrap()
    .expect("这一轮还没跑 fact/promise");
    let extractor = ScriptBackend::new(vec![Ok(large_fact_reply())]);
    let queued = run_fact_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &extractor,
        NOW,
        ReviewMode::EnqueueOnly,
    )
    .unwrap();
    assert_eq!(extractor.calls.get(), 1, "EnqueueOnly 只问抽取");
    assert_eq!(queued.state, PassState::Pending);
    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "pending_review", "长正文的批照样能入队");
    let payload_sha: String = root
        .memory()
        .conn()
        .query_row("SELECT payload_sha256 FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap();

    let engine = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(engine.calls.get(), 0, "超长输入不许送模型");
    assert_eq!(out.item_state, ReviewState::Quarantined);
    assert_eq!(out.verdicts, None, "隔离不是判决，更不是全 pass");
    assert_eq!(out.fail_closed_reason, Some(REVIEW_INPUT_TOO_LARGE));
    assert_eq!((out.written, out.facts, out.cards, out.mentions), (0, 0, 0, 0));
    assert_eq!(count(root.memory(), "fact"), 0, "隔离批一行都不写");
    assert_eq!(item_state(root.memory(), id), "quarantined", "队列必须看得见这条");

    let rows = decisions(root.memory(), id);
    assert_eq!(rows.len(), 2, "program pass + 程序侧的超长裁定");
    let last = &rows[1];
    assert_eq!(
        (last.stage.as_str(), last.verdict.as_str(), last.reason_code.as_str()),
        ("program", "uncertain", REVIEW_INPUT_TOO_LARGE)
    );
    assert_eq!(last.detail, REVIEW_INPUT_TOO_LARGE);
    assert_eq!(
        (last.decider.as_str(), last.model.as_str(), last.prompt_version.as_str()),
        ("program", "", ""),
        "程序侧裁定：没有模型/prompt 参与"
    );
    assert_eq!(last.input_sha256, payload_sha, "记的是入队时的载荷指纹");
    assert_eq!(last.review_input_sha256, "", "没有送审：送审指纹为空");
    let program_code: Option<String> = root
        .memory()
        .conn()
        .query_row("SELECT program_code FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap();
    assert_eq!(program_code.as_deref(), Some(REVIEW_INPUT_TOO_LARGE), "隔离原因码留在 item 上");

    // 实际触发尺寸：用 runner 同一套公开渲染函数重算（不读引擎内部），确认真的越过上限。
    let stored_body: String = root
        .transcript()
        .conn()
        .query_row("SELECT body FROM turns WHERE turn_uid=?1", [uids[0].as_str()], |r| r.get(0))
        .unwrap();
    let draft = FactDraft {
        reference: "f1".into(),
        turn_uid: uids[0].clone(),
        subject_anchor_id: subject.clone(),
        object_anchor_id: None,
        subject_canonical: "张三".into(),
        object_canonical: None,
        predicate: "promise".into(),
        value: Some("把对账单发给你".into()),
        category: "promise".into(),
        mode: "stated".into(),
        time_text: "下周三之前".into(),
        quote: PROMISE.into(),
        evidence_turn_uids: vec![uids[0].clone()],
        text: "张三承诺在下周三之前把对账单发给你。".into(),
    };
    let batch = Batch {
        session_key: "sess-batch-A".into(),
        device_id: "dev-batch-A".into(),
        source_agent: "agent-batch-A".into(),
        project_hint: None,
        rows: vec![TurnRow {
            turn_uid: uids[0].clone(),
            role: "human".into(),
            body: stored_body,
            created_at_ms: 1_000,
            ingested_at_ms: 1_001_000,
            speaker: None,
            speaker_anchor_id: None,
        }],
    };
    let system = fact_review_system(FactFamily::Promise);
    let ctx = ReviewInputContext {
        session_key: batch.session_key.clone(),
        owner_agent_id: "alice".into(),
        visibility: "project".into(),
        scope: "project:proj-x".into(),
        speakers: vec!["小明".into()],
    };
    let user = fact_review_user(FactFamily::Promise, &[draft], &batch, &ctx);
    let input_bytes = system.len() + user.len();
    assert!(
        input_bytes > REVIEW_INPUT_MAX_BYTES,
        "审核输入必须真的越过 {REVIEW_INPUT_MAX_BYTES} 字节，实际 {input_bytes} 字节"
    );
}

// ── event ─────────────────────────────────────────────────────────────────────

/// 9. event happy path：extras 的 participants/place/value_json 落到事件行。
#[test]
fn event_happy_path_writes_extras_columns() {
    let (_dir, root, _uids, zhang, place) = seeded_event();
    let id = pending_event(&root);
    let engine = ScriptBackend::engine(event_review(&[("e1", "pass", None)]));

    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);
    assert!(out.applied_now);
    assert_eq!(out.written, 1);
    assert_eq!(out.verdicts, Some(vec![("e1".to_string(), ReviewVerdict::Pass)]));
    assert_eq!(count(root.memory(), "fact"), 1);
    let rows = decisions(root.memory(), id);
    assert_eq!(rows[1].prompt_version, PASS_EVENT_REVIEW_VERSION);

    let (participants_json, value_json, value_text, place_anchor_id): (
        String,
        String,
        Option<String>,
        Option<String>,
    ) = root
        .memory()
        .conn()
        .query_row(
            "SELECT participants_json, value_json, value_text, place_anchor_id FROM fact",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    let participants: Value = serde_json::from_str(&participants_json).unwrap();
    assert_eq!(participants, json!([zhang]), "subject/participants 去重后只留 actor 一格");
    assert_eq!(value_text.as_deref(), Some("配置文件"));
    let value: Value = serde_json::from_str(&value_json).unwrap();
    assert_eq!(value["action"], "restore_config");
    assert_eq!(value["result"], "done");
    assert_eq!(value["object_text"], "配置文件");
    assert_eq!(place_anchor_id.as_deref(), Some(place.as_str()), "place 锚落到事件行");
}

/// 16. Applied 重放的 event 读数（C5b）：与 fact 同口径——去重循环照跑，库里已有同
///     `(轮, actor, action, object)` 的行 ⇒ 计 `duplicate`、没有行交给写库层；
///     插入读数恒 0、`applied_now=false`、行数不变。
#[test]
fn applied_event_replay_counts_pass_drafts_without_writing() {
    let (_dir, root, _uids, _zhang, _place) = seeded_event();
    let id = pending_event(&root);
    let first = ScriptBackend::engine(event_review(&[("e1", "pass", None)]));
    let applied =
        run_item(root.memory(), root.transcript(), root.ledger(), id, &first, NOW).unwrap();
    assert_eq!((applied.written, applied.facts), (1, 1));
    let before = [
        count(root.memory(), "fact"),
        count(root.memory(), "card"),
        count(root.memory(), "memory_mention"),
    ];

    let again = ScriptBackend::engine(event_review(&[("e1", "pass", None)]));
    let out = run_item(
        root.memory(),
        root.transcript(),
        root.ledger(),
        id,
        &again,
        NOW + 5_000,
    )
    .unwrap();
    assert_eq!(again.calls.get(), 0, "applied 不再调模型");
    assert_eq!(out.item_state, ReviewState::Applied);
    assert!(!out.applied_now);
    assert_eq!(out.written, 0, "库里已有这一条 ⇒ 去重后没有行可交");
    assert_eq!(out.duplicate, 1, "命中的那一条按 fact 同口径计数");
    assert_eq!((out.facts, out.cards, out.mentions), (0, 0, 0), "重放不新增插入行");
    assert_eq!(out.verdicts, Some(vec![("e1".to_string(), ReviewVerdict::Pass)]));
    let after = [
        count(root.memory(), "fact"),
        count(root.memory(), "card"),
        count(root.memory(), "memory_mention"),
    ];
    assert_eq!(before, after, "行数不变");
}

// ── 未支持 ────────────────────────────────────────────────────────────────────

/// 10. `pass_id=time` ⇒ Unsupported（不读载荷、不调模型）。naming 自 S3b-3b、rows 自
///     S3c-3b 起受支持，所以 `Unsupported` 的钉子换到 time——这台引擎没有 time 的
///     重放与判定实现（`supported_pass` 的分派表里没有它）。
#[test]
fn time_pass_is_unsupported() {
    let (_dir, root, _uids) = seeded_fact();
    let queued = enqueue(
        root.memory(),
        &NewItem {
            source: "rows".into(),
            pass_id: "time".into(),
            family: "".into(),
            session_key: "sess-batch-A".into(),
            device_id: "dev-batch-A".into(),
            turn_uids_json: "[]".into(),
            owner_json: "{}".into(),
            payload_json: json!({
                "schema": "review.pass.v1",
                "pass_id": "time",
                "family": "",
                "run_id": "time-manual",
                "batch": {},
                "owner": {},
                "items": []
            })
            .to_string(),
            batch_key: String::new(),
        },
        NOW,
    )
    .unwrap();

    let engine = ScriptBackend::engine("{}".to_string());
    let err = run_item(
        root.memory(),
        root.transcript(),
        root.ledger(),
        queued.id,
        &engine,
        NOW,
    )
    .unwrap_err();
    assert!(matches!(err, RunError::Unsupported), "{err:?}");
    assert_eq!(engine.calls.get(), 0);
    assert_eq!(item_state(root.memory(), queued.id), "pending_review");
    assert_eq!(count(root.memory(), "fact"), 0);
}

// ── approved ──────────────────────────────────────────────────────────────────

/// 11. approved 重试按 review 判定的 detail 重建：mixed 里 pass 的 f1 已写过（dedup），
///     reject 的 f2 不借重试写进记忆表；item 重新 applied。
#[test]
fn approved_retry_uses_the_persisted_verdicts() {
    let (_dir, root, uids) = seeded_fact();
    let id = pending_fact_mixed(&root);
    let engine = ScriptBackend::engine(fact_review(&[
        ("f1", "pass", None),
        ("f2", "reject", Some("not_durable")),
    ]));
    run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(item_state(root.memory(), id), "applied");
    assert_eq!(count(root.memory(), "fact"), 1);

    force_approved(root.memory(), id);
    let again = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &again, NOW + 5_000)
        .unwrap();
    assert_eq!(again.calls.get(), 0, "approved 不调模型");
    assert_eq!(out.item_state, ReviewState::Applied);
    assert!(out.applied_now);
    assert_eq!(out.written, 0, "f1 已写过（dedup）、f2 是 reject");
    assert_eq!(out.duplicate, 1);
    assert_eq!(out.duplicate_uids.iter().collect::<Vec<_>>(), vec![&uids[0]]);
    assert_eq!(
        out.verdicts,
        Some(vec![
            ("f1".to_string(), ReviewVerdict::Pass),
            ("f2".to_string(), ReviewVerdict::Reject),
        ])
    );
    assert_eq!(count(root.memory(), "fact"), 1);
    let leaked: i64 = root
        .memory()
        .conn()
        .query_row(
            "SELECT count(*) FROM fact WHERE COALESCE(value_text,'')=?1",
            [REJECTED_FACT_VALUE],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leaked, 0, "曾被 reject 的草稿不许借 approved 重试写进记忆表");
}

/// 12. 最后一条判定是 human pass ⇒ 全部草稿 pass：mixed 里 reject 的 f2 也被人工放行。
#[test]
fn human_pass_makes_every_draft_pass() {
    let (_dir, root, _uids) = seeded_fact();
    let id = pending_fact_mixed(&root);
    let engine = ScriptBackend::engine(fact_review(&[
        ("f1", "pass", None),
        ("f2", "reject", Some("not_durable")),
    ]));
    run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(count(root.memory(), "fact"), 1);

    force_approved(root.memory(), id);
    human_pass(&root, id);
    let again = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &again, NOW + 5_000)
        .unwrap();
    assert_eq!(again.calls.get(), 0);
    assert_eq!(out.item_state, ReviewState::Applied);
    assert_eq!(out.written, 1, "f2 被人工放行后写入；f1 已存在按 dedup 跳过");
    assert_eq!(out.duplicate, 1);
    assert_eq!(
        out.verdicts,
        Some(vec![
            ("f1".to_string(), ReviewVerdict::Pass),
            ("f2".to_string(), ReviewVerdict::Pass),
        ])
    );
    assert_eq!(count(root.memory(), "fact"), 2);
    let released: i64 = root
        .memory()
        .conn()
        .query_row(
            "SELECT count(*) FROM fact WHERE COALESCE(value_text,'')=?1",
            [REJECTED_FACT_VALUE],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(released, 1, "人工放行后这一条才允许入库");
}

/// H2（P0-2）：approved + 载荷缺口不许静默返回 `Ok(Approved)`。
/// 记一条 `program/uncertain/PAYLOAD_GAP` 把 item 推到 `quarantined`：`verdicts=None`、
/// 隔离码是 gap、0 行、不调模型。
#[test]
fn approved_payload_gap_quarantines_with_a_program_decision() {
    let (_dir, root, _uids) = seeded_fact();
    let (id, _) = pending_fact(&root);
    human_pass(&root, id);
    assert_eq!(item_state(root.memory(), id), "approved");

    // 删一个重放必需键（重新落章，测的是 replay 的 gap，不是 H4a 的篡改闸）。
    rewrite_payload(root.memory(), id, |payload| {
        payload["items"][0].as_object_mut().unwrap().remove("turn_uid");
    });

    let engine = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(engine.calls.get(), 0, "approved/gap 都不调模型");
    assert_eq!(out.item_state, ReviewState::Quarantined);
    assert_eq!(out.verdicts, None, "缺口不是判决，更不是全 pass");
    assert_eq!(out.fail_closed_reason, Some(PAYLOAD_GAP));
    assert_eq!(out.written, 0);
    assert_eq!(count(root.memory(), "fact"), 0, "缺口不写记忆行");

    assert_eq!(item_state(root.memory(), id), "quarantined", "队列必须看得见这条");
    let rows = decisions(root.memory(), id);
    let last = rows.last().expect("program/uncertain 判定必须落库");
    assert_eq!(
        (last.stage.as_str(), last.verdict.as_str(), last.reason_code.as_str()),
        ("program", "uncertain", PAYLOAD_GAP)
    );
    assert_eq!(last.detail, "approved_gap");
    assert_eq!(
        (last.decider.as_str(), last.model.as_str(), last.prompt_version.as_str()),
        ("program", "", ""),
        "程序侧裁定：没有模型/prompt 参与"
    );
    let payload_sha: String = root
        .memory()
        .conn()
        .query_row("SELECT payload_sha256 FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap();
    assert_eq!(last.input_sha256, payload_sha, "input_sha256 是载荷指纹");
    let rules_version: String = root
        .memory()
        .conn()
        .query_row(
            "SELECT rules_version FROM review_decision WHERE item_id=?1 ORDER BY id DESC LIMIT 1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rules_version, GATE_RULES_VERSION, "规则版本 = gate 版本");
    let program_code: Option<String> = root
        .memory()
        .conn()
        .query_row("SELECT program_code FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap();
    assert_eq!(
        program_code.as_deref(),
        Some(PAYLOAD_GAP),
        "隔离的原因码留在 item 上"
    );
}

/// H2：approved + 判决重建不出（`REVIEW_VERDICTS_MISSING`）走同一条去向——
/// 不再 `Ok(Approved)`，而是 `program/uncertain` ⇒ `quarantined`。
#[test]
fn approved_verdicts_missing_quarantines_with_a_program_decision() {
    let (_dir, root, _uids) = seeded_fact();
    let (id, _) = pending_fact(&root);
    let first = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    run_item(root.memory(), root.transcript(), root.ledger(), id, &first, NOW).unwrap();
    assert_eq!(item_state(root.memory(), id), "applied");
    assert_eq!(count(root.memory(), "fact"), 1);

    force_approved(root.memory(), id);
    // 弄坏 review pass 判定的 detail：逐条判决重建不出。
    root.memory()
        .conn()
        .execute(
            "UPDATE review_decision SET detail='corrupt' WHERE item_id=?1 AND stage='review'",
            [id],
        )
        .unwrap();

    let again = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    let out =
        run_item(root.memory(), root.transcript(), root.ledger(), id, &again, NOW + 5_000).unwrap();
    assert_eq!(again.calls.get(), 0);
    assert_eq!(out.item_state, ReviewState::Quarantined);
    assert_eq!(out.verdicts, None);
    assert_eq!(out.fail_closed_reason, Some(REVIEW_VERDICTS_MISSING));
    assert_eq!(out.written, 0);
    assert_eq!(count(root.memory(), "fact"), 1, "隔离既不写也不删");
    assert_eq!(item_state(root.memory(), id), "quarantined");

    let rows = decisions(root.memory(), id);
    let last = rows.last().expect("program/uncertain 判定必须落库");
    assert_eq!(
        (last.stage.as_str(), last.verdict.as_str(), last.reason_code.as_str()),
        ("program", "uncertain", REVIEW_VERDICTS_MISSING)
    );
    assert_eq!(last.detail, "approved_verdicts_missing");
    assert_eq!(last.decider, "program");
    assert_eq!(last.model, "");
    assert_eq!(last.prompt_version, "");
}

/// 引擎在 applied 上重建不出判决时走 fail-closed（不是 "全 pass"）：拿掉 review pass
/// 判定的 detail 后，verdicts=None 且隔离码是 `review_verdicts_missing`。
#[test]
fn applied_item_with_unreadable_verdicts_fails_closed() {
    let (_dir, root, _uids) = seeded_fact();
    let (id, _) = pending_fact(&root);
    let engine = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    root.memory()
        .conn()
        .execute(
            "UPDATE review_decision SET detail='corrupt' WHERE item_id=?1 AND stage='review'",
            [id],
        )
        .unwrap();

    let again = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &again, NOW + 5_000)
        .unwrap();
    assert_eq!(again.calls.get(), 0);
    assert_eq!(out.item_state, ReviewState::Applied);
    assert_eq!(out.verdicts, None);
    assert_eq!(out.fail_closed_reason, Some(REVIEW_VERDICTS_MISSING));
    assert_eq!(out.written, 0);
}

// ── H4a：载荷防篡改 ──────────────────────────────────────────────────────────

/// H4a-1. pending + 载荷被改（列指纹没变）⇒ 不重放、不问模型、不写库：
/// `program/uncertain(PAYLOAD_GAP, detail=payload_tampered)`、item → quarantined。
#[test]
fn tampered_pending_payload_is_quarantined_without_calling_the_model() {
    let (_dir, root, _uids) = seeded_fact();
    let (id, _) = pending_fact(&root);
    let payload_sha: String = root
        .memory()
        .conn()
        .query_row("SELECT payload_sha256 FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap();
    // 改一个**形状合法**的字段：拦下它的必须是指纹，不是 replay 的形状校验。
    tamper_payload(root.memory(), id, |payload| {
        payload["items"][0]["quote"] = json!("改过的引文");
    });

    let engine = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(engine.calls.get(), 0, "篡改的载荷不许送去审核");
    assert_eq!(out.item_state, ReviewState::Quarantined);
    assert_eq!(out.verdicts, None, "缺口不是判决，更不是全 pass");
    assert_eq!(out.fail_closed_reason, Some(PAYLOAD_GAP));
    assert_eq!((out.written, out.facts, out.cards, out.mentions), (0, 0, 0, 0));
    assert_eq!(count(root.memory(), "fact"), 0, "篡改的载荷不许写库");
    assert_eq!(item_state(root.memory(), id), "quarantined", "队列必须看得见这条");

    let rows = decisions(root.memory(), id);
    assert_eq!(rows.len(), 2, "program pass + 程序侧的篡改裁定");
    let last = &rows[1];
    assert_eq!(
        (last.stage.as_str(), last.verdict.as_str(), last.reason_code.as_str()),
        ("program", "uncertain", PAYLOAD_GAP)
    );
    assert_eq!(last.detail, "payload_tampered");
    assert_eq!(
        (last.decider.as_str(), last.model.as_str(), last.prompt_version.as_str()),
        ("program", "", ""),
        "程序侧裁定：没有模型/prompt 参与"
    );
    assert_eq!(last.input_sha256, payload_sha, "记的是入队时的载荷指纹");
    let program_code: Option<String> = root
        .memory()
        .conn()
        .query_row("SELECT program_code FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap();
    assert_eq!(program_code.as_deref(), Some(PAYLOAD_GAP), "隔离原因码留在 item 上");
}

/// H4a-2. approved + 载荷被改 ⇒ 走 H2 同一条 `program/uncertain` 隔离，退出可见。
#[test]
fn tampered_approved_payload_is_quarantined_and_visible() {
    let (_dir, root, _uids) = seeded_fact();
    let (id, _) = pending_fact(&root);
    human_pass(&root, id);
    assert_eq!(item_state(root.memory(), id), "approved");
    tamper_payload(root.memory(), id, |payload| {
        payload["items"][0]["text"] = json!("改过的结论");
    });

    let engine = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(engine.calls.get(), 0, "approved/篡改都不调模型");
    assert_eq!(out.item_state, ReviewState::Quarantined);
    assert_eq!(out.verdicts, None);
    assert_eq!(out.fail_closed_reason, Some(PAYLOAD_GAP));
    assert_eq!(out.written, 0);
    assert_eq!(count(root.memory(), "fact"), 0);
    assert_eq!(item_state(root.memory(), id), "quarantined", "队列必须看得见这条");

    let rows = decisions(root.memory(), id);
    assert_eq!(
        rows.iter().map(|r| r.stage.as_str()).collect::<Vec<_>>(),
        vec!["program", "human", "program"]
    );
    let last = rows.last().expect("program/uncertain 判定必须落库");
    assert_eq!(
        (last.stage.as_str(), last.verdict.as_str(), last.reason_code.as_str()),
        ("program", "uncertain", PAYLOAD_GAP)
    );
    assert_eq!(last.detail, "payload_tampered");
    assert_eq!(last.decider, "program");
}

/// H4a-3. applied + 载荷被改 ⇒ 只 fail-closed：`item_state=Applied`、ledger 不动、
/// 不写判定、不写库（ledger 此刻已是 Done，改它会把"已应用"改写成隔离）。
#[test]
fn tampered_applied_payload_fails_closed_without_touching_the_ledger() {
    let (_dir, root, _uids) = seeded_fact();
    let (id, _) = pending_fact(&root);
    let first = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    run_item(root.memory(), root.transcript(), root.ledger(), id, &first, NOW).unwrap();
    assert_eq!(item_state(root.memory(), id), "applied");
    let facts_before = count(root.memory(), "fact");
    let decisions_before = decisions(root.memory(), id).len();

    tamper_payload(root.memory(), id, |payload| {
        payload["items"][0]["quote"] = json!("改过的引文");
    });
    let again = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &again, NOW + 5_000)
        .unwrap();
    assert_eq!(again.calls.get(), 0, "篡改的载荷不许送去审核");
    assert_eq!(out.item_state, ReviewState::Applied, "applied 是终态，不许被改写");
    assert!(!out.applied_now);
    assert_eq!(out.verdicts, None);
    assert_eq!(out.fail_closed_reason, Some(PAYLOAD_GAP));
    assert_eq!((out.written, out.facts, out.cards, out.mentions), (0, 0, 0, 0));
    assert_eq!(count(root.memory(), "fact"), facts_before, "不写库");
    assert_eq!(decisions(root.memory(), id).len(), decisions_before, "不写判定");
    assert_eq!(item_state(root.memory(), id), "applied", "item 状态不动");
}

// ── H5b：审核批 ≤10 分片 ─────────────────────────────────────────────────────
//
// 12 条草稿 ⇒ 两次审核调用（10 + 2）。片内 ITEM 编号从 1 起，解析结果按
// `chunk_offset + local_index` 映射回全局 ref；判定仍只有一条（整 item 的总判决）。

/// 12 轮、每轮一句「张三说：对账单第N条已发出」：一轮一条 promise 草稿。
const CHUNK_DRAFTS: usize = 12;

fn chunk_body(index: usize) -> String {
    format!("张三说：对账单第{}条已发出", index + 1)
}

/// 12 条草稿的抽取回复：`quotes[i]` 是第 i 轮要抄的引文。
fn fact_chunk_reply(quotes: &[String]) -> String {
    let facts: Vec<Value> = quotes
        .iter()
        .enumerate()
        .map(|(index, quote)| {
            json!({
                "ref": format!("f{}", index + 1),
                "turn": format!("t{}", index + 1),
                "subject": "a1",
                "predicate": "promise",
                "object": null,
                "value": format!("对账单第{}条", index + 1),
                "category": "promise",
                "mode": "stated",
                "time_text": null,
                "quote": quote,
                "evidence": [format!("t{}", index + 1)],
                "text": "张三承诺发出对账单。"
            })
        })
        .collect();
    json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": facts, "empty_turns": []
    })
    .to_string()
}

fn chunk_quotes() -> Vec<String> {
    (0..CHUNK_DRAFTS).map(chunk_body).collect()
}

fn fact_twelve_reply() -> String {
    fact_chunk_reply(&chunk_quotes())
}

/// 12 轮夹具：张三 a1 一条活锚，每轮一条草稿的抽取回复能过契约。
fn seeded_fact_twelve() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let bodies = chunk_quotes();
    let borrowed: Vec<&str> = bodies.iter().map(String::as_str).collect();
    let uids = seed_bodies(&mut root, &borrowed, 1_000);
    set_owner(&mut root);
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    add_anchor(&root, &scope, "person", "张三", &uids[0], NOW - 1_000);
    (dir, root, uids)
}

/// 造一条停在 `pending_review` 的 12 草稿 fact item（第 3 次调用=窄审失败）。
fn pending_fact_twelve(root: &Root) -> i64 {
    let plan = fact_plan(root);
    let backend =
        ScriptBackend::new(vec![Ok(fact_twelve_reply()), Err(LlmFailure::BackendError)]);
    let err = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW)
        .unwrap_err();
    assert!(matches!(err, DistillError::Llm(LlmFailure::BackendError)), "{err}");
    assert_eq!(count(root.memory(), "fact"), 0);
    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "pending_review");
    id
}

/// 片内 10 条全 pass（第一片）。
const PASS_TEN: [(&str, &str, Option<&str>); 10] = [
    ("f1", "pass", None),
    ("f2", "pass", None),
    ("f3", "pass", None),
    ("f4", "pass", None),
    ("f5", "pass", None),
    ("f6", "pass", None),
    ("f7", "pass", None),
    ("f8", "pass", None),
    ("f9", "pass", None),
    ("f10", "pass", None),
];

/// 解析失败时 artifact 的原文（按落库顺序）。
fn review_artifact_raws(root: &Root) -> Vec<String> {
    let mut st = root
        .ledger()
        .conn()
        .prepare("SELECT raw FROM extract_pass_artifact WHERE stage='review' ORDER BY id")
        .unwrap();
    st.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// H5b-1. 12 条草稿全 pass：两次调用（10 + 2）、判定只有一条、`verdicts` 12 条且
/// 第 11/12 条映射回全局 f11/f12、12 行写库、item applied；指纹是分片合成指纹。
#[test]
fn twelve_drafts_are_reviewed_in_two_chunks_and_apply_as_one_decision() {
    let (_dir, root, _uids) = seeded_fact_twelve();
    let id = pending_fact_twelve(&root);
    let engine = ScriptBackend::new(vec![
        Ok(fact_review(&PASS_TEN)),
        Ok(fact_review(&PASS_TEN)),
        Ok(fact_review(&[("f1", "pass", None), ("f2", "pass", None)])),
        Ok(fact_review(&[("f1", "pass", None), ("f2", "pass", None)])),
    ]);

    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(engine.calls.get(), 4, "12 条草稿 ⇒ 两片，每片 proposer + challenger");
    assert_eq!(out.item_state, ReviewState::Applied);
    assert!(out.applied_now);
    assert_eq!(out.written, 12);
    assert_eq!(count(root.memory(), "fact"), 12);
    let verdicts = out.verdicts.as_ref().expect("聚合判决");
    assert_eq!(verdicts.len(), 12, "逐条判决仍是 12 条（全局 ref）");
    assert_eq!(verdicts[9], ("f10".to_string(), ReviewVerdict::Pass));
    assert_eq!(
        verdicts[10],
        ("f11".to_string(), ReviewVerdict::Pass),
        "第二片的 f1 ⇒ 全局 f11"
    );
    assert_eq!(
        verdicts[11],
        ("f12".to_string(), ReviewVerdict::Pass),
        "第二片的 f2 ⇒ 全局 f12"
    );

    let rows = decisions(root.memory(), id);
    assert_eq!(
        rows.iter().map(|r| r.stage.as_str()).collect::<Vec<_>>(),
        vec!["program", "review", "apply"],
        "多个分片仍只有一条 review 判定"
    );
    assert_eq!(rows[1].reason_code, "review_pass");
    assert_eq!(
        rows[1].detail,
        "p=12;r=0;u=0;pass=f1,f2,f3,f4,f5,f6,f7,f8,f9,f10,f11,f12;uncertain=-"
    );

    // 片内编号：每片两次调用（proposer 在前、challenger 在后），各片都从 ITEM f1 起。
    let prompts = engine.prompts.borrow();
    assert_eq!(prompts.len(), 4, "两片 × 两档");
    assert!(prompts[0].user.contains("ITEM f10\n"));
    assert!(!prompts[0].user.contains("ITEM f11"), "第一片只到 f10");
    assert_eq!(prompts[1].user, prompts[0].user, "同一片的两档读同一份 user");
    assert!(
        prompts[2].user.starts_with("PASS fact_review v0 | family promise\n"),
        "第二片仍以同一行首开始：{}",
        prompts[2].user
    );
    assert!(prompts[2].user.contains("ITEM f1\n"), "第二片从 1 重新编号");
    assert!(!prompts[2].user.contains("ITEM f3\n"), "第二片只有 f1/f2");
    assert_eq!(prompts[3].user, prompts[2].user, "同一片的两档读同一份 user");

    // H5b：送审输入指纹是分片合成指纹（各片 user 按顺序 join），不单指某一次调用；
    // §7.2 起 challenger system 也进第四格（两档共用同一片 user，user 只进一次）。
    let users = [prompts[0].user.clone(), prompts[2].user.clone()];
    let synthetic = water::extraction_fingerprint(
        &format!("{FACT_PIPELINE_PREFIX}/promise"),
        MODEL,
        &prompts[0].system,
        &format!(
            "{}\n\u{1e}\n{}",
            fact_review_challenge_system(FactFamily::Promise),
            users.join("\n\u{1e}\n")
        ),
    );
    assert_eq!(rows[1].review_input_sha256, synthetic);
    let single = water::extraction_fingerprint(
        &format!("{FACT_PIPELINE_PREFIX}/promise"),
        MODEL,
        &prompts[0].system,
        &prompts[0].user,
    );
    assert_ne!(rows[1].review_input_sha256, single, "多片时不是单片指纹");
}

/// H5b-2. 跨片 mixed：第一片全 pass、第二片 f1(reject)/f2(uncertain) ⇒ 只写 10 行、
/// `verdicts` 12 条、detail 里 pass/uncertain 是**全局** ref。
#[test]
fn mixed_verdicts_across_chunks_keep_global_refs_in_the_detail() {
    let (_dir, root, _uids) = seeded_fact_twelve();
    let id = pending_fact_twelve(&root);
    let engine = ScriptBackend::new(vec![
        Ok(fact_review(&PASS_TEN)),
        Ok(fact_review(&PASS_TEN)),
        Ok(fact_review(&[
            ("f1", "reject", Some("not_durable")),
            ("f2", "uncertain", Some("quote_mismatch")),
        ])),
        Ok(fact_review(&[
            ("f1", "reject", Some("not_durable")),
            ("f2", "uncertain", Some("quote_mismatch")),
        ])),
    ]);

    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(engine.calls.get(), 4);
    assert_eq!(out.item_state, ReviewState::Applied, "p>0 ⇒ 尽力写 pass 草稿");
    assert!(out.applied_now);
    assert_eq!(out.written, 10, "第二片没有 pass 草稿：只写第一片的 10 行");
    assert_eq!(count(root.memory(), "fact"), 10);
    let verdicts = out.verdicts.as_ref().expect("聚合判决");
    assert_eq!(verdicts.len(), 12);
    assert_eq!(verdicts[10], ("f11".to_string(), ReviewVerdict::Reject));
    assert_eq!(verdicts[11], ("f12".to_string(), ReviewVerdict::Uncertain));

    let rows = decisions(root.memory(), id);
    assert_eq!(rows[1].reason_code, "review_mixed");
    assert_eq!(
        rows[1].detail,
        "p=10;r=1;u=1;pass=f1,f2,f3,f4,f5,f6,f7,f8,f9,f10;uncertain=f12",
        "detail 里的 ref 必须是全局编号（第二片的 f2 是 f12）"
    );
}

/// H5b-3. 第二片解析失败：第一片成功也不算数——整 item quarantine、0 行，
/// artifact 原文是**第二片**的 raw、判定 reason 是解析 slug。
#[test]
fn parse_failure_in_the_second_chunk_quarantines_the_whole_item() {
    let (_dir, root, _uids) = seeded_fact_twelve();
    let id = pending_fact_twelve(&root);
    let engine = ScriptBackend::new(vec![
        Ok(fact_review(&PASS_TEN)),
        Ok(fact_review(&PASS_TEN)),
        Ok("{}".to_string()),
        Ok("{}".to_string()),
    ]);

    let err = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW)
        .unwrap_err();
    assert!(
        matches!(err, RunError::Review(DistillError::Fact(FactPassError::Json))),
        "{err:?}"
    );
    assert_eq!(
        engine.calls.get(),
        4,
        "第一片两档成功、第二片两档坏 JSON：四次调用都发生过"
    );
    assert_eq!(item_state(root.memory(), id), "quarantined", "整 item 隔离，不写半份");
    assert_eq!(count(root.memory(), "fact"), 0, "第一片的 10 条也不许写");

    let raws = review_artifact_raws(&root);
    assert_eq!(
        raws,
        vec!["{}".to_string(), "{}".to_string()],
        "失败那一片的 proposer 与 challenger 各留一份原文"
    );
    let rows = decisions(root.memory(), id);
    assert_eq!(
        rows.iter().map(|r| r.stage.as_str()).collect::<Vec<_>>(),
        vec!["program", "review"],
        "失败只记一条 uncertain，没有 apply"
    );
    assert_eq!(
        (rows[1].verdict.as_str(), rows[1].reason_code.as_str()),
        ("uncertain", "review_json")
    );

    // 失败那次的送审指纹覆盖已送出的两片（第一片成功 + 第二片失败）。
    let prompts = engine.prompts.borrow();
    let users = [prompts[0].user.clone(), prompts[2].user.clone()];
    let expected = water::extraction_fingerprint(
        &format!("{FACT_PIPELINE_PREFIX}/promise"),
        MODEL,
        &prompts[0].system,
        &format!(
            "{}\n\u{1e}\n{}",
            fact_review_challenge_system(FactFamily::Promise),
            users.join("\n\u{1e}\n")
        ),
    );
    assert_eq!(rows[1].review_input_sha256, expected);
}

/// H5b-4. 第二片超长：12 条草稿里第 11 轮正文 ≥ 300 KiB ⇒ 第二片 user 越过
/// [`REVIEW_INPUT_MAX_BYTES`]：**一片都不送**、0 次调用、`review_input_too_large` 隔离。
#[test]
fn oversized_second_chunk_quarantines_without_any_model_call() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let huge = format!("张三说：{PROMISE}{}", "补".repeat(120_000));
    assert!(huge.len() >= 300 * 1024, "第 11 轮正文必须 ≥ 300 KiB");
    let mut bodies: Vec<String> = chunk_quotes();
    bodies[10] = huge.clone();
    let borrowed: Vec<&str> = bodies.iter().map(String::as_str).collect();
    let uids = seed_bodies(&mut root, &borrowed, 1_000);
    set_owner(&mut root);
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    let subject = add_anchor(&root, &scope, "person", "张三", &uids[0], NOW - 1_000);

    // 引文仍是短句（≤120 字、逐字子串）：超长的是目标轮正文。
    let mut quotes = chunk_quotes();
    quotes[10] = PROMISE.to_string();
    let plan = select_fact_batch(
        root.transcript(),
        root.ledger(),
        root.memory(),
        0,
        FactFamily::Promise,
        NOW,
    )
    .unwrap()
    .expect("12 轮都还没跑 fact/promise");
    let extractor = ScriptBackend::new(vec![Ok(fact_chunk_reply(&quotes))]);
    let queued = run_fact_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &extractor,
        NOW,
        ReviewMode::EnqueueOnly,
    )
    .unwrap();
    assert_eq!(extractor.calls.get(), 1, "EnqueueOnly 只问抽取");
    assert_eq!(queued.state, PassState::Pending);
    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "pending_review");

    // 第二片（草稿 11/12）真的超限：用公开渲染重建这一片，不读引擎内部。
    let draft_of = |index: usize| FactDraft {
        reference: format!("f{}", index + 1),
        turn_uid: uids[index].clone(),
        subject_anchor_id: subject.clone(),
        object_anchor_id: None,
        subject_canonical: "张三".into(),
        object_canonical: None,
        predicate: "promise".into(),
        value: Some(format!("对账单第{}条", index + 1)),
        category: "promise".into(),
        mode: "stated".into(),
        time_text: String::new(),
        quote: quotes[index].clone(),
        evidence_turn_uids: vec![uids[index].clone()],
        text: "张三承诺发出对账单。".into(),
    };
    let batch = Batch {
        session_key: "sess-batch-A".into(),
        device_id: "dev-batch-A".into(),
        source_agent: "agent-batch-A".into(),
        project_hint: None,
        rows: bodies
            .iter()
            .enumerate()
            .map(|(index, body)| TurnRow {
                turn_uid: uids[index].clone(),
                role: if index % 2 == 0 { "human" } else { "agent" }.into(),
                body: body.clone(),
                created_at_ms: 1_000 + index as i64,
                ingested_at_ms: 1_001_000,
                speaker: None,
                speaker_anchor_id: None,
            })
            .collect(),
    };
    let system = fact_review_system(FactFamily::Promise);
    let ctx = ReviewInputContext {
        session_key: batch.session_key.clone(),
        owner_agent_id: "alice".into(),
        visibility: "project".into(),
        scope: "project:proj-x".into(),
        speakers: batch
            .rows
            .iter()
            .map(|row| {
                if row.role == "human" { "小明".to_string() } else { batch.source_agent.clone() }
            })
            .collect(),
    };
    let first_chunk =
        fact_review_user(FactFamily::Promise, &[draft_of(0), draft_of(1)], &batch, &ctx);
    let second_chunk =
        fact_review_user(FactFamily::Promise, &[draft_of(10), draft_of(11)], &batch, &ctx);
    assert!(
        system.len() + first_chunk.len() <= REVIEW_INPUT_MAX_BYTES,
        "第一片必须没超限（否则测不到第二片）"
    );
    assert!(
        system.len() + second_chunk.len() > REVIEW_INPUT_MAX_BYTES,
        "第二片必须真的越过 {REVIEW_INPUT_MAX_BYTES} 字节，实际 {} 字节",
        system.len() + second_chunk.len()
    );

    let engine = ScriptBackend::engine(fact_review(&PASS_TEN));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(engine.calls.get(), 0, "任一片超限 ⇒ 一片都不送");
    assert_eq!(out.item_state, ReviewState::Quarantined);
    assert_eq!(out.verdicts, None);
    assert_eq!(out.fail_closed_reason, Some(REVIEW_INPUT_TOO_LARGE));
    assert_eq!((out.written, out.facts, out.cards, out.mentions), (0, 0, 0, 0));
    assert_eq!(count(root.memory(), "fact"), 0);
    let rows = decisions(root.memory(), id);
    assert_eq!(rows.len(), 2, "program pass + 程序侧的超长裁定");
    assert_eq!(
        (rows[1].stage.as_str(), rows[1].verdict.as_str(), rows[1].reason_code.as_str()),
        ("program", "uncertain", REVIEW_INPUT_TOO_LARGE)
    );
    assert_eq!(rows[1].decider, "program");
    assert_eq!(rows[1].review_input_sha256, "", "没有送审：送审指纹为空");
}

// ── H7c2：审查轴水位 ──────────────────────────────────────────────────────────

/// H7c2：一次**成功**的 fact 引擎运行在 ledger 里留下 `pass_id='fact'` 的水位：
/// `max_decision_id` 是本地判定台账的最大 id，三态与 `stats` 的 by_pass 同一格
/// （取的是当前值，不是增量）。水位是读数，引擎的返回一个字节都不变。
#[test]
fn a_successful_run_writes_the_pass_watermark_from_stats() {
    let (_dir, root, _uids) = seeded_fact();
    let (id, _) = pending_fact(&root);
    assert!(
        review_mark::all(root.ledger()).unwrap().is_empty(),
        "入队/程序闸还没跑完时不该有水位行"
    );

    let engine = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);

    let marks = review_mark::all(root.ledger()).unwrap();
    assert_eq!(marks.len(), 1, "只应有一条 fact 水位：{marks:?}");
    let mark = &marks[0];
    assert_eq!(mark.pass_id, "fact");
    let expected_max = rc_memory::review::max_decision_id(root.memory()).unwrap();
    assert!(expected_max > 0, "三段判定已落，最大 id 必须是正数");
    assert_eq!(mark.max_decision_id, expected_max, "水位记的是判定台账的最大 id");
    let stats = rc_memory::review::stats(root.memory(), NOW).unwrap();
    let fact = stats
        .by_pass
        .iter()
        .find(|(pass, _)| pass == "fact")
        .expect("fact 的 by_pass 行不见了")
        .1;
    assert_eq!(
        (mark.pending_review, mark.quarantined, mark.applied),
        (fact[0], fact[4], fact[2]),
        "三态必须与 stats 当前值一致（下标 0/4/2）"
    );
    assert_eq!(mark.applied, 1, "一条 applied");
    assert_eq!(mark.pending_review, 0);
    assert_eq!(mark.updated_at_ms, NOW, "updated_at_ms 是调用方给的钟");
}

/// H7c2：纯 LLM 故障**不写**水位——程序闸的 pass 判定已记，但这一次没有审核结论，
/// item 还停在 pending，表必须还是空的（下一次成功运行才写）。
#[test]
fn an_llm_failure_does_not_write_a_watermark() {
    let (_dir, root, _uids) = seeded_fact();
    let (id, _) = pending_fact(&root);
    let engine = ScriptBackend::new(vec![Err(LlmFailure::BackendError)]);

    let err =
        run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap_err();
    assert!(matches!(err, RunError::Llm(LlmFailure::BackendError)), "{err:?}");
    assert_eq!(item_state(root.memory(), id), "pending_review");
    assert!(
        review_mark::all(root.ledger()).unwrap().is_empty(),
        "纯 LLM 故障不是已记判定，不许写水位"
    );
}

// ── A-rebind（任务 B）：agent 轮的 actor 程序回填 ──────────────────────────────

/// agent 轮正文：只有"我"，没有行动者姓名（item 141 的形状）。
const REBIND_BODY: &str = "我把配置改回去，服务就恢复了";
const REBIND_QUOTE: &str = "我把配置改回去，服务就恢复了";

/// 两轮：第 0 轮 human 陪衬、第 1 轮 agent 正文（`seed_bodies` 按序交替）。
/// 另建一格**包名锚**（`kind=work`，canonical `rc-distill`）——模型会选错的正是它。
fn seeded_rebind() -> (tempfile::TempDir, Root, Vec<String>, String) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &["先对一下本周的进度", REBIND_BODY], 1_000);
    set_owner(&mut root);
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    let package = add_anchor(&root, &scope, "work", "rc-distill", &uids[1], NOW - 2_000);
    (dir, root, uids, package)
}

fn rebind_event_reply(actor_ref: &str) -> String {
    json!({
        "schema_version": 0, "pass": "event",
        "events": [{
            "ref": "e1", "turn": "t2", "category": "state_change", "actor": actor_ref,
            "trigger": "改回去", "action": "restore_config", "object": null, "object_text": "配置文件",
            "participants": [actor_ref], "place": null, "result": "done", "mode": "stated",
            "time_text": null, "quote": REBIND_QUOTE, "evidence": ["t2"],
            "text": "昨晚把配置改回去，服务恢复。"
        }],
        "empty_turns": []
    })
    .to_string()
}

/// ① agent 轮 + 唯一自锚 + actor 不在正文 ⇒ 写成会话自锚、`actor_rebound=1`。
#[test]
fn event_agent轮_actor回填成会话自锚并计数() {
    let (_dir, root, uids, package) = seeded_rebind();
    let plan = event_plan(&root);
    let actor_ref = plan
        .candidates
        .iter()
        .find(|candidate| candidate.anchor_id == package)
        .expect("包名锚必须在候选池里")
        .reference
        .clone();
    let backend =
        ScriptBackend::new(vec![Ok(rebind_event_reply(&actor_ref)), Err(LlmFailure::BackendError)]);
    let err = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW)
        .unwrap_err();
    assert!(matches!(err, DistillError::Llm(LlmFailure::BackendError)), "{err}");
    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "pending_review");

    let engine = ScriptBackend::engine(event_review(&[("e1", "pass", None)]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(out.actor_rebound, 1, "回填必须计数（程序记，不靠模型自报）");
    assert_eq!(out.written, 1, "回填后照常写库");

    let self_anchor: String = root.memory().conn()
        .query_row("SELECT id FROM anchor WHERE kind='agent' AND canonical='agent-batch-A'",
                   [], |r| r.get(0))
        .unwrap();
    let (subject, participants): (String, String) = root.memory().conn()
        .query_row("SELECT subject_anchor_id, participants_json FROM fact", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(subject, self_anchor, "落库的 actor 是会话自锚，不是包名");
    assert!(participants.contains(&self_anchor), "participants 里也要含回填后的 actor");
    assert!(!participants.contains(&package), "旧 actor 被换掉，不留在 participants 里");
    assert_eq!(uids.len(), 2);

    let rows = decisions(root.memory(), id);
    assert!(rows.last().unwrap().detail.ends_with("rebound=1"),
            "apply 的 detail 带上计数：{}", rows.last().unwrap().detail);
}

/// ② agent 轮 + 多个 person 锚、没有**可见且唯一**的自锚 ⇒ 仍拒（整条 rejected，零行）。
///
/// 自锚不在候选池里（归属行写成了另一个 agent）时，"回填"没有可辩护的目标：
/// 程序不猜，6c 原样拒。池里另有多个人锚，正是"多个候选、无唯一会话 agent 锚"的形状。
#[test]
fn event_多个person锚且无唯一自锚时仍拒() {
    let (_dir, root, _uids, _package) = seeded_rebind();
    let plan = event_plan(&root);
    // 自锚在这一刻已被 `ensure_agent_anchor` 建出来；把它的归属换成另一个 agent ⇒ 不可见。
    // `owner::insert` 是 INSERT OR IGNORE，后续 ensure 不会把它改回来。
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    let evidence_turn = plan.batch.rows[1].turn_uid.clone();
    add_anchor(&root, &scope, "person", "张三", &evidence_turn, NOW - 1_500);
    add_anchor(&root, &scope, "person", "李四", &evidence_turn, NOW - 1_400);
    // 顺序有讲究：`add_anchor` 的 `adopt_rows` 会把**全部**锚的归属改成 alice/project，
    // 所以"把自锚挪出候选池"必须放在它后面。`private_agent` + 另一个 owner ⇒
    // `anchor_visible` 为假（project 可见性会把它重新放行，两列都要改）。
    root.memory().conn()
        .execute("UPDATE resource_owner SET owner_agent_id='agent-other', visibility='private_agent'
                   WHERE mem_table='anchor' AND mem_id=(
                     SELECT id FROM anchor WHERE kind='agent' AND canonical='agent-batch-A')", [])
        .unwrap();
    let actor_ref = plan
        .candidates
        .iter()
        .find(|candidate| candidate.canonical == "rc-distill")
        .expect("包名锚必须在候选池里")
        .reference
        .clone();
    let backend =
        ScriptBackend::new(vec![Ok(rebind_event_reply(&actor_ref)), Ok("{}".into())]);
    // 抽取这一侧就会在闸上拒：整条 item 不进审核（I3），行数为 0。
    run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW)
        .unwrap_or_else(|e| panic!("run_event 失败：{e:?}；actor_ref={actor_ref}"));
    assert_eq!(count(root.memory(), "fact"), 0, "没有唯一自锚 ⇒ 整条拒，不写行");
    let (_, state, _) = only_item(root.memory());
    assert_eq!(state, "rejected", "闸 6c 仍是整条拒，一个字不变");
}
