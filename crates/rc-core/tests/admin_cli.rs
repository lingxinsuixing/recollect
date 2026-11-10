//! `recollect-core` 操作员 CLI 的黑盒判据：真进程 + 真数据目录。
//! 只断言注册表与 key 文件的存在性/可见性；**任何输出都不许出现 key 内容**。

use std::path::Path;
use std::process::{Command, Output};

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_recollect-core"))
        .arg("--data-dir")
        .arg(dir)
        .args(args)
        .output()
        .expect("recollect-core binary")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn register_creates_the_key_and_lists_version_zero() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["register", "agent-a"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("registered agent-a"), "{}", text(&out));

    let key = rc_core::key_path(dir.path(), "agent-a");
    assert!(key.is_file(), "key file must exist: {}", key.display());
    let key_hex = hex(&std::fs::read(&key).unwrap());
    assert_eq!(key_hex.len(), 64);
    assert!(!text(&out).contains(&key_hex), "CLI must never print key bytes");

    let list = run(dir.path(), &["list"]);
    assert_eq!(list.status.code(), Some(0), "{}", text(&list));
    let line = String::from_utf8_lossy(&list.stdout);
    let expected_prefix = format!("agent-a\tk-{}\t0\t", rc_core::agent_table_suffix("agent-a"));
    assert!(line.starts_with(&expected_prefix), "list was: {line}");
    assert!(!line.contains(&key_hex), "list must not print key bytes");
}

#[test]
fn revoke_keeps_the_key_unless_delete_key_is_given() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(run(dir.path(), &["register", "agent-a"]).status.code(), Some(0));

    let out = run(dir.path(), &["revoke", "agent-a"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("revoked agent-a"), "{}", text(&out));
    assert!(rc_core::key_path(dir.path(), "agent-a").is_file(), "default revoke keeps the key");
    assert!(String::from_utf8_lossy(&run(dir.path(), &["list"]).stdout).trim().is_empty());

    assert_eq!(run(dir.path(), &["register", "agent-b"]).status.code(), Some(0));
    let out = run(dir.path(), &["revoke", "agent-b", "--delete-key"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(!rc_core::key_path(dir.path(), "agent-b").exists(), "--delete-key removes the key");
}

#[test]
fn revoke_of_an_unregistered_agent_is_exit_three() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["revoke", "agent-nobody"]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(!out.stderr.is_empty(), "must explain the failure");
}

#[test]
fn an_unsafe_agent_id_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["register", "bad/id"]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(!dir.path().join("keys").join("core_bad").exists());
}
