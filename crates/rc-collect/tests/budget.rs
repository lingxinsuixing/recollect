//! P1：日常额度（`window`）与历史回填额度（`backfill`）是两条独立的预算。
//!
//! 这一面钉的是"额度互不消耗"与"历史 vs 日常的判据"：回填不能把当天的新会话额度
//! 吃光（2026-11-10 实测：一次回填用掉 1,452 轮，额度到 11-11 才恢复）。
//! 迷你库用 `rusqlite` 在临时目录造，假转换器把一份固定的 v1 拷到 `{out}/v1.json`
//! 并把 `{since_ms}` 编进标记文件名。

use std::path::{Path, PathBuf};

use rc_collect::config::{Config, Limits, Source, SourceKind};
use rc_collect::run::{run_once, Receipt, RunOpts, Sink};
use rc_collect::state::{StateDoc, BUDGET_BACKFILL, BUDGET_WINDOW, DAY_MS};
use rusqlite::Connection;

/// 两轮 human+agent 的合法 v1。最大 `created_at_ms` = `V1_MAX_MS`。
const V1: &str = r#"{"format_version":1,"sessions":[{"session_key":"mcode:s1","device_id":"dev-mcode","source_agent":"mcode","turns":[{"role":"human","body":"H","created_at_ms":1700000000000},{"role":"agent","body":"A","created_at_ms":1700000001000}]}]}"#;

/// v1 里实际收到的最大时间戳：回填按它推进水位（不是库内 max）。
const V1_MAX_MS: i64 = 1_700_000_001_000;

/// 迷你库里的唯一一行消息时间。
const DB_MS: i64 = 1_700_000_000_000;

const QUIET_MS: i64 = 600_000;
const CURSOR_SQL: &str = "select max(created_at_ms) from events";

