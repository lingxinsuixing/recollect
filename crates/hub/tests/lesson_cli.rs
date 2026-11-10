// crates/hub/tests/lesson_cli.rs
//! 波三刀 A：`hub distill --pass lesson` 的分派面。
//!
//! 两条出口各断一次：
//! - `--dry-run`：真实选批 + 同一组 `lesson_system()`/`lesson_user(...)` 打到 stdout，
//!   且 `review_item`/`review_decision`/`card` 的行数跑前跑后一致（**dry-run 不写库**）；
//! - 真实跑（`--replies` 回放）：抽取 + 独立审核 + 落卡走完，读数 `cards=1`，库里多一张
//!   `kind='lesson'` 的卡，没有 `fact` 行。
//!
//! 另外两格形状：没有候选 ⇒ `=== NO BATCH ===`（退出 0）；`--family` ⇒ 用法错误（退出 3）。
//!
//! 纪律与其余 CLI 用例同源：不跑真实 LLM、夹具走 `parse`→`normalize`→`insert_batch` 真链路。

use rc_contract::{normalize, parse};
use rc_ledger::insert_batch;
use rc_store::Root;
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Output};

const DEV: &str = "dev-lesson";
const SESS: &str = "sess-lesson";
const DATA: &str = "data";
const NOW: i64 = 1_700_000_000_000;
/// 两轮正文：它们必须原样出现在 dry-run 的 USER 里。
const BODY_1: &str = "先把计数报出来，一共 37 条";
const BODY_2: &str = "那个数原文里没有，是凭印象写的";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin()).args(args).output().expect("hub binary")
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap()
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

fn count(root: &Root, table: &str) -> i64 {
    root.memory()
        .conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// 播种两轮真转录，并落一条 `state='rejected'` 的 review_item（lesson 的候选源）。
fn seed(dir: &Path, with_candidate: bool) -> Vec<String> {
    let doc = json!({
        "format_version": 1,
        "sessions": [{
            "session_key": SESS,
            "device_id": DEV,
            "source_agent": "agent-lesson",
            "turns": [
                {"role": "human", "body": BODY_1, "created_at_ms": NOW},
                {"role": "agent", "body": BODY_2, "created_at_ms": NOW + 1}
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
    if with_candidate {
        root.memory()
            .conn()
            .execute(
                "INSERT INTO review_item(source, pass_id, family, session_key, device_id,
                                         turn_uids_json, owner_json, payload_json, payload_sha256,
                                         state, program_code, created_at_ms, updated_at_ms)
                 VALUES('distill', 'fact', '', ?1, ?2, ?3, '{\"owner_agent_id\":\"alice\"}',
                        '{\"schema\":\"review.pass.v1\"}', ?4, 'rejected', 'number_not_grounded',
                        ?5, ?5)",
                rusqlite::params![
                    SESS,
                    DEV,
                    json!(uids).to_string(),
                    "a".repeat(64),
                    NOW
                ],
            )
            .unwrap();
    }
    uids
}

fn lesson_reply() -> String {
    json!({
        "schema_version": 0,
        "pass": "lesson",
        "lessons": [{
            "title": "引用数字前先回到原文",
            "description": "输出里要出现数量或日期时适用；防止把记忆里的估算写成语料原文",
            "content": "写任何数字前先在引文里找到同一个数字；找不到就写 unknown",
            "error_kind": "grounding"
        }]
    })
    .to_string()
}

fn review_reply() -> String {
    json!({
        "schema_version": 0,
        "pass": "lesson_review",
        "verdicts": [{"ref": "l1", "verdict": "pass", "code": null}]
    })
    .to_string()
}

#[test]
fn dry_run_prints_the_lesson_prompt_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let _uids = seed(dir.path(), true);
    let root = open(dir.path());
    let before = (
        count(&root, "review_item"),
        count(&root, "review_decision"),
        count(&root, "card"),
    );
    drop(root);

    let dd = data_dir(dir.path());
    let out = run(&["distill", "once", "--pass", "lesson", "--dry-run", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let text = stdout_text(&out);
    assert!(text.contains("=== PASS ===\nlesson\n"), "PASS 那一格：{text}");
    assert!(text.contains(rc_distill::pass_prompt::LESSON_CANARY), "system 的 canary 必须逐字到 stdout");
    assert!(text.contains("ERROR FAMILY: number_not_grounded"), "程序错因必须进 USER：{text}");
    assert!(text.contains(BODY_1) && text.contains(BODY_2), "两轮正文必须进 USER：{text}");
    assert!(text.contains("TARGET [t1]"), "位置编号必须进 USER：{text}");
    assert!(!text.contains("=== NO BATCH ==="), "有候选，不许报空批：{text}");

    let root = open(dir.path());
    let after = (
        count(&root, "review_item"),
        count(&root, "review_decision"),
        count(&root, "card"),
    );
    assert_eq!(after, before, "dry-run 不写库（item/判定/卡三表行数必须一致）");
}

#[test]
fn no_candidate_is_a_legal_empty_batch() {
    let dir = tempfile::tempdir().unwrap();
    let _uids = seed(dir.path(), false);
    let dd = data_dir(dir.path());
    let out = run(&["distill", "once", "--pass", "lesson", "--dry-run", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    assert_eq!(stdout_text(&out), "=== NO BATCH ===\n");
}

#[test]
fn lesson_takes_no_family() {
    let dir = tempfile::tempdir().unwrap();
    let _uids = seed(dir.path(), true);
    let dd = data_dir(dir.path());
    let out = run(&[
        "distill", "once", "--pass", "lesson", "--family", "decision", "--dry-run",
        "--data-dir", &dd,
    ]);
    assert_eq!(code(&out), 3);
    assert!(
        stderr_text(&out).contains("--pass lesson takes no --family"),
        "stderr: {}",
        stderr_text(&out)
    );
}

#[test]
fn a_replayed_run_writes_one_lesson_card_and_no_fact_row() {
    let dir = tempfile::tempdir().unwrap();
    let _uids = seed(dir.path(), true);
    let replies = dir.path().join("replies.json");
    std::fs::write(&replies, Value::Array(vec![
        Value::String(lesson_reply()),
        Value::String(review_reply()),
    ]).to_string())
    .unwrap();
    let dd = data_dir(dir.path());
    let out = run(&[
        "distill", "once", "--pass", "lesson",
        "--replies", replies.to_str().unwrap(),
        "--data-dir", &dd,
    ]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let readout: Value = serde_json::from_str(&stdout_text(&out)).expect("读数必须是 JSON");
    assert_eq!(readout["pass"], "lesson");
    assert_eq!(readout["cards"], 1);
    assert_eq!(readout["turns"], 2);
    assert_eq!(readout["state"], "done");

    let root = open(dir.path());
    let (kind, title): (String, String) = root
        .memory()
        .conn()
        .query_row("SELECT kind, title FROM card", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!(kind, "lesson");
    assert_eq!(title, "引用数字前先回到原文");
    assert_eq!(count(&root, "fact"), 0, "lesson 不写 fact 行");
    let state: String = root
        .memory()
        .conn()
        .query_row("SELECT state FROM review_item WHERE pass_id='lesson'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(state, "applied", "审核 pass 之后 lesson item 必须 applied");
}
