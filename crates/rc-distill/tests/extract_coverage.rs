//! 覆盖缺口：模型对批内某条 turn 一个字都没答（既没引用、也没列进 worthless）时，
//! 那一条不能留在无状态里——`select_batch` 只认状态行，游标越过它以后就永久丢失。
//! 处置是单轮 `Rejected(schema)`，已抽出的行照写、游标照推，并留下 `turns_uncovered` 计数。

mod common;

use rc_distill::extract::{run_extract, select_batch, state_of, MAX_TURNS_PER_BATCH};
use rc_ledger::water::{self, ExtractState};
use rc_llm::mock::MockBackend;
use rc_store::Root;

#[test]
fn a_turn_the_model_never_mentions_is_rejected_not_left_stateless() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = common::seed(&mut root, 2, 1_000);
    // 只覆盖第一条：第二条既不在任何 provenance 里，也不在 worthless 里。
    let payload = serde_json::json!({
        "entities": [],
        "cards": [{"kind":"lesson","title":"只覆盖第一条","body":"合成正文",
                   "valid_to_ms":null,"provenance":[uids[0]]}],
        "events": [],
        "assertions": [],
        "worthless_turn_uids": []
    }).to_string();
    let backend = MockBackend::scripted(vec![Ok(payload)]);
    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH)
        .unwrap().expect("两条 turn 必须选得出来");
    assert_eq!(plan.rows_ref().len(), 2, "夹具的前提是两条都进批");

    let out = run_extract(root.memory(), root.ledger(), &plan, &backend, 3_000).unwrap();
    assert_eq!(out.rows_written, 1, "已覆盖的那一条照常落库");
    assert_eq!(state_of(root.ledger(), &uids[0]).unwrap(), Some(ExtractState::Extracted));
    assert_eq!(state_of(root.ledger(), &uids[1]).unwrap(), Some(ExtractState::Rejected),
        "模型没提的那一条必须有状态行；无状态 + 游标前进 = 永久丢失");
    let reason: String = root.ledger().conn().query_row(
        "SELECT reason FROM extract_state WHERE turn_uid=?1",
        [&uids[1]], |r| r.get(0)).unwrap();
    assert_eq!(reason, "schema", "reason 只用契约族的封闭短语，不造第五个状态词");

    // 覆盖缺口在运维面可数：状态行只说 rejected，分不出"答复不合格"与"模型压根没提"。
    let uncovered: i64 = root.ledger().conn().query_row(
        "SELECT count FROM usage_counter
         WHERE subject_kind='llm_job' AND subject_id='extract' AND metric='turns_uncovered'",
        [], |r| r.get(0)).unwrap();
    assert_eq!(uncovered, 1);

    // 游标照推：下一轮不会再选这两条（第一条已 extracted、第二条已 rejected）。
    assert!(water::read(root.ledger(), "distill", "dev-batch-A/sess-batch-A")
        .unwrap().is_some(), "成功批次必须推游标");
}
