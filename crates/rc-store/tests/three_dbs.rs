use rc_store::{AgentRoot, Root};

#[test]
fn root_open_creates_all_three_files_and_none_of_them_sees_another() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    for f in rc_store::DATA_FILES {
        assert!(dir.path().join(f).exists(), "缺库文件 {f}");
    }
    // 永不 ATTACH：三条连接各自的 database_list 只有一条 main
    for conn in [root.transcript().conn(), root.ledger().conn(), root.memory().conn()] {
        let mut st = conn.prepare("PRAGMA database_list").unwrap();
        let names: Vec<String> = st.query_map([], |r| r.get(1)).unwrap()
            .filter_map(|r| r.ok()).collect();
        let real: Vec<&String> = names.iter().filter(|n| *n != "temp").collect();
        assert_eq!(real.len(), 1, "database_list = {names:?}");
    }
    // 记忆柄查转录层的表：不是"没数据"，是没有这张表
    let e = root.memory().conn()
        .query_row("SELECT count(*) FROM turns", [], |r| r.get::<_, i64>(0))
        .unwrap_err();
    assert!(e.to_string().contains("no such table"), "{e}");
}

#[test]
fn agent_root_never_needs_the_other_two_files() {
    // 守卫 6 的行为层那一半：把 transcript.db / ledger.db 从盘上拿走，消费侧仍然要能起来。
    // 拿不到的东西不需要守卫。
    let dir = tempfile::tempdir().unwrap();
    {
        let root = Root::open(dir.path()).unwrap();
        root.memory().conn()
            .execute_batch("CREATE TABLE probe_marker(x INTEGER); INSERT INTO probe_marker VALUES(1)")
            .unwrap();
    }
    for f in ["transcript.db", "ledger.db"] {
        std::fs::rename(dir.path().join(f), dir.path().join(format!("{f}.gone"))).unwrap();
    }
    let ar = AgentRoot::open(dir.path()).unwrap();
    let n: i64 = ar.memory().conn()
        .query_row("SELECT count(*) FROM probe_marker", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "AgentRoot 读不到记忆库 ⇒ 它其实还需要别的库");
    assert!(!ar.memory().is_read_only());
    for f in ["transcript.db", "ledger.db"] {
        assert!(
            !dir.path().join(f).exists(),
            "AgentRoot 打开了它不该打开的库：{f} 被创建了出来"
        );
    }
}

#[test]
fn agent_root_open_creates_a_missing_nested_data_dir() {
    // 任务 9 第一段补的那一行 `create_dir_all` 的运行时读者（此前 `AgentRoot::open` 只在
    // 已存在的目录上被调用过 ⇒ 少那一行没有用例能红）。宿主配置里 `--data-dir ./data`
    // 是首次运行，目录连同父目录都还不存在；少那一行的症状不是崩溃，而是"hook 每次会话
    // 开头报一次运维失败"，而宿主会把 hook 关掉。
    // 两级不存在的路径（`fresh/nested`）才是判据：把 `create_dir_all` 写成 `create_dir` 时这里红。
    let dir = tempfile::tempdir().unwrap();
    let fresh = dir.path().join("fresh/nested");
    let ar = AgentRoot::open(&fresh).unwrap();
    assert!(fresh.join("memory.db").exists(), "AgentRoot::open 没把记忆库落在它自己建的目录里");
    assert!(!ar.memory().is_read_only());
}

#[test]
fn replica_root_is_read_only_and_says_so() {
    // §6"本地副本只读"：任何本地写在下次整替时被静默吃掉 ⇒ 这条路径根本不许写。
    let dir = tempfile::tempdir().unwrap();
    {
        let root = Root::open(dir.path()).unwrap();
        root.memory().conn().execute_batch("CREATE TABLE r(x INTEGER)").unwrap();
    }
    let snap = dir.path().join("replica.db");
    std::fs::copy(dir.path().join("memory.db"), &snap).unwrap();
    let ar = AgentRoot::open_replica(dir.path(), &snap).unwrap();
    assert!(ar.memory().is_read_only());
    let e = ar.memory().conn().execute("INSERT INTO r VALUES(1)", []).unwrap_err();
    assert!(e.to_string().contains("readonly"), "{e}");
}

#[test]
fn replica_refuses_a_schema_from_the_future() {
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("replica.db");
    {
        let root = Root::open(dir.path()).unwrap();
        root.memory().conn().pragma_update(None, "user_version", 99).unwrap();
    }
    std::fs::copy(dir.path().join("memory.db"), &snap).unwrap();
    // 同 tests\ddl.rs：不派生 Debug（AgentRoot 里坐着 data_dir），先脱掉 Ok 值再 unwrap_err。
    let e = AgentRoot::open_replica(dir.path(), &snap).map(|_| ()).unwrap_err();
    assert!(matches!(e, rc_store::StoreError::SchemaTooNew { db: "memory.db", .. }), "{e}");
}
