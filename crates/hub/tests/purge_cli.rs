// crates/hub/tests/purge_cli.rs
//! T5-B：清除的两段式。无 `--confirm` 只写抑制账（转录行数不变），
//! 有 `--confirm` 才物理删这一个会话/这一台设备的转录行。
//! CLI 是唯一出口面：`suppress`/`purge` 有没有接线，只有真进程的 stdout 与库里的行数看得见。
use rc_store::Root;
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("hub binary")
}

/// 合成一轮转录。`turn_uid` 按位编号；`ingested_at_ms` 随编号递增。
/// `sessions` 行顺手 `INSERT OR REPLACE`：purge confirm 用例要断它还在
/// （会话行是归属的一半，删转录行不许把它一起删掉）。
fn synth_turn(t: &rc_store::TranscriptHandle, i: u32, session: &str, device: &str) {
    let uid = format!("{:064x}", i + 1);
    t.conn()
        .execute(
            "INSERT INTO turns(turn_uid, session_key, device_id, role, body, body_fp,
                               turn_key_tier, project_hint, created_at_ms, ingested_at_ms)
             VALUES(?1, ?2, ?3, 'human', '合成正文', '合成指纹', 1, NULL, ?4, ?4)",
            rusqlite::params![uid, session, device, 1000 + i as i64],
        )
        .unwrap();
    t.conn()
        .execute(
            "INSERT INTO sessions(session_key, device_id, source_agent, machine,
                                  project_hint, last_saved_at_ms)
             VALUES(?1, ?2, 'agent', NULL, NULL, ?3)
             ON CONFLICT(session_key, device_id)
             DO UPDATE SET last_saved_at_ms=excluded.last_saved_at_ms",
            rusqlite::params![session, device, 1000 + i as i64],
        )
        .unwrap();
}

fn data_arg(data: &Path) -> String {
    data.to_str().unwrap().to_string()
}

fn count_turns(root: &Root) -> i64 {
    root.transcript()
        .conn()
        .query_row("SELECT count(*) FROM turns", [], |r| r.get(0))
        .unwrap()
}

fn count_sessions(root: &Root) -> i64 {
    root.transcript()
        .conn()
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .unwrap()
}

fn count_purge_audits(root: &Root, detail: &str) -> i64 {
    root.ledger()
        .conn()
        .query_row(
            "SELECT count(*) FROM audit_log WHERE action='purge' AND detail=?1",
            [detail],
            |r| r.get(0),
        )
        .unwrap()
}

fn counter(root: &Root, metric: &str) -> Option<i64> {
    rc_ledger::counters::read_all(root.ledger())
        .unwrap()
        .into_iter()
        .find(|r| {
            r.subject_kind == "maintenance" && r.subject_id == "purge" && r.metric == metric
        })
        .map(|r| r.count)
}

#[test]
fn purge_without_confirm_only_writes_the_suppression_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let root = Root::open(&data).unwrap();
        for i in 0..3 {
            synth_turn(root.transcript(), i, "sess-a", "dev-a");
        }
    }
    let dd = data_arg(&data);
    let out = run(&["purge", "--session-key", "sess-a", "--data-dir", &dd]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "无确认的那一段必须成功退出：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象");
    assert_eq!(doc["scope"], "session");
    assert_eq!(doc["suppressed_turns"], 3);
    assert_eq!(doc["deleted"], false);

    let root = Root::open(&data).unwrap();
    assert_eq!(count_turns(&root), 3, "无确认的那一段不许物理删行");
    assert_eq!(
        count_purge_audits(&root, "suppressed"),
        1,
        "抑制段必须恰好留一行 purge+suppressed 审计"
    );
    assert_eq!(
        count_purge_audits(&root, "deleted"),
        0,
        "没确认就出现了 deleted 审计"
    );
    assert_eq!(
        counter(&root, "pending_redistill_turns"),
        Some(3),
        "抑制段必须把待重蒸的轮数记进 pending_redistill_turns"
    );
}

