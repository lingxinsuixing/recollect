//! S3a-3a：fact/event runner 接进待审队列（设计 `2026-10-26-review-queue-s3` §4/§5/§6/§8）。
//! S3a-3b：naming 也接进同一条队列（本文件末的 naming 段）。
//! S3a-3c：time 也接进同一条队列（本文件末的 time 段）。
//!
//! 断的是**接线**，不是闸门本身（逐码判定在 `review_gate.rs`）：
//! - happy path：程序 pass → 审核 pass → 写库 pass 三段判定各一条，item=`applied`；
//! - 闸失败（tombstone）：记忆 0 行、item=`rejected`、审核模型一次都不开；
//! - 审核全 reject / uncertain / mixed：判定码与逐轮终态；
//! - LLM 失败：item 留在 `pending_review`，只有 program pass；
//! - 幂等：同一载荷重跑只有一个 item、不再调审核；
//! - event：extras 三格进载荷。
//!
//! 夹具风格与 `pass_fact.rs`/`pass_event.rs` 相同：真导入链 + 回放 backend。
//! 正反例只用通用占位名（张三/李四/某项目），不含任何语料专名。

use rc_distill::pass_event::{run_event, select_event_batch, EventOutcome};
use rc_distill::pass_fact::{run_fact, select_fact_batch, FactOutcome};
use rc_distill::pass_naming::{run_naming, select_naming_batch, NamingOutcome};
use rc_distill::pass_prompt::FactFamily;
use rc_distill::pass_time::run_time;
use rc_distill::DistillError;
use rc_ledger::pass_state::{self, PassKey, PassState};
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_fact::{put_fact_rows, FactWriteItem};
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{AnchorRole, BatchCtx, Provenance, Scope, ScopeKind};
use rc_store::{MemoryHandle, Root};
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

mod common;
use common::{adopt_rows, seed_bodies};

const NOW: i64 = 1_700_000_000_000;
const PROMISE: &str = "我下周三之前把对账单发给你";
/// t2 是 **agent** 轮：正文要真写着主语"张三"（旧夹具的"好的，收到"里没有主语名）。
const RECEIVED_BODY: &str = "张三已经收到对账单";
const CONFIG_BODY: &str = "昨晚在机房我把配置改回去，服务就恢复了";
const CONFIG_QUOTE: &str = "昨晚在机房我把配置改回去，服务就恢复了";
/// t2 是 **agent** 轮：发布记录那条也把主语"张三"写进正文（旧夹具是"我把…"，
/// human 口吻挂在 agent 轮上，主语名不在正文里）。
const RELEASE_BODY: &str = "张三今天上午把发布记录更新了";
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

/// fact：3 轮（承诺 + 两句寒暄），张三/李四两个活锚（张三 a1、李四 a2）。
fn seeded_fact() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[PROMISE, RECEIVED_BODY, "谢谢"], 1_000);
    set_owner(&mut root);
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    for (canonical, created_at_ms, turn) in
        [("李四", NOW - 2_000, uids[2].clone()), ("张三", NOW - 1_000, uids[0].clone())]
    {
        put_entity(
            root.memory(),
            &EntityContent {
                kind: "person".into(),
                title: canonical.into(),
                scope: scope.clone(),
                aliases: vec![],
            },
            &[Provenance { turn_uid: turn, anchor_role: AnchorRole::Human }],
            created_at_ms,
        )
        .unwrap();
    }
    adopt_rows(&root, "anchor", "alice", Some("p1"), "project", "project", "proj-x");
    (dir, root, uids)
}

/// event：1 轮（配置恢复），张三一个活锚（a1）。
fn seeded_event() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[CONFIG_BODY], 1_000);
    set_owner(&mut root);
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    put_entity(
        root.memory(),
        &EntityContent {
            kind: "person".into(),
            title: "张三".into(),
            scope,
            aliases: vec![],
        },
        &[Provenance { turn_uid: uids[0].clone(), anchor_role: AnchorRole::Human }],
        NOW - 1_000,
    )
    .unwrap();
    adopt_rows(&root, "anchor", "alice", Some("p1"), "project", "project", "proj-x");
    (dir, root, uids)
}

/// event mixed：两轮（配置恢复 + 更新发布记录），张三一个活锚（a1）。
fn seeded_event_mixed() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[CONFIG_BODY, RELEASE_BODY], 1_000);
    set_owner(&mut root);
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    put_entity(
        root.memory(),
        &EntityContent {
            kind: "person".into(),
            title: "张三".into(),
            scope,
            aliases: vec![],
        },
        &[Provenance { turn_uid: uids[0].clone(), anchor_role: AnchorRole::Human }],
        NOW - 1_000,
    )
    .unwrap();
    adopt_rows(&root, "anchor", "alice", Some("p1"), "project", "project", "proj-x");
    (dir, root, uids)
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

fn fact_review(decisions: &[(&str, &str, Option<&str>)]) -> String {
    let verdicts: Vec<Value> = decisions
        .iter()
        .map(|(reference, verdict, code)| {
            // §7.2：pass/reject 带 quote 内的一段原文（夹具引文都含「对账单」）；uncertain 可 null。
            let evidence = if *verdict == "uncertain" { None } else { Some("对账单") };
            json!({
                "ref": reference, "verdict": verdict, "code": code, "evidence": evidence
            })
        })
        .collect();
    json!({"schema_version": 0, "pass": "fact_review", "family": "promise", "verdicts": verdicts})
        .to_string()
}

/// event 抽取回复：t1 一条 state_change（actor a1=张三），extras 三格由程序现编。
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

