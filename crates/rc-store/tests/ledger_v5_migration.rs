//! v4 库升级到 v5：三张新表建出来，既有数据一行不动。
use rc_store::{ddl, Root};

fn make_v4_ledger(path: &std::path::Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(r#"
        PRAGMA user_version=4;
        CREATE TABLE usage_counter(
            subject_kind TEXT NOT NULL, subject_id TEXT NOT NULL, metric TEXT NOT NULL,
            count INTEGER NOT NULL DEFAULT 0, last_at_ms INTEGER NOT NULL,
            PRIMARY KEY(subject_kind, subject_id, metric));
        INSERT INTO usage_counter VALUES('device','dev-old','ingest_turns_accepted',7,123);
    "#).unwrap();
}

#[test]
fn v4_ledger_gains_device_pair_code_and_audit_log_without_touching_rows() {
    let dir = tempfile::tempdir().unwrap();
    make_v4_ledger(&dir.path().join("ledger.db"));
    let root = Root::open(dir.path()).unwrap();
    assert_eq!(ddl::schema_version(root.ledger().conn()).unwrap(), ddl::LEDGER_VERSION);
    for table in ["device", "pair_code", "audit_log"] {
        let n: i64 = root.ledger().conn()
            .query_row("SELECT count(*) FROM pragma_table_info(?1)", [table], |r| r.get(0))
            .unwrap();
        assert!(n > 0, "{table} 没建出来");
    }
    let count: i64 = root.ledger().conn()
        .query_row("SELECT count FROM usage_counter WHERE subject_id='dev-old'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 7, "迁移不许动既有行");
}

#[test]
fn v5_ledger_reopens_without_changes() {
    let dir = tempfile::tempdir().unwrap();
    make_v4_ledger(&dir.path().join("ledger.db"));
    let _ = Root::open(dir.path()).unwrap();
    let root = Root::open(dir.path()).unwrap();
    assert_eq!(ddl::schema_version(root.ledger().conn()).unwrap(), ddl::LEDGER_VERSION);
}
