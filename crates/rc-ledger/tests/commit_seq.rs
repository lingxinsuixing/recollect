//! 中心提交号：每次真正新增 turn 的导入分配一个单调 commit_seq，
//! turn 行与 ingest_log 同事务；全重复重放不消耗提交号。

use rc_contract::{normalize, parse, ExchangeFile};
use rc_ledger::transcript::{
    commit_sha256, insert_batch_committed, CommitTurn,
};
use rc_store::Root;
use serde_json::json;

fn file(body: &str) -> ExchangeFile {
    parse(
        &serde_json::to_vec(&json!({
            "format_version": 1,
            "sessions": [{"session_key": "ses-A", "device_id": "dev-1", "source_agent": "qoder",
                          "turns": [{"role": "human", "body": body, "created_at_ms": 1730000000000i64}]}]
        }))
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn committed_imports_get_monotonic_seq_and_a_log_row() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let rep = normalize(file("第一个提交"), 1);
    let session = &rep.accepted[0].session;
    let turn = &rep.accepted[0].turn;

    let first = insert_batch_committed(
        root.transcript_mut(),
        session,
        std::slice::from_ref(turn),
        10,
        "import",
    )
    .unwrap();
    assert_eq!(first.inserted, 1);
    assert_eq!(first.commit_seq, Some(1));
    assert_eq!(first.epoch, Some(1));

    let (stamp, logged): (i64, String) = root
        .transcript()
        .conn()
        .query_row(
            "SELECT t.commit_seq, l.content_sha256
             FROM turns t JOIN ingest_log l ON l.commit_seq = t.commit_seq
             WHERE t.turn_uid = ?1",
            rusqlite::params![turn.turn_uid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(stamp, 1);
    let expected = commit_sha256(&[CommitTurn {
        turn_uid: turn.turn_uid.as_str(),
        role: turn.role.as_str(),
        body: turn.body.as_str(),
        body_fp: turn.body_fp.as_str(),
        created_at_ms: turn.created_at_ms,
        ingested_at_ms: 10,
    }]);
    assert_eq!(logged, expected, "ingest_log 的内容哈希与 turn 行对不上");

    let rep2 = normalize(file("第二个提交"), 1);
    let second = insert_batch_committed(
        root.transcript_mut(),
        &rep2.accepted[0].session,
        std::slice::from_ref(&rep2.accepted[0].turn),
        20,
        "import",
    )
    .unwrap();
    assert_eq!(second.commit_seq, Some(2));

    // 同一 turn 重放：不新增行、不消耗提交号、不写第二条日志。
    let replay = insert_batch_committed(
        root.transcript_mut(),
        session,
        std::slice::from_ref(turn),
        30,
        "import",
    )
    .unwrap();
    assert_eq!((replay.inserted, replay.skipped_duplicate), (0, 1));
    assert_eq!(replay.commit_seq, None);
    let (logs, last): (i64, i64) = root
        .transcript()
        .conn()
        .query_row("SELECT (SELECT count(*) FROM ingest_log), last_commit_seq FROM center_state WHERE id=1",
                   [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!(logs, 2);
    assert_eq!(last, 2);
}
