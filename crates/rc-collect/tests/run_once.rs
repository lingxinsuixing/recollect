//! `run_once` 的行为测试：转换器调用、回执、水位推进、日帽与毒化。
//!
//! 假转换器 = 把源段原样复制到 out 目录的一份 `v1.json`；源文件内容本身
//! 就是一份合法 v1（两条 human/agent 轮，`created_at_ms` 固定）。
//! 全程用 `tempfile`，不读真实用户数据、不联网。

use std::path::{Path, PathBuf};

use rc_collect::config::{Config, Limits, Source, SourceKind};
use rc_collect::run::{human_agent_turns, run_once, Receipt, RunOpts, Sink};
use rc_collect::state::StateDoc;

/// 两轮 human+agent 的合法 v1；`created_at_ms` 固定，便于断言。
const V1: &str = r#"{"format_version":1,"sessions":[{"session_key":"ses-A","device_id":"dev-1","source_agent":"qoder","turns":[{"role":"human","body":"HELLO-1110","created_at_ms":1730000000000},{"role":"agent","body":"HI-1110","created_at_ms":1730000001000}]}]}"#;

const QUIET_MS: i64 = 600_000;

fn mtime_ms(p: &Path) -> i64 {
    std::fs::metadata(p)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// 假转换器：`{segment}` 原样拷到 `{out}/v1.json`。
fn copy_converter() -> Vec<String> {
    #[cfg(windows)]
    {
        vec![
            "cmd".into(),
            "/C".into(),
            "copy".into(),
            "/Y".into(),
            "{segment}".into(),
            "{out}\\v1.json".into(),
        ]
    }
    #[cfg(not(windows))]
    {
        vec![
            "sh".into(),
            "-c".into(),
            "cp \"$1\" \"$2\"".into(),
            "x".into(),
            "{segment}".into(),
            "{out}/v1.json".into(),
        ]
    }
}

/// 假转换器：退出 0 但不在 out 目录留任何 json。
fn silent_converter() -> Vec<String> {
    #[cfg(windows)]
    {
        vec!["cmd".into(), "/C".into(), "exit".into(), "0".into()]
    }
    #[cfg(not(windows))]
    {
        vec!["sh".into(), "-c".into(), "exit 0".into()]
    }
}

/// 假转换器：一旦真的被执行就会留下标记文件（dry_run 测试用）。
fn marker_converter(marker: &Path) -> Vec<String> {
    let m = marker.to_string_lossy().to_string();
    #[cfg(windows)]
    {
        vec![
            "cmd".into(),
            "/C".into(),
            "echo".into(),
            "x".into(),
            ">".into(),
            m,
        ]
    }
    #[cfg(not(windows))]
    {
        vec![
            "sh".into(),
            "-c".into(),
            "echo x > \"$1\"".into(),
            "x".into(),
            m,
        ]
    }
}

fn source(root: &Path, converter: Vec<String>) -> Source {
    Source {
        name: "qoder".into(),
        kind: SourceKind::Files,
        root: Some(root.into()),
        glob: Some("*.jsonl".into()),
        db: None,
        cursor_sql: None,
        converter,
        device_id: "dev-1".into(),
        source_agent: "qoder".into(),
        project: "session".into(),
    }
}

fn config(root: &Path, converter: Vec<String>, max_files: u32, max_turns: u32) -> Config {
    Config {
        limits: Limits {
            quiet_ms: QUIET_MS,
            max_files_per_run: max_files,
            max_turns_per_day: max_turns,
            max_backfill_turns_per_day: 20_000,
        },
        sources: vec![source(root, converter)],
    }
}

fn write_v1(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("{V1}\n")).unwrap();
    p
}

struct FakeSink {
    n: usize,
    receipt: fn() -> Receipt,
}

impl Default for FakeSink {
    fn default() -> Self {
        FakeSink {
            n: 0,
            receipt: || Receipt {
                accepted: 1,
                duplicate: 0,
                rejected: 0,
                batch_reject: None,
            },
        }
    }
}

impl Sink for FakeSink {
    fn submit(&mut self, _v1: &Path) -> Result<Receipt, String> {
        self.n += 1;
        Ok((self.receipt)())
    }
}

