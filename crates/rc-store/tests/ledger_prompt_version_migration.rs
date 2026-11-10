//! P0-2：给 `extract_pass_state` 补 `prompt_version` 的迁移面。
//!
//! 两条性质：① 旧库（没有这一列、已有状态行）开库即自愈，旧行留 NULL，其它列一个字节不动；
//! ② 对同一个已迁移的库**连开两次**不报错（补列是列存在性驱动的，不是"每次开都 ALTER"）。
use rc_store::{ddl, Root};

/// 建一个"升级前形状"的账库：`extract_pass_state` 没有 `prompt_version`，且已有一条 done 行。
fn make_ledger_without_prompt_version(path: &std::path::Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    // 先让当前版本建全表，再把新列摘掉——等价于"旧二进制建出来的库"。
    drop(conn);
    let root = Root::open(path.parent().unwrap()).unwrap();
    root.ledger()
        .conn()
        .execute_batch("ALTER TABLE extract_pass_state DROP COLUMN prompt_version")
        .unwrap();
    root.ledger()
        .conn()
        .execute(
            "INSERT INTO extract_pass_state(turn_uid, pass_id, family, state, attempt,
                                            next_retry_at_ms, fingerprint, reason, run_id,
                                            updated_at_ms)
             VALUES('turn-old', 'naming', '', 'done', 0, NULL, 'fp-old', NULL, 'run-old', 1)",
            [],
        )
        .unwrap();
}

#[test]
fn the_prompt_version_column_is_added_once_and_old_rows_stay_null() {
    let dir = tempfile::tempdir().unwrap();
    make_ledger_without_prompt_version(&dir.path().join("ledger.db"));

    let has_col = |root: &Root| -> bool {
        let n: i64 = root
            .ledger()
            .conn()
            .query_row(
                "SELECT count(*) FROM pragma_table_info('extract_pass_state') \
                 WHERE name='prompt_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        n == 1
    };
    let old_version = |root: &Root| -> Option<String> {
        root.ledger()
            .conn()
            .query_row(
                "SELECT prompt_version FROM extract_pass_state WHERE turn_uid='turn-old'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };

    // ① 第一次开库：补列；旧行是 NULL；其它列（指纹/run_id/state）原样。
    {
        let root = Root::open(dir.path()).unwrap();
        assert!(has_col(&root), "开库必须补出 prompt_version");
        assert_eq!(old_version(&root), None, "旧行必须留 NULL（不自动全量重跑）");
        let (state, fingerprint, run_id): (String, String, String) = root
            .ledger()
            .conn()
            .query_row(
                "SELECT state, fingerprint, run_id FROM extract_pass_state
                  WHERE turn_uid='turn-old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((state.as_str(), fingerprint.as_str(), run_id.as_str()),
                   ("done", "fp-old", "run-old"), "迁移不许动既有列");
        assert_eq!(ddl::schema_version(root.ledger().conn()).unwrap(), ddl::LEDGER_VERSION);
    }

    // ② 连开第二次：同一张已迁移的库不许报错，列与旧行都不变（补列幂等）。
    let root = Root::open(dir.path()).unwrap();
    assert!(has_col(&root), "第二次开库把列弄丢了");
    assert_eq!(old_version(&root), None, "第二次开库改写了旧行");
    assert_eq!(ddl::schema_version(root.ledger().conn()).unwrap(), ddl::LEDGER_VERSION);
}
