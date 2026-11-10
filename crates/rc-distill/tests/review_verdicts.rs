//! §7.3a：逐条判决与证据落库（`review_verdict`）的 E2E。
//!
//! 断的是"落库"这一件事，不是重跑队列（那是 §7.3b）：
//! 1. 一次双判（proposer + challenger）跑完：两侧各 N 行、`final` N 行，坐标/版本/模型齐全；
//! 2. 被证据闸降级的条目：该侧那行是 `uncertain` + `evidence_state=missing|not_in_quote`，
//!    `evidence` 与模型给的**逐字节相同**（含空格与标点，不 trim 不改写）；
//! 3. 只写 fact：event 审核跑完，`review_verdict` 里 `pass_id='event'` 行数为 0；
//! 4. 迁移幂等：同一个库连开两次不报错、不重复建表、不堆重复行；
//! 5. 读取面：`item_verdicts` 能按 `side`/`verdict` 过滤（§7.3 的输入）。
//!
//! 夹具风格与 `tests/review_run.rs` 相同：真 `Root::open(tempdir)` + `mod common`，
//! 正反例只用通用占位名（张三/李四/某项目），不含任何语料专名。

use rc_distill::pass_event::{run_event, select_event_batch};
use rc_distill::pass_fact::{run_fact, select_fact_batch};
use rc_distill::pass_prompt::FactFamily;
use rc_distill::review::run::{run_item, RunOutcome};
use rc_distill::review::PASS_FACT_REVIEW_VERSION;
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::review::ReviewState;
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{
    item_verdicts, item_verdicts_filtered, replace_item_verdicts, AnchorRole, EvidenceState,
    NewVerdict, Provenance, ReviewVerdict, Scope, ScopeKind, VerdictSide,
};
use rc_store::{MemoryHandle, Root};
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

mod common;
use common::{adopt_rows, seed_bodies};

const NOW: i64 = 1_700_000_000_000;
const MODEL: &str = "fake-review-model";
/// t1 的正文与引文（同一段）。带一个全角逗号与句号：证据原文的"含标点原样存"要能测出来。
const PROMISE: &str = "我下周三之前把对账单发给你，记得确认一下。";
/// t2 是 **agent** 轮：正文要真写着主语"张三"。
const RECEIVED_BODY: &str = "张三已经收到对账单";
const CONFIG_BODY: &str = "昨晚在机房我把配置改回去，服务就恢复了";
const CONFIG_QUOTE: &str = "昨晚在机房我把配置改回去，服务就恢复了";

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

/// fact：3 轮（承诺 + 一句收到 + 一句寒暄），张三 a1、李四 a2 两个活锚。
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
fn seeded_event() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[CONFIG_BODY], 1_000);
    set_owner(&mut root);
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    add_anchor(&root, &scope, "place", "某项目", &uids[0], NOW - 2_000);
    add_anchor(&root, &scope, "person", "张三", &uids[0], NOW - 1_000);
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

/// 抽取回复：t1 一条 promise（a1=张三、a2=李四），t2/t3 空覆盖。
fn fact_reply() -> String {
    json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a1", "predicate": "promise",
            "object": "a2", "value": "把对账单发给你", "category": "promise",
            "mode": "stated", "time_text": "下周三之前", "quote": PROMISE,
            "evidence": ["t1"], "text": "张三承诺在下周三之前把对账单发给你。"
        }],
        "empty_turns": ["t2", "t3"]
    })
    .to_string()
}

