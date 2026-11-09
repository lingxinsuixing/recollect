//! transcript.db v4 -> v5：`sessions` 补 7 个归属列（可空、无 DEFAULT），旧行留 NULL；
//! 重开幂等；fresh 库与迁移库的列形状逐列相等；`sessions_by_owner` 在两种库上都在场。
//!
//! 夹具手写 v4 形状（6 列 sessions + commit_seq + v2/v3/v4 的四张新表），
//! 与 `transcript_v2_migration.rs` 同一手法：不依赖任何旧产物。

use rc_store::Root;
use rusqlite::Connection;

/// 7 个归属列，顺序就是 `TRANSCRIPT_SQL` / `backfill_transcript_v5` 的追加顺序。
const OWNERSHIP: [&str; 7] = [
    "owner_principal_id",
    "owner_agent_id",
    "scope_kind",
    "scope_key",
    "visibility",
    "retention",
    "ownership_src",
];

fn make_v4_transcript(path: &std::path::Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE sessions(
            session_key TEXT NOT NULL,
            device_id   TEXT NOT NULL,
            source_agent TEXT NOT NULL,
            machine     TEXT,
            project_hint TEXT,
            last_saved_at_ms INTEGER,
            PRIMARY KEY(session_key, device_id));
         CREATE TABLE turns(
            turn_uid TEXT PRIMARY KEY,
            session_key TEXT NOT NULL,
            device_id TEXT NOT NULL,
            role TEXT NOT NULL CHECK(role IN ('human','agent','system')),
            body TEXT NOT NULL,
            body_fp TEXT NOT NULL,
            turn_key_tier INTEGER NOT NULL CHECK(turn_key_tier IN (1,2,3)),
            project_hint TEXT,
            created_at_ms INTEGER NOT NULL,
            ingested_at_ms INTEGER NOT NULL,
            commit_seq INTEGER NOT NULL DEFAULT 0);
         CREATE INDEX turns_by_session ON turns(session_key, created_at_ms);
         CREATE INDEX turns_by_session_ingested ON turns(session_key, ingested_at_ms);
         CREATE INDEX turns_by_commit ON turns(commit_seq, turn_uid);
         CREATE TABLE center_state(
            id INTEGER PRIMARY KEY CHECK(id=1),
            last_commit_seq INTEGER NOT NULL DEFAULT 0,
            epoch INTEGER NOT NULL DEFAULT 1);
         INSERT INTO center_state(id, last_commit_seq, epoch) VALUES(1, 0, 1);
         CREATE TABLE ingest_log(
            commit_seq INTEGER PRIMARY KEY,
            epoch INTEGER NOT NULL,
            op TEXT NOT NULL CHECK(op IN ('import')),
            session_key TEXT NOT NULL,
            device_id TEXT NOT NULL,
            turns_count INTEGER NOT NULL,
            content_sha256 TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL);
         CREATE TABLE tombstone(
            commit_seq INTEGER PRIMARY KEY,
            epoch INTEGER NOT NULL,
            scope_kind TEXT NOT NULL CHECK(scope_kind IN ('session','device')),
            scope_key TEXT NOT NULL,
            turns_removed INTEGER NOT NULL,
            content_sha256 TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL);
         CREATE INDEX tombstone_scope ON tombstone(scope_kind, scope_key);
         CREATE TABLE replica_ack(
            device_id TEXT PRIMARY KEY,
            applied_seq INTEGER NOT NULL,
            epoch INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL);
         INSERT INTO sessions(session_key, device_id, source_agent, machine, project_hint,
                              last_saved_at_ms)
            VALUES('s1','d1','agent',NULL,'proj-old',1000);
         INSERT INTO turns(turn_uid, session_key, device_id, role, body, body_fp,
                           turn_key_tier, project_hint, created_at_ms, ingested_at_ms, commit_seq)
            VALUES('u1','s1','d1','human','旧正文','旧指纹',1,'proj-old',1000,1000,0);
         PRAGMA user_version=4;",
    )
    .unwrap();
}

/// `pragma_table_info` 的 (name, type) 逐列序列——`cid` 序即建表/追加顺序。
fn column_shape(conn: &Connection, table: &str) -> Vec<(String, String)> {
    let mut st = conn
        .prepare(&format!(
            "SELECT name, type FROM pragma_table_info('{table}') ORDER BY cid"
        ))
        .unwrap();
    st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn index_present(conn: &Connection, name: &str) -> bool {
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='index' AND name=?1",
            [name],
            |r| r.get(0),
        )
        .unwrap();
    n == 1
}

fn open_fresh(dir: &std::path::Path) -> Root {
    let data = dir.join("data");
    std::fs::create_dir_all(&data).unwrap();
    Root::open(&data).unwrap()
}

type Ownership = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

