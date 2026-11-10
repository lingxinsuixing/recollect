//! F2（终审 Important #1）：`recollect-core --data-dir <DIR> <子命令>` 的参数顺序判据。
//!
//! 为什么这一对用例存在：mcp 的 `unregistered` hint 是一条**要被逐字抄进 shell 的命令**，
//! 而它的顺序在 Task 9 里是错的（`register <id> --data-dir <dir>` ⇒ clap 在解析期就拒，
//! 连数据目录都不建）。Task 9 的正证走的是库 API（`CoreStore::open(..).register_agent(..)`），
//! 不经 CLI ⇒ 那一格在测试面上是空的。这里补上**阳性对照**：
//! - 正证：`--data-dir` 在子命令**前** ⇒ rc=0，`core.db` 落地；
//! - 反证：`--data-dir` 在子命令**后** ⇒ rc≠0、stderr 含 `unexpected argument`、`core.db` 没建。
//!
//! 两条合起来才说明 hint 那个占位符模板的形状是唯一可行的形状（不是恰好也能跑）。
//! 机制见 `crates/rc-core/src/bin/recollect-core.rs:20-24`：`data_dir` 是父级参数、
//! 没有 `global = true`。本文件**只断 key 文件的存在性**，从不读取其内容。

use std::process::{Command, Output};

fn core_bin() -> &'static std::path::Path {
    std::path::Path::new(env!("CARGO_BIN_EXE_recollect-core"))
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// 真 CLI，**不带**任何隐藏参数：`args` 就是操作员会敲的那一串。
fn cli(args: &[&str]) -> Output {
    Command::new(core_bin())
        .args(args)
        .output()
        .expect("recollect-core binary")
}

/// 正证：`--data-dir` 写在子命令前面 ⇒ 注册成功，`core.db` 与 `keys/` 落在这个目录里。
#[test]
fn data_dir_before_the_subcommand_registers_into_that_directory() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().to_str().unwrap();
    let out = cli(&["--data-dir", data_dir, "register", "agent-order-ok"]);
    let text = combined(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("registered agent-order-ok"), "{text}");
    assert!(dir.path().join("core.db").is_file(), "core.db 未落地：{text}");
    // key 只断存在性，绝不读内容（纪律：不打印任何密钥值）。
    let key = rc_core::key_path(dir.path(), "agent-order-ok");
    assert!(key.is_file(), "key file missing: {}", key.display());
}

/// 反证：`--data-dir` 写在子命令后面 ⇒ clap 解析期报错退出，动作**根本没发生**。
/// 这一条是 mcp 侧 hint 顺序断言的另一半牙齿：把它跑红就证明"抄错顺序"不是多此一举，
/// 而是操作员以为注册成功了、实际什么都没建。
#[test]
fn data_dir_after_the_subcommand_is_rejected_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().to_str().unwrap();
    let out = cli(&["register", "agent-order-bad", "--data-dir", data_dir]);
    assert_ne!(
        out.status.code(),
        Some(0),
        "错误顺序居然成功了：{}",
        combined(&out)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unexpected argument"),
        "stderr 没有 clap 的参数报错：{stderr}"
    );
    assert!(!dir.path().join("core.db").exists(), "错误顺序却建了 core.db");
    assert_eq!(
        std::fs::read_dir(dir.path()).map(|entries| entries.count()).unwrap_or(0),
        0,
        "错误顺序在数据目录里留下了东西"
    );
}
