//! sqlite 源：首跑播种、静默闸、按水位增量、只按回执推进。
//!
//! 迷你库用 `rusqlite` 在临时目录造（`events(id, created_at_ms)`），不读任何真实库。
//! 假转换器把一份固定的 v1 拷到 `{out}/v1.json`，同时把 `{since_ms}` 编进标记文件名
//! 落到测试自己指定的目录——`process_*` 结束时会删掉 `<state_dir>/tmp`，不能在那里断言。

use std::path::{Path, PathBuf};

use rc_collect::config::{Config, Limits, Source, SourceKind};
use rc_collect::run::{run_once, Receipt, RunOpts, Sink};
use rc_collect::scan::{scan_sqlite, SqliteDue};
use rc_collect::state::StateDoc;
use rusqlite::Connection;

const V1: &str = r#"{"format_version":1,"sessions":[{"session_key":"mcode:s1","device_id":"dev-mcode","source_agent":"mcode","turns":[{"role":"human","body":"MCODE-1110","created_at_ms":1730000000000},{"role":"agent","body":"ACK-1110","created_at_ms":1730000001000}]}]}"#;

const QUIET_MS: i64 = 600_000;
const T0: i64 = 1_730_000_000_000;
const T1: i64 = 1_730_000_100_000;
const T2: i64 = 1_730_000_600_000;

const CURSOR_SQL: &str = "select max(created_at_ms) from events";

fn make_db(dir: &Path, rows: &[i64]) -> PathBuf {
    let p = dir.join("runtime-state.sqlite");
    {
        let con = Connection::open(&p).unwrap();
        con.execute_batch(
            "create table events (id integer primary key autoincrement, created_at_ms integer not null);",
        )
        .unwrap();
        for ts in rows {
            con.execute("insert into events (created_at_ms) values (?1)", [ts])
                .unwrap();
        }
    }
    p
}

fn insert_row(db: &Path, ts: i64) {
    let con = Connection::open(db).unwrap();
    con.execute("insert into events (created_at_ms) values (?1)", [ts])
        .unwrap();
}

fn source(db: &Path, converter: Vec<String>) -> Source {
    Source {
        name: "mcode".into(),
        kind: SourceKind::Sqlite,
        root: None,
        glob: None,
        db: Some(db.into()),
        cursor_sql: Some(CURSOR_SQL.into()),
        converter,
        device_id: "dev-mcode".into(),
        source_agent: "mcode".into(),
        project: "session".into(),
    }
}

fn config(db: &Path, converter: Vec<String>) -> Config {
    Config {
        limits: Limits {
            quiet_ms: QUIET_MS,
            max_files_per_run: 3,
            max_turns_per_day: 300,
            max_backfill_turns_per_day: 20_000,
        },
        sources: vec![source(db, converter)],
    }
}

/// 假转换器：`{out}/v1.json` = 固定 v1；`marks/since-<since_ms>.txt` 留一个水位标记。
///
/// Windows 走一份测试自己写的 `.bat`：`cmd /C` 一次只认一条命令，把多条命令塞进
/// 单个 argv 元素会被 cmd 的引号剥离规则弄坏，批次文件里写就干净了。
fn fake_converter(v1_fixture: &Path, marks: &Path, dir: &Path) -> Vec<String> {
    let fix = v1_fixture.to_string_lossy().to_string();
    let mk = marks.to_string_lossy().to_string();
    #[cfg(windows)]
    {
        let bat = dir.join("fake-converter.cmd");
        std::fs::write(
            &bat,
            "@echo off\r\n\
             copy /Y \"%~1\" \"%~2\\v1.json\" >nul\r\n\
             copy /Y \"%~1\" \"%~3\\since-%~4.txt\" >nul\r\n",
        )
        .unwrap();
        let _ = (&mk,);
        vec![
            "cmd".into(),
            "/C".into(),
            bat.to_string_lossy().to_string(),
            fix,
            "{out}".into(),
            mk,
            "{since_ms}".into(),
        ]
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        vec![
            "sh".into(),
            "-c".into(),
            "cp \"$1\" \"$2/v1.json\" && cp \"$1\" \"$3/since-$4.txt\"".into(),
            "x".into(),
            fix,
            "{out}".into(),
            mk,
            "{since_ms}".into(),
        ]
    }
}

#[derive(Default)]
struct FakeSink {
    calls: usize,
    fail: bool,
}

impl Sink for FakeSink {
    fn submit(&mut self, _v1: &Path) -> Result<Receipt, String> {
        self.calls += 1;
        if self.fail {
            return Err("import_failed".into());
        }
        Ok(Receipt {
            accepted: 1,
            duplicate: 0,
            rejected: 0,
            batch_reject: None,
        })
    }
}

struct Env {
    _dir: tempfile::TempDir,
    db: PathBuf,
    marks: PathBuf,
    state: PathBuf,
    converter: Vec<String>,
}

fn env(rows: &[i64]) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let db = make_db(dir.path(), rows);
    let marks = dir.path().join("marks");
    std::fs::create_dir_all(&marks).unwrap();
    let fixture = dir.path().join("fixture.json");
    std::fs::write(&fixture, format!("{V1}\n")).unwrap();
    let converter = fake_converter(&fixture, &marks, dir.path());
    let state = dir.path().join("collect-state.json");
    Env {
        db,
        marks,
        state,
        converter,
        _dir: dir,
    }
}

impl Env {
    fn since_marks(&self) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(&self.marks)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        out.sort();
        out
    }
}

