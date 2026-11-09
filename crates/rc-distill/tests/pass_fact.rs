//! S2b-2b：fact pass 的库面（设计 `2026-10-26-s2-prompt-v0` §2.2、§3、§4、§7）。
//!
//! 这一层断六件事：选批按 `(turn, 'fact', family)` 独立表态且继承会话归属/scope；
//! 抽取是一次调用、审核是**另一次**调用（第二次）；只有审核 `pass` 的行写库；
//! `reject` 丢行、`uncertain` 丢行且该轮 `quarantined`；LLM/契约/存储三类失败的落点；
//! 以及重放/换 family 的幂等与隔离。S2e-8b 起多一条：单条坏草稿只丢这一条
//! （`dropped`），覆盖缺口只记 `uncovered`，两种情况都不再折整批；只有信封坏才整批拒。
//! **不**跑真实 LLM、不碰 holdout。
//!
//! 正反例只用通用占位名（张三/李四/某项目），不含任何语料专名。

use rc_distill::pass_fact::{run_fact, run_fact_with, select_fact_batch, FactOutcome};
use rc_distill::pass_prompt::{
    fact_review_system, fact_system_selected, is_compaction_body, FactFamily,
};
use rc_distill::review::ReviewMode;
use rc_distill::{DistillError, FactPassError};
use rc_ledger::pass_state::{self, PassKey, PassRecord, PassState, REVIEW_PENDING_REASON};
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_v4::{ensure_agent_anchor, ensure_principal_anchor};
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{AnchorRole, BatchCtx, Provenance, Scope, ScopeKind};
use rc_store::{LedgerHandle, MemoryHandle, Root};
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, VecDeque};

mod common;
use common::{adopt_rows, insert_anchor_full, seed_bodies};

const NOW: i64 = 1_700_000_000_000;
/// 目标轮正文（`quote` 必须逐字出现在它里面）。
const PROMISE: &str = "我下周三之前把对账单发给你";

/// 夹具：一个会话 3 轮（第 1 轮是人说的承诺，后两轮是寒暄），归属改成
/// `p1/alice/project/keep`、scope 改成 `project/proj-x`（naming 同一行，fact 要继承它）；
/// 再铺两个活锚：张三较新（`a1`）、李四较早（`a2`）。
fn seeded() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[PROMISE, "好的，收到", "谢谢"], 1_000);
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
    // 候选池按会话归属过滤（隐私修复）：夹具的实体锚改挂会话 owner（真实链路由命名 pass 做）。
    adopt_rows(&root, "anchor", "alice", Some("p1"), "project", "project", "proj-x");
    (dir, root, uids)
}

/// 没有活锚的同一份转录（"没有候选时仍可跑，parser 拒"）。
fn seeded_without_candidates() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[PROMISE, "好的，收到", "谢谢"], 1_000);
    (dir, root, uids)
}

fn plan_of(root: &Root, family: FactFamily) -> rc_distill::pass_fact::FactPlan {
    select_fact_batch(root.transcript(), root.ledger(), root.memory(), 0, family, NOW)
        .unwrap()
        .expect("夹具里 3 轮都还没跑这族")
}

fn key_of<'a>(uid: &'a str, family: FactFamily) -> PassKey<'a> {
    PassKey { turn_uid: uid, pass_id: "fact", family: family.slug() }
}

fn record(l: &LedgerHandle, uid: &str, family: FactFamily) -> PassRecord {
    pass_state::get(l, key_of(uid, family))
        .unwrap()
        .unwrap_or_else(|| panic!("{uid} 没有 fact/{} 状态行", family.slug()))
}

fn count(m: &MemoryHandle, table: &str) -> i64 {
    m.conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn anchor_id(m: &MemoryHandle, canonical: &str) -> String {
    m.conn()
        .query_row("SELECT id FROM anchor WHERE canonical=?1", [canonical], |r| r.get(0))
        .unwrap()
}

/// `canonical` 对应的 **person** 锚 id：主人锚与实体锚可能同名，按 kind 定位。
fn person_anchor_id(m: &MemoryHandle, canonical: &str) -> String {
    m.conn()
        .query_row(
            "SELECT id FROM anchor WHERE canonical=?1 AND kind='person'",
            [canonical],
            |r| r.get(0),
        )
        .unwrap()
}

/// 直插一个锚 + 它的归属行（隐私用例的夹具：owner/visibility/scope 由调用方指定。
/// 生产里这两行由命名 pass / `ensure_agent_anchor` 在同一事务里写）。
///
/// 形状只有一份实现（`common::insert_anchor_full`）。这里的默认值就是旧夹具逐字那一份：
/// `person` / `real` / `active`、归属行的设备是会话设备、principal 为空。需要造别的形状
/// （主人锚、休眠锚、合并前的旧锚）时直接调 `insert_anchor_full`，别再复制一份 SQL。
#[allow(clippy::too_many_arguments)]
fn insert_anchor_owned(
    m: &MemoryHandle,
    id: &str,
    canonical: &str,
    owner_agent: &str,
    visibility: &str,
    scope_kind: &str,
    scope_key: &str,
    created_at_ms: i64,
) {
    insert_anchor_full(
        m,
        id,
        canonical,
        "person",
        "real",
        "active",
        owner_agent,
        None,
        Some("dev-batch-A"),
        visibility,
        scope_kind,
        scope_key,
        created_at_ms,
    );
}

/// 抽取回复：t1 一条 promise（subject a1=张三、object a2=李四），t2/t3 进 `empty_turns`。
fn promise_reply() -> String {
    serde_json::json!({
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

/// 同一批、主语换成**主人锚那一格**（a4）：B 语义下"我"只能靠主人锚/说话人章命中，
/// 不能靠"语料里有个也叫张三的人"命中。
fn promise_reply_on_principal() -> String {
    serde_json::json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a4", "predicate": "promise",
            "object": "a2", "value": "把对账单发给你", "category": "promise",
            "mode": "stated", "time_text": "下周三之前", "quote": PROMISE,
            "evidence": ["t1"], "text": "张三承诺在下周三之前把对账单发给李四。"
        }],
        "empty_turns": ["t2", "t3"]
    })
    .to_string()
}

/// 同一批的 decision 变体：谓词/类别换成 decision，正文仍是同一轮（形状合法）。
fn decision_reply() -> String {
    serde_json::json!({
        "schema_version": 0, "pass": "fact", "family": "decision",
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a1", "predicate": "decide",
            "object": null, "value": "把对账单发给你", "category": "decision",
            "mode": "stated", "time_text": "下周三之前", "quote": PROMISE,
            "evidence": ["t1"], "text": "张三决定在下周三之前把对账单发给李四。"
        }],
        "empty_turns": ["t2", "t3"]
    })
    .to_string()
}

/// 合法空批：`facts=[]`，三个目标轮全进 `empty_turns`。
fn empty_reply() -> String {
    serde_json::json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": [], "empty_turns": ["t1", "t2", "t3"]
    })
    .to_string()
}

/// fact 审核回复（两档同形）：pass/reject 必须带 quote 内的连续原文（工单 §7.2）。
/// 夹具引文都是 `PROMISE`（含「对账单」），uncertain 允许 null。
fn review_reply(family: FactFamily, decisions: &[(&str, &str, Option<&str>)]) -> String {
    let verdicts: Vec<serde_json::Value> = decisions
        .iter()
        .map(|(reference, verdict, code)| {
            let evidence = if *verdict == "uncertain" { None } else { Some("对账单") };
            serde_json::json!({
                "ref": reference, "verdict": verdict, "code": code, "evidence": evidence
            })
        })
        .collect();
    serde_json::json!({
        "schema_version": 0, "pass": "fact_review", "family": family.slug(),
        "verdicts": verdicts
    })
    .to_string()
}

fn review_pass() -> String {
    review_reply(FactFamily::Promise, &[("f1", "pass", None)])
}

/// 测试自带的后端：按序回放脚本，记录调用次数（脚本耗尽 = `Timeout`，与回放座位同一条语义）。
struct ScriptBackend {
    replies: RefCell<VecDeque<Result<String, LlmFailure>>>,
    calls: Cell<usize>,
}

impl ScriptBackend {
    fn new(replies: Vec<Result<String, LlmFailure>>) -> Self {
        ScriptBackend { replies: RefCell::new(replies.into()), calls: Cell::new(0) }
    }

    /// 抽取 + 审核两档都成功：proposer 与 challenger 同形，用同一份回复喂两次。
    fn two(extraction: String, review: String) -> Self {
        ScriptBackend::new(vec![Ok(extraction), Ok(review.clone()), Ok(review)])
    }
}

impl LlmBackend for ScriptBackend {
    fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
        self.calls.set(self.calls.get() + 1);
        self.replies.borrow_mut().pop_front().unwrap_or(Err(LlmFailure::Timeout))
    }
    fn slug(&self) -> Option<&str> {
        Some("fake-fact-model")
    }
}

/// `fact` 表的一行关键列（断言面）。
struct FactRow {
    id: String,
    subject_anchor_id: String,
    object_anchor_id: Option<String>,
    predicate: String,
    value_text: Option<String>,
    category: String,
    mode: String,
    confidence: f64,
    text: String,
    evidence_turn_uid: String,
    occurred_at_ms: Option<i64>,
    valid_from_ms: Option<i64>,
    due_at_ms: Option<i64>,
    status: Option<String>,
    scope_kind: String,
    scope_key: String,
    source_agent: String,
    device_id: String,
    created_at_ms: i64,
    participants_json: String,
    speaker_anchor_id: Option<String>,
}

fn fact_row(m: &MemoryHandle) -> FactRow {
    m.conn()
        .query_row(
            "SELECT id, subject_anchor_id, object_anchor_id, predicate, value_text, category,
                    mode, confidence, text, evidence_turn_uid, occurred_at_ms,
                    valid_from_ms, due_at_ms, status, scope_kind, scope_key, source_agent,
                    device_id, created_at_ms, participants_json, speaker_anchor_id
             FROM fact",
            [],
            |r| {
                Ok(FactRow {
                    id: r.get(0)?,
                    subject_anchor_id: r.get(1)?,
                    object_anchor_id: r.get(2)?,
                    predicate: r.get(3)?,
                    value_text: r.get(4)?,
                    category: r.get(5)?,
                    mode: r.get(6)?,
                    confidence: r.get(7)?,
                    text: r.get(8)?,
                    evidence_turn_uid: r.get(9)?,
                    occurred_at_ms: r.get(10)?,
                    valid_from_ms: r.get(11)?,
                    due_at_ms: r.get(12)?,
                    status: r.get(13)?,
                    scope_kind: r.get(14)?,
                    scope_key: r.get(15)?,
                    source_agent: r.get(16)?,
                    device_id: r.get(17)?,
                    created_at_ms: r.get(18)?,
                    participants_json: r.get(19)?,
                    speaker_anchor_id: r.get(20)?,
                })
            },
        )
        .unwrap()
}

/// 归属的一行（fact 批的断言面）。
struct OwnerRow {
    owner_principal_id: Option<String>,
    owner_agent_id: String,
    created_by_agent: String,
    visibility: String,
    retention: String,
    scope_kind: String,
    scope_key: String,
    created_at_ms: i64,
}

