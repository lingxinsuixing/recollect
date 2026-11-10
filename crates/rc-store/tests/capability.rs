use rc_store::{capability::*, ddl, Root};

#[test]
fn fts5_is_present_because_sqlite_is_bundled() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    assert!(fts5_available(root.transcript().conn()),
        "bundled SQLite 不带 FTS5 ⇒ 检索面当场失效，这不是可选项");
}

#[test]
fn cjk_analyzer_segments_chinese_instead_of_returning_nothing() {
    // 守卫 8：中文不许静默空返回。语言无关，但一期先钉中文这一臂。
    let t = cjk_tokens("先跑测试 再合并");
    assert!(!t.is_empty());
    assert!(t.iter().any(|x| x.contains("测试")), "{t:?}");
}

#[test]
fn report_carries_target_and_versions_and_no_paths() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let r = probe(root.transcript(), root.ledger(), root.memory()).unwrap();
    assert!(!r.target.is_empty());
    assert_eq!(r.sqlite_version.split('.').count(), 3);
    assert_eq!(r.transcript_schema, ddl::TRANSCRIPT_VERSION);
    assert_eq!(r.ledger_schema, ddl::LEDGER_VERSION);
    assert_eq!(r.memory_schema, Some(ddl::MEMORY_VERSION), "Root::open 迁移三库 ⇒ 记忆层版本要报得出来");
    assert_eq!(r.norm_version, NORM_VERSION);
    // 零路径：报告里没有任何字段带文件系统分隔符
    let s = serde_json::to_string(&r).unwrap();
    assert!(!s.contains('/') && !s.contains('\\'), "{s}");
}
