//! D 表 40 钉值：派生批次的行数上限是四十。
//! 夹具照 `derive_pass_cycle.rs` 的 `put_card` 形状，不新造 harness。

use rc_derive::select::select_batch;
use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

fn put_cards(m: &rc_store::MemoryHandle, n: usize) -> Vec<rc_memory::MemKey> {
    (0..n)
        .map(|i| {
            put_card(
                m,
                &FactContent {
                    kind: "decision".into(),
                    title: format!("batch cap {i}"),
                    body: format!("body {i}"),
                    scope: Scope { kind: ScopeKind::Project, key: "proj-cap".into() },
                    valid_to_ms: None,
                },
                &[Provenance {
                    turn_uid: "a".repeat(64),
                    anchor_role: AnchorRole::Human,
                }],
                NOW + i as i64,
            )
            .unwrap()
        })
        .collect()
}

#[test]
fn the_derive_batch_stops_at_forty_rows() {
    assert_eq!(rc_distill::derive::DERIVE_MAX_ROWS_PER_BATCH, 40);
}

#[test]
fn a_batch_of_forty_one_rows_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let keys = put_cards(root.memory(), 41);
    let batch = select_batch(
        root.memory(),
        None,
        rc_distill::derive::DERIVE_MAX_ROWS_PER_BATCH,
    )
    .unwrap();
    assert_eq!(batch.len(), 40, "上限是 40 行，不是 41");
    let got: Vec<String> = batch.iter().map(|r| r.key.key_string()).collect();
    assert!(got.contains(&keys[39].key_string()), "第 40 行必须在批里");
    assert!(!got.contains(&keys[40].key_string()), "第 41 行留给下一轮");
}