fn owner_of(m: &MemoryHandle, table: &str, mem_id: &str) -> OwnerRow {
    m.conn()
        .query_row(
            "SELECT owner_principal_id, owner_agent_id, created_by_agent, visibility, retention,
                    scope_kind, scope_key, created_at_ms
             FROM resource_owner WHERE mem_table=?1 AND mem_id=?2",
            [table, mem_id],
            |r| {
                Ok(OwnerRow {
                    owner_principal_id: r.get(0)?,
                    owner_agent_id: r.get(1)?,
                    created_by_agent: r.get(2)?,
                    visibility: r.get(3)?,
                    retention: r.get(4)?,
                    scope_kind: r.get(5)?,
                    scope_key: r.get(6)?,
                    created_at_ms: r.get(7)?,
                })
            },
        )
        .unwrap()
}

fn assert_no_fact_rows(m: &MemoryHandle) {
    assert_eq!(count(m, "fact"), 0, "fact 在失败批里留下了行");
    assert_eq!(count(m, "card"), 0, "card 在失败批里留下了行");
    let fact_provenance: i64 = m
        .conn()
        .query_row("SELECT count(*) FROM provenance WHERE mem_table='fact'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fact_provenance, 0, "fact 的 provenance 在失败批里留下了行");
    let fact_mentions: i64 = m
        .conn()
        .query_row("SELECT count(*) FROM memory_mention WHERE mem_table='fact'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fact_mentions, 0, "fact 的 mention 在失败批里留下了行");
    assert_eq!(
        m.conn()
            .query_row(
                "SELECT count(*) FROM resource_owner WHERE mem_table IN ('fact','card')",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0,
        "失败批不写 fact/card 的归属"
    );
}

/// 1. 选批：批内 3 轮、ctx/owner/scope 继承会话行、候选带 `a1..aN` 与真实 anchor_id。
#[test]
fn selection_inherits_the_session_owner_scope_and_candidates() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);

    assert_eq!(plan.batch.session_key, "sess-batch-A");
    assert_eq!(plan.batch.device_id, "dev-batch-A");
    assert_eq!(plan.batch.source_agent, "agent-batch-A");
    assert_eq!(plan.ctx.source_agent, "agent-batch-A");
    assert_eq!(plan.ctx.device_id, "dev-batch-A");
    assert_eq!(plan.owner.owner_principal_id.as_deref(), Some("p1"));
    assert_eq!(plan.owner.owner_agent_id, "alice");
    assert_eq!(plan.owner.visibility, Visibility::Project);
    assert_eq!(plan.owner.retention, "keep");
    assert_eq!(plan.scope.kind, ScopeKind::Project);
    assert_eq!(plan.scope.key, "proj-x");
    assert_eq!(plan.family, FactFamily::Promise);
    let got: Vec<String> = plan.batch.rows.iter().map(|r| r.turn_uid.clone()).collect();
    assert_eq!(got, uids, "批内轮次按 created_at_ms, turn_uid 排序");

    assert_eq!(
        plan.candidates.len(),
        4,
        "两个实体锚 + 一个程序自锚（S2e-7）+ 一个主人锚（I2）"
    );
    // 最近创建的在前：张三 a1、李四 a2；id 只给 parser/写库，不在 prompt 里。
    assert_eq!(plan.candidates[0].reference, "a1");
    assert_eq!(plan.candidates[0].kind, "person");
    assert_eq!(plan.candidates[0].canonical, "张三");
    assert_eq!(plan.candidates[0].anchor_id, anchor_id(root.memory(), "张三"));
    assert_eq!(plan.candidates[1].reference, "a2");
    assert_eq!(plan.candidates[1].canonical, "李四");
    assert_eq!(plan.candidates[1].anchor_id, anchor_id(root.memory(), "李四"));
    // 自锚排在原列表之后：`aN | agent | <source_agent>`，它的 id/scope 由程序填。
    assert_eq!(plan.candidates[2].reference, "a3");
    assert_eq!(plan.candidates[2].kind, "agent");
    assert_eq!(plan.candidates[2].canonical, "agent-batch-A", "canonical = 会话的 source_agent");
    assert_eq!(plan.candidates[2].anchor_id, anchor_id(root.memory(), "agent-batch-A"));
    // I2：主人锚排在自锚之后（会话列 `owner_principal_id='p1'`，没有可读名 ⇒ canonical = id）。
    assert_eq!(plan.candidates[3].reference, "a4", "主人锚接在自锚之后，位置连续");
    assert_eq!(plan.candidates[3].kind, "person", "主人锚的 kind 是 person");
    assert_eq!(plan.candidates[3].canonical, "p1", "没有名字时 canonical 回落到 principal_id");
    assert_eq!(plan.candidates[3].anchor_id, person_anchor_id(root.memory(), "p1"));

    // 自锚是程序产物：active/real、scope 是批的 scope、时刻是选批的 now_ms。
    let (kind, reality, status, scope_kind, scope_key, created): (
        String,
        String,
        String,
        String,
        String,
        i64,
    ) = root
        .memory()
        .conn()
        .query_row(
            "SELECT kind, reality_status, status, scope_kind, scope_key, created_at_ms
             FROM anchor WHERE id=?1",
            [plan.candidates[2].anchor_id.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .unwrap();
    assert_eq!((kind.as_str(), reality.as_str(), status.as_str()), ("agent", "real", "active"));
    assert_eq!((scope_kind.as_str(), scope_key.as_str()), ("project", "proj-x"));
    assert_eq!(created, NOW);
    // 锚的归属与 fact 行同一份 OwnerCtx（会话的 effective 归属，不是合成默认）。
    let anchored = owner_of(root.memory(), "anchor", &plan.candidates[2].anchor_id);
    assert_eq!(anchored.owner_principal_id.as_deref(), Some("p1"));
    assert_eq!(anchored.owner_agent_id, "alice");
    assert_eq!(anchored.created_by_agent, "agent-batch-A");
    assert_eq!(anchored.visibility, "project");
    assert_eq!(anchored.retention, "keep");
    assert_eq!(anchored.created_at_ms, NOW, "锚时刻取选批的 now_ms");
}

/// 1b. 候选池的隐私边界（隐私修复的回归用例）。
///
/// A 的私有锚**不进** B 的候选表；同 owner 的锚（夹具里张三/李四已改挂 alice）与
/// 他人但共享可见（visibility=project + scope 全等）的锚仍保留，且顺序仍"最近创建在前"。
/// 修复前这条会红：私有锚没有 owner 过滤，`LIMIT 30` 一取就把它排成 `a1`。
#[test]
fn candidate_pool_excludes_other_owners_private_anchors() {
    let (_dir, root, _uids) = seeded();
    insert_anchor_owned(
        root.memory(),
        "anchor-private-a",
        "A 的私有锚",
        "agent-synthetic-2",
        "private_agent",
        "project",
        "proj-x",
        NOW,
    );
    insert_anchor_owned(
        root.memory(),
        "anchor-shared",
        "共享锚",
        "agent-other",
        "project",
        "project",
        "proj-x",
        NOW - 500,
    );
    // 没有 resource_owner 行的锚：fail-closed，不进任何候选表（即使它比谁都新）。
    root.memory()
        .conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                                status, created_at_ms, updated_at_ms, norm_version)
             VALUES('anchor-orphan', 'person', '无归属锚', 'real', 'project', 'proj-x',
                    'active', ?1, ?1, 1)",
            [NOW],
        )
        .unwrap();
    let plan = plan_of(&root, FactFamily::Promise);
    let canonicals: Vec<&str> =
        plan.candidates.iter().map(|candidate| candidate.canonical.as_str()).collect();
    assert!(
        !canonicals.contains(&"A 的私有锚"),
        "别人的私有锚不得进候选表：{canonicals:?}"
    );
    // 共享可见的锚保留，且按创建时刻排在两个同 owner 实体锚之前。
    assert_eq!(
        canonicals,
        vec!["共享锚", "张三", "李四", "agent-batch-A", "p1"],
        "过滤只删行、不改序；自锚与主人锚仍在末尾（I2 起主人锚接在自锚之后）"
    );
    assert_eq!(plan.candidates[0].reference, "a1");
    assert_eq!(plan.candidates[3].reference, "a4", "位置契约不变");
    assert_eq!(plan.candidates[4].reference, "a5", "主人锚是最后一格");
}

/// 2. 成功批：抽取 + 审核 = 两次调用；审核 pass 的行写五处行，逐列取程序值。
#[test]
fn a_reviewed_pass_writes_fact_provenance_mention_card_and_owner() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);
    let backend = ScriptBackend::two(promise_reply(), review_pass());

    let out: FactOutcome = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(
        (out.turns, out.drafts, out.written, out.rejected, out.uncertain),
        (3, 1, 1, 0, 0)
    );
    assert_eq!((out.facts, out.cards, out.mentions), (1, 1, 2));
    assert_eq!(out.state, PassState::Done);
    assert_eq!(backend.calls.get(), 3, "抽取一次 + 审核两次（proposer + challenger）");

    let row = fact_row(root.memory());
    let zhang = anchor_id(root.memory(), "张三");
    let li = anchor_id(root.memory(), "李四");
    assert_eq!(row.subject_anchor_id, zhang);
    assert_eq!(row.object_anchor_id.as_deref(), Some(li.as_str()));
    assert_eq!(row.predicate, "promise");
    assert_eq!(row.value_text.as_deref(), Some("把对账单发给你"));
    assert_eq!(row.category, "promise");
    assert_eq!(row.mode, "stated");
    assert_eq!(row.confidence, 0.8, "程序盖章的默认置信度");
    assert_eq!(row.text, "张三承诺在下周三之前把对账单发给李四。");
    assert_eq!(row.evidence_turn_uid, uids[0], "首条证据写 evidence_turn_uid");
    assert_eq!(row.occurred_at_ms, None, "时间列等 time pass 回填");
    assert_eq!(row.valid_from_ms, None);
    assert_eq!(row.due_at_ms, None);
    assert_eq!(row.status, None);
    assert_eq!((row.scope_kind.as_str(), row.scope_key.as_str()), ("project", "proj-x"));
    assert_eq!(row.source_agent, "agent-batch-A");
    assert_eq!(row.device_id, "dev-batch-A");
    // 源轮时刻：t1 的 created_at_ms（夹具从 1000 起）。
    assert_eq!(row.created_at_ms, 1_000);
    assert_eq!(
        row.participants_json,
        format!("[\"{zhang}\",\"{li}\"]"),
        "participants = subject 在前、object 在后"
    );

    // 出处：一条 evidence、role 取自转录行（t1 是 human）。
    let (prov_turn, prov_role): (String, String) = root
        .memory()
        .conn()
        .query_row(
            "SELECT turn_uid, anchor_role FROM provenance WHERE mem_table='fact' AND mem_id=?1",
            [row.id.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(prov_turn, uids[0]);
    assert_eq!(prov_role, "human");

    // 提及：subject/object 各一条。
    let mentions: Vec<String> = {
        let conn = root.memory().conn();
        let mut st = conn
            .prepare("SELECT anchor_id FROM memory_mention WHERE mem_id=?1 ORDER BY anchor_id")
            .unwrap();
        let rows = st.query_map([row.id.as_str()], |r| r.get::<_, String>(0)).unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(mentions.len(), 2);
    assert!(mentions.contains(&row.subject_anchor_id));

    // 最小卡：正文同 fact、指针指回 fact、verdict 不冒充已审。
    let (card_text, card_pointers, card_verdict): (String, String, String) = root
        .memory()
        .conn()
        .query_row(
            "SELECT text, pointers_json, verdict FROM card",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(card_text, row.text);
    assert_eq!(card_pointers, format!("[\"fact:{}\"]", row.id));
    // S4：内联窄审 apply 之后，本次写下的这张卡被提升为 `passed`（与 `hub review run`
    // 同一个 helper、同一条口径）；未 apply 的批不会走到这里（见 `an_inline_...` 的反例）。
    assert_eq!(card_verdict, "passed", "内联窄审 applied ⇒ 卡必须被提升");

    // 归属：fact 与 card 各一行，逐列取会话归属（card 的行 id 是另一个内容寻址 id）。
    let card_id: String =
        root.memory().conn().query_row("SELECT id FROM card", [], |r| r.get(0)).unwrap();
    for (table, mem_id) in [("fact", row.id.as_str()), ("card", card_id.as_str())] {
        let owner = owner_of(root.memory(), table, mem_id);
        assert_eq!(owner.owner_principal_id.as_deref(), Some("p1"), "{table}");
        assert_eq!(owner.owner_agent_id, "alice", "{table}");
        assert_eq!(owner.created_by_agent, "agent-batch-A", "{table}");
        assert_eq!(owner.visibility, "project", "{table}");
        assert_eq!(owner.retention, "keep", "{table}");
        assert_eq!((owner.scope_kind.as_str(), owner.scope_key.as_str()), ("project", "proj-x"));
        assert_eq!(owner.created_at_ms, 1_000, "{table} 用源轮时刻");
    }

    // 每轮终态：t1 有通过行 ⇒ done；t2/t3 是合法空覆盖 ⇒ empty。
    let want_run = format!("fact-promise-{NOW}");
    for (uid, want) in [(&uids[0], PassState::Done), (&uids[1], PassState::Empty), (&uids[2], PassState::Empty)] {
        let r = record(root.ledger(), uid, FactFamily::Promise);
        assert_eq!(r.state, want, "{uid}");
        assert!(r.reason.is_none(), "{uid}");
        assert_eq!(r.attempt, 0, "{uid}");
        assert_eq!(r.run_id.as_deref(), Some(want_run.as_str()), "{uid}");
        let fp = r.fingerprint.expect("done/empty 行必须带指纹");
        assert_eq!(fp.len(), 64, "sha256 十六进制");
        assert!(fp.bytes().all(|b| b.is_ascii_hexdigit()), "{fp}");
    }
}

/// S4 的 fact 内联路径（`distill once` / `cycle once` 的默认窄审）：item applied ⇒ 本次
/// 写下的卡必须被提升为 `passed`。这一条钉的是"apply 的每个入口都提升"，不是只有
/// `hub review run` 提升——同一族事实换一条命令落库，判决不能退回 `unreviewed`。
#[test]
fn an_inline_review_promotes_the_card_it_wrote() {
    let (_dir, root, _uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);
    let backend = ScriptBackend::two(promise_reply(), review_pass());
    let out = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW)
        .unwrap();
    assert_eq!((out.facts, out.cards), (1, 1), "夹具前提：这一批真写下一张卡");
    assert_eq!(out.state, PassState::Done);

    // 白盒读数：item 真的 applied，卡真的只有这一张，且它的判决是 passed。
    let (item_state, cards): (String, Vec<String>) = {
        let conn = root.memory().conn();
        let state = conn
            .query_row("SELECT state FROM review_item", [], |r| r.get(0))
            .unwrap();
        let mut st = conn.prepare("SELECT verdict FROM card").unwrap();
        let verdicts = st
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        (state, verdicts)
    };
    assert_eq!(item_state, "applied", "夹具前提：内联窄审真的写库了");
    assert_eq!(cards, vec!["passed".to_string()], "内联窄审 applied ⇒ 卡必须被提升（S4）");
}

/// 3. 重放：成功后选批 None；拿同一份 plan 手动重跑 ⇒ 行数与归属不变，第二次不再写。
///    S2e-6 起同一 (证据轮, value) 由程序判重：重放那条草稿走 `duplicate`，`written=0`。
#[test]
fn a_replay_keeps_row_counts_and_the_owner_unchanged() {
    let (_dir, root, _uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);
    let first = ScriptBackend::two(promise_reply(), review_pass());
    run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &first, NOW).unwrap();
    assert!(
        select_fact_batch(
            root.transcript(),
            root.ledger(),
            root.memory(),
            0,
            FactFamily::Promise,
            NOW
        )
        .unwrap()
        .is_none(),
        "成功的轮已被 fact/promise 状态表态"
    );

    let tables = ["fact", "card", "provenance", "memory_mention", "resource_owner"];
    let before: Vec<i64> = tables.iter().map(|t| count(root.memory(), t)).collect();
    let fact_id: String = root
        .memory()
        .conn()
        .query_row("SELECT id FROM fact", [], |r| r.get(0))
        .unwrap();
    let owner_before = owner_of(root.memory(), "fact", &fact_id);

    let again = ScriptBackend::two(promise_reply(), review_pass());
    let out = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &again, NOW + 5_000).unwrap();
    assert_eq!(out.written, 0, "同 evidence+value 已在库 ⇒ 不再交给写库层");
    assert_eq!(out.duplicate, 1, "重复的那一条要计数");
    assert_eq!((out.rejected, out.uncertain), (0, 0), "去重不是拒收，也不是不确定");
    assert_eq!((out.facts, out.cards, out.mentions), (0, 0, 0), "重放不新增插入行");
    assert_eq!(out.state, PassState::Done, "整批只被去重 ⇒ 仍是已处理");

    let after: Vec<i64> = tables.iter().map(|t| count(root.memory(), t)).collect();
    assert_eq!(before, after, "重放不许长行");
    let owner_after = owner_of(root.memory(), "fact", &fact_id);
    assert_eq!(owner_before.owner_agent_id, owner_after.owner_agent_id);
    assert_eq!(owner_before.created_at_ms, owner_after.created_at_ms, "已有归属不被刷新");
}