fn key(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

#[test]
fn human_agent_turns_counts_both_roles_across_sessions() {
    let d = tempfile::tempdir().unwrap();
    let p = write_v1(d.path(), "a.jsonl");
    assert_eq!(human_agent_turns(&p).unwrap(), 2);
    // 多 session 文件累加。
    let two = r#"{"format_version":1,"sessions":[{"session_key":"a","device_id":"d","source_agent":"a","turns":[{"role":"human","body":"x"}]},{"session_key":"b","device_id":"d","source_agent":"a","turns":[{"role":"agent","body":"y"},{"role":"system","body":"z"}]}]}"#;
    let q = d.path().join("b.jsonl");
    std::fs::write(&q, two).unwrap();
    assert_eq!(human_agent_turns(&q).unwrap(), 2);
}

#[test]
fn first_run_advances_offset_and_second_run_is_idle() {
    let d = tempfile::tempdir().unwrap();
    let src_dir = d.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let f = write_v1(&src_dir, "a.jsonl");
    let now = mtime_ms(&f) + QUIET_MS + 1_000;

    let cfg = config(&src_dir, copy_converter(), 3, 300);
    let state_path = d.path().join("collect-state.json");
    let mut sink = FakeSink::default();
    let opts = RunOpts::default();

    let reports = run_once(&cfg, &state_path, &mut sink, now, &opts).unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].due, 1);
    assert_eq!(reports[0].new_turns, 2);
    assert_eq!(reports[0].accepted, 1);
    assert!(reports[0].advanced);
    assert_eq!(sink.n, 1);

    let st = StateDoc::load(&state_path).unwrap();
    let cur = &st.files[&key(&f)];
    assert_eq!(cur.offset, std::fs::metadata(&f).unwrap().len() as i64);
    assert_eq!(cur.last_turns, 2);
    assert!(!cur.head_sha256.is_empty());

    let again = run_once(&cfg, &state_path, &mut sink, now, &opts).unwrap();
    assert_eq!(again[0].due, 0);
    assert!(!again[0].advanced);
    assert_eq!(sink.n, 1);
}

#[test]
fn batch_reject_never_advances_and_poisons_after_three_attempts() {
    let d = tempfile::tempdir().unwrap();
    let src_dir = d.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let f = write_v1(&src_dir, "a.jsonl");
    let now = mtime_ms(&f) + QUIET_MS + 1_000;

    let cfg = config(&src_dir, copy_converter(), 3, 300);
    let state_path = d.path().join("collect-state.json");
    let mut sink = FakeSink {
        n: 0,
        receipt: || Receipt {
            accepted: 0,
            duplicate: 0,
            rejected: 2,
            batch_reject: Some("bad_batch".into()),
        },
    };
    let opts = RunOpts::default();

    for _ in 0..3 {
        let reports = run_once(&cfg, &state_path, &mut sink, now, &opts).unwrap();
        assert_eq!(reports[0].due, 1);
        assert!(!reports[0].advanced);
        assert!(
            reports[0].reason.contains("batch_reject"),
            "{}",
            reports[0].reason
        );
    }
    assert_eq!(sink.n, 3);

    let st = StateDoc::load(&state_path).unwrap();
    assert!(!st.files.contains_key(&key(&f)));
    let poison = &st.poisoned[&key(&f)];
    assert_eq!(poison.attempts, 3);
    assert_eq!(poison.last_code, "bad_batch");

    // 毒化后默认跳过：不再调 sink。
    let skipped = run_once(&cfg, &state_path, &mut sink, now, &opts).unwrap();
    assert_eq!(sink.n, 3);
    assert!(
        skipped[0].reason.contains("poisoned"),
        "{}",
        skipped[0].reason
    );
}

#[test]
fn dry_run_does_not_invoke_converter() {
    let d = tempfile::tempdir().unwrap();
    let src_dir = d.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let f = write_v1(&src_dir, "a.jsonl");
    let now = mtime_ms(&f) + QUIET_MS + 1_000;
    let marker = d.path().join("converter-ran.txt");

    let cfg = config(&src_dir, marker_converter(&marker), 3, 300);
    let state_path = d.path().join("collect-state.json");
    let mut sink = FakeSink::default();
    let opts = RunOpts {
        dry_run: true,
        ..Default::default()
    };

    let reports = run_once(&cfg, &state_path, &mut sink, now, &opts).unwrap();
    assert_eq!(reports[0].due, 1);
    assert_eq!(reports[0].new_turns, 0);
    assert_eq!(reports[0].reason, "dry_run");
    assert!(!reports[0].advanced);
    assert_eq!(sink.n, 0);
    assert!(!marker.exists(), "dry_run must not execute the converter");

    let st = StateDoc::load(&state_path).unwrap();
    assert!(st.files.is_empty());
}

