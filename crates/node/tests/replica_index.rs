use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_store::Root;

fn seed_centre(path: &std::path::Path) -> String {
    let root = Root::open(path).unwrap();
    let key = put_card(
        root.memory(),
        &FactContent {
            kind: "decision".into(),
            title: "index me".into(),
            body: "index body".into(),
            scope: Scope { kind: ScopeKind::Project, key: "p".into() },
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: "a".repeat(64), anchor_role: AnchorRole::Human }],
        10,
    )
    .unwrap();
    key.key_string()
}

#[test]
fn a_replica_without_an_index_gets_one_and_the_base_stays_byte_for_byte() {
    let node = tempfile::tempdir().unwrap();
    let centre = tempfile::tempdir().unwrap();
    seed_centre(centre.path());
    let replica = node.path().join("replica");
    std::fs::create_dir_all(&replica).unwrap();
    let base = replica.join("memory.db");
    std::fs::copy(centre.path().join("memory.db"), &base).unwrap();
    let before = std::fs::read(&base).unwrap();

    node::index::ensure_current_replica(node.path(), &Default::default(), 100).unwrap();
    assert_eq!(std::fs::read(&base).unwrap(), before, "底座必须保持清单字节");
    let current = node::state::current_replica_path(node.path()).expect("indexed replica");
    assert!(
        current.ends_with("memory.indexed.db"),
        "读面应指向索引副本：{current:?}"
    );
    let memory = rc_store::open_replica(&current).unwrap();
    assert!(
        rc_index::index_status(&memory).unwrap().is_some(),
        "索引副本必须带 marker"
    );

    // 第二次调用是 no-op：不重写底座，也不重做索引。
    node::index::ensure_current_replica(node.path(), &Default::default(), 200).unwrap();
    assert_eq!(std::fs::read(&base).unwrap(), before);
}
