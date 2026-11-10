//! Task 5：`recollect-hub collect` 的黑盒面。
//!
//! 这一面是采集器的本机入口（设计 §6/§7）：`--json` 一对象一源一行读数，
//! `--status` 只读水位，`--dry-run` 只扫描不提交。三条用例钉的是"零动作"与
//! "零数据"——采集无新数据时不许有任何写副作用。

use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

const QUIET_LONG_MS: u64 = 600_000;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin()).args(args).output().expect("hub binary")
}

/// 写一份单源 `collect.toml`。路径按正斜杠写入：TOML 基本串里 `\U` 是转义，
/// Windows 路径不做这次归一化会把配置本身写坏。
fn write_cfg(path: &Path, src_root: &Path, quiet_ms: u64) {
    let root = src_root.to_string_lossy().replace('\\', "/");
    let body = format!(
        r#"
[limits]
quiet_ms = {quiet_ms}
[[source]]
name = "qoder"
kind = "files"
root = "{root}"
glob = "*.jsonl"
converter = ["true"]
device_id = "dev-1"
source_agent = "qoder"
"#
    );
    std::fs::write(path, body).unwrap();
}

fn stdout_json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).expect("stdout must be one JSON object")
}

fn stderr_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// 转录层里的会话数：dry-run 前后必须相等。
fn sessions_count(data_dir: &Path) -> i64 {
    let root = rc_store::Root::open(data_dir).unwrap();
    root.transcript()
        .conn()
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap()
}

/// 一条能被 ingest 收下的 v1（hub 侧要走真契约，不许拿假字节糊弄）。
const V1: &str = r#"{"format_version":1,"sessions":[{"session_key":"ses-BF","device_id":"dev-1","source_agent":"mcode","turns":[{"role":"human","body":"BACKFILL-1","created_at_ms":1700000000000}]}]}"#;

/// 造一个只含一条消息的迷你 sqlite 库（hub 侧也有 rusqlite 依赖）。
fn make_db(path: &Path) {
    let con = rusqlite::Connection::open(path).unwrap();
    con.execute_batch(
        "create table events (id integer primary key autoincrement, created_at_ms integer not null);",
    )
    .unwrap();
    con.execute("insert into events (created_at_ms) values (?1)", [1_700_000_000_000i64])
        .unwrap();
}

/// 写一份单源 sqlite `collect.toml` + 假转换器（把固定 v1 拷到 `{out}/v1.json`）。
fn write_sqlite_cfg(path: &Path, db: &Path, dir: &Path) {
    let db_s = db.to_string_lossy().replace('\\', "/");
    let fixture = dir.join("fixture.json");
    std::fs::write(&fixture, format!("{V1}\n")).unwrap();

    // 转换器的 argv 走 TOML 字面串（单引号）：路径里的反斜杠原样留给 `cmd`。
    // 换正斜杠会让 `cmd copy` 在跨盘工作目录下解析错源文件（实测 "cannot find the file"）。
    let converter = {
        #[cfg(windows)]
        {
            let bat = dir.join("fake-converter.cmd");
            std::fs::write(
                &bat,
                "@echo off\r\ncopy /Y \"%~1\" \"%~2\\v1.json\" >nul\r\n",
            )
            .unwrap();
            format!(
                "converter = ['cmd', '/C', '{}', '{}', '{{out}}']",
                bat.to_string_lossy(),
                fixture.to_string_lossy()
            )
        }
        #[cfg(not(windows))]
        {
            format!(
                "converter = ['sh', '-c', 'cp \"$1\" \"$2/v1.json\"', 'x', '{}', '{{out}}']",
                fixture.to_string_lossy()
            )
        }
    };

    let body = format!(
        r#"
[limits]
quiet_ms = 1
[[source]]
name = "mcode"
kind = "sqlite"
db = "{db_s}"
cursor_sql = "select max(created_at_ms) from events"
{converter}
device_id = "dev-1"
source_agent = "mcode"
"#
    );
    std::fs::write(path, body).unwrap();
}

