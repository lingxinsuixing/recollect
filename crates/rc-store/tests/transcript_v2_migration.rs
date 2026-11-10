//! transcript.db v1 -> 当前版本：只加 `turns.commit_seq`、`center_state`、`ingest_log`、
//! `tombstone`、`replica_ack`，旧行保留且 commit_seq 为 0；重开幂等。

use rc_store::Root;
use rusqlite::Connection;

fn make_v1_transcript(path: &std::path::Path) {
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
            ingested_at_ms INTEGER NOT NULL);
         CREATE INDEX turns_by_session ON turns(session_key, created_at_ms);
         CREATE INDEX turns_by_session_ingested ON turns(session_key, ingested_at_ms);
         INSERT INTO sessions(session_key, device_id, source_agent, machine, project_hint,
                              last_saved_at_ms)
            VALUES('s1','d1','agent',NULL,NULL,1000);
         INSERT INTO turns(turn_uid, session_key, device_id, role, body, body_fp,
                           turn_key_tier, project_hint, created_at_ms, ingested_at_ms)
            VALUES('u1','s1','d1','human','旧正文','旧指纹',1,NULL,1000,1000);
         PRAGMA user_version=1;",
    )
    .unwrap();
}

#[test]
fn v1_transcript_is_upgraded_without_losing_rows() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    make_v1_transcript(&data.join("transcript.db"));

    let root = Root::open(&data).unwrap();
    let conn = root.transcript().conn();
    let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(version, rc_store::ddl::TRANSCRIPT_VERSION);

    let cols: Vec<String> = {
        let mut st = conn
            .prepare("SELECT name FROM pragma_table_info('turns') ORDER BY cid")
            .unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert!(cols.iter().any(|c| c == "commit_seq"), "v2 没补 turns.commit_seq");
    let (body, commit_seq): (String, i64) = conn
        .query_row(
            "SELECT body, commit_seq FROM turns WHERE turn_uid='u1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(body, "旧正文", "迁移弄丢了旧正文");
    assert_eq!(commit_seq, 0, "旧行不应凭空获得提交号");

    let tables: Vec<String> = {
        let mut st = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert!(tables.iter().any(|t| t == "center_state"));
    assert!(tables.iter().any(|t| t == "ingest_log"));
    assert!(tables.iter().any(|t| t == "tombstone"));
    assert!(tables.iter().any(|t| t == "replica_ack"));
    let (last_seq, epoch): (i64, i64) = conn
        .query_row("SELECT last_commit_seq, epoch FROM center_state WHERE id=1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((last_seq, epoch), (0, 1));
}

#[test]
fn reopening_the_v2_transcript_keeps_the_migration_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    make_v1_transcript(&data.join("transcript.db"));
    {
        let _ = Root::open(&data).unwrap();
    }
    let root = Root::open(&data).unwrap();
    let (rows, commit_seq): (i64, i64) = root
        .transcript()
        .conn()
        .query_row(
            "SELECT count(*), max(commit_seq) FROM turns WHERE turn_uid='u1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(rows, 1);
    assert_eq!(commit_seq, 0);
    let version: i32 = root
        .transcript()
        .conn()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, rc_store::ddl::TRANSCRIPT_VERSION);
}

/// v3→v4 只加一张表：模拟一个没有 `replica_ack` 的 v3 库，重开后表回来、旧行不动。
#[test]
fn v3_transcript_gains_replica_ack_without_touching_rows() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    make_v1_transcript(&data.join("transcript.db"));
    {
        let root = Root::open(&data).unwrap();
        let conn = root.transcript().conn();
        conn.execute("INSERT INTO ingest_log(commit_seq, epoch, op, session_key, device_id, turns_count, content_sha256, created_at_ms)
                      VALUES(1, 1, 'import', 's1', 'd1', 1, 'sha', 1000)", []).unwrap();
    }
    {
        let conn = Connection::open(data.join("transcript.db")).unwrap();
        conn.execute("DROP TABLE replica_ack", []).unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();
    }
    let root = Root::open(&data).unwrap();
    let conn = root.transcript().conn();
    let has_ack: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='replica_ack'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_ack, 1, "v4 没把 replica_ack 建回来");
    let (turns, logs): (i64, i64) = conn
        .query_row(
            "SELECT (SELECT count(*) FROM turns), (SELECT count(*) FROM ingest_log)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((turns, logs), (1, 1), "加表不许动既有行");
    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, rc_store::ddl::TRANSCRIPT_VERSION);
}
