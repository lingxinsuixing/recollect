// crates/hub/tests/distill_reset_cli.rs
//! W7b：`hub distill reset` 的真进程面——按轮把某一 pass 的终态行**定向回拨**成 `stale`。
//!
//! 为什么是 CLI 面：这条命令的全部意义就是"让下一轮选批重新捡起这几轮"，而选批的开口
//! （`should_run`）只有真库上的真行看得见。四条用例各钉一头：
//!
//! - 终态（`done` 与 `quarantined`）回拨成 `stale`、指纹与 `run_id` 留着，`should_run` 因此
//!   点头；别的 pass 的行、清单外的轮一个都不碰；
//! - **阴性对照**：`pending`/`running` 原样不动（状态与退避时刻逐字不变），未知轮只进
//!   `not_found`，两种都是**退出 0**——"这批轮里有一格不是终态"是读数，不是失败；
//! - 坏清单（长度不对 / 不是十六进制）是用法错误，退出 3 且**不建库**；
//! - 审计：每次回拨落一条 `distill_reset`，记 pass、三项计数、理由与清单文件的 sha256
//!   （清单正文一个字节都不进库）。
use rc_ledger::pass_state::{self, PassKey, PassRecord, PassState};
use rc_store::Root;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DATA: &str = "data";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

/// 跑一条子命令，`--data-dir` 统一补在末尾（用法错误的用例也要它，才能断言"没建库"）。
fn run(dir: &Path, args: &[&str]) -> Output {
    let data = dir.join(DATA);
    let mut full: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    full.push("--data-dir".into());
    full.push(data.to_str().unwrap().to_string());
    Command::new(bin()).args(&full).output().expect("hub binary")
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap()
}

fn stdout_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON")
}

fn stderr_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// 合成轮号：64 位小写十六进制（与真实 `turn_uid` 同形）。
fn turn_uid(i: u32) -> String {
    format!("{:064x}", i)
}

fn key<'a>(turn_uid: &'a str, pass_id: &'a str, family: &'a str) -> PassKey<'a> {
    PassKey { turn_uid, pass_id, family }
}

fn record(root: &Root, k: PassKey<'_>) -> PassRecord {
    pass_state::get(root.ledger(), k).unwrap().expect("状态行不存在")
}

fn data_dir(dir: &Path) -> PathBuf {
    dir.join(DATA)
}

/// 写一份清单文件，返回它的路径。正文按参数逐字落盘（注释行、空行由用例自己带）。
fn write_turns(dir: &Path, body: &str) -> PathBuf {
    let p = dir.join("turns.txt");
    std::fs::write(&p, body).unwrap();
    p
}

fn digest(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// `audit_log` 里唯一那条 `distill_reset`：`(subject, detail)`。多于一条就是两次回拨
/// 落了同一份账，直接红。
fn audit_row(root: &Root) -> (Option<String>, String) {
    let mut st = root
        .ledger()
        .conn()
        .prepare("SELECT subject, detail FROM audit_log WHERE action='distill_reset' ORDER BY id")
        .unwrap();
    let rows: Vec<(Option<String>, String)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows.len(), 1, "一次回拨恰一条审计行：{rows:?}");
    rows.into_iter().next().unwrap()
}

// ── 1. 终态 → stale，且下一轮真的会重新选中它 ────────────────────────────────

