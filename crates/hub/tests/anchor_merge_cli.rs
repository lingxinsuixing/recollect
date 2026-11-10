//! `hub anchor merge` 的 CLI 面：默认预演、JSON 形状、`--only A`、幂等。
//!
//! 真把引用搬走的证据在 `rc-memory/tests/anchor_merge.rs`；这里只证运维面：
//! 不带 `--apply` 一格不写、`--json` 是数组且带 A/B/C、`--apply --only A` 只合并 A 类、
//! 第二次运行 `merged=0`。
use rc_store::Root;
use rusqlite::Connection;
use serde_json::Value;
use std::process::Output;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(args: &[&str]) -> Output {
    std::process::Command::new(bin()).args(args).output().expect("hub binary")
}

fn text(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn err(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn fake(n: u8) -> String {
    format!("{:064x}", n)
}

fn insert_anchor(conn: &Connection, id: &str, kind: &str, canonical: &str, status: &str) {
    conn.execute(
        "INSERT INTO anchor(id, kind, canonical, scope_kind, scope_key, status,
                            created_at_ms, updated_at_ms, norm_version)
         VALUES(?1, ?2, ?3, 'project', 'proj-cli', ?4, 1700000000000, 1700000000000, 1)",
        rusqlite::params![id, kind, canonical, status],
    )
    .unwrap();
}

/// 一组 A（同 kind 同名，active/provisional 两格）+ 一组 B（跨 kind 同名）。
/// 返回 (a_from, a_to, b_from, b_to)。
fn seed(dir: &std::path::Path) -> (String, String, String, String) {
    let root = Root::open(dir).unwrap();
    let conn = root.memory().conn();
    let (a_from, a_to) = (fake(2), fake(1));
    let (b_from, b_to) = (fake(4), fake(3));
    insert_anchor(conn, &a_to, "org", "同一个组织", "active");
    insert_anchor(conn, &a_from, "org", "同一个组织", "provisional");
    insert_anchor(conn, &b_to, "work", "d3", "active");
    insert_anchor(conn, &b_from, "agent", "d3", "provisional");
    (a_from, a_to, b_from, b_to)
}

fn status(dir: &std::path::Path, id: &str) -> String {
    let root = Root::open(dir).unwrap();
    root.memory()
        .conn()
        .query_row("SELECT status FROM anchor WHERE id=?1", [id], |r| r.get(0))
        .unwrap()
}

fn count(dir: &std::path::Path, table: &str) -> i64 {
    let root = Root::open(dir).unwrap();
    root.memory()
        .conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn merge_flags_are_wired() {
    let help = run(&["anchor", "merge", "--help"]);
    assert!(help.status.success());
    let h = text(&help);
    for flag in ["--apply", "--only", "--json", "--reason", "--dry-run"] {
        assert!(h.contains(flag), "缺 {flag}：{h}");
    }
}

#[test]
fn default_run_is_a_read_only_plan_with_a_and_b_classified() {
    let dir = tempfile::tempdir().unwrap();
    let (a_from, a_to, b_from, b_to) = seed(dir.path());

    let out = run(&["anchor", "merge", "--json", "--data-dir", dir.path().to_str().unwrap()]);
    assert!(out.status.success(), "{}", err(&out));
    let plan: Vec<Value> = serde_json::from_str(text(&out).trim()).expect("预演是 JSON 数组");
    assert_eq!(plan.len(), 2, "{plan:?}");
    let a = plan.iter().find(|r| r["class"] == "A").expect("A 类一行");
    assert_eq!(a["action"], "merge");
    assert_eq!((a["from_id"].as_str(), a["to_id"].as_str()),
               (Some(a_from.as_str()), Some(a_to.as_str())));
    assert_eq!(a["from_kind"], "org");
    let b = plan.iter().find(|r| r["class"] == "B").expect("B 类一行");
    assert_eq!(b["action"], "manual");
    assert_eq!(b["reason"], "cross_kind");
    assert_eq!((b["from_id"].as_str(), b["to_id"].as_str()),
               (Some(b_from.as_str()), Some(b_to.as_str())));

    // 一格都没写：审计 0、状态不变、锚集合不变。
    assert_eq!(count(dir.path(), "anchor_merge"), 0, "预演不许写审计行");
    assert_eq!(status(dir.path(), &a_from), "provisional");
    assert_eq!(status(dir.path(), &b_from), "provisional");
    assert_eq!(count(dir.path(), "anchor"), 4);
}

#[test]
fn apply_only_a_merges_exactly_the_a_group_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let (a_from, a_to, b_from, b_to) = seed(dir.path());
    let dd = dir.path().to_str().unwrap();

    let out = run(&[
        "anchor", "merge", "--apply", "--only", "A", "--reason", "同名同 kind",
        "--json", "--data-dir", dd,
    ]);
    assert!(out.status.success(), "{}", err(&out));
    let applied: Vec<Value> = serde_json::from_str(text(&out).trim()).expect("apply 也是 JSON 数组");
    assert_eq!(applied.len(), 1, "只合并 A 类：{applied:?}");
    assert_eq!(applied[0]["class"], "A");
    assert_eq!(applied[0]["from_id"], a_from);

    assert_eq!(count(dir.path(), "anchor_merge"), 1);
    assert_eq!(status(dir.path(), &a_from), "merged", "旧锚退场但不物理删");
    assert_eq!(status(dir.path(), &a_to), "active");
    assert_eq!(status(dir.path(), &b_from), "provisional", "B 类一行不动");
    assert_eq!(status(dir.path(), &b_to), "active");
    assert_eq!(count(dir.path(), "anchor"), 4);

    // 幂等：再跑一次 merged=0，审计行不长第二条。
    let again = run(&["anchor", "merge", "--apply", "--only", "A", "--data-dir", dd]);
    assert!(again.status.success(), "{}", err(&again));
    assert!(text(&again).contains("merged=0"), "{}", text(&again));
    assert_eq!(count(dir.path(), "anchor_merge"), 1);
}

#[test]
fn apply_with_only_b_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (_a_from, a_to, b_from, b_to) = seed(dir.path());
    let out = run(&[
        "anchor", "merge", "--apply", "--only", "B", "--data-dir", dir.path().to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", err(&out));
    assert!(text(&out).contains("merged=0"), "{}", text(&out));
    assert_eq!(count(dir.path(), "anchor_merge"), 0);
    assert_eq!(status(dir.path(), &a_to), "active");
    assert_eq!(status(dir.path(), &b_from), "provisional");
    assert_eq!(status(dir.path(), &b_to), "active");
}
