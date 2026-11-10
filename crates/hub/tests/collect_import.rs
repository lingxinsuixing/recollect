use rc_store::Root;

const OK: &str = r#"{"format_version":1,"sessions":[{"session_key":"ses-A","device_id":"dev-1","source_agent":"qoder","turns":[{"role":"human","body":"COLLECT-8842","created_at_ms":1730000000000}]}]}"#;

#[test]
fn import_bytes_accepts_then_dedups() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let a = hub::ingest::import_bytes(&mut root, OK.as_bytes(), 1730000000000).unwrap();
    assert_eq!((a.accepted, a.duplicate, a.rejected), (1, 0, 0));
    let b = hub::ingest::import_bytes(&mut root, OK.as_bytes(), 1730000000000).unwrap();
    assert_eq!((b.accepted, b.duplicate, b.rejected), (0, 1, 0));
}

#[test]
fn import_bytes_reports_batch_reject() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let bad = r#"{"format_version":9,"sessions":[]}"#;
    match hub::ingest::import_bytes(&mut root, bad.as_bytes(), 1730000000000) {
        Err(hub::ingest::ImportError::Rejected { batch_reject, .. }) => assert!(batch_reject.is_some()),
        other => panic!("expected batch reject, got {other:?}"),
    }
}
