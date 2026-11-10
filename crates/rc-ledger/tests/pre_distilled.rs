use rc_ledger::water;
use rc_store::Root;

#[test]
fn marking_is_idempotent_and_reads_back() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    water::mark_pre_distilled(root.ledger(), "turn-a", 100).unwrap();
    water::mark_pre_distilled(root.ledger(), "turn-a", 200).unwrap();
    assert!(water::pre_distilled(root.ledger(), "turn-a").unwrap());
    assert!(!water::pre_distilled(root.ledger(), "turn-b").unwrap());
    let at: i64 = root.ledger().conn()
        .query_row("SELECT at_ms FROM pre_distilled_turn WHERE turn_uid='turn-a'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(at, 100, "重复标记不许刷新 at_ms");
}

#[test]
fn pre_distilled_uids_returns_every_marked_turn() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    water::mark_pre_distilled(root.ledger(), "turn-a", 100).unwrap();
    water::mark_pre_distilled(root.ledger(), "turn-b", 101).unwrap();
    let got = water::pre_distilled_uids(root.ledger()).unwrap();
    let want: std::collections::HashSet<String> =
        ["turn-a", "turn-b"].iter().map(|s| s.to_string()).collect();
    assert_eq!(got, want);
}
