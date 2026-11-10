//! `hub derive status|retry` 的黑盒面（工单 `2026-10-28-derive-quarantine`）。
//!
//! 三条口径由真进程钉住：① 空库的 status 是五个 null/0，不是"猜一个 0 游标"；
//! ② 隔离之后 status 读得到 cursor/attempts/open_issues；`retry --issue` 把游标回拨到
//! issue.detail 的 `prev` 并把那一行解决；③ 不存在的 issue、坏 detail、别的 pass 的 issue
//! 都是运维错误（退出 3），不猜。另有一条把 `cycle once --json` 的两个新格子钉在真进程上。
use rc_distill::derive::run_derive;
use rc_ledger::water;
use rc_llm::mock::MockBackend;
use rc_memory::keys::Verdict;
use rc_memory::{issues, set_verdict, AnchorRole, FactContent, Provenance, Scope, ScopeKind, put_card};
use rc_store::Root;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const NOW: i64 = 1_700_000_000_000;
const BAD: &str = r#"{"relationships":[]}"#;

fn bin() -> &'static str { env!("CARGO_BIN_EXE_recollect-hub") }

fn data_dir(dir: &Path) -> PathBuf { dir.join("data") }

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .arg("--data-dir")
        .arg(data_dir(dir))
        .output()
        .expect("hub binary")
}

fn code(out: &Output) -> i32 { out.status.code().unwrap() }

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象")
}

fn stderr_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// 两张活卡（带人工轮出处）。`verified` 为真时把 verdict 推到 `passed`——`cycle once`
/// 的审核段只挑 `unreviewed` 行，不这样做它会把回放脚本的口子抢走。
fn seed_cards(data: &Path, verified: bool) -> Vec<rc_memory::MemKey> {
    let root = Root::open(data).unwrap();
    let keys: Vec<_> = ["甲决策", "乙决策"].iter().enumerate().map(|(i, title)| {
        put_card(root.memory(), &FactContent {
            kind: "decision".into(),
            title: (*title).into(),
            body: format!("{title} 的正文"),
            scope: Scope { kind: ScopeKind::Project, key: "proj-derive-cli".into() },
            valid_to_ms: None,
        }, &[Provenance { turn_uid: format!("{}", (b'a' + i as u8) as char).repeat(64),
                          anchor_role: AnchorRole::Human }], NOW).unwrap()
    }).collect();
    if verified {
        for key in &keys {
            set_verdict(root.memory(), key, Verdict::Passed, NOW).unwrap();
        }
    }
    keys
}

/// 三次契约丢批之后的那一条 issue id（隔离轮由库面跑出来，CLI 只读它）。
fn quarantine(data: &Path) -> i64 {
    let root = Root::open(data).unwrap();
    seed_cards(data, false);
    for _ in 0..3 {
        let backend = MockBackend::scripted(vec![Ok(BAD.to_string())]);
        let _ = run_derive(root.memory(), root.ledger(), &backend, NOW);
    }
    root.memory().conn().query_row(
        "SELECT max(id) FROM extraction_issue", [], |r| r.get::<_, Option<i64>>(0))
        .unwrap().expect("第三轮必须隔离")
}

#[test]
fn status_on_an_empty_store_reports_nulls_and_zero() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["derive", "status", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v, json!({
        "cursor": null,
        "attempts": 0,
        "last_success_at_ms": null,
        "open_issues": 0,
        "oldest_issue_age_ms": null,
    }), "空库不许伪造游标：{v}");
}

#[test]
fn status_reports_the_cursor_attempts_and_open_issue() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    let issue_id = quarantine(&data);
    let out = run(dir.path(), &["derive", "status", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v["cursor"]["at_ms"], NOW, "隔离轮把游标推到本批末尾：{v}");
    assert!(v["cursor"]["turn_uid"].as_str().unwrap().contains(':'), "游标是 key_string：{v}");
    assert_eq!(v["attempts"], 0, "结算写口把 attempts 归零：{v}");
    assert!(v["last_success_at_ms"].is_number(), "隔离轮也是一次成功推进：{v}");
    assert_eq!(v["open_issues"], 1, "{v}");
    assert!(v["oldest_issue_age_ms"].as_i64().unwrap() >= 0, "{v}");
    // 非 JSON 形也要能读：同一份读数，一行文本。
    let text = Command::new(bin()).args(["derive", "status"])
        .arg("--data-dir").arg(&data).output().unwrap();
    assert_eq!(code(&text), 0);
    let line = String::from_utf8(text.stdout).unwrap();
    assert!(line.contains("open_issues=1") && line.contains("attempts=0"), "{line}");
    assert_eq!(issues::get(Root::open(&data).unwrap().memory(), issue_id)
                   .unwrap().unwrap().resolved_at_ms, None);
}

