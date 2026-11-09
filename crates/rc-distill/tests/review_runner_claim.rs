//! H1b：三个 runner 的**租约占位**与失败收尾（设计 `H1b-runner-claim`）。
//!
//! 断的是 runner 侧的四件事（引擎/闸门/队列本身各有自己的用例）：
//! 1. 占位被他人持有 ⇒ 只问抽取、不碰引擎；有草稿的轮 `pending(review_pending)`、
//!    写库面全 0、`state=Pending`、item 仍 `pending_review`；
//! 2. 成功后释放：happy path ⇒ 记忆 1 行、item applied、`claimed_by`/`claim_expires_at_ms` 为 NULL；
//! 3. 存储失败不改终态：已 `done` 的轮保持 `done`、没有 `rejected(store)`、失败那条 item 仍 pending；
//! 4. naming 占位：外部持有 claim ⇒ 0 锚、整批轮 `pending(review_pending)`、`state=Pending`。
//!
//! 夹具照 `review_wiring.rs`/`review_no_review.rs`：真库 + 脚本后端，正反例只用通用占位名
//! （张三/李四），不含任何语料专名。

use rc_distill::pass_fact::{run_fact, run_fact_with, select_fact_batch, FactOutcome};
use rc_distill::pass_naming::{run_naming, run_naming_with, select_naming_batch};
use rc_distill::pass_prompt::FactFamily;
use rc_distill::review::ReviewMode;
use rc_ledger::pass_state::{self, PassKey, PassRecord, PassState, REVIEW_PENDING_REASON};
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::review::{claim, release};
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

const NAMING_BODIES: [&str; 3] = [
    "张三说明天把对账单发给你",
    "老张说对账单后天到，张三负责跟进",
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

/// naming：3 轮（都含 `张三`/`老张`）。
fn seeded_naming() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &NAMING_BODIES, 1_000);
    set_owner(&mut root);
    (dir, root, uids)
}

fn fact_plan(root: &Root, family: FactFamily) -> rc_distill::pass_fact::FactPlan {
    select_fact_batch(root.transcript(), root.ledger(), root.memory(), 0, family, NOW)
        .unwrap()
        .expect("夹具里的轮还没跑这一族 fact")
}

fn naming_plan(root: &Root) -> rc_distill::pass_naming::NamingPlan {
    select_naming_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("夹具里 3 轮都还没跑 naming")
}

/// promise 抽取回复：t1 一条（a1=张三、a2=李四），t2/t3 空覆盖。
fn promise_reply() -> String {
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

/// plan 抽取回复：同一批轮、同一组锚，只换 family（第二条 item 的来源）。
fn plan_reply() -> String {
    json!({
        "schema_version": 0, "pass": "fact", "family": "plan",
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a1", "predicate": "plan",
            "object": "a2", "value": "下周三之前", "category": "plan",
            "mode": "stated", "time_text": "下周三之前", "quote": PROMISE,
            "evidence": ["t1"], "text": "张三计划在下周三之前把对账单发给李四。"
        }],
        "empty_turns": ["t2", "t3"]
    })
    .to_string()
}

fn fact_review_pass() -> String {
    json!({
        "schema_version": 0, "pass": "fact_review", "family": "promise",
        "verdicts": [{"ref": "f1", "verdict": "pass", "code": null, "evidence": "把对账单发给你"}]
    })
    .to_string()
}

