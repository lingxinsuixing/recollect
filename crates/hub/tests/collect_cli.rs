//! Task 5：`recollect-hub collect` 的黑盒面。
//!
//! 这一面是采集器的本机入口（设计 §6/§7）：`--json` 一对象一源一行读数，
//! `--status` 只读水位，`--dry-run` 只扫描不提交。三条用例钉的是"零动作"与
//! "零数据"——采集无新数据时不许有任何写副作用。

use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

const QUIET_LONG_MS: u64 = 600_000;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin()).args(args).output().expect("hub binary")
}

/// 写一份单源 `collect.toml`。路径按正斜杠写入：TOML 基本串里 `\U` 是转义，
/// Windows 路径不做这次归一化会把配置本身写坏。
fn write_cfg(path: &Path, src_root: &Path, quiet_ms: u64) {
    let root = src_root.to_string_lossy().replace('\\', "/");
    let body = format!(
        r#"
[limits]
quiet_ms = {quiet_ms}
[[source]]
name = "qoder"
kind = "files"
root = "{root}"
glob = "*.jsonl"
converter = ["true"]
device_id = "dev-1"
source_agent = "qoder"
"#
    );
    std::fs::write(path, body).unwrap();
}

fn stdout_json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).expect("stdout must be one JSON object")
}

fn stderr_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// 转录层里的会话数：dry-run 前后必须相等。
fn sessions_count(data_dir: &Path) -> i64 {
    let root = rc_store::Root::open(data_dir).unwrap();
    root.transcript()
        .conn()
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn collect_cli_idle_reports_zero_without_side_effects() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let cfg = d.path().join("collect.toml");
    write_cfg(&cfg, &src, QUIET_LONG_MS);
    let data = d.path().join("data");

    let out = run(&[
        "collect",
        "--config",
        cfg.to_str().unwrap(),
        "--data-dir",
        data.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["sources"][0]["source"], "qoder");
    assert_eq!(v["sources"][0]["due"], 0);
    assert_eq!(v["sources"][0]["reason"], "idle");
    assert_eq!(v["sources"][0]["advanced"], false);
    assert!(v["state_path"]
        .as_str()
        .unwrap()
        .ends_with("collect-state.json"));
}

#[test]
fn collect_status_on_empty_state_is_zeroed_json() {
    let d = tempfile::tempdir().unwrap();
    let data = d.path().join("data");

    let out = run(&[
        "collect",
        "--status",
        "--json",
        "--data-dir",
        data.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v["files"], 0);
    assert_eq!(v["sqlite"], 0);
    assert_eq!(v["poisoned"], 0);
    assert!(v["state_path"]
        .as_str()
        .unwrap()
        .ends_with("collect-state.json"));
}

#[test]
fn collect_dry_run_counts_due_without_writing_data() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("a.jsonl"),
        "{\"format_version\":1,\"sessions\":[]}\n",
    )
    .unwrap();
    // quiet_ms = 1：先让文件落地，再等两毫秒，进程里的"现在"就必然跨过静默窗。
    std::thread::sleep(Duration::from_millis(20));
    let cfg = d.path().join("collect.toml");
    write_cfg(&cfg, &src, 1);
    let data = d.path().join("data");
    let before = sessions_count(&data);

    let out = run(&[
        "collect",
        "--config",
        cfg.to_str().unwrap(),
        "--data-dir",
        data.to_str().unwrap(),
        "--dry-run",
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v["sources"][0]["due"], 1);
    assert_eq!(v["sources"][0]["reason"], "dry_run");
    assert_eq!(v["sources"][0]["new_turns"], 0);
    assert_eq!(sessions_count(&data), before);
}
