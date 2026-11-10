// crates/hub/tests/review_retry_cli.rs
//! §7.3b：`hub review retry-uncertain` 的真进程面（派发档 `2026-10-29-7-3b`）。
//!
//! 夹具与 `review_run_cli.rs` 同源：先让 `distill once --pass fact` 把 item 停在
//! `quarantined`（§7.2 的两侧都判 uncertain），再用 `review retry-uncertain` 的 `--replies`
//! 逐条重问一次。断的是六条：
//!
//! 1. **队列**：只选 `state='quarantined'` 且 `review_verdict` 有 `side='final' AND
//!    verdict='uncertain'` 行的 fact item；审核坏 JSON 的隔离项（没有逐条判决）不进队列；
//! 2. **合成终态 · applied**：重跑全 pass ⇒ item applied（走既有 apply 路径写事实行）；
//! 3. **合成终态 · rejected**：任一条 reject ⇒ item rejected（code 取那条的）；
//! 4. **合成终态 · quarantined**：重跑仍 uncertain ⇒ 保持 quarantined，reason 仍是
//!    `review_uncertain`；`note` 落 artifact（程序只存不判分）；
//! 5. **证据越界**：重跑证据不在 quote 内 ⇒ 记 uncertain 并计入 `evidence_not_in_quote`；
//! 6. **三个率**：一次跑读出的率与手算一致，且 `review_decision.detail` 与 `--json` 是同一组数。
//!
//! 纪律与 `review_run_cli.rs` 同源：不跑真实 LLM（`--replies` 唯一来源）、stdout 不带正文/路径。
use rc_contract::{normalize, parse};
use rc_ledger::insert_batch;
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{AnchorRole, Provenance, Scope, ScopeKind};
use rc_store::Root;
use rusqlite::params;
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Output};

const DEV: &str = "dev-review-retry";
const SESS: &str = "sess-review-retry";
const DATA: &str = "data";
const NOW: i64 = 1_700_000_000_000;
/// 目标轮正文：断它**不出现在** stdout/stderr（守卫 12 的零正文）。
const CANARY: &str = "我下周三之前把对账单发给你";
/// 审核证据：`quote` 的子串（§7.2 的证据闸）。
const EVIDENCE: &str = "把对账单发给你";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin()).args(args).output().expect("hub binary")
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap()
}

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON")
}

fn stdout_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn data_dir(dir: &Path) -> String {
    dir.join(DATA).to_str().unwrap().to_string()
}

fn open(dir: &Path) -> Root {
    Root::open(&dir.join(DATA)).unwrap()
}

fn scalar(root: &Root, sql: &str) -> i64 {
    root.memory()
        .conn()
        .query_row(sql, [], |r| r.get(0))
        .unwrap()
}