fn make_db(dir: &Path, name: &str, rows: &[i64]) -> PathBuf {
    let p = dir.join(format!("{name}.sqlite"));
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

/// 假转换器：`{out}/v1.json` = 固定 v1；`marks/since-<since_ms>.txt` 留一个水位标记。
fn fake_converter(fixture: &Path, marks: &Path, dir: &Path) -> Vec<String> {
    let fix = fixture.to_string_lossy().to_string();
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

fn source(db: &Path, name: &str, converter: Vec<String>) -> Source {
    Source {
        name: name.into(),
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

fn config(sources: Vec<Source>) -> Config {
    config_with_backfill_budget(sources, 20_000)
}

/// 造一份配置，`max_backfill_turns_per_day` 可调（24 小时回填预算）。
fn config_with_backfill_budget(sources: Vec<Source>, backfill_budget: u32) -> Config {
    Config {
        limits: Limits {
            quiet_ms: QUIET_MS,
            max_files_per_run: 3,
            max_turns_per_day: 300,
            max_backfill_turns_per_day: backfill_budget,
        },
        sources,
    }
}

#[derive(Default)]
struct FakeSink {
    calls: usize,
}

impl Sink for FakeSink {
    fn submit(&mut self, _v1: &Path) -> Result<Receipt, String> {
        self.calls += 1;
        Ok(Receipt {
            accepted: 1,
            duplicate: 0,
            rejected: 0,
            batch_reject: None,
        })
    }
}

/// 一个测试环境：一个迷你库 + 假转换器 + 状态文件路径。
struct Env {
    _dir: tempfile::TempDir,
    db: PathBuf,
    marks: PathBuf,
    state: PathBuf,
    converter: Vec<String>,
}

fn env(name: &str, rows: &[i64]) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let db = make_db(dir.path(), name, rows);
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
    fn source(&self, name: &str) -> Source {
        source(&self.db, name, self.converter.clone())
    }

    /// 转换器实际拿到的 `since_ms`（按文件名里的标记读）。
    fn since_marks(&self) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(&self.marks)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        out.sort();
        out
    }

    fn state_doc(&self) -> StateDoc {
        StateDoc::load(&self.state).unwrap()
    }
}

/// 回填轮消耗 `backfill` 额度，且**不动** `window`。
#[test]
fn backfill_spends_backfill_budget_and_leaves_window_alone() {
    let e = env("bf", &[DB_MS]);
    let cfg = config(vec![e.source("mcode")]);
    let mut sink = FakeSink::default();
    let opts = RunOpts {
        backfill: true,
        ..Default::default()
    };

    let reports = run_once(&cfg, &e.state, &mut sink, DB_MS + QUIET_MS + 1_000, &opts).unwrap();
    assert_eq!(reports[0].reason, "backfill");
    assert_eq!(reports[0].budget, "backfill");
    assert_eq!(reports[0].new_turns, 2);
    assert_eq!(sink.calls, 1);

    let st = e.state_doc();
    assert_eq!(st.daily[BUDGET_BACKFILL].used, 2, "回填轮吃回填额度");
    assert_eq!(
        st.daily[BUDGET_WINDOW].used, 0,
        "回填轮不许动日常额度"
    );
}

/// 日常轮消耗 `window`，且**不动** `backfill`。
#[test]
fn daily_spends_window_budget_and_leaves_backfill_alone() {
    let e = env("daily", &[DB_MS]);
    let cfg = config(vec![e.source("mcode")]);
    let mut st = StateDoc::default();
    // 水位只落后库内最早消息一小时 ⇒ 日常增量。
    st.sqlite.insert(
        "mcode".into(),
        rc_collect::state::SqliteCursor {
            last_created_at_ms: DB_MS - 3_600_000,
            last_msg_id: None,
        },
    );
    st.save(&e.state).unwrap();

    let mut sink = FakeSink::default();
    let reports = run_once(
        &cfg,
        &e.state,
        &mut sink,
        DB_MS + QUIET_MS + 1_000,
        &RunOpts::default(),
    )
    .unwrap();
    assert_eq!(reports[0].reason, "ok");
    assert_eq!(reports[0].budget, "window");
    assert_eq!(sink.calls, 1);

    let st = e.state_doc();
    assert_eq!(st.daily[BUDGET_WINDOW].used, 2, "日常轮吃日常额度");
    assert_eq!(
        st.daily.get(BUDGET_BACKFILL).map(|d| d.used).unwrap_or(0),
        0,
        "日常轮不许动回填额度"
    );
}

/// 两条预算各自滚动 24 小时：过期归零，互不影响。
#[test]
fn both_budgets_expire_on_their_own_24h_window() {
    let e = env("exp", &[DB_MS]);
    let cfg = config(vec![e.source("mcode")]);
    let now: i64 = 1_800_000_000_000;
    // 24 小时回填预算的缺省值（配置 `[limits] max_backfill_turns_per_day`）。
    let backfill_cap = cfg.limits.max_backfill_turns_per_day;

    // window 过期、backfill 还有效 ⇒ 只归零 window。
    let mut st = StateDoc::default();
    st.daily.insert(
        BUDGET_WINDOW.into(),
        rc_collect::state::DailyUse {
            cap: 300,
            used: 300,
            start_ms: now - DAY_MS - 1,
        },
    );
    st.daily.insert(
        BUDGET_BACKFILL.into(),
        rc_collect::state::DailyUse {
            cap: backfill_cap,
            used: 5,
            start_ms: now - 1_000,
        },
    );
    st.save(&e.state).unwrap();

    let mut sink = FakeSink::default();
    run_once(&cfg, &e.state, &mut sink, now, &RunOpts::default()).unwrap();
    let st = e.state_doc();
    assert_eq!(st.daily[BUDGET_WINDOW].used, 0, "过期归零");
    assert_eq!(st.daily[BUDGET_WINDOW].start_ms, now);
    assert_eq!(
        st.daily[BUDGET_BACKFILL].used, 5,
        "另一条预算不该被连带归零"
    );

    // backfill 过期 ⇒ 只归零 backfill。
    let mut st = StateDoc::default();
    st.daily.insert(
        BUDGET_WINDOW.into(),
        rc_collect::state::DailyUse {
            cap: 300,
            used: 7,
            start_ms: now - 1_000,
        },
    );
    st.daily.insert(
        BUDGET_BACKFILL.into(),
        rc_collect::state::DailyUse {
            cap: backfill_cap,
            used: backfill_cap,
            start_ms: now - DAY_MS - 1,
        },
    );
    st.save(&e.state).unwrap();

    let mut sink = FakeSink::default();
    run_once(&cfg, &e.state, &mut sink, now, &RunOpts::default()).unwrap();
    let st = e.state_doc();
    assert_eq!(st.daily[BUDGET_BACKFILL].used, 0);
    assert_eq!(st.daily[BUDGET_WINDOW].used, 7, "日常额度不该被连带归零");
}

/// sqlite 新源：不带 `--backfill` ⇒ `seeded`、零导入；带 `--backfill` ⇒ 有导入、
/// 水位 = 收到的最大时间戳（不是库内 max）。
#[test]
fn new_sqlite_source_seeds_without_backfill_and_imports_with_it() {
    let e = env("new", &[DB_MS]);
    let cfg = config(vec![e.source("mcode")]);
    let mut sink = FakeSink::default();

    // 不带 --backfill：只收未来。
    let seeded = run_once(
        &cfg,
        &e.state,
        &mut sink,
        DB_MS + QUIET_MS + 1_000,
        &RunOpts::default(),
    )
    .unwrap();
    assert_eq!(seeded[0].reason, "seeded");
    assert!(!seeded[0].advanced);
    assert_eq!(sink.calls, 0, "seeded 不许导入");
    assert_eq!(e.state_doc().sqlite["mcode"].last_created_at_ms, DB_MS);

    // 带 --backfill：从 0 全量。
    let fresh = env("new2", &[DB_MS]);
    let cfg2 = config(vec![fresh.source("mcode")]);
    let mut sink2 = FakeSink::default();
    let opts = RunOpts {
        backfill: true,
        ..Default::default()
    };
    let bf = run_once(
        &cfg2,
        &fresh.state,
        &mut sink2,
        DB_MS + QUIET_MS + 1_000,
        &opts,
    )
    .unwrap();
    assert_eq!(bf[0].reason, "backfill");
    assert!(bf[0].advanced);
    assert_eq!(bf[0].new_turns, 2);
    assert_eq!(sink2.calls, 1);
    assert_eq!(
        fresh.since_marks(),
        vec!["since-0.txt".to_string()],
        "回填从 0 起"
    );
    assert_eq!(
        fresh.state_doc().sqlite["mcode"].last_created_at_ms,
        V1_MAX_MS,
        "水位 = 本轮实际收到的最大时间戳"
    );
}

/// 有水位时：落后**当前数据**（库内 max）超过 24h ⇒ 走回填预算；落后不到 24h ⇒ 走日常额度。
///
/// 判据只看「水位落后当前多远」：库里多早以前的数据不在判据里。
#[test]
fn lagging_watermark_picks_budget_by_the_24h_rule() {
    // 落后三天 ⇒ 历史 ⇒ 回填预算。
    let e = env("lag", &[DB_MS]);
    let cfg = config(vec![e.source("mcode")]);
    let mut st = StateDoc::default();
    st.sqlite.insert(
        "mcode".into(),
        rc_collect::state::SqliteCursor {
            last_created_at_ms: DB_MS - 3 * DAY_MS,
            last_msg_id: None,
        },
    );
    st.save(&e.state).unwrap();
    let mut sink = FakeSink::default();
    let reports = run_once(
        &cfg,
        &e.state,
        &mut sink,
        DB_MS + QUIET_MS + 1_000,
        &RunOpts::default(),
    )
    .unwrap();
    assert_eq!(reports[0].budget, "backfill", "落后三天算历史");
    assert_eq!(reports[0].reason, "backfill");
    assert_eq!(
        e.state_doc().daily[BUDGET_BACKFILL].used, 2,
        "历史轮吃回填额度"
    );

    // 落后一小时 ⇒ 日常 ⇒ 日常额度。
    let e2 = env("lag2", &[DB_MS]);
    let cfg2 = config(vec![e2.source("mcode")]);
    let mut st = StateDoc::default();
    st.sqlite.insert(
        "mcode".into(),
        rc_collect::state::SqliteCursor {
            last_created_at_ms: DB_MS - 3_600_000,
            last_msg_id: None,
        },
    );
    st.save(&e2.state).unwrap();
    let mut sink2 = FakeSink::default();
    let reports2 = run_once(
        &cfg2,
        &e2.state,
        &mut sink2,
        DB_MS + QUIET_MS + 1_000,
        &RunOpts::default(),
    )
    .unwrap();
    assert_eq!(reports2[0].budget, "window", "落后一小时算日常");
    assert_eq!(e2.state_doc().daily[BUDGET_WINDOW].used, 2);
}

/// 库里同时有很老的数据和当前数据时，判据只看**当前数据**：水位落后当前一天以上
/// ⇒ 回填预算（哪怕库里的老数据比水位还老）。
#[test]
fn judgement_uses_the_current_max_not_the_oldest_row() {
    // 库里两行：三天前与现在。水位落在两天前。
    let e = env("mix", &[DB_MS - 3 * DAY_MS, DB_MS]);
    let cfg = config(vec![e.source("mcode")]);
    let mut st = StateDoc::default();
    st.sqlite.insert(
        "mcode".into(),
        rc_collect::state::SqliteCursor {
            last_created_at_ms: DB_MS - 2 * DAY_MS,
            last_msg_id: None,
        },
    );
    st.save(&e.state).unwrap();
    let mut sink = FakeSink::default();
    let reports = run_once(
        &cfg,
        &e.state,
        &mut sink,
        DB_MS + QUIET_MS + 1_000,
        &RunOpts::default(),
    )
    .unwrap();
    assert_eq!(
        reports[0].budget, "backfill",
        "落后当前数据两天 ⇒ 历史缺口"
    );
    assert_eq!(
        e.state_doc().daily[BUDGET_WINDOW].used, 0,
        "历史轮不吃日常额度"
    );
}

/// 水位与当前数据同步 ⇒ 日常增量（不判成历史）。
#[test]
fn watermark_in_sync_with_max_is_a_daily_run() {
    // 落后一分钟 ⇒ 日常轮。
    let e = env("sync", &[DB_MS]);
    let cfg = config(vec![e.source("mcode")]);
    let mut st = StateDoc::default();
    st.sqlite.insert(
        "mcode".into(),
        rc_collect::state::SqliteCursor {
            last_created_at_ms: DB_MS - 60_000,
            last_msg_id: None,
        },
    );
    st.save(&e.state).unwrap();
    let mut sink = FakeSink::default();
    let reports = run_once(
        &cfg,
        &e.state,
        &mut sink,
        DB_MS + QUIET_MS + 1_000,
        &RunOpts::default(),
    )
    .unwrap();
    assert_eq!(reports[0].budget, "window", "与当前同步 ⇒ 日常");
    assert_eq!(reports[0].reason, "ok");
    assert_eq!(e.state_doc().daily[BUDGET_WINDOW].used, 2);
    assert_eq!(
        e.state_doc().daily.get(BUDGET_BACKFILL).map(|d| d.used).unwrap_or(0),
        0,
        "日常轮不吃回填额度"
    );

    // 完全同步（水位 = 库内 max）⇒ 没有新数据 ⇒ idle，仍是日常额度，不判历史。
    let e2 = env("sync2", &[DB_MS]);
    let cfg2 = config(vec![e2.source("mcode")]);
    let mut st = StateDoc::default();
    st.sqlite.insert(
        "mcode".into(),
        rc_collect::state::SqliteCursor {
            last_created_at_ms: DB_MS,
            last_msg_id: None,
        },
    );
    st.save(&e2.state).unwrap();
    let mut sink2 = FakeSink::default();
    let reports2 = run_once(
        &cfg2,
        &e2.state,
        &mut sink2,
        DB_MS + QUIET_MS + 1_000,
        &RunOpts::default(),
    )
    .unwrap();
    assert_eq!(reports2[0].budget, "window");
    assert_eq!(reports2[0].reason, "idle");
    assert_eq!(sink2.calls, 0);
}

/// 回填触顶 `max_backfill_turns`（**单轮**上限）⇒ 本轮停止、剩余顺延（第二个源一个轮都没进）。
#[test]
fn backfill_cap_stops_the_round_and_defers_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let db_a = make_db(dir.path(), "a", &[DB_MS]);
    let db_b = make_db(dir.path(), "b", &[DB_MS]);
    let marks = dir.path().join("marks");
    std::fs::create_dir_all(&marks).unwrap();
    let fixture = dir.path().join("fixture.json");
    std::fs::write(&fixture, format!("{V1}\n")).unwrap();
    let converter = fake_converter(&fixture, &marks, dir.path());
    let state = dir.path().join("collect-state.json");

    let cfg = config(vec![
        source(&db_a, "a", converter.clone()),
        source(&db_b, "b", converter),
    ]);
    let mut sink = FakeSink::default();
    let opts = RunOpts {
        backfill: true,
        max_backfill_turns: 2,
        ..Default::default()
    };

    let reports = run_once(&cfg, &state, &mut sink, DB_MS + QUIET_MS + 1_000, &opts).unwrap();
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0].reason, "backfill");
    assert_eq!(reports[1].reason, "backfill_capped");
    assert!(!reports[1].advanced);
    assert_eq!(reports[1].new_turns, 0);
    assert_eq!(sink.calls, 1, "触顶后不许再导入");
}

