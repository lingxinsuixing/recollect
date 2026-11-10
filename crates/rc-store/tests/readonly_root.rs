//! P1-5：诊断面的只读装配（`Root::open_readonly`）。
//!
//! 这一层断四件事：
//! 1. 三个句柄都写不动（`CREATE TABLE` 直接被 SQLite 拒绝）；
//! 2. 打开前后 `sqlite_master` 的表数不变——先删一张可选表，打开后它仍然缺（迁移不跑）；
//! 3. 库缺失 ⇒ `Err`（诊断路径不建库、不建目录）；
//! 4. schema 不是当前版本 ⇒ `Err`（诊断路径不迁移；版本原地不动）。

use rc_store::ddl::{LEDGER_VERSION, MEMORY_VERSION, TRANSCRIPT_VERSION};
use rc_store::{Root, StoreError};

fn table_count(path: &std::path::Path) -> i64 {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

fn has_table(path: &std::path::Path, name: &str) -> bool {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [name],
            |r| r.get(0),
        )
        .unwrap();
    n == 1
}

#[test]
fn open_readonly_handles_refuse_every_write() {
    let dir = tempfile::tempdir().unwrap();
    // 先造一个当前版本的库（可写入口是唯一能建库的地方）。
    Root::open(dir.path()).unwrap();
    let ro = Root::open_readonly(dir.path()).unwrap();
    assert!(ro.memory().is_read_only(), "记忆柄必须是只读形态");
    for (name, conn) in [
        ("transcript", ro.transcript().conn()),
        ("ledger", ro.ledger().conn()),
        ("memory", ro.memory().conn()),
    ] {
        let e = conn
            .execute_batch("CREATE TABLE probe_write(x INTEGER)")
            .expect_err(&format!("{name} 的只读连接居然建出了表"));
        let msg = e.to_string().to_lowercase();
        assert!(
            msg.contains("readonly") || msg.contains("read-only"),
            "{name} 的拒绝理由不是只读：{e}"
        );
    }
    // DML 同样拒绝（不只是 DDL）：拿记忆库一张已知的表试一行插入。
    let e = ro
        .memory()
        .conn()
        .execute(
            "INSERT INTO attention(kind, mem_table, mem_id, created_at_ms)
             VALUES('quote_conflict','fact','ro-probe',1)",
            [],
        )
        .expect_err("只读连接居然写出了行");
    assert!(
        e.to_string().to_lowercase().contains("readonly"),
        "DML 的拒绝理由不是只读：{e}"
    );
}

#[test]
fn open_readonly_does_not_recreate_a_dropped_table() {
    let dir = tempfile::tempdir().unwrap();
    {
        let root = Root::open(dir.path()).unwrap();
        // `card_verdict_event` 是可选的读取侧表：删掉它不影响 `user_version`。
        // 审计里 doctor 的旧实现正是把这张表新建出来的，所以挑它当证人。
        root.memory()
            .conn()
            .execute_batch("DROP TABLE card_verdict_event")
            .unwrap();
    }
    let path = dir.path().join("memory.db");
    let before = table_count(&path);
    assert!(!has_table(&path, "card_verdict_event"), "夹具没删掉：表还在");

    let ro = Root::open_readonly(dir.path()).unwrap();
    // 读一条（哪怕只是一句 PRAGMA）也不许触发建表。
    let _: i64 = ro
        .memory()
        .conn()
        .query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))
        .unwrap();

    assert_eq!(table_count(&path), before, "只读打开改了表数");
    assert!(
        !has_table(&path, "card_verdict_event"),
        "只读打开把删掉的表建回来了（迁移跑了）"
    );
}

#[test]
fn open_readonly_on_a_missing_store_fails_instead_of_creating_one() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    assert!(Root::open_readonly(&data).is_err(), "缺库必须失败");
    assert!(!data.exists(), "只读装配不许建目录");
}

#[test]
fn open_readonly_refuses_a_stale_schema_without_migrating() {
    let dir = tempfile::tempdir().unwrap();
    Root::open(dir.path()).unwrap();
    // 把会话库版本退一格：可写入口会自动迁移（`apply` 幂等），只读入口必须报 `SchemaStale`。
    let conn = rusqlite::Connection::open(dir.path().join("transcript.db")).unwrap();
    conn.pragma_update(None, "user_version", TRANSCRIPT_VERSION - 1).unwrap();
    drop(conn);

    match Root::open_readonly(dir.path()) {
        Err(StoreError::Sql(msg)) => {
            assert!(msg.contains("transcript.db"), "错误没有点名是哪个库：{msg}");
            assert!(msg.contains("只读装配不做迁移"), "错误没说清修法：{msg}");
        }
        Ok(_) => panic!("旧 schema 的库被只读装配放过了"),
        Err(e) => panic!("期望版本不符的 Sql 错误，实际：{e}"),
    }
    // 失败路径也不许顺手迁移：版本原样留在原地。
    let conn = rusqlite::Connection::open(dir.path().join("transcript.db")).unwrap();
    let v: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, TRANSCRIPT_VERSION - 1, "只读装配把 schema 迁了");
}

/// 版本常量与门是同一份真相：三个都核一遍，避免"只核记忆库"的实现混过去。
#[test]
fn open_readonly_checks_all_three_schema_versions() {
    let dir = tempfile::tempdir().unwrap();
    Root::open(dir.path()).unwrap();
    assert!(Root::open_readonly(dir.path()).is_ok());

    for (file, supported) in [
        ("ledger.db", LEDGER_VERSION),
        ("memory.db", MEMORY_VERSION),
    ] {
        let path = dir.path().join(file);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", supported + 1).unwrap();
        drop(conn);
        match Root::open_readonly(dir.path()) {
            Err(StoreError::SchemaTooNew { db, .. }) => assert_eq!(db, file),
            Ok(_) => panic!("{file} 版本过高的库被只读装配放过了"),
            Err(e) => panic!("{file} 版本过高必须是 SchemaTooNew，实际：{e}"),
        }
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", supported).unwrap();
    }
}
