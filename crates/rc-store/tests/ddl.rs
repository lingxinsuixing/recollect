use rc_store::{ddl, Root};

/// 正文行上一律不许出现的派生列（架构 §1：状态住水位库，不住转录库）。
/// 这是禁止词表而非名单：要禁的东西得写死，比对方向是"库里现算出的列 ∉ 禁止词表"。
const FORBIDDEN_COLUMNS: [&str; 7] = [
    "distill_state",
    "extract_state",
    "extracted",
    "imported",
    "score",
    "importance",
    "verdict",
];

fn columns(conn: &rusqlite::Connection, table: &str) -> Vec<String> {
    let sql = format!("SELECT name FROM pragma_table_info('{table}')");
    let mut st = conn.prepare(&sql).unwrap();
    st.query_map([], |r| r.get::<_, String>(0)).unwrap().filter_map(|r| r.ok()).collect()
}

/// 表清单现算（守卫 2：不抄名单）—— 唯一来源是 sqlite_master。
/// 转录侧与账侧共用这一条，两边任何一张没登记的表都会在这里露头。
fn tables(conn: &rusqlite::Connection) -> Vec<String> {
    let mut st = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap();
    st.query_map([], |r| r.get::<_, String>(0)).unwrap().filter_map(|r| r.ok()).collect()
}

/// 索引清单现算。`tables` 只数 `type='table'`，所以本阶段新加的索引**不会**打破它——
/// 那是刻意的（见 `provenance_has_a_turn_index_for_the_directory_join` 的注释），
/// 也因此必须有一个专门数 `type='index'` 的助手。
fn index_names(conn: &rusqlite::Connection, _db: &str) -> Vec<String> {
    let mut st = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap();
    st.query_map([], |r| r.get::<_, String>(0)).unwrap().filter_map(|r| r.ok()).collect()
}

fn table_sql(conn: &rusqlite::Connection, table: &str) -> String {
    let mut st = conn
        .prepare("SELECT sql FROM sqlite_master WHERE type='table' AND name=?")
        .unwrap();
    st.query_row(rusqlite::params![table], |r| r.get::<_, String>(0)).unwrap()
}

/// 逐列对拍禁止词表；转录库每张表都过一遍，不只是 turns。
fn assert_no_derived_columns(conn: &rusqlite::Connection, table: &str) {
    let cols = columns(conn, table);
    for bad in FORBIDDEN_COLUMNS {
        assert!(!cols.iter().any(|c| c == bad), "{table} 出现了派生列 {bad}");
    }
}

#[test]
fn transcript_rows_have_no_derived_columns() {
    // 架构 §1：转录行上不许有任何派生列（"是否已提取"住水位库）
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let conn = root.transcript().conn();
    assert_no_derived_columns(conn, "turns");
    let cols = columns(conn, "turns");
    for want in ["turn_uid", "session_key", "device_id", "role", "body", "body_fp",
                 "turn_key_tier", "created_at_ms", "ingested_at_ms", "commit_seq"] {
        assert!(cols.iter().any(|c| c == want), "turns 缺列 {want}");
    }
}

#[test]
fn transcript_has_exactly_the_registered_tables() {
    // 守卫 2 的转录那一半（review I-1）：清单从 sqlite_master 现算，禁止词表
    // 要过一遍转录库的每张表，不只是 turns。只盯 turns 的话，往 sessions 加一列
    // score、或者塞第三张没登记的影子表，两条守卫都还是绿的。
    // v2 登记 center_state/ingest_log；v3 再登记 tombstone（purge 的复制轴）；
    // v4 登记 replica_ack（备份回执，设备清理本地副本的依据）；
    // 身份层 I1 登记 session_principal_claim（主人身份的追加式侧表，设计 principal-identity §3.1）。
    // W38a 刀 A 登记 turn_shadow（FTS5 影子表）+ 它的 5 张自动辅助表
    // （_config/_content/_data/_docsize/_idx，与 rc-index 的 rc_shadow 同一家族形状）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let conn = root.transcript().conn();
    let roster = tables(conn);
    assert_eq!(
        roster,
        vec![
            "center_state".to_string(),
            "ingest_log".to_string(),
            "replica_ack".to_string(),
            "session_principal_claim".to_string(),
            "sessions".to_string(),
            "tombstone".to_string(),
            "turn_shadow".to_string(),
            "turn_shadow_config".to_string(),
            "turn_shadow_content".to_string(),
            "turn_shadow_data".to_string(),
            "turn_shadow_docsize".to_string(),
            "turn_shadow_idx".to_string(),
            "turns".to_string()
        ]
    );
    for table in &roster {
        assert_no_derived_columns(conn, table);
    }
}

