//! S3b-4a：`review::run` 的两件新面——`RetryPolicy`（`quarantined` 复审）与
//! `mark_turns`（把一次引擎运行的终态铺到批内轮，设计 `2026-10-26-review-queue-s3`
//! §4/§11 的 S3b 段）。
//!
//! 夹具风格与 `tests/review_run.rs` / `tests/review_naming_run.rs` 相同：真
//! `Root::open(tempdir)` + `mod common`，正反例只用通用占位名（张三/李四/老张），
//! 不含任何语料专名。
//!
//! 覆盖（与派发档 §3.3 一一对应）：
//! 1. quarantined 复审：先造 quarantined item（审核坏 JSON），`RespectState` 空操作；
//!    再用 `RetryPolicy::Quarantined` + pass 回复 ⇒ applied、fact 1 行；
//! 2. `mark_turns` fact mixed（pass/reject/uncertain 三轮各一）⇒ Done/Rejected/Quarantined；
//!    批内没有草稿的轮（`empty_turns` 覆盖）不动；
//! 3. `mark_turns` naming applied ⇒ 全 Done；
//! 4. `mark_turns` verdicts None + Applied ⇒ 载荷里有草稿的轮 Done，批内其余轮保持抽取期状态。
//!
//! H3（同轮多草稿 + 作用域）：
//! 5. 同轮 pass+reject ⇒ Done（不是 Rejected）；
//! 6. 同轮 pass+uncertain ⇒ Quarantined(review_uncertain)；
//! 7. 同轮 reject+uncertain ⇒ Quarantined(review_uncertain)；
//! 8. 同轮两条 pass 且该轮在 `duplicate_uids` ⇒ Done；另一轮全 reject 仍是 Rejected；
//! 9. `mark_turns`（verdicts=None）只碰载荷里有草稿的轮：t3 不在载荷里，保持 `empty`；
//! 10. `mark_batch` 同一条作用域：只改 t1/t2；载荷删掉 `turn_uid` ⇒ 回退整批（t3 被改）。

use rc_distill::pass_fact::{run_fact, select_fact_batch};
use rc_distill::pass_naming::{run_naming, select_naming_batch};
use rc_distill::pass_prompt::FactFamily;
use rc_distill::review::run::{
    mark_batch, mark_turns, run_item, run_item_with_policy, RetryPolicy, RunOutcome,
};
use rc_distill::review::{REVIEW_REJECT, REVIEW_UNCERTAIN};
use rc_distill::DistillError;
use rc_ledger::pass_state;
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::review::{Item, ReviewState, ReviewVerdict};
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{AnchorRole, Provenance, Scope, ScopeKind};
use rc_store::Root;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

mod common;
use common::{adopt_rows, seed_bodies};

const NOW: i64 = 1_700_000_000_000;
const MODEL: &str = "fake-mark-turns";

const T1: &str = "我下周三之前把对账单发给你";
/// t2 是 **agent** 轮（`seed_bodies` 按 i 交替造 human/agent），正文必须真写着主语
/// "张三"——agent 说"收到对账单"而主语挂张三，是旧夹具里不真实的形状。
const T2: &str = "张三已经收到对账单";
const T3: &str = "那台打印机还没修好";
const T4: &str = "谢谢";

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

fn add_anchor(root: &Root, kind: &str, canonical: &str, uid: &str, created_at_ms: i64) {
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    put_entity(
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
}

/// fact：四轮（承诺 / 收到 / 打印机 / 寒暄），张三 a1、李四 a2 两个活锚。
fn seeded_fact() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[T1, T2, T3, T4], 1_000);
    set_owner(&mut root);
    add_anchor(&root, "person", "李四", &uids[0], NOW - 2_000);
    add_anchor(&root, "person", "张三", &uids[0], NOW - 1_000);
    assert_eq!(uids.len(), 4);
    (dir, root, uids)
}

