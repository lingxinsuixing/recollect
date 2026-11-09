//! S3c-3b：`review::run` 的 **rows 路径** E2E（派发档 `S3c-3b-rows-engine.md` §3/§5）。
//!
//! 断的是引擎本身：只凭入队的 `review.rows.v1` 载荷 + 转录库，把 rows 的窄审、判定、
//! 写库（`insert_fact_row`）与读数跑完。夹具风格同 `tests/review_rows_replay.rs`：
//! 真 `Root::open(tempdir)` + `common::seed_bodies` 落三轮转录，锚用测试内 SQL 直插
//! `anchor` 表（与 `review_gate_rows.rs` 同一套写法），fact 行按 hub 的列形状给全。
//!
//! 覆盖（与派发档 §5 一一对应）：
//! 1. happy 2 行（第二行带 conflict）⇒ fact 2 行、归属 2 行、item applied、判定链
//!    program/review/apply、review 的 prompt_version 与两枚指纹正确、user 含 text 与 `conflict:`；
//! 2. mixed（r1 pass、r2 reject）⇒ 只写 1 行、判定两条、item applied；
//! 3. 全 reject ⇒ 0 行、item rejected；
//! 4. LLM 失败 ⇒ `Err(Llm)`、item pending、0 行；
//! 5. Applied 重放（`RetryPolicy::RespectState`）⇒ 读数 0、行数不变；
//! 6. Approved（human pass）⇒ 不调模型、只写 pass 行；
//! 7. `mark_turns`/`mark_batch` 对 rows 无副作用（`extract_pass_state` 行数不变）。
//!
//! 8（附加）：主语锚查不到 ⇒ `Err(ReviewRows("review_ref_missing"))`、不调模型、0 行——
//! 这是 §3.2 唯一新增的确定性缺口，不放进上面 7 条里会无人守。
//!
//! 正反例只用通用占位名（张三/李四/发布/预算表），不含任何语料专名或 holdout。
use rc_distill::batch::{Batch, TurnRow};
use rc_distill::pass_prompt::rows_review_system;
use rc_distill::review::run::{
    mark_batch, mark_turns, run_item, run_item_with_policy, RetryPolicy, RunError, RunOutcome,
};
use rc_distill::review::{
    gate_rows, owner_json, record_program, rows_new_item, rows_payload, RowsCandidate, RowsRowInput,
    PROGRAM_PASS, ROWS_PIPELINE, ROWS_REVIEW_VERSION,
};
use rc_ledger::water;
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::review::{
    enqueue, get, record_decision, Decider, Decision, ReviewStage, ReviewState, ReviewVerdict,
};
use rc_memory::{BatchCtx, Scope, ScopeKind};
use rc_store::Root;
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

mod common;
use common::seed_bodies;

const NOW: i64 = 1_700_000_000_000;
const FROM: i64 = 1_000;
const SESSION: &str = "sess-batch-A";
const DEVICE: &str = "dev-batch-A";
const SOURCE_AGENT: &str = "agent-batch-A";
const RUN_ID: &str = "run-rows-1";
const MODEL: &str = "fake-rows-review";

/// 三轮正文：第一轮含 `12`/`3`（row 1 的数字证据），后两轮不含这两段数字。
/// 证据轮 `BODIES[0]` 与邻居轮（非证据）刻意不同：窄审渲染的证据正文只能是前者。
const BODIES: [&str; 3] = [
    "我决定发布推迟到 12 月 3 日",
    "张三负责跟进发布",
    "李四说预算表还没做完",
];

// ── 夹具 ──────────────────────────────────────────────────────────────────────

fn batch_of(uids: &[String]) -> Batch {
    assert_eq!(uids.len(), BODIES.len());
    Batch {
        session_key: SESSION.into(),
        device_id: DEVICE.into(),
        source_agent: SOURCE_AGENT.into(),
        project_hint: None,
        rows: uids
            .iter()
            .zip(BODIES)
            .enumerate()
            .map(|(i, (uid, body))| TurnRow {
                turn_uid: uid.clone(),
                role: if i % 2 == 0 { "human".into() } else { "agent".into() },
                body: (*body).into(),
                created_at_ms: FROM + i as i64,
                ingested_at_ms: FROM + i as i64,
                speaker: None,
                speaker_anchor_id: None,
            })
            .collect(),
    }
}

