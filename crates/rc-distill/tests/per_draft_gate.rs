//! I3 第一刀：程序闸**逐 draft** 判定（工单 `2026-10-28-per-draft-gate`，设计 §3）。
//!
//! 背景：旧闸"首个失败码即整批失败"，一批 20 轮里 3 条草稿坏 1 条就整批 rejected、0 落库
//! （中心实测 `review_item` id=134）。本文件的四条用例钉死新口径：
//!
//! a. 混合批：1 条通过、1 条 `subject_not_in_evidence` ⇒ 通过条入库、坏条 rejected，
//!    两条证据轮分别 done / rejected；
//! b. 全部 draft 失败 ⇒ 整批 rejected（不调审核模型、0 行）；
//! c. 批级前置失败（`turn_missing`）⇒ 整批 rejected（与今天一致，码不变）；
//! d. 被坏 draft 覆盖不到的同批轮 ⇒ `empty`，不因同批有坏 draft 变成 rejected。
//!
//! 夹具走真链路（`seed_bodies` 真导入转录、真锚 + 归属行），后端是脚本回放，
//! **不跑真实 LLM**。正反例只用通用占位名（张三/王五/某项目），不含语料专名。

use rc_distill::pass_fact::{run_fact, select_fact_batch, FactOutcome};
use rc_distill::pass_prompt::FactFamily;
use rc_ledger::pass_state::{self, PassKey, PassRecord, PassState};
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::review::{decisions, list, ReviewStage, ReviewState, ReviewVerdict};
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{AnchorRole, Provenance, Scope, ScopeKind};
use rc_store::{LedgerHandle, MemoryHandle, Root};
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

mod common;
use common::{adopt_rows, seed_bodies};

const NOW: i64 = 1_700_000_000_000;
const FAMILY: FactFamily = FactFamily::Decision;

/// 第 1 轮（human）：主语"张三"逐字在正文里 ⇒ 主语检查通过。
const BODY_DECIDE: &str = "张三决定把发布推迟到 12 月 3 日";
/// 第 2 轮（agent）：正文里没有"王五" ⇒ 引用王五做主语必判 `subject_not_in_evidence`。
const BODY_ACK: &str = "好的，收到，我会跟进";
/// 第 3 轮（human）：寒暄，只用来制造"没有草稿覆盖的轮"。
const BODY_TAIL: &str = "谢谢";

/// 夹具：会话三轮（human/agent/human），归属 `alice/p1/project/proj-x`；
/// 两个活锚——张三较新（`a1`）、王五较早（`a2`），都按会话归属过滤后可见。
fn seeded(bodies: &[&str]) -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, bodies, 1_000);
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
    for (canonical, created_at_ms) in [("王五", NOW - 2_000), ("张三", NOW - 1_000)] {
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
    // 候选池按会话归属过滤：夹具的实体锚改挂会话 owner（真实链路由命名 pass 做）。
    adopt_rows(&root, "anchor", "alice", Some("p1"), "project", "project", "proj-x");
    (dir, root, uids)
}

fn plan_of(root: &Root) -> rc_distill::pass_fact::FactPlan {
    select_fact_batch(root.transcript(), root.ledger(), root.memory(), 0, FAMILY, NOW)
        .unwrap()
        .expect("夹具里的轮都还没跑这一族")
}

fn key_of(uid: &str) -> PassKey<'_> {
    PassKey { turn_uid: uid, pass_id: "fact", family: FAMILY.slug() }
}

fn record(l: &LedgerHandle, uid: &str) -> PassRecord {
    pass_state::get(l, key_of(uid))
        .unwrap()
        .unwrap_or_else(|| panic!("{uid} 没有 fact/{} 状态行", FAMILY.slug()))
}

fn fact_count(m: &MemoryHandle) -> i64 {
    m.conn()
        .query_row("SELECT count(*) FROM fact", [], |r| r.get(0))
        .unwrap()
}