#[test]
fn max_files_per_run_limits_files_processed() {
    let d = tempfile::tempdir().unwrap();
    let src_dir = d.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let a = write_v1(&src_dir, "a.jsonl");
    let b = write_v1(&src_dir, "b.jsonl");
    let now = mtime_ms(&a).max(mtime_ms(&b)) + QUIET_MS + 1_000;

    let cfg = config(&src_dir, copy_converter(), 1, 300);
    let state_path = d.path().join("collect-state.json");
    let mut sink = FakeSink::default();
    let opts = RunOpts::default();

    let reports = run_once(&cfg, &state_path, &mut sink, now, &opts).unwrap();
    assert_eq!(reports[0].due, 2);
    assert_eq!(sink.n, 1);
    assert!(reports[0].advanced);

    let st = StateDoc::load(&state_path).unwrap();
    assert_eq!(st.files.len(), 1);
}

#[test]
fn daily_turn_cap_stops_further_files() {
    let d = tempfile::tempdir().unwrap();
    let src_dir = d.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let a = write_v1(&src_dir, "a.jsonl");
    let now = mtime_ms(&a) + QUIET_MS + 1_000;

    // 日帽 = 单个文件正好 2 轮：首跑用满。
    let cfg = config(&src_dir, copy_converter(), 5, 2);
    let state_path = d.path().join("collect-state.json");
    let mut sink = FakeSink::default();
    let opts = RunOpts::default();

    let first = run_once(&cfg, &state_path, &mut sink, now, &opts).unwrap();
    assert!(first[0].advanced);
    assert_eq!(sink.n, 1);
    let st = StateDoc::load(&state_path).unwrap();
    assert!(st.daily.values().any(|d| d.used == 2));

    // 新文件到达，但今日帽已满：不再处理。
    let b = write_v1(&src_dir, "b.jsonl");
    let now2 = mtime_ms(&b) + QUIET_MS + 1_000;
    let second = run_once(&cfg, &state_path, &mut sink, now2, &opts).unwrap();
    assert_eq!(second[0].due, 1);
    assert!(!second[0].advanced);
    assert!(second[0].reason.contains("cap"), "{}", second[0].reason);
    assert_eq!(sink.n, 1);

    let st = StateDoc::load(&state_path).unwrap();
    assert!(!st.files.contains_key(&key(&b)));
}

#[test]
fn zero_json_from_converter_is_reported_and_not_advanced() {
    let d = tempfile::tempdir().unwrap();
    let src_dir = d.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let f = write_v1(&src_dir, "a.jsonl");
    let now = mtime_ms(&f) + QUIET_MS + 1_000;

    let cfg = config(&src_dir, silent_converter(), 3, 300);
    let state_path = d.path().join("collect-state.json");
    let mut sink = FakeSink::default();
    let opts = RunOpts::default();

    let reports = run_once(&cfg, &state_path, &mut sink, now, &opts).unwrap();
    assert_eq!(reports[0].due, 1);
    assert!(
        reports[0].reason.contains("converter_output"),
        "{}",
        reports[0].reason
    );
    assert!(!reports[0].advanced);
    assert_eq!(sink.n, 0);

    let st = StateDoc::load(&state_path).unwrap();
    assert!(!st.files.contains_key(&key(&f)));
}

/// 日帽是滚动 24 小时窗口：窗口过期后自动归零，跨 UTC 午夜也不会把昨天的 used 带进今天。
#[test]
fn daily_cap_resets_after_window_expires() {
    use rc_collect::state::DailyUse;
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("src");
    std::fs::create_dir_all(&root).unwrap();
    let cfg = Config {
        limits: Limits {
            quiet_ms: QUIET_MS,
            max_files_per_run: 3,
            max_turns_per_day: 2,
            max_backfill_turns_per_day: 20_000,
        },
        sources: vec![Source {
            name: "s".into(), kind: SourceKind::Files, root: Some(root), glob: Some("*.jsonl".into()),
            db: None, cursor_sql: None, converter: copy_converter(),
            device_id: "dev-1".into(), source_agent: "qoder".into(), project: "session".into(),
        }],
    };
    let state = d.path().join("collect-state.json");
    let now: i64 = 1_800_000_000_000;
    let mut st = StateDoc::default();
    st.daily.insert("window".into(), DailyUse { cap: 2, used: 2, start_ms: now - rc_collect::run::DAY_MS - 1 });
    st.save(&state).unwrap();

    let mut sink = FakeSink::default();
    run_once(&cfg, &state, &mut sink, now, &RunOpts::default()).unwrap();
    let back = StateDoc::load(&state).unwrap();
    let w = &back.daily["window"];
    assert_eq!(w.used, 0, "窗口过期后 used 必须归零");
    assert_eq!(w.start_ms, now);
}