fn fixture_owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p1".into()),
        owner_agent_id: "alice".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}

fn fixture_scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-x".into() }
}

fn fixture_ctx() -> BatchCtx {
    BatchCtx { source_agent: SOURCE_AGENT.into(), device_id: DEVICE.into() }
}

/// 一行 fact 的原列（键集合与 hub 的 `allowed_columns("fact")` 同向）。
/// `category='promise'`、`mode='stated'` 都在闸门的闭集里。
fn row_values(id: &str, text: &str, evidence: &str, object: Option<&str>) -> Map<String, Value> {
    let mut row = json!({
        "id": id,
        "subject_anchor_id": "anchor-zhang",
        "predicate": "decide",
        "category": "promise",
        "mode": "stated",
        "text": text,
        "evidence_turn_uid": evidence,
        "value_text": "待定",
        "participants_json": "[]",
        "scope_kind": "project",
        "scope_key": "proj-x",
        "source_agent": SOURCE_AGENT,
        "device_id": DEVICE,
        "created_at_ms": FROM,
        "norm_version": 1,
    })
    .as_object()
    .expect("夹具的列值必须是对象")
    .clone();
    if let Some(object) = object {
        row.insert("object_anchor_id".to_string(), json!(object));
    }
    row
}

fn fixture_rows(uids: &[String]) -> Vec<RowsRowInput> {
    vec![
        RowsRowInput {
            values: row_values("fact-1", "发布推迟到 12 月 3 日", &uids[0], Some("anchor-lisi")),
            evidence_turn_uid: uids[0].clone(),
            conflict: None,
        },
        RowsRowInput {
            values: row_values("fact-2", "张三负责跟进发布", &uids[1], None),
            evidence_turn_uid: uids[1].clone(),
            // 既有事实的异值：同一主语同一谓词的另一个值（窄审渲染成 `conflict: pending`）。
            conflict: Some(json!({"value_text": "pending", "turn_uid": uids[0]})),
        },
    ]
}

fn insert_anchor(root: &Root, id: &str, canonical: &str) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                                status, created_at_ms, updated_at_ms, norm_version)
             VALUES(?1, 'person', ?2, 'real', 'project', 'proj-x', 'active', 1, 1, 1)",
            rusqlite::params![id, canonical],
        )
        .unwrap();
    root.memory()
        .conn()
        .execute(
            "INSERT INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                        created_by_agent, visibility, retention, scope_kind,
                                        scope_key, source_agent, device_id, created_at_ms)
             VALUES('anchor', ?1, 'p1', 'alice', ?2, 'project', 'keep', 'project', 'proj-x',
                    ?2, ?3, 1)",
            rusqlite::params![id, SOURCE_AGENT, DEVICE],
        )
        .unwrap();
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: Root,
    item_id: i64,
    payload_sha256: String,
}

/// 一条停在 `pending_review` 的 rows item：3 轮导入 → 锚 → 载荷/入队 → 逐行过闸 →
/// 记 `program/pass`（与 runner 同一条路：全过才记 pass，不过就当场红）。
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &BODIES, FROM);
    insert_anchor(&root, "anchor-zhang", "张三");
    insert_anchor(&root, "anchor-lisi", "李四");

    let batch = batch_of(&uids);
    let ctx = fixture_ctx();
    let owner = fixture_owner();
    let scope = fixture_scope();
    let rows = fixture_rows(&uids);
    let payload = rows_payload(RUN_ID, &batch, &ctx, &owner, &scope, &rows);
    let new = rows_new_item(&batch, owner_json(&owner, &scope), &payload);
    let queued = enqueue(root.memory(), &new, NOW).unwrap();
    let item = get(root.memory(), queued.id).unwrap().unwrap();
    assert_eq!(item.meta.pass_id, "rows");
    assert_eq!(item.meta.family, "");
    assert_eq!(item.meta.state, ReviewState::PendingReview);

    // 程序闸逐行过：任一行不过就不该继续（夹具坏了要当场红，不留给引擎兜）。
    for (index, row) in rows.iter().enumerate() {
        let cand = RowsCandidate {
            owner: owner.clone(),
            scope: scope.clone(),
            session_key: SESSION.into(),
            device_id: DEVICE.into(),
            turn_uids: uids.clone(),
            values: row.values.clone(),
        };
        let gate = gate_rows(root.memory(), root.transcript(), &cand, NOW).unwrap();
        assert!(gate.passed, "夹具第 {index} 行必须过闸，实际 {:?}", gate.code);
    }
    record_program(
        root.memory(),
        queued.id,
        ReviewVerdict::Pass,
        PROGRAM_PASS,
        &queued.payload_sha256,
        NOW,
    )
    .unwrap();

    Fixture {
        _dir: dir,
        root,
        item_id: queued.id,
        payload_sha256: queued.payload_sha256,
    }
}