#[test]
fn retry_rewinds_to_prev_and_resolves_the_issue() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    let issue_id = quarantine(&data);
    let out = run(dir.path(), &["derive", "retry", "--issue", &issue_id.to_string(), "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v, json!({
        "retried": true,
        "issue": issue_id,
        "from": {"at_ms": 0, "turn_uid": ""},
    }), "回拨点是 issue.detail 的 prev：{v}");

    let root = Root::open(&data).unwrap();
    let cur = water::read(root.ledger(), "derive", "").unwrap().unwrap();
    assert_eq!((cur.at_ms, cur.turn_uid.as_str()), (0, ""), "游标回到出生点");
    assert_eq!(water::attempts(root.ledger(), "derive", "").unwrap(), 0);
    let issue = issues::get(root.memory(), issue_id).unwrap().unwrap();
    assert!(issue.resolved_at_ms.is_some(), "retry 把那一行解决掉");
    drop(root);

    let after = run(dir.path(), &["derive", "status", "--json"]);
    let v = stdout_json(&after);
    assert_eq!(v["open_issues"], 0, "解决之后不再计入未解决：{v}");
    assert_eq!(v["oldest_issue_age_ms"], Value::Null, "{v}");
    assert_eq!(v["cursor"]["at_ms"], 0, "{v}");
}

#[test]
fn retry_refuses_unknown_malformed_and_foreign_issues() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    // 未知 id：不猜一个游标。
    let out = run(dir.path(), &["derive", "retry", "--issue", "9999", "--json"]);
    assert_eq!(code(&out), 3, "{}", stderr_text(&out));
    assert!(stderr_text(&out).contains("no such issue"), "{}", stderr_text(&out));

    // detail 不是 JSON：不猜 prev。
    let root = Root::open(&data).unwrap();
    let bad_json = issues::record(root.memory(), "derive", "schema", "derive-1", "not json", NOW)
        .unwrap();
    // 别的 pass 的行：拿它的 detail 回拨推导游标是另一件事，拒绝。
    let foreign = issues::record(root.memory(), "entity", "schema", "derive-2",
                                 r#"{"prev_at_ms":0,"prev_uid":""}"#, NOW).unwrap();
    drop(root);

    for id in [bad_json, foreign] {
        let out = run(dir.path(), &["derive", "retry", "--issue", &id.to_string(), "--json"]);
        assert_eq!(code(&out), 3, "issue {id}：{}", stderr_text(&out));
        assert!(stderr_text(&out).contains("refusing"), "issue {id}：{}", stderr_text(&out));
    }
}

#[test]
fn cycle_once_reports_the_quarantine_issue_only_on_the_quarantining_round() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    // 两轮失败先记在账上：cycle 这一轮的 note_attempt 正好到上限 3。
    {
        let root = Root::open(&data).unwrap();
        seed_cards(&data, true);
        water::note_attempt(root.ledger(), "derive", "").unwrap();
        water::note_attempt(root.ledger(), "derive", "").unwrap();
    }
    let replies = dir.path().join("replies.json");
    std::fs::write(&replies, Value::Array(vec![Value::String(BAD.to_owned())]).to_string()).unwrap();
    let out = Command::new(bin())
        .args(["cycle", "once", "--replies", replies.to_str().unwrap()])
        .arg("--data-dir").arg(&data)
        .output().unwrap();
    assert_eq!(code(&out), 2, "隔离轮仍是整批拒收的退出码：{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v["derive_quarantined"], 1, "cycle 读数缺隔离那一格：{v}");
    assert!(v["derive_issue_id"].is_i64(), "cycle 读数缺 issue id：{v}");
    // 十七格 + 丢批类别（`derive_drop_reason`，隔离轮仍是 Drop）+ 隔离两格。
    assert_eq!(v.as_object().unwrap().len(), 20, "十七格 + 类别 + 隔离两格：{v}");
    assert_eq!(v["derive_dropped"], 1, "{v}");

    // 再跑一轮：批已越过、游标在末尾 ⇒ 没有隔离，两格不出现（null 时照旧）。
    let reply_again = dir.path().join("replies-again.json");
    std::fs::write(&reply_again,
                   Value::Array(vec![Value::String(BAD.to_owned())]).to_string()).unwrap();
    let again = Command::new(bin())
        .args(["cycle", "once", "--replies", reply_again.to_str().unwrap()])
        .arg("--data-dir").arg(&data)
        .output().unwrap();
    assert_eq!(code(&again), 0, "空批不是故障：{}", stderr_text(&again));
    let v2 = stdout_json(&again);
    assert!(v2.get("derive_quarantined").is_none(), "没有隔离就不加那一格：{v2}");
    assert!(v2.get("derive_issue_id").is_none(), "{v2}");
    assert_eq!(v2.as_object().unwrap().len(), 17, "空批轮的 JSON 回到十七格：{v2}");

    let root = Root::open(&data).unwrap();
    let cur = water::read(root.ledger(), "derive", "").unwrap().unwrap();
    assert_ne!((cur.at_ms, cur.turn_uid.as_str()), (0, ""), "隔离轮推过游标");
}