#[test]
fn v4_transcript_is_upgraded_to_v5_with_the_seven_ownership_columns() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    make_v4_transcript(&data.join("transcript.db"));

    let root = Root::open(&data).unwrap();
    let conn = root.transcript().conn();
    let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(version, rc_store::ddl::TRANSCRIPT_VERSION);
    assert_eq!(version, 5, "v4->v5 是本片唯一的版本动作");

    let shape = column_shape(conn, "sessions");
    assert_eq!(shape.len(), 13, "sessions 应当是 6 旧列 + 7 归属列：{shape:?}");
    for name in OWNERSHIP {
        assert!(
            shape.iter().any(|(n, t)| n == name && t == "TEXT"),
            "sessions 缺列 {name}（或类型不是 TEXT）：{shape:?}"
        );
    }

    // 旧行不动：6 个旧列原样，7 个新列全 NULL（迁移不回填）。
    let (source_agent, old_ownership): (String, Ownership) = conn
        .query_row(
            "SELECT source_agent, owner_principal_id, owner_agent_id, scope_kind, scope_key,
                    visibility, retention, ownership_src
             FROM sessions WHERE session_key='s1' AND device_id='d1'",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    (
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ),
                ))
            },
        )
        .unwrap();
    assert_eq!(source_agent, "agent", "迁移弄丢了旧行的 source_agent");
    assert_eq!(
        old_ownership,
        (None, None, None, None, None, None, None),
        "旧行的归属列不该被迁移凭空填上（NULL = 未知 ⇒ 读侧最保守）"
    );
    let (body, commit_seq): (String, i64) = conn
        .query_row("SELECT body, commit_seq FROM turns WHERE turn_uid='u1'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((body.as_str(), commit_seq), ("旧正文", 0), "迁移不许动 turns");
}

#[test]
fn reopening_the_v5_transcript_keeps_the_migration_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    make_v4_transcript(&data.join("transcript.db"));
    let shape_before;
    {
        let root = Root::open(&data).unwrap();
        shape_before = column_shape(root.transcript().conn(), "sessions");
        // 二次迁移的判据不只是"不报错"：列不许被重复追加。
        assert_eq!(shape_before.len(), 13);
    }
    let root = Root::open(&data).unwrap();
    let conn = root.transcript().conn();
    assert_eq!(column_shape(conn, "sessions"), shape_before, "重开又动了一次列形状");
    let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(version, rc_store::ddl::TRANSCRIPT_VERSION);
    let rows: i64 = conn.query_row("SELECT count(*) FROM sessions", [], |r| r.get(0)).unwrap();
    assert_eq!(rows, 1, "重开把 sessions 行复制/清空了");
}

#[test]
fn fresh_and_migrated_sessions_have_the_same_column_shape() {
    // 迁移库与 fresh 库不许分叉（名字/类型/顺序逐列相等）：
    // 分叉的第一个受害者是 `restore` 的显式列拷贝与备份版本分支。
    let fresh_dir = tempfile::tempdir().unwrap();
    let fresh = open_fresh(fresh_dir.path());

    let old_dir = tempfile::tempdir().unwrap();
    let old_data = old_dir.path().join("data");
    std::fs::create_dir_all(&old_data).unwrap();
    make_v4_transcript(&old_data.join("transcript.db"));
    let migrated = Root::open(&old_data).unwrap();

    let fresh_shape = column_shape(fresh.transcript().conn(), "sessions");
    let migrated_shape = column_shape(migrated.transcript().conn(), "sessions");
    assert_eq!(fresh_shape, migrated_shape, "fresh 与迁移库的 sessions 列形状分叉");
    assert_eq!(fresh_shape.len(), 13);

    // 相邻对照：整张表的建表语句也只剩这 13 列（`CREATE TABLE IF NOT EXISTS`
    // 对旧表是空操作 ⇒ 迁移库的形状只能来自 ALTER，不可能来自重放 DDL）。
    for root in [&fresh, &migrated] {
        for name in OWNERSHIP {
            // 无 DEFAULT、可空：ALTER 与 fresh 都不许带值域/默认值，否则两侧分叉。
            let (notnull, dflt): (i64, Option<String>) = root
                .transcript()
                .conn()
                .query_row(
                    &format!(
                        "SELECT \"notnull\", dflt_value FROM pragma_table_info('sessions')
                         WHERE name='{name}'"
                    ),
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!((notnull, dflt), (0, None), "{name} 不是可空无默认值列");
        }
    }
}

#[test]
fn sessions_by_owner_index_exists_on_both_shapes() {
    let fresh_dir = tempfile::tempdir().unwrap();
    let fresh = open_fresh(fresh_dir.path());

    let old_dir = tempfile::tempdir().unwrap();
    let old_data = old_dir.path().join("data");
    std::fs::create_dir_all(&old_data).unwrap();
    make_v4_transcript(&old_data.join("transcript.db"));
    let migrated = Root::open(&old_data).unwrap();

    assert!(
        index_present(fresh.transcript().conn(), "sessions_by_owner"),
        "fresh 库没有 sessions_by_owner"
    );
    assert!(
        index_present(migrated.transcript().conn(), "sessions_by_owner"),
        "迁移库没有 sessions_by_owner（索引不靠版本，apply 每次都要补）"
    );
    // 索引确实盖在这两列上：改列名后它必须红，而不是一条恒真的"名字在场"。
    let sql: String = migrated
        .transcript()
        .conn()
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name='sessions_by_owner'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        sql.contains("owner_agent_id") && sql.contains("owner_principal_id"),
        "sessions_by_owner 盖错了列：{sql}"
    );
}
