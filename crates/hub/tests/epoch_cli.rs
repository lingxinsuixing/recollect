//! `hub epoch require|allow|status`：兼容窗口标记的 CLI 面。
//!
//! 这条测试只碰文件与 stdout；标记对请求的影响在 `promote_fence.rs` 里已经真跑过。

use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

fn run(data: &Path, job: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_recollect-hub"))
        .args([
            "epoch",
            job,
            "--data-dir",
            data.to_str().unwrap(),
        ])
        .output()
        .unwrap()
}

fn required(out: &Output) -> bool {
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let doc: Value = serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象");
    doc["epoch_required"].as_bool().expect("epoch_required 必须是布尔")
}

#[test]
fn epoch_cli_toggles_the_marker_idempotently() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();

    assert!(!required(&run(&data, "status")));
    assert!(required(&run(&data, "require")));
    assert!(required(&run(&data, "require")), "重复 require 必须幂等");
    assert!(required(&run(&data, "status")));
    assert!(!required(&run(&data, "allow")));
    assert!(!required(&run(&data, "allow")), "重复 allow 必须幂等");
    assert!(!required(&run(&data, "status")));
}
