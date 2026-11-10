//! `kind = "whole"`（整文件转换器源）的行为测试。
//!
//! 整文件源与 files 源的三条差别：
//! 1. **没有字节偏移**：水位只记 `{mtime_ms, size, head_sha256}`，offset 恒为 0；
//!    文件变了（mtime/size/头哈希任一）就整份重转。
//! 2. **`{segment}` 是源文件的原始路径**：不复制、不切段；转换器自己读它、写 v1 到 `{out}`。
//! 3. **多段通配**：`glob = "*/*/session.v4.jsonl.zstd"` 逐段匹配，不递归通配 `**`。

use std::path::{Path, PathBuf};

use rc_collect::config::{Config, Limits, Source, SourceKind};
use rc_collect::run::{run_once, Receipt, RunOpts, Sink};
use rc_collect::scan::{scan_whole, path_key};
use rc_collect::state::StateDoc;

const QUIET_MS: i64 = 600_000;

/// 两轮 human/agent 的合法 v1（源文件本身就是一份 v1，假转换器原样拷贝）。
const V1: &str = r#"{"format_version":1,"sessions":[{"session_key":"ses-A","device_id":"dev-dsh","source_agent":"dsh","turns":[{"role":"human","body":"HELLO-1110","created_at_ms":1730000000000},{"role":"agent","body":"HI-1110","created_at_ms":1730000001000}]}]}"#;

fn mtime_ms(p: &Path) -> i64 {
    std::fs::metadata(p)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// 整文件源的假转换器：把 `{segment}`（源文件原始路径）原样拷到 `{out}/v1.json`，
/// 并把收到的路径写到 `recorder`（记录文件放在 out 目录之外——out 目录跑完就被删）。
fn whole_converter(recorder: &Path) -> Vec<String> {
    #[cfg(windows)]
    {
        let bat = recorder.with_extension("cmd");
        std::fs::write(
            &bat,
            "@echo off\r\necho %~1>\"%~3\"\r\ncopy /Y \"%~1\" \"%~2\\v1.json\" >nul\r\n",
        )
        .unwrap();
        vec![
            "cmd".into(),
            "/C".into(),
            bat.to_string_lossy().to_string(),
            "{segment}".into(),
            "{out}".into(),
            recorder.to_string_lossy().to_string(),
        ]
    }
    #[cfg(not(windows))]
    {
        vec![
            "sh".into(),
            "-c".into(),
            "printf '%s' \"$1\" > \"$3\"; cat \"$1\" > \"$2/v1.json\"".into(),
            "sh".into(),
            "{segment}".into(),
            "{out}".into(),
            recorder.to_string_lossy().to_string(),
        ]
    }
}

fn source(root: &Path, converter: Vec<String>) -> Source {
    Source {
        name: "dsh".into(),
        kind: SourceKind::Whole,
        root: Some(root.into()),
        glob: Some("*/*/session.v4.jsonl.zstd".into()),
        db: None,
        cursor_sql: None,
        converter,
        device_id: "dev-dsh".into(),
        source_agent: "dsh".into(),
        project: "session".into(),
    }
}

fn config(root: &Path, converter: Vec<String>) -> Config {
    Config {
        limits: Limits {
            quiet_ms: QUIET_MS,
            max_files_per_run: 3,
            max_turns_per_day: 300,
            max_backfill_turns_per_day: 20_000,
        },
        sources: vec![source(root, converter)],
    }
}

/// 造一棵 `<root>/<项目目录>/<会话目录>/session.v4.jsonl.zstd`。
fn make_session(root: &Path, project: &str, session: &str) -> PathBuf {
    let dir = root.join(project).join(session);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("session.v4.jsonl.zstd");
    std::fs::write(&p, V1).unwrap();
    p
}

#[derive(Default)]
struct FakeSink {
    n: usize,
    bodies: Vec<String>,
}

impl Sink for FakeSink {
    fn submit(&mut self, v1_path: &Path) -> Result<Receipt, String> {
        self.n += 1;
        let text = std::fs::read_to_string(v1_path).unwrap();
        self.bodies.push(text);
        Ok(Receipt {
            accepted: 1,
            duplicate: 0,
            rejected: 0,
            batch_reject: None,
        })
    }
}

/// ① 变更判定：文件没变 ⇒ 不重转；变了 ⇒ 整份重转。
#[test]
fn whole_source_reconverts_only_when_the_file_changes() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("sessions");
    let file = make_session(&root, "proj-a", "sess-1");
    let recorder = d.path().join("argv.txt");
    let cfg = config(&root, whole_converter(&recorder));
    let state = d.path().join("collect-state.json");
    let mut sink = FakeSink::default();

    let now = mtime_ms(&file) + QUIET_MS + 1_000;
    let first = run_once(&cfg, &state, &mut sink, now, &RunOpts::default()).unwrap();
    assert_eq!(first[0].due, 1);
    assert_eq!(first[0].new_turns, 2);
    assert!(first[0].advanced);
    assert_eq!(sink.n, 1);

    // 水位：{mtime_ms, size, head_sha256}，没有字节偏移。
    let st = StateDoc::load(&state).unwrap();
    let cur = &st.whole[&path_key(&file)];
    assert_eq!(cur.mtime_ms, mtime_ms(&file));
    assert_eq!(cur.size, std::fs::metadata(&file).unwrap().len() as i64);
    assert!(!cur.head_sha256.is_empty());

    // 文件没变：不再重转。
    let again = run_once(&cfg, &state, &mut sink, now, &RunOpts::default()).unwrap();
    assert_eq!(again[0].due, 0);
    assert!(!again[0].advanced);
    assert_eq!(sink.n, 1);

    // 文件变了（长度变了）：整份重转。
    std::fs::write(&file, format!("{V1}\n")).unwrap();
    let now2 = mtime_ms(&file) + QUIET_MS + 1_000;
    let third = run_once(&cfg, &state, &mut sink, now2, &RunOpts::default()).unwrap();
    assert_eq!(third[0].due, 1);
    assert!(third[0].advanced);
    assert_eq!(sink.n, 2);
}

