//! H7c2：`review_watermark` 的存储面（派发档 §6 第一条）。
//!
//! 三条：put/all 往返（字段一个不差）、同 pass 覆盖（`INSERT OR REPLACE` 不是追加）、
//! 空表读回空 vec（不是错误）。用真 `Root::open(tempdir)`：DDL 的 `IF NOT EXISTS`
//! 是否把新表建出来，也只有真库能钉住。

use rc_ledger::review_mark::{self, ReviewWatermark};
use rc_store::Root;

fn mark(pass_id: &str, max_decision_id: i64, updated_at_ms: i64) -> ReviewWatermark {
    ReviewWatermark {
        pass_id: pass_id.to_string(),
        max_decision_id,
        pending_review: 3,
        quarantined: 1,
        applied: 2,
        updated_at_ms,
    }
}

#[test]
fn put_and_all_round_trip_every_column() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let w = mark("fact", 42, 1_700_000_000_000);
    review_mark::put(root.ledger(), &w).unwrap();

    let rows = review_mark::all(root.ledger()).unwrap();
    assert_eq!(rows, vec![w.clone()], "六列必须逐字往返（含 updated_at_ms）");
    assert_eq!(rows[0].pending_review, 3);
    assert_eq!(rows[0].quarantined, 1);
    assert_eq!(rows[0].applied, 2);
    assert_eq!(rows[0].max_decision_id, 42);
}

#[test]
fn a_second_put_of_the_same_pass_replaces_the_row() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    review_mark::put(root.ledger(), &mark("event", 10, 100)).unwrap();
    let mut newer = mark("event", 17, 200);
    newer.pending_review = 0;
    newer.quarantined = 0;
    newer.applied = 5;
    review_mark::put(root.ledger(), &newer).unwrap();

    let rows = review_mark::all(root.ledger()).unwrap();
    assert_eq!(rows.len(), 1, "同 pass 必须覆盖，不是追加：{rows:?}");
    assert_eq!(rows[0], newer);
}

#[test]
fn all_is_ordered_by_pass_id_and_an_empty_table_reads_empty() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    assert_eq!(
        review_mark::all(root.ledger()).unwrap(),
        Vec::<ReviewWatermark>::new(),
        "空表读回空 vec，不是错误"
    );

    // 反序写入三行：读回必须按 pass_id 升序（快照导出的顺序与这里同一把尺子）。
    for pass in ["time", "naming", "fact"] {
        review_mark::put(root.ledger(), &mark(pass, 1, 10)).unwrap();
    }
    let ids: Vec<String> = review_mark::all(root.ledger())
        .unwrap()
        .into_iter()
        .map(|w| w.pass_id)
        .collect();
    assert_eq!(ids, vec!["fact", "naming", "time"]);
}
