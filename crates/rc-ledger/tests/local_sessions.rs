use rc_contract::{NormalizedTurn, SessionId, TurnTier};
use rc_ledger::transcript::sessions_after;

fn turn(uid: &str, body: &str, created: i64) -> NormalizedTurn {
    NormalizedTurn {
        turn_uid: uid.to_string(),
        body_fp: rc_contract::compute_body_fp(body),
        tier: TurnTier::ContentOnly,
        body: body.to_string(),
        role: "agent".into(),
        created_at_ms: created,
    }
}

#[test]
fn only_the_turns_after_the_cursor_are_selected() {
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    let sid = SessionId {
        session_key: "s1".into(),
        device_id: "dev-1".into(),
        source_agent: "agent".into(),
        machine: None,
        project_hint: None,
        last_saved_at_ms: None,
        owner_principal_id: None,
        principal_name: None,
        principal_source: None,
    };
    rc_ledger::insert_batch(
        root.transcript(),
        &sid,
        &[turn("t1", "one", 100), turn("t2", "two", 200)],
        1000,
    )
    .unwrap();
    let all = sessions_after(root.transcript(), 0, "").unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].turns.len(), 2);
    let after = sessions_after(root.transcript(), 1000, "t1").unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].turns.len(), 1);
    assert_eq!(after[0].turns[0].turn_uid, "t2");
}

#[test]
fn the_same_session_is_grouped_even_when_another_session_sits_between_its_turns() {
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    for (key, uid, at) in [("s1", "t1", 10), ("s2", "t2", 20), ("s1", "t3", 30)] {
        let sid = SessionId {
            session_key: key.into(),
            device_id: "dev-1".into(),
            source_agent: "agent".into(),
            machine: None,
            project_hint: None,
            last_saved_at_ms: None,
            owner_principal_id: None,
            principal_name: None,
            principal_source: None,
        };
        rc_ledger::insert_batch(
            root.transcript(),
            &sid,
            &[turn(uid, uid, at)],
            at,
        )
        .unwrap();
    }
    let picked = sessions_after(root.transcript(), 0, "").unwrap();
    assert_eq!(picked.len(), 2, "两个会话，不是三个");
    let s1 = picked.iter().find(|s| s.session_key == "s1").unwrap();
    assert_eq!(s1.turns.len(), 2, "s1 的两条轮必须在一个会话里");
}