/// mixed：t1 一条（审核 pass）+ t2 一条（审核 reject）。
fn event_mixed_reply() -> String {
    json!({
        "schema_version": 0, "pass": "event",
        "events": [{
            "ref": "e1", "turn": "t1", "category": "state_change", "actor": "a1",
            "trigger": "改回去", "action": "restore_config", "object": null, "object_text": "配置文件",
            "participants": ["a1"], "place": null, "result": "done", "mode": "stated",
            "time_text": "昨晚", "quote": CONFIG_QUOTE, "evidence": ["t1"],
            "text": "张三昨晚把配置改回去，服务恢复。"
        }, {
            "ref": "e2", "turn": "t2", "category": "event", "actor": "a1",
            "trigger": "更新", "action": "update_release_log", "object": null, "object_text": "发布记录",
            "participants": ["a1"], "place": null, "result": "done", "mode": "stated",
            "time_text": "今天上午", "quote": RELEASE_BODY, "evidence": ["t2"],
            "text": "张三今天上午更新了发布记录。"
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

/// 回放 backend：按序给回复，记录调用次数（脚本耗尽 = `Timeout`，与回放座位同一条语义）。
struct ScriptBackend {
    replies: RefCell<VecDeque<Result<String, LlmFailure>>>,
    calls: Cell<usize>,
}

impl ScriptBackend {
    fn new(replies: Vec<Result<String, LlmFailure>>) -> Self {
        ScriptBackend { replies: RefCell::new(replies.into()), calls: Cell::new(0) }
    }
    fn two(extraction: String, review: String) -> Self {
        // 一片 fact 审核 = proposer + challenger（§7.2）。
        ScriptBackend::new(vec![Ok(extraction), Ok(review.clone()), Ok(review)])
    }
    /// 只给抽取（若审核被错误地调用，会耗尽脚本得到 `Timeout`，测试当场红）。
    fn extraction_only(extraction: String) -> Self {
        ScriptBackend::new(vec![Ok(extraction)])
    }
}

impl LlmBackend for ScriptBackend {
    fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
        self.calls.set(self.calls.get() + 1);
        self.replies.borrow_mut().pop_front().unwrap_or(Err(LlmFailure::Timeout))
    }
    fn slug(&self) -> Option<&str> {
        Some("fake-review-model")
    }
}

/// 只允许一次调用（抽取）。第二次调用当场 panic：approved/applied 的重试若又问了
/// 审核模型（或任何第二次调用），测试立刻红，而不是靠脚本耗尽得到一个模糊的 Timeout。
struct NoReviewBackend {
    extraction: String,
    calls: Cell<usize>,
}

impl NoReviewBackend {
    fn new(extraction: String) -> Self {
        NoReviewBackend { extraction, calls: Cell::new(0) }
    }
}

impl LlmBackend for NoReviewBackend {
    fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
        let next = self.calls.get() + 1;
        self.calls.set(next);
        assert_eq!(next, 1, "approved/applied 重试只问抽取，不再问审核模型");
        Ok(self.extraction.clone())
    }
    fn slug(&self) -> Option<&str> {
        Some("fake-review-model")
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

/// 测试专用：把 item 从 applied 拨回 approved，模拟"上次写库失败/重试"。
fn force_approved(m: &MemoryHandle, id: i64) {
    m.conn()
        .execute(
            "UPDATE review_item SET state='approved', applied_at_ms=NULL WHERE id=?1",
            [id],
        )
        .unwrap();
    let state: String = m
        .conn()
        .query_row("SELECT state FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap();
    assert_eq!(state, "approved");
}

struct DecRow {
    stage: String,
    verdict: String,
    reason_code: String,
    rules_version: String,
    model: String,
    prompt_version: String,
    input_sha256: String,
    review_input_sha256: String,
    detail: String,
}

fn decisions(m: &MemoryHandle, item_id: i64) -> Vec<DecRow> {
    let mut st = m
        .conn()
        .prepare(
            "SELECT stage, verdict, reason_code, rules_version, model, prompt_version,
                    input_sha256, review_input_sha256, detail
               FROM review_decision WHERE item_id=?1 ORDER BY id",
        )
        .unwrap();
    let rows = st
        .query_map([item_id], |r| {
            Ok(DecRow {
                stage: r.get(0)?,
                verdict: r.get(1)?,
                reason_code: r.get(2)?,
                rules_version: r.get(3)?,
                model: r.get(4)?,
                prompt_version: r.get(5)?,
                input_sha256: r.get(6)?,
                review_input_sha256: r.get(7)?,
                detail: r.get(8)?,
            })
        })
        .unwrap();
    rows.collect::<Result<_, _>>().unwrap()
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

// ── fact ──────────────────────────────────────────────────────────────────────

/// 1. happy path：1 行入库、item=applied、判定链 program/review/apply 各一条，
///    程序行的 model/prompt 为空、审核行带 model/prompt、三段都带 rules_version。
#[test]
fn fact_happy_path_applies_with_a_three_stage_ledger() {
    let (_dir, root, _uids) = seeded_fact();
    let plan = fact_plan(&root);
    let backend = ScriptBackend::two(fact_reply(), fact_review(&[("f1", "pass", None)]));

    let out: FactOutcome = run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
    )
    .unwrap();
    assert_eq!((out.turns, out.drafts, out.written), (3, 1, 1));
    assert_eq!((out.facts, out.cards, out.mentions), (1, 1, 2));
    assert_eq!(out.state, PassState::Done);
    assert_eq!(backend.calls.get(), 3, "抽取一次 + 审核两次（proposer + challenger）");
    assert_eq!(count(root.memory(), "fact"), 1);

    let (id, state, program_code) = only_item(root.memory());
    assert_eq!(state, "applied");
    assert_eq!(program_code, None, "闸 pass 时 program_code 为 NULL");

    let rows = decisions(root.memory(), id);
    assert_eq!(rows.len(), 3, "program / review / apply 各一条");
    assert_eq!(
        (rows[0].stage.as_str(), rows[0].verdict.as_str(), rows[0].reason_code.as_str()),
        ("program", "pass", "program_pass")
    );
    assert_eq!((rows[0].model.as_str(), rows[0].prompt_version.as_str()), ("", ""), "程序行不带模型");
    assert_eq!(
        (rows[1].stage.as_str(), rows[1].verdict.as_str(), rows[1].reason_code.as_str()),
        ("review", "pass", "review_pass")
    );
    assert_eq!(rows[1].model, "fake-review-model");
    assert_eq!(rows[1].prompt_version, "pass-fact-review-v0.5");
    assert_eq!(rows[1].detail, "p=1;r=0;u=0;pass=f1;uncertain=-");
    assert_eq!(
        (rows[2].stage.as_str(), rows[2].verdict.as_str(), rows[2].reason_code.as_str()),
        ("apply", "pass", "applied")
    );
    assert_eq!(rows[2].detail, "facts=1;cards=1;mentions=2");
    assert_eq!(rows[2].model, "fake-review-model");
    for row in &rows {
        assert!(!row.rules_version.is_empty(), "每条判定都要带规则版本");
    }
    // H4b：review 行的 input_sha256 是载荷指纹，送审指纹落在 review_input_sha256；
    // apply 行没有送审输入：input_sha256 是载荷指纹、review_input_sha256 为空。
    assert_eq!(rows[1].input_sha256.len(), 64, "review 行的载荷指纹是 sha256 十六进制");
    assert_eq!(rows[1].review_input_sha256.len(), 64, "送审输入哈希是 sha256 十六进制");
    assert_eq!(rows[2].input_sha256.len(), 64);
    assert_eq!(rows[2].review_input_sha256, "", "apply 没有送审输入");
}

/// 2. 程序闸失败（tombstone）：记忆 0 行、item=rejected、审核模型一次都不开。
///    抽取仍是一次调用（草稿是载荷的原料）；第二次调用被闸挡掉——回放脚本只给抽取一条，
///    若审核被调用会耗尽成 Timeout，测试当场红。
#[test]
fn fact_tombstone_is_rejected_by_the_program_gate_without_a_review_call() {
    let (_dir, root, _uids) = seeded_fact();
    root.transcript()
        .conn()
        .execute(
            "INSERT INTO tombstone(commit_seq, epoch, scope_kind, scope_key, turns_removed,
                                   content_sha256, created_at_ms)
             VALUES(1, 1, 'session', 'sess-batch-A', 1, 'deadbeef', 1)",
            [],
        )
        .unwrap();
    let plan = fact_plan(&root);
    let backend = ScriptBackend::extraction_only(fact_reply());

    let out = run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
    )
    .unwrap();
    assert_eq!(out.state, PassState::Rejected);
    assert_eq!((out.written, out.facts, out.cards, out.mentions), (0, 0, 0, 0));
    assert_eq!(out.drafts, 1, "草稿仍是解析条数");
    assert_eq!(backend.calls.get(), 1, "闸不过不调审核模型（只有抽取那一次）");
    assert_eq!(count(root.memory(), "fact"), 0);

    let (id, state, program_code) = only_item(root.memory());
    assert_eq!(state, "rejected");
    assert_eq!(program_code.as_deref(), Some("turn_tombstoned"));
    let rows = decisions(root.memory(), id);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].stage.as_str(), rows[0].verdict.as_str(), rows[0].reason_code.as_str()),
        ("program", "reject", "turn_tombstoned")
    );
}

/// 3. 审核全 reject：0 行、item=rejected、判定码 `review_reject`、该轮 `rejected(review_reject)`。
#[test]
fn fact_review_reject_writes_nothing_and_rejects_the_item() {
    let (_dir, root, uids) = seeded_fact();
    let plan = fact_plan(&root);
    let backend =
        ScriptBackend::two(fact_reply(), fact_review(&[("f1", "reject", Some("not_durable"))]));

    let out = run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
    )
    .unwrap();
    assert_eq!((out.written, out.rejected, out.uncertain), (0, 1, 0));
    assert_eq!(out.state, PassState::Rejected);
    assert_eq!(count(root.memory(), "fact"), 0);

    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "rejected");
    let rows = decisions(root.memory(), id);
    assert_eq!(rows.len(), 2, "program pass + review reject，没有 apply");
    assert_eq!(rows[1].reason_code, "review_reject");
    assert_eq!(rows[1].detail, "p=0;r=1;u=0;pass=-;uncertain=-");
    assert_eq!(
        pass_state::get(root.ledger(), fact_key(&uids[0])).unwrap().unwrap().reason.as_deref(),
        Some("review_reject")
    );
}