#[test]
fn reset_turns_terminal_rows_stale_so_the_next_batch_picks_them_up() {
    let dir = tempfile::tempdir().unwrap();
    let first = turn_uid(1);
    let second = turn_uid(2);
    let outside = turn_uid(9);
    {
        let root = Root::open(&data_dir(dir.path())).unwrap();
        let l = root.ledger();
        // 清单内第一轮：state 的 done 行带指纹（回拨后必须还能看到这条指纹）。
        pass_state::mark_running(l, key(&first, "state", ""), "fp-first", "run-first", 1_000)
            .unwrap();
        pass_state::mark_terminal(l, key(&first, "state", ""), PassState::Done, Some("ok"), 2_000)
            .unwrap();
        // 同一轮的**另一条 pass**：这一刀与它无关。
        pass_state::mark_terminal(
            l, key(&first, "fact", "decision"), PassState::Done, Some("ok"), 2_000,
        )
        .unwrap();
        // 清单内第二轮：隔离也是终态，一并回拨。
        pass_state::mark_terminal(
            l, key(&second, "state", ""), PassState::Quarantined, Some("schema"), 2_000,
        )
        .unwrap();
        // 清单外：一个字节都不许动。
        pass_state::mark_terminal(l, key(&outside, "state", ""), PassState::Done, Some("ok"), 2_000)
            .unwrap();
    }

    let body = format!("# 要重蒸的轮\n\n{first}\n{second}\n");
    let turns = write_turns(dir.path(), &body);
    let out = run(
        dir.path(),
        &[
            "distill", "reset", "--pass", "state", "--turns", turns.to_str().unwrap(),
            "--reason", "operator_reset", "--json",
        ],
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    assert_eq!(
        stdout_json(&out),
        json!({
            "job": "distill_reset",
            "pass": "state",
            "turns": 2,
            "reset": 2,
            "skipped_non_terminal": 0,
            "not_found": 0,
        }),
        "读数形状：注释行与空行不算轮"
    );

    let root = Root::open(&data_dir(dir.path())).unwrap();
    let r = record(&root, key(&first, "state", ""));
    assert_eq!(r.state, PassState::Stale);
    assert_eq!(r.reason.as_deref(), Some("operator_reset"));
    assert_eq!(r.next_retry_at_ms, None, "stale 行没有下一次重试");
    assert_eq!(r.fingerprint.as_deref(), Some("fp-first"), "指纹不动，选批才走 stale 那一支");
    assert_eq!(r.run_id.as_deref(), Some("run-first"), "回拨不许抹掉 run_id");
    assert!(
        pass_state::should_run(root.ledger(), key(&first, "state", ""), "fp-first", 9_000).unwrap(),
        "回拨后这一格必须重新被选中（同指纹 + stale）"
    );
    assert_eq!(
        record(&root, key(&second, "state", "")).state,
        PassState::Stale,
        "quarantined 也是终态"
    );
    assert_eq!(
        record(&root, key(&first, "fact", "decision")).state,
        PassState::Done,
        "别的 pass 的行不许碰"
    );
    assert_eq!(
        record(&root, key(&outside, "state", "")).state,
        PassState::Done,
        "清单外的轮不许碰"
    );

    let (subject, detail) = audit_row(&root);
    assert_eq!(subject.as_deref(), Some("state"), "审计的 subject 是这条 pass");
    assert!(detail.contains("reset=2"), "审计要记回拨条数：{detail}");
    assert!(detail.contains("reason=operator_reset"), "审计要记理由：{detail}");
    assert!(
        detail.contains(&format!("turns_sha256={}", digest(body.as_bytes()))),
        "审计要记清单文件的指纹：{detail}"
    );
    assert!(!detail.contains(turns.to_str().unwrap()), "审计不记路径：{detail}");
}

// ── 2. 阴性对照：非终态逐字不动，未知轮只是计数 ──────────────────────────────

#[test]
fn non_terminal_rows_stay_byte_for_byte_and_an_unknown_turn_is_only_a_count() {
    let dir = tempfile::tempdir().unwrap();
    let pending = turn_uid(3);
    let running = turn_uid(4);
    let absent = turn_uid(5);
    let (pending_before, running_before) = {
        let root = Root::open(&data_dir(dir.path())).unwrap();
        let l = root.ledger();
        pass_state::mark_running(l, key(&pending, "state", ""), "fp-pending", "run-pending", 1_000)
            .unwrap();
        pass_state::note_attempt(l, key(&pending, "state", ""), 60_000, 2_000).unwrap();
        pass_state::mark_running(l, key(&running, "state", ""), "fp-running", "run-running", 3_000)
            .unwrap();
        (
            record(&root, key(&pending, "state", "")),
            record(&root, key(&running, "state", "")),
        )
    };
    assert_eq!(pending_before.next_retry_at_ms, Some(62_000), "夹具：退避中");

    let turns = write_turns(dir.path(), &format!("{pending}\n{running}\n{absent}\n"));
    let out = run(
        dir.path(),
        &[
            "distill", "reset", "--pass", "state", "--turns", turns.to_str().unwrap(),
            "--reason", "operator_reset", "--json",
        ],
    );
    assert_eq!(
        code(&out),
        0,
        "非终态与未知轮都是读数，不是失败：{}",
        stderr_text(&out)
    );
    assert_eq!(
        stdout_json(&out),
        json!({
            "job": "distill_reset",
            "pass": "state",
            "turns": 3,
            "reset": 0,
            "skipped_non_terminal": 2,
            "not_found": 1,
        })
    );

    let root = Root::open(&data_dir(dir.path())).unwrap();
    assert_eq!(
        record(&root, key(&pending, "state", "")),
        pending_before,
        "pending 行的状态与退避时刻必须逐字不变"
    );
    assert_eq!(
        record(&root, key(&running, "state", "")),
        running_before,
        "running 行可能是活的，一个字节都不许写"
    );
    // 零回拨也落审计：操作员递进来的这份清单必须留账（否则"跑过一次但什么都没发生"
    // 与"根本没跑"在读侧分不开）。
    let (subject, detail) = audit_row(&root);
    assert_eq!(subject.as_deref(), Some("state"));
    assert!(detail.contains("reset=0"), "{detail}");
}

// ── 3. 坏清单 ⇒ 退出 3，且不建库 ─────────────────────────────────────────────

#[test]
fn a_malformed_turns_file_is_a_usage_error_and_opens_no_store() {
    // 三条都不合格：长度差一位、64 位但不是十六进制、混进一行别的东西。
    let cases = [
        format!("{}\n", &turn_uid(7)[..63]),
        format!("{}\n", "g".repeat(64)),
        format!("{}\nnot-a-turn\n", turn_uid(8)),
    ];
    for body in cases {
        let dir = tempfile::tempdir().unwrap();
        let turns = write_turns(dir.path(), &body);
        let out = run(
            dir.path(),
            &[
                "distill", "reset", "--pass", "state", "--turns", turns.to_str().unwrap(),
                "--reason", "operator_reset",
            ],
        );
        assert_eq!(code(&out), 3, "清单 {body:?} 必须退出 3：{}", stderr_text(&out));
        assert!(out.stdout.is_empty(), "用法错误不打读数：{}", stdout_text(&out));
        let err = stderr_text(&out);
        assert!(err.contains("--turns"), "stderr 必须点名旗标：{err}");
        assert!(err.starts_with("error: "), "运维下行的前缀是 error: {err}");
        assert!(!data_dir(dir.path()).exists(), "用法错误不建库");
    }
}

// ── 4. 未知 pass / 理由不合形状 ⇒ 退出 3 ─────────────────────────────────────

#[test]
fn an_unknown_pass_or_a_reason_outside_the_slug_shape_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let turns = write_turns(dir.path(), &format!("{}\n", turn_uid(6)));
    let turns_arg = turns.to_str().unwrap().to_string();
    for (pass, reason) in [("rows", "operator_reset"), ("state", "Operator Reset")] {
        let out = run(
            dir.path(),
            &[
                "distill", "reset", "--pass", pass, "--turns", &turns_arg, "--reason", reason,
            ],
        );
        assert_eq!(code(&out), 3, "{pass}/{reason}：{}", stderr_text(&out));
        assert!(out.stdout.is_empty(), "{pass}/{reason} 不打读数");
        let err = stderr_text(&out);
        assert!(
            err.contains("--pass") || err.contains("--reason"),
            "stderr 必须点名旗标：{err}"
        );
        assert!(!data_dir(dir.path()).exists(), "用法错误不建库");
    }
}
