//! v5 库升级到当前 ledger：新表 `pre_distilled_turn`、`device_capability` 建出来，既有数据一行不动。
use rc_store::{ddl, Root};

fn make_v5_ledger(path: &std::path::Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(r#"
        PRAGMA user_version=5;
        CREATE TABLE usage_counter(
            subject_kind TEXT NOT NULL, subject_id TEXT NOT NULL, metric TEXT NOT NULL,
            count INTEGER NOT NULL DEFAULT 0, last_at_ms INTEGER NOT NULL,
            PRIMARY KEY(subject_kind, subject_id, metric));
        INSERT INTO usage_counter VALUES('device','dev-v5','ingest_turns_accepted',7,123);
    "#).unwrap();
}

#[test]
fn a_v5_ledger_migrates_to_v6_and_keeps_its_rows() {
    let dir = tempfile::tempdir().unwrap();
    make_v5_ledger(&dir.path().join("ledger.db"));
    let root = Root::open(dir.path()).unwrap();
    assert_eq!(ddl::schema_version(root.ledger().conn()).unwrap(), ddl::LEDGER_VERSION);
    let n: i64 = root.ledger().conn()
        .query_row("SELECT count(*) FROM pragma_table_info('pre_distilled_turn')", [], |r| r.get(0))
        .unwrap();
    assert!(n > 0, "pre_distilled_turn 没建出来");
    let n: i64 = root.ledger().conn()
        .query_row("SELECT count(*) FROM pragma_table_info('device_capability')", [], |r| r.get(0))
        .unwrap();
    assert!(n > 0, "device_capability 没建出来");
    // 旧行/旧计数不动。
    let count: i64 = root.ledger().conn()
        .query_row("SELECT count FROM usage_counter WHERE subject_id='dev-v5'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 7, "迁移不许动既有行");
    // 新表可写读。
    root.ledger().conn()
        .execute("INSERT INTO pre_distilled_turn(turn_uid, at_ms) VALUES('u-1', 9)", [])
        .unwrap();
    let at: i64 = root.ledger().conn()
        .query_row("SELECT at_ms FROM pre_distilled_turn WHERE turn_uid='u-1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(at, 9);
}