/// 一条 fact 草稿的 JSON。`subject` 是候选引用（`a1`/`a2`），`turn` 是批内位置（`t1`/`t2`）。
fn draft(
    reference: &str,
    turn: &str,
    subject: &str,
    value: &str,
    quote: &str,
    text: &str,
) -> serde_json::Value {
    // C2：`time_text` 必须接地（quote 或证据正文的子串）。这里只有 `BODY_DECIDE` 那一轮
    // 真的说了「12 月 3 日」，其余草稿不写时间短语——否则会被判 `time_text_ungrounded`。
    let time_text = if quote.contains("12 月 3 日") { "12 月 3 日" } else { "" };
    json!({
        "ref": reference,
        "turn": turn,
        "subject": subject,
        "predicate": "decide",
        "object": null,
        "value": value,
        "category": "decision",
        "mode": "stated",
        "time_text": time_text,
        "quote": quote,
        "evidence": [turn],
        "text": text
    })
}

fn extraction(drafts: Vec<serde_json::Value>, empty_turns: &[&str]) -> String {
    json!({
        "schema_version": 0,
        "pass": "fact",
        "family": FAMILY.slug(),
        "facts": drafts,
        "empty_turns": empty_turns,
    })
    .to_string()
}

fn review(verdicts: Vec<serde_json::Value>) -> String {
    json!({
        "schema_version": 0,
        "pass": "fact_review",
        "family": FAMILY.slug(),
        "verdicts": verdicts,
    })
    .to_string()
}

fn verdict(reference: &str, value: &str, code: Option<&str>) -> serde_json::Value {
    // §7.2：pass/reject 必须带 quote 内的一段原文；夹具的引文都是 BODY_DECIDE。
    let evidence = if value == "uncertain" { None } else { Some("把发布推迟") };
    json!({"ref": reference, "verdict": value, "code": code, "evidence": evidence})
}

/// 测试自带的后端：按序回放脚本，记录调用次数（脚本耗尽 = `Timeout`）。
struct ScriptBackend {
    replies: RefCell<VecDeque<Result<String, LlmFailure>>>,
    calls: Cell<usize>,
}

impl ScriptBackend {
    fn new(replies: Vec<Result<String, LlmFailure>>) -> Self {
        ScriptBackend { replies: RefCell::new(replies.into()), calls: Cell::new(0) }
    }

    fn two(extraction: String, review: String) -> Self {
        // 一片 fact 审核 = proposer + challenger 两次调用（§7.2）。
        ScriptBackend::new(vec![Ok(extraction), Ok(review.clone()), Ok(review)])
    }

    fn only(extraction: String) -> Self {
        ScriptBackend::new(vec![Ok(extraction)])
    }
}

impl LlmBackend for ScriptBackend {
    fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
        self.calls.set(self.calls.get() + 1);
        self.replies.borrow_mut().pop_front().unwrap_or(Err(LlmFailure::Timeout))
    }
    fn slug(&self) -> Option<&str> {
        Some("fake-per-draft-model")
    }
}

fn run(
    root: &Root,
    plan: &rc_distill::pass_fact::FactPlan,
    backend: &ScriptBackend,
) -> FactOutcome {
    run_fact(root.memory(), root.transcript(), root.ledger(), plan, backend, NOW).unwrap()
}

fn only_item(root: &Root) -> rc_memory::review::ItemMeta {
    let items = list(root.memory(), None, Some("fact"), 0).unwrap();
    assert_eq!(items.len(), 1, "这一批只该有一条 review_item");
    items.into_iter().next().unwrap()
}

/// 某条 item 的 review 阶段判决（最后一条）。
fn last_review_decision(root: &Root, id: i64) -> rc_memory::review::DecisionRow {
    decisions(root.memory(), id)
        .unwrap()
        .into_iter()
        .rfind(|row| row.stage == ReviewStage::Review)
        .expect("引擎应当记过一条 review 判定")
}

// ── a. 混合批：通过的入库、坏的只拒自己 ─────────────────────────────────────