/// C5-fix 的夹具：同一轮 + 三个实体锚（主语 张三，两个宾语 李四 / 王五）。
///
/// 候选按 created_at 降序：张三 a1、李四 a2、王五 a3（正文没有点名，不会被证据预筛重排）。
fn seeded_two_objects() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[PROMISE, "好的，收到", "谢谢"], 1_000);
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
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    for (canonical, created_at_ms) in
        [("王五", NOW - 3_000), ("李四", NOW - 2_000), ("张三", NOW - 1_000)]
    {
        put_entity(
            root.memory(),
            &EntityContent {
                kind: "person".into(),
                title: canonical.into(),
                scope: scope.clone(),
                aliases: vec![],
            },
            &[Provenance { turn_uid: uids[0].clone(), anchor_role: AnchorRole::Human }],
            created_at_ms,
        )
        .unwrap();
    }
    adopt_rows(&root, "anchor", "alice", Some("p1"), "project", "project", "proj-x");
    (dir, root, uids)
}

/// C5-fix（P0）：同一轮、同一主语、空 value、**两个不同 object 锚** ⇒ 两条都要落库。
///
/// 旧键是 `(轮, 主语, 类别, 谓词)`，而类别/谓词是族常量 ⇒ 退化成 `(轮, 主语)`，第二条被
/// 静默记成 `duplicate` 丢掉。宾语不同就是两条事（审核 R1 的 P0）。
#[test]
fn two_promises_on_one_turn_with_different_objects_both_land() {
    let (_dir, root, _uids) = seeded_two_objects();
    let plan = plan_of(&root, FactFamily::Promise);
    let reply = json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a1", "predicate": "promise",
            "object": "a2", "value": null, "category": "promise",
            "mode": "stated", "time_text": "下周三之前", "quote": PROMISE,
            "evidence": ["t1"], "text": "张三承诺把对账单发给李四。"
        }, {
            "ref": "f2", "turn": "t1", "subject": "a1", "predicate": "promise",
            "object": "a3", "value": null, "category": "promise",
            "mode": "stated", "time_text": "下周三之前", "quote": PROMISE,
            "evidence": ["t1"], "text": "张三也承诺把对账单发给王五。"
        }],
        "empty_turns": ["t2", "t3"]
    })
    .to_string();
    let review =
        review_reply(FactFamily::Promise, &[("f1", "pass", None), ("f2", "pass", None)]);
    let out = run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &ScriptBackend::two(reply, review),
        NOW,
    )
    .unwrap();

    assert_eq!(
        (out.written, out.duplicate),
        (2, 0),
        "同轮同主语的两条不同承诺都要落库：{out:?}"
    );
    assert_eq!(count(root.memory(), "fact"), 2);
    let objects: Vec<String> = {
        let mut st = root
            .memory()
            .conn()
            .prepare("SELECT object_anchor_id FROM fact")
            .unwrap();
        let got = st
            .query_map([], |r| r.get::<_, Option<String>>(0))
            .unwrap()
            .map(|row| row.unwrap().unwrap_or_default())
            .collect();
        got
    };
    assert_eq!(objects.len(), 2);
    assert_ne!(objects[0], objects[1], "两行挂在不同的宾语锚上");
    assert_eq!(out.state, PassState::Done);
}

/// C5-fix（R1 的 P2）：`value=" "` 的两次写 ⇒ 第二条在**库内**被判重复（不是只在批内）。
///
/// 第二次是同一份载荷、同一份草稿，`seen_in_batch` 是空的 ⇒ 命中只可能来自库内查询。
/// 库里存的是原样的空白（`value_text=' '`），所以库侧比较必须与批内一样先 `trim()`。
#[test]
fn a_blank_value_is_treated_the_same_in_batch_and_in_store() {
    let (_dir, root, _uids) = seeded_two_objects();
    let plan = plan_of(&root, FactFamily::Promise);
    let reply = json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a1", "predicate": "promise",
            "object": "a2", "value": " ", "category": "promise",
            "mode": "stated", "time_text": "下周三之前", "quote": PROMISE,
            "evidence": ["t1"], "text": "张三承诺把对账单发给李四。"
        }],
        "empty_turns": ["t2", "t3"]
    })
    .to_string();

    let first = ScriptBackend::two(reply.clone(), review_pass());
    let out = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &first, NOW).unwrap();
    assert_eq!((out.written, out.duplicate), (1, 0), "第一次真写一行：{out:?}");
    let stored: Option<String> = root
        .memory()
        .conn()
        .query_row("SELECT value_text FROM fact", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored.as_deref(), Some(" "), "库里存的是原样的空白");

    // 第二次：批内集合是空的，唯一可能命中重复的地方是库内。
    let again = ScriptBackend::two(reply, review_pass());
    let out = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &again, NOW + 5_000)
        .unwrap();
    assert_eq!(
        (out.written, out.duplicate),
        (0, 1),
        "空白 value 在库内也要走「没有 value」那一支：{out:?}"
    );
    assert_eq!(count(root.memory(), "fact"), 1, "空白 value 不许落第二行");
    assert_eq!(out.state, PassState::Done, "去重不是失败态");
}