/// rows 窄审回复：逐行判决（`pass` 的 code 必须是 null）。
fn rows_review(decisions: &[(&str, &str, Option<&str>)]) -> String {
    let verdicts: Vec<Value> = decisions
        .iter()
        .map(|(reference, verdict, code)| {
            json!({"ref": reference, "verdict": verdict, "code": code})
        })
        .collect();
    json!({"schema_version": 0, "pass": "rows_review", "verdicts": verdicts}).to_string()
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

    fn engine(review: String) -> Self {
        ScriptBackend::new(vec![Ok(review)])
    }

    fn silent() -> Self {
        ScriptBackend::new(Vec::new())
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

// ── 读数 ──────────────────────────────────────────────────────────────────────

fn count(root: &Root, table: &str) -> i64 {
    root.memory()
        .conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// `resource_owner` 里 `mem_table='fact'` 的行数：夹具的锚也各有一行归属，总的 `count` 不再
/// 等于"写了几条 fact"。
fn fact_owner_rows(root: &Root) -> i64 {
    root.memory()
        .conn()
        .query_row("SELECT count(*) FROM resource_owner WHERE mem_table='fact'", [], |r| r.get(0))
        .unwrap()
}

fn ledger_count(root: &Root, table: &str) -> i64 {
    root.ledger()
        .conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn item_state(root: &Root) -> String {
    root.memory()
        .conn()
        .query_row("SELECT state FROM review_item", [], |r| r.get(0))
        .unwrap()
}

fn fact_ids(root: &Root) -> Vec<String> {
    let mut st = root
        .memory()
        .conn()
        .prepare("SELECT id FROM fact ORDER BY id")
        .unwrap();
    st.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
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

fn decisions(root: &Root, item_id: i64) -> Vec<DecRow> {
    let mut st = root
        .memory()
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

/// 窄审渲染项的期望值不在这里重算：`rows_review_user` 是公开渲染函数，各条用例直接拿
/// 引擎捕获的 user 断言（text/conflict/证据正文都必须在场），不复制第二份模板。

// ── 1. happy 2 行（第二行带 conflict）─────────────────────────────────────────

#[test]
fn happy_two_rows_apply_both_and_match_the_review_prompt() {
    let f = fixture();
    let engine = ScriptBackend::engine(rows_review(&[
        ("r1", "pass", None),
        ("r2", "pass", None),
    ]));

    let out: RunOutcome =
        run_item(f.root.memory(), f.root.transcript(), f.root.ledger(), f.item_id, &engine, NOW)
            .unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);
    assert!(out.applied_now);
    assert_eq!((out.written, out.facts, out.cards, out.mentions), (2, 2, 0, 0));
    assert_eq!(out.duplicate, 0);
    assert_eq!(out.fail_closed_reason, None);
    assert_eq!(
        out.verdicts,
        Some(vec![
            ("r1".to_string(), ReviewVerdict::Pass),
            ("r2".to_string(), ReviewVerdict::Pass),
        ])
    );

    assert_eq!(fact_ids(&f.root), vec!["fact-1".to_string(), "fact-2".to_string()]);
    assert_eq!(fact_owner_rows(&f.root), 2, "每条新行一份归属");
    assert_eq!(item_state(&f.root), "applied");

    // 一条审核调用：2 行 ≤ REVIEW_MAX_ITEMS(10)，一片。
    let prompts = engine.prompts.borrow();
    assert_eq!(engine.calls.get(), 1);
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].system, rows_review_system(), "system 是 rows 窄审那一份");
    assert!(!prompts[0].derive);
    let user = &prompts[0].user;
    assert!(user.starts_with("PASS rows_review v0\n"));
    assert!(user.contains("ROW r1\n") && user.contains("ROW r2\n"));
    assert!(user.contains("text: 发布推迟到 12 月 3 日\n"));
    assert!(user.contains("text: 张三负责跟进发布\n"));
    // S3c-fix（复审 P1-b）：谓词逐行进 prompt（判据"同一主语同一谓词"的另一半）。
    assert_eq!(user.matches("predicate:").count(), 2, "两行各一条 predicate：{user}");
    assert!(user.contains("predicate: decide\n"), "行谓词没有渲染：{user}");
    assert!(user.contains("conflict: pending\n"), "第二行的异值必须渲染");
    assert!(
        user.contains("TURN:\n我决定发布推迟到 12 月 3 日"),
        "证据正文只取 evidence_turn_uid 那一轮"
    );
    assert!(
        !user.contains(BODIES[2]),
        "批内非证据轮不参与 rows 窄审"
    );

    // 判定链：program → review → apply。
    let rows = decisions(&f.root, f.item_id);
    assert_eq!(
        rows.iter().map(|r| r.stage.as_str()).collect::<Vec<_>>(),
        vec!["program", "review", "apply"]
    );
    assert_eq!((rows[0].verdict.as_str(), rows[0].reason_code.as_str()), ("pass", PROGRAM_PASS));
    assert_eq!((rows[1].verdict.as_str(), rows[1].reason_code.as_str()), ("pass", "review_pass"));
    assert_eq!(rows[1].detail, "p=2;r=0;u=0;pass=r1,r2;uncertain=-");
    assert_eq!(rows[1].model, MODEL);
    assert_eq!(rows[1].prompt_version, ROWS_REVIEW_VERSION);
    assert_eq!(rows[1].prompt_version, "row-support-review-v0.2");
    assert_eq!(rows[1].decider, "center");
    assert_eq!(rows[1].input_sha256, f.payload_sha256, "review 记的是载荷指纹");
    let fingerprint =
        water::extraction_fingerprint(ROWS_PIPELINE, MODEL, &prompts[0].system, &prompts[0].user);
    assert_eq!(rows[1].review_input_sha256, fingerprint, "送审指纹走 rows pipeline");
    assert_eq!(rows[2].reason_code, "applied");
    assert_eq!(rows[2].detail, "rows=2;duplicate=0");
    assert_eq!(rows[2].model, MODEL);
    assert_eq!(rows[2].prompt_version, ROWS_REVIEW_VERSION);
    assert_eq!(rows[2].input_sha256, f.payload_sha256);
    assert_eq!(rows[2].review_input_sha256, "", "apply 没有送审输入");

    // 水位：ledger 的 review_watermark 允许 rows（stats.by_pass 会带 rows）。
    let (pass_id, applied, max_id): (String, i64, i64) = f
        .root
        .ledger()
        .conn()
        .query_row(
            "SELECT pass_id, applied, max_decision_id FROM review_watermark WHERE pass_id='rows'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(pass_id, "rows");
    assert_eq!(applied, 1);
    assert_eq!(max_id, 3, "三条判定都已落库");
}

// ── 2. mixed（r1 pass、r2 reject）────────────────────────────────────────────

#[test]
fn mixed_writes_only_the_passing_row() {
    let f = fixture();
    let engine = ScriptBackend::engine(rows_review(&[
        ("r1", "pass", None),
        ("r2", "reject", Some("unsupported")),
    ]));

    let out =
        run_item(f.root.memory(), f.root.transcript(), f.root.ledger(), f.item_id, &engine, NOW)
            .unwrap();
    assert_eq!(out.item_state, ReviewState::Applied, "有 pass 行就写、就 applied");
    assert!(out.applied_now);
    assert_eq!((out.written, out.facts, out.cards, out.mentions), (1, 1, 0, 0));
    assert_eq!(out.verdicts, Some(vec![
        ("r1".to_string(), ReviewVerdict::Pass),
        ("r2".to_string(), ReviewVerdict::Reject),
    ]));

    assert_eq!(fact_ids(&f.root), vec!["fact-1".to_string()], "被 reject 的行不许落库");
    assert_eq!(fact_owner_rows(&f.root), 1);

    let rows = decisions(&f.root, f.item_id);
    assert_eq!(rows[1].reason_code, "review_mixed");
    assert_eq!(rows[1].detail, "p=1;r=1;u=0;pass=r1;uncertain=-");
    assert_eq!(rows[2].detail, "rows=1;duplicate=0");
}

// ── 3. 全 reject ─────────────────────────────────────────────────────────────

#[test]
fn all_reject_writes_nothing_and_rejects_the_item() {
    let f = fixture();
    let engine = ScriptBackend::engine(rows_review(&[
        ("r1", "reject", Some("invented_number")),
        ("r2", "reject", Some("wrong_subject")),
    ]));

    let out =
        run_item(f.root.memory(), f.root.transcript(), f.root.ledger(), f.item_id, &engine, NOW)
            .unwrap();
    assert_eq!(out.item_state, ReviewState::Rejected);
    assert!(!out.applied_now);
    assert_eq!((out.written, out.facts, out.cards, out.mentions), (0, 0, 0, 0));
    assert_eq!(count(&f.root, "fact"), 0);
    assert_eq!(fact_owner_rows(&f.root), 0);
    assert_eq!(item_state(&f.root), "rejected");

    let rows = decisions(&f.root, f.item_id);
    assert_eq!(
        rows.iter().map(|r| r.stage.as_str()).collect::<Vec<_>>(),
        vec!["program", "review"]
    );
    assert_eq!(rows[1].reason_code, "review_reject");
    assert_eq!(rows[1].detail, "p=0;r=2;u=0;pass=-;uncertain=-");
}

// ── 4. LLM 失败 ─────────────────────────────────────────────────────────────

#[test]
fn llm_failure_leaves_the_item_pending_and_writes_nothing() {
    let f = fixture();
    let engine = ScriptBackend::new(vec![Err(LlmFailure::BackendError)]);

    let err =
        run_item(f.root.memory(), f.root.transcript(), f.root.ledger(), f.item_id, &engine, NOW)
            .unwrap_err();
    assert!(matches!(err, RunError::Llm(LlmFailure::BackendError)), "{err:?}");
    assert_eq!(engine.calls.get(), 1);
    assert_eq!(item_state(&f.root), "pending_review", "审核没开口：item 留在 pending");
    assert_eq!(count(&f.root, "fact"), 0);
    let rows = decisions(&f.root, f.item_id);
    assert_eq!(rows.len(), 1, "只有 program pass，不记半份判定");
    assert_eq!(rows[0].stage, "program");
}

// ── 5. Applied 重放 ─────────────────────────────────────────────────────────

#[test]
fn applied_replay_is_a_read_only_no_op() {
    let f = fixture();
    let first = ScriptBackend::engine(rows_review(&[
        ("r1", "pass", None),
        ("r2", "pass", None),
    ]));
    let applied =
        run_item(f.root.memory(), f.root.transcript(), f.root.ledger(), f.item_id, &first, NOW)
            .unwrap();
    assert_eq!((applied.written, applied.facts), (2, 2));
    let decisions_before = decisions(&f.root, f.item_id).len();

    let again = ScriptBackend::silent();
    let out = run_item_with_policy(
        f.root.memory(),
        f.root.transcript(),
        f.root.ledger(),
        f.item_id,
        &again,
        NOW + 5_000,
        RetryPolicy::RespectState,
    )
    .unwrap();
    assert_eq!(again.calls.get(), 0, "applied 不再调模型");
    assert_eq!(out.item_state, ReviewState::Applied);
    assert!(!out.applied_now);
    assert_eq!((out.written, out.facts, out.cards, out.mentions), (0, 0, 0, 0));
    assert_eq!(out.duplicate, 0);
    assert_eq!(out.fail_closed_reason, None);
    assert_eq!(
        out.verdicts,
        Some(vec![
            ("r1".to_string(), ReviewVerdict::Pass),
            ("r2".to_string(), ReviewVerdict::Pass),
        ]),
        "逐条判决按台账重建"
    );
    assert_eq!(count(&f.root, "fact"), 2, "行数不变");
    assert_eq!(fact_owner_rows(&f.root), 2);
    assert_eq!(decisions(&f.root, f.item_id).len(), decisions_before, "apply 判定不增加");
}

// ── 6. Approved（human pass）─────────────────────────────────────────────────

#[test]
fn approved_retry_uses_the_persisted_verdict_and_writes_pass_rows() {
    let f = fixture();
    // 人工放行：pending_review + human pass ⇒ approved（写库留给引擎）。
    let decision = Decision {
        stage: ReviewStage::Human,
        verdict: ReviewVerdict::Pass,
        reason_code: "operator_ok".into(),
        rules_version: "review-gate-v0.1".into(),
        model: String::new(),
        prompt_version: String::new(),
        input_sha256: f.payload_sha256.clone(),
        review_input_sha256: String::new(),
        decider: Decider::Human,
        detail: "operator".into(),
    };
    record_decision(f.root.memory(), f.item_id, &decision, NOW + 1).unwrap();
    assert_eq!(item_state(&f.root), "approved");

    let engine = ScriptBackend::silent();
    let out =
        run_item(f.root.memory(), f.root.transcript(), f.root.ledger(), f.item_id, &engine, NOW + 2)
            .unwrap();
    assert_eq!(engine.calls.get(), 0, "approved 不调模型");
    assert_eq!(out.item_state, ReviewState::Applied);
    assert!(out.applied_now);
    assert_eq!((out.written, out.facts, out.cards, out.mentions), (2, 2, 0, 0));
    assert_eq!(
        out.verdicts,
        Some(vec![
            ("r1".to_string(), ReviewVerdict::Pass),
            ("r2".to_string(), ReviewVerdict::Pass),
        ])
    );
    assert_eq!(fact_ids(&f.root), vec!["fact-1".to_string(), "fact-2".to_string()]);
    let rows = decisions(&f.root, f.item_id);
    assert_eq!(
        rows.iter().map(|r| r.stage.as_str()).collect::<Vec<_>>(),
        vec!["program", "human", "apply"]
    );
    assert_eq!(rows[2].detail, "rows=2;duplicate=0");
}

// ── 7. mark_turns / mark_batch 对 rows 无副作用 ───────────────────────────────

#[test]
fn mark_turns_is_a_no_op_for_rows() {
    let f = fixture();
    let engine = ScriptBackend::engine(rows_review(&[
        ("r1", "pass", None),
        ("r2", "pass", None),
    ]));
    let out =
        run_item(f.root.memory(), f.root.transcript(), f.root.ledger(), f.item_id, &engine, NOW)
            .unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);

    assert_eq!(ledger_count(&f.root, "extract_pass_state"), 0, "rows 不写逐轮终态");
    let item = get(f.root.memory(), f.item_id).unwrap().unwrap();
    mark_turns(f.root.ledger(), &item, &out, NOW + 3).unwrap();
    assert_eq!(
        ledger_count(&f.root, "extract_pass_state"),
        0,
        "mark_turns 对 rows 必须直接 Ok(())"
    );
    // 批级终态的同一个入口：同样是 no-op（rows 的隔离/驳回不落 extract_pass_state）。
    mark_batch(
        f.root.ledger(),
        &item,
        rc_ledger::pass_state::PassState::Quarantined,
        Some("review_uncertain"),
        NOW + 4,
    )
    .unwrap();
    assert_eq!(ledger_count(&f.root, "extract_pass_state"), 0);
    assert_eq!(item_state(&f.root), "applied", "终态映射不改 item");
}

// ── 8（附加）. 主语锚查不到 ⇒ review_ref_missing ──────────────────────────────

#[test]
fn missing_subject_anchor_is_a_review_rows_gap() {
    let f = fixture();
    f.root
        .memory()
        .conn()
        .execute("DELETE FROM anchor WHERE id='anchor-zhang'", [])
        .unwrap();
    let engine = ScriptBackend::engine(rows_review(&[
        ("r1", "pass", None),
        ("r2", "pass", None),
    ]));

    let err =
        run_item(f.root.memory(), f.root.transcript(), f.root.ledger(), f.item_id, &engine, NOW)
            .unwrap_err();
    assert!(
        matches!(&err, RunError::ReviewRows(slug) if slug == "review_ref_missing"),
        "{err:?}"
    );
    assert_eq!(engine.calls.get(), 0, "缺口在开口之前判掉");
    assert_eq!(count(&f.root, "fact"), 0);
    assert_eq!(item_state(&f.root), "quarantined");
    let rows = decisions(&f.root, f.item_id);
    assert_eq!((rows[1].stage.as_str(), rows[1].verdict.as_str()), ("review", "uncertain"));
    assert_eq!(rows[1].reason_code, "review_ref_missing");
    assert_eq!(rows[1].decider, "center");
    assert_eq!(rows[1].input_sha256, f.payload_sha256);
    assert_eq!(rows[1].review_input_sha256, "", "没有送审输入");
}