/// A1 回复：1 个 person 锚（张三/老张）+ 1 条提及；两跑同一份 ⇒ stable。
fn a1_reply() -> String {
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

/// 同一 pass 可能有两条 item（三族 fact 各一条）：按 family 取 id。
fn fact_item_id(m: &MemoryHandle, family: FactFamily) -> i64 {
    m.conn()
        .query_row(
            "SELECT id FROM review_item WHERE pass_id='fact' AND family=?1",
            [family.slug()],
            |r| r.get(0),
        )
        .unwrap()
}

fn item_state(m: &MemoryHandle, id: i64) -> String {
    m.conn()
        .query_row("SELECT state FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap()
}

/// 回放 backend：按序给回复，记录调用次数（脚本耗尽 = `Timeout`，与回放座位同一条语义）。
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
    /// fact 一片 = 抽取 + proposer + challenger（§7.2；两档同形回复）。
    fn fact(extraction: String, review: String) -> Self {
        ScriptBackend::new(vec![Ok(extraction), Ok(review.clone()), Ok(review)])
    }
}

impl LlmBackend for ScriptBackend {
    fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
        self.calls.set(self.calls.get() + 1);
        self.replies.borrow_mut().pop_front().unwrap_or(Err(LlmFailure::Timeout))
    }
    fn slug(&self) -> Option<&str> {
        Some("fake-claim-model")
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
        .query_row("SELECT id, state, claimed_by FROM review_item", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap()
}

fn claim_columns(m: &MemoryHandle, id: i64) -> (Option<String>, Option<i64>) {
    m.conn()
        .query_row(
            "SELECT claimed_by, claim_expires_at_ms FROM review_item WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

fn fact_key<'a>(uid: &'a str, family: FactFamily) -> PassKey<'a> {
    PassKey { turn_uid: uid, pass_id: "fact", family: family.slug() }
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

// ── 1. 占位被他人持有 ────────────────────────────────────────────────────────

/// 先 `EnqueueOnly` 造出 item，再让外部 `claim` 持位，然后 `run_fact`：
/// 只问抽取（脚本只给一条）、不碰引擎；有草稿的轮 `pending(review_pending)`、
/// 记忆 0 行、item 仍 `pending_review`、`state=Pending`。外部释放后两列回 NULL。
#[test]
fn a_held_claim_skips_the_engine_and_keeps_the_draft_turn_pending() {
    let (_dir, root, uids) = seeded_fact();
    let plan = fact_plan(&root, FactFamily::Promise);
    // 先只抽取入队：item 留在 pending_review，闸已有 program pass。
    let setup = ScriptBackend::one(promise_reply());
    run_fact_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &setup,
        NOW,
        ReviewMode::EnqueueOnly,
    )
    .unwrap();
    assert_eq!(setup.calls.get(), 1);
    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "pending_review");

    assert!(
        claim(root.memory(), id, "test-holder", 60_000, NOW).unwrap(),
        "外部写者先占位必须成功"
    );
    assert_eq!(
        claim_columns(root.memory(), id),
        (Some("test-holder".to_string()), Some(NOW + 60_000))
    );

    // 只给抽取一条：审核若被调用会耗尽脚本成 Timeout，测试当场红。
    let backend = ScriptBackend::one(promise_reply());
    let out: FactOutcome =
        run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();

    assert_eq!(backend.calls.get(), 1, "占位被持有：只抽取，不再问审核");
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
        "没抢到租约 = 没写库：七格读数全 0"
    );
    assert_eq!(out.state, PassState::Pending);
    assert_eq!(count(root.memory(), "fact"), 0, "不许写记忆行");

    let (again, state, holder) = only_item(root.memory());
    assert_eq!(again, id);
    assert_eq!(state, "pending_review", "早退不许改 item 状态");
    assert_eq!(holder.as_deref(), Some("test-holder"), "别人的租约不许被清掉");
    assert_awaiting_review(&root, fact_key(&uids[0], FactFamily::Promise));
    for uid in &uids[1..] {
        let r = record(&root, fact_key(uid, FactFamily::Promise));
        assert_eq!(
            (r.state, r.reason.as_deref()),
            (PassState::Empty, None),
            "没有草稿的轮沿用抽取终态，不许留在 running"
        );
    }

    release(root.memory(), id, "test-holder").unwrap();
    assert_eq!(
        claim_columns(root.memory(), id),
        (None, None),
        "release 只清自己的位"
    );
}

// ── 2. 成功后释放 ─────────────────────────────────────────────────────────────

/// happy path（抽取 + 审核 pass）⇒ 记忆 1 行、item applied；成功后两列必须为 NULL。
#[test]
fn a_successful_run_releases_the_claim() {
    let (_dir, root, uids) = seeded_fact();
    let plan = fact_plan(&root, FactFamily::Promise);
    let backend = ScriptBackend::fact(promise_reply(), fact_review_pass());

    let out = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW)
        .unwrap();

    assert_eq!(backend.calls.get(), 3, "抽取 + 两档窄审");
    assert_eq!(out.state, PassState::Done);
    assert_eq!(out.facts, 1);
    let (id, state, holder) = only_item(root.memory());
    assert_eq!(state, "applied");
    assert_eq!(holder, None, "成功后释放：occupied 列回 NULL");
    assert_eq!(
        claim_columns(root.memory(), id),
        (None, None),
        "租约两列都必须清空"
    );
    assert_eq!(count(root.memory(), "fact"), 1);
    let done = record(&root, fact_key(&uids[0], FactFamily::Promise));
    assert_eq!((done.state, done.reason.as_deref()), (PassState::Done, None));
}

// ── 3. Store 失败不改终态 ─────────────────────────────────────────────────────

