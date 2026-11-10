use rc_store::{NodeRoot, StoreError};

#[test]
fn node_root_has_a_transcript_and_a_ledger_but_no_writable_memory() {
    let dir = tempfile::tempdir().unwrap();
    let root = NodeRoot::open_node(dir.path(), None).unwrap();
    assert!(root.memory().is_none(), "还没同步过 ⇒ 没有副本句柄");
    assert!(root.transcript().conn().query_row("SELECT 1", [], |r| r.get::<_, i64>(0)).is_ok());
    assert!(root.ledger().conn().query_row("SELECT 1", [], |r| r.get::<_, i64>(0)).is_ok());
    // 水位库与转录库是两只不同的连接：水位句柄看不见 turns 表。
    let e = root
        .ledger()
        .conn()
        .query_row("SELECT count(*) FROM main.turns", [], |r| r.get::<_, i64>(0))
        .unwrap_err();
    assert!(e.to_string().contains("no such table"), "{e}");
}

#[test]
fn a_missing_replica_file_is_replica_missing_not_a_generic_io_error() {
    let dir = tempfile::tempdir().unwrap();
    // `unwrap_err()` 要 `NodeRoot: Debug`，而计划里的 `NodeRoot` 没有派生它
    // （`TranscriptHandle` / `LedgerHandle` / `MemoryHandle` 也没有）⇒ 这里改成手工取错，
    // 断言的仍然是同一个变体，牙一点没少。
    let err = match NodeRoot::open_node(dir.path(), Some(&dir.path().join("replica.db"))) {
        Ok(_) => panic!("副本文件不在 ⇒ 必须是 ReplicaMissing"),
        Err(e) => e,
    };
    assert!(matches!(err, StoreError::ReplicaMissing), "{err:?}");
}

#[test]
fn an_existing_replica_is_opened_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let replica = dir.path().join("replica.db");
    {
        let root = rc_store::Root::open(dir.path()).unwrap();
        // **先关连接，再拷**：三库跑的是 WAL 模式（`crates/rc-store/src/ddl.rs` 的
        // `PRAGMA journal_mode=WAL`），最新页还留在 `memory.db-wal` 里。
        // 拷一个还开着的库的主文件，拿到的是**半截库**（没有那些表），
        // 症状是后面那句 `no such table` —— 而它长得像"副本没造好"。
        std::mem::drop(root);
    }
    std::fs::copy(dir.path().join("memory.db"), &replica).unwrap();
    let node = NodeRoot::open_node(dir.path(), Some(&replica)).unwrap();
    let m = node.memory().expect("副本在");
    assert!(m.is_read_only(), "副本句柄必须是只读");
    let e = m
        .conn()
        .execute("CREATE TABLE probe_write(x)", [])
        .unwrap_err();
    assert!(e.to_string().contains("readonly"), "{e}");
}
