//! S3b-3b：`review::run` 的 **naming 路径** E2E（设计 `2026-10-26-review-queue-s3` §4/§5/§6）。
//!
//! 断的是引擎本身：只凭 runner 写下的载荷 + 转录库，把 naming 的窄审、判定、过滤与写库跑完。
//! 夹具风格同 `tests/review_wiring.rs`：真 `Root::open(tempdir)` + `mod common`，正反例
//! 只用通用占位名（张三/李四/老张/那台打印机），不含任何语料专名。
//!
//! **pending 的造法**（S3b-3c 起走真路径，见 [`pending_naming`]）：给 `run_naming` 的
//! 第 3 次调用（窄审）一个 `LlmFailure`——A1 双跑成功、入队与程序闸 pass、审核没开口，
//! 于是 item 停在 `pending_review`、0 锚、program 判定在、轮回 `pending`（60s 退避）。
//! 这等价于"runner 写完 `running`、审核还没开口"的那一刻，不再需要手工回滚行/状态。
//! 审核输入与 runner 的等价性用**同一套公开渲染函数**从 `select_naming_batch` + `parse_a1` +
//! `plan_a1` + `drop_noise_anchors` 重建后逐字节比对（见 [`expected_review_prompt`]），
//! 不读引擎自己的输出。
//!
//! 覆盖（与派发档 §5 一一对应）：
//! 1. pending 来源：runner 两次 A1 回复 + 第三次 `LlmFailure` ⇒ 调三次、item pending、0 锚；
//! 2. 载荷形状：`mentions[].anchor_ref` + `issues` + `unresolved` 四键（用真 A1 回复让后两键非空）；
//! 3. happy path：两锚全 pass ⇒ 锚/提及落库、item applied、三段判定、prompt 逐字节相等；
//! 4. mixed：一锚 pass、一锚 reject ⇒ 只落 pass 锚与它的提及，issues 过滤/丢弃，unresolved 全留；
//! 5. 全 reject ⇒ 0 锚、item rejected；
//! 6. 审核坏 JSON ⇒ `Err(ReviewNaming("review_json"))`、item quarantined、artifact 一行
//!    （H6：隔离码带 `review_` 前缀，判定与错误面同串；版本 v0.2）；
//! 7. Applied 重放 ⇒ `written=0`、`applied_now=false`、锚数不变；
//! 8. 旧载荷缺 `anchor_ref`/`issues`/`unresolved` ⇒ `Ok`、quarantined、`PAYLOAD_GAP`、0 锚；
//! 9. 引用校验：mention 的 `anchor_ref`/`speaker_ref`、锚证据的 `turn_uid` 指到批外 ⇒ `PAYLOAD_GAP`。

use rc_distill::pass_naming::{
    drop_noise_anchors, run_naming, run_naming_with, select_naming_batch, NamingPlan,
    NAMING_PIPELINE,
};
use rc_distill::pass_prompt::{naming_review_system, naming_review_user};
use rc_distill::review::run::{
    run_item, RunError, RunOutcome, PAYLOAD_GAP, REVIEW_INPUT_MAX_BYTES, REVIEW_INPUT_TOO_LARGE,
};
use rc_distill::review::{ReviewMode, PASS_NAMING_REVIEW_VERSION};
use rc_distill::{parse_a1, plan_a1, DistillError};
use rc_ledger::pass_state;
use rc_ledger::water;
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::review::ReviewState;
use rc_store::Root;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

mod common;
use common::seed_bodies;

const NOW: i64 = 1_700_000_000_000;
const MODEL: &str = "fake-naming-review";

/// 四轮正文：张三/老张（t1/t2）、李四（t3）、小结（t4）。四轮都留在批内。
const NAMING_BODIES: [&str; 4] = [
    "张三说明天把报价单发给你",
    "老张说报价单后天到，张三负责跟进",
    "李四说预算表还没做完",
    "小结：老张把报价单交给了张三",
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

fn seeded_naming() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &NAMING_BODIES, 1_000);
    set_owner(&mut root);
    (dir, root, uids)
}

fn naming_plan(root: &Root) -> NamingPlan {
    select_naming_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("夹具里 4 轮都还没跑 naming")
}

/// A1 回复：两个 person 锚（张三/老张、李四）、两条提及、两条 issue、一条 unresolved。
///
/// issue 的两条形状是为过滤规则量身定的：
/// - `quote_mismatch` 的 refs 同时指向 a1/a2 ⇒ 只 pass 一个时**过滤剩下一个**（不丢）；
/// - `conflict` 的 refs 只指向 a2 ⇒ a2 被 reject 时**整条丢弃**。
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