/// 4. 审核 uncertain：0 行、item=quarantined、该轮 `quarantined(review_uncertain)`。
#[test]
fn fact_review_uncertain_quarantines_the_item() {
    let (_dir, root, uids) = seeded_fact();
    let plan = fact_plan(&root);
    let backend = ScriptBackend::two(
        fact_reply(),
        fact_review(&[("f1", "uncertain", Some("quote_mismatch"))]),
    );

    let out = run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
    )
    .unwrap();
    assert_eq!((out.written, out.rejected, out.uncertain), (0, 0, 1));
    assert_eq!(out.state, PassState::Quarantined);
    // W19c2：审核不确定是**质量**隔离，不是"重试耗尽"。日路径的连续隔离墙只认那一格 ——
    // 这里若为 true，一位质量不达标的批就会被当成座位卡死，把整族停掉。
    assert!(!out.retry_exhausted, "审核不确定不许被记成重试耗尽");
    assert_eq!(count(root.memory(), "fact"), 0);

    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "quarantined");
    let rows = decisions(root.memory(), id);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].reason_code, "review_uncertain");
    let r = pass_state::get(root.ledger(), fact_key(&uids[0])).unwrap().unwrap();
    assert_eq!((r.state, r.reason.as_deref()), (PassState::Quarantined, Some("review_uncertain")));
}

/// 5. mixed（1 pass + 1 reject，分属两轮）：只写 pass 那条、item approved→applied、
///    review 判定 `review_mixed`、reject 那一轮仍是 `rejected(review_reject)`。
#[test]
fn fact_mixed_writes_only_the_passing_row() {
    let (_dir, root, uids) = seeded_fact();
    let plan = fact_plan(&root);
    let backend = ScriptBackend::two(
        fact_mixed_reply(),
        fact_review(&[("f1", "pass", None), ("f2", "reject", Some("not_durable"))]),
    );

    let out = run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
    )
    .unwrap();
    assert_eq!((out.drafts, out.written, out.rejected, out.uncertain), (2, 1, 1, 0));
    assert_eq!((out.facts, out.cards, out.mentions), (1, 1, 2));
    assert_eq!(count(root.memory(), "fact"), 1);

    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "applied");
    let rows = decisions(root.memory(), id);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].reason_code, "program_pass");
    assert_eq!(rows[1].reason_code, "review_mixed");
    assert_eq!(rows[1].detail, "p=1;r=1;u=0;pass=f1;uncertain=-");
    assert_eq!(rows[2].reason_code, "applied");
    assert_eq!(rows[2].detail, "facts=1;cards=1;mentions=2");

    // 逐轮终态：pass 的那轮 done，reject 的那轮仍是 review_reject（与 S2 一致）。
    let done = pass_state::get(root.ledger(), fact_key(&uids[0])).unwrap().unwrap();
    assert_eq!((done.state, done.reason.as_deref()), (PassState::Done, None));
    let rejected = pass_state::get(root.ledger(), fact_key(&uids[1])).unwrap().unwrap();
    assert_eq!(
        (rejected.state, rejected.reason.as_deref()),
        (PassState::Rejected, Some("review_reject"))
    );
}

/// 6. 审核单元级失败（超时）：0 行、item 留在 pending_review（只有 program pass）、
///    整批按 `Ok(Pending)` 交回（W19c2：坏的是这一格，不是座位）。
#[test]
fn fact_llm_failure_leaves_the_item_pending() {
    let (_dir, root, _uids) = seeded_fact();
    let plan = fact_plan(&root);
    let backend = ScriptBackend::new(vec![Ok(fact_reply()), Err(LlmFailure::Timeout)]);

    let out = run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
    )
    .unwrap();
    assert_eq!(out.state, PassState::Pending);
    assert_eq!((out.written, out.facts), (0, 0), "落库计数全 0：审核没答");
    // W19c2：还没到上限的一发不是"重试耗尽"。
    assert!(!out.retry_exhausted, "退避中的一批不许预先记成重试耗尽");
    assert_eq!(count(root.memory(), "fact"), 0);

    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "pending_review");
    let rows = decisions(root.memory(), id);
    assert_eq!(rows.len(), 1, "只应有 program pass 判定");
    assert_eq!((rows[0].stage.as_str(), rows[0].verdict.as_str()), ("program", "pass"));
}

/// 6b. 审核座位级失败（传输）：不隔离、按 `Err` 交回；item 仍留在 pending_review（W19c2）。
#[test]
fn fact_backend_failure_is_a_hard_failure_not_a_quarantine() {
    let (_dir, root, uids) = seeded_fact();
    let plan = fact_plan(&root);
    let backend = ScriptBackend::new(vec![Ok(fact_reply()), Err(LlmFailure::BackendError)]);

    let err = run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
    )
    .unwrap_err();
    assert!(matches!(err, DistillError::Llm(LlmFailure::BackendError)), "{err}");
    assert_eq!(count(root.memory(), "fact"), 0);

    let (_id, state, _) = only_item(root.memory());
    assert_eq!(state, "pending_review");
    for uid in &uids {
        let r = pass_state::get(root.ledger(), fact_key(uid)).unwrap().unwrap();
        assert_eq!(r.state, PassState::Pending, "{uid} 座位级失败不许落终态");
        assert!(r.reason.is_none(), "{uid}");
    }
}

/// 6c. 单元级失败连发三次（超时）：前两发 `Ok(Pending)`、第三发 `Ok(Quarantined)`，
///     **只有第三发**带 `retry_exhausted`（W19c2：这是日路径隔离墙唯一认的信号）。
///     与 6b 对照着读：座位级失败永远走 `Err`、一次都不带这一格。
#[test]
fn fact_three_unit_failures_exhaust_and_flag_only_the_last_batch() {
    let (_dir, root, uids) = seeded_fact();
    let plan = fact_plan(&root);

    for round in 1..=2 {
        let backend = ScriptBackend::new(vec![Err(LlmFailure::Timeout)]);
        let out = run_fact(
            root.memory(),
            root.transcript(),
            root.ledger(),
            &plan,
            &backend,
            NOW,
        )
        .unwrap();
        assert_eq!(out.state, PassState::Pending, "第 {round} 发还没到上限");
        assert!(!out.retry_exhausted, "第 {round} 发不是重试耗尽");
        for uid in &uids {
            let r = pass_state::get(root.ledger(), fact_key(uid)).unwrap().unwrap();
            assert_eq!(r.state, PassState::Pending, "{uid} 第 {round} 发");
            assert_eq!(r.attempt, round, "{uid} 的计数没有跨发累积");
        }
    }

    let backend = ScriptBackend::new(vec![Err(LlmFailure::Timeout)]);
    let out = run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
    )
    .unwrap();
    assert_eq!(out.state, PassState::Quarantined, "第三发该到上限");
    assert!(out.retry_exhausted, "第三发必须显式说'重试耗尽'");
    assert_eq!((out.written, out.facts), (0, 0), "隔离的批不许落行");
    for uid in &uids {
        let r = pass_state::get(root.ledger(), fact_key(uid)).unwrap().unwrap();
        assert_eq!((r.state, r.reason.as_deref()), (PassState::Quarantined, Some("retry_exhausted")));
    }
    assert_eq!(count(root.memory(), "fact"), 0);
}

/// 7. 幂等：同输入跑两次 ⇒ 1 个 item、行数不变、第二次不再调审核、不重复记账。
#[test]
fn fact_replay_keeps_one_item_and_does_not_review_again() {
    let (_dir, root, _uids) = seeded_fact();
    let plan = fact_plan(&root);
    let first = ScriptBackend::two(fact_reply(), fact_review(&[("f1", "pass", None)]));
    run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &first, NOW).unwrap();
    let (id, _, _) = only_item(root.memory());
    let facts_before = count(root.memory(), "fact");
    let decisions_before = decisions(root.memory(), id).len();

    // 第二次只给抽取一条：审核若被调用会耗尽脚本成 Timeout。
    let again = ScriptBackend::extraction_only(fact_reply());
    let out = run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &again,
        NOW + 5_000,
    )
    .unwrap();
    assert_eq!(again.calls.get(), 1, "第二次只有抽取，不再调审核");
    assert_eq!(out.state, PassState::Done);
    assert_eq!((out.written, out.duplicate), (0, 1), "已 applied 的载荷按既有去重语义计数");
    assert_eq!((out.facts, out.cards, out.mentions), (0, 0, 0));

    assert_eq!(count(root.memory(), "review_item"), 1, "同一载荷不产生第二个 item");
    assert_eq!(count(root.memory(), "fact"), facts_before, "行数不变");
    assert_eq!(only_item(root.memory()).0, id);
    assert_eq!(decisions(root.memory(), id).len(), decisions_before, "不重复写判定");
}

