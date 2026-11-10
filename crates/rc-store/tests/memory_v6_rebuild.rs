// crates/rc-store/tests/memory_v6_rebuild.rs
//! canonical 基线**没有迁移**：v≤5 的 `memory.db` 一律拒绝（`SchemaRebuildRequired`），
//! 由操作员删除/归档后重建。可写装配（`Root::open`）与副本装配（`AgentRoot::open_replica`）
//! 走同一道版本门——副本也不能带旧 schema 进来。
//!
//! 本文件的前身是 `memory_v5_migration.rs`（v4 库缺 `event_link` 重开即补）。那条性质随
//! v6 一起作废：旧库的表名与列都对不上 canonical，补表补列只会造出半个库。

use rc_store::{ddl, Root, StoreError};

/// 手工建一个空库并压上指定 `user_version`（不跑任何 DDL）。
fn empty_db_with_version(dir: &std::path::Path, version: i32) {
    let conn = rusqlite::Connection::open(dir.join("memory.db")).unwrap();
    conn.pragma_update(None, "user_version", version).unwrap();
}

fn tables(db: &std::path::Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut st = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
    ).unwrap();
    st.query_map([], |r| r.get::<_, String>(0)).unwrap().filter_map(|r| r.ok()).collect()
}

#[test]
fn v5_db_is_rejected_with_schema_rebuild_required() {
    let dir = tempfile::tempdir().unwrap();
    empty_db_with_version(dir.path(), 5);

    let e = Root::open(dir.path()).map(|_| ()).unwrap_err();
    match e {
        StoreError::SchemaRebuildRequired { db, found, supported } => {
            assert_eq!(db, "memory.db", "报错的库必须是记忆库");
            assert_eq!(found, 5);
            assert_eq!(supported, ddl::MEMORY_VERSION);
            let msg = StoreError::SchemaRebuildRequired { db, found, supported }.to_string();
            assert!(msg.contains("无迁移"), "错误文案必须写明没有迁移：{msg}");
            assert!(msg.contains("重建"), "错误文案必须给出操作员动作（删除/归档后重建）：{msg}");
        }
        other => panic!("期望 SchemaRebuildRequired，拿到 {other:?}"),
    }

    // 拒绝发生在任何 DDL 之前：版本没被抬、表一张没建、journal_mode 仍是默认。
    let db = dir.path().join("memory.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    assert_eq!(ddl::schema_version(&conn).unwrap(), 5, "被拒的 v5 库被静默抬了版本");
    let journal_mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
    assert_eq!(journal_mode, "delete", "MEMORY_SQL 的 PRAGMA 都不该落到被拒的库上");
    drop(conn);
    assert_eq!(tables(&db), Vec::<String>::new(), "拒绝之前记忆库就跑过 DDL 了");
}

#[test]
fn every_legacy_version_is_rejected() {
    // v1..=v5 全是旧 12 表形状的同一条路：没有一条被迁。
    for v in 1..=5 {
        let dir = tempfile::tempdir().unwrap();
        empty_db_with_version(dir.path(), v);
        let e = Root::open(dir.path()).map(|_| ()).unwrap_err();
        let e_str = format!("{e:?}");
        assert!(matches!(
            e,
            StoreError::SchemaRebuildRequired { db: "memory.db", found, supported }
                if found == v && supported == ddl::MEMORY_VERSION
        ), "v{v} 库必须返回 SchemaRebuildRequired，实际 {e_str}");
    }
}

#[test]
fn a_replica_with_a_legacy_schema_is_refused_too() {
    // 副本侧只校验、不迁移；旧形状的副本一样进不来（`require_memory_schema`）。
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("replica.db");
    {
        let conn = rusqlite::Connection::open(&snap).unwrap();
        conn.pragma_update(None, "user_version", 5).unwrap();
    }
    let e = rc_store::AgentRoot::open_replica(dir.path(), &snap).map(|_| ()).unwrap_err();
    let e_str = format!("{e:?}");
    assert!(matches!(
        e,
        StoreError::SchemaRebuildRequired { db: "memory.db", found: 5, supported }
            if supported == ddl::MEMORY_VERSION
    ), "旧 schema 的副本必须同样被拒，实际 {e_str}");
    // 版本没被改写：副本是只读柄，拒绝就是不碰它。
    let conn = rusqlite::Connection::open(&snap).unwrap();
    assert_eq!(ddl::schema_version(&conn).unwrap(), 5);
}

#[test]
fn v0_and_v6_are_both_accepted_idempotently() {
    // 0 = 空库（fresh）⇒ 建到 6；6 = canonical ⇒ 重放但不动形状。
    let dir = tempfile::tempdir().unwrap();
    {
        let root = Root::open(dir.path()).unwrap();
        assert_eq!(ddl::schema_version(root.memory().conn()).unwrap(), ddl::MEMORY_VERSION);
    }
    let before = tables(&dir.path().join("memory.db"));
    let root = Root::open(dir.path()).unwrap();
    assert_eq!(ddl::schema_version(root.memory().conn()).unwrap(), ddl::MEMORY_VERSION);
    drop(root);
    assert_eq!(tables(&dir.path().join("memory.db")), before, "v6 重开改动了表清单");
    assert!(!before.is_empty(), "fresh 库不该是空的");
}

#[test]
fn a_future_memory_version_is_still_schema_too_new() {
    // 门的上半段没有被重建语义吃掉：>6 报 SchemaTooNew（拒绝降级），不是 SchemaRebuildRequired。
    let dir = tempfile::tempdir().unwrap();
    empty_db_with_version(dir.path(), ddl::MEMORY_VERSION + 1);
    let e = Root::open(dir.path()).map(|_| ()).unwrap_err();
    let e_str = format!("{e:?}");
    assert!(matches!(
        e,
        StoreError::SchemaTooNew { db: "memory.db", found, supported }
            if found == ddl::MEMORY_VERSION + 1 && supported == ddl::MEMORY_VERSION
    ), "v7 库必须返回 SchemaTooNew，实际 {e_str}");
}
