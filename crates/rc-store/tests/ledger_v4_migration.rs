//! E2：v3 的 job_watermark 只有 cursor_created_at_ms；升级后列改名，distill 归零、derive 原样。
//!
//! **与工单 Step 8 测试原文的唯一差异**：工单用 `rc_ledger::water::read` 读游标，但
//! `crates/rc-store/Cargo.toml` 的 dev-dependencies 里没有 `rc-ledger`（本段边界明令不许碰
//! `Cargo.toml`/`Cargo.lock`）⇒ 这里改用同一把句柄上的 SQL 读**同一格库状态**：
//! 断言的列值与 `water::read` 返回的 `at_ms` / `turn_uid` 逐字同源。
use rc_store::{ddl, Root};

fn make_v3_ledger(path: &std::path::Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(r#"
        PRAGMA user_version=3;
        CREATE TABLE job_watermark(
            family TEXT NOT NULL CHECK(family IN ('distill','derive','anomaly_scan')),
            scope_key TEXT NOT NULL,
            cursor_created_at_ms INTEGER NOT NULL DEFAULT 0,
            cursor_turn_uid TEXT NOT NULL DEFAULT '',
            attempts INTEGER NOT NULL DEFAULT 0,
            last_success_at_ms INTEGER,
            halted_at_ms INTEGER,
            PRIMARY KEY(family, scope_key));
        INSERT INTO job_watermark(family, scope_key, cursor_created_at_ms, cursor_turn_uid)
        VALUES('distill','dev-origin/ses-late',1234,'u'), ('derive','',5678,'m');
    "#).unwrap();
}

/// `water::read` 那一格在库里的形状：游标 ms + 定序 uid。
fn cursor(root: &Root, family: &str, scope: &str) -> (i64, String) {
    root.ledger().conn()
        .query_row("SELECT cursor_at_ms, cursor_turn_uid FROM job_watermark
                    WHERE family=?1 AND scope_key=?2",
                   rusqlite::params![family, scope],
                   |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
}

#[test]
fn v3_升级到_v4_只归零_distill_并保留_derive() {
    let dir = tempfile::tempdir().unwrap();
    make_v3_ledger(&dir.path().join("ledger.db"));
    let root = Root::open(dir.path()).unwrap();
    // 版本号按常量读：本用例钉的是"迁移把版本落盘"，阶段 4 T1 把账本抬到 v5 时
    // 字面量 4 会变成一条与常量脱钩的幻影（v5 的逐字读数由 ledger_v5_migration 钉）。
    assert_eq!(ddl::schema_version(root.ledger().conn()).unwrap(), ddl::LEDGER_VERSION);
    // 挡"ADD COLUMN 而不是 RENAME"的变异：新列必须在、旧列必须已经不在。
    let cols: Vec<String> = root.ledger().conn()
        .prepare("SELECT name FROM pragma_table_info('job_watermark') ORDER BY cid").unwrap()
        .query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    assert!(cols.contains(&"cursor_at_ms".to_string()), "新列不在：{cols:?}");
    assert!(!cols.contains(&"cursor_created_at_ms".to_string()), "旧列必须已被 RENAME 掉：{cols:?}");
    assert_eq!(cursor(&root, "distill", "dev-origin/ses-late"), (0, String::new()),
               "distill 游标必须归零，迫使重扫");
    assert_eq!(cursor(&root, "derive", ""), (5678, "m".to_string()),
               "derive 的轴没变，不许被重置");
    assert_eq!(root.migration_report().ledger_cursor_reset, 1, "只打印一个 distill scope");
}

/// 迁移提示的文本只有这一处（hub / doctor / mcp 三个入口共用）：
/// 两态逐字钉死，免得某个入口自己改一个字而另一个入口照旧。
#[test]
fn 迁移提示两态_文本逐字() {
    // UFCS 而不是方法调用：`bites 3 '\.ledger_warning\(\)' crates` 数的是三个入口的接线，
    // 这条单测不能再往那个读数里掺一处（否则摘掉一个入口的接线仍然够 3）。
    let none = ddl::MigrationReport::default();
    assert_eq!(ddl::MigrationReport::ledger_warning(&none), None, "没回拨就不许有提示");
    let one = ddl::MigrationReport { ledger_cursor_reset: 1 };
    assert_eq!(
        ddl::MigrationReport::ledger_warning(&one).as_deref(),
        Some("watermarks reset: 1 distill scopes (cursor_created_at -> cursor_at_ms; created_at -> ingested_at)"),
        "文本改了：三个入口的读者会看到两个版本"
    );
    let many = ddl::MigrationReport { ledger_cursor_reset: 7 };
    assert_eq!(
        ddl::MigrationReport::ledger_warning(&many).as_deref(),
        Some("watermarks reset: 7 distill scopes (cursor_created_at -> cursor_at_ms; created_at -> ingested_at)"),
        "文本改了：三个入口的读者会看到两个版本"
    );
}

/// v4 + 旧列的半迁移库（旧实现的崩溃窗口：版本抬了、backfill 没跑）。
/// 迁移必须按**列存在性**自愈，而不是看到 user_version=4 就跳过。
#[test]
fn v4_带旧列的半迁移库会自愈() {
    let dir = tempfile::tempdir().unwrap();
    make_v3_ledger(&dir.path().join("ledger.db"));
    // 把版本抬到 4，列仍是 v3 形状——复刻"apply 先落盘、backfill 没跑"的那一具尸体。
    {
        let conn = rusqlite::Connection::open(dir.path().join("ledger.db")).unwrap();
        conn.pragma_update(None, "user_version", 4).unwrap();
    }
    let root = Root::open(dir.path()).unwrap();
    let cols: Vec<String> = root.ledger().conn()
        .prepare("SELECT name FROM pragma_table_info('job_watermark') ORDER BY cid").unwrap()
        .query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    assert!(cols.contains(&"cursor_at_ms".to_string()), "半迁移库没被改名：{cols:?}");
    assert!(!cols.contains(&"cursor_created_at_ms".to_string()), "旧列必须已被 RENAME 掉：{cols:?}");
    assert_eq!(cursor(&root, "distill", "dev-origin/ses-late"), (0, String::new()),
               "半迁移库的 distill 游标必须归零");
    assert_eq!(cursor(&root, "derive", ""), (5678, "m".to_string()), "derive 不许被重置");
    assert_eq!(root.migration_report().ledger_cursor_reset, 1, "自愈也要报出一个 distill scope");

    // 自愈只发生一次：再开看到旧列已不在 ⇒ 0、不重复归零。
    let root = Root::open(dir.path()).unwrap();
    assert_eq!(root.migration_report().ledger_cursor_reset, 0, "已是新列，第二次不许再归零");
    assert_eq!(cursor(&root, "derive", ""), (5678, "m".to_string()));
}

#[test]
fn 再开一次不重复归零() {
    let dir = tempfile::tempdir().unwrap();
    make_v3_ledger(&dir.path().join("ledger.db"));
    let _ = Root::open(dir.path()).unwrap();
    let root = Root::open(dir.path()).unwrap();
    assert_eq!(root.migration_report().ledger_cursor_reset, 0, "已是 v4，第二次不许再归零");
    assert_eq!(cursor(&root, "derive", ""), (5678, "m".to_string()));
}