#[test]
fn ledger_has_exactly_the_registered_tables() {
    // 守卫 2 的本阶段那一半：表清单从 sqlite_master 现算，不抄名单。改它 = 登记新表。
    // 将来有人往 ledger 塞一张没登记的表，这条必红。
    // S2a-1 登记 `extract_pass_state`（多 pass 状态机；字典序在 `extract_state` 前）。
    // S2e-3 登记 `extract_pass_artifact`（拒收原文；字典序在 `extract_pass_state` 前）。
    // H7c2 登记 `review_watermark`（审查轴水位；字典序在 `pre_distilled_turn` 后）。
    // S4b1 登记 `invite`（邀请票登记面；字典序在 `import_log` 后、`job_watermark` 前）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    assert_eq!(tables(root.ledger().conn()),
               vec!["audit_log".to_string(), "device".to_string(),
                    "device_capability".to_string(),
                    "extract_pass_artifact".to_string(), "extract_pass_state".to_string(),
                    "extract_state".to_string(), "import_log".to_string(),
                    "invite".to_string(),
                    "job_watermark".to_string(), "llm_state".to_string(),
                    "pair_code".to_string(), "pre_distilled_turn".to_string(),
                    "review_watermark".to_string(),
                    "usage_counter".to_string()]);
}

#[test]
fn schema_version_is_recorded_and_forward_only() {
    let dir = tempfile::tempdir().unwrap();
    { let root = Root::open(dir.path()).unwrap();
      assert_eq!(ddl::schema_version(root.transcript().conn()).unwrap(), ddl::TRANSCRIPT_VERSION); }
    // 假装拿到一个更未来的库：版本前进只许拒绝，不许降级
    { let conn = rusqlite::Connection::open(dir.path().join("transcript.db")).unwrap();
      conn.pragma_update(None, "user_version", 99).unwrap(); }
    // 计划原文是 `Root::open(dir.path()).unwrap_err()`，那要求 `Root: Debug`；
    // 给 Root 派生 Debug 会把两条 .db 的路径印出来（踩零路径守卫），所以先脱掉 Ok 值。
    let e = Root::open(dir.path()).map(|_| ()).unwrap_err();
    assert!(matches!(e, rc_store::StoreError::SchemaTooNew { .. }), "{e}");
}

/// 合成行：转录一条（转录连接）、账一条（账连接）——句柄互盲，两条连接各写各的。
/// fixtures 一律合成，不碰真实导出。
fn insert_synthetic_turn(conn: &rusqlite::Connection) {
    conn.execute(
        "INSERT INTO turns(turn_uid, session_key, device_id, role, body, body_fp,
                           turn_key_tier, project_hint, created_at_ms, ingested_at_ms)
         VALUES('synthetic-turn', 'synthetic-session', 'synthetic-device', 'human',
               'synthetic body', 'synthetic-fp', 1, 'synthetic-project', 1000, 1001)",
        [],
    )
    .unwrap();
}

fn insert_synthetic_import(conn: &rusqlite::Connection) {
    conn.execute(
        "INSERT INTO import_log(started_at_ms, finished_at_ms, device_id, exchange_sha256,
                               sessions_in, turns_accepted, turns_rejected, batch_reject_code)
         VALUES(1000, 1001, 'synthetic-device', 'synthetic-sha256', 1, 1, 0, NULL)",
        [],
    )
    .unwrap();
}