/// 该 family 那一条 item 的 `(id, state)`。
fn item_of(root: &Root, family: &str) -> (i64, String) {
    root.memory()
        .conn()
        .query_row(
            "SELECT id, state FROM review_item WHERE pass_id='fact' AND family=?1",
            [family],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

fn item_state(root: &Root, id: i64) -> String {
    root.memory()
        .conn()
        .query_row("SELECT state FROM review_item WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .unwrap()
}

fn decision_stages(root: &Root, id: i64) -> Vec<String> {
    let mut st = root
        .memory()
        .conn()
        .prepare("SELECT stage FROM review_decision WHERE item_id=?1 ORDER BY id")
        .unwrap();
    st.query_map([id], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// 最后一条 `stage='review'` 判定的 `(verdict, reason_code, detail, prompt_version)`。
fn last_review(root: &Root, id: i64) -> (String, String, String, String) {
    root.memory()
        .conn()
        .query_row(
            "SELECT verdict, reason_code, detail, prompt_version FROM review_decision
              WHERE item_id=?1 AND stage='review' ORDER BY id DESC LIMIT 1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
}

/// `(side, verdict)` 行数：`side='final' AND verdict='uncertain'` 的条数（队列的判据）。
fn final_uncertain(root: &Root, id: i64) -> i64 {
    scalar(
        root,
        &format!(
            "SELECT count(*) FROM review_verdict
              WHERE item_id={id} AND side='final' AND verdict='uncertain'"
        ),
    )
}

/// 一条 fact 状态行（promise 族）：`(state, reason)`。
fn fact_turn(root: &Root, uid: &str, family: &str) -> (String, Option<String>) {
    root.ledger()
        .conn()
        .query_row(
            "SELECT state, reason FROM extract_pass_state
             WHERE turn_uid=?1 AND pass_id='fact' AND family=?2",
            params![uid, family],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

/// 播种三轮真转录 + 两个活锚（张三较新 ⇒ `a1`、李四较早 ⇒ `a2`），同上一个夹具。
fn seed(dir: &Path) -> Vec<String> {
    let doc = json!({
        "format_version": 1,
        "sessions": [{
            "session_key": SESS,
            "device_id": DEV,
            "source_agent": "agent-review-retry",
            "turns": [
                {"role": "human", "body": CANARY, "created_at_ms": NOW},
                {"role": "agent", "body": "好的，收到", "created_at_ms": NOW + 1},
                {"role": "human", "body": "谢谢", "created_at_ms": NOW + 2},
            ]
        }]
    });
    let file = parse(doc.to_string().as_bytes()).expect("夹具必须过契约");
    let rep = normalize(file, NOW + 100_000);
    let mut root = Root::open(&dir.join(DATA)).unwrap();
    let mut uids = Vec::new();
    for (i, a) in rep.accepted.iter().enumerate() {
        insert_batch(
            root.transcript_mut(),
            &a.session,
            std::slice::from_ref(&a.turn),
            NOW + i as i64,
        )
        .unwrap();
        uids.push(a.turn.turn_uid.clone());
    }
    assert_eq!(uids.len(), 3, "夹具的条数必须真是 3");
    root.transcript_mut()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id='p1', owner_agent_id='alice',
                    visibility='project', retention='keep',
                    scope_kind='project', scope_key='proj-x', ownership_src='operator'
             WHERE session_key=?1 AND device_id=?2",
            params![SESS, DEV],
        )
        .unwrap();

    let scope = Scope {
        kind: ScopeKind::Project,
        key: "proj-x".into(),
    };
    for (canonical, created_at_ms) in [("李四", NOW - 2_000), ("张三", NOW - 1_000)] {
        put_entity(
            root.memory(),
            &EntityContent {
                kind: "person".into(),
                title: canonical.into(),
                scope: scope.clone(),
                aliases: vec![],
            },
            &[Provenance {
                turn_uid: uids[0].clone(),
                anchor_role: AnchorRole::Human,
            }],
            created_at_ms,
        )
        .unwrap();
    }
    root.memory()
        .conn()
        .execute(
            "UPDATE resource_owner SET owner_agent_id='alice', owner_principal_id='p1',
                    visibility='project', scope_kind='project', scope_key='proj-x'
              WHERE mem_table='anchor'",
            [],
        )
        .unwrap();
    uids
}

/// 一条 fact 草稿（同一轮 `t1`）。`predicate`/`category` 与 family 逐字同值（闭集）。
fn extraction(family: &str, drafts: &[(&str, Option<&str>)]) -> Value {
    let facts: Vec<Value> = drafts
        .iter()
        .enumerate()
        .map(|(index, (value, object))| {
            let mut entry = json!({
                "ref": format!("f{}", index + 1), "turn": "t1", "subject": "a1",
                "predicate": family, "value": value, "category": family, "mode": "stated",
                "time_text": "下周三之前", "quote": CANARY, "evidence": ["t1"],
            });
            if let Some(object) = object {
                entry["object"] = json!(object);
            }
            entry
        })
        .collect();
    json!({
        "schema_version": 0, "pass": "fact", "family": family,
        "facts": facts, "empty_turns": ["t2", "t3"]
    })
}

fn extraction_one(family: &str) -> Value {
    extraction(family, &[("把对账单发给你", Some("a2"))])
}

fn extraction_two(family: &str) -> Value {
    extraction(
        family,
        &[("把对账单发给你", Some("a2")), ("下周三之前", None)],
    )
}

fn extraction_three(family: &str) -> Value {
    extraction(
        family,
        &[
            ("把对账单发给你", Some("a2")),
            ("下周三之前", None),
            ("对账单", None),
        ],
    )
}

fn reply(text: &Value) -> Value {
    Value::String(text.to_string())
}

/// 审核回复：逐条判决（proposer 与 challenger 同一份）。
fn review_reply(family: &str, verdicts: Vec<Value>) -> Value {
    json!({
        "schema_version": 0, "pass": "fact_review", "family": family, "verdicts": verdicts
    })
}

fn verdict_uncertain(reference: &str) -> Value {
    json!({"ref": reference, "verdict": "uncertain", "code": null, "evidence": null})
}

/// 重跑的单条回复（§2.2 的四键契约）：`--replies` 数组的元素是**模型原文串**。
fn retry_reply(verdict: &str, code: Value, evidence: Value, note: Value) -> Value {
    json!({"verdict": verdict, "code": code, "evidence": evidence, "note": note})
        .to_string()
        .into()
}

fn distill(dir: &Path, family: &str, replies: &[Value]) -> Output {
    let p = dir.join(format!("distill-{family}.json"));
    std::fs::write(&p, Value::Array(replies.to_vec()).to_string()).unwrap();
    let dd = data_dir(dir);
    run(&[
        "distill",
        "once",
        "--pass",
        "fact",
        "--family",
        family,
        "--replies",
        p.to_str().unwrap(),
        "--data-dir",
        &dd,
    ])
}

/// 一次「全部 uncertain」的审核：两侧同判 ⇒ item `quarantined`，§7.3a 落 final 行。
///
/// 审核**判成了** uncertain（判决成功、不是解析失败）⇒ 退出 0；隔离是队列状态，不是错误。
fn distill_all_uncertain(dir: &Path, family: &str, extraction: &Value, drafts: usize) -> i64 {
    let verdicts: Vec<Value> = (1..=drafts)
        .map(|i| verdict_uncertain(&format!("f{i}")))
        .collect();
    let both_sides = review_reply(family, verdicts);
    let out = distill(
        dir,
        family,
        &[reply(extraction), reply(&both_sides), reply(&both_sides)],
    );
    assert_eq!(
        code(&out),
        0,
        "判成 uncertain 不是失败退出：stderr {}",
        stderr_text(&out)
    );
    let root = open(dir);
    let (id, state) = item_of(&root, family);
    assert_eq!(state, "quarantined", "两侧都判 uncertain ⇒ 隔离");
    assert_eq!(
        final_uncertain(&root, id),
        drafts as i64,
        "§7.3a 的 final=uncertain 行"
    );
    id
}

/// `review retry-uncertain [--limit N] [--pass P] --replies <file> --json --data-dir <dd>`。
fn retry(
    dir: &Path,
    limit: Option<usize>,
    pass: Option<&str>,
    replies: Option<&[Value]>,
) -> Output {
    let dd = data_dir(dir);
    let mut args: Vec<String> = vec!["review".into(), "retry-uncertain".into()];
    if let Some(limit) = limit {
        args.push("--limit".into());
        args.push(limit.to_string());
    }
    if let Some(pass) = pass {
        args.push("--pass".into());
        args.push(pass.to_string());
    }
    if let Some(replies) = replies {
        let p = dir.join("retry-replies.json");
        std::fs::write(&p, Value::Array(replies.to_vec()).to_string()).unwrap();
        args.push("--replies".into());
        args.push(p.to_str().unwrap().to_string());
    }
    args.push("--data-dir".into());
    args.push(dd);
    args.push("--json".into());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run(&refs)
}

fn assert_no_body_or_path(out: &Output) {
    let text = stdout_text(out);
    assert!(!text.contains(CANARY), "stdout 带正文：{text}");
    assert!(!text.contains(DATA), "stdout 带路径：{text}");
    assert!(!stderr_text(out).contains(CANARY), "stderr 带正文");
}

// ── 1. 队列选择 ───────────────────────────────────────────────────────────────

/// 只有 `quarantined` 且有 `final=uncertain` 行的 fact item 进队列：审核坏 JSON 的隔离项
/// （两侧都没解析出来 ⇒ 没有逐条判决）不进队列，也不被这条命令碰到。
#[test]
fn the_retry_queue_takes_only_quarantined_items_whose_final_verdict_is_uncertain() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    distill_all_uncertain(dir.path(), "promise", &extraction_one("promise"), 1);
    // plan：两侧回坏 JSON ⇒ quarantined，但 §7.3a 一行都不写（没有逐条判决可落）。
    let out = distill(
        dir.path(),
        "plan",
        &[
            reply(&extraction_one("plan")),
            reply(&json!({})),
            reply(&json!({})),
        ],
    );
    assert_ne!(
        code(&out),
        0,
        "审核坏 JSON 不是成功退出；stdout {} / stderr {}",
        stdout_text(&out),
        stderr_text(&out)
    );
    {
        let root = open(dir.path());
        let (plan_id, state) = item_of(&root, "plan");
        assert_eq!(state, "quarantined");
        assert_eq!(
            scalar(&root, "SELECT count(*) FROM review_verdict"),
            3,
            "只有 promise 那一条（1 条草稿 = proposer + challenger + final 三行）"
        );
        assert_eq!(
            final_uncertain(&root, plan_id),
            0,
            "plan 没有 final=uncertain 行"
        );
    }

    let out = retry(
        dir.path(),
        None,
        None,
        Some(&[retry_reply(
            "pass",
            json!(null),
            json!(EVIDENCE),
            json!(null),
        )]),
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["items"], 1, "只重跑 promise：{doc}");
    assert_eq!(doc["retried_drafts"], 1);
    assert_eq!(
        doc["remaining_retryable"], 0,
        "plan 不进队列 ⇒ 没有遗留可重跑：{doc}"
    );
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    let (promise_id, promise_state) = item_of(&root, "promise");
    assert_eq!(promise_state, "applied");
    assert_eq!(
        fact_turn(&root, &uids[0], "promise"),
        ("done".to_string(), None)
    );
    let (plan_id, plan_state) = item_of(&root, "plan");
    assert_eq!(
        plan_state, "quarantined",
        "没有逐条判决的隔离项不许被这条命令改写"
    );
    assert_eq!(
        scalar(
            &root,
            &format!(
                "SELECT count(*) FROM review_decision WHERE item_id={plan_id} AND stage='review'"
            )
        ),
        1,
        "plan 只有原来那条解析失败的 review 判定"
    );
    assert_eq!(
        scalar(
            &root,
            &format!("SELECT count(*) FROM review_decision WHERE item_id={promise_id}")
        ),
        4,
        "promise：program + 首审 review + 重跑 review + apply"
    );
}

// ── 2. 合成终态：全部 pass ⇒ applied ─────────────────────────────────────────

/// 两条草稿首审都 uncertain（item 隔离）⇒ 重跑都 pass ⇒ item `applied`、两条事实行、
/// ledger done；判定链是 program/review(首审)/review(重跑)/apply。
#[test]
fn a_retry_that_passes_every_draft_applies_the_item_and_writes_its_rows() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let id = distill_all_uncertain(dir.path(), "promise", &extraction_two("promise"), 2);

    let out = retry(
        dir.path(),
        None,
        None,
        Some(&[
            retry_reply("pass", json!(null), json!(EVIDENCE), json!(null)),
            retry_reply("pass", json!(null), json!("下周三之前"), json!(null)),
        ]),
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["items"], 1);
    assert_eq!(doc["retried_drafts"], 2);
    assert_eq!(doc["retry_pass"], 2);
    assert_eq!(doc["retry_reject"], 0);
    assert_eq!(doc["retry_uncertain"], 0);
    assert_eq!(doc["applied"], 1);
    assert_eq!(doc["errors"], 0);
    assert_eq!(doc["remaining_retryable"], 0);
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(item_state(&root, id), "applied");
    assert_eq!(
        scalar(&root, "SELECT count(*) FROM fact"),
        2,
        "两条草稿各写一行"
    );
    assert_eq!(
        fact_turn(&root, &uids[0], "promise"),
        ("done".to_string(), None)
    );
    assert_eq!(
        decision_stages(&root, id),
        vec![
            "program".to_string(),
            "review".to_string(),
            "review".to_string(),
            "apply".to_string()
        ],
        "首审一条 review、重跑一条 review，apply 仍是既有那条路"
    );
    let (verdict, reason, detail, prompt_version) = last_review(&root, id);
    assert_eq!((verdict.as_str(), reason.as_str()), ("pass", "review_pass"));
    assert_eq!(
        prompt_version, "pass-fact-review-retry-v0.1",
        "重跑有自己的版本号"
    );
    assert!(
        detail.starts_with("retry=1;"),
        "detail 点明这是重跑那一格：{detail}"
    );
}

/// S4 的 retry 路径：重跑到 applied 也是一次 apply（`write_rows` 在这里写事实行与卡），
/// 所以卡必须与 `hub review run`、内联窄审同一条口径被提升为 `passed`。
///
/// 钉的是"每个 apply 入口都提升"：只把边界留在 `run_claimed` 的话，这条命令重跑通过而
/// 写下的卡会停在 `unreviewed`，只能等 audit 再花三票。
#[test]
fn 重跑全部通过写下的卡必须被提升为_passed() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let id = distill_all_uncertain(dir.path(), "promise", &extraction_two("promise"), 2);

    let out = retry(
        dir.path(),
        None,
        None,
        Some(&[
            retry_reply("pass", json!(null), json!(EVIDENCE), json!(null)),
            retry_reply("pass", json!(null), json!("下周三之前"), json!(null)),
        ]),
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    assert_eq!(stdout_json(&out)["applied"], 1);
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(item_state(&root, id), "applied", "夹具前提：重跑真的写库了");
    assert_eq!(scalar(&root, "SELECT count(*) FROM card"), 2, "两条草稿各写一张卡");
    assert_eq!(
        scalar(&root, "SELECT count(*) FROM card WHERE verdict='passed'"),
        2,
        "重跑 applied ⇒ 本次写下的两张卡都必须被提升（S4）"
    );
}

// ── 3. 合成终态：任一条 reject ⇒ rejected ────────────────────────────────────

/// 两条草稿首审 uncertain，重跑一条 pass、一条 reject ⇒ item `rejected`（code 取 reject 那条）。
#[test]
fn a_reject_in_the_retry_rejects_the_whole_item_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let id = distill_all_uncertain(dir.path(), "promise", &extraction_two("promise"), 2);

    let out = retry(
        dir.path(),
        None,
        None,
        Some(&[
            retry_reply("pass", json!(null), json!(EVIDENCE), json!(null)),
            retry_reply(
                "reject",
                json!("not_durable"),
                json!("下周三之前"),
                json!(null),
            ),
        ]),
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["rejected"], 1);
    assert_eq!(
        (doc["retry_pass"].as_u64(), doc["retry_reject"].as_u64()),
        (Some(1), Some(1))
    );
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(item_state(&root, id), "rejected");
    assert_eq!(
        scalar(&root, "SELECT count(*) FROM fact"),
        0,
        "驳回不写事实行"
    );
    let (verdict, reason, _, _) = last_review(&root, id);
    assert_eq!(
        (verdict.as_str(), reason.as_str()),
        ("reject", "not_durable"),
        "code 取那条的"
    );
    // 两条草稿同在一轮：逐轮终态沿用既有聚合（同一轮有一条 pass ⇒ 该轮 done），
    // item 本身仍是 rejected——这是 H3 的既有口径，重跑不改它。
    assert_eq!(
        fact_turn(&root, &uids[0], "promise"),
        ("done".to_string(), None),
        "同轮有一条 pass ⇒ 该轮 done（H3 的逐轮聚合）"
    );
}

// ── 4. 合成终态：仍 uncertain ⇒ 保持 quarantined ─────────────────────────────

/// 重跑仍判 uncertain ⇒ item 保持 `quarantined`，reason 仍是 `review_uncertain`；
/// 模型给的 `note`（缺哪类信息）落 artifact，程序只存不判分。
#[test]
fn a_retry_that_stays_uncertain_keeps_the_item_quarantined_and_stores_the_note() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let id = distill_all_uncertain(dir.path(), "promise", &extraction_one("promise"), 1);
    let note = "缺主语锚：候选表里没有对账单的收件人";

    let out = retry(
        dir.path(),
        None,
        None,
        Some(&[retry_reply(
            "uncertain",
            json!(null),
            json!(null),
            json!(note),
        )]),
    );
    assert_eq!(
        code(&out),
        4,
        "仍可重跑 ⇒ 4（与 review run 的 remaining 同一条）；stderr: {}",
        stderr_text(&out)
    );
    let doc = stdout_json(&out);
    assert_eq!(doc["quarantined"], 1);
    assert_eq!(doc["retry_uncertain"], 1);
    assert_eq!(
        doc["remaining_retryable"], 1,
        "仍然 uncertain ⇒ 下一次还能再重跑：{doc}"
    );
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(item_state(&root, id), "quarantined");
    assert_eq!(scalar(&root, "SELECT count(*) FROM fact"), 0);
    let (verdict, reason, _, prompt_version) = last_review(&root, id);
    assert_eq!(
        (verdict.as_str(), reason.as_str()),
        ("uncertain", "review_uncertain")
    );
    assert_eq!(prompt_version, "pass-fact-review-retry-v0.1");
    assert_eq!(
        fact_turn(&root, &uids[0], "promise"),
        (
            "quarantined".to_string(),
            Some("review_uncertain".to_string())
        )
    );
    // note 的唯一落点：重跑那一次的模型原文（artifact），程序只存不判分。
    let raw: String = root
        .ledger()
        .conn()
        .query_row(
            "SELECT raw FROM extract_pass_artifact
              WHERE pass_id='fact' AND stage='review' ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(raw.contains(note), "note 必须原样落 artifact：{raw}");
}

// ── 5. 证据越界 ⇒ 记 uncertain 并计数 ────────────────────────────────────────

/// 重跑的 pass 带上了 quote 之外的原文 ⇒ 程序降 uncertain（与 §7.2 同一条闸），
/// 计入 `evidence_not_in_quote`，item 保持 quarantined。
#[test]
fn a_retry_whose_evidence_is_outside_the_quote_is_downgraded_and_counted() {
    let dir = tempfile::tempdir().unwrap();
    let _uids = seed(dir.path());
    let id = distill_all_uncertain(dir.path(), "promise", &extraction_one("promise"), 1);

    let out = retry(
        dir.path(),
        None,
        None,
        // 「李四」不在 quote（CANARY）里。
        Some(&[retry_reply("pass", json!(null), json!("李四"), json!(null))]),
    );
    assert_eq!(
        code(&out),
        4,
        "降级后仍 uncertain ⇒ 还能重跑；stderr: {}",
        stderr_text(&out)
    );
    let doc = stdout_json(&out);
    assert_eq!(doc["retry_uncertain"], 1, "越界 ⇒ 降级成 uncertain：{doc}");
    assert_eq!(doc["retry_pass"], 0);
    assert_eq!(doc["evidence_not_in_quote"], 1);
    assert_eq!(doc["evidence_missing"], 0);
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(item_state(&root, id), "quarantined");
    let (verdict, reason, _, _) = last_review(&root, id);
    assert_eq!(
        (verdict.as_str(), reason.as_str()),
        ("uncertain", "review_uncertain")
    );
}

// ── 6. 三个率：程序记，手算一致 ─────────────────────────────────────────────

/// 一条 item 三条草稿（首审全 uncertain）：重跑分别 pass / reject / uncertain ⇒
/// item rejected；三个率的分母写清：
/// - `uncertain_rate` = 重跑前 uncertain 草稿数 / 该 item 载荷草稿数 = 3/3；
/// - `retry_pass_rate` / `retry_reject_rate` / `retry_still_uncertain_rate` 的分母 = 重跑条数 = 3。
#[test]
fn the_three_rates_are_computed_by_the_program_and_match_a_hand_count() {
    let dir = tempfile::tempdir().unwrap();
    let _uids = seed(dir.path());
    let id = distill_all_uncertain(dir.path(), "promise", &extraction_three("promise"), 3);

    let out = retry(
        dir.path(),
        None,
        None,
        Some(&[
            retry_reply("pass", json!(null), json!(EVIDENCE), json!(null)),
            retry_reply(
                "reject",
                json!("not_durable"),
                json!("下周三之前"),
                json!(null),
            ),
            retry_reply("uncertain", json!(null), json!(null), json!("缺时间锚")),
        ]),
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["drafts_total"], 3, "分母：该 item 的载荷草稿数");
    assert_eq!(
        doc["prescan_uncertain"], 3,
        "分子：重跑前 final=uncertain 的草稿数"
    );
    assert_eq!(doc["uncertain_rate"], json!(1.0), "3/3");
    assert_eq!(doc["retried_drafts"], 3, "分母：重跑条数");
    assert_eq!(doc["retry_pass"], 1);
    assert_eq!(doc["retry_reject"], 1);
    assert_eq!(doc["retry_uncertain"], 1);
    assert_eq!(doc["retry_pass_rate"], json!(0.3333), "1/3");
    assert_eq!(doc["retry_reject_rate"], json!(0.3333), "1/3");
    assert_eq!(doc["retry_still_uncertain_rate"], json!(0.3333), "1/3");
    assert_eq!(doc["rejected"], 1);
    assert_no_body_or_path(&out);

    // 同一组数也落 `review_decision`（重跑那一格）；`--json` 是同一份读数。
    let root = open(dir.path());
    assert_eq!(item_state(&root, id), "rejected");
    let (verdict, reason, detail, _) = last_review(&root, id);
    assert_eq!(
        (verdict.as_str(), reason.as_str()),
        ("reject", "not_durable")
    );
    for cell in [
        "retry=1",
        "pre_u=3",
        "pre_n=3",
        "u_rate=1.0000",
        "retried=3",
        "p=1",
        "r=1",
        "u=1",
        "p_rate=0.3333",
        "r_rate=0.3333",
        "su_rate=0.3333",
    ] {
        assert!(detail.contains(cell), "detail 缺 {cell}：{detail}");
    }
}

// ── 7. `--limit` 按 item 计数 + `--pass` 只做 fact ───────────────────────────

/// `--limit 1` 只重跑一条 item，另一条留在队列里（退出 4，`remaining_retryable=1`）；
/// `--pass plan` 是用法错误（重跑只做 fact）。
#[test]
fn the_limit_is_counted_in_items_and_only_fact_is_retried() {
    let dir = tempfile::tempdir().unwrap();
    let _uids = seed(dir.path());
    distill_all_uncertain(dir.path(), "promise", &extraction_one("promise"), 1);
    distill_all_uncertain(dir.path(), "plan", &extraction_one("plan"), 1);

    let out = retry(
        dir.path(),
        Some(1),
        None,
        Some(&[retry_reply(
            "pass",
            json!(null),
            json!(EVIDENCE),
            json!(null),
        )]),
    );
    assert_eq!(
        code(&out),
        4,
        "还有一条留给下一次；stderr: {}",
        stderr_text(&out)
    );
    let doc = stdout_json(&out);
    assert_eq!(doc["items"], 1, "--limit 按 item 计数：{doc}");
    assert_eq!(doc["retried_drafts"], 1);
    assert_eq!(doc["remaining_retryable"], 1);
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(item_of(&root, "promise").1, "applied");
    assert_eq!(item_of(&root, "plan").1, "quarantined");
    drop(root);

    let dd = data_dir(dir.path());
    let out = run(&[
        "review",
        "retry-uncertain",
        "--pass",
        "event",
        "--data-dir",
        &dd,
        "--json",
    ]);
    assert_eq!(
        code(&out),
        3,
        "非 fact 是用法错误：stderr {}",
        stderr_text(&out)
    );
    assert!(stdout_text(&out).is_empty(), "用法错误不许打读数");
    assert!(!stderr_text(&out).contains(DATA), "stderr 带路径");
}
