//! H1a：`RunError::Conflict` 的分类（引擎面）。
//!
//! 造法：一条 `pending_review` 的 fact item 交给引擎；审核后端在开口之前**模拟另一个写者**
//! 把它一路推到 `applied`（两条 `record_decision`）。引擎随后记 review 判定时撞上
//! `illegal transition from applied`，必须被分类为 `RunError::Conflict`——不是 `Store`。
//!
//! 断言三件事：变体正确、引擎那条判定整条回滚（不会被误当存储失败）、逐轮 ledger 不动。

use rc_distill::pass_fact::{run_fact, select_fact_batch};
use rc_distill::review::run::{run_item, RunError};
use rc_distill::review::GATE_RULES_VERSION;
use rc_distill::FactFamily;
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::review::{
    record_decision, Decider, Decision, ReviewStage, ReviewState, ReviewVerdict,
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
const MODEL: &str = "fake-review-model";
const PROMISE: &str = "我下周三之前把对账单发给你";

// ── 夹具（与 review_run.rs 同一条：真 Root + 合成转录 + 两个活锚）───────────────

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

fn add_anchor(root: &Root, scope: &Scope, canonical: &str, uid: &str, created_at_ms: i64) {
    put_entity(
        root.memory(),
        &EntityContent {
            kind: "person".into(),
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

fn seeded_fact() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[PROMISE, "好的，收到", "谢谢"], 1_000);
    set_owner(&mut root);
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    add_anchor(&root, &scope, "李四", &uids[2], NOW - 2_000);
    add_anchor(&root, &scope, "张三", &uids[0], NOW - 1_000);
    (dir, root, uids)
}

fn fact_plan(root: &Root) -> rc_distill::pass_fact::FactPlan {
    select_fact_batch(root.transcript(), root.ledger(), root.memory(), 0, FactFamily::Promise, NOW)
        .unwrap()
        .expect("夹具里 3 轮都还没跑 fact/promise")
}

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

fn fact_review() -> String {
    json!({
        "schema_version": 0, "pass": "fact_review", "family": "promise",
        "verdicts": [{"ref": "f1", "verdict": "pass", "code": null, "evidence": "把对账单发给你"}]
    })
    .to_string()
}

/// 回放 backend：按序给回复（脚本耗尽 = `Timeout`）。
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

/// 造一条停在 `pending_review` 的 fact item（抽取成功、审核 `BackendError`）。
fn pending_fact(root: &Root) -> i64 {
    let plan = fact_plan(root);
    let backend = ScriptBackend::new(vec![Ok(fact_reply()), Err(LlmFailure::BackendError)]);
    let err = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW)
        .unwrap_err();
    assert!(matches!(err, rc_distill::DistillError::Llm(LlmFailure::BackendError)), "{err}");
    let (id, state): (i64, String) = root
        .memory()
        .conn()
        .query_row("SELECT id, state FROM review_item", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!(state, "pending_review");
    id
}

fn item_state(m: &MemoryHandle, id: i64) -> String {
    m.conn()
        .query_row("SELECT state FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap()
}

fn count(m: &MemoryHandle, table: &str) -> i64 {
    m.conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn decision(stage: ReviewStage, verdict: ReviewVerdict, decider: Decider, code: &str) -> Decision {
    Decision {
        stage,
        verdict,
        reason_code: code.into(),
        rules_version: GATE_RULES_VERSION.into(),
        model: MODEL.into(),
        prompt_version: String::new(),
        input_sha256: "0".repeat(64),
        review_input_sha256: String::new(),
        decider,
        detail: String::new(),
    }
}

/// 「另一个写者」：在引擎开口之前把 item 推到 applied，然后给一份合法审核输出。
/// 只越权一次：§7.2 起引擎对同一片发 proposer + challenger 两次调用，第二次不再重复记账。
struct RacingBackend<'a> {
    m: &'a MemoryHandle,
    item_id: i64,
    raced: Cell<bool>,
}

impl LlmBackend for RacingBackend<'_> {
    fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
        if !self.raced.replace(true) {
            record_decision(
                self.m,
                self.item_id,
                &decision(ReviewStage::Review, ReviewVerdict::Pass, Decider::Center, "review_ok"),
                NOW + 10,
            )
            .unwrap();
            record_decision(
                self.m,
                self.item_id,
                &decision(ReviewStage::Apply, ReviewVerdict::Pass, Decider::Program, "write_ok"),
                NOW + 11,
            )
            .unwrap();
        }
        Ok(fact_review())
    }
    fn slug(&self) -> Option<&str> {
        Some(MODEL)
    }
}

/// 引擎记判定时撞上"另一写者已推进" ⇒ `RunError::Conflict`（不是 `Store`）；
/// 判定整条回滚、记忆行 0、逐轮 ledger 保持 pending。
#[test]
fn a_racing_writer_turns_the_engine_error_into_conflict() {
    let (_dir, root, uids) = seeded_fact();
    let id = pending_fact(&root);
    let before = count(root.memory(), "review_decision");
    assert_eq!(before, 1, "抽取那次只有 program 判定");

    let race = RacingBackend { m: root.memory(), item_id: id, raced: Cell::new(false) };
    let ledger_before: Vec<(String, Option<String>)> = root
        .ledger()
        .conn()
        .prepare(
            "SELECT state, reason FROM extract_pass_state
              WHERE turn_uid=?1 AND pass_id='fact' AND family='promise'",
        )
        .unwrap()
        .query_map([&uids[0]], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let err = run_item(root.memory(), root.transcript(), root.ledger(), id, &race, NOW)
        .expect_err("另一写者已把状态推到 applied，引擎必须报错");
    match err {
        RunError::Conflict => {}
        RunError::Store(e) => panic!("竞态/非法转换被折成 Store（P0-1）：{e}"),
        other => panic!("必须是 RunError::Conflict，实际：{other:?}"),
    }

    // 「另一写者」的两条判定在，引擎那条 review 判定整条回滚。
    assert_eq!(
        count(root.memory(), "review_decision"),
        before + 2,
        "引擎的冲突判定不许落行"
    );
    assert_eq!(item_state(root.memory(), id), "applied");
    assert_eq!(count(root.memory(), "fact"), 0, "冲突分支一行记忆都不许写");

    // 逐轮 ledger 一格不动（冲突分支不写 ledger）。
    let ledger_after: Vec<(String, Option<String>)> = root
        .ledger()
        .conn()
        .prepare(
            "SELECT state, reason FROM extract_pass_state
              WHERE turn_uid=?1 AND pass_id='fact' AND family='promise'",
        )
        .unwrap()
        .query_map([&uids[0]], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(ledger_after, ledger_before, "Conflict 不许改 ledger");
    assert_eq!(ledger_after.len(), 1);
    assert_eq!(ledger_after[0].0, "pending");

    // 这条 item 的可见状态仍是 Applied（引擎没有把它拨回去）。
    assert_eq!(ReviewState::Applied.slug(), "applied");
}