/// 4. 审核 reject：该行不写库、该轮 `rejected(review_reject)`；其余空覆盖轮 `empty`。
#[test]
fn a_rejected_row_is_not_written_and_the_turn_is_rejected() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);
    let review = review_reply(FactFamily::Promise, &[("f1", "reject", Some("not_durable"))]);
    let backend = ScriptBackend::two(promise_reply(), review);

    let out = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(
        (out.turns, out.drafts, out.written, out.rejected, out.uncertain),
        (3, 1, 0, 1, 0)
    );
    assert_eq!((out.facts, out.cards, out.mentions), (0, 0, 0));
    assert_eq!(out.state, PassState::Rejected, "正常拒收不是故障，也不冒充 done");
    assert_eq!(backend.calls.get(), 3, "抽取一次 + 审核两次");

    assert_no_fact_rows(root.memory());
    let r = record(root.ledger(), &uids[0], FactFamily::Promise);
    assert_eq!(r.state, PassState::Rejected);
    assert_eq!(r.reason.as_deref(), Some("review_reject"));
    assert_eq!(record(root.ledger(), &uids[1], FactFamily::Promise).state, PassState::Empty);
    assert_eq!(record(root.ledger(), &uids[2], FactFamily::Promise).state, PassState::Empty);
}

/// 5. 审核 uncertain：丢该行、该轮 `quarantined(review_uncertain)`、零行。
#[test]
fn an_uncertain_verdict_quarantines_the_turn_without_writing() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);
    let review =
        review_reply(FactFamily::Promise, &[("f1", "uncertain", Some("quote_mismatch"))]);
    let backend = ScriptBackend::two(promise_reply(), review);

    let out = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(
        (out.turns, out.drafts, out.written, out.rejected, out.uncertain),
        (3, 1, 0, 0, 1)
    );
    assert_eq!(out.state, PassState::Quarantined);
    assert_no_fact_rows(root.memory());

    let r = record(root.ledger(), &uids[0], FactFamily::Promise);
    assert_eq!(r.state, PassState::Quarantined);
    assert_eq!(r.reason.as_deref(), Some("review_uncertain"));
}

/// 6. 审核单元级故障（超时）：抽取已成功但**一行不写**；每轮 pending + attempt=1 + 60s 退避，
///    整批按 `Ok(Pending)` 交回（W19c2：单元级失败不是这一族的失败）。
#[test]
fn a_review_llm_failure_writes_nothing_and_leaves_every_turn_pending() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);
    let backend =
        ScriptBackend::new(vec![Ok(promise_reply()), Err(LlmFailure::Timeout)]);

    let out = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(out.state, PassState::Pending, "单元级失败按 pending 交回");
    assert_eq!((out.turns, out.written, out.facts), (3, 0, 0), "落库计数全 0");
    assert_eq!(backend.calls.get(), 2, "第二次调用就是 proposer，开口即坏");
    assert_no_fact_rows(root.memory());

    for uid in &uids {
        let r = record(root.ledger(), uid, FactFamily::Promise);
        assert_eq!(r.state, PassState::Pending, "{uid}");
        assert_eq!(r.attempt, 1, "{uid}");
        assert_eq!(r.next_retry_at_ms, Some(NOW + 60_000), "{uid}");
    }
}

/// 6b. 审核座位级故障（传输）：**不隔离**、按 `Err` 交回，每轮仍 pending（W19c2）。
#[test]
fn a_review_backend_failure_is_a_hard_failure_not_a_quarantine() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);
    let backend =
        ScriptBackend::new(vec![Ok(promise_reply()), Err(LlmFailure::BackendError)]);

    let err = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap_err();
    assert!(matches!(err, DistillError::Llm(LlmFailure::BackendError)), "{err}");
    assert_no_fact_rows(root.memory());

    for uid in &uids {
        let r = record(root.ledger(), uid, FactFamily::Promise);
        assert_eq!(r.state, PassState::Pending, "{uid} 座位级失败不许落终态");
        assert!(r.reason.is_none(), "{uid}");
        assert_eq!(r.attempt, 1, "{uid}");
    }
}

/// 7. 抽取 LLM 故障：只开一次口；每轮 pending；零行。
#[test]
fn an_extraction_llm_failure_leaves_every_turn_pending() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);
    let backend = ScriptBackend::new(vec![Err(LlmFailure::BackendError)]);

    let err = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap_err();
    assert!(matches!(err, DistillError::Llm(LlmFailure::BackendError)), "{err}");
    assert_eq!(backend.calls.get(), 1, "抽取就坏了，不再烧审核");

    assert_no_fact_rows(root.memory());
    for uid in &uids {
        let r = record(root.ledger(), uid, FactFamily::Promise);
        assert_eq!(r.state, PassState::Pending, "{uid}");
        assert_eq!(r.attempt, 1, "{uid}");
    }
}

/// 8. 抽取坏 JSON：每轮 `rejected(json)`，零行，零审核调用。
#[test]
fn malformed_extraction_json_rejects_every_turn() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);
    let backend = ScriptBackend::new(vec![Ok("{}".to_string())]);

    let err = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap_err();
    assert!(matches!(err, DistillError::Fact(_)), "{err}");
    assert_eq!(backend.calls.get(), 1);

    assert_no_fact_rows(root.memory());
    for uid in &uids {
        let r = record(root.ledger(), uid, FactFamily::Promise);
        assert_eq!(r.state, PassState::Rejected, "{uid}");
        assert_eq!(r.reason.as_deref(), Some("json"), "{uid}");
    }
}

/// 9. 审核输出坏 JSON：每轮 `quarantined(review_json)`，零行（审核的契约失败单独一格）。
#[test]
fn malformed_review_json_quarantines_every_turn() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);
    let backend = ScriptBackend::two(promise_reply(), "{}".to_string());

    let err = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap_err();
    assert!(matches!(err, DistillError::Fact(_)), "{err}");

    assert_no_fact_rows(root.memory());
    for uid in &uids {
        let r = record(root.ledger(), uid, FactFamily::Promise);
        assert_eq!(r.state, PassState::Quarantined, "{uid}");
        assert_eq!(r.reason.as_deref(), Some("review_json"), "{uid}");
    }
}

/// 10. 空产出：`facts=[]` + empty_turns 覆盖全部 ⇒ 每轮 empty、零行、审核不开口。
#[test]
fn an_empty_reply_is_recorded_as_empty_without_a_review_call() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);
    let backend = ScriptBackend::new(vec![Ok(empty_reply())]);

    let out = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(
        (out.turns, out.drafts, out.written, out.rejected, out.uncertain),
        (3, 0, 0, 0, 0)
    );
    assert_eq!(out.state, PassState::Empty);
    assert_eq!(backend.calls.get(), 1, "没有待审行 ⇒ 不开第二次口");
    assert_no_fact_rows(root.memory());
    for uid in &uids {
        let r = record(root.ledger(), uid, FactFamily::Promise);
        assert_eq!(r.state, PassState::Empty, "{uid}");
        assert!(r.reason.is_none(), "{uid}");
    }
}

/// 11. 两个 family 互不影响：promise done 后 decision 仍可选，状态键不同。
///     decision 落的是同一条 (证据轮, value) ⇒ S2e-6 的跨 family 去重只让它计数、不再写行。
#[test]
fn two_families_keep_separate_state_keys() {
    let (_dir, root, uids) = seeded();
    let promise = plan_of(&root, FactFamily::Promise);
    run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &promise,
        &ScriptBackend::two(promise_reply(), review_pass()),
        NOW,
    )
    .unwrap();
    assert!(
        select_fact_batch(
            root.transcript(),
            root.ledger(),
            root.memory(),
            0,
            FactFamily::Promise,
            NOW
        )
        .unwrap()
        .is_none()
    );

    let decision = select_fact_batch(
        root.transcript(),
        root.ledger(),
        root.memory(),
        0,
        FactFamily::Decision,
        NOW,
    )
    .unwrap()
    .expect("decision 的 3 轮还没表态");
    assert_eq!(
        decision.batch.rows.iter().map(|r| r.turn_uid.clone()).collect::<Vec<_>>(),
        uids
    );
    let review = review_reply(FactFamily::Decision, &[("f1", "pass", None)]);
    let out = run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &decision,
        &ScriptBackend::two(decision_reply(), review),
        NOW + 1_000,
    )
    .unwrap();
    assert_eq!(out.state, PassState::Done);
    assert_eq!(out.duplicate, 1, "同一 (证据轮, value) 的第二个 family 被去重");
    assert_eq!(count(root.memory(), "fact"), 1, "跨 family 重复只留一行（S2e-6）");

    let rows: i64 = root
        .ledger()
        .conn()
        .query_row(
            "SELECT count(*) FROM extract_pass_state WHERE pass_id='fact' AND family='promise'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 3);
    let decision_rows: i64 = root
        .ledger()
        .conn()
        .query_row(
            "SELECT count(*) FROM extract_pass_state WHERE pass_id='fact' AND family='decision'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(decision_rows, 3);
    assert_eq!(record(root.ledger(), &uids[0], FactFamily::Promise).state, PassState::Done);
    assert_eq!(record(root.ledger(), &uids[0], FactFamily::Decision).state, PassState::Done);
    // 空串 family 的 naming 行一条都不存在（状态键不许串族）。
    let naming_rows: i64 = root
        .ledger()
        .conn()
        .query_row(
            "SELECT count(*) FROM extract_pass_state WHERE pass_id='naming'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(naming_rows, 0);
}

/// 12. 没有实体锚：候选里只剩程序自锚（S2e-7），批次仍可跑；模型引用越界的 `a2`
///     只丢这一条（S2e-8b）⇒ `dropped=1`、零行、零审核调用，该轮 `rejected(dropped)`。
#[test]
fn a_batch_with_only_the_agent_anchor_drops_an_out_of_range_subject() {
    let (_dir, root, uids) = seeded_without_candidates();
    let plan = plan_of(&root, FactFamily::Promise);
    assert_eq!(plan.candidates.len(), 1, "唯一候选是程序自锚");
    assert_eq!(plan.candidates[0].kind, "agent");
    assert_eq!(plan.candidates[0].canonical, "agent-batch-A");
    let mut reply: Value = serde_json::from_str(&promise_reply()).unwrap();
    reply["facts"][0]["subject"] = json!("a2");
    let backend = ScriptBackend::new(vec![Ok(reply.to_string())]);

    let out = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(
        (out.turns, out.drafts, out.dropped, out.written, out.uncovered),
        (3, 0, 1, 0, 1),
        "坏主语只丢这一条，不再整批拒"
    );
    assert_eq!(out.state, PassState::Rejected);
    assert_eq!(backend.calls.get(), 1, "没有保留草稿 ⇒ 不开审核的口");
    assert_no_fact_rows(root.memory());
    let r = record(root.ledger(), &uids[0], FactFamily::Promise);
    assert_eq!(r.state, PassState::Rejected);
    assert_eq!(r.reason.as_deref(), Some("unknown_candidate"), "§10.63：写坏在哪一格，不只写 dropped");
    assert_eq!(record(root.ledger(), &uids[1], FactFamily::Promise).state, PassState::Empty);
}

/// 13. 重选判据：未到期的 pending 不选、到期那一刻选、stale 不看时钟；done 不复活。
#[test]
fn selection_re_picks_due_pending_and_stale_turns_only() {
    let (_dir, root, uids) = seeded();
    let l = root.ledger();
    pass_state::note_attempt(l, key_of(&uids[0], FactFamily::Promise), 60_000, NOW).unwrap();
    pass_state::mark_terminal(l, key_of(&uids[1], FactFamily::Promise), PassState::Stale, None, NOW)
        .unwrap();
    pass_state::mark_terminal(l, key_of(&uids[2], FactFamily::Promise), PassState::Done, None, NOW)
        .unwrap();

    let selected = |now_ms: i64, limit: usize| -> Vec<String> {
        select_fact_batch(
            root.transcript(),
            root.ledger(),
            root.memory(),
            limit,
            FactFamily::Promise,
            now_ms,
        )
        .unwrap()
        .map(|p| p.batch.rows.into_iter().map(|r| r.turn_uid).collect())
        .unwrap_or_default()
    };
    assert_eq!(selected(NOW, 0), vec![uids[1].clone()], "退避期内的 pending 不选，stale 选");
    assert_eq!(selected(NOW + 59_999, 0), vec![uids[1].clone()], "差 1ms 仍未到期");
    assert_eq!(
        selected(NOW + 60_000, 0),
        vec![uids[0].clone(), uids[1].clone()],
        "到期那一刻（<=）重选 pending；done 不选"
    );
    assert_eq!(selected(NOW + 60_000, 1), vec![uids[0].clone()], "limit 截会话内前 N");
}

/// 13b. H5a：`pending(review_pending)` 不再被选批重选——那一轮的产出已经在 `review_item`
///      里等 `review run`，再选一次只会重抽一遍、白烧两次调用。
///
///      规则只看 reason 这一格（不看时钟）：把它清成 NULL、退避置到过去 ⇒ 同一条轮又能被选中，
///      证明"不重选"不是指纹或别的列造成的。
#[test]
fn a_review_pending_turn_is_not_reselected_until_the_reason_clears() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);
    let backend = ScriptBackend::new(vec![Ok(promise_reply())]);
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
    assert_eq!(backend.calls.get(), 1, "EnqueueOnly 只问抽取");
    assert_eq!(out.state, PassState::Pending);
    let awaiting = record(root.ledger(), &uids[0], FactFamily::Promise);
    assert_eq!(awaiting.state, PassState::Pending);
    assert_eq!(awaiting.reason.as_deref(), Some(REVIEW_PENDING_REASON));
    assert_eq!(awaiting.next_retry_at_ms, None, "等审核不是等退避");

    let selected = |now_ms: i64| -> Vec<String> {
        select_fact_batch(
            root.transcript(),
            root.ledger(),
            root.memory(),
            0,
            FactFamily::Promise,
            now_ms,
        )
        .unwrap()
        .map(|p| p.batch.rows.into_iter().map(|r| r.turn_uid).collect())
        .unwrap_or_default()
    };
    assert_eq!(
        selected(NOW),
        Vec::<String>::new(),
        "同批只剩这一轮待审：review_pending 不可重选"
    );
    assert_eq!(
        selected(NOW + 3_600_000),
        Vec::<String>::new(),
        "时钟走多远都一样：这一格不看时间"
    );

    // 规则只看 reason：清成 NULL + 退避到期 ⇒ 又能选。
    root.ledger()
        .conn()
        .execute(
            "UPDATE extract_pass_state SET reason=NULL, next_retry_at_ms=?1
              WHERE turn_uid=?2 AND pass_id='fact' AND family='promise'",
            rusqlite::params![NOW - 1, uids[0]],
        )
        .unwrap();
    assert_eq!(
        selected(NOW),
        vec![uids[0].clone()],
        "reason 清空后按退避重选：不可重选的唯一原因是 review_pending"
    );
}