/// `--backfill` 旗标透传：新 sqlite 源带旗标发生导入，不带旗标只播种。
#[test]
fn collect_cli_backfill_flag_controls_a_new_sqlite_source() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("runtime-state.sqlite");
    make_db(&db);
    let cfg = d.path().join("collect.toml");
    write_sqlite_cfg(&cfg, &db, d.path());

    // 不带 --backfill：只收未来（seeded），零导入。
    let data_a = d.path().join("data-a");
    let out = run(&[
        "collect",
        "--config",
        cfg.to_str().unwrap(),
        "--data-dir",
        data_a.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v["sources"][0]["reason"], "seeded");
    assert_eq!(v["sources"][0]["advanced"], false);
    assert_eq!(sessions_count(&data_a), 0, "seeded 不许导入");

    // 带 --backfill：从 0 全量，发生导入。
    let data_b = d.path().join("data-b");
    let out = run(&[
        "collect",
        "--config",
        cfg.to_str().unwrap(),
        "--data-dir",
        data_b.to_str().unwrap(),
        "--backfill",
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v["sources"][0]["reason"], "backfill");
    assert_eq!(v["sources"][0]["budget"], "backfill");
    assert_eq!(v["sources"][0]["advanced"], true);
    assert_eq!(sessions_count(&data_b), 1, "--backfill 必须真的导入");
}

#[test]
fn collect_cli_idle_reports_zero_without_side_effects() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let cfg = d.path().join("collect.toml");
    write_cfg(&cfg, &src, QUIET_LONG_MS);
    let data = d.path().join("data");

    let out = run(&[
        "collect",
        "--config",
        cfg.to_str().unwrap(),
        "--data-dir",
        data.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["sources"][0]["source"], "qoder");
    assert_eq!(v["sources"][0]["due"], 0);
    assert_eq!(v["sources"][0]["reason"], "idle");
    assert_eq!(v["sources"][0]["advanced"], false);
    assert!(v["state_path"]
        .as_str()
        .unwrap()
        .ends_with("collect-state.json"));
}

#[test]
fn collect_status_on_empty_state_is_zeroed_json() {
    let d = tempfile::tempdir().unwrap();
    let data = d.path().join("data");

    let out = run(&[
        "collect",
        "--status",
        "--json",
        "--data-dir",
        data.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v["files"], 0);
    assert_eq!(v["sqlite"], 0);
    assert_eq!(v["poisoned"], 0);
    assert!(v["state_path"]
        .as_str()
        .unwrap()
        .ends_with("collect-state.json"));
}

#[test]
fn collect_dry_run_counts_due_without_writing_data() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("a.jsonl"),
        "{\"format_version\":1,\"sessions\":[]}\n",
    )
    .unwrap();
    // quiet_ms = 1：先让文件落地，再等两毫秒，进程里的"现在"就必然跨过静默窗。
    std::thread::sleep(Duration::from_millis(20));
    let cfg = d.path().join("collect.toml");
    write_cfg(&cfg, &src, 1);
    let data = d.path().join("data");
    let before = sessions_count(&data);

    let out = run(&[
        "collect",
        "--config",
        cfg.to_str().unwrap(),
        "--data-dir",
        data.to_str().unwrap(),
        "--dry-run",
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v["sources"][0]["due"], 1);
    assert_eq!(v["sources"][0]["reason"], "dry_run");
    assert_eq!(v["sources"][0]["new_turns"], 0);
    assert_eq!(sessions_count(&data), before);
}

/// 水位落后「当前数据」超过 24h ⇒ 自动走回填额度（不需要任何旗标）。
#[test]
fn collect_cli_lagging_watermark_uses_the_backfill_budget() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("runtime-state.sqlite");
    make_db(&db);
    let cfg = d.path().join("collect.toml");
    write_sqlite_cfg(&cfg, &db, d.path());

    let data = d.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    // 水位落后库内当前值三天 ⇒ 历史缺口。
    let state = data.join("collect-state.json");
    std::fs::write(
        &state,
        r#"{"sqlite":{"mcode":{"last_created_at_ms":1699740800000,"last_msg_id":null}}}"#,
    )
    .unwrap();

    let out = run(&[
        "collect",
        "--config",
        cfg.to_str().unwrap(),
        "--data-dir",
        data.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v["sources"][0]["budget"], "backfill");
    assert_eq!(v["sources"][0]["reason"], "backfill");
    assert_eq!(sessions_count(&data), 1, "历史缺口要真的导入");
}

/// `[limits] max_backfill_turns_per_day` 是合法配置项；`min_cursor_sql` 已被删除，
/// 再写它 ⇒ 配置错误（退出码 2）。
#[test]
fn collect_cli_backfill_budget_limit_is_configured_and_min_cursor_sql_is_rejected() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("runtime-state.sqlite");
    make_db(&db);
    let cfg = d.path().join("collect.toml");
    write_sqlite_cfg(&cfg, &db, d.path());
    let body = std::fs::read_to_string(&cfg).unwrap();
    let with_limit = body.replace("[limits]\nquiet_ms = 1", "[limits]\nquiet_ms = 1\nmax_backfill_turns_per_day = 500");
    assert_ne!(with_limit, body, "配置模板要能定位 [limits] 段");

    let data = d.path().join("data");
    std::fs::write(&cfg, &with_limit).unwrap();
    let out = run(&[
        "collect",
        "--config",
        cfg.to_str().unwrap(),
        "--data-dir",
        data.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr_text(&out));

    // min_cursor_sql 已经不存在：再写它 ⇒ 未知字段 ⇒ 退出码 2。
    let with_min = body.replace(
        "cursor_sql = \"select max(created_at_ms) from events\"",
        "cursor_sql = \"select max(created_at_ms) from events\"\nmin_cursor_sql = \"select min(created_at_ms) from events\"",
    );
    std::fs::write(&cfg, &with_min).unwrap();
    let out = run(&[
        "collect",
        "--config",
        cfg.to_str().unwrap(),
        "--data-dir",
        d.path().join("data2").to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr_text(&out));
}