#[test]
fn first_run_seeds_watermark_without_converting() {
    let e = env(&[T0, T1]);
    let cfg = config(&e.db, e.converter.clone());
    let mut sink = FakeSink::default();

    let reports = run_once(&cfg, &e.state, &mut sink, T1 + QUIET_MS + 1_000, &RunOpts::default())
        .unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].due, 1);
    assert_eq!(reports[0].reason, "seeded");
    assert!(!reports[0].advanced);
    assert_eq!(sink.calls, 0, "首跑不转换");
    assert!(e.since_marks().is_empty());

    let st = StateDoc::load(&e.state).unwrap();
    assert_eq!(st.sqlite["mcode"].last_created_at_ms, T1);
}

#[test]
fn new_rows_become_due_with_old_watermark_as_since() {
    let e = env(&[T0, T1]);
    let src = source(&e.db, e.converter.clone());
    let mut st = StateDoc::default();
    st.sqlite.insert(
        "mcode".into(),
        rc_collect::state::SqliteCursor {
            last_created_at_ms: T1,
            last_msg_id: None,
        },
    );

    // 静默窗口内：不收。
    assert_eq!(scan_sqlite(&src, &st, QUIET_MS, T1 + 1_000).unwrap(), None);
    // 没有新行（max 仍等于水位）：不收。
    assert_eq!(
        scan_sqlite(&src, &st, QUIET_MS, T1 + QUIET_MS + 1_000).unwrap(),
        None
    );

    insert_row(&e.db, T2);
    let due = scan_sqlite(&src, &st, QUIET_MS, T2 + 1_000).unwrap();
    assert_eq!(due, None, "新行还在静默窗口内");
    let due = scan_sqlite(&src, &st, QUIET_MS, T2 + QUIET_MS + 1_000)
        .unwrap()
        .expect("过了静默窗口就该 due");
    assert_eq!(
        due,
        SqliteDue {
            since_ms: T1,
            latest_ms: T2
        }
    );
}

#[test]
fn receipt_advances_watermark_to_latest_and_passes_since() {
    let e = env(&[T0, T1]);
    let cfg = config(&e.db, e.converter.clone());
    let mut sink = FakeSink::default();
    let opts = RunOpts::default();

    // 首跑播种到 T1。
    run_once(&cfg, &e.state, &mut sink, T1 + QUIET_MS + 1_000, &opts).unwrap();

    insert_row(&e.db, T2);
    let reports = run_once(&cfg, &e.state, &mut sink, T2 + QUIET_MS + 1_000, &opts).unwrap();
    assert_eq!(reports[0].due, 1);
    assert_eq!(reports[0].new_turns, 2);
    assert_eq!(reports[0].accepted, 1);
    assert!(reports[0].advanced);
    assert_eq!(reports[0].reason, "ok");
    assert_eq!(sink.calls, 1);

    // 转换器拿到的是旧水位。
    assert_eq!(e.since_marks(), vec![format!("since-{T1}.txt")]);

    let st = StateDoc::load(&e.state).unwrap();
    assert_eq!(st.sqlite["mcode"].last_created_at_ms, T2);

    // 再跑：max 等于水位 ⇒ idle，不再调转换器。
    let again = run_once(&cfg, &e.state, &mut sink, T2 + QUIET_MS + 1_000, &opts).unwrap();
    assert_eq!(again[0].due, 0);
    assert_eq!(again[0].reason, "idle");
    assert_eq!(sink.calls, 1);
}

#[test]
fn sink_failure_leaves_watermark_untouched() {
    let e = env(&[T0, T1]);
    let cfg = config(&e.db, e.converter.clone());
    let opts = RunOpts::default();

    let mut good = FakeSink::default();
    run_once(&cfg, &e.state, &mut good, T1 + QUIET_MS + 1_000, &opts).unwrap();

    insert_row(&e.db, T2);
    let mut bad = FakeSink {
        calls: 0,
        fail: true,
    };
    let reports = run_once(&cfg, &e.state, &mut bad, T2 + QUIET_MS + 1_000, &opts).unwrap();
    assert_eq!(reports[0].due, 1);
    assert!(!reports[0].advanced);
    assert!(
        reports[0].reason.contains("sink_failed"),
        "{}",
        reports[0].reason
    );
    assert_eq!(bad.calls, 1);

    let st = StateDoc::load(&e.state).unwrap();
    assert_eq!(st.sqlite["mcode"].last_created_at_ms, T1, "失败不推进");
    assert!(
        st.poisoned.is_empty(),
        "运维失败不计数毒化：{:?}",
        st.poisoned
    );

    // 恢复后再跑：从旧水位重新转换，成功即推进。
    let mut good = FakeSink::default();
    let retry = run_once(&cfg, &e.state, &mut good, T2 + QUIET_MS + 1_000, &opts).unwrap();
    assert!(retry[0].advanced);
    let st = StateDoc::load(&e.state).unwrap();
    assert_eq!(st.sqlite["mcode"].last_created_at_ms, T2);
}

#[test]
fn missing_db_reports_reason_instead_of_failing_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.sqlite");
    let cfg = config(&missing, vec!["true".into()]);
    let state = dir.path().join("collect-state.json");
    let mut sink = FakeSink::default();

    let reports = run_once(&cfg, &state, &mut sink, T1 + QUIET_MS + 1_000, &RunOpts::default())
        .unwrap();
    assert_eq!(reports[0].reason, "sqlite_failed");
    assert_eq!(sink.calls, 0);
}