/// 13c（P0-2）. 改判据后旧轮自动可重跑：版本号只由这一族的 system prompt 算。
///   - 同一版本 ⇒ 已 done 的轮不重选（写状态时版本号同步落库）；
///   - 版本非空且不同 ⇒ 旧 done 轮可重选；
///   - NULL（旧库升级的老行）⇒ 不重选。
#[test]
fn a_changed_prompt_version_makes_done_fact_turns_selectable_again() {
    let (_dir, root, uids) = seeded();
    let family = FactFamily::Promise;
    let plan = plan_of(&root, family);
    let backend = ScriptBackend::two(promise_reply(), review_pass());
    run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();

    let current = pass_state::prompt_version(&fact_system_selected(family));
    for (uid, state) in uids.iter().zip([PassState::Done, PassState::Empty, PassState::Empty]) {
        let r = record(root.ledger(), uid, family);
        assert_eq!(r.state, state, "{uid}");
        assert_eq!(r.prompt_version.as_deref(), Some(current.as_str()),
                   "{uid}：开跑必须同步写 system-only 版本号");
    }
    let selected = |now_ms: i64| -> Vec<String> {
        select_fact_batch(root.transcript(), root.ledger(), root.memory(), 0, family, now_ms)
            .unwrap()
            .map(|p| p.batch.rows.into_iter().map(|r| r.turn_uid).collect())
            .unwrap_or_default()
    };
    assert_eq!(selected(NOW + 1), Vec::<String>::new(), "同一版本 ⇒ done 不可重选");

    // 模拟"改了 system prompt 之后重跑"：旧行带的是上一版版本号。
    let next = pass_state::prompt_version("下一版 fact system prompt（判据改了一句）");
    root.ledger()
        .conn()
        .execute(
            "UPDATE extract_pass_state SET prompt_version=?1
              WHERE pass_id='fact' AND family='promise'",
            [next.as_str()],
        )
        .unwrap();
    assert_eq!(selected(NOW + 1), uids, "换了判据 ⇒ 旧 done 轮必须可重选");

    // NULL（升级前的老行）：不重选。
    root.ledger()
        .conn()
        .execute(
            "UPDATE extract_pass_state SET prompt_version=NULL
              WHERE pass_id='fact' AND family='promise'",
            [],
        )
        .unwrap();
    assert_eq!(
        selected(NOW + 1),
        Vec::<String>::new(),
        "NULL 视为不可重选：升级后不自动全量重跑"
    );
}

/// 14a. 一条坏引文只丢那一条（S2e-8b，真实冒烟的形态）：同轮两条草稿，`f2` 的 quote 挪了
///      一个反引号 ⇒ `dropped=1`、保留的 `f1` 照常审核/写库、该轮 `done`。
///      读数关系：`drafts + dropped = 模型原始草稿数`（这里 1 + 1 = 2）。
#[test]
fn one_bad_quote_drops_only_that_draft_and_the_rest_is_written() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);
    let mut reply: Value = serde_json::from_str(&promise_reply()).unwrap();
    let good = reply["facts"][0].clone();
    let mut bad = good.clone();
    bad["ref"] = json!("f2");
    bad["quote"] = json!("我下周三之前把对账单发给你了");
    reply["facts"] = json!([good, bad]);
    let backend = ScriptBackend::two(reply.to_string(), review_pass());

    let out = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(
        (out.turns, out.drafts, out.dropped, out.uncovered),
        (3, 1, 1, 0),
        "一条坏引文只丢一条，其余照常"
    );
    assert_eq!(out.drafts + out.dropped, 2, "drafts + dropped = 原始草稿数");
    assert_eq!((out.written, out.rejected, out.uncertain), (1, 0, 0));
    assert_eq!((out.facts, out.cards, out.mentions), (1, 1, 2));
    assert_eq!(out.state, PassState::Done, "保住的那条写出 ⇒ done");
    assert_eq!(backend.calls.get(), 3, "抽取 + 审核两档（都只喂保留的草稿）");
    assert_eq!(count(root.memory(), "fact"), 1);

    // 每轮终态：t1 的草稿有一条过审 ⇒ done；t2/t3 是 empty_turns 覆盖 ⇒ empty。
    assert_eq!(record(root.ledger(), &uids[0], FactFamily::Promise).state, PassState::Done);
    assert_eq!(record(root.ledger(), &uids[1], FactFamily::Promise).state, PassState::Empty);
    assert_eq!(record(root.ledger(), &uids[2], FactFamily::Promise).state, PassState::Empty);
}

/// 14b. 全部草稿都坏 ⇒ 一条都不留：`drafts=0`、`dropped=2`、零行、零审核调用，
///      该轮 `rejected(dropped)`——不是 `uncovered`：模型开过口，只是草稿被丢。
#[test]
fn a_batch_whose_drafts_are_all_dropped_is_rejected_as_dropped() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);
    let mut reply: Value = serde_json::from_str(&promise_reply()).unwrap();
    let good = reply["facts"][0].clone();
    let mut ellipsis = good.clone();
    ellipsis["ref"] = json!("f2");
    ellipsis["quote"] = json!("我下周三之前…");
    let mut other = good.clone();
    other["ref"] = json!("f3");
    other["quote"] = json!("我下周三之前把对账单发给你了");
    reply["facts"] = json!([ellipsis, other]);
    let backend = ScriptBackend::new(vec![Ok(reply.to_string())]);

    let out = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(
        (out.turns, out.drafts, out.dropped, out.uncovered),
        (3, 0, 2, 1),
        "两条都被丢；t1 没有合法草稿覆盖"
    );
    assert_eq!(out.drafts + out.dropped, 2, "drafts + dropped = 原始草稿数");
    assert_eq!((out.written, out.facts), (0, 0));
    assert_eq!(out.state, PassState::Rejected);
    assert_eq!(backend.calls.get(), 1, "没有保留草稿 ⇒ 不开审核的口");
    assert_no_fact_rows(root.memory());

    let r = record(root.ledger(), &uids[0], FactFamily::Promise);
    assert_eq!(r.state, PassState::Rejected);
    assert_eq!(r.reason.as_deref(), Some("ellipsis"), "§10.63：取第一条坏草稿的码，不写 dropped");
    // §10.63：被丢的批必须留下模型原文，否则事后无法复查坏在哪一格。
    let kept = rc_ledger::pass_state::latest_artifact(
        root.ledger(), "fact", FactFamily::Promise.slug(), "extract").unwrap();
    assert!(kept.is_some(), "dropped>0 ⇒ 落 extract_pass_artifact");
    assert_eq!(record(root.ledger(), &uids[1], FactFamily::Promise).state, PassState::Empty);
    assert_eq!(record(root.ledger(), &uids[2], FactFamily::Promise).state, PassState::Empty);
}

/// 14c. 模型漏一轮（既不给草稿、也不宣告 empty）⇒ 该轮 `rejected(uncovered)`，
///      其余轮照常审核/写库（S2e-8b：覆盖缺口不再折整批）。
#[test]
fn a_turn_the_model_left_out_is_recorded_as_uncovered() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);
    let mut reply: Value = serde_json::from_str(&promise_reply()).unwrap();
    reply["empty_turns"] = json!(["t2"]); // t3 既没草稿也没宣告空
    let backend = ScriptBackend::two(reply.to_string(), review_pass());

    let out = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(
        (out.turns, out.drafts, out.dropped, out.uncovered),
        (3, 1, 0, 1),
        "漏掉的 t3 只计 uncovered，不折整批"
    );
    assert_eq!((out.written, out.facts), (1, 1));
    assert_eq!(out.state, PassState::Done, "有草稿写出 ⇒ done");
    assert_eq!(backend.calls.get(), 3, "抽取 + proposer + challenger");

    assert_eq!(record(root.ledger(), &uids[0], FactFamily::Promise).state, PassState::Done);
    assert_eq!(record(root.ledger(), &uids[1], FactFamily::Promise).state, PassState::Empty);
    let r = record(root.ledger(), &uids[2], FactFamily::Promise);
    assert_eq!(r.state, PassState::Rejected);
    assert_eq!(r.reason.as_deref(), Some("uncovered"), "短 id 已映射回真实 turn_uid");
}