#[test]
fn reopening_a_migrated_db_reapplies_nothing_and_keeps_rows() {
    // 迁移梯子的幂等那一半（review I-2）：Root::open 每次都会重跑 migrate_all，
    // 对一个已经到版本的库只许 ① 不报错 ② 不动 user_version
    // ③ 不重建表（行不重复、不丢失、AUTOINCREMENT 计数器不归零、建表语句原文不变）。
    let dir = tempfile::tempdir().unwrap();
    let turns_sql_before;
    {
        let root = Root::open(dir.path()).unwrap();
        insert_synthetic_turn(root.transcript().conn());
        insert_synthetic_import(root.ledger().conn());
        // import_log 用 AUTOINCREMENT：插一行 ⇒ sqlite_sequence.seq == 1。
        // 若重开把表 DROP 再建，这个计数器会回到 0 或整行消失。
        let seq: i64 = root
            .ledger()
            .conn()
            .query_row("SELECT seq FROM sqlite_sequence WHERE name='import_log'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(seq, 1);
        turns_sql_before = table_sql(root.transcript().conn(), "turns");
    }

    let root = Root::open(dir.path()).unwrap();
    assert_eq!(ddl::schema_version(root.transcript().conn()).unwrap(), ddl::TRANSCRIPT_VERSION,
               "重开一个已到版本的转录库不该再动 user_version");
    assert_eq!(ddl::schema_version(root.ledger().conn()).unwrap(), ddl::LEDGER_VERSION,
               "重开一个已到版本的账库不该再动 user_version");

    let rows: i64 =
        root.transcript().conn().query_row("SELECT count(*) FROM turns", [], |r| r.get(0)).unwrap();
    assert_eq!(rows, 1, "重开把转录 DDL 又应用了一遍 ⇒ 行被重复或清空");
    let body: String = root
        .transcript()
        .conn()
        .query_row("SELECT body FROM turns WHERE turn_uid=?", rusqlite::params!["synthetic-turn"], |r| r.get(0))
        .unwrap();
    assert_eq!(body, "synthetic body");
    let rows: i64 =
        root.ledger().conn().query_row("SELECT count(*) FROM import_log", [], |r| r.get(0)).unwrap();
    assert_eq!(rows, 1, "重开把账库 DDL 又应用了一遍 ⇒ 行被重复或清空");
    let seq: i64 = root
        .ledger()
        .conn()
        .query_row("SELECT seq FROM sqlite_sequence WHERE name='import_log'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(seq, 1, "import_log 被重建过（AUTOINCREMENT 计数器被清零）");
    assert_eq!(table_sql(root.transcript().conn(), "turns"), turns_sql_before, "turns 被重建过");
    assert_eq!(
        tables(root.transcript().conn()),
        vec![
            "center_state".to_string(),
            "ingest_log".to_string(),
            "replica_ack".to_string(),
            "session_principal_claim".to_string(),
            "sessions".to_string(),
            "tombstone".to_string(),
            "turn_shadow".to_string(),
            "turn_shadow_config".to_string(),
            "turn_shadow_content".to_string(),
            "turn_shadow_data".to_string(),
            "turn_shadow_docsize".to_string(),
            "turn_shadow_idx".to_string(),
            "turns".to_string()
        ]
    );
    // 这张清单从 sqlite_master 现算，改它 = 登记新表。
    assert_eq!(tables(root.ledger().conn()),
               vec!["audit_log".to_string(), "device".to_string(),
                    "device_capability".to_string(),
                    "extract_pass_artifact".to_string(), "extract_pass_state".to_string(),
                    "extract_state".to_string(), "import_log".to_string(),
                    "invite".to_string(),
                    "job_watermark".to_string(), "llm_state".to_string(),
                    "pair_code".to_string(), "pre_distilled_turn".to_string(),
                    "review_watermark".to_string(),
                    "usage_counter".to_string()]);
}

#[test]
fn ledger_too_new_is_rejected_before_any_ddl_runs() {
    // SchemaTooNew 的账库那一半（review I-2 / M-1）：apply 的版本检查在任何
    // execute_batch 之前，所以太新的账库既不会被降级，也不会被建出表来。
    const FUTURE: i32 = 99;
    let dir = tempfile::tempdir().unwrap();
    {
        let conn = rusqlite::Connection::open(dir.path().join("ledger.db")).unwrap();
        conn.pragma_update(None, "user_version", FUTURE).unwrap();
    }
    // 同上：不打印 Root/连接的 Debug，先脱掉 Ok 值再 unwrap_err。
    let e = Root::open(dir.path()).map(|_| ()).unwrap_err();
    match e {
        rc_store::StoreError::SchemaTooNew { db, found, supported } => {
            assert_eq!(db, "ledger.db", "报错的库必须是账库，不是转录库");
            assert_eq!(found, FUTURE);
            assert_eq!(supported, ddl::LEDGER_VERSION);
        }
        other => panic!("期望 SchemaTooNew，拿到 {other}"),
    }

    let conn = rusqlite::Connection::open(dir.path().join("ledger.db")).unwrap();
    assert_eq!(ddl::schema_version(&conn).unwrap(), FUTURE, "太新的账库被静默降级了");
    assert_eq!(tables(&conn), Vec::<String>::new(), "拒绝之前账库就跑过 DDL 了");
    let journal_mode: String =
        conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
    assert_eq!(journal_mode, "delete", "LEDGER_SQL 的第一条 PRAGMA 都不该落到太新的库上");
}

#[test]
fn memory_db_is_versioned_and_wal_and_carries_tables() {
    // 任务 1 立版本与 WAL；任务 2 填上事实族/判定族。
    // 表清单的现算对拍归 rc-memory\tests\ownership.rs（双向，含索引层过滤），
    // 这条只验证迁移真的落了地：库不再为空、事实族在场，且刚 open 的记忆库里【没有】索引层。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let conn = root.memory().conn();
    assert_eq!(ddl::schema_version(conn).unwrap(), ddl::MEMORY_VERSION);
    let journal_mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
    assert_eq!(journal_mode, "wal", "MEMORY_SQL 的第一条就得是 WAL");
    let roster = tables(conn);
    assert!(!roster.is_empty(), "任务 2 之后记忆库不该是空的");
    assert!(roster.iter().any(|n| n == "card"), "记忆库缺表 card（迁移没落地？）");
    // 裁定 R15：索引层（rc_shadow / rc_index_marker）的唯一所有者是 rc-index，MEMORY_SQL 不许建它。
    // 一被建出来就要求立刻填数据，而"整层可丢弃、幂等重建"这条性质会当场破——每次 open 都把空表复活。
    // 所以这条从"断存在"翻成"断不存在"：这是更强的形状，不是削弱。
    for t in ["rc_shadow", "rc_index_marker"] {
        assert!(!roster.iter().any(|n| n == t), "刚 open 的记忆库出现了索引层表 {t}（R15：不该由 MEMORY_SQL 建）");
    }
}

#[test]
fn migrating_a_read_only_memory_handle_is_refused() {
    // ReadOnly 的那一半牙齿：副本句柄上任何迁移路径都要当场拒绝，而不是写进去。
    // migrate_memory 只吃 &MemoryHandle ⇒ 一个只读柄就够构造这条失败。
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("replica.db");
    { let root = Root::open(dir.path()).unwrap(); drop(root); }
    std::fs::copy(dir.path().join("memory.db"), &snap).unwrap();
    let ar = rc_store::AgentRoot::open_replica(dir.path(), &snap).unwrap();
    let e = ddl::migrate_memory(ar.memory()).unwrap_err();
    assert!(matches!(e, rc_store::StoreError::ReadOnly(_)), "{e}");
}

#[test]
fn provenance_has_a_turn_index_for_the_directory_join() {
    // P-57 的 schema 半边。目录查询按 `turn_uid` 自连 `provenance`，而这张表的主键
    // 是 (mem_table, mem_id, turn_uid)——反方向帮不上，没索引就是每次 status 全表扫两遍。
    //
    // 这一条为什么写成"索引在场"而不是"表清单里有什么"：加索引**不 bump `MEMORY_VERSION`**
    // （索引不是表，`apply()` 对 `CREATE ... IF NOT EXISTS` 幂等重放 ⇒ 旧库下次 open 就有），
    // 所以它不能靠版本号执法；而 `type='table'` 那三条清单查询都数不到它，
    // 于是"有人删了这条索引"在今天**一条断言都不红**。这条就是那个缺口。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();          // open 就是 apply
    let names = index_names(root.memory().conn(), "memory");
    assert!(names.iter().any(|n| n == "provenance_turn"), "{names:?}");
    // 另两条既有索引不许因为这条而被搬走或改名（同一段的三条是一个整体）。
    assert!(names.iter().any(|n| n == "card_asof") && names.iter().any(|n| n == "attestation_target"));
}

#[test]
fn fresh_ledger_has_llm_state_and_the_roster_query_proves_itself() {
    // 节 4 第 7 条：轮转态落盘的建表读数（fresh 库 `sqlite_master` 里查得到 llm_state）。
    // 阳性对照与零对照必须与【同一条查询】成对出现（一正一零才算这条查询有效）：
    // 只断"查得到"的话，一条恒返回 1 的坏查询也能骗绿；只断既有表又证明不了新表在场。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let hit = |name: &str| -> i64 {
        root.ledger().conn()
            .query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                       rusqlite::params![name], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(hit("llm_state"), 1, "fresh 账库里查不到 llm_state（建表没落地？）");
    assert_eq!(hit("job_watermark"), 1, "阳性对照：同一条查询对既有表也必须命中");
    assert_eq!(hit("table_that_does_not_exist"), 0, "零对照：同一条查询对不存在的表名必须不命中");
}

#[test]
fn a_v2_ledger_regains_llm_state_on_reopen() {
    // 老库迁移（这一格里唯一能证明"表真的在迁移路径上"的读数）。
    // 形状要点：`apply()` 是无条件重放 LEDGER_SQL、只在 found<target 时写 user_version，
    // 所以【只压版本不删表 ⇒ 断言恒真】。可红的形状是：先 DROP 掉 llm_state、
    // 再把 user_version 压回 2、重开 `Root::open` ⇒ 表被幂等重放重新建出来、版本跟到 4。
    // DROP 的写法先例见 `rc-ledger/tests/water.rs`，`Connection::open(…join("ledger.db"))`
    // + `pragma_update` 的写法先例见本文件的 `ledger_too_new_is_rejected_before_any_ddl_runs`。
    let dir = tempfile::tempdir().unwrap();
    {
        let root = Root::open(dir.path()).unwrap();
        root.ledger().conn().execute_batch("DROP TABLE llm_state").unwrap();
        root.ledger().conn().pragma_update(None, "user_version", 2).unwrap();
    }
    let root = Root::open(dir.path()).unwrap();
    let hit: i64 = root.ledger().conn()
        .query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='llm_state'",
                   [], |r| r.get(0))
        .unwrap();
    assert_eq!(hit, 1, "把 user_version 压回 2 重开后，llm_state 没有被迁移路径重建");
    assert_eq!(ddl::schema_version(root.ledger().conn()).unwrap(), ddl::LEDGER_VERSION,
               "迁移后的账库版本没跟到 LEDGER_VERSION（= 4）");
}

#[test]
fn transcript_has_turn_shadow_fts_with_the_canonical_shape() {
    // W38a 刀 A（设计 2026-11-08-w38a-transcript-search §二.1）：转录检索影子表的建表读数。
    // 阳性对照与零对照与【同一条查询】成对出现（`fresh_ledger_has_llm_state...` 的同一条纪律）。
    // 形状三列逐字钉死；`tokenize='unicode61'` 必须显式出现在 DDL 文本里——
    // 分词器换了列名测不出来（R20-4 同一条），唯一的执法点是建表原文。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let conn = root.transcript().conn();
    let hit = |name: &str| -> i64 {
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
            rusqlite::params![name],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(hit("turn_shadow"), 1, "fresh 转录库里查不到 turn_shadow（FTS 建表没落地？）");
    assert_eq!(hit("sessions"), 1, "阳性对照：同一条查询对既有表也必须命中");
    assert_eq!(hit("no_such_shadow"), 0, "零对照：同一条查询对不存在的表名必须不命中");
    let cols = columns(conn, "turn_shadow");
    assert_eq!(
        cols,
        vec!["turn_uid".to_string(), "tokens".to_string(), "row_len".to_string()],
        "影子表三列形状漂了：{cols:?}"
    );
    let sql = table_sql(conn, "turn_shadow");
    assert!(sql.contains("fts5"), "必须是 FTS5 虚拟表：{sql}");
    assert!(sql.contains("tokenize='unicode61'"), "分词器必须显式写出：{sql}");
    // 名字常量与 DDL 文本同源：写两份就是两处漂移的机会。
    assert!(sql.contains(rc_store::ddl::TURN_SHADOW), "DDL 文本与 TURN_SHADOW 常量漂移：{sql}");
}

#[test]
fn transcript_regains_turn_shadow_on_reopen_after_it_was_dropped() {
    // 老库开库自愈的形状（W38a 刀 A：不 bump TRANSCRIPT_VERSION ⇒ 不能靠版本号执法，
    // 与 session_principal_claim 同先例）。影子层可整层丢弃——DROP 之后重开，
    // `apply` 幂等重放把它建回来，且 turns 一字不动（检索的权威面在正文行上）。
    let dir = tempfile::tempdir().unwrap();
    {
        let root = Root::open(dir.path()).unwrap();
        insert_synthetic_turn(root.transcript().conn());
        root.transcript().conn().execute_batch("DROP TABLE turn_shadow").unwrap();
    }
    let root = Root::open(dir.path()).unwrap();
    let hit: i64 = root
        .transcript()
        .conn()
        .query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='turn_shadow'",
                   [], |r| r.get(0))
        .unwrap();
    assert_eq!(hit, 1, "重开没有把 turn_shadow 幂等重建出来");
    assert_eq!(ddl::schema_version(root.transcript().conn()).unwrap(), ddl::TRANSCRIPT_VERSION,
               "影子层自愈不该动 user_version");
    let rows: i64 = root
        .transcript()
        .conn()
        .query_row("SELECT count(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1, "重建影子层动了正文行");
}