/// fact 审核回复（两档同形）：`(ref, verdict, code, evidence)` 四键。
fn fact_review(decisions: &[(&str, &str, Option<&str>, Option<&str>)]) -> String {
    let verdicts: Vec<Value> = decisions
        .iter()
        .map(|(reference, verdict, code, evidence)| {
            json!({"ref": reference, "verdict": verdict, "code": code, "evidence": evidence})
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

/// 回放 backend：按序给回复（脚本耗尽 = `Timeout`）。
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

/// 造一条停在 `pending_review` 的 fact item，返回 id。
fn pending_fact(root: &Root) -> i64 {
    let plan = fact_plan(root);
    let backend = ScriptBackend::new(vec![Ok(fact_reply()), Err(LlmFailure::BackendError)]);
    run_fact(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap_err();
    only_pending(root.memory())
}

fn pending_event(root: &Root) -> i64 {
    let plan = event_plan(root);
    let backend = ScriptBackend::new(vec![Ok(event_reply()), Err(LlmFailure::BackendError)]);
    run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap_err();
    only_pending(root.memory())
}

fn only_pending(m: &MemoryHandle) -> i64 {
    m.conn()
        .query_row("SELECT id FROM review_item WHERE state='pending_review'", [], |r| r.get(0))
        .unwrap()
}

/// 一次双判：proposer 在前、challenger 在后。
fn run_pair(root: &Root, id: i64, proposer: &str, challenger: &str) -> RunOutcome {
    let engine = ScriptBackend::new(vec![Ok(proposer.to_string()), Ok(challenger.to_string())]);
    run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap()
}

// ── 读数 ──────────────────────────────────────────────────────────────────────

/// `review_verdict` 的一行（原样列值，断言直接对字面量）。
struct VRow {
    item_id: i64,
    pass_id: String,
    turn_uid: String,
    reference: String,
    draft_index: i64,
    side: String,
    verdict: String,
    code: Option<String>,
    evidence: Option<String>,
    evidence_state: String,
    prompt_version: String,
    model: String,
}

fn rows(m: &MemoryHandle, item_id: i64) -> Vec<VRow> {
    let mut st = m
        .conn()
        .prepare(
            "SELECT item_id, pass_id, turn_uid, \"ref\", draft_index, side, verdict, code,
                    evidence, evidence_state, prompt_version, model
               FROM review_verdict WHERE item_id=?1 ORDER BY id",
        )
        .unwrap();
    st.query_map([item_id], |r| {
        Ok(VRow {
            item_id: r.get(0)?,
            pass_id: r.get(1)?,
            turn_uid: r.get(2)?,
            reference: r.get(3)?,
            draft_index: r.get(4)?,
            side: r.get(5)?,
            verdict: r.get(6)?,
            code: r.get(7)?,
            evidence: r.get(8)?,
            evidence_state: r.get(9)?,
            prompt_version: r.get(10)?,
            model: r.get(11)?,
        })
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

/// `review_verdict` 的总行数。表不在（还没写过/只读副本）⇒ 0——"没有这张表"与"空表"
/// 是同一个答案，用例不必先断言表存在。
fn total_rows(m: &MemoryHandle) -> i64 {
    let exists: i64 = m
        .conn()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='review_verdict'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    if exists == 0 {
        return 0;
    }
    m.conn().query_row("SELECT count(*) FROM review_verdict", [], |r| r.get(0)).unwrap()
}

fn rows_of_pass(m: &MemoryHandle, pass_id: &str) -> i64 {
    let exists: i64 = m
        .conn()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='review_verdict'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    if exists == 0 {
        return 0;
    }
    m.conn()
        .query_row("SELECT count(*) FROM review_verdict WHERE pass_id=?1", [pass_id], |r| r.get(0))
        .unwrap()
}

fn table_entries(m: &MemoryHandle) -> i64 {
    m.conn()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='review_verdict'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

// ── 1. 双判两侧 + final ───────────────────────────────────────────────────────

/// 一次双判跑完：`proposer`/`challenger` 各 N 行、`final` N 行；坐标（item/ref/下标/轮）、
/// 版本与模型逐列对得上；`final` 的 evidence 留空。
#[test]
fn a_fact_pair_run_stores_both_sides_and_the_final_ruling() {
    let (_dir, root, uids) = seeded_fact();
    let id = pending_fact(&root);
    let out = run_pair(
        &root,
        id,
        &fact_review(&[("f1", "pass", None, Some("对账单"))]),
        &fact_review(&[("f1", "pass", None, Some("记得确认"))]),
    );
    assert_eq!(out.item_state, ReviewState::Applied);

    let got = rows(root.memory(), id);
    assert_eq!(
        got.iter().map(|r| r.side.as_str()).collect::<Vec<_>>(),
        vec!["proposer", "challenger", "final"],
        "两侧原判各一行 + 裁决后 final 一行"
    );
    for row in &got {
        assert_eq!(row.item_id, id);
        assert_eq!(row.pass_id, "fact", "只写 fact");
        assert_eq!(row.turn_uid, uids[0], "该条草稿所属轮");
        assert_eq!(row.reference, "f1", "片内 ref");
        assert_eq!(row.draft_index, 0, "载荷 items[] 下标从 0 起");
        assert_eq!(row.model, MODEL);
        assert_eq!(row.prompt_version, PASS_FACT_REVIEW_VERSION);
    }
    assert_eq!(got[0].verdict, "pass");
    assert_eq!(got[0].code, None);
    assert_eq!(got[0].evidence.as_deref(), Some("对账单"));
    assert_eq!(got[0].evidence_state, "ok");
    assert_eq!(got[1].verdict, "pass");
    assert_eq!(got[1].evidence.as_deref(), Some("记得确认"));
    assert_eq!(got[1].evidence_state, "ok");
    assert_eq!(got[2].verdict, "pass", "两侧都 pass ⇒ final pass");
    assert_eq!(got[2].code, None);
    assert_eq!(got[2].evidence, None, "final 行 evidence 留空");
    assert_eq!(got[2].evidence_state, "none");
}

// ── 2. 证据闸降级：原样存 + 状态闭集 ──────────────────────────────────────────

/// 被证据闸降级的条目：该侧那行 `uncertain` + `evidence_state=not_in_quote`，
/// `evidence` 与模型给的**逐字节相同**（含前后空格与全角标点，不 trim、不改写）；
/// 另一侧证据合格 ⇒ `ok`；`final` 因任一侧 uncertain ⇒ `uncertain`、code 清空。
#[test]
fn a_downgraded_side_keeps_the_raw_evidence_and_its_state() {
    let (_dir, root, _uids) = seeded_fact();
    let id = pending_fact(&root);
    // 前导空格 + 尾随空格：`" 对账单 "` 不是 quote 的子串 ⇒ 降级；落库必须逐字节保留空格。
    out_of_quote(&root, id, " 对账单 ", "对账单发给你，记得");

    let got = rows(root.memory(), id);
    assert_eq!(got.len(), 3);
    assert_eq!(
        (got[0].side.as_str(), got[0].verdict.as_str(), got[0].evidence_state.as_str()),
        ("proposer", "uncertain", "not_in_quote"),
        "证据越界 ⇒ 该侧 uncertain + not_in_quote"
    );
    assert_eq!(
        got[0].evidence.as_deref(),
        Some(" 对账单 "),
        "模型给的原文原样存（不 trim）"
    );
    assert_eq!(got[0].code, None, "降级后 code 清空（那条 code 没有证据支撑）");
    assert_eq!(
        (got[1].side.as_str(), got[1].verdict.as_str(), got[1].evidence_state.as_str()),
        ("challenger", "pass", "ok")
    );
    assert_eq!(
        got[1].evidence.as_deref(),
        Some("对账单发给你，记得"),
        "含全角标点的原文原样存"
    );
    assert_eq!(
        (got[2].side.as_str(), got[2].verdict.as_str(), got[2].evidence_state.as_str()),
        ("final", "uncertain", "none"),
        "任一侧 uncertain ⇒ final uncertain；final 没有证据需求"
    );
    assert_eq!(got[2].evidence, None);
}

/// 缺证据（`evidence: null`）⇒ `missing`；两侧都降级 ⇒ 两条 not_in_quote/missing 都在，
/// `final` 仍只有一行（每草稿每侧恰一行）。
#[test]
fn a_missing_evidence_side_is_stored_as_missing() {
    let (_dir, root, _uids) = seeded_fact();
    let id = pending_fact(&root);
    let out = run_pair(
        &root,
        id,
        &fact_review(&[("f1", "pass", None, None)]),
        &fact_review(&[("f1", "reject", Some("not_durable"), None)]),
    );
    assert_eq!(out.item_state, ReviewState::Quarantined);

    let got = rows(root.memory(), id);
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].evidence_state, "missing");
    assert_eq!(got[0].evidence, None, "null 就是没有原文，不编一个空串出来");
    assert_eq!(got[1].evidence_state, "missing");
    assert_eq!(got[2].side, "final");
    assert_eq!(got[2].verdict, "uncertain");
}

// ── 3. 只写 fact ──────────────────────────────────────────────────────────────

/// 另一次 event 审核跑完：`review_verdict` 里 `pass_id='event'` 行数为 0（整表 0 行）。
#[test]
fn an_event_review_writes_no_verdict_rows() {
    let (_dir, root) = seeded_event();
    let id = pending_event(&root);
    let engine = ScriptBackend::new(vec![
        Ok(event_review(&[("e1", "pass", None)])),
        Ok(event_review(&[("e1", "pass", None)])),
    ]);
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(out.item_state, ReviewState::Applied, "event 审核照常跑完");
    assert_eq!(rows_of_pass(root.memory(), "event"), 0, "event 不写逐条判决");
    assert_eq!(total_rows(root.memory()), 0, "整表一条都没有（event 那次审核没有写过）");
}

// ── 4. 迁移幂等 ───────────────────────────────────────────────────────────────

/// 同一个库连开两次：第二次开库后再写一次逐条判决，不报错、不重复建表、不堆重复行
/// （写入是整条 item 覆盖）。
#[test]
fn the_verdict_table_is_created_idempotently_across_opens() {
    let dir = tempfile::tempdir().unwrap();
    let id;
    // 第一次开库：跑一次双判，表在这一刻被建出来。
    {
        let mut root = Root::open(dir.path()).unwrap();
        let uids = seed_bodies(&mut root, &[PROMISE, RECEIVED_BODY, "谢谢"], 1_000);
        set_owner(&mut root);
        let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
        add_anchor(&root, &scope, "person", "李四", &uids[2], NOW - 2_000);
        add_anchor(&root, &scope, "person", "张三", &uids[0], NOW - 1_000);
        id = pending_fact(&root);
        run_pair(
            &root,
            id,
            &fact_review(&[("f1", "pass", None, Some("对账单"))]),
            &fact_review(&[("f1", "pass", None, Some("对账单"))]),
        );
        assert_eq!(table_entries(root.memory()), 1);
        assert_eq!(rows(root.memory(), id).len(), 3);
    }

    // 第二次开库：同一张表、同一条 item。再写一次（同内容）⇒ 仍是 3 行、仍只有一张表。
    {
        let root = Root::open(dir.path()).unwrap();
        let m = root.memory();
        assert_eq!(table_entries(m), 1, "开库不重复建表");
        let old = rows(m, id);
        assert_eq!(old.len(), 3);
        let again: Vec<NewVerdict> = old
            .iter()
            .map(|row| NewVerdict {
                pass_id: row.pass_id.clone(),
                turn_uid: row.turn_uid.clone(),
                reference: row.reference.clone(),
                draft_index: row.draft_index,
                side: VerdictSide::parse(&row.side).unwrap(),
                verdict: ReviewVerdict::parse(&row.verdict).unwrap(),
                code: row.code.clone(),
                evidence: row.evidence.clone(),
                evidence_state: EvidenceState::parse(&row.evidence_state).unwrap(),
                prompt_version: row.prompt_version.clone(),
                model: row.model.clone(),
            })
            .collect();
        let written = replace_item_verdicts(m, id, &again, NOW + 1).unwrap();
        assert_eq!(written, 3);
        assert_eq!(table_entries(m), 1, "重放 DDL 是 IF NOT EXISTS，不会再建一张");
        assert_eq!(rows(m, id).len(), 3, "整条 item 覆盖，不堆重复行");
        assert_eq!(item_verdicts(m, id).unwrap().len(), 3);

        // 空批 = 清空该 item（合法输入），不报错。
        assert_eq!(replace_item_verdicts(m, id, &[], NOW + 2).unwrap(), 0);
        assert_eq!(total_rows(m), 0);
    }
}

// ── 5. 读取面 ────────────────────────────────────────────────────────────────

/// `item_verdicts` 按 `side`/`verdict` 过滤：§7.3 取"final=uncertain 的草稿清单"就靠这两道。
#[test]
fn item_verdicts_filters_by_side_and_verdict() {
    let (_dir, root, _uids) = seeded_fact();
    let id = pending_fact(&root);
    out_of_quote(&root, id, " 对账单 ", "对账单发给你，记得");
    let m = root.memory();

    let all = item_verdicts(m, id).unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all.iter().map(|r| r.side.slug()).collect::<Vec<_>>(),
               vec!["proposer", "challenger", "final"]);

    let finals = item_verdicts_filtered(m, id, Some(VerdictSide::Final), None).unwrap();
    assert_eq!(finals.len(), 1);
    let uncertain_finals = item_verdicts_filtered(
        m,
        id,
        Some(VerdictSide::Final),
        Some(ReviewVerdict::Uncertain),
    )
    .unwrap();
    assert_eq!(uncertain_finals.len(), 1);
    assert_eq!(uncertain_finals[0].reference, "f1");
    let proposer = item_verdicts_filtered(m, id, Some(VerdictSide::Proposer), None).unwrap();
    assert_eq!(proposer.len(), 1);
    assert_eq!(proposer[0].evidence.as_deref(), Some(" 对账单 "));
    assert_eq!(proposer[0].evidence_state, EvidenceState::NotInQuote);
    assert_eq!(
        item_verdicts_filtered(m, id, Some(VerdictSide::Challenger), Some(ReviewVerdict::Reject))
            .unwrap()
            .len(),
        0,
        "没有 reject 的那一侧"
    );

    // 不存在的 item ⇒ 空集（不是错误）。
    assert!(item_verdicts(m, id + 999).unwrap().is_empty());
}

// ── 共用的一步 ────────────────────────────────────────────────────────────────

/// proposer 的证据越界、challenger 的证据合格：一次双判 + 顺带断言 item 被隔离。
fn out_of_quote(root: &Root, id: i64, proposer_evidence: &str, challenger_evidence: &str) {
    let out = run_pair(
        root,
        id,
        &fact_review(&[("f1", "pass", None, Some(proposer_evidence))]),
        &fact_review(&[("f1", "pass", None, Some(challenger_evidence))]),
    );
    assert_eq!(
        out.item_state,
        ReviewState::Quarantined,
        "证据越界降 uncertain ⇒ 聚合 uncertain ⇒ 隔离"
    );
}