// ── event ─────────────────────────────────────────────────────────────────────

/// 8. event happy path：extras 三格进载荷且与 items 等长；写 1 行、item=applied、三段判定。
#[test]
fn event_happy_path_applies_with_extras_in_the_payload() {
    let (_dir, root, _uids) = seeded_event();
    let plan = event_plan(&root);
    let backend = ScriptBackend::two(event_reply(), event_review(&[("e1", "pass", None)]));

    let out: EventOutcome = run_event(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
    )
    .unwrap();
    assert_eq!((out.turns, out.drafts, out.written), (1, 1, 1));
    assert_eq!((out.facts, out.cards, out.mentions), (1, 1, 1));
    assert_eq!(out.state, PassState::Done);
    assert_eq!(count(root.memory(), "fact"), 1);

    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "applied");
    let rows = decisions(root.memory(), id);
    assert_eq!(
        rows.iter().map(|r| r.stage.as_str()).collect::<Vec<_>>(),
        vec!["program", "review", "apply"]
    );
    assert_eq!(rows[1].prompt_version, "pass-event-review-v0.4");
    assert_eq!(rows[1].reason_code, "review_pass");
    assert_eq!(rows[2].detail, "facts=1;cards=1;mentions=1");

    // 载荷：items 与 extras 等长，extras 三格都在。
    let payload_json: String = root
        .memory()
        .conn()
        .query_row("SELECT payload_json FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap();
    let payload: Value = serde_json::from_str(&payload_json).unwrap();
    assert_eq!(payload["schema"], "review.pass.v1");
    assert_eq!(payload["pass_id"], "event");
    assert_eq!(payload["family"], "");
    assert_eq!(payload["items"].as_array().unwrap().len(), 1);
    let extras = payload["extras"].as_array().unwrap();
    assert_eq!(extras.len(), payload["items"].as_array().unwrap().len(), "extras 与 items 等长");
    assert!(extras[0].get("participants").unwrap().is_array());
    assert!(extras[0].get("value_json").unwrap().is_object());
    assert!(extras[0].get("place_anchor_id").is_some());
    // `turn_uids_json` 与 `batch.turn_uids` 是同一个数组（闸按排序后对拍）。
    let turn_uids: Value = serde_json::from_str(
        &root
            .memory()
            .conn()
            .query_row("SELECT turn_uids_json FROM review_item WHERE id=?1", [id], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
    )
    .unwrap();
    assert_eq!(turn_uids, payload["batch"]["turn_uids"]);
}

/// 9. event uncertain：0 行、item=quarantined、该轮 `quarantined(review_uncertain)`。
#[test]
fn event_review_uncertain_quarantines_the_item() {
    let (_dir, root, uids) = seeded_event();
    let plan = event_plan(&root);
    let backend = ScriptBackend::two(
        event_reply(),
        event_review(&[("e1", "uncertain", Some("result_unsupported"))]),
    );

    let out = run_event(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
    )
    .unwrap();
    assert_eq!((out.written, out.rejected, out.uncertain), (0, 0, 1));
    assert_eq!(out.state, PassState::Quarantined);
    // W19c2：同上——事件侧的审核不确定也是质量隔离，不是重试耗尽。
    assert!(!out.retry_exhausted, "审核不确定不许被记成重试耗尽");
    assert_eq!(count(root.memory(), "fact"), 0);

    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "quarantined");
    let rows = decisions(root.memory(), id);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].reason_code, "review_uncertain");
    let r = pass_state::get(root.ledger(), event_key(&uids[0])).unwrap().unwrap();
    assert_eq!((r.state, r.reason.as_deref()), (PassState::Quarantined, Some("review_uncertain")));
}

// ── S3a-3a-fix：approved 重试 / applied 重放按逐条判决重建 ─────────────────────

/// 10. fact mixed + approved 重试：先跑 mixed（1 pass + 1 reject）→ applied；把 item
///     拨回 approved 再跑同批 ⇒ 不再问审核模型，只写 pass 那条，reject 草稿没有新行，
///     逐轮终态仍是 done / rejected(review_reject)。
#[test]
fn fact_mixed_approved_retry_writes_only_the_pass_draft() {
    let (_dir, root, uids) = seeded_fact();
    let plan = fact_plan(&root);
    let first = ScriptBackend::two(
        fact_mixed_reply(),
        fact_review(&[("f1", "pass", None), ("f2", "reject", Some("not_durable"))]),
    );
    let out = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &first, NOW).unwrap();
    assert_eq!((out.written, out.rejected), (1, 1));
    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "applied");
    assert_eq!(count(root.memory(), "fact"), 1);

    force_approved(root.memory(), id);

    let again = NoReviewBackend::new(fact_mixed_reply());
    let out = run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &again,
        NOW + 5_000,
    )
    .unwrap();
    assert_eq!(again.calls.get(), 1, "重试只问抽取，不再问审核模型");
    assert_eq!(count(root.memory(), "fact"), 1, "reject 草稿没有新行，pass 行也不重复");
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
    // pass 那条已在库里 ⇒ 重试按既有去重语义计 duplicate，不重复插入。
    assert_eq!((out.written, out.rejected, out.duplicate), (0, 1, 1));
    assert_eq!(out.state, PassState::Done);

    let done = pass_state::get(root.ledger(), fact_key(&uids[0])).unwrap().unwrap();
    assert_eq!((done.state, done.reason.as_deref()), (PassState::Done, None));
    let rejected = pass_state::get(root.ledger(), fact_key(&uids[1])).unwrap().unwrap();
    assert_eq!(
        (rejected.state, rejected.reason.as_deref()),
        (PassState::Rejected, Some("review_reject"))
    );
    assert_eq!(only_item(root.memory()).1, "applied");
}

/// 11. fact mixed + applied 重放：同批再跑一次（已 applied）⇒ reject 轮仍是
///     rejected(review_reject)，不是 Done；行数不变，也不再问审核模型。
#[test]
fn fact_mixed_applied_replay_keeps_rejected_turn_rejected() {
    let (_dir, root, uids) = seeded_fact();
    let plan = fact_plan(&root);
    let first = ScriptBackend::two(
        fact_mixed_reply(),
        fact_review(&[("f1", "pass", None), ("f2", "reject", Some("not_durable"))]),
    );
    run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &first, NOW).unwrap();
    assert_eq!(only_item(root.memory()).1, "applied");
    assert_eq!(count(root.memory(), "fact"), 1);

    let again = NoReviewBackend::new(fact_mixed_reply());
    let out = run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &again,
        NOW + 5_000,
    )
    .unwrap();
    assert_eq!(again.calls.get(), 1, "重放只问抽取，不再问审核模型");
    assert_eq!(count(root.memory(), "fact"), 1, "行数不变");
    assert_eq!((out.written, out.rejected), (0, 1));
    let done = pass_state::get(root.ledger(), fact_key(&uids[0])).unwrap().unwrap();
    assert_eq!((done.state, done.reason.as_deref()), (PassState::Done, None));
    let rejected = pass_state::get(root.ledger(), fact_key(&uids[1])).unwrap().unwrap();
    assert_eq!(
        (rejected.state, rejected.reason.as_deref()),
        (PassState::Rejected, Some("review_reject")),
        "reject 轮不许在 applied 重放里被错标成 Done"
    );
}