/// 24 小时回填预算（配置 `max_backfill_turns_per_day`）用尽 ⇒ 拒绝回填、剩余等下一个窗口。
///
/// 它与**单轮**上限是两只旋钮：这里单轮上限是默认的 2000（远没到），
/// 挡住它的是 24 小时预算。
#[test]
fn daily_backfill_budget_blocks_when_it_is_exhausted() {
    let dir = tempfile::tempdir().unwrap();
    let db_a = make_db(dir.path(), "a", &[DB_MS]);
    let db_b = make_db(dir.path(), "b", &[DB_MS]);
    let marks = dir.path().join("marks");
    std::fs::create_dir_all(&marks).unwrap();
    let fixture = dir.path().join("fixture.json");
    std::fs::write(&fixture, format!("{V1}\n")).unwrap();
    let converter = fake_converter(&fixture, &marks, dir.path());
    let state = dir.path().join("collect-state.json");

    // 24 小时预算只够一个源的两轮。
    let cfg = config_with_backfill_budget(
        vec![
            source(&db_a, "a", converter.clone()),
            source(&db_b, "b", converter),
        ],
        2,
    );
    let mut sink = FakeSink::default();
    let opts = RunOpts {
        backfill: true,
        ..Default::default()
    };

    let reports = run_once(&cfg, &state, &mut sink, DB_MS + QUIET_MS + 1_000, &opts).unwrap();
    assert_eq!(reports[0].reason, "backfill");
    assert_eq!(
        reports[1].reason, "backfill_budget_capped",
        "24 小时预算用尽 ⇒ 拒绝回填"
    );
    assert!(!reports[1].advanced);
    assert_eq!(sink.calls, 1, "预算用尽后不许再导入");
    let st = StateDoc::load(&state).unwrap();
    assert_eq!(st.daily[BUDGET_BACKFILL].used, 2);
}