/// 15. 审核的拒收理由同样精确到格：给了一条待审集合外的判决 ⇒ `quarantined(review_unknown_ref)`。
#[test]
fn a_review_failure_records_the_review_cell_as_the_reason() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);
    let review = review_reply(FactFamily::Promise, &[("f9", "pass", None)]);
    let backend = ScriptBackend::two(promise_reply(), review);

    let err = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap_err();
    assert!(matches!(err, DistillError::Fact(FactPassError::UnknownReviewRef(_))), "{err}");

    assert_no_fact_rows(root.memory());
    for uid in &uids {
        let r = record(root.ledger(), uid, FactFamily::Promise);
        assert_eq!(r.state, PassState::Quarantined, "{uid}");
        assert_eq!(r.reason.as_deref(), Some("review_unknown_ref"), "{uid}");
    }
}

/// 16. 解析失败的原文证据（S2e-3）：抽取/审核解析失败时把模型原文的**限长副本**落 ledger
///     `extract_pass_artifact`——一次失败一条（不是每轮一条），`turn_uid` 取批内第一轮，
///     `run_id` 与状态行同值；成功批与没有原文的 LLM 故障一条都不留。
#[test]
fn a_parse_failure_keeps_the_model_text_as_a_ledger_artifact() {
    let artifact_count = |l: &LedgerHandle| -> i64 {
        l.conn()
            .query_row("SELECT count(*) FROM extract_pass_artifact", [], |r| r.get(0))
            .unwrap()
    };

    // ① 抽取坏 JSON（截断的数组，判 `json`）：stage=extract，raw 逐字等于脚本原文。
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);
    let raw = "{\"schema_version\": 0, \"pass\": \"fact\", \"facts\": [".to_string();
    let backend = ScriptBackend::new(vec![Ok(raw.clone())]);
    let err = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap_err();
    assert!(matches!(err, DistillError::Fact(_)), "{err}");
    let a = pass_state::latest_artifact(root.ledger(), "fact", "promise", "extract")
        .unwrap()
        .expect("抽取拒收必须留下原文");
    assert_eq!(a.pass_id, "fact");
    assert_eq!(a.family, "promise");
    assert_eq!(a.stage, "extract");
    assert_eq!(a.run_id, format!("fact-promise-{NOW}"), "run_id 与状态行同值");
    assert_eq!(a.turn_uid, uids[0], "turn_uid 取批内第一轮");
    assert_eq!(a.raw, raw, "artifact 里的原文必须逐字等于模型回复");
    assert_eq!(a.created_at_ms, NOW);
    assert_eq!(artifact_count(root.ledger()), 1, "一次失败一条（不是每轮一条）");
    assert!(
        pass_state::latest_artifact(root.ledger(), "fact", "promise", "review").unwrap().is_none(),
        "抽取失败不写 review 格"
    );

    // ② 审核坏 JSON：stage=review，raw 是第二次调用的回复，不是抽取的那份。
    let (_dir2, root2, uids2) = seeded();
    let plan2 = plan_of(&root2, FactFamily::Promise);
    let review_raw = "{\"schema_version\": 0, \"verdicts\": [".to_string();
    let backend2 = ScriptBackend::two(promise_reply(), review_raw.clone());
    let err = run_fact(root2.memory(), root2.transcript(), root2.ledger(), &plan2, &backend2, NOW).unwrap_err();
    assert!(matches!(err, DistillError::Fact(_)), "{err}");
    let a = pass_state::latest_artifact(root2.ledger(), "fact", "promise", "review")
        .unwrap()
        .expect("审核拒收必须留下原文");
    assert_eq!(a.stage, "review");
    assert_eq!(a.raw, review_raw);
    assert_eq!(a.run_id, format!("fact-promise-{NOW}"));
    assert_eq!(a.turn_uid, uids2[0]);
    assert_eq!(
        artifact_count(root2.ledger()),
        2,
        "proposer 与 challenger 各留一条（两次都坏）"
    );
    assert!(
        pass_state::latest_artifact(root2.ledger(), "fact", "promise", "extract").unwrap().is_none(),
        "审核失败不补写抽出格"
    );

    // ③ 成功批：不存成功输出。
    let (_dir3, root3, _uids3) = seeded();
    let plan3 = plan_of(&root3, FactFamily::Promise);
    run_fact(
        root3.memory(),
        root3.transcript(),
        root3.ledger(),
        &plan3,
        &ScriptBackend::two(promise_reply(), review_pass()),
        NOW,
    )
    .unwrap();
    assert_eq!(artifact_count(root3.ledger()), 0, "成功批不留原文");

    // ④ LLM 故障：没有原文可存，不是"解析失败"（单元级超时按 pending 交回，不是 Err）。
    let (_dir4, root4, _uids4) = seeded();
    let plan4 = plan_of(&root4, FactFamily::Promise);
    let backend4 = ScriptBackend::new(vec![Err(LlmFailure::Timeout)]);
    let out4 = run_fact(root4.memory(), root4.transcript(), root4.ledger(), &plan4, &backend4, NOW).unwrap();
    assert_eq!(out4.state, PassState::Pending, "超时是单元级失败");
    assert_eq!(artifact_count(root4.ledger()), 0, "运维故障不写 artifact");
}

// ── S2e-6：错主语 / 跨 family 去重 / 压缩块隔离 ───────────────────────────────

/// 一族一行的草稿：quote 仍是 `PROMISE` 的原文（逐字），只有 value/text 由调用方换。
fn family_reply(family: FactFamily, value: &str, text: &str) -> String {
    serde_json::json!({
        "schema_version": 0, "pass": "fact", "family": family.slug(),
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a1", "predicate": family.predicate(),
            "object": "a2", "value": value, "category": family.category(),
            "mode": "stated", "time_text": "下周三之前", "quote": PROMISE,
            "evidence": ["t1"], "text": text
        }],
        "empty_turns": ["t2", "t3"]
    })
    .to_string()
}

/// 压缩摘要块三种通用形状之一：旧摘要的英文前缀（大小写不敏感）。
const CONTINUED_BODY: &str =
    "This session is being continued from a previous conversation that ran out of context.";

/// 通用形状之三：自动 checkpoint 的起句（适配器生成的样板话，不含任何一次会话的私有内容）。
const CHECKPOINT_BODY: &str = "This is an automatically generated checkpoint condensing an \
    earlier span of the conversation to free up context. Treat the captured context as \
    established background and build on it without restating it.";

/// 夹具：t1 是正经内容、t2 是压缩摘要块、t3 是寒暄（选批只该剩下 t1/t3）。
fn seeded_with_compaction() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[PROMISE, CONTINUED_BODY, "谢谢"], 1_000);
    (dir, root, uids)
}

/// 17. 错主语（S2e-6）：审核给 `reject + wrong_subject` ⇒ 该行不写、该轮 `rejected(review_reject)`。
///     这是冒烟发现 1 的落点："项目记忆/阶段台账"当行动主体的行必须被程序侧挡在库外。
#[test]
fn a_wrong_subject_rejection_writes_nothing_and_rejects_the_turn() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root, FactFamily::Decision);
    let review =
        review_reply(FactFamily::Decision, &[("f1", "reject", Some("wrong_subject"))]);
    let backend = ScriptBackend::two(decision_reply(), review);

    let out = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(
        (out.turns, out.drafts, out.written, out.duplicate, out.rejected, out.uncertain),
        (3, 1, 0, 0, 1, 0)
    );
    assert_eq!((out.facts, out.cards, out.mentions), (0, 0, 0));
    assert_eq!(out.state, PassState::Rejected);
    assert_eq!(backend.calls.get(), 3, "抽取 + 审核两档");
    assert_no_fact_rows(root.memory());

    let r = record(root.ledger(), &uids[0], FactFamily::Decision);
    assert_eq!(r.state, PassState::Rejected);
    assert_eq!(r.reason.as_deref(), Some("review_reject"));
}

/// 18. 审核 prompt 的**唯一问题**明确问"是谁做的"：来源/文档/记忆库不是行动主体。
///     字面断言关键子串——判定词不在文本里，模型就没有被问过这件事。
#[test]
fn the_fact_review_prompt_asks_who_did_it() {
    for family in FactFamily::ALL {
        let sys = fact_review_system(family);
        for needle in ["WHO DID IT", "rather than a memory store", "wrong_subject"] {
            assert!(sys.contains(needle), "审核 system 缺 `{needle}`：{sys}");
        }
    }
}

/// 19. 跨 family 去重（S2e-6）：promise 已落 (t1, 把对账单发给你)，
///     plan 落同 evidence+value ⇒ 不写第二行、`duplicate=1`、该轮仍是 done。
#[test]
fn a_second_family_with_the_same_evidence_and_value_is_only_counted() {
    let (_dir, root, uids) = seeded();
    let promise = plan_of(&root, FactFamily::Promise);
    run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &promise,
        &ScriptBackend::two(promise_reply(), review_pass()),
        NOW,
    )
    .unwrap();
    assert_eq!(count(root.memory(), "fact"), 1, "第一族先落一行");

    let plan = select_fact_batch(
        root.transcript(),
        root.ledger(),
        root.memory(),
        0,
        FactFamily::Plan,
        NOW,
    )
    .unwrap()
    .expect("plan 的三轮还没表态");
    let reply = family_reply(FactFamily::Plan, "把对账单发给你", "张三计划在下周三之前把对账单发给李四。");
    let review = review_reply(FactFamily::Plan, &[("f1", "pass", None)]);
    let out =
        run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &ScriptBackend::two(reply, review), NOW + 1_000)
            .unwrap();

    assert_eq!((out.drafts, out.written, out.duplicate), (1, 0, 1));
    assert_eq!((out.rejected, out.uncertain), (0, 0), "去重不算拒收/不确定");
    assert_eq!((out.facts, out.cards, out.mentions), (0, 0, 0));
    assert_eq!(out.state, PassState::Done, "全被去重 ⇒ 已处理");
    assert_eq!(count(root.memory(), "fact"), 1, "库里还是那一行");
    let r = record(root.ledger(), &uids[0], FactFamily::Plan);
    assert_eq!(r.state, PassState::Done);
    assert!(r.reason.is_none(), "去重不是失败态，不写 reason");
}

/// 20. 不同 value 不被去重：同一个证据轮、同一个主语，值不同 ⇒ 两行。
#[test]
fn a_different_value_on_the_same_turn_is_not_deduped() {
    let (_dir, root, _uids) = seeded();
    let promise = plan_of(&root, FactFamily::Promise);
    run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &promise,
        &ScriptBackend::two(promise_reply(), review_pass()),
        NOW,
    )
    .unwrap();

    let plan = select_fact_batch(
        root.transcript(),
        root.ledger(),
        root.memory(),
        0,
        FactFamily::Plan,
        NOW,
    )
    .unwrap()
    .unwrap();
    // P0-1：value 必须是 quote 的逐字区间，且与 promise 那条的 value 互不包含
    // （去重按 value 区间判，包含即 duplicate）——「下周三之前」是这段引文里唯一
    // 与「把对账单发给你」不相交的实义片段；text 不受逐字约束，仍写另一件事。
    let reply = family_reply(FactFamily::Plan, "下周三之前", "张三计划先抄送一份财务。");
    let review = review_reply(FactFamily::Plan, &[("f1", "pass", None)]);
    let out =
        run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &ScriptBackend::two(reply, review), NOW + 1_000)
            .unwrap();

    assert_eq!((out.written, out.duplicate, out.facts), (1, 0, 1));
    assert_eq!(count(root.memory(), "fact"), 2, "值不同 = 另一条事实");
}

