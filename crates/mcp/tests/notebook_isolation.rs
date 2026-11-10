//! W36 细案 §6 的第三条钉：小本本与蒸馏链**零耦合**——`notebook_append` 只碰 core.db，
//! transcript.db / memory.db / ledger.db 三个库**逐表行数**在 append 前后一格不动，
//! review 队列的 pending 也不动。判据是"行数逐表相等"，不是"接口没返回它"：
//! 任何"顺手写了台账/转录/审查行"的新坏法都会当场红，不依赖测试作者事先知道写了哪张表。
//!
//! 模式照 [`handoff_isolation`]（REQUIRED_DBS）：三个库文件**必须都真实读到**，
//! 读不到文件不是"没写行"的证据。core.db 本身不在对账清单里——它是小本本的家，
//! append 后 +1 恰是正证（顺带钉住"这条真的落库了"，否则下面的"零变化"是空断言）。

use rc_core::CoreStore;
use std::collections::BTreeMap;

/// 记忆侧三件（core.db 是小本本自己的家，见档头）。
const REQUIRED_DBS: [&str; 3] = ["transcript.db", "memory.db", "ledger.db"];

/// 逐库逐表行数。`BTreeMap` 给确定序，断言失败时 diff 可读。
fn row_counts(dir: &std::path::Path) -> BTreeMap<String, i64> {
    let mut out = BTreeMap::new();
    for name in REQUIRED_DBS {
        let path = dir.join(name);
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap_or_else(|e| panic!("{name} 必须存在（读不到就不是「没写行」的证据）：{e}"));
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        for table in tables {
            let n: i64 = conn
                .prepare(&format!("SELECT COUNT(*) FROM \"{table}\""))
                .unwrap()
                .query_row([], |r| r.get(0))
                .unwrap();
            out.insert(format!("{name}:{table}"), n);
        }
    }
    out
}

/// review 队列的 pending 读数（细案 §6：不写 review、不占抽取——pending 一格都不许动）。
fn review_pending(dir: &std::path::Path) -> i64 {
    let conn = rusqlite::Connection::open_with_flags(
        dir.join("memory.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM review_item WHERE state='pending_review'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn a_notebook_append_touches_only_core_db() {
    let dir = tempfile::tempdir().unwrap();
    // 三个库与生产同一个入口（`Root::open` 建 transcript/memory/ledger 及其全部表）。
    let _root = rc_store::Root::open(dir.path()).unwrap();
    let store = CoreStore::open(dir.path()).unwrap();
    store.register_agent("agent-a", 1_000).unwrap();

    let before = row_counts(dir.path());
    let pending_before = review_pending(dir.path());

    let written = store
        .notebook_append(
            "agent-a",
            Some("零耦合"),
            &["w36".to_string()],
            "小本本隔离-marker-9d2f：append 只落 core.db",
            None,
            "dev-test",
            1_000,
        )
        .unwrap();
    // 先证明它真的写进去了（否则下面的"三库零变化"是空断言）。
    let listed = store.notebook_list("agent-a", None, None).unwrap();
    assert_eq!(listed.notes.len(), 1);
    assert_eq!(listed.notes[0].id, written.id);
    assert!(written.id.starts_with("n-"));

    let after = row_counts(dir.path());
    assert_eq!(before, after, "append 前后 transcript/memory/ledger 逐表行数必须相等（零耦合）");
    assert_eq!(
        pending_before,
        review_pending(dir.path()),
        "review 队列 pending 不许被 notebook_append 触碰"
    );

    // core.db 里确实多了一张表 + 一行（append 的落库正证，顺带钉 per-agent 动态表形状）。
    // 表名是 `core_note_<8hex>`（sha256 前缀），测试不复制派生算法，直接从 schema 读。
    let core = rusqlite::Connection::open_with_flags(
        dir.path().join("core.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let note_tables: Vec<String> = core
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'core_note_%'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(note_tables.len(), 1, "per-agent 小本本表没建出来：{note_tables:?}");
    let rows: i64 = core
        .query_row(&format!("SELECT COUNT(*) FROM \"{}\"", note_tables[0]), [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1, "append 后小本本表该恰有一行");
}