#[test]
fn 混合批_一条通过一条主语不在证据_通过条入库坏条只拒自己() {
    let (_dir, root, uids) = seeded(&[BODY_DECIDE, BODY_ACK, BODY_TAIL]);
    let plan = plan_of(&root);
    let backend = ScriptBackend::two(
        extraction(
            vec![
                draft("f1", "t1", "a1", "把发布推迟", BODY_DECIDE, "张三决定把发布推迟到 12 月 3 日。"),
                // a2 = 王五：agent 轮正文里没有这个名字 ⇒ `subject_not_in_evidence`。
                draft("f2", "t2", "a2", "我会跟进", BODY_ACK, "王五决定跟进发布。"),
            ],
            &["t3"],
        ),
        review(vec![verdict("f1", "pass", None)]),
    );
    let outcome = run(&root, &plan, &backend);

    // 抽取 + 窄审各一次：坏草稿没有进审核输入（不问模型就不可能被放行）。
    assert_eq!(backend.calls.get(), 3, "抽取 + 两档审核；坏 draft 不该进审核输入");
    // 通过的那一条真落了库，且只有它。
    assert_eq!(fact_count(root.memory()), 1, "通过条应当入库、坏条不该留行");
    let evidence: String = root
        .memory()
        .conn()
        .query_row("SELECT evidence_turn_uid FROM fact", [], |r| r.get(0))
        .unwrap();
    assert_eq!(evidence, uids[0], "落库的应当是通过 draft 的证据轮");
    assert_eq!(outcome.written, 1);
    assert_eq!(outcome.state, PassState::Done);

    // 逐轮粒度：好 draft 的轮 done，坏 draft 的轮 rejected(<该 draft 的失败码>)。
    assert_eq!(record(root.ledger(), &uids[0]).state, PassState::Done);
    let bad = record(root.ledger(), &uids[1]);
    assert_eq!(bad.state, PassState::Rejected, "坏 draft 的证据轮必须是 rejected");
    assert_eq!(bad.reason.as_deref(), Some("subject_not_in_evidence"));

    // item 不是整批 reject：程序 pass + 审核 mixed（p=1;r=1，坏条计入 reject 计数）。
    let meta = only_item(&root);
    assert_eq!(meta.state, ReviewState::Applied, "混合批应当 apply 而不是 rejected");
    let decision = last_review_decision(&root, meta.id);
    assert_eq!(decision.verdict, ReviewVerdict::Pass);
    assert_eq!(decision.reason_code, "review_mixed");
    assert!(
        decision.detail.contains("p=1;r=1"),
        "detail 应当把坏条计进 reject：{}",
        decision.detail
    );
}

// ── b. 全部 draft 失败 ──────────────────────────────────────────────────────

#[test]
fn 全部草稿失败_整批rejected且不调审核模型() {
    let (_dir, root, uids) = seeded(&[BODY_DECIDE, BODY_ACK, BODY_TAIL]);
    let plan = plan_of(&root);
    // 两条草稿都引用王五、证据都在 agent 轮 t2 上 ⇒ 全部 `subject_not_in_evidence`。
    let backend = ScriptBackend::only(extraction(
        vec![
            draft("f1", "t2", "a2", "收到", "好的，收到", "王五决定跟进发布。"),
            draft("f2", "t2", "a2", "我会跟进", "我会跟进", "王五决定继续跟进。"),
        ],
        &["t1", "t3"],
    ));
    let outcome = run(&root, &plan, &backend);

    assert_eq!(backend.calls.get(), 1, "全部失败时不调审核模型");
    assert_eq!(fact_count(root.memory()), 0, "整批 rejected 不许留行");
    assert_eq!(outcome.state, PassState::Rejected);
    for uid in &uids {
        let rec = record(root.ledger(), uid);
        assert_eq!(rec.state, PassState::Rejected, "{uid} 应当整批 rejected");
        assert_eq!(rec.reason.as_deref(), Some("subject_not_in_evidence"));
    }
    let meta = only_item(&root);
    assert_eq!(meta.state, ReviewState::Rejected);
    assert_eq!(meta.program_code.as_deref(), Some("subject_not_in_evidence"));
}