/// 先跑一条成功（promise 轮 ledger=`done`），再用**第二个连接** `BEGIN IMMEDIATE` 持住
/// memory.db 的写锁，对同一数据目录跑另一条 item（同批的 plan 族）：忙等 `busy_timeout`
/// 后返回 Err。断言：原 `done` 轮仍是 `done`、没有 `rejected(store)`、失败那条 item 仍 pending。
#[test]
fn a_store_failure_leaves_the_done_turn_done() {
    let (dir, root, uids) = seeded_fact();
    // 第一条：promise 成功写库 ⇒ 该轮 ledger=done。
    let promise_plan = fact_plan(&root, FactFamily::Promise);
    let first = ScriptBackend::fact(promise_reply(), fact_review_pass());
    let out = run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &promise_plan,
        &first,
        NOW,
    )
    .unwrap();
    assert_eq!(out.state, PassState::Done);
    let done = record(&root, fact_key(&uids[0], FactFamily::Promise));
    assert_eq!((done.state, done.reason.as_deref()), (PassState::Done, None));

    // 第二条：同批的 plan 族，先入队（item 留在 pending_review），再让存储写失败。
    let plan = fact_plan(&root, FactFamily::Plan);
    let setup = ScriptBackend::one(plan_reply());
    run_fact_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &setup,
        NOW,
        ReviewMode::EnqueueOnly,
    )
    .unwrap();
    let plan_id = fact_item_id(root.memory(), FactFamily::Plan);
    assert_eq!(item_state(root.memory(), plan_id), "pending_review");
    let fact_rows_before = count(root.memory(), "fact");

    // 第二个连接持写锁：runner 的 memory 写会等满 busy_timeout（5s）后撞 SQLITE_BUSY。
    // 不能拿 root.memory() 自己持锁——同一连接会直接进入那个事务，测不到竞争。
    let hold = rusqlite::Connection::open(dir.path().join("memory.db")).unwrap();
    hold.execute_batch("BEGIN IMMEDIATE").unwrap();
    // 另一连接确认锁真的挡住了写（200ms 就放弃，不拖时间）。
    {
        let probe = rusqlite::Connection::open(dir.path().join("memory.db")).unwrap();
        probe
            .busy_timeout(std::time::Duration::from_millis(200))
            .unwrap();
        assert!(
            probe
                .execute("UPDATE review_item SET updated_at_ms=updated_at_ms", [])
                .is_err(),
            "写锁没有生效：另一个连接写成功了"
        );
    }

    let backend = ScriptBackend::one(plan_reply());
    let failed = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW);
    hold.execute_batch("ROLLBACK").unwrap();

    assert!(failed.is_err(), "写锁未释放时这次运行必须失败");
    // 原 done 轮不许被改写。
    let still = record(&root, fact_key(&uids[0], FactFamily::Promise));
    assert_eq!(
        (still.state, still.reason.as_deref()),
        (PassState::Done, None),
        "已写好的 done 必须保持 done"
    );
    assert_eq!(
        root.ledger()
            .conn()
            .query_row(
                "SELECT count(*) FROM extract_pass_state
                  WHERE state='rejected' AND reason='store'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap(),
        0,
        "Store 失败不许写 rejected(store)"
    );
    assert_eq!(count(root.memory(), "fact"), fact_rows_before, "记忆行不增");
    assert_eq!(
        item_state(root.memory(), plan_id),
        "pending_review",
        "失败那条 item 仍 pending"
    );
}

// ── 4. naming 占位 ───────────────────────────────────────────────────────────

/// 外部持有 claim 后跑 `run_naming`：A1 双跑照做，但 0 锚、整批轮
/// `pending(review_pending)`、`state=Pending`，item 仍 `pending_review`。
#[test]
fn a_held_claim_on_naming_marks_every_turn_pending() {
    let (_dir, root, uids) = seeded_naming();
    let plan = naming_plan(&root);
    let setup = ScriptBackend::two(a1_reply(), a1_reply());
    run_naming_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &setup,
        NOW,
        ReviewMode::EnqueueOnly,
    )
    .unwrap();
    assert_eq!(setup.calls.get(), 2);
    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "pending_review");
    assert!(claim(root.memory(), id, "test-holder", 60_000, NOW).unwrap());

    let backend = ScriptBackend::two(a1_reply(), a1_reply());
    let out = run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW)
        .unwrap();

    assert_eq!(backend.calls.get(), 2, "占位被持有：只跑 A1 双跑，不问窄审");
    assert_eq!((out.turns, out.anchors, out.mentions, out.filtered), (3, 0, 0, 0));
    assert_eq!(out.state, PassState::Pending);
    assert_eq!(count(root.memory(), "anchor"), 0, "没抢到租约就不许写锚");
    assert_eq!(only_item(root.memory()).1, "pending_review");
    for uid in &uids {
        assert_awaiting_review(&root, naming_key(uid));
    }

    release(root.memory(), id, "test-holder").unwrap();
    assert_eq!(claim_columns(root.memory(), id), (None, None));
}
