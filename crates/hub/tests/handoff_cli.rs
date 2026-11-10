//! H3：`hub handoff prune` 的真进程面。
//!
//! 只断三件事：没过期的不动；过了 `--now-ms` 的删掉并打印行数；没有可删的回执 0 而不是报错。
//! 交接条住在 `core.db` 的加密载荷里，所以造数走 `CoreStore` 本身（与 MCP 面同一个入口）。

use rc_core::CoreStore;
use std::path::Path;
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(dir: &Path, args: &[&str]) -> Output {
    let data = dir.join("data");
    let mut full: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    full.push("--data-dir".into());
    full.push(data.to_str().unwrap().to_string());
    Command::new(bin()).args(&full).output().expect("hub binary")
}

fn stdout_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// §2.8：未来的 `--now-ms` 会把还没到点的条当过期删掉 ⇒ 除非显式 `--force`，一律 exit 3 且一条不删。
#[test]
fn a_future_now_ms_is_refused_without_force() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let store = CoreStore::open(&data).unwrap();
    store.register_agent("agent-a", 1_000).unwrap();
    let alive = store
        .handoff_write("agent-a", "note", "还没到点", Some(9_000), "dev", 1_000)
        .unwrap();
    let system_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let future = (system_ms + 86_400_000).to_string();

    // 没有 --force：拒绝，退出码 3，一条都不删。
    let out = run(dir.path(), &["handoff", "prune", "--now-ms", &future]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr_text(&out));
    assert!(
        stderr_text(&out).contains("now-ms 在未来"),
        "拒绝的理由没说出来：{}",
        stderr_text(&out)
    );
    let left = store.handoff_list("agent-a", 1_000).unwrap();
    assert_eq!(left.len(), 1, "被拒的那次一条都不许删");
    assert_eq!(left[0].id, alive.id);

    // 显式 --force：按操作员的决定删。
    let out = run(dir.path(), &["handoff", "prune", "--now-ms", &future, "--force"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_text(&out));
    assert!(stdout_text(&out).contains("1 rows"), "{}", stdout_text(&out));
    assert!(
        store.handoff_list("agent-a", 1_000).unwrap().is_empty(),
        "--force 之后才删"
    );
}

#[test]
fn prune_deletes_only_expired_notes_and_reports_the_count() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let store = CoreStore::open(&data).unwrap();
    store.register_agent("agent-a", 1_000).unwrap();
    let now = 1_000;
    let expired = store
        .handoff_write("agent-a", "note", "已经过期", Some(now + 100), "dev", now)
        .unwrap();
    let alive = store
        .handoff_write("agent-a", "note", "还没过期", Some(now + 9_000), "dev", now)
        .unwrap();

    // 没过期：回执 0，一条都不删。
    let out = run(dir.path(), &["handoff", "prune", "--now-ms", "500"]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout_text(&out));
    assert!(stdout_text(&out).contains("0 rows"), "{}", stdout_text(&out));
    assert_eq!(store.handoff_list("agent-a", now).unwrap().len(), 2);

    // 过期一条：回执 1，留下没过期的那条。
    let out = run(dir.path(), &["handoff", "prune", "--now-ms", "2000"]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout_text(&out));
    assert!(stdout_text(&out).contains("1 rows"), "{}", stdout_text(&out));
    let left = store.handoff_list("agent-a", 2_000).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, alive.id, "没过期的那条必须留下");
    assert!(left.iter().all(|h| h.id != expired.id));
}
