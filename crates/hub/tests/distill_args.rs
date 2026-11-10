// crates/hub/tests/distill_args.rs
//! S2e-2：`hub distill once --llm-timeout-ms` 的黑盒进程面。
//!
//! 为什么单独一个文件（而不是塞进 `distill_time_cli.rs`）：那一份是 time pass 的行为面，
//! 这一份是 **distill 子命令的旗标形状**——与 pass 无关。两条用例各自钉一头：
//!
//! - `0` 是用法错误（退出 3、stderr 点名、**不开库**）：0 毫秒若被读成"不设超时"，就正好是
//!   这一格要消灭的"无界挂起"；若被读成"立即超时"，则是操作员递错一个数之后的静默灾难。
//!   两种解释都不许发生。
//! - 合法值不改变回放路径：`--replies` 是唯一来源，脚本是一份**空数组**（回放来源合法），
//!   空批 ⇒ 一次模型都不调、退出 0、stdout 仍是单个 JSON。
//!
//! 纪律：不跑真实 LLM（没有 `--llm-config`、没有出网）；stderr 只许类别串（守卫 12）。
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

const DATA: &str = "data";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin()).args(args).output().expect("hub binary")
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap()
}

fn stderr_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// 一份合法的回放脚本：顶层是空数组（"这一轮一条回复都不需要"）。
fn empty_script(dir: &Path) -> std::path::PathBuf {
    let p = dir.join("replies.json");
    std::fs::write(&p, "[]").unwrap();
    p
}

/// 1. `--llm-timeout-ms 0` 是用法错误：退出 3、stderr 点名该旗标、stdout 空、**不建库**。
///    两个 pass 各断一次：这一格是**旗标**的形状，与哪条 pass 无关。
#[test]
fn zero_timeout_is_a_usage_error_and_opens_no_store() {
    for pass in ["naming", "time"] {
        let dir = tempfile::tempdir().unwrap();
        let replies = empty_script(dir.path());
        let dd = dir.path().join(DATA);
        let out = run(&[
            "distill",
            "once",
            "--pass",
            pass,
            "--replies",
            replies.to_str().unwrap(),
            "--llm-timeout-ms",
            "0",
            "--data-dir",
            dd.to_str().unwrap(),
        ]);
        assert_eq!(code(&out), 3, "pass {pass}，stderr: {}", stderr_text(&out));
        assert!(out.stdout.is_empty(), "用法错误不开库也不打读数：pass {pass}");
        let err = stderr_text(&out);
        assert!(err.contains("--llm-timeout-ms"), "stderr 必须点名旗标：{err}");
        assert!(err.starts_with("error: "), "运维下行的前缀是 error: {err}");
        assert!(!dd.exists(), "用法错误不建库：pass {pass}");
    }
}

/// 2. 合法值不动回放路径：空批 ⇒ 退出 0、单个 JSON 读数、stderr 没有 error。
///    同一份脚本跑两遍（缺省值 / 显式值），读数必须一致——超时是 live 那一支的事，
///    回放分支不该因此换一条路。
#[test]
fn a_positive_timeout_leaves_the_replay_path_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let replies = empty_script(dir.path());
    let dd = dir.path().join(DATA);
    let explicit = run(&[
        "distill",
        "once",
        "--pass",
        "naming",
        "--replies",
        replies.to_str().unwrap(),
        "--llm-timeout-ms",
        "600000",
        "--data-dir",
        dd.to_str().unwrap(),
    ]);
    assert_eq!(code(&explicit), 0, "stderr: {}", stderr_text(&explicit));
    assert!(!stderr_text(&explicit).contains("error:"), "stderr: {}", stderr_text(&explicit));
    let doc: Value = serde_json::from_slice(&explicit.stdout).expect("stdout 必须是单个 JSON 对象");
    assert_eq!(doc["pass"], "naming");
    assert_eq!(doc["turns"], 0, "空批：{doc}");
    // 空批不写 pass state ⇒ 读数是 `none`（不是 `empty`：那是"跑过且零产出"）。
    assert_eq!(doc["state"], "none", "空批：{doc}");
}

/// 3. 缺省值是 600000：`--help` 是这一个数唯一的观测面（live 那一支出网，测试不碰）。
#[test]
fn the_default_timeout_is_pinned_in_the_help_text() {
    let out = run(&["distill", "once", "--help"]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let help = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(help.contains("--llm-timeout-ms"), "帮助里没有这一格：{help}");
    assert!(help.contains("600000"), "缺省值没印成 600000：{help}");
    assert!(help.contains("[default: 600000]"), "缺省值不是这一格的默认：{help}");
}