/// naming 窄审回复：逐锚判决（`pass` 的 code 必须是 null）。
fn naming_review(decisions: &[(&str, &str, Option<&str>)]) -> String {
    let verdicts: Vec<Value> = decisions
        .iter()
        .map(|(reference, verdict, code)| {
            json!({"ref": reference, "verdict": verdict, "code": code})
        })
        .collect();
    json!({"schema_version": 0, "pass": "naming_review", "verdicts": verdicts}).to_string()
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

    /// 引擎只发一次审核调用。
    fn engine(review: String) -> Self {
        ScriptBackend::new(vec![Ok(review)])
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

/// 审核输入的期望值：**不经过引擎的重放**，用 runner 同一套公开函数重算一次。
///
/// `select_naming_batch`（选批）→ `parse_a1`（两跑）→ `plan_a1`（合成）→
/// `drop_noise_anchors`（噪声闸）→ `naming_review_user`（渲染）。这正是 S3b-3c 会把
/// 第三次调用发出去的输入；本片 runner 还不发它，所以由测试代劳。
fn expected_review_prompt(plan: &NamingPlan, reply: &str) -> Prompt {
    let first = parse_a1(reply, &plan.batch).expect("A1 回复必须过契约");
    let second = parse_a1(reply, &plan.batch).expect("A1 回复必须过契约");
    let mut a1 = plan_a1(&first, &second, &plan.batch);
    drop_noise_anchors(&mut a1);
    Prompt {
        system: naming_review_system(),
        user: naming_review_user(&a1, &plan.batch),
        derive: false,
    }
}

// ── 读数 ──────────────────────────────────────────────────────────────────────

fn count(m: &rc_store::MemoryHandle, table: &str) -> i64 {
    m.conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn only_item(m: &rc_store::MemoryHandle) -> (i64, String, Option<String>, String) {
    m.conn()
        .query_row(
            "SELECT id, state, program_code, payload_json FROM review_item",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
}

fn item_state(m: &rc_store::MemoryHandle, id: i64) -> String {
    m.conn()
        .query_row("SELECT state FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap()
}

fn payload_of(m: &rc_store::MemoryHandle, id: i64) -> Value {
    let raw: String = m
        .conn()
        .query_row("SELECT payload_json FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap();
    serde_json::from_str(&raw).unwrap()
}

/// 合法改写载荷：同步用 `enqueue` 的同一条公式重算列指纹。
///
/// H4a 起引擎在 replay 之前复核 `payload_json` 与列 `payload_sha256`；只改载荷不改指纹
/// 就是篡改（会被 `payload_tampered` 拦下）。这些用例要测的是**载荷形状**（缺键/坏引用）
/// 与过滤，不是防篡改，所以这里按"重新落章"的合法路径改写。
fn set_payload(m: &rc_store::MemoryHandle, id: i64, payload: &Value) {
    let (owner_json, pass_id, family): (String, String, String) = m
        .conn()
        .query_row(
            "SELECT owner_json, pass_id, family FROM review_item WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    let payload_json = payload.to_string();
    let sha = rc_memory::review::payload_sha256(&pass_id, &family, &owner_json, &payload_json);
    m.conn()
        .execute(
            "UPDATE review_item SET payload_json=?2, payload_sha256=?3 WHERE id=?1",
            rusqlite::params![id, payload_json, sha],
        )
        .unwrap();
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

fn decisions(m: &rc_store::MemoryHandle, item_id: i64) -> Vec<DecRow> {
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

/// `(code, detail, run_id)`：写库层落的问题行（issues + unresolved 共用这张表）。
fn issue_rows(root: &Root) -> Vec<(String, String, String)> {
    let mut st = root
        .memory()
        .conn()
        .prepare("SELECT code, detail, run_id FROM extraction_issue ORDER BY code, id")
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// `(pass_id, family, run_id, turn_uid, raw)`：审核解析失败留下的 artifact 行。
fn review_artifact_rows(root: &Root) -> Vec<(String, String, String, String, String)> {
    let mut st = root
        .ledger()
        .conn()
        .prepare(
            "SELECT pass_id, family, run_id, turn_uid, raw FROM extract_pass_artifact
              WHERE stage='review' ORDER BY id",
        )
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn anchor_canonicals(m: &rc_store::MemoryHandle) -> Vec<String> {
    let mut st = m
        .conn()
        .prepare("SELECT canonical FROM anchor ORDER BY canonical")
        .unwrap();
    st.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

// ── pending 夹具（runner 真路径：第 3 次调用失败）──────────────────────────────

/// 一条停在 `pending_review` 的 naming item + 复核输入的期望值。
///
/// mock 是"两次 A1 回复 + 第三次 `LlmFailure`"：runner 现在会开口问窄审（S3b-3c），
/// 第三次失败 ⇒ 逐轮回到 `pending`（60s 退避）、item 留在 `pending_review`、0 锚，
/// 程序判定已在台账里。不需要手工回滚行或状态。
fn pending_naming(root: &Root) -> (i64, Vec<String>, Prompt) {
    let plan = naming_plan(root);
    let reply = a1_reply();
    let expected = expected_review_prompt(&plan, &reply);
    let backend = ScriptBackend::new(vec![
        Ok(reply.clone()),
        Ok(reply.clone()),
        Err(LlmFailure::BackendError),
    ]);
    let err =
        run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap_err();
    assert!(matches!(err, DistillError::Llm(LlmFailure::BackendError)), "{err}");
    assert_eq!(backend.calls.get(), 3, "A1 双跑 + 窄审一次（审核开口即失败）");
    assert_eq!(backend.replies.borrow().len(), 0, "第三次回复被消费");

    let (id, state, program_code, _) = only_item(root.memory());
    assert_eq!(state, "pending_review", "审核没开口：item 留在 pending");
    assert_eq!(program_code, None, "闸 pass 时 program_code 为 NULL");
    assert_eq!(count(root.memory(), "anchor"), 0, "失败批一条锚都不许留");
    let uids: Vec<String> =
        plan.batch.uids().into_iter().map(str::to_string).collect();
    (id, uids, expected)
}

// ── 1/2. pending 来源与载荷形状 ───────────────────────────────────────────────

/// 1. runner 侧：两次 A1 回复 + 第三次 `LlmFailure` ⇒ 调三次；item pending、0 锚，
///    轮回到 `pending`（60s 退避）而 run_id 仍是本批那枚（artifact/写库要同源）。
#[test]
fn runner_leaves_a_pending_naming_item_with_zero_anchors() {
    let (_dir, root, _uids) = seeded_naming();
    let (id, uids, expected) = pending_naming(&root);

    assert_eq!(item_state(root.memory(), id), "pending_review");
    assert_eq!(count(root.memory(), "anchor"), 0);
    assert_eq!(count(root.memory(), "mention"), 0);
    let (_, _, program_code, _) = only_item(root.memory());
    assert_eq!(program_code, None, "闸 pass 时 program_code 为 NULL");

    // 期望的审核输入按批内轮渲染：四轮都在，且 system 是命名审那一份 canary。
    assert!(expected.user.starts_with("PASS naming_review v0\n"));
    for uid in &uids {
        let key = pass_state::PassKey { turn_uid: uid, pass_id: "naming", family: "" };
        let record = pass_state::get(root.ledger(), key).unwrap().unwrap();
        assert_eq!(record.state, pass_state::PassState::Pending, "审核失败 ⇒ 轮等退避");
        assert_eq!(record.attempt, 1, "失败记一次尝试");
        assert_eq!(record.next_retry_at_ms, Some(NOW + 60_000), "60s 退避");
        assert_eq!(
            record.run_id.as_deref(),
            Some(format!("naming-{NOW}").as_str()),
            "runner 的 run_id 还在（artifact/写库要同源）"
        );
    }
}

/// 2. 载荷形状：四键齐全，`mentions[].anchor_ref` 逐条在场，issues/unresolved 非空且按 A1 顺序。
#[test]
fn payload_carries_anchor_ref_issues_and_unresolved() {
    let (_dir, root, _uids) = seeded_naming();
    let (id, uids, _expected) = pending_naming(&root);
    let payload = payload_of(root.memory(), id);

    assert_eq!(payload["pass_id"], "naming");
    assert_eq!(payload["family"], "");
    assert_eq!(payload["items"]["anchors"].as_array().unwrap().len(), 2);

    let mentions = payload["items"]["mentions"].as_array().unwrap();
    assert_eq!(mentions.len(), 2);
    assert_eq!(mentions[0]["anchor_ref"], "a1");
    assert_eq!(mentions[1]["anchor_ref"], "a2");
    assert_eq!(mentions[1]["speaker_ref"], "a1");
    assert_eq!(mentions[0]["surface"], "老张");
    assert_eq!(mentions[0]["turn_uid"], uids[1].as_str());

    let issues = payload["items"]["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 2, "issue 按 `a1.issues` 顺序直录");
    // `plan_a1` 的 issues 是 `BTreeMap<(kind, refs, reason)>` 的值序：`conflict` 在
    // `quote_mismatch` 之前。载荷照抄这个顺序，不在 `review_items` 里重排。
    assert_eq!(issues[0]["kind"], "conflict");
    assert_eq!(issues[0]["refs"], json!(["a2"]));
    assert_eq!(issues[0]["reason"], "只指向李四");
    assert_eq!(issues[1]["kind"], "quote_mismatch");
    assert_eq!(issues[1]["refs"], json!(["a1", "a2"]));
    assert_eq!(issues[1]["reason"], "两处口径不一致");

    let unresolved = payload["items"]["unresolved"].as_array().unwrap();
    assert_eq!(unresolved.len(), 1);
    assert_eq!(unresolved[0]["surface"], "那台打印机");
    assert_eq!(unresolved[0]["turn_uid"], uids[3].as_str(), "turn_uid 是 A1 契约映射后的真 uid");
    assert_eq!(unresolved[0]["reason"], "信息不足");

    // 四键之外没有第五个：载荷形状就是这四个键。
    let keys: Vec<&String> = payload["items"].as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), 4, "{keys:?}");
}

// ── 3. happy path ────────────────────────────────────────────────────────────

/// 3. 两锚全 pass ⇒ 锚/提及落库、item applied、三段判定、审核 prompt 与 runner 渲染逐字节相等。
#[test]
fn happy_path_applies_both_anchors_and_matches_the_review_prompt() {
    let (_dir, root, _uids) = seeded_naming();
    let (id, uids, expected) = pending_naming(&root);
    let engine =
        ScriptBackend::engine(naming_review(&[("a1", "pass", None), ("a2", "pass", None)]));

    let out: RunOutcome =
        run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);
    assert!(out.applied_now);
    assert_eq!(out.written, 2, "written 与 naming 的 anchors 同格");
    assert_eq!((out.facts, out.cards, out.mentions), (2, 0, 2));
    assert_eq!(out.fail_closed_reason, None);
    assert_eq!(
        out.verdicts,
        Some(vec![
            ("a1".to_string(), rc_memory::review::ReviewVerdict::Pass),
            ("a2".to_string(), rc_memory::review::ReviewVerdict::Pass),
        ])
    );

    assert_eq!(anchor_canonicals(root.memory()), vec!["张三".to_string(), "李四".to_string()]);
    assert_eq!(count(root.memory(), "alias"), 1, "老张 是 张三 的别名");
    assert_eq!(count(root.memory(), "mention"), 2);
    assert_eq!(count(root.memory(), "resource_owner"), 2, "每个锚一行归属");

    // 审核输入逐字节等价：system + user 与 runner 同一套渲染（测试侧重算）逐字相同。
    let prompts = engine.prompts.borrow();
    assert_eq!(prompts.len(), 1, "引擎只发一次审核调用");
    assert_eq!(prompts[0].system, expected.system, "system 必须逐字节相同");
    assert_eq!(prompts[0].user, expected.user, "user 必须逐字节相同");
    assert!(!prompts[0].derive);

    let rows = decisions(root.memory(), id);
    assert_eq!(
        rows.iter().map(|r| r.stage.as_str()).collect::<Vec<_>>(),
        vec!["program", "review", "apply"]
    );
    assert_eq!((rows[1].verdict.as_str(), rows[1].reason_code.as_str()), ("pass", "review_pass"));
    assert_eq!(rows[1].model, MODEL, "review 的 model 有值");
    assert_eq!(rows[1].prompt_version, PASS_NAMING_REVIEW_VERSION);
    assert_eq!(rows[1].decider, "center");
    assert_eq!(rows[2].reason_code, "applied");
    assert_eq!(rows[2].detail, "anchors=2;mentions=2", "apply 的 detail 只放计数");
    assert_eq!(rows[2].prompt_version, PASS_NAMING_REVIEW_VERSION);
    let payload_sha: String = root
        .memory()
        .conn()
        .query_row("SELECT payload_sha256 FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap();
    assert_eq!(rows[2].input_sha256, payload_sha, "apply 记的是载荷 sha");
    assert_eq!(rows[2].review_input_sha256, "", "apply 没有送审输入");

    // input_sha 与 runner 同一指纹式：拿引擎这次发出的 system/user 现算一遍。
    let fingerprint = water::extraction_fingerprint(
        NAMING_PIPELINE,
        MODEL,
        &prompts[0].system,
        &prompts[0].user,
    );
    // H4b：载荷指纹进 input_sha256，送审输入指纹进 review_input_sha256。
    assert_eq!(rows[1].input_sha256, payload_sha, "review 行的 input_sha256 是载荷指纹");
    assert_eq!(
        rows[1].review_input_sha256, fingerprint,
        "review_input_sha256 与命名审的指纹同源"
    );

    // 写库的 run_id 取状态行（`naming-<ms>`），不取载荷里那枚抽取指纹。
    let issue_run_ids = issue_rows(&root);
    assert_eq!(issue_run_ids.len(), 3, "两条 issue + 一条 unresolved");
    for (_, _, run_id) in &issue_run_ids {
        assert_eq!(run_id, &format!("naming-{NOW}"));
        assert_ne!(run_id.len(), 64, "不是 64 位载荷/抽取指纹");
    }
    let _ = uids;
}

// ── 4. mixed ─────────────────────────────────────────────────────────────────

/// 4. 一锚 pass、一锚 reject ⇒ 只落 pass 锚与它的提及；issues 过滤到 pass、空则丢；unresolved 全留。
#[test]
fn mixed_writes_only_the_passing_anchor_and_filters_issues() {
    let (_dir, root, uids) = seeded_naming();
    let (id, _, _expected) = pending_naming(&root);
    let engine = ScriptBackend::engine(naming_review(&[
        ("a1", "pass", None),
        ("a2", "reject", Some("not_verbatim")),
    ]));

    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);
    assert!(out.applied_now);
    assert_eq!(out.written, 1);
    assert_eq!((out.facts, out.cards, out.mentions), (1, 0, 1));
    assert_eq!(
        out.verdicts,
        Some(vec![
            ("a1".to_string(), rc_memory::review::ReviewVerdict::Pass),
            ("a2".to_string(), rc_memory::review::ReviewVerdict::Reject),
        ])
    );
    let rows = decisions(root.memory(), id);
    assert_eq!(rows[1].reason_code, "review_mixed");
    assert_eq!(rows[1].detail, "p=1;r=1;u=0;pass=a1;uncertain=-");
    assert_eq!(rows[2].detail, "anchors=1;mentions=1");

    assert_eq!(anchor_canonicals(root.memory()), vec!["张三".to_string()], "李四 不许落库");
    assert_eq!(count(root.memory(), "mention"), 1, "只落 a1 的那条提及");
    let surface: String = root
        .memory()
        .conn()
        .query_row("SELECT surface FROM mention", [], |r| r.get(0))
        .unwrap();
    assert_eq!(surface, "老张");

    // issue 过滤：quote_mismatch 的 refs 收到 [a1] 留下；conflict 只指向 a2 ⇒ 整条丢弃；
    // unresolved 不指向锚 ⇒ 全留（轮级）。W14e 起 detail 尾巴带程序侧候选数（本批没有
    // 这个表面的落点 ⇒ `cand=0`）。
    let issues = issue_rows(&root);
    assert_eq!(issues.len(), 2, "{issues:?}");
    assert_eq!(
        issues[0],
        (
            "quote_mismatch".to_string(),
            "refs=a1; reason=两处口径不一致".to_string(),
            format!("naming-{NOW}")
        )
    );
    assert_eq!(
        issues[1],
        (
            "unresolved_surface".to_string(),
            format!("turn={}; surface=那台打印机; reason=信息不足; cand=0", uids[3]),
            format!("naming-{NOW}")
        )
    );
}

/// 4b. 过滤的第 2 条后半：mention 活着但 `speaker_ref` 指向被 reject 的锚 ⇒ 提及留、
///     转述者置空（否则写库层 `by_ref[speaker]` 整批 UnknownAnchorRef）。
#[test]
fn mention_whose_speaker_was_rejected_keeps_the_mention_and_drops_the_speaker() {
    let (_dir, root, _uids) = seeded_naming();
    let (id, _, _expected) = pending_naming(&root);
    // 把第二条提及改成"a1 的提及、转述者是 a2"：a2 被 reject 时它必须留下、主语置空。
    let mut payload = payload_of(root.memory(), id);
    payload["items"]["mentions"][1]["anchor_ref"] = json!("a1");
    payload["items"]["mentions"][1]["speaker_ref"] = json!("a2");
    set_payload(root.memory(), id, &payload);

    let engine = ScriptBackend::engine(naming_review(&[
        ("a1", "pass", None),
        ("a2", "reject", Some("generic")),
    ]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(out.item_state, ReviewState::Applied);
    assert!(out.applied_now);
    assert_eq!((out.written, out.mentions), (1, 2), "两条提及都随 a1 落库");
    assert_eq!(anchor_canonicals(root.memory()), vec!["张三".to_string()]);

    let mut st = root
        .memory()
        .conn()
        .prepare("SELECT surface, speaker_anchor_id FROM mention ORDER BY surface")
        .unwrap();
    let rows = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, "李四", "被 reject 的锚不能再当主语，但提及本身属于 a1");
    assert_eq!(rows[0].1, None, "speaker_ref 置空（写库层查不到被丢的锚）");
    assert_eq!(rows[1].0, "老张");
    assert_eq!(rows[1].1, None, "原本就没有转述者");
}

// ── 5. 全 reject ─────────────────────────────────────────────────────────────
/// 5. 全 reject ⇒ 0 锚、0 提及、item rejected、只有 program/review 两条判定。
#[test]
fn all_reject_writes_nothing() {
    let (_dir, root, _uids) = seeded_naming();
    let (id, _, _expected) = pending_naming(&root);
    let engine = ScriptBackend::engine(naming_review(&[
        ("a1", "reject", Some("pronoun")),
        ("a2", "reject", Some("generic")),
    ]));

    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(out.item_state, ReviewState::Rejected);
    assert!(!out.applied_now);
    assert_eq!((out.written, out.facts, out.cards, out.mentions), (0, 0, 0, 0));
    assert_eq!(
        out.verdicts,
        Some(vec![
            ("a1".to_string(), rc_memory::review::ReviewVerdict::Reject),
            ("a2".to_string(), rc_memory::review::ReviewVerdict::Reject),
        ])
    );
    assert_eq!(count(root.memory(), "anchor"), 0);
    assert_eq!(count(root.memory(), "mention"), 0);
    assert_eq!(count(root.memory(), "extraction_issue"), 0);
    let rows = decisions(root.memory(), id);
    assert_eq!(rows.iter().map(|r| r.stage.as_str()).collect::<Vec<_>>(), vec!["program", "review"]);
    assert_eq!(rows[1].reason_code, "review_reject");
}

// ── 6. 坏审核输出 ───────────────────────────────────────────────────────────

/// 6. 审核坏 JSON ⇒ `Err(ReviewNaming(review_json))`、item quarantined、artifact(stage=review)
///    一行。H6：隔离码带 `review_` 前缀，与 fact/event 同口径；判定与错误面装的是同一个串。
#[test]
fn bad_review_json_quarantines_and_records_the_artifact() {
    let (_dir, root, uids) = seeded_naming();
    let (id, _, _expected) = pending_naming(&root);
    let engine = ScriptBackend::engine("{}".to_string());

    let err =
        run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap_err();
    assert!(
        matches!(&err, RunError::ReviewNaming(slug) if slug == "review_json"),
        "{err:?}"
    );
    assert_eq!(item_state(root.memory(), id), "quarantined");
    assert_eq!(count(root.memory(), "anchor"), 0);
    assert_eq!(count(root.memory(), "extraction_issue"), 0);

    let rows = decisions(root.memory(), id);
    assert_eq!((rows[1].stage.as_str(), rows[1].verdict.as_str()), ("review", "uncertain"));
    assert_eq!(rows[1].reason_code, "review_json");
    assert_eq!(rows[1].model, MODEL);
    assert_eq!(rows[1].prompt_version, PASS_NAMING_REVIEW_VERSION);
    assert_eq!(rows[1].prompt_version, "pass-naming-review-v0.2", "H6：模板语义变了，版本升 v0.2");

    let artifacts = review_artifact_rows(&root);
    assert_eq!(artifacts.len(), 1, "model 原文要留一份");
    assert_eq!(artifacts[0].0, "naming");
    assert_eq!(artifacts[0].1, "", "naming 没有 family");
    assert_eq!(artifacts[0].2, format!("naming-{NOW}"), "artifact 的 run_id 取状态行");
    assert_eq!(artifacts[0].3, uids[0], "turn_uid 是批内第一轮");
    assert_eq!(artifacts[0].4, "{}");
}

// ── 7. Applied 重放 ─────────────────────────────────────────────────────────

/// 7. Applied 重放 ⇒ `written=0`、`applied_now=false`、锚数不变、不再调模型、不加判定。
#[test]
fn applied_replay_is_a_read_only_no_op() {
    let (_dir, root, _uids) = seeded_naming();
    let (id, _, _expected) = pending_naming(&root);
    let first = ScriptBackend::engine(naming_review(&[("a1", "pass", None), ("a2", "pass", None)]));
    let applied = run_item(root.memory(), root.transcript(), root.ledger(), id, &first, NOW).unwrap();
    assert_eq!((applied.written, applied.mentions), (2, 2));
    assert_eq!(item_state(root.memory(), id), "applied");
    let anchors_before = count(root.memory(), "anchor");
    let decisions_before = decisions(root.memory(), id).len();

    let again = ScriptBackend::new(Vec::new());
    let out =
        run_item(root.memory(), root.transcript(), root.ledger(), id, &again, NOW + 5_000).unwrap();
    assert_eq!(again.calls.get(), 0, "applied 不再调模型");
    assert_eq!(out.item_state, ReviewState::Applied);
    assert!(!out.applied_now);
    assert_eq!((out.written, out.facts, out.cards, out.mentions), (0, 0, 0, 0));
    assert_eq!(
        out.verdicts,
        Some(vec![
            ("a1".to_string(), rc_memory::review::ReviewVerdict::Pass),
            ("a2".to_string(), rc_memory::review::ReviewVerdict::Pass),
        ]),
        "逐条判决照旧重建"
    );
    assert_eq!(out.fail_closed_reason, None);
    assert_eq!(count(root.memory(), "anchor"), anchors_before, "锚数不变");
    assert_eq!(decisions(root.memory(), id).len(), decisions_before, "apply 判定不增加");
}

// ── 8. 旧载荷 ───────────────────────────────────────────────────────────────

/// 8. 旧载荷（mention 没有 anchor_ref、items 没有 issues/unresolved）⇒ `Ok`、quarantined、0 锚。
#[test]
fn payload_without_the_new_keys_is_a_payload_gap() {
    let (_dir, root, _uids) = seeded_naming();
    let (id, _, _expected) = pending_naming(&root);
    let mut payload = payload_of(root.memory(), id);
    let items = payload["items"].as_object_mut().unwrap();
    items.remove("issues");
    items.remove("unresolved");
    items["mentions"][0].as_object_mut().unwrap().remove("anchor_ref");
    set_payload(root.memory(), id, &payload);

    let engine = ScriptBackend::engine(naming_review(&[("a1", "pass", None)]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(engine.calls.get(), 0, "gap 不调模型");
    assert_eq!(out.item_state, ReviewState::Quarantined);
    assert_eq!(out.verdicts, None);
    assert_eq!(out.fail_closed_reason, Some(PAYLOAD_GAP));
    assert_eq!((out.written, out.facts, out.cards, out.mentions), (0, 0, 0, 0));
    assert_eq!(count(root.memory(), "anchor"), 0);

    let rows = decisions(root.memory(), id);
    assert_eq!(rows.len(), 2, "program pass + review uncertain");
    assert_eq!(
        (rows[1].stage.as_str(), rows[1].verdict.as_str(), rows[1].reason_code.as_str()),
        ("review", "uncertain", PAYLOAD_GAP)
    );
    assert_eq!((rows[1].decider.as_str(), rows[1].model.as_str()), ("program", ""));
}

// ── 9. 引用校验 ─────────────────────────────────────────────────────────────

/// 9a. mention 的 `anchor_ref` 指向不存在的锚 ⇒ `PAYLOAD_GAP`（不调模型、0 锚）。
#[test]
fn mention_anchor_ref_outside_the_anchors_is_a_payload_gap() {
    let (_dir, root, _uids) = seeded_naming();
    let (id, _, _expected) = pending_naming(&root);
    let mut payload = payload_of(root.memory(), id);
    payload["items"]["mentions"][0]["anchor_ref"] = json!("a99");
    set_payload(root.memory(), id, &payload);

    let engine = ScriptBackend::engine(naming_review(&[("a1", "pass", None)]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(engine.calls.get(), 0);
    assert_eq!(out.item_state, ReviewState::Quarantined);
    assert_eq!(out.fail_closed_reason, Some(PAYLOAD_GAP));
    assert_eq!(count(root.memory(), "anchor"), 0);
}

/// 9b. `speaker_ref` 指向不存在的锚 ⇒ `PAYLOAD_GAP`（写库层会拿它查锚）。
#[test]
fn speaker_ref_outside_the_anchors_is_a_payload_gap() {
    let (_dir, root, _uids) = seeded_naming();
    let (id, _, _expected) = pending_naming(&root);
    let mut payload = payload_of(root.memory(), id);
    payload["items"]["mentions"][1]["speaker_ref"] = json!("a99");
    set_payload(root.memory(), id, &payload);

    let engine = ScriptBackend::engine(naming_review(&[("a1", "pass", None)]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(engine.calls.get(), 0);
    assert_eq!(out.item_state, ReviewState::Quarantined);
    assert_eq!(out.fail_closed_reason, Some(PAYLOAD_GAP));
    assert_eq!(count(root.memory(), "anchor"), 0);
}

/// 9c. 锚证据的 `turn_uid` 在批外 ⇒ `PAYLOAD_GAP`（审核 prompt 会渲染不出这一轮）。
#[test]
fn evidence_turn_outside_the_batch_is_a_payload_gap() {
    let (_dir, root, uids) = seeded_naming();
    let (id, _, _expected) = pending_naming(&root);
    let mut payload = payload_of(root.memory(), id);
    // 形状合法（64 位 hex）但不在本批：`0000..01` 不可能是夹具里的 uid。
    let outside = format!("{}1", "0".repeat(63));
    assert!(!uids.contains(&outside));
    payload["items"]["anchors"][0]["evidence"][0]["turn_uid"] = json!(outside);
    set_payload(root.memory(), id, &payload);

    let engine = ScriptBackend::engine(naming_review(&[("a1", "pass", None)]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(engine.calls.get(), 0);
    assert_eq!(out.item_state, ReviewState::Quarantined);
    assert_eq!(out.fail_closed_reason, Some(PAYLOAD_GAP));
    assert_eq!(count(root.memory(), "anchor"), 0);
}

// ── 10. H5a：超长审核输入 ───────────────────────────────────────────────────

/// 10. 批内有一轮正文 ≥ 300 KiB ⇒ 审核输入 `system + user` 超过
///     [`REVIEW_INPUT_MAX_BYTES`]：不调模型、不写库、`program/uncertain(review_input_too_large)`、
///     item → `quarantined`。三个 pass 共用引擎里同一段检查，这里钉 naming 那一支。
///
/// 造法走真路径：`EnqueueOnly` 只跑 A1 双抽（`--no-review`），长正文从导入起就在批里；
/// `naming_review_user` 会把批内**每一轮**正文整段渲染进 user。
#[test]
fn oversized_review_input_is_quarantined_without_calling_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    // 3 字节/汉字 ⇒ 120k 字 = 360 KB；原句在前，canonical/alias 仍逐字出现（程序闸 9c）。
    let long = format!("{}说：{}", "张三", "补".repeat(120_000));
    assert!(long.len() >= 300 * 1024, "夹具正文必须 ≥ 300 KiB，实际 {} 字节", long.len());
    let bodies = [long.as_str(), NAMING_BODIES[1], NAMING_BODIES[2], NAMING_BODIES[3]];
    let _uids = seed_bodies(&mut root, &bodies, 1_000);
    set_owner(&mut root);

    let plan = naming_plan(&root);
    let expected = expected_review_prompt(&plan, &a1_reply());
    let input_bytes = expected.system.len() + expected.user.len();
    assert!(
        input_bytes > REVIEW_INPUT_MAX_BYTES,
        "审核输入必须真的越过 {REVIEW_INPUT_MAX_BYTES} 字节，实际 {input_bytes} 字节"
    );

    let extractor = ScriptBackend::new(vec![Ok(a1_reply()), Ok(a1_reply())]);
    let queued = run_naming_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &extractor,
        NOW,
        ReviewMode::EnqueueOnly,
    )
    .unwrap();
    assert_eq!(extractor.calls.get(), 2, "EnqueueOnly 只跑 A1 双抽，不开窄审");
    assert_eq!(queued.state, pass_state::PassState::Pending);
    assert_eq!(count(root.memory(), "anchor"), 0, "EnqueueOnly 不许写锚");
    let (id, state, _program_code, _) = only_item(root.memory());
    assert_eq!(state, "pending_review", "长正文的批照样能入队");
    let payload_sha: String = root
        .memory()
        .conn()
        .query_row("SELECT payload_sha256 FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap();

    let engine = ScriptBackend::engine(naming_review(&[
        ("a1", "pass", None),
        ("a2", "pass", None),
    ]));
    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(engine.calls.get(), 0, "超长输入不许送模型");
    assert_eq!(out.item_state, ReviewState::Quarantined);
    assert_eq!(out.verdicts, None, "隔离不是判决，更不是全 pass");
    assert_eq!(out.fail_closed_reason, Some(REVIEW_INPUT_TOO_LARGE));
    assert_eq!((out.written, out.facts, out.cards, out.mentions), (0, 0, 0, 0));
    assert_eq!(count(root.memory(), "anchor"), 0, "隔离批一个锚都不写");
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
}

// ── 11. H5b：审核批 ≤10 分片 ────────────────────────────────────────────────
//
// 12 个锚 ⇒ 两次审核调用（10 + 2）。每个子批只含该片锚与其提及，片内 `a1..ak` 映射回
// 该锚的**原始 reference**；判定仍只有一条，写库时整批按全局 pass 集合过滤。

const CHUNK_ANCHORS: usize = 12;

fn chunk_name(index: usize) -> String {
    format!("甲{:02}", index + 1)
}

/// 12 个 person 锚、每个锚一条提及：canonical/surface 逐字出现在各自的轮里。
fn a1_reply_twelve() -> String {
    let anchors: Vec<Value> = (0..CHUNK_ANCHORS)
        .map(|index| {
            json!({
                "ref": format!("a{}", index + 1),
                "kind": "person",
                "canonical": chunk_name(index),
                "aliases": [],
                "reality_status": "real",
                "evidence": [format!("t{}", index + 1)],
                "reason": "一轮点名"
            })
        })
        .collect();
    let mentions: Vec<Value> = (0..CHUNK_ANCHORS)
        .map(|index| {
            json!({
                "anchor_ref": format!("a{}", index + 1),
                "surface": chunk_name(index),
                "turn_uid": format!("t{}", index + 1),
                "speaker_ref": null,
                "discourse_role": "mentioned",
                "resolution": "resolved",
                "confidence": 0.9
            })
        })
        .collect();
    json!({
        "schema_version": 4, "pass": "a1",
        "anchors": anchors, "mentions": mentions,
        "issues": [], "unresolved_surfaces": []
    })
    .to_string()
}

fn seeded_naming_twelve() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let bodies: Vec<String> = (0..CHUNK_ANCHORS)
        .map(|index| format!("{}说报价单已发出", chunk_name(index)))
        .collect();
    let borrowed: Vec<&str> = bodies.iter().map(String::as_str).collect();
    let uids = seed_bodies(&mut root, &borrowed, 1_000);
    // 12 轮全部改成 human 轮：`person` 锚要求证据轮里有一条 human（P0-2 的 9e），
    // 而 `seed_bodies` 按序号交替 human/agent ⇒ 半数锚只落在 agent 轮上会被闸拒，
    // 分片读数就不再是"12 个锚"。
    root.transcript_mut()
        .conn()
        .execute("UPDATE turns SET role='human' WHERE session_key='sess-batch-A'", [])
        .unwrap();
    set_owner(&mut root);
    (dir, root, uids)
}

/// 造一条停在 `pending_review` 的 12 锚 naming item（第 3 次调用=窄审失败）。
fn pending_naming_twelve(root: &Root) -> i64 {
    let plan = naming_plan(root);
    let reply = a1_reply_twelve();
    let backend = ScriptBackend::new(vec![
        Ok(reply.clone()),
        Ok(reply),
        Err(LlmFailure::BackendError),
    ]);
    let err =
        run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap_err();
    assert!(matches!(err, DistillError::Llm(LlmFailure::BackendError)), "{err}");
    assert_eq!(backend.calls.get(), 3, "A1 双跑 + 窄审一次（开口即失败）");
    let (id, state, program_code, _) = only_item(root.memory());
    assert_eq!(state, "pending_review");
    assert_eq!(program_code, None);
    assert_eq!(count(root.memory(), "anchor"), 0, "失败批一条锚都不许留");
    id
}

/// H5b-5. 12 个锚：两片、各段回复；断言 2 次调用、判定 1 条、全局 ref 映射正确、
/// 只写 pass 锚；提及跟随其锚的分片。
#[test]
fn twelve_anchors_are_reviewed_in_two_chunks_and_only_passing_anchors_are_written() {
    let (_dir, root, _uids) = seeded_naming_twelve();
    let id = pending_naming_twelve(&root);
    let first: [(&str, &str, Option<&str>); 10] = [
        ("a1", "pass", None),
        ("a2", "pass", None),
        ("a3", "pass", None),
        ("a4", "pass", None),
        ("a5", "pass", None),
        ("a6", "pass", None),
        ("a7", "pass", None),
        ("a8", "pass", None),
        ("a9", "pass", None),
        ("a10", "pass", None),
    ];
    let engine = ScriptBackend::new(vec![
        Ok(naming_review(&first)),
        Ok(naming_review(&[
            ("a1", "reject", Some("generic")),
            ("a2", "pass", None),
        ])),
    ]);

    let out = run_item(root.memory(), root.transcript(), root.ledger(), id, &engine, NOW).unwrap();
    assert_eq!(engine.calls.get(), 2, "12 个锚 ⇒ 10 + 2 两次审核调用");
    assert_eq!(out.item_state, ReviewState::Applied);
    assert!(out.applied_now);
    assert_eq!(out.written, 11, "只写 pass 锚（10 + 1）");
    assert_eq!((out.facts, out.cards, out.mentions), (11, 0, 11));
    let verdicts = out.verdicts.as_ref().expect("聚合判决");
    assert_eq!(verdicts.len(), 12, "逐条判决仍是 12 条（全局 ref）");
    assert_eq!(
        verdicts[10],
        ("a11".to_string(), rc_memory::review::ReviewVerdict::Reject),
        "第二片的 a1 ⇒ 全局 a11"
    );
    assert_eq!(
        verdicts[11],
        ("a12".to_string(), rc_memory::review::ReviewVerdict::Pass),
        "第二片的 a2 ⇒ 全局 a12"
    );

    // 一条判定（不是每片一条），detail 用全局 ref。
    let rows = decisions(root.memory(), id);
    assert_eq!(
        rows.iter().map(|r| r.stage.as_str()).collect::<Vec<_>>(),
        vec!["program", "review", "apply"]
    );
    assert_eq!(rows[1].reason_code, "review_mixed");
    assert_eq!(
        rows[1].detail,
        "p=11;r=1;u=0;pass=a1,a2,a3,a4,a5,a6,a7,a8,a9,a10,a12;uncertain=-"
    );
    assert_eq!(rows[2].detail, "anchors=11;mentions=11");

    // 全局 ref 映射：只写 pass 锚，a11（甲11）不落库。
    let canonicals = anchor_canonicals(root.memory());
    assert_eq!(canonicals.len(), 11);
    assert!(!canonicals.contains(&"甲11".to_string()), "被 reject 的锚不许落库");
    assert!(canonicals.contains(&"甲12".to_string()), "第二片 pass 的锚按全局 ref 写回");
    assert!(canonicals.contains(&"甲01".to_string()));

    // 提及跟随其锚的分片：a11 的提及不落库，其余 11 条落库。
    assert_eq!(count(root.memory(), "mention"), 11);
    let mut st = root
        .memory()
        .conn()
        .prepare("SELECT surface FROM mention ORDER BY surface")
        .unwrap();
    let surfaces: Vec<String> = st
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!surfaces.contains(&"甲11".to_string()));
    assert!(surfaces.contains(&"甲12".to_string()));

    // 分片边界：第一片只有 ANCHOR a1..a10；第二片把甲11/甲12 重新编成 a1/a2。
    let prompts = engine.prompts.borrow();
    assert_eq!(prompts.len(), 2);
    let anchors_section = |user: &str| {
        user.split("ANCHORS:\n")
            .nth(1)
            .expect("naming 审核 user 必有 ANCHORS 段")
            .to_string()
    };
    let first_section = anchors_section(&prompts[0].user);
    let second_section = anchors_section(&prompts[1].user);
    assert!(first_section.contains("ANCHOR a10\n"));
    assert!(!first_section.contains("甲11"), "第一片只含 a1..a10 的锚");
    assert!(second_section.contains("ANCHOR a1\n"));
    assert!(second_section.contains("ANCHOR a2\n"));
    assert!(!second_section.contains("ANCHOR a3"), "第二片只有两个锚");
    assert!(second_section.contains("甲11") && second_section.contains("甲12"));
    assert!(!second_section.contains("甲01"), "第二片不含第一片的锚");

    // 分片合成指纹：各片 user 按顺序 join。
    let users = [prompts[0].user.clone(), prompts[1].user.clone()];
    let expected = water::extraction_fingerprint(
        NAMING_PIPELINE,
        MODEL,
        &prompts[0].system,
        &users.join("\n\u{1e}\n"),
    );
    assert_eq!(rows[1].review_input_sha256, expected);
}
