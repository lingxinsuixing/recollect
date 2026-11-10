use node::state::{self, PushCursor, SyncState};

#[test]
fn a_missing_state_file_is_none() {
    let dir = tempfile::tempdir().unwrap();
    assert!(state::load(&state::state_path(dir.path())).unwrap().is_none());
}

#[test]
fn the_state_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let s = SyncState {
        current_endpoint: "lan-ipv6".into(),
        push_cursor: PushCursor { at_ms: 42, turn_uid: "t-7".into() },
        resume_count: 3,
        ..SyncState::default()
    };
    let p = state::state_path(dir.path());
    state::save(&p, &s).unwrap();
    let back = state::load(&p).unwrap().expect("the state is there");
    assert_eq!(back, s);
}

#[test]
fn the_state_file_holds_no_body_no_path_and_no_token() {
    let dir = tempfile::tempdir().unwrap();
    let s = SyncState { last_error: "endpoint_unreachable".into(), ..SyncState::default() };
    let p = state::state_path(dir.path());
    state::save(&p, &s).unwrap();
    let text = std::fs::read_to_string(&p).unwrap();
    assert!(!text.contains(dir.path().to_str().unwrap()), "路径漏进状态文件：{text}");
    assert!(!text.contains("canary-body-9f2a"), "正文漏进状态文件：{text}");
    assert!(!text.contains("Bearer"), "凭证漏进状态文件：{text}");
}

#[test]
fn cleanup_keeps_the_current_generation_and_drops_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let replica = dir.path().join("replica");
    std::fs::create_dir_all(&replica).unwrap();
    std::fs::write(replica.join("snapshot-1-1.db"), b"old").unwrap();
    std::fs::write(replica.join("snapshot-2-1.db"), b"current").unwrap();
    std::fs::write(replica.join("snapshot-2-1.indexed.db"), b"current-index").unwrap();
    std::fs::write(replica.join("snapshot-3-1.part"), b"partial").unwrap();
    std::fs::write(replica.join("memory.db"), b"legacy").unwrap();
    std::fs::write(replica.join("snapshot-1-1.indexed.db"), b"old-index").unwrap();
    let current = node::state::CurrentManifest {
        epoch: 2,
        seq: 1,
        sha256: "x".into(),
        created_at_ms: 1,
    };
    node::state::cleanup_old_replicas(dir.path(), &current);
    assert!(replica.join("snapshot-2-1.db").is_file(), "当前代不许删");
    assert!(replica.join("snapshot-2-1.indexed.db").is_file(), "当前代的索引副本不许删");
    assert!(!replica.join("snapshot-1-1.indexed.db").exists(), "旧代的索引副本要删");
    assert!(!replica.join("snapshot-1-1.db").exists(), "旧代要删");
    assert!(replica.join("snapshot-3-1.part").is_file(), "续传点不许删");
    assert!(!replica.join("memory.db").exists(), "旧名要删");
}

#[test]
fn cleanup_deletes_nothing_when_the_current_generation_is_absent() {
    let dir = tempfile::tempdir().unwrap();
    let replica = dir.path().join("replica");
    std::fs::create_dir_all(&replica).unwrap();
    std::fs::write(replica.join("snapshot-2-1.db"), b"old").unwrap();
    std::fs::write(replica.join("memory.db"), b"legacy").unwrap();
    let current = node::state::CurrentManifest {
        epoch: 3,
        seq: 1,
        sha256: "x".into(),
        created_at_ms: 1,
    };
    node::state::cleanup_old_replicas(dir.path(), &current);
    assert!(replica.join("snapshot-2-1.db").is_file(), "当前代不在时一个都不许删");
    assert!(replica.join("memory.db").is_file(), "升级回退可能正靠旧名");
}