/// 12. event mixed + approved 重试：只写 pass 那条，reject 草稿没有新行；逐轮终态正确。
#[test]
fn event_mixed_approved_retry_writes_only_the_pass_event() {
    let (_dir, root, uids) = seeded_event_mixed();
    let plan = event_plan(&root);
    let first = ScriptBackend::two(
        event_mixed_reply(),
        event_review(&[("e1", "pass", None), ("e2", "reject", Some("result_unsupported"))]),
    );
    let out = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &first, NOW).unwrap();
    assert_eq!((out.written, out.rejected), (1, 1));
    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "applied");
    assert_eq!(count(root.memory(), "fact"), 1);

    force_approved(root.memory(), id);

    let again = NoReviewBackend::new(event_mixed_reply());
    let out = run_event(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &again,
        NOW + 5_000,
    )
    .unwrap();
    assert_eq!(again.calls.get(), 1, "重试只问抽取，不再问审核模型");
    // 事件行的 id 是内容寻址：重写同一条不会多一行；reject 草稿一条都不写。
    assert_eq!(count(root.memory(), "fact"), 1, "reject 草稿没有新行");
    assert_eq!((out.rejected, out.uncertain), (1, 0));
    assert_eq!(out.facts, 0, "重试没有插入新行");
    assert_eq!(out.state, PassState::Done);

    let done = pass_state::get(root.ledger(), event_key(&uids[0])).unwrap().unwrap();
    assert_eq!((done.state, done.reason.as_deref()), (PassState::Done, None));
    let rejected = pass_state::get(root.ledger(), event_key(&uids[1])).unwrap().unwrap();
    assert_eq!(
        (rejected.state, rejected.reason.as_deref()),
        (PassState::Rejected, Some("review_reject"))
    );
    assert_eq!(only_item(root.memory()).1, "applied");
}

/// 13. event mixed + applied 重放：reject 轮仍是 rejected(review_reject)，行数不变。
#[test]
fn event_mixed_applied_replay_keeps_rejected_turn_rejected() {
    let (_dir, root, uids) = seeded_event_mixed();
    let plan = event_plan(&root);
    let first = ScriptBackend::two(
        event_mixed_reply(),
        event_review(&[("e1", "pass", None), ("e2", "reject", Some("result_unsupported"))]),
    );
    run_event(root.memory(), root.transcript(), root.ledger(), &plan, &first, NOW).unwrap();
    assert_eq!(only_item(root.memory()).1, "applied");
    assert_eq!(count(root.memory(), "fact"), 1);

    let again = NoReviewBackend::new(event_mixed_reply());
    let out = run_event(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &again,
        NOW + 5_000,
    )
    .unwrap();
    assert_eq!(again.calls.get(), 1, "重放只问抽取，不再问审核模型");
    assert_eq!(count(root.memory(), "fact"), 1, "行数不变");
    // C5b：第一次写下的那条在重放里命中去重 ⇒ 没有行交给写库层，但轮级判决不变。
    assert_eq!((out.written, out.rejected, out.duplicate), (0, 1, 1));
    let done = pass_state::get(root.ledger(), event_key(&uids[0])).unwrap().unwrap();
    assert_eq!((done.state, done.reason.as_deref()), (PassState::Done, None));
    let rejected = pass_state::get(root.ledger(), event_key(&uids[1])).unwrap().unwrap();
    assert_eq!(
        (rejected.state, rejected.reason.as_deref()),
        (PassState::Rejected, Some("review_reject")),
        "reject 轮不许在 applied 重放里被错标成 Done"
    );
}

/// 14. 判决不可读 fail-closed：approved item 的 review 判定 detail 是 `pass=!overflow`
///     ⇒ 一行不写、所有轮 quarantined(review_verdicts_missing)、item 也推 quarantined、
///     返回 Ok（不是 Err）。
#[test]
fn fact_approved_with_unreadable_verdicts_fails_closed() {
    let (_dir, root, uids) = seeded_fact();
    let plan = fact_plan(&root);
    // 先造一条 item：全 reject ⇒ item=rejected、记忆 0 行（这样 fail-closed 的 0 行是绝对的）。
    let first =
        ScriptBackend::two(fact_reply(), fact_review(&[("f1", "reject", Some("not_durable"))]));
    let out = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &first, NOW).unwrap();
    assert_eq!(out.state, PassState::Rejected);
    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "rejected");
    assert_eq!(count(root.memory(), "fact"), 0);

    // 测试专用直改：把 item 拨回 approved，并把 review 判定改成"pass 但逐条判决不可读"。
    force_approved(root.memory(), id);
    root.memory()
        .conn()
        .execute(
            "UPDATE review_decision
                SET verdict='pass', reason_code='review_pass', detail='pass=!overflow'
              WHERE item_id=?1 AND stage='review'",
            [id],
        )
        .unwrap();

    let again = NoReviewBackend::new(fact_reply());
    let out = run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &again,
        NOW + 5_000,
    )
    .unwrap();
    assert_eq!(again.calls.get(), 1, "fail-closed 也不问审核模型");
    assert_eq!(count(root.memory(), "fact"), 0, "一行不写，不退回全 pass");
    assert_eq!((out.written, out.rejected, out.uncertain), (0, 0, 0));
    assert_eq!(out.state, PassState::Quarantined);
    for uid in &uids {
        let rec = pass_state::get(root.ledger(), fact_key(uid)).unwrap().unwrap();
        assert_eq!(
            (rec.state, rec.reason.as_deref()),
            (PassState::Quarantined, Some("review_verdicts_missing")),
            "每一轮都要隔离"
        );
    }
    // H2：返回 Ok，但 item 不再留在 approved——程序侧记一条 `program/uncertain` 把 item
    // 推到 `quarantined`（P0-2 的修复点：remaining/退出码必须看得见这条，不能静默成功）。
    assert_eq!(only_item(root.memory()).1, "quarantined");
}

// ── naming（S3a-3b 接线，S3b-3c 委托引擎）─────────────────────────────────────
//
// naming 的 A1 双跑与噪声闸仍在 runner；闸之后的一切（窄审、判定、写库）由
// `review::run::run_item` 做（S3b-3c）。这一层断的是整条接线：入队 → 程序闸 →
// 窄审（`pass-naming-review-v0.2`）→ 只写 pass 锚。
// 批内正文逐字含规范名/别名：闸的 9c 判据是"canonical 与每个 alias 逐字出现在该批某轮正文，
// 或等于某个 mention 的 surface"。

const NAMING_BODIES: [&str; 3] = [
    "张三说明天把对账单发给你",
    "老张说对账单后天到，张三负责跟进",
    "小结：老张把对账单交给了张三",
];

/// naming：3 轮（都含 `张三`/`老张`）+ 会话归属一行。
fn seeded_naming() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &NAMING_BODIES, 1_000);
    set_owner(&mut root);
    (dir, root, uids)
}