// ── c. 批级前置失败（turn_missing）─────────────────────────────────────────

#[test]
fn 批级前置失败_轮缺失时整批rejected与今天一致() {
    let (_dir, root, uids) = seeded(&[BODY_DECIDE, BODY_ACK, BODY_TAIL]);
    let mut plan = plan_of(&root);
    // 让批里有一轮在转录里不存在：闸的批级前置必须先命中 `turn_missing`（整批不可用），
    // 而不是被逐 draft 判定"消化"掉。
    plan.batch.rows[1].turn_uid = "f".repeat(64);
    let batch_uids: Vec<String> =
        plan.batch.rows.iter().map(|row| row.turn_uid.clone()).collect();
    let backend = ScriptBackend::only(extraction(
        vec![
            draft("f1", "t1", "a1", "把发布推迟", BODY_DECIDE, "张三决定把发布推迟到 12 月 3 日。"),
            draft("f2", "t2", "a1", "我会跟进", BODY_ACK, "张三决定跟进发布。"),
        ],
        &["t3"],
    ));
    let outcome = run(&root, &plan, &backend);

    assert_eq!(backend.calls.get(), 1, "批级前置失败不调审核模型");
    assert_eq!(fact_count(root.memory()), 0);
    assert_eq!(outcome.state, PassState::Rejected);
    for uid in &batch_uids {
        let rec = record(root.ledger(), uid);
        assert_eq!(rec.state, PassState::Rejected, "{uid} 应当整批 rejected");
        assert_eq!(rec.reason.as_deref(), Some("turn_missing"), "码与今天逐字一致");
    }
    // 批里的真实轮（除了被换掉的那一格）也都在整批 rejected 里。
    assert_eq!(record(root.ledger(), &uids[0]).state, PassState::Rejected);
    assert_eq!(record(root.ledger(), &uids[2]).state, PassState::Rejected);
    let meta = only_item(&root);
    assert_eq!(meta.state, ReviewState::Rejected);
    assert_eq!(meta.program_code.as_deref(), Some("turn_missing"));
}

// ── d. 没被坏 draft 覆盖的轮仍是 empty ──────────────────────────────────────

#[test]
fn 没被坏草稿覆盖的轮是empty_不因同批有坏草稿变成rejected() {
    // 四轮：t1 好草稿（human，张三在正文里）、t2 坏草稿（agent，王五不在正文里）、
    // t3/t4 没有草稿（被 `empty_turns` 覆盖）。
    let (_dir, root, uids) = seeded(&[BODY_DECIDE, BODY_ACK, BODY_TAIL, "收到"]);
    let plan = plan_of(&root);
    let backend = ScriptBackend::two(
        extraction(
            vec![
                draft("f1", "t1", "a1", "把发布推迟", BODY_DECIDE, "张三决定把发布推迟到 12 月 3 日。"),
                draft("f2", "t2", "a2", "我会跟进", BODY_ACK, "王五决定跟进发布。"),
            ],
            &["t3", "t4"],
        ),
        review(vec![verdict("f1", "pass", None)]),
    );
    let outcome = run(&root, &plan, &backend);

    assert_eq!(fact_count(root.memory()), 1, "同批的好草稿应当落库");
    assert_eq!(outcome.state, PassState::Done);
    // 核心断言：坏草稿覆盖不到的轮是 empty，不是 rejected。
    for uid in [&uids[2], &uids[3]] {
        let rec = record(root.ledger(), uid);
        assert_eq!(
            rec.state,
            PassState::Empty,
            "{uid} 没有草稿覆盖，不该被同批的坏 draft 拖成 rejected（reason={:?}）",
            rec.reason
        );
        assert_eq!(rec.reason, None);
    }
    assert_eq!(record(root.ledger(), &uids[0]).state, PassState::Done);
    assert_eq!(record(root.ledger(), &uids[1]).state, PassState::Rejected);
}
