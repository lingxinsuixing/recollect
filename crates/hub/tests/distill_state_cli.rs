// crates/hub/tests/distill_state_cli.rs
//! P2：`hub distill once --pass state` 的黑盒进程面。照 `distill_event_cli.rs` 的写法：
//! CLI 是这条 pass 的唯一出口，装配漏了一格（来源、选批、审核、计数器、退出码）只有子进程看得见。
//!
//! 纪律与 naming/fact/event 同源：
//! - **不跑真实 LLM**：`--replies` 是唯一来源；
//! - stdout 只许一个 JSON（守卫 12）：正文与 data-dir 路径都不许出现；
//! - state **不接受** `--family`（单一 pass，空串 family），也不接受 `--retry-empty`
//!   （v1 的聚合是"跑两遍"，不是 runner 内部重采）；给了就是用法错误（3，且不开库）；
//! - 读数比 event 多 `inferred`/`conflicts`/`states` 三格、少 `cards`/`retries`/`recovered` 三格。

use rc_contract::{normalize, parse};
use rc_ledger::insert_batch;
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{AnchorRole, Provenance, Scope, ScopeKind};
use rc_store::Root;
use rusqlite::params;
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Output};

const DEV: &str = "dev-state";
const SESS: &str = "sess-state";
const DATA: &str = "data";
const NOW: i64 = 1_700_000_000_000;
/// 目标轮正文：断它**不出现在** stdout/stderr（守卫 12 的零正文）。
const CANARY: &str = "张三这两天焦虑得很，晚上睡不着。";

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
    serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象")
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

fn count(root: &Root, table: &str, mem_table: Option<&str>) -> i64 {
    let sql = match mem_table {
        Some(m) => format!("SELECT count(*) FROM {table} WHERE mem_table='{m}'"),
        None => format!("SELECT count(*) FROM {table}"),
    };
    root.memory().conn().query_row(&sql, [], |r| r.get(0)).unwrap()
}