/// 20b. 跨族同句（§10.45）：同一证据轮、同一 quote，两族的 value 互相包含
///      ⇒ 同一句只留先写的那一行、第二族计 `duplicate`。
///      与用例 20 的差别只在 value：这里两段 value 互相包含（"把对账单发给你"
///      ⊂ "下周三之前把对账单发给你"），那里两段 value 不相交（"下周三之前"）。
#[test]
fn a_cross_family_row_on_the_same_sentence_and_value_span_is_only_counted() {
    let (_dir, root, _uids) = seeded();
    let promise = plan_of(&root, FactFamily::Promise);
    run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &promise,
        &ScriptBackend::two(promise_reply(), review_pass()),
        NOW,
    )
    .unwrap();

    let plan = select_fact_batch(
        root.transcript(),
        root.ledger(),
        root.memory(),
        0,
        FactFamily::Plan,
        NOW,
    )
    .unwrap()
    .unwrap();
    let reply = family_reply(
        FactFamily::Plan,
        "下周三之前把对账单发给你",
        "张三计划在下周三之前把对账单发给李四。",
    );
    let review = review_reply(FactFamily::Plan, &[("f1", "pass", None)]);
    let out =
        run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &ScriptBackend::two(reply, review), NOW + 1_000)
            .unwrap();

    assert_eq!(
        (out.drafts, out.written, out.duplicate, out.rejected, out.uncertain),
        (1, 0, 1, 0, 0),
        "同一句在两族各抽一次 ⇒ 只落一行，另一条计 duplicate"
    );
    assert_eq!((out.facts, out.cards, out.mentions), (0, 0, 0));
    assert_eq!(out.state, PassState::Done, "全被去重 ⇒ 已处理");
    assert_eq!(count(root.memory(), "fact"), 1, "库里只有 promise 那行");
}

/// 21. 批内两条同 (turn, value) ⇒ 只写一条，另一条计 duplicate（不靠模型，程序判定）。
#[test]
fn two_rows_with_the_same_value_in_one_batch_write_only_one() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);
    let one = serde_json::json!({
        "ref": "f1", "turn": "t1", "subject": "a1", "predicate": "promise",
        "object": "a2", "value": "把对账单发给你", "category": "promise",
        "mode": "stated", "time_text": "下周三之前", "quote": PROMISE,
        "evidence": ["t1"], "text": "张三承诺在下周三之前把对账单发给李四。"
    });
    let mut two = one.clone();
    two["ref"] = json!("f2");
    let reply = serde_json::json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": [one, two], "empty_turns": ["t2", "t3"]
    })
    .to_string();
    let review = review_reply(FactFamily::Promise, &[("f1", "pass", None), ("f2", "pass", None)]);
    let backend = ScriptBackend::two(reply, review);

    let out = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(
        (out.turns, out.drafts, out.written, out.duplicate, out.rejected, out.uncertain),
        (3, 2, 1, 1, 0, 0)
    );
    assert_eq!((out.facts, out.cards), (1, 1));
    assert_eq!(out.state, PassState::Done);
    assert_eq!(count(root.memory(), "fact"), 1, "批内重复不造第二行");
    assert_eq!(record(root.ledger(), &uids[0], FactFamily::Promise).state, PassState::Done);
}

/// 22. 压缩块隔离（S2e-6）：选批认出摘要轮、当场落 `rejected(compact_summary)` 并跳过；
///     它不进 batch、不进下一次选批，普通轮照常。
#[test]
fn a_compaction_turn_is_rejected_before_it_enters_the_batch() {
    let (_dir, root, uids) = seeded_with_compaction();
    let plan = select_fact_batch(
        root.transcript(),
        root.ledger(),
        root.memory(),
        0,
        FactFamily::Promise,
        NOW,
    )
    .unwrap()
    .expect("t1/t3 可选");
    let got: Vec<String> = plan.batch.rows.iter().map(|r| r.turn_uid.clone()).collect();
    assert_eq!(got, vec![uids[0].clone(), uids[2].clone()], "压缩摘要轮不进批");

    let r = record(root.ledger(), &uids[1], FactFamily::Promise);
    assert_eq!(r.state, PassState::Rejected);
    assert_eq!(r.reason.as_deref(), Some("compact_summary"));

    // 第二次选批：摘要在场（rejected），普通轮仍在。
    let again = select_fact_batch(
        root.transcript(),
        root.ledger(),
        root.memory(),
        0,
        FactFamily::Promise,
        NOW + 1,
    )
    .unwrap()
    .unwrap();
    let got: Vec<String> = again.batch.rows.iter().map(|r| r.turn_uid.clone()).collect();
    assert_eq!(got, vec![uids[0].clone(), uids[2].clone()], "同一轮不被重复扫");
}

/// 23. 压缩块的判定只认**前缀**（前两种通用形状），句子中间的标记不算。
#[test]
fn compaction_bodies_are_recognised_by_prefix_only() {
    assert!(is_compaction_body("[[compact_summary]]\n## Key Decisions\n- 先做这个"));
    assert!(is_compaction_body("  \n\t[[compact_summary]] 摘要"));
    assert!(is_compaction_body(CONTINUED_BODY));
    assert!(is_compaction_body(
        "  this session is being continued from a previous conversation, so here is the gist"
    ), "大小写不敏感、允许前导空白");
    assert!(!is_compaction_body("日志里提到 [[compact_summary]] 这件事，但不是块首"));
    assert!(!is_compaction_body(PROMISE));
}

/// 23b. 第三种通用形状：自动 checkpoint（DSH 那类 agent 会话里的压缩块，2026-10-31 实测
/// 554 条非 system 轮里有 4 条是这种写法，角色是 `human` ⇒ 旧判据会把它们当话送进四个抽取 pass）。
///
/// 拆成独立一条而不是塞进上面那张表，是为了**让负例的红可归因**：表里那条负例守的是标记那一族的
/// 块首规则，这一条守的是英文前缀那一族——合在一条用例里，任何一条先红就看不出是哪条规则失守。
#[test]
fn an_automatic_checkpoint_block_is_recognised_only_at_the_head() {
    assert!(is_compaction_body(CHECKPOINT_BODY), "块首的自动 checkpoint 是索引，不是话");
    assert!(
        !is_compaction_body(
            "他说 This is an automatically generated checkpoint condensing 那段其实不算摘要块"
        ),
        "第三个前缀同样只认**块首**：句中出现不算（挡住把判据写成 contains 的实现）"
    );
    assert!(
        is_compaction_body(
            "  THIS IS AN AUTOMATICALLY GENERATED CHECKPOINT CONDENSING an earlier span"
        ),
        "大小写不敏感、允许前导空白——三条前缀同一条尺子"
    );
}

/// 23c. 适配器**注入的上下文块**与压缩摘要同族：都不是某人说过的话。DSH 语料实测
/// （554 条非 system 轮）三类块首各有多少条：`Current runtime context` 29、`<system-reminder>` 31、
/// `Time sampled while preparing turn` 61 ⇒ 合起来 121 条（21.8%）此前会被当成普通轮送进四个抽取 pass。
/// 三条都**超过** notice 那道 240 字符的长度闸（实测 min 429 / 1485 / 248），所以正确的家是
/// 块首前缀这一族，不是"短通知"那一族——这条形状差别是本用例存在的全部理由。
#[test]
fn injected_context_blocks_are_recognised_at_the_head() {
    assert!(
        is_compaction_body(
            "Current runtime context. This snapshot supersedes earlier runtime-context snapshots.              Working set: 3 tabs open, browser attached."
        ),
        "runtime-context 快照是注入的上下文，不是话"
    );
    assert!(
        is_compaction_body("<system-reminder> A skill is a reusable set of task-specific instructions. </system-reminder>"),
        "system-reminder 块同理"
    );
    assert!(
        is_compaction_body(
            "Time sampled while preparing turn 1, step 1: 2026-10-10T23:14:30+08:00[Asia/Shanghai]              Browser state captured for later comparison; latency 257ms measured end to end here."
        ),
        "计时标注同理（248–262 字符，正好在 notice 的 240 长度闸之外）"
    );
    assert!(
        !is_compaction_body("我在日志里看到 <system-reminder> 这个标记，但这句话是我自己说的"),
        "仍然只看块首"
    );
}

// ── S2e-7：会话 agent 自锚进候选 ──────────────────────────────────────────────

/// 抽取回复：t1 一条 promise，subject 指向调用方给的自锚引用（`aN`），object 指向李四。
fn agent_subject_reply(subject: &str, object: &str) -> String {
    serde_json::json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": subject, "predicate": "promise",
            "object": object, "value": "把对账单发给你", "category": "promise",
            "mode": "stated", "time_text": "下周三之前", "quote": PROMISE,
            "evidence": ["t1"], "text": "已确认：下周三之前把对账单发给李四。"
        }],
        "empty_turns": ["t2", "t3"]
    })
    .to_string()
}