/// 三张草稿（t1/t2/t3 各一条）+ t4 由 `empty_turns` 宣告。
fn fact_three_reply() -> String {
    json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a1", "predicate": "promise",
            "object": "a2", "value": "把对账单发给你", "category": "promise",
            "mode": "stated", "time_text": "下周三之前", "quote": T1,
            "evidence": ["t1"], "text": "张三承诺在下周三之前把对账单发给李四。"
        }, {
            "ref": "f2", "turn": "t2", "subject": "a1", "predicate": "promise",
            "object": null, "value": "收到对账单", "category": "promise",
            "mode": "stated", "time_text": "", "quote": T2,
            "evidence": ["t2"], "text": "张三收到对账单。"
        }, {
            "ref": "f3", "turn": "t3", "subject": "a1", "predicate": "promise",
            "object": null, "value": "打印机", "category": "promise",
            "mode": "stated", "time_text": "", "quote": T3,
            "evidence": ["t3"], "text": "张三承诺修打印机。"
        }],
        "empty_turns": ["t4"]
    })
    .to_string()
}

/// 只有 t1 一条草稿（复审用例的窄批）。
fn fact_single_reply() -> String {
    json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a1", "predicate": "promise",
            "object": "a2", "value": "把对账单发给你", "category": "promise",
            "mode": "stated", "time_text": "下周三之前", "quote": T1,
            "evidence": ["t1"], "text": "张三承诺在下周三之前把对账单发给李四。"
        }],
        "empty_turns": ["t2", "t3", "t4"]
    })
    .to_string()
}

