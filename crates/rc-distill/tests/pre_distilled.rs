//! B3a：已由节点自蒸、中心按行吸收的 turn 不再进中心的抽取批。
//! 阳性对照是这条用例的另一半：**同一条 turn 未标记时必须返回 `Some`**，
//! 否则标记后的 `None` 可能只是读者坏了（例如 `pre_distilled_uids` 恒返回全集）。
use rc_distill::extract::{select_batch, MAX_TURNS_PER_BATCH};
use rc_ledger::water;
use rc_store::Root;
mod common;

#[test]
fn a_pre_distilled_turn_is_not_selected_again() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = common::seed(&mut root, 1, 1_000);
    assert!(
        select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH)
            .unwrap()
            .is_some(),
        "未标记的 turn 必须能被选中"
    );
    water::mark_pre_distilled(root.ledger(), &uids[0], 2_000).unwrap();
    assert!(
        select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH)
            .unwrap()
            .is_none(),
        "已回并的 turn 不许再进中心抽取批"
    );
}