fn naming_plan(root: &Root) -> rc_distill::pass_naming::NamingPlan {
    select_naming_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("夹具里 3 轮都还没跑 naming")
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

/// A1 双跑：两次调用都给同一份回复（闸拒在前时只走到这里）。
fn a1_pair() -> ScriptBackend {
    ScriptBackend::two(a1_reply(), a1_reply())
}

/// naming 窄审回复（S3b-3c）：逐锚判决；`pass` 的 code 必须是显式 null。
fn naming_review(decisions: &[(&str, &str, Option<&str>)]) -> String {
    let verdicts: Vec<Value> = decisions
        .iter()
        .map(|(reference, verdict, code)| json!({"ref": reference, "verdict": verdict, "code": code}))
        .collect();
    json!({"schema_version": 0, "pass": "naming_review", "verdicts": verdicts}).to_string()
}

/// 一次完整的 naming 运行：A1 双跑 + 窄审（闸 pass 后引擎必然开口）。
fn a1_then_review(review: &str) -> ScriptBackend {
    ScriptBackend::new(vec![Ok(a1_reply()), Ok(a1_reply()), Ok(review.to_string())])
}

/// 15. naming happy path：锚/提及入库、item=applied、判定链 program(pass)/review(pass)/
///     apply；程序行不带模型，窄审与 apply 行都带 model/prompt；载荷字段与
///     `gate::parse_naming` 逐字一致。
#[test]
fn naming_happy_path_applies_after_the_naming_review() {
    let (_dir, root, uids) = seeded_naming();
    let plan = naming_plan(&root);
    let backend = a1_then_review(&naming_review(&[("a1", "pass", None)]));

    let out: NamingOutcome =
        run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!((out.turns, out.anchors, out.mentions, out.filtered), (3, 1, 1, 0));
    assert_eq!(out.state, PassState::Done);
    assert_eq!(backend.calls.get(), 3, "A1 双跑 + 窄审一次");
    assert_eq!(count(root.memory(), "anchor"), 1);
    assert_eq!(count(root.memory(), "mention"), 1);

    let (id, state, program_code) = only_item(root.memory());
    assert_eq!(state, "applied");
    assert_eq!(program_code, None, "闸 pass 时 program_code 为 NULL");

    let rows = decisions(root.memory(), id);
    assert_eq!(
        rows.iter().map(|r| r.stage.as_str()).collect::<Vec<_>>(),
        vec!["program", "review", "apply"]
    );
    assert_eq!(
        (rows[0].stage.as_str(), rows[0].verdict.as_str(), rows[0].reason_code.as_str()),
        ("program", "pass", "program_pass")
    );
    assert_eq!((rows[0].model.as_str(), rows[0].prompt_version.as_str()), ("", ""), "程序行不带模型");
    assert_eq!(
        (rows[1].verdict.as_str(), rows[1].reason_code.as_str()),
        ("pass", "review_pass"),
        "闸过之后由命名窄审表态"
    );
    assert_eq!(
        (rows[1].model.as_str(), rows[1].prompt_version.as_str(), rows[1].detail.as_str()),
        ("fake-review-model", "pass-naming-review-v0.2", "p=1;r=0;u=0;pass=a1;uncertain=-"),
        "窄审行带模型与 prompt 版本"
    );
    assert_eq!((rows[2].verdict.as_str(), rows[2].reason_code.as_str()), ("pass", "applied"));
    assert_eq!(rows[2].detail, "anchors=1;mentions=1", "apply 的 detail 用引擎口径（无 filtered=）");
    assert_eq!(
        (rows[2].model.as_str(), rows[2].prompt_version.as_str()),
        ("fake-review-model", "pass-naming-review-v0.2"),
        "apply 行的 model/prompt 来自引擎"
    );
    for row in &rows {
        assert!(!row.rules_version.is_empty(), "每条判定都要带规则版本");
        assert_eq!(row.input_sha256.len(), 64, "载荷 sha256 是十六进制");
    }

    // 载荷：`run_id` 是本批指纹（同输入同 sha），`items` 逐字对上 A1 计划。
    let payload_json: String = root
        .memory()
        .conn()
        .query_row("SELECT payload_json FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap();
    let payload: Value = serde_json::from_str(&payload_json).unwrap();
    assert_eq!(payload["schema"], "review.pass.v1");
    assert_eq!(payload["pass_id"], "naming");
    assert_eq!(payload["family"], "");
    let rec = pass_state::get(root.ledger(), naming_key(&uids[0])).unwrap().unwrap();
    let fp = rec.fingerprint.expect("done 行必须带指纹");
    assert_eq!(payload["run_id"], fp, "载荷 run_id 用本批指纹");

    let anchors = payload["items"]["anchors"].as_array().unwrap();
    assert_eq!(anchors.len(), 1);
    let a = anchors[0].as_object().unwrap();
    assert_eq!(a["reference"], "a1");
    assert_eq!(a["kind"], "person");
    assert_eq!(a["canonical"], "张三");
    assert_eq!(a["aliases"], json!(["老张"]));
    assert_eq!(a["reality_status"], "real");
    assert_eq!(a["stable"], true, "双跑同一个锚 ⇒ stable");
    let evidence = a["evidence"].as_array().unwrap();
    assert_eq!(evidence.len(), 2);
    assert_eq!(evidence[0]["turn_uid"], uids[0].as_str());
    assert_eq!(evidence[0]["anchor_role"], "human", "角色从批内 turns 读，不读模型");
    assert_eq!(evidence[1]["anchor_role"], "agent");

    let mentions = payload["items"]["mentions"].as_array().unwrap();
    assert_eq!(mentions.len(), 1);
    let m = mentions[0].as_object().unwrap();
    assert_eq!(m["anchor_ref"], "a1", "写库层查锚的唯一键（S3b-3b 起在载荷里）");
    assert_eq!(m["surface"], "老张");
    assert_eq!(m["turn_uid"], uids[1].as_str());
    assert_eq!(m["discourse_role"], "mentioned");
    assert_eq!(m["resolution"], "resolved");
    assert_eq!(m["confidence"], 0.9);
    assert!(
        m.get("span_start").is_none() && m.get("span_end").is_none(),
        "A1Mention 没有 span，载荷不编坐标"
    );
    // 四键里的另两键：这份 A1 回复没有 issue/unresolved，载荷照抄空数组。
    assert_eq!(payload["items"]["issues"], json!([]));
    assert_eq!(payload["items"]["unresolved"], json!([]));

    let turn_uids: Value = serde_json::from_str(
        &root
            .memory()
            .conn()
            .query_row("SELECT turn_uids_json FROM review_item WHERE id=?1", [id], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
    )
    .unwrap();
    assert_eq!(turn_uids, payload["batch"]["turn_uids"]);
}

/// 16. 程序闸失败（tombstone 作用域 session）：0 锚、item=rejected、该轮
///     `rejected(turn_tombstoned)`、判定台账只有一条 program reject。
///     锚表 0 行即证 `write_a1_with_owner` 没有被调用。
#[test]
fn naming_tombstoned_batch_is_rejected_before_any_anchor_is_written() {
    let (_dir, root, uids) = seeded_naming();
    root.transcript()
        .conn()
        .execute(
            "INSERT INTO tombstone(commit_seq, epoch, scope_kind, scope_key, turns_removed,
                                   content_sha256, created_at_ms)
             VALUES(1, 1, 'session', 'sess-batch-A', 1, 'deadbeef', 1)",
            [],
        )
        .unwrap();
    let plan = naming_plan(&root);
    let backend = a1_pair();

    let out =
        run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(out.state, PassState::Rejected);
    assert_eq!((out.anchors, out.mentions, out.filtered), (0, 0, 0));
    assert_eq!(backend.calls.get(), 2, "闸拒在前，窄审一次都不开（只有 A1 双跑）");
    for table in ["anchor", "alias", "mention", "provenance", "resource_owner"] {
        assert_eq!(count(root.memory(), table), 0, "{table} 在闸不过的批里留下了行");
    }

    let (id, state, program_code) = only_item(root.memory());
    assert_eq!(state, "rejected");
    assert_eq!(program_code.as_deref(), Some("turn_tombstoned"));
    let rows = decisions(root.memory(), id);
    assert_eq!(rows.len(), 1, "只有 program reject，没有 review/apply");
    assert_eq!(
        (rows[0].stage.as_str(), rows[0].verdict.as_str(), rows[0].reason_code.as_str()),
        ("program", "reject", "turn_tombstoned")
    );
    for uid in &uids {
        let r = pass_state::get(root.ledger(), naming_key(uid)).unwrap().unwrap();
        assert_eq!(
            (r.state, r.reason.as_deref()),
            (PassState::Rejected, Some("turn_tombstoned")),
            "{uid}"
        );
    }
}

/// 17. 幂等：同批重跑 ⇒ 1 个 item、锚数不变、判定不重复、读数按 0 计。
///     第二次是 Applied 早退：只重跑 A1 双跑，不再问窄审。
#[test]
fn naming_replay_keeps_one_item_and_does_not_grow_anchors() {
    let (_dir, root, _uids) = seeded_naming();
    let plan = naming_plan(&root);
    let first = a1_then_review(&naming_review(&[("a1", "pass", None)]));
    run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &first, NOW).unwrap();
    let (id, _, _) = only_item(root.memory());
    let anchors_before = count(root.memory(), "anchor");
    let decisions_before = decisions(root.memory(), id).len();
    assert_eq!((anchors_before, decisions_before), (1, 3));

    let again = a1_pair();
    let out = run_naming(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &again,
        NOW + 5_000,
    )
    .unwrap();
    assert_eq!(again.calls.get(), 2, "Applied 早退只重跑 A1 双跑，不再问窄审");
    assert_eq!(out.state, PassState::Done);
    assert_eq!((out.anchors, out.mentions), (0, 0), "已 applied 的载荷不再写库");
    assert_eq!(count(root.memory(), "review_item"), 1, "同一载荷不产生第二个 item");
    assert_eq!(count(root.memory(), "anchor"), anchors_before, "锚数不变");
    assert_eq!(only_item(root.memory()).0, id);
    assert_eq!(decisions(root.memory(), id).len(), decisions_before, "不重复写判定");
}

/// 18. pending_review 续跑（闸 pass 之后进程退出）：已有 program pass 不重复记，
///     补一次窄审后直接 apply，不砌第二个锚。
#[test]
fn naming_pending_item_resumes_without_a_second_program_decision() {
    let (_dir, root, _uids) = seeded_naming();
    let plan = naming_plan(&root);
    let first = a1_then_review(&naming_review(&[("a1", "pass", None)]));
    run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &first, NOW).unwrap();
    let (id, state, _) = only_item(root.memory());
    assert_eq!(state, "applied");

    root.memory()
        .conn()
        .execute("DELETE FROM review_decision WHERE item_id=?1 AND stage<>'program'", [id])
        .unwrap();
    root.memory()
        .conn()
        .execute(
            "UPDATE review_item SET state='pending_review', applied_at_ms=NULL WHERE id=?1",
            [id],
        )
        .unwrap();

    let again = a1_then_review(&naming_review(&[("a1", "pass", None)]));
    let out = run_naming(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &again,
        NOW + 5_000,
    )
    .unwrap();
    assert_eq!(again.calls.get(), 3, "A1 双跑 + 补一次窄审");
    assert_eq!(out.state, PassState::Done);
    let rows = decisions(root.memory(), id);
    assert_eq!(
        rows.iter().map(|r| r.stage.as_str()).collect::<Vec<_>>(),
        vec!["program", "review", "apply"],
        "已有 program pass 不重复记"
    );
    assert_eq!(only_item(root.memory()).1, "applied");
    assert_eq!(count(root.memory(), "anchor"), 1, "重跑不砌第二个锚");
}