fn fact_review(decisions: &[(&str, &str, Option<&str>)]) -> String {
    let verdicts: Vec<Value> = decisions
        .iter()
        .map(|(reference, verdict, code)| {
            // §7.2：pass/reject 带 quote 内的一段原文（f1/f2 的引文都含「对账单」）；
            // uncertain 允许 null。
            let evidence = if *verdict == "uncertain" { None } else { Some("对账单") };
            json!({
                "ref": reference, "verdict": verdict, "code": code, "evidence": evidence
            })
        })
        .collect();
    json!({"schema_version": 0, "pass": "fact_review", "family": "promise", "verdicts": verdicts})
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
    fn engine(review: String) -> Self {
        // 一片 fact 审核 = proposer + challenger（§7.2）；event/naming 只消费第一条。
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

fn fact_plan(root: &Root) -> rc_distill::pass_fact::FactPlan {
    select_fact_batch(root.transcript(), root.ledger(), root.memory(), 0, FactFamily::Promise, NOW)
        .unwrap()
        .expect("夹具里 4 轮都还没跑 fact/promise")
}

/// 造一条 `pending_review` 的 fact item（抽取成功、审核没开口）。
fn pending_fact(root: &Root, extraction: String) -> i64 {
    let plan = fact_plan(root);
    let backend = ScriptBackend::new(vec![Ok(extraction), Err(LlmFailure::BackendError)]);
    let err = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW)
        .unwrap_err();
    assert!(matches!(err, DistillError::Llm(LlmFailure::BackendError)), "{err}");
    only_item(root.memory())
}

// ── 读数 ──────────────────────────────────────────────────────────────────────

fn count(m: &rc_store::MemoryHandle, table: &str) -> i64 {
    m.conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn only_item(m: &rc_store::MemoryHandle) -> i64 {
    m.conn()
        .query_row("SELECT id FROM review_item", [], |r| r.get(0))
        .unwrap()
}

fn item_state(m: &rc_store::MemoryHandle, id: i64) -> String {
    m.conn()
        .query_row("SELECT state FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap()
}

/// 一条 fact 状态行：`(state, reason)`。
fn fact_turn(root: &Root, uid: &str) -> (String, Option<String>) {
    root.ledger()
        .conn()
        .query_row(
            "SELECT state, reason FROM extract_pass_state
             WHERE turn_uid=?1 AND pass_id='fact' AND family='promise'",
            [uid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

fn naming_turn(root: &Root, uid: &str) -> String {
    root.ledger()
        .conn()
        .query_row(
            "SELECT state FROM extract_pass_state WHERE turn_uid=?1 AND pass_id='naming' AND family=''",
            [uid],
            |r| r.get(0),
        )
        .unwrap()
}

/// 测试专用：把这条 pass 的批内轮拨回 `pending`，让 `mark_turns` 的写入成为唯一可观测的那一次。
/// 直接改 SQL 是刻意的：`pass_state::note_attempt` 对终态行**故意不改状态**（它不复活已完成
/// 的批），而这里要的正是"从任意状态回到 pending"。
fn reset_turns(root: &Root, pass_id: &str, family: &str, uids: &[String]) {
    for uid in uids {
        root.ledger()
            .conn()
            .execute(
                "UPDATE extract_pass_state
                    SET state='pending', reason=NULL, next_retry_at_ms=NULL
                  WHERE turn_uid=?1 AND pass_id=?2 AND family=?3",
                rusqlite::params![uid, pass_id, family],
            )
            .unwrap();
    }
}

fn reset_fact_turns(root: &Root, uids: &[String]) {
    reset_turns(root, "fact", "promise", uids);
    for uid in uids {
        assert_eq!(fact_turn(root, uid).0, "pending", "{uid}");
    }
}

fn reset_naming_turns(root: &Root, uids: &[String]) {
    reset_turns(root, "naming", "", uids);
    for uid in uids {
        assert_eq!(naming_turn(root, uid), "pending", "{uid}");
    }
}

// ── 1. quarantined 复审 ───────────────────────────────────────────────────────

/// 先造 quarantined item（审核坏 JSON）：`RespectState` 空操作；再用
/// `RetryPolicy::Quarantined` + pass 回复 ⇒ applied、fact 1 行、判定链多一条 review pass。
#[test]
fn a_quarantined_item_is_re_reviewed_only_under_the_explicit_policy() {
    let (_dir, root, _uids) = seeded_fact();
    let id = pending_fact(&root, fact_single_reply());
    // 审核坏 JSON：引擎记 uncertain(review_json)，runner 把整批铺成 quarantined。
    let bad = ScriptBackend::engine("{}".to_string());
    let err = run_item(root.memory(), root.transcript(), root.ledger(), id, &bad, NOW).unwrap_err();
    assert!(matches!(err, rc_distill::review::run::RunError::Review(_)), "{err:?}");
    assert_eq!(item_state(root.memory(), id), "quarantined");
    // 引擎只记判定，不碰 `extract_pass_state`（逐轮铺开是 runner 的活）；这里断的是
    // 引擎这一侧唯一的东西：0 行。逐轮状态留给 `mark_turns` 的用例。
    assert_eq!(count(root.memory(), "fact"), 0);

    // RespectState：隔离项空操作——不调模型、不改状态、不写行。
    let idle = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    let out = run_item_with_policy(
        root.memory(),
        root.transcript(),
        root.ledger(),
        id,
        &idle,
        NOW,
        RetryPolicy::RespectState,
    )
    .unwrap();
    assert_eq!(idle.calls.get(), 0, "RespectState 不许为隔离项开口");
    assert_eq!(out.item_state, ReviewState::Quarantined);
    assert_eq!(out.verdicts, None);
    assert_eq!(item_state(root.memory(), id), "quarantined");
    assert_eq!(count(root.memory(), "fact"), 0);

    // Quarantined：按 pending 同一路径重跑，pass ⇒ approved ⇒ 写库 ⇒ applied。
    let engine = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    let out = run_item_with_policy(
        root.memory(),
        root.transcript(),
        root.ledger(),
        id,
        &engine,
        NOW,
        RetryPolicy::Quarantined,
    )
    .unwrap();
    assert_eq!(engine.calls.get(), 2, "复审必须真的再问两次（两档）");
    assert_eq!(out.item_state, ReviewState::Applied);
    assert!(out.applied_now);
    assert_eq!(count(root.memory(), "fact"), 1, "复审通过后写一行");
    assert_eq!(item_state(root.memory(), id), "applied");

    // 判定链：program pass → review uncertain（第一次）→ review pass（复审）→ apply。
    let stages: Vec<(String, String, String)> = root
        .memory()
        .conn()
        .prepare("SELECT stage, verdict, reason_code FROM review_decision WHERE item_id=?1 ORDER BY id")
        .unwrap()
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        stages
            .iter()
            .map(|(stage, verdict, _)| (stage.as_str(), verdict.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("program", "pass"),
            ("review", "uncertain"),
            ("review", "pass"),
            ("apply", "pass"),
        ]
    );
    assert_eq!(stages[2].2, "review_pass");
}

// ── 2. fact mixed ─────────────────────────────────────────────────────────────

/// `mark_turns`：fact mixed（三轮各一）⇒ Done/Rejected/Quarantined；`empty_turns` 那轮不动。
#[test]
fn mark_turns_maps_a_mixed_fact_batch_per_draft_and_leaves_undrafted_turns_alone() {
    let (_dir, root, uids) = seeded_fact();
    let id = pending_fact(&root, fact_three_reply());
    let item = rc_memory::review::get(root.memory(), id).unwrap().unwrap();

    let engine = ScriptBackend::engine(fact_review(&[
        ("f1", "pass", None),
        ("f2", "reject", Some("not_durable")),
        ("f3", "uncertain", Some("quote_mismatch")),
    ]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(out.item_state, ReviewState::Applied, "有 pass 草稿 ⇒ 整条前进");

    // 三轮都还在 pending（引擎不碰 extract_pass_state）；把 t4 落成 empty，证明
    // "批内没有草稿的轮不动"——它来自抽取阶段，不是 mark_turns 猜的。
    reset_fact_turns(&root, &uids[..3]);
    pass_state::mark_terminal(
        root.ledger(),
        pass_state::PassKey { turn_uid: &uids[3], pass_id: "fact", family: "promise" },
        pass_state::PassState::Empty,
        None,
        NOW,
    )
    .unwrap();
    assert_eq!(fact_turn(&root, &uids[3]).0, "empty", "t4 没有草稿");

    mark_turns(root.ledger(), &item, &out, NOW).unwrap();
    assert_eq!(fact_turn(&root, &uids[0]), ("done".to_string(), None));
    assert_eq!(
        fact_turn(&root, &uids[1]),
        ("rejected".to_string(), Some(REVIEW_REJECT.to_string()))
    );
    assert_eq!(
        fact_turn(&root, &uids[2]),
        ("quarantined".to_string(), Some(REVIEW_UNCERTAIN.to_string()))
    );
    assert_eq!(
        fact_turn(&root, &uids[3]).0,
        "empty",
        "批内没有草稿的轮不许被 mark_turns 改写"
    );
}

// ── 3. naming applied ─────────────────────────────────────────────────────────

fn naming_a1() -> String {
    json!({
        "schema_version": 4, "pass": "a1",
        "anchors": [{
            "ref": "a1", "kind": "person", "canonical": "张三", "aliases": ["老张"],
            "reality_status": "real", "evidence": ["t1", "t2"], "reason": "两轮点名"
        }],
        "mentions": [{
            "anchor_ref": "a1", "surface": "老张", "turn_uid": "t2", "speaker_ref": null,
            "discourse_role": "mentioned", "resolution": "resolved", "confidence": 0.9
        }],
        "issues": [], "unresolved_surfaces": []
    })
    .to_string()
}

fn naming_review() -> String {
    json!({
        "schema_version": 0, "pass": "naming_review",
        "verdicts": [{"ref": "a1", "verdict": "pass", "code": null}]
    })
    .to_string()
}

/// `mark_turns`：naming applied ⇒ 整批 Done（不按锚的位置切片）。
#[test]
fn mark_turns_marks_the_whole_naming_batch_done_on_apply() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    // naming 的正文里必须真的出现 canonical/alias（程序闸的逐字检查）。
    let uids = seed_bodies(
        &mut root,
        &[
            "张三说明天把对账单发给你",
            "老张说对账单后天到，张三负责跟进",
            "李四说预算表还没做完",
            "小结：老张把对账单交给了张三",
        ],
        1_000,
    );
    set_owner(&mut root);
    let plan = select_naming_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("四轮都还没跑 naming");
    let backend = ScriptBackend::new(vec![
        Ok(naming_a1()),
        Ok(naming_a1()),
        Ok(naming_review()),
    ]);
    run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    let id = only_item(root.memory());
    assert_eq!(item_state(root.memory(), id), "applied");

    // 把批内轮拨回 pending：接下来 Done 只能是 mark_turns 写的。
    reset_naming_turns(&root, &uids);
    let item = rc_memory::review::get(root.memory(), id).unwrap().unwrap();
    let out = run_item(
        root.memory(),
        root.transcript(),
        root.ledger(),
        id,
        &ScriptBackend::engine(String::new()),
        NOW,
    )
    .unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);
    assert!(out.verdicts.is_some(), "applied 重放要能重建判决");

    mark_turns(root.ledger(), &item, &out, NOW).unwrap();
    for uid in &uids {
        assert_eq!(naming_turn(&root, uid), "done", "{uid}");
    }
}

// ── 4. verdicts None + Applied ───────────────────────────────────────────────

/// `mark_turns`：verdicts=None 且 item_state=Applied ⇒ 载荷里有草稿的轮 Done（已写库，
/// 不是隔离）；批内没有草稿的轮保持抽取期状态——H3 的作用域。
#[test]
fn mark_turns_marks_the_batch_done_when_verdicts_are_unreadable_but_the_item_is_applied() {
    let (_dir, root, uids) = seeded_fact();
    let id = pending_fact(&root, fact_single_reply());
    let engine = ScriptBackend::engine(fact_review(&[("f1", "pass", None)]));
    run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(count(root.memory(), "fact"), 1);
    // 判决读不出：applied 重放会以 verdicts=None 收尾（fail-closed），但仍已写库。
    root.memory()
        .conn()
        .execute(
            "UPDATE review_decision SET detail='corrupt' WHERE item_id=?1 AND stage='review'",
            [id],
        )
        .unwrap();
    let item = rc_memory::review::get(root.memory(), id).unwrap().unwrap();
    let out = run_item(
        root.memory(),
        root.transcript(),
        root.ledger(),
        id,
        &ScriptBackend::engine(String::new()),
        NOW,
    )
    .unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);
    assert_eq!(out.verdicts, None);
    assert_eq!(
        out.fail_closed_reason,
        Some(rc_distill::review::REVIEW_VERDICTS_MISSING)
    );

    // 只有 t1 有草稿：拨回 pending 让 Done 只能是 mark_turns 写的；t2..t4 没有草稿，
    // 保持抽取期的 `pending(review_pending)`。
    reset_fact_turns(&root, &uids[..1]);
    mark_turns(root.ledger(), &item, &out, NOW).unwrap();
    assert_eq!(
        fact_turn(&root, &uids[0]).0,
        "done",
        "已写库的草稿轮必须落 done"
    );
    for uid in &uids[1..] {
        assert_eq!(
            fact_turn(&root, uid),
            ("pending".to_string(), None),
            "{uid}: 载荷里没有草稿，抽取期留下的状态不许被改写"
        );
    }
}

// ── 5–10. H3：同轮多草稿的聚合与作用域 ───────────────────────────────────────

/// 手工 outcome：测试直接给"引擎本来会交出来的判决"，不跑第二次模型。
fn run_outcome(
    item_state: ReviewState,
    verdicts: Option<Vec<(&str, ReviewVerdict)>>,
    duplicate_uids: &[&str],
) -> RunOutcome {
    RunOutcome {
        item_state,
        verdicts: verdicts
            .map(|rows| rows.into_iter().map(|(r, v)| (r.to_string(), v)).collect()),
        fail_closed_reason: None,
        // §7.2：这条用例不涉及证据闸降级，整格留空。
        evidence_downgrades: Vec::new(),
        adjudications: Vec::new(),
        // I3：这条用例不涉及程序闸逐 draft 拒收，整格留空（`mark_turns` 因此不改写任何轮）。
        program_rejected_turns: Vec::new(),
        duplicate_uids: duplicate_uids.iter().map(|uid| uid.to_string()).collect(),
        written: 0,
        duplicate: 0,
        // A-rebind：这条用例不涉及 event 的 actor 回填。
        actor_rebound: 0,
        facts: 0,
        cards: 0,
        mentions: 0,
        applied_now: false,
        // S4：这条用例不涉及写库，没有卡句柄要交出去。
        card_ids: Vec::new(),
    }
}

/// 把 item 的载荷 `items` 换成手工条目（`mark_turns` 只读 `items[].turn_uid`）。
fn rewrite_payload_items(root: &Root, id: i64, items: Value) -> Item {
    let payload = json!({
        "schema": "review.pass.v1",
        "pass_id": "fact",
        "family": "promise",
        "items": items,
    });
    root.memory()
        .conn()
        .execute(
            "UPDATE review_item SET payload_json=?2 WHERE id=?1",
            rusqlite::params![id, payload.to_string()],
        )
        .unwrap();
    rc_memory::review::get(root.memory(), id).unwrap().unwrap()
}

/// 每条草稿一条载荷项，全部落在给定轮上（同轮两条就重复给两次）。
fn items_for(turns: &[&str]) -> Value {
    Value::Array(turns.iter().map(|uid| json!({"turn_uid": uid})).collect())
}

fn preset_fact_turn(root: &Root, uid: &str, state: pass_state::PassState) {
    pass_state::mark_terminal(
        root.ledger(),
        pass_state::PassKey { turn_uid: uid, pass_id: "fact", family: "promise" },
        state,
        None,
        NOW,
    )
    .unwrap();
}

/// 同轮 pass + reject ⇒ `Done`：后写的 reject 不许覆盖先写的 pass。
#[test]
fn a_turn_with_a_pass_and_a_reject_draft_ends_done() {
    let (_dir, root, uids) = seeded_fact();
    let id = pending_fact(&root, fact_three_reply());
    reset_fact_turns(&root, &uids[..3]);
    let item = rewrite_payload_items(&root, id, items_for(&[&uids[0], &uids[0]]));
    let out = run_outcome(
        ReviewState::Applied,
        Some(vec![("f1", ReviewVerdict::Pass), ("f2", ReviewVerdict::Reject)]),
        &[],
    );

    mark_turns(root.ledger(), &item, &out, NOW).unwrap();
    assert_eq!(
        fact_turn(&root, &uids[0]),
        ("done".to_string(), None),
        "同轮任一 pass ⇒ Done（不是后写的 Rejected）"
    );
    assert_eq!(fact_turn(&root, &uids[1]).0, "pending", "载荷里没有的轮不动");
}

/// 同轮 pass + uncertain ⇒ `Quarantined(review_uncertain)`：不确定优先于 pass。
/// uncertain 放在**后**被判的那一条之前（f1 uncertain、f2 pass），旧实现按 verdict 顺序
/// 写库会以 pass 收尾（Done），本用例因此钉的是优先级而不是写入顺序。
#[test]
fn a_turn_with_a_pass_and_an_uncertain_draft_is_quarantined() {
    let (_dir, root, uids) = seeded_fact();
    let id = pending_fact(&root, fact_three_reply());
    reset_fact_turns(&root, &uids[..3]);
    let item = rewrite_payload_items(&root, id, items_for(&[&uids[0], &uids[0]]));
    let out = run_outcome(
        ReviewState::Applied,
        Some(vec![("f1", ReviewVerdict::Uncertain), ("f2", ReviewVerdict::Pass)]),
        &[],
    );

    mark_turns(root.ledger(), &item, &out, NOW).unwrap();
    assert_eq!(
        fact_turn(&root, &uids[0]),
        ("quarantined".to_string(), Some(REVIEW_UNCERTAIN.to_string())),
        "同轮任一 uncertain ⇒ 隔离，即使另一条 pass"
    );
}

/// 同轮 reject + uncertain ⇒ `Quarantined`：不确定优先于 reject。
/// 同样把 uncertain 放在写入顺序的前面，旧实现会以 reject 收尾。
#[test]
fn a_turn_with_a_reject_and_an_uncertain_draft_is_quarantined() {
    let (_dir, root, uids) = seeded_fact();
    let id = pending_fact(&root, fact_three_reply());
    reset_fact_turns(&root, &uids[..3]);
    let item = rewrite_payload_items(&root, id, items_for(&[&uids[0], &uids[0]]));
    let out = run_outcome(
        ReviewState::Applied,
        Some(vec![("f1", ReviewVerdict::Uncertain), ("f2", ReviewVerdict::Reject)]),
        &[],
    );

    mark_turns(root.ledger(), &item, &out, NOW).unwrap();
    assert_eq!(
        fact_turn(&root, &uids[0]),
        ("quarantined".to_string(), Some(REVIEW_UNCERTAIN.to_string())),
        "同轮任一 uncertain ⇒ 隔离，即使另一条 reject"
    );
}

/// 同轮两条 pass、该轮在 `duplicate_uids` ⇒ `Done`；另一轮**全 reject 但也在
/// `duplicate_uids` 里** ⇒ 仍 `Done`（去重 = 内容已在库里，优先于 reject）；第三轮全 reject
/// 且不在去重集 ⇒ `Rejected`。
#[test]
fn duplicate_turns_end_done_even_when_their_drafts_are_rejected() {
    let (_dir, root, uids) = seeded_fact();
    let id = pending_fact(&root, fact_three_reply());
    reset_fact_turns(&root, &uids[..3]);
    let item = rewrite_payload_items(
        &root,
        id,
        items_for(&[&uids[0], &uids[0], &uids[1], &uids[2]]),
    );
    let out = run_outcome(
        ReviewState::Applied,
        Some(vec![
            ("f1", ReviewVerdict::Pass),
            ("f2", ReviewVerdict::Pass),
            ("f3", ReviewVerdict::Reject),
            ("f4", ReviewVerdict::Reject),
        ]),
        &[&uids[0], &uids[1]],
    );

    mark_turns(root.ledger(), &item, &out, NOW).unwrap();
    assert_eq!(
        fact_turn(&root, &uids[0]),
        ("done".to_string(), None),
        "两条 pass + 去重命中 ⇒ Done（无 reason）"
    );
    assert_eq!(
        fact_turn(&root, &uids[1]),
        ("done".to_string(), None),
        "去重命中优先于 reject：内容已在库里，不是失败"
    );
    assert_eq!(
        fact_turn(&root, &uids[2]),
        ("rejected".to_string(), Some(REVIEW_REJECT.to_string())),
        "去重是逐轮的，不许把没命中的 reject 抬成 done"
    );
}

/// `mark_turns`（verdicts=None）：只写载荷里有草稿的轮；t3 有批内记录但没草稿 ⇒ 保持 `empty`。
#[test]
fn mark_turns_without_verdicts_only_touches_turns_that_carry_drafts() {
    let (_dir, root, uids) = seeded_fact();
    let id = pending_fact(&root, fact_three_reply());
    reset_fact_turns(&root, &uids[..3]);
    preset_fact_turn(&root, &uids[2], pass_state::PassState::Empty);
    assert_eq!(fact_turn(&root, &uids[2]).0, "empty");
    let item = rewrite_payload_items(&root, id, items_for(&[&uids[0], &uids[1]]));
    let out = run_outcome(ReviewState::Applied, None, &[]);

    mark_turns(root.ledger(), &item, &out, NOW).unwrap();
    assert_eq!(fact_turn(&root, &uids[0]), ("done".to_string(), None));
    assert_eq!(fact_turn(&root, &uids[1]), ("done".to_string(), None));
    assert_eq!(
        fact_turn(&root, &uids[2]),
        ("empty".to_string(), None),
        "批内有 t3、载荷没草稿：抽取期的 empty 不许被改写"
    );
}

/// `mark_batch` 作用域：同夹具只改载荷覆盖的 t1/t2；t3 保持 `empty`。
#[test]
fn mark_batch_quarantines_only_the_turns_whose_drafts_are_in_the_payload() {
    let (_dir, root, uids) = seeded_fact();
    let id = pending_fact(&root, fact_three_reply());
    reset_fact_turns(&root, &uids[..3]);
    preset_fact_turn(&root, &uids[2], pass_state::PassState::Empty);
    let item = rewrite_payload_items(&root, id, items_for(&[&uids[0], &uids[1]]));

    mark_batch(
        root.ledger(),
        &item,
        pass_state::PassState::Quarantined,
        Some(REVIEW_UNCERTAIN),
        NOW,
    )
    .unwrap();
    assert_eq!(
        fact_turn(&root, &uids[0]),
        ("quarantined".to_string(), Some(REVIEW_UNCERTAIN.to_string()))
    );
    assert_eq!(
        fact_turn(&root, &uids[1]),
        ("quarantined".to_string(), Some(REVIEW_UNCERTAIN.to_string()))
    );
    assert_eq!(
        fact_turn(&root, &uids[2]),
        ("empty".to_string(), None),
        "没有草稿的轮不许被审核失败改写"
    );
}

/// 载荷不可读（`items` 缺 `turn_uid`）⇒ `mark_batch` 回退整批：t3 也被改写（fail-closed）。
#[test]
fn mark_batch_falls_back_to_the_whole_batch_when_the_payload_is_unreadable() {
    let (_dir, root, uids) = seeded_fact();
    let id = pending_fact(&root, fact_three_reply());
    reset_fact_turns(&root, &uids[..3]);
    preset_fact_turn(&root, &uids[2], pass_state::PassState::Empty);
    // 两条草稿位都在，但都没有 turn_uid：拿不到草稿范围 ⇒ 不许只写一部分。
    let item = rewrite_payload_items(&root, id, json!([{"ref": "f1"}, {"ref": "f2"}]));

    mark_batch(
        root.ledger(),
        &item,
        pass_state::PassState::Quarantined,
        Some(REVIEW_UNCERTAIN),
        NOW,
    )
    .unwrap();
    for uid in &uids {
        assert_eq!(
            fact_turn(&root, uid),
            ("quarantined".to_string(), Some(REVIEW_UNCERTAIN.to_string())),
            "{uid}: 载荷不可读时回退整批"
        );
    }
}