/// 一条 state 状态行：`(state, reason)`。`family` 恒空串。
fn pass_state(root: &Root, uid: &str) -> (String, Option<String>) {
    root.ledger()
        .conn()
        .query_row(
            "SELECT state, reason FROM extract_pass_state
             WHERE turn_uid=?1 AND pass_id='state' AND family=''",
            [uid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

fn llm_calls(root: &Root, pass: &str) -> i64 {
    root.ledger()
        .conn()
        .query_row(
            "SELECT coalesce(sum(count),0) FROM usage_counter
             WHERE subject_kind='llm_job' AND subject_id=?1 AND metric='llm_calls'",
            [pass],
            |r| r.get(0),
        )
        .unwrap()
}

/// S3a-fix：某一主语（`llm_job:<pass>`）的"最近一次运行是否触顶"格。
/// `None` = 这一 pass 从没写过（与 `Some(0)`＝"跑过且没触顶"是两件事）。
fn quota_stopped(root: &Root, subject: &str) -> Option<i64> {
    root.ledger()
        .conn()
        .query_row(
            "SELECT count FROM usage_counter
             WHERE subject_kind='llm_job' AND subject_id=?1 AND metric='quota_stopped'",
            [subject],
            |r| r.get(0),
        )
        .ok()
}

/// 播种两轮真转录（parse → normalize → insert_batch），归属改成非默认值，
/// 再铺一个活锚：张三（`a*` 的位置由证据预筛决定，脚本里按名字引用）。
fn seed(dir: &Path) -> Vec<String> {
    let doc = json!({
        "format_version": 1,
        "sessions": [{
            "session_key": SESS,
            "device_id": DEV,
            "source_agent": "agent-state",
            "turns": [
                {"role": "human", "body": CANARY, "created_at_ms": NOW},
                {"role": "agent", "body": "好的，收到", "created_at_ms": NOW + 1},
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
    assert_eq!(uids.len(), 2, "夹具的条数必须真是 2");
    let changed = root
        .transcript_mut()
        .conn()
        .execute(
            "UPDATE sessions SET owner_agent_id='alice', visibility='project',
                    retention='keep', scope_kind='project', scope_key='proj-x',
                    ownership_src='operator'
             WHERE session_key=?1 AND device_id=?2",
            params![SESS, DEV],
        )
        .unwrap();
    assert_eq!(changed, 1, "归属那一行必须真的被改到");

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
    root.memory()
        .conn()
        .execute(
            "UPDATE resource_owner SET owner_agent_id='alice', visibility='project',
                    scope_kind='project', scope_key='proj-x'
              WHERE mem_table='anchor'",
            [],
        )
        .unwrap();
    uids
}

/// t1 的一条直陈：subject 张三、维度 emotion、值逐字在引文里。
fn state_reply(value: &str, quote: &str) -> Value {
    json!({
        "schema_version": 0, "pass": "state",
        "states": [{
            "ref": "s1", "turn": "t1", "subject": "a1", "dimension": "emotion",
            "value_after": value, "quote": quote, "evidence": ["t1"]
        }],
        "empty_turns": ["t2"]
    })
}

fn review_pass() -> Value {
    json!({"schema_version": 0, "pass": "state_review",
           "verdicts": [{"ref": "s1", "verdict": "pass", "code": null}]})
}

fn review_reject() -> Value {
    json!({"schema_version": 0, "pass": "state_review",
           "verdicts": [{"ref": "s1", "verdict": "reject", "code": "value_drift"}]})
}

fn reply(text: &Value) -> Value {
    Value::String(text.to_string())
}

fn distill(dir: &Path, extra: &[&str], replies: &[Value]) -> Output {
    let p = dir.join("state-replies.json");
    std::fs::write(&p, Value::Array(replies.to_vec()).to_string()).unwrap();
    let dd = data_dir(dir);
    let mut args: Vec<&str> = vec!["distill", "once", "--pass", "state"];
    args.extend_from_slice(extra);
    args.push("--replies");
    args.push(p.to_str().unwrap());
    args.push("--data-dir");
    args.push(&dd);
    run(&args)
}

fn assert_no_body_or_path(out: &Output) {
    let text = stdout_text(out);
    assert!(!text.contains(CANARY), "stdout 带正文：{text}");
    assert!(!text.contains(DATA), "stdout 带路径：{text}");
    assert!(!stderr_text(out).contains(CANARY), "stderr 带正文");
}

/// 1. 成功一批：exit 0、单 JSON（十六格）、`state_assertion` 与出处/提及/归属各一行、
///    每轮 done、`llm_job:state` 记 2。
#[test]
fn a_state_pass_writes_row_provenance_owner_and_done_states() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let out = distill(dir.path(), &[], &[reply(&state_reply("焦虑得很", CANARY)), reply(&review_pass())]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let json = stdout_json(&out);
    assert_eq!(json["pass"], "state");
    assert_eq!(json["turns"], 2);
    assert_eq!(json["drafts"], 1);
    assert_eq!(json["written"], 1);
    assert_eq!(json["states"], 1);
    assert_eq!(json["mentions"], 1);
    assert_eq!(json["inferred"], 0);
    assert_eq!(json["conflicts"], 0);
    assert_eq!(json["state"], "done");
    // 没有 family 格、没有 cards 格：状态是单一 pass，也不产卡。
    assert!(json.get("family").is_none());
    assert!(json.get("cards").is_none());
    // W19c2b：读数面多一格 `retry_exhausted`（16→17），它只区分"重试耗尽"与其它隔离。
    assert_eq!(json["retry_exhausted"], 0, "正常成功的一批不许带重试耗尽：{json}");
    // W33-A10：观测面再追两格（17→19）；stub 不发明耗时 ⇒ max_call_ms 恒 0。
    assert_eq!(json["max_call_ms"], 0, "stub 不发明传输耗时：{json}");
    assert_eq!(json["timeouts"], 0, "没有显式超时 ⇒ 计数 0：{json}");
    assert_eq!(json.as_object().unwrap().len(), 19, "读数格子数漂了：{json}");

    let root = open(dir.path());
    assert_eq!(count(&root, "state_assertion", None), 1);
    assert_eq!(count(&root, "provenance", Some("state_assertion")), 1);
    assert_eq!(count(&root, "memory_mention", Some("state_assertion")), 1);
    assert_eq!(count(&root, "resource_owner", Some("state_assertion")), 1);
    assert_eq!(count(&root, "card", None), 0);
    assert_eq!(pass_state(&root, &uids[0]), ("done".to_string(), None));
    assert_eq!(pass_state(&root, &uids[1]), ("empty".to_string(), None));
    assert_eq!(llm_calls(&root, "state"), 2);
    // S3a-fix：state 也写 `llm_calls`，所以它同样是一次"记过账的运行"——收尾必须落触顶位 0。
    assert_eq!(quota_stopped(&root, "state"), Some(0), "没触顶的一轮必须把这一格写成 0");
    assert_no_body_or_path(&out);
}

/// S3a-fix 阳性（评审 I-2）：`--pass state` 触顶时必须把 `llm_job:state` 写成 1。
/// 变异电池：删掉 `run_distill_state` 收尾的 `record_quota_stopped(...)` ⇒ 这一格停在清 0 的 0 ⇒ 红。
/// 这条用例存在的理由是"state 从前一个字节都不写"：那种实现下 `status` 会把上一个 pass 的 0
/// 当成"最近一次运行"的结论。
#[test]
fn a_quota_stop_marks_the_state_cell_and_no_other() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let out = distill(dir.path(), &[], &[json!({"fail": "quota_exhausted"})]);
    assert_eq!(code(&out), 3, "触顶是 LLM 面故障（退出 3）：{}", stderr_text(&out));
    assert!(
        stderr_text(&out).contains("llm quota_exhausted"),
        "stderr 少了四态那一格：{}",
        stderr_text(&out)
    );

    let root = open(dir.path());
    assert_eq!(quota_stopped(&root, "state"), Some(1),
        "触顶那一轮必须落在 `llm_job:state` 自己的格上（主语写错＝这一格没有行）");
    assert_eq!(quota_stopped(&root, "naming"), None,
        "主语写成别的 pass 会在这里多出一行：这一格必须是空的");
}

/// S3a-fix 阴性（评审 I-1 的 clear 半边）：空批是早退，走不到收尾那一句 ⇒ 这一格只能由开头
/// 那一次清 0 写。种 1 再跑空批：删掉 `clear_quota_stopped` 那一行，这一格留在 1 ⇒ 红。
#[test]
fn an_empty_batch_clears_the_previous_runs_quota_cell() {
    let dir = tempfile::tempdir().unwrap();
    // 空库：没有候选轮 ⇒ `Ok(None)` ⇒ 早退。
    let root = open(dir.path());
    rc_ledger::counters::set_one(
        root.ledger(), "llm_job", "state", rc_ledger::counters::QUOTA_STOPPED_METRIC, 1, NOW,
    )
    .unwrap();
    assert_eq!(quota_stopped(&root, "state"), Some(1), "夹具必须先真的种上 1");
    drop(root);

    let out = distill(dir.path(), &[], &[]);
    assert_eq!(code(&out), 0, "空批是合法结果：{}", stderr_text(&out));
    assert_eq!(stdout_json(&out)["state"], "none");

    let root = open(dir.path());
    assert_eq!(quota_stopped(&root, "state"), Some(0),
        "早退的运行也必须把上一次的 1 清掉（否则 status 会一直报上次的触顶）");
}

/// 2. 审核 reject：一行不写，该轮 `rejected(review_reject)`。
#[test]
fn a_rejected_draft_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let out = distill(dir.path(), &[], &[reply(&state_reply("焦虑得很", CANARY)), reply(&review_reject())]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let json = stdout_json(&out);
    assert_eq!(json["states"], 0);
    assert_eq!(json["rejected"], 1);
    assert_eq!(json["state"], "rejected");
    let root = open(dir.path());
    assert_eq!(count(&root, "state_assertion", None), 0);
    assert_eq!(pass_state(&root, &uids[0]).1.as_deref(), Some("review_reject"));
}

/// 3. `--no-review`：入队但不问模型、不写行（队列里等 `review run --pass state`）。
#[test]
fn no_review_leaves_the_item_queued_and_the_row_unwritten() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let out = distill(dir.path(), &["--no-review"], &[reply(&state_reply("焦虑得很", CANARY))]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let json = stdout_json(&out);
    assert_eq!(json["state"], "pending");
    assert_eq!(json["states"], 0);
    let root = open(dir.path());
    assert_eq!(count(&root, "state_assertion", None), 0);
    assert_eq!(pass_state(&root, &uids[0]).1.as_deref(), Some("review_pending"));
    let queued: i64 = root
        .memory()
        .conn()
        .query_row("SELECT count(*) FROM review_item WHERE pass_id='state'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(queued, 1);
    assert_eq!(llm_calls(&root, "state"), 1);
}

/// 4. 阴性对照的进程面：问句被程序闸挡在落库之前，而且一次审核都不问。
#[test]
fn a_question_is_refused_by_the_program_gate_without_a_review_call() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let question = "你是不是还在生气？";
    Root::open(&dir.path().join(DATA))
        .unwrap()
        .transcript_mut()
        .conn()
        .execute(
            "UPDATE turns SET body=?1 WHERE session_key=?2 AND role='human'",
            params![question, SESS],
        )
        .unwrap();
    let out = distill(
        dir.path(),
        &[],
        &[reply(&state_reply("生气", question))],
    );
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let json = stdout_json(&out);
    assert_eq!(json["drafts"], 0, "问句不该成为可审的草稿：{json}");
    assert_eq!(json["dropped"], 1);
    assert_eq!(json["states"], 0);
    let root = open(dir.path());
    assert_eq!(count(&root, "state_assertion", None), 0);
    assert_eq!(llm_calls(&root, "state"), 1, "没有草稿就不该问第二次");
    let first = pass_state(&root, &uids[0]);
    assert_eq!((first.0.as_str(), first.1.as_deref()), ("rejected", Some("not_assertive")));
}

/// 5. 推导值（值不在引文里）走隔离：`inferred=1`、该轮 quarantined、库里零行。
#[test]
fn a_reworded_value_is_quarantined_not_written() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let out = distill(dir.path(), &[], &[reply(&state_reply("压力很大", CANARY))]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let json = stdout_json(&out);
    assert_eq!(json["inferred"], 1);
    assert_eq!(json["drafts"], 0);
    assert_eq!(json["state"], "quarantined");
    let root = open(dir.path());
    assert_eq!(count(&root, "state_assertion", None), 0);
    assert_eq!(
        pass_state(&root, &uids[0]),
        ("quarantined".to_string(), Some("inferred_deferred".to_string()))
    );
}

/// 6. 用法面：`--family` 与 `--retry-empty` 都不属于这一条 pass，且**不开库**。
#[test]
fn state_takes_no_family_and_no_retry_empty() {
    let dir = tempfile::tempdir().unwrap();
    let dd = data_dir(dir.path());
    let with_family = run(&["distill", "once", "--pass", "state", "--family", "decision",
                            "--data-dir", &dd]);
    assert_eq!(code(&with_family), 3);
    assert!(
        stderr_text(&with_family).contains("distill --pass state takes no --family"),
        "{}",
        stderr_text(&with_family)
    );
    assert!(!dir.path().join(DATA).exists(), "用法错误必须先看库外收口");

    let with_retry = run(&["distill", "once", "--pass", "state", "--retry-empty", "2",
                           "--data-dir", &dd]);
    assert_eq!(code(&with_retry), 3);
    assert!(stderr_text(&with_retry).contains("--retry-empty"), "{}", stderr_text(&with_retry));

    let unknown = run(&["distill", "once", "--pass", "mood", "--data-dir", &dd]);
    assert_eq!(code(&unknown), 3);
    let text = stderr_text(&unknown);
    assert!(text.contains("--pass state"), "未知 pass 的点名词表里必须有 state：{text}");
}

/// 7. 空库/没有可选轮：`state="none"`、退出 0、一次调用都不发。
#[test]
fn an_empty_queue_is_a_legal_zero() {
    let dir = tempfile::tempdir().unwrap();
    // 只有库、没有转录：选批交回"没有可选的批"，读数打 `state="none"`。
    Root::open(&dir.path().join(DATA)).unwrap();
    let out = distill(dir.path(), &[], &[reply(&state_reply("焦虑得很", CANARY))]);
    let json = stdout_json(&out);
    assert_eq!(json["state"], "none", "{}", stdout_text(&out));
    assert_eq!(code(&out), 0);
    let root = open(dir.path());
    assert_eq!(llm_calls(&root, "state"), 0);
}

/// 8. `--dry-run` 导出模型看到的原文：首行、TARGET、候选与 CURRENT STATES 都在，且不开模型口。
#[test]
fn dry_run_exports_the_state_prompt() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let dd = data_dir(dir.path());
    let out = run(&[
        "distill", "once", "--pass", "state", "--dry-run", "--data-dir", &dd,
    ]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let text = stdout_text(&out);
    assert!(text.contains("PASS state v0"), "{text}");
    assert!(text.contains("CURRENT STATES"), "{text}");
    assert!(text.contains("CANDIDATES"), "{text}");
    let root = open(dir.path());
    assert_eq!(llm_calls(&root, "state"), 0, "dry-run 不许记账");
    assert_eq!(count(&root, "state_assertion", None), 0);
}

/// 9. `review list --pass state` / `review run --pass state` 必须是**认识**的值。
///
/// 这条用例是真实读数跑出来的：`review.rs` 的运行时闭集当时已含 `state`，
/// 但 `main.rs` 上三处 clap `value_parser` 没有 ⇒ CLI 在开库前就把 `--pass state` 判成
/// 用法错误（exit 3 + 整段 usage）。运行时过滤改了两处、声明面漏了三处，只有子进程看得见。
#[test]
fn state_is_a_recognized_pass_for_the_review_commands() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let dd = data_dir(dir.path());
    // 队列表里先挂一条 state 的待审项（`--no-review` 只入队，不问模型）。
    let queued = distill(
        dir.path(),
        &["--no-review"],
        &[reply(&state_reply("焦虑得很", CANARY))],
    );
    assert_eq!(code(&queued), 0, "{}", stderr_text(&queued));

    let listed = run(&["review", "list", "--pass", "state", "--data-dir", &dd, "--json"]);
    assert_eq!(code(&listed), 0, "review list 认识 state：{}", stderr_text(&listed));
    assert!(!stderr_text(&listed).contains("invalid invocation"), "{}", stderr_text(&listed));

    // `--pass state` 走到队列，而不是被用法面挡下：空库上它该回一份零读数。
    let empty = tempfile::tempdir().unwrap();
    let empty_dd = data_dir(empty.path());
    let ran = run(&[
        "review", "run", "--pass", "state", "--limit", "1", "--data-dir", &empty_dd, "--json",
    ]);
    assert_eq!(code(&ran), 0, "review run 认识 state：{}", stderr_text(&ran));
    let json = stdout_json(&ran);
    assert_eq!(json["processed"], 0);
}