/// 19. approved 重试（上次写库失败）：不再过闸、不再问窄审，按已记下的逐条判决直接 apply。
///     内容寻址保证锚表不多行。
#[test]
fn naming_approved_retry_applies_without_a_new_anchor() {
    let (_dir, root, _uids) = seeded_naming();
    let plan = naming_plan(&root);
    let first = a1_then_review(&naming_review(&[("a1", "pass", None)]));
    run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &first, NOW).unwrap();
    let (id, _, _) = only_item(root.memory());
    force_approved(root.memory(), id);

    let again = a1_pair();
    let out = run_naming(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &again,
        NOW + 5_000,
    )
    .unwrap();
    assert_eq!(again.calls.get(), 2, "approved 重试只重跑 A1，不再问窄审");
    assert_eq!(out.state, PassState::Done);
    assert_eq!(count(root.memory(), "anchor"), 1, "内容寻址：不砌第二个锚");
    assert_eq!(only_item(root.memory()).1, "applied");
    let rows = decisions(root.memory(), id);
    assert_eq!(rows.len(), 4, "program / review / apply / apply（重试）");
    assert_eq!((rows[2].stage.as_str(), rows[3].stage.as_str()), ("apply", "apply"));
    assert_eq!(rows[3].reason_code, "applied");
}

// ── time（S3a-3c）─────────────────────────────────────────────────────────────
//
// time 是确定性解析：程序闸通过即判定通过（S3a 口径，`program_only`），但不许"解析完直接
// 写库"——解析结果先入队，闸通过后才 apply。这里钉四件事：happy path 的三段判定、闸失败
// （墓碑）时 time_expr 一行不改、同批重跑不产生第二个 item、跨会话两行分成两个 item。
//
// time pass 不消费正文：夹具直接落 `sessions`/`turns`（闸只查 origin/会话/墓碑），
// 事实行由写库层落（它同时产出 `time_expr`）。合成时间取真实量级（闸的 2000–2100）。

const TIME_ANCHOR: &str = "00000000000000000000000000000000000000000000000000000000000000c1";
const TIME_UID_A: &str = "0000000000000000000000000000000000000000000000000000000000000041";
const TIME_UID_B: &str = "0000000000000000000000000000000000000000000000000000000000000042";
const TIME_FROM: i64 = NOW - 86_400_000;
const TIME_TEXT: &str = "下周三之前";
const TIME_QUOTE: &str = "我下周三之前把对账单发给你";

/// 一条会话 + 一行源轮（正文只是占位；`body_fp` 要 NOT NULL）。
fn seed_time_turn(
    root: &Root,
    session_key: &str,
    device_id: &str,
    source_agent: &str,
    uid: &str,
) {
    root.transcript()
        .conn()
        .execute(
            "INSERT INTO sessions(session_key, device_id, source_agent)
             VALUES(?1,?2,?3)",
            rusqlite::params![session_key, device_id, source_agent],
        )
        .unwrap();
    root.transcript()
        .conn()
        .execute(
            "INSERT INTO turns(turn_uid, session_key, device_id, role, body, body_fp,
                               turn_key_tier, project_hint, created_at_ms, ingested_at_ms,
                               commit_seq)
             VALUES(?1,?2,?3,'human','time 用例的源句',?4,1,NULL,?5,?5,0)",
            rusqlite::params![uid, session_key, device_id, format!("fp-{uid}"), TIME_FROM],
        )
        .unwrap();
}

fn seed_time_anchor(root: &Root) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind,
                                scope_key, status, profile_text, created_at_ms, updated_at_ms,
                                norm_version)
             VALUES(?1,'person','张三','unknown',NULL,'project','proj-time','active',NULL,?2,?2,?3)",
            rusqlite::params![TIME_ANCHOR, TIME_FROM, rc_store::capability::NORM_VERSION as i64],
        )
        .unwrap();
}

/// 写一条带 `下周三之前` 的 fact（写库层同时落一条未解析的 `time_expr`）。
fn write_time_fact(root: &Root, uid: &str, text: &str) {
    let it = FactWriteItem {
        subject_anchor_id: TIME_ANCHOR.into(),
        object_anchor_id: None,
        predicate: "承诺".into(),
        value: Some("把对账单发出去".into()),
        category: "promise".into(),
        mode: "stated".into(),
        time_text: TIME_TEXT.into(),
        quote: TIME_QUOTE.into(),
        evidence: vec![(uid.into(), AnchorRole::Human)],
        text: text.into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-time".into() },
        source_ms: TIME_FROM,
    };
    let ctx = BatchCtx { source_agent: "agent-time-a".into(), device_id: "dev-time-a".into() };
    let owner = OwnerCtx {
        owner_principal_id: Some("p-time".into()),
        owner_agent_id: "alice-time".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    };
    put_fact_rows(root.memory(), &[it], &ctx, &owner, TIME_FROM).unwrap();
}

/// 单会话夹具：一行源轮、一条 fact、一条 `time_expr`；会话归属七列显式写成 alice/project。
fn seeded_time() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed_time_turn(&root, "sess-time-a", "dev-time-a", "agent-time-a", TIME_UID_A);
    root.transcript()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id='p-time', owner_agent_id='alice-time',
                    visibility='project', retention='keep', scope_kind='project',
                    scope_key='proj-time', ownership_src='operator'
             WHERE session_key='sess-time-a' AND device_id='dev-time-a'",
            [],
        )
        .unwrap();
    seed_time_anchor(&root);
    write_time_fact(&root, TIME_UID_A, "时间 happy：下周三之前发对账单");
    (dir, root)
}

fn count_items(m: &MemoryHandle) -> i64 {
    count(m, "review_item")
}