/// 积压超阈值 ⇒ 拒绝回填并报出积压数，且一个轮都没导入。
#[test]
fn pending_backpressure_blocks_backfill_and_reports_the_backlog() {
    let e = env("pend", &[DB_MS]);
    let cfg = config(vec![e.source("mcode")]);
    let mut sink = FakeSink::default();
    let opts = RunOpts {
        backfill: true,
        max_backfill_pending: Some(100),
        pending_turns: Some(500),
        ..Default::default()
    };

    let reports = run_once(
        &cfg,
        &e.state,
        &mut sink,
        DB_MS + QUIET_MS + 1_000,
        &opts,
    )
    .unwrap();
    assert_eq!(reports[0].reason, "backfill_blocked_pending");
    assert_eq!(reports[0].pending_turns, Some(500), "回执里要带当前积压数");
    assert!(!reports[0].advanced);
    assert_eq!(sink.calls, 0, "拒绝回填 ⇒ 一个轮都不许进");
    assert!(
        !e.state_doc().sqlite.contains_key("mcode"),
        "被拒时不许留下水位"
    );

    // 读不到积压 ⇒ 按通过处理（不因为读不到就拒绝采集）。
    let e2 = env("pend2", &[DB_MS]);
    let cfg2 = config(vec![e2.source("mcode")]);
    let mut sink2 = FakeSink::default();
    let opts2 = RunOpts {
        backfill: true,
        max_backfill_pending: Some(100),
        pending_turns: None,
        ..Default::default()
    };
    let reports2 = run_once(
        &cfg2,
        &e2.state,
        &mut sink2,
        DB_MS + QUIET_MS + 1_000,
        &opts2,
    )
    .unwrap();
    assert_eq!(reports2[0].reason, "backfill");
    assert_eq!(sink2.calls, 1);
}

/// 回填失败（sink 报错）⇒ 水位停在失败点之前，下一轮继续。
#[test]
fn failed_backfill_keeps_the_watermark_and_does_not_spend_the_budget() {
    let e = env("fail", &[DB_MS]);
    let cfg = config(vec![e.source("mcode")]);
    let mut bad = FailingSink;
    let opts = RunOpts {
        backfill: true,
        ..Default::default()
    };
    let reports = run_once(
        &cfg,
        &e.state,
        &mut bad,
        DB_MS + QUIET_MS + 1_000,
        &opts,
    )
    .unwrap();
    assert!(reports[0].reason.contains("sink_failed"), "{}", reports[0].reason);
    assert!(!reports[0].advanced);
    let st = e.state_doc();
    assert!(
        !st.sqlite.contains_key("mcode"),
        "失败不推进水位"
    );
    assert_eq!(
        st.daily.get(BUDGET_BACKFILL).map(|d| d.used).unwrap_or(0),
        0,
        "失败不消耗额度"
    );
}

struct FailingSink;

impl Sink for FailingSink {
    fn submit(&mut self, _v1: &Path) -> Result<Receipt, String> {
        Err("import_failed".into())
    }
}