/// ② 多级 glob：`*/*/session.v4.jsonl.zstd` 逐段匹配，别的形状不命中。
#[test]
fn multi_level_glob_matches_only_the_nested_session_files() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("sessions");
    let hit_a = make_session(&root, "proj-a", "sess-1");
    let hit_b = make_session(&root, "proj-b", "sess-2");

    // 太浅：`<项目>/session.v4.jsonl.zstd`。
    std::fs::create_dir_all(root.join("proj-a")).unwrap();
    std::fs::write(
        root.join("proj-a").join("session.v4.jsonl.zstd"),
        V1,
    )
    .unwrap();
    // 太深：`<项目>/<会话>/<子目录>/session.v4.jsonl.zstd`。
    let deep = root.join("proj-a").join("sess-1").join("sub");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("session.v4.jsonl.zstd"), V1).unwrap();
    // 同层但文件名不对。
    std::fs::write(root.join("proj-a").join("sess-1").join("other.txt"), V1).unwrap();

    let src = source(&root, vec!["true".into()]);
    let due = scan_whole(&src, &StateDoc::default(), 0, i64::MAX);
    let mut got: Vec<String> = due.iter().map(|f| path_key(&f.path)).collect();
    got.sort();
    let mut want = vec![path_key(&hit_a), path_key(&hit_b)];
    want.sort();
    assert_eq!(got, want);
}

/// ③ `{segment}` 给的是源文件的原始路径（不是临时副本、不是切段）。
#[test]
fn segment_placeholder_is_the_original_source_path() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("sessions");
    let file = make_session(&root, "proj-a", "sess-1");
    let recorder = d.path().join("argv.txt");
    let cfg = config(&root, whole_converter(&recorder));
    let state = d.path().join("collect-state.json");
    let mut sink = FakeSink::default();

    let now = mtime_ms(&file) + QUIET_MS + 1_000;
    run_once(&cfg, &state, &mut sink, now, &RunOpts::default()).unwrap();

    let seen = std::fs::read_to_string(&recorder).unwrap();
    let seen = seen.trim();
    assert_eq!(
        seen,
        file.to_string_lossy(),
        "整文件源的 {{segment}} 必须是源文件的原始路径"
    );
    // 源文件本身不动（没有被拷贝、没有被消费）。
    assert_eq!(std::fs::read_to_string(&file).unwrap(), V1);
}

/// ④ 转换器产出的 v1 被正常计数，水位被推进。
#[test]
fn converter_v1_is_counted_and_watermark_advances() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("sessions");
    let file = make_session(&root, "proj-a", "sess-1");
    let recorder = d.path().join("argv.txt");
    let cfg = config(&root, whole_converter(&recorder));
    let state = d.path().join("collect-state.json");
    let mut sink = FakeSink::default();

    let now = mtime_ms(&file) + QUIET_MS + 1_000;
    let reports = run_once(&cfg, &state, &mut sink, now, &RunOpts::default()).unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].due, 1);
    assert_eq!(reports[0].new_turns, 2, "v1 里的 human+agent 轮要被数出来");
    assert_eq!(reports[0].accepted, 1);
    assert!(reports[0].advanced);
    assert_eq!(sink.n, 1);
    assert!(sink.bodies[0].contains("HELLO-1110"));

    let st = StateDoc::load(&state).unwrap();
    assert!(st.whole.contains_key(&path_key(&file)));
}

/// 配置面：whole 源必须带 root/glob，不得带 db/cursor_sql；`**` 不递归通配。
#[test]
fn whole_source_config_shape_is_checked() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("collect.toml");

    // 缺 root。
    std::fs::write(
        &p,
        r#"
[[source]]
name = "dsh"
kind = "whole"
glob = "*/*/session.v4.jsonl.zstd"
converter = ["true"]
device_id = "dev-dsh"
source_agent = "dsh"
"#,
    )
    .unwrap();
    let err = rc_collect::config::Config::load(&p).unwrap_err();
    assert!(err.contains("root"), "{err}");

    // 不得带 db。
    std::fs::write(
        &p,
        r#"
[[source]]
name = "dsh"
kind = "whole"
root = "C:/tmp/sessions"
glob = "*/*/session.v4.jsonl.zstd"
db = "C:/tmp/state.db"
converter = ["true"]
device_id = "dev-dsh"
source_agent = "dsh"
"#,
    )
    .unwrap();
    let err = rc_collect::config::Config::load(&p).unwrap_err();
    assert!(err.contains("db"), "{err}");

    // `**` 不支持。
    std::fs::write(
        &p,
        r#"
[[source]]
name = "dsh"
kind = "whole"
root = "C:/tmp/sessions"
glob = "**/session.v4.jsonl.zstd"
converter = ["true"]
device_id = "dev-dsh"
source_agent = "dsh"
"#,
    )
    .unwrap();
    let err = rc_collect::config::Config::load(&p).unwrap_err();
    assert!(err.contains("glob"), "{err}");

    // 正常形状。
    std::fs::write(
        &p,
        r#"
[[source]]
name = "dsh"
kind = "whole"
root = "C:/tmp/sessions"
glob = "*/*/session.v4.jsonl.zstd"
converter = ["true"]
device_id = "dev-dsh"
source_agent = "dsh"
"#,
    )
    .unwrap();
    let cfg = rc_collect::config::Config::load(&p).unwrap();
    assert_eq!(cfg.sources[0].kind, SourceKind::Whole);
}