fn time_expr_row(m: &MemoryHandle, turn_uid: &str) -> (Option<i64>, Option<i64>, String, String) {
    m.conn()
        .query_row(
            "SELECT start_ms, end_ms, basis, kind FROM time_expr WHERE turn_uid=?1",
            [turn_uid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
}

/// 20. happy path：一条可解析 time_expr ⇒ 闸 pass → program_only → apply；item=applied，
///     判定链 program/review/apply；time_expr 被填、fact 的 due 被回填。
#[test]
fn time_happy_path_applies_with_a_three_stage_ledger() {
    let (_dir, root) = seeded_time();
    let out = run_time(root.memory(), root.ledger(), root.transcript(), 0, NOW).unwrap();
    assert_eq!((out.turns, out.expressions, out.resolved, out.facts), (1, 1, 1, 1));
    assert_eq!(out.state, PassState::Done);

    assert_eq!(
        time_expr_row(root.memory(), TIME_UID_A),
        (Some(1_700_582_400_000), Some(1_700_668_799_999), "inferred".into(), "deadline".into()),
        "下周三（2023-11-22）当日末：time_expr 被 CAS 更新",
    );
    let due: Option<i64> = root
        .memory()
        .conn()
        .query_row("SELECT due_at_ms FROM fact WHERE text='时间 happy：下周三之前发对账单'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(due, Some(1_700_668_799_999), "deadline 回填 due_at_ms");

    let (id, state, program_code) = only_item(root.memory());
    assert_eq!(state, "applied");
    assert_eq!(program_code, None);
    let rows = decisions(root.memory(), id);
    assert_eq!(
        rows.iter().map(|r| r.stage.as_str()).collect::<Vec<_>>(),
        vec!["program", "review", "apply"]
    );
    assert_eq!(
        (rows[0].reason_code.as_str(), rows[1].reason_code.as_str()),
        ("program_pass", "program_only"),
        "time 没有模型面：程序闸通过即判定通过"
    );
    assert_eq!(
        (rows[1].model.as_str(), rows[1].prompt_version.as_str(), rows[1].detail.as_str()),
        ("", "", "s3a_program_only")
    );
    assert_eq!(rows[2].detail, "resolved=1;facts=1", "apply 的 detail 只放计数");

    // 载荷：items 的 time_expr_id 是内容寻址主键（TEXT），batch.turn_uids 与 item 同数组。
    let payload_json: String = root
        .memory()
        .conn()
        .query_row("SELECT payload_json FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap();
    let payload: Value = serde_json::from_str(&payload_json).unwrap();
    assert_eq!(payload["pass_id"], "time");
    assert_eq!(payload["family"], "");
    assert_eq!(payload["batch"]["turn_uids"], json!([TIME_UID_A]));
    assert_eq!(payload["batch"]["session_key"], "sess-time-a");
    assert_eq!(payload["batch"]["source_ms"], TIME_FROM);
    assert_eq!(payload["owner"]["owner_agent_id"], "alice-time");
    assert_eq!(payload["owner"]["visibility"], "project");
    assert_eq!(payload["owner"]["scope_key"], "proj-time");
    let item = &payload["items"][0];
    assert_eq!(item["time_expr_id"].as_str().unwrap().len(), 64);
    assert_eq!(item["turn_uid"], TIME_UID_A);
    assert_eq!(item["text"], TIME_TEXT);
    assert_eq!(item["kind"], "deadline");

    let rec = pass_state::get(root.ledger(), time_key(TIME_UID_A)).unwrap().unwrap();
    assert_eq!((rec.state, rec.reason.as_deref()), (PassState::Done, None));
    assert_eq!(payload["run_id"], rec.fingerprint.expect("done 行必须带指纹"));
}

/// 21. 闸失败（tombstone session）：time_expr 仍 `basis='none'`、item=`rejected`、
///     该轮 `rejected(turn_tombstoned)`；判定台账只有 program reject。
#[test]
fn time_tombstoned_batch_is_rejected_before_the_time_expr_is_written() {
    let (_dir, root) = seeded_time();
    root.transcript()
        .conn()
        .execute(
            "INSERT INTO tombstone(commit_seq, epoch, scope_kind, scope_key, turns_removed,
                                   content_sha256, created_at_ms)
             VALUES(1, 1, 'session', 'sess-time-a', 1, 'deadbeef', 1)",
            [],
        )
        .unwrap();

    let out = run_time(root.memory(), root.ledger(), root.transcript(), 0, NOW).unwrap();
    assert_eq!((out.resolved, out.facts), (0, 0));
    assert_eq!(out.state, PassState::Rejected);
    assert_eq!(
        time_expr_row(root.memory(), TIME_UID_A),
        (None, None, "none".into(), "deadline".into()),
        "闸不过 ⇒ time_expr 一行不改（仍待解析）",
    );
    let due: Option<i64> = root
        .memory()
        .conn()
        .query_row("SELECT due_at_ms FROM fact WHERE text='时间 happy：下周三之前发对账单'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(due, None, "闸不过 ⇒ fact 一行不改");

    let (id, state, program_code) = only_item(root.memory());
    assert_eq!(state, "rejected");
    assert_eq!(program_code.as_deref(), Some("turn_tombstoned"));
    let rows = decisions(root.memory(), id);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].stage.as_str(), rows[0].verdict.as_str(), rows[0].reason_code.as_str()),
        ("program", "reject", "turn_tombstoned")
    );
    let rec = pass_state::get(root.ledger(), time_key(TIME_UID_A)).unwrap().unwrap();
    assert_eq!(
        (rec.state, rec.reason.as_deref()),
        (PassState::Rejected, Some("turn_tombstoned"))
    );
}

/// 22. 幂等：同批重跑 ⇒ 1 个 item、time_expr 不再被改（basis 已非 none ⇒ 解析列表为空 ⇒
///     不入队），判定链不重复。
#[test]
fn time_replay_keeps_one_item_and_does_not_touch_the_time_expr() {
    let (_dir, root) = seeded_time();
    run_time(root.memory(), root.ledger(), root.transcript(), 0, NOW).unwrap();
    let (id, _, _) = only_item(root.memory());
    let decisions_before = decisions(root.memory(), id).len();
    let row_before = time_expr_row(root.memory(), TIME_UID_A);
    assert_eq!((count_items(root.memory()), decisions_before), (1, 3));

    let again = run_time(root.memory(), root.ledger(), root.transcript(), 0, NOW + 5_000).unwrap();
    assert_eq!((again.turns, again.expressions, again.resolved, again.facts), (0, 0, 0, 0));
    assert_eq!(again.state, PassState::Empty);
    assert_eq!(count_items(root.memory()), 1, "同一载荷不产生第二个 item");
    assert_eq!(time_expr_row(root.memory(), TIME_UID_A), row_before, "time_expr 不再被改");
    assert_eq!(decisions(root.memory(), id).len(), decisions_before, "不重复写判定");
}

/// 23. 跨会话两行 ⇒ 两个 item（按 session 分组），各自 applied；两轮都 done。
#[test]
fn time_rows_in_two_sessions_become_two_items() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed_time_turn(&root, "sess-time-a", "dev-time-a", "agent-time-a", TIME_UID_A);
    seed_time_turn(&root, "sess-time-b", "dev-time-b", "agent-time-b", TIME_UID_B);
    seed_time_anchor(&root);
    write_time_fact(&root, TIME_UID_A, "时间 A：下周三之前发对账单");
    write_time_fact(&root, TIME_UID_B, "时间 B：下周三之前发对账单");

    let out = run_time(root.memory(), root.ledger(), root.transcript(), 0, NOW).unwrap();
    assert_eq!((out.turns, out.resolved, out.facts), (2, 2, 2));

    assert_eq!(count_items(root.memory()), 2, "两个会话各一个 item");
    let states: Vec<String> = {
        let mut st = root
            .memory()
            .conn()
            .prepare("SELECT state FROM review_item ORDER BY id")
            .unwrap();
        let rows = st.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(states, vec!["applied".to_string(), "applied".to_string()]);
    assert_eq!(time_expr_row(root.memory(), TIME_UID_A).2, "inferred");
    assert_eq!(time_expr_row(root.memory(), TIME_UID_B).2, "inferred");
    for uid in [TIME_UID_A, TIME_UID_B] {
        let rec = pass_state::get(root.ledger(), time_key(uid)).unwrap().unwrap();
        assert_eq!((rec.state, rec.reason.as_deref()), (PassState::Done, None), "{uid}");
    }
}

fn time_key(uid: &str) -> PassKey<'_> {
    PassKey { turn_uid: uid, pass_id: "time", family: "" }
}
