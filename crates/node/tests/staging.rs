use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_store::Root;

fn seed_centre(path: &std::path::Path) -> String {
    let root = Root::open(path).unwrap();
    let key = put_card(
        root.memory(),
        &FactContent {
            kind: "decision".into(),
            title: "staging base".into(),
            body: "staging base body".into(),
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
fn staging_is_a_copy_of_the_base_with_its_own_index_and_marker() {
    let node = tempfile::tempdir().unwrap();
    let centre = tempfile::tempdir().unwrap();
    seed_centre(centre.path());

    let replica = node.path().join("replica");
    std::fs::create_dir_all(&replica).unwrap();
    let base = replica.join("memory.db");
    std::fs::copy(centre.path().join("memory.db"), &base).unwrap();
    let before = std::fs::read(&base).unwrap();

    let manifest = node::state::CurrentManifest::default();
    node::staging::prepare(node.path(), &manifest, 100).unwrap();
    assert!(node::staging::is_active(node.path()));
    assert_eq!(std::fs::read(&base).unwrap(), before, "底座必须逐字节不动");

    let staged = node::staging::path(node.path());
    let memory = rc_store::open_replica(&staged).unwrap();
    assert!(
        rc_index::index_status(&memory).unwrap().is_some(),
        "暂存层要带自己的索引"
    );
    let empty: i64 = memory
        .conn()
        .query_row("SELECT count(*) FROM staging_new_row", [], |r| r.get(0))
        .unwrap();
    assert_eq!(empty, 0);
    // Windows 上留着只读句柄会挡住后面整层删除。
    drop(memory);

    let new_handle = {
        let root = rc_store::AgentRoot::open_replica_rw(node.path(), &staged).unwrap();
        let key = put_card(
            root.memory(),
            &FactContent {
                kind: "decision".into(),
                title: "staging new".into(),
                body: "staging new body".into(),
                scope: Scope { kind: ScopeKind::Project, key: "p".into() },
                valid_to_ms: None,
            },
            &[Provenance { turn_uid: "b".repeat(64), anchor_role: AnchorRole::Human }],
            200,
        )
        .unwrap();
        rc_index::rebuild(root.memory(), 200).unwrap();
        root.memory()
            .conn()
            .execute(
                "INSERT INTO staging_new_row(id, table_name) VALUES(?1,'card')",
                [key.key_string()],
            )
            .unwrap();
        key.key_string()
    };
    assert_eq!(node::staging::new_rows(node.path()), vec![new_handle]);

    node::staging::drop_layer(node.path()).unwrap();
    assert!(!node::staging::is_active(node.path()), "整层丢弃");
    assert!(base.is_file(), "底座不许被暂存层带走");
    assert_eq!(std::fs::read(&base).unwrap(), before);
}
