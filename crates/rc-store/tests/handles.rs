use rc_store::Root;

#[test]
fn ledger_handle_cannot_see_transcript_tables() {
    // 句柄各持一条连接 ⇒ 用 ledger 句柄查 turns 必然失败（不是"没数据"，是没有这张表）
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let e = root
        .ledger()
        .conn()
        .query_row("SELECT count(*) FROM main.turns", [], |r| r.get::<_, i64>(0))
        .unwrap_err();
    assert!(e.to_string().contains("no such table"), "{e}");
}

#[test]
fn each_connection_has_exactly_one_attached_schema() {
    // 永不 ATTACH 的证据：PRAGMA database_list 里除了 temp 只有一条
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    for conn in [root.transcript().conn(), root.ledger().conn(), root.memory().conn()] {
        let mut st = conn.prepare("PRAGMA database_list").unwrap();
        let names: Vec<String> = st.query_map([], |r| r.get(1)).unwrap().filter_map(|r| r.ok()).collect();
        let real: Vec<&String> = names.iter().filter(|n| *n != "temp").collect();
        assert_eq!(real.len(), 1, "database_list = {names:?}");
    }
}

#[test]
fn memory_handle_cannot_see_ledger_tables() {
    // 第三只句柄同样互盲：用记忆柄查水位层的表，失败原因是"没有这张表"。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let e = root
        .memory()
        .conn()
        .query_row("SELECT count(*) FROM main.import_log", [], |r| r.get::<_, i64>(0))
        .unwrap_err();
    assert!(e.to_string().contains("no such table"), "{e}");
}