/// 24. 模型用自锚作主语：审核 pass 后 fact 行的 subject_anchor_id 等于那条锚的 id，
///     锚在库里是 kind='agent' 的活锚——"我/本 agent 决定/计划…"终于有合法主体。
#[test]
fn the_agent_self_anchor_can_be_the_subject_of_a_written_fact() {
    let (_dir, root, _uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);
    let self_anchor = plan
        .candidates
        .iter()
        .find(|candidate| candidate.kind == "agent")
        .expect("自锚必须在候选里")
        .clone();
    assert_eq!(self_anchor.reference, "a3", "自锚排在原 active 列表之后");
    let li = plan
        .candidates
        .iter()
        .find(|candidate| candidate.canonical == "李四")
        .expect("李四在候选里")
        .reference
        .clone();

    let reply = agent_subject_reply(&self_anchor.reference, &li);
    let backend = ScriptBackend::two(reply, review_pass());
    let out = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();

    assert_eq!((out.drafts, out.written, out.rejected), (1, 1, 0));
    let row = fact_row(root.memory());
    assert_eq!(row.subject_anchor_id, self_anchor.anchor_id, "subject 落在程序自锚上");
    assert_eq!(row.object_anchor_id.as_deref(), Some(anchor_id(root.memory(), "李四").as_str()));
    let (kind, status): (String, String) = root
        .memory()
        .conn()
        .query_row("SELECT kind, status FROM anchor WHERE id=?1", [&self_anchor.anchor_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((kind.as_str(), status.as_str()), ("agent", "active"));
}

/// 25. naming 已经建过同 canonical 的 agent 锚 ⇒ 不重复 append（候选里只有一个 agent 项），
///     也不造第二行锚：自锚的复用判据是 anchor_id，不是"名字像"。
#[test]
fn an_existing_agent_anchor_is_reused_instead_of_appended_twice() {
    let (_dir, root, uids) = seeded();
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    put_entity(
        root.memory(),
        &EntityContent {
            kind: "agent".into(),
            title: "agent-batch-A".into(),
            scope,
            aliases: vec![],
        },
        &[Provenance { turn_uid: uids[0].clone(), anchor_role: AnchorRole::Human }],
        NOW - 500,
    )
    .unwrap();

    let plan = plan_of(&root, FactFamily::Promise);
    let agents: Vec<&rc_distill::pass_prompt::FactCandidate> =
        plan.candidates.iter().filter(|candidate| candidate.kind == "agent").collect();
    assert_eq!(agents.len(), 1, "同 canonical 的自锚只出现一次");
    assert_eq!(agents[0].canonical, "agent-batch-A");
    assert_eq!(agents[0].anchor_id, anchor_id(root.memory(), "agent-batch-A"));
    assert_eq!(count(root.memory(), "anchor"), 4, "张三 / 李四 / naming 的 agent 锚 / 主人锚，没有第二行");
}

/// 26. 身份层 I1：`owner_principal_id` 取 claim 表的最新一条，而不是 `sessions` 那一列。
///
/// 夹具把会话列抹成 NULL、只留一条 claim：行的 owner（`resource_owner`）必须用 claim 的值。
/// 这一格是证红点——把解析退回 BASE（只读 `sessions.owner_principal_id`）时它会掉成 NULL。
/// 候选池不受影响：它按 `owner_agent_id` + scope 过滤，与 principal 正交。
#[test]
fn the_written_owner_comes_from_the_claim_table_when_the_session_column_is_null() {
    let (_dir, root, _uids) = seeded();
    root.transcript()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id=NULL
             WHERE session_key='sess-batch-A' AND device_id='dev-batch-A'",
            [],
        )
        .unwrap();
    // B 语义（用户 2026-11-01 拍板"改名不换锚 + 同名第三方不并入主人"）：这一章的可读名是"张三"，
    // 而候选池里本来就有一格语料人名也叫张三 ⇒ **两格各归各**（合并会把同事的状态记到主人头上，
    // 那正是 D19/D7 一路在拆的形状）。于是 t1"我下周三…"的主语只能落在**主人那一格**（a4），
    // 命中途径是程序盖的说话人章/主人锚同格，不是靠名字字符串撞车。
    rc_ledger::transcript::claim_principal(
        root.transcript(),
        rc_ledger::transcript::NewPrincipalClaim {
            session_key: "sess-batch-A",
            device_id: "dev-batch-A",
            principal_id: "p-claim",
            principal_name: Some("张三"),
            source: "human_claim",
            claimed_by: "qoder",
            reason: None,
        },
        NOW,
    )
    .unwrap();

    // 选批：owner 从 claim 解析出来（会话列是 NULL），落库的 fact 行的 `resource_owner` 用它。
    let plan = plan_of(&root, FactFamily::Promise);
    assert_eq!(
        plan.candidates[3].kind, "person",
        "a4 必须是主人锚那一格（位置即契约：池里的张三/李四、自锚、主人锚）"
    );
    assert_eq!(plan.candidates[3].canonical, "张三", "候选表露出的是**可读名**，不是账号主键");
    assert_ne!(
        plan.candidates[3].anchor_id, plan.candidates[0].anchor_id,
        "主人锚与语料里那个同名的张三必须是两格（B：同名第三方不并入主人）"
    );
    let backend = ScriptBackend::two(promise_reply_on_principal(), review_pass());
    let out = run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(out.written, 1, "主语是主人那一格时，human 轮的『我』必须能落库");
    // 先断**写进库的那一行**（这一格就是证红点）。
    let row = fact_row(root.memory());
    let owner = owner_of(root.memory(), "fact", &row.id);
    assert_eq!(owner.owner_principal_id.as_deref(), Some("p-claim"));
    assert_eq!(owner.owner_agent_id, "alice", "agent 归属仍取会话行");
    assert_eq!(plan.owner.owner_principal_id.as_deref(), Some("p-claim"));
    assert_eq!(
        row.subject_anchor_id, plan.candidates[3].anchor_id,
        "落库的主语就是主人那一格，不是同名的第三方"
    );
}


// ── I2：主人锚进候选 ─────────────────────────────────────────────────────────

/// 表里的 person 锚（`canonical`，按字节序）——判据 5 的只读读数就走这一格。
fn person_canonicals(m: &MemoryHandle) -> Vec<String> {
    let conn = m.conn();
    let mut st = conn
        .prepare("SELECT canonical FROM anchor WHERE kind='person' ORDER BY canonical")
        .unwrap();
    let rows = st.query_map([], |r| r.get::<_, String>(0)).unwrap();
    rows.map(|row| row.unwrap()).collect()
}

/// 27. 候选表同时含 agent 锚与主人锚：`reference` 连续（a1..a4）、`anchor_id` 无重复，
///     主人锚真落库（kind=person；这一章没有可读名 ⇒ canonical = principal_id）。
#[test]
fn candidates_carry_the_agent_anchor_then_the_principal_anchor_without_duplicates() {
    let (_dir, root, _uids) = seeded();
    let plan = plan_of(&root, FactFamily::Promise);

    let refs: Vec<&str> = plan.candidates.iter().map(|c| c.reference.as_str()).collect();
    assert_eq!(refs, ["a1", "a2", "a3", "a4"], "位置即契约：a1..aN 连续");
    let ids: BTreeSet<&str> =
        plan.candidates.iter().map(|c| c.anchor_id.as_str()).collect();
    assert_eq!(ids.len(), plan.candidates.len(), "同一个锚不许占两个 a*");
    assert_eq!(plan.candidates[2].kind, "agent", "自锚在前");
    assert_eq!(plan.candidates[3].kind, "person", "主人锚接在自锚之后");
    assert_eq!(plan.candidates[3].canonical, "p1");
    assert_eq!(plan.candidates[3].anchor_id, person_anchor_id(root.memory(), "p1"));

    // 判据 5 的只读读数：主人锚真落库（张三 / 李四 是夹具实体锚，p1 是主人锚）。
    let persons = person_canonicals(root.memory());
    println!("person anchors on this fixture: {persons:?}");
    assert_eq!(persons, ["p1", "张三", "李四"]);
}

/// 28. `principal_id="unknown"` ⇒ 不建锚：候选表不加格、anchor 表不多行。
#[test]
fn an_unknown_principal_creates_no_anchor() {
    let (_dir, root, _uids) = seeded();
    root.transcript()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id=NULL
             WHERE session_key='sess-batch-A' AND device_id='dev-batch-A'",
            [],
        )
        .unwrap();
    rc_ledger::transcript::claim_principal(
        root.transcript(),
        rc_ledger::transcript::NewPrincipalClaim {
            session_key: "sess-batch-A",
            device_id: "dev-batch-A",
            principal_id: "unknown",
            principal_name: None,
            source: "unknown",
            claimed_by: "agent-batch-A",
            reason: None,
        },
        NOW,
    )
    .unwrap();

    let plan = plan_of(&root, FactFamily::Promise);
    assert_eq!(
        plan.owner.owner_principal_id.as_deref(),
        Some("unknown"),
        "解析值仍是 unknown（不知道也是一种读数）"
    );
    assert_eq!(plan.candidates.len(), 3, "张三 / 李四 / agent 锚，没有第四格");
    assert_eq!(count(root.memory(), "anchor"), 3, "unknown 不建锚：表里零新行");
    let unknown_persons: i64 = root
        .memory()
        .conn()
        .query_row(
            "SELECT count(*) FROM anchor WHERE kind='person' AND canonical='unknown'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unknown_persons, 0, "不造'无名用户'");
}

// ── 主人臂：human 轮的说话人是主人锚（审计 P2-2）────────────────────────────────

/// 会话声明（与 `seeded` 改的那一行逐列相同）与批上下文：真生产者要的就是这两样。
fn session_owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p1".to_string()),
        owner_agent_id: "alice".to_string(),
        visibility: Visibility::Project,
        retention: "keep".to_string(),
    }
}

fn session_ctx() -> BatchCtx {
    BatchCtx {
        source_agent: "agent-batch-A".to_string(),
        device_id: "dev-batch-A".to_string(),
    }
}

fn session_scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-x".into() }
}

/// 28b（审计 P2-2）. human 轮的 fact 行，`speaker_anchor_id` 是**主人锚**那一格。
///
/// 两条锚都由真生产者建：`ensure_agent_anchor` 出自锚、`ensure_principal_anchor` 出主人锚
/// （它的归属行不带设备）。写库层按"归属行不带设备"找回主人，夹具此前只会造"person +
/// 带设备"的行，于是这一臂在用例里永远走不到，也没有任何断言盯着它。
#[test]
fn a_human_turn_writes_the_principal_anchor_as_its_speaker() {
    let (_dir, root, uids) = seeded();
    let (owner, ctx, scope) = (session_owner(), session_ctx(), session_scope());
    // 选批内部先走一遍两个生产者；再显式调一次：同 (canonical, scope) 落在同一格（幂等），
    // 返回的就是这一批真正在用的那两格。
    let plan = plan_of(&root, FactFamily::Promise);
    let self_anchor =
        ensure_agent_anchor(root.memory(), &ctx, &owner, &scope, NOW).unwrap().id;
    let principal_anchor =
        ensure_principal_anchor(root.memory(), "p1", None, &owner, &scope, NOW)
            .unwrap()
            .expect("p1 是已知主人")
            .id;
    assert_ne!(self_anchor, principal_anchor, "自锚与主人锚必须是两格");
    assert_eq!(plan.candidates[2].anchor_id, self_anchor, "a3 就是 `ensure_agent_anchor` 那一格");
    assert_eq!(
        plan.candidates[3].anchor_id, principal_anchor,
        "a4 就是 `ensure_principal_anchor` 那一格"
    );

    // 生产者契约：主人锚的归属行不带设备，自锚带会话设备——这一格就是两者在库里的分界。
    let device_of = |anchor_id: &str| -> Option<String> {
        root.memory()
            .conn()
            .query_row(
                "SELECT device_id FROM resource_owner WHERE mem_table='anchor' AND mem_id=?1",
                [anchor_id],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(device_of(&principal_anchor), None, "主人锚的归属行不带设备");
    assert_eq!(device_of(&self_anchor).as_deref(), Some("dev-batch-A"), "自锚带会话设备");

    let backend = ScriptBackend::two(promise_reply(), review_pass());
    let out =
        run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(out.written, 1, "夹具前提：这一条真写进库");

    // t1 是 human 轮 ⇒ 说话人是主人锚：不是自锚、也不是 NULL。
    let row = fact_row(root.memory());
    assert_eq!(row.evidence_turn_uid, uids[0], "证据轮就是那条 human 轮");
    assert_eq!(
        row.speaker_anchor_id.as_deref(),
        Some(principal_anchor.as_str()),
        "human 说的那句话，说话人是主人锚"
    );
    assert_ne!(
        row.speaker_anchor_id.as_deref(),
        Some(self_anchor.as_str()),
        "不许回落成会话自锚"
    );
}

/// §10.66 C：运行期通知不是 utterance —— fact 选批同样当场落 `rejected(not_utterance)` 并跳过。
#[test]
fn a_runtime_notice_is_rejected_before_it_enters_the_fact_batch() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(
        &mut root,
        &[
            PROMISE,
            "Model response interrupted. Completed output is preserved; automatic continuation stopped.",
        ],
        1_000,
    );

    let plan = select_fact_batch(
        root.transcript(), root.ledger(), root.memory(), 0, FactFamily::Promise, NOW)
        .unwrap()
        .expect("t1 可选");
    let got: Vec<String> = plan.batch.rows.iter().map(|r| r.turn_uid.clone()).collect();
    assert_eq!(got, vec![uids[0].clone()], "运行期通知不进批");

    let r = record(root.ledger(), &uids[1], FactFamily::Promise);
    assert_eq!(r.state, PassState::Rejected);
    assert_eq!(r.reason.as_deref(), Some("not_utterance"));
}
