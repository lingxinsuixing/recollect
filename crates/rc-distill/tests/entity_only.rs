//! I7：`extract_state=extracted` 只发给产出 card/event/assertion 的轮。
//! 只产 entity 的轮落 `pending` + reason='entity_only'，并进独立计数器 `entity_only_turns`
//! （一臂一库口径：读本库 ledger.db 的 usage_counter）。13/6 的高估因此变成可分辨的三格。

mod common;

use rc_distill::extract::{run_extract, select_batch, state_of, MAX_TURNS_PER_BATCH};
use rc_ledger::water::ExtractState;
use rc_llm::mock::MockBackend;
use rc_store::Root;

fn counter(root: &Root, metric: &str) -> i64 {
    root.ledger().conn().query_row(
        "SELECT count FROM usage_counter
         WHERE subject_kind='llm_job' AND subject_id='extract' AND metric=?1",
        [metric], |r| r.get(0)).unwrap_or(0)
}

fn name_of(root: &Root, uid: &str) -> Option<String> {
    root.ledger().conn().query_row(
        "SELECT state FROM extract_state WHERE turn_uid=?1", [uid], |r| r.get(0)).ok()
}

#[test]
fn an_entity_only_turn_does_not_get_the_extracted_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = common::seed(&mut root, 2, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH)
        .unwrap().unwrap();
    // 五键齐全、只产两个实体：没有 card/event/assertion。
    let payload = serde_json::json!({
        "entities": [
            {"kind":"concept","title":"只有实体甲","aliases":[],"provenance":[uids[0]]},
            {"kind":"concept","title":"只有实体乙","aliases":[],"provenance":[uids[1]]}
        ],
        "cards": [], "events": [], "assertions": [], "worthless_turn_uids": []
    }).to_string();
    let backend = MockBackend::scripted(vec![Ok(payload)]);
    run_extract(root.memory(), root.ledger(), &plan, &backend, 9_999).unwrap();

    for uid in &uids {
        assert_eq!(state_of(root.ledger(), uid).unwrap(), Some(ExtractState::Pending),
            "只产 entity 的轮不许拿 extracted（I7）");
    }
    let reason: String = root.ledger().conn().query_row(
        "SELECT reason FROM extract_state WHERE turn_uid=?1", [&uids[0]], |r| r.get(0)).unwrap();
    assert_eq!(reason, "entity_only", "状态词表是库级 CHECK，单列靠 reason + 计数器");
    assert_eq!(counter(&root, "entity_only_turns"), 2, "独立计数器必须数到两轮");
    let extracted: i64 = root.ledger().conn().query_row(
        "SELECT count(*) FROM extract_state WHERE state='extracted'", [], |r| r.get(0)).unwrap();
    assert_eq!(extracted, 0, "有效抽取（fact-extracted）读数必须是 0，不是被实体抬高的 2");
}

#[test]
fn a_turn_with_a_card_and_a_turn_with_only_an_entity_are_separated() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = common::seed(&mut root, 2, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH)
        .unwrap().unwrap();
    let payload = serde_json::json!({
        "entities": [{"kind":"concept","title":"同一批的实体","aliases":[],
                      "provenance":[uids[1]]}],
        "cards": [{"kind":"lesson","title":"有事实的轮","body":"合成正文",
                   "valid_to_ms":null,"provenance":[uids[0]]}],
        "events": [], "assertions": [], "worthless_turn_uids": []
    }).to_string();
    let backend = MockBackend::scripted(vec![Ok(payload)]);
    run_extract(root.memory(), root.ledger(), &plan, &backend, 9_999).unwrap();

    assert_eq!(state_of(root.ledger(), &uids[0]).unwrap(), Some(ExtractState::Extracted),
        "产出 card 的轮是 fact-extracted");
    assert_eq!(state_of(root.ledger(), &uids[1]).unwrap(), Some(ExtractState::Pending),
        "只产 entity 的轮单列");
    assert_eq!(name_of(&root, &uids[1]).as_deref(), Some("pending"));
    assert_eq!(counter(&root, "entity_only_turns"), 1);
}
