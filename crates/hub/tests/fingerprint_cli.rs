//! S1 的观测面：`hub fingerprint status`。
//!
//! 两形一空：`--json` 的数组（字段名与 `FingerprintCount` 一致）、默认的一行一组文本、
//! 以及空库那一句。夹具直接 seed `extract_state`——命令读的是那一列，不是 cycle 的产物
//! （cycle 那边由 `rc-cycle/tests/fingerprint.rs` 钉着）。

use rc_store::Root;
use serde_json::Value;
use std::path::Path;
use std::process::Command;

const DATA: &str = "data";

fn run(dir: &Path, extra: &[&str]) -> std::process::Output {
    let mut args: Vec<&str> = vec!["fingerprint", "status"];
    args.extend_from_slice(extra);
    let dd = dir.join(DATA);
    args.extend_from_slice(&["--data-dir", dd.to_str().unwrap()]);
    Command::new(env!("CARGO_BIN_EXE_recollect-hub"))
        .args(&args)
        .output()
        .expect("hub binary")
}

/// `(turn_uid, state, fingerprint, at_ms)`。`fingerprint=None` 就是 legacy 行。
fn seed(dir: &Path, rows: &[(&str, &str, Option<&str>, i64)]) {
    let root = Root::open(&dir.join(DATA)).unwrap();
    for (uid, state, fp, at) in rows {
        root.ledger()
            .conn()
            .execute(
                "INSERT INTO extract_state(turn_uid, state, reason, at_ms, extraction_fingerprint)
                 VALUES(?1, ?2, NULL, ?3, ?4)",
                rusqlite::params![uid, state, at, fp],
            )
            .unwrap();
    }
}

#[test]
fn status_json_lists_one_group_per_fingerprint_and_null_for_legacy() {
    let dir = tempfile::tempdir().unwrap();
    let fp = "a".repeat(64);
    seed(dir.path(), &[
        ("t-1", "extracted", Some(fp.as_str()), 100),
        ("t-2", "extracted", Some(fp.as_str()), 300),
        ("t-3", "rejected", None, 200),
    ]);
    let out = run(dir.path(), &["--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    let groups = doc.as_array().expect("--json 必须是一个数组");
    assert_eq!(groups.len(), 2, "一值一 NULL ⇒ 两组：{doc}");
    assert_eq!(groups[0]["fingerprint"], Value::String(fp.clone()));
    assert_eq!(groups[0]["turns"], 2);
    assert_eq!(groups[0]["first_at_ms"], 100);
    assert_eq!(groups[0]["last_at_ms"], 300);
    assert!(groups[1]["fingerprint"].is_null(), "legacy 组的 fingerprint 必须是 null");
    assert_eq!(groups[1]["turns"], 1);
    assert_eq!(groups[1]["first_at_ms"], 200);
    assert_eq!(groups[1]["last_at_ms"], 200);
}

#[test]
fn status_text_prints_one_line_per_group_and_legacy_for_the_null_group() {
    let dir = tempfile::tempdir().unwrap();
    let fp = "a".repeat(64);
    seed(dir.path(), &[
        ("t-1", "extracted", Some(fp.as_str()), 100),
        ("t-2", "extracted", Some(fp.as_str()), 300),
        ("t-3", "rejected", None, 200),
    ]);
    let out = run(dir.path(), &[]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("{fp}  turns=2  first=100  last=300\nlegacy  turns=1  first=200  last=200\n"),
    );
}

#[test]
fn an_empty_store_says_no_extraction_fingerprints_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    // 连库都还不存在：命令必须自己开/建库，然后给出"没有"这句读数。
    let out = run(dir.path(), &[]);
    assert_eq!(out.status.code(), Some(0), "空库是读数不是失败：{}",
               String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "no extraction fingerprints\n");
}