#[test]
fn purge_with_confirm_deletes_the_transcript_rows() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let root = Root::open(&data).unwrap();
        for i in 0..3 {
            synth_turn(root.transcript(), i, "sess-a", "dev-a");
        }
    }
    let dd = data_arg(&data);
    let out = run(&[
        "purge",
        "--session-key",
        "sess-a",
        "--confirm",
        "--data-dir",
        &dd,
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "有确认的那一段必须成功退出：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象");
    assert_eq!(doc["scope"], "session");
    assert_eq!(doc["suppressed_turns"], 3);
    assert_eq!(doc["deleted"], true);

    let root = Root::open(&data).unwrap();
    assert_eq!(count_turns(&root), 0, "确认之后这一个会话的转录行必须被删光");
    assert_eq!(
        count_sessions(&root),
        1,
        "会话行是归属的一半，purge 不许把它一起删掉"
    );
    assert_eq!(
        counter(&root, "purged_turns"),
        Some(3),
        "物理删的那一段必须把删掉的轮数记进 purged_turns"
    );
    assert_eq!(
        count_purge_audits(&root, "deleted"),
        1,
        "物理删必须恰好留一行 purge+deleted 审计"
    );
    assert_eq!(
        count_purge_audits(&root, "suppressed"),
        0,
        "直接确认的那一段不该再有 suppressed 审计"
    );
}

#[test]
fn purge_blocks_a_later_import_of_the_same_scope() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let root = Root::open(&data).unwrap();
        for i in 0..2 {
            synth_turn(root.transcript(), i, "sess-a", "dev-a");
        }
    }
    let dd = data_arg(&data);
    let out = run(&[
        "purge",
        "--session-key",
        "sess-a",
        "--confirm",
        "--data-dir",
        &dd,
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));

    let input = dir.path().join("reimport.jsonl");
    std::fs::write(
        &input,
        "{\"format_version\":1,\"sessions\":[{\"session_key\":\"sess-a\",\"device_id\":\"dev-a\",\"source_agent\":\"agent\",\"turns\":[{\"role\":\"human\",\"body\":\"purged session re-import\",\"created_at_ms\":2000}]}]}\n",
    )
    .unwrap();
    let out = run(&[
        "import",
        input.to_str().unwrap(),
        "--data-dir",
        &dd,
        "--json",
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["turns_accepted"], 0, "墓碑挡住的 turn 不许落库");
    assert_eq!(doc["tombstoned"], 1, "必须明确报告被墓碑挡住的 turn 数");
    let root = Root::open(&data).unwrap();
    assert_eq!(count_turns(&root), 0, "purge 之后同 scope 的导入不能复活行");
}

#[test]
fn purge_refuses_a_list_of_ids() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let root = Root::open(&data).unwrap();
        for i in 0..3 {
            synth_turn(root.transcript(), i, "sess-a", "dev-a");
        }
    }
    let dd = data_arg(&data);
    let both = run(&[
        "purge",
        "--session-key",
        "sess-a",
        "--device-id",
        "dev-a",
        "--data-dir",
        &dd,
    ]);
    let neither = run(&["purge", "--data-dir", &dd]);
    for out in [&both, &neither] {
        assert_eq!(
            out.status.code(),
            Some(3),
            "参数不是恰好一个口径时必须退出 3：{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.stdout.is_empty(), "拒绝时 stdout 必须为空");
        // 拒绝必须点名 purge 的用法，而不是落到"这个子命令不存在"那一支：
        // 后者也退出 3、stdout 也空，光看退出码分不出这两件事。
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("purge"),
            "拒绝串里没有 purge 的用法：{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let root = Root::open(&data).unwrap();
    assert_eq!(count_turns(&root), 3, "被拒的调用一个转录行都不许动");
    assert_eq!(
        count_purge_audits(&root, "suppressed") + count_purge_audits(&root, "deleted"),
        0,
        "被拒的调用一条 purge 审计都不许写"
    );
}
