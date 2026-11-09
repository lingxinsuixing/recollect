//! I1：压缩摘要（`[[compact_summary]]`）单列一类输入。
//!
//! 三条判据：
//! ① 带标记的轮**禁止直接判 worthless**——模型若把它列进 worthless，那一条被拒收
//!    （状态 `rejected` + reason='compact_summary'），并进 `compact_summary_worthless`；
//! ② 模型按 prompt 真抽出事实时，那一轮拿 `extracted`（压缩摘要不是"天然无价值"）；
//! ③ user 段必须点名 Key Decisions / Preferences / Pending / User Messages，且写明禁止 worthless。

mod common;

use rc_distill::extract::{run_extract, select_batch, state_of, MAX_TURNS_PER_BATCH};
use rc_distill::prompt::{extract_user, is_compact_summary, COMPACT_SUMMARY_MARKER};
use rc_ledger::water::ExtractState;
use rc_llm::mock::MockBackend;
use rc_store::Root;

fn counter(root: &Root, metric: &str) -> i64 {
    root.ledger().conn().query_row(
        "SELECT count FROM usage_counter
         WHERE subject_kind='llm_job' AND subject_id='extract' AND metric=?1",
        [metric], |r| r.get(0)).unwrap_or(0)
}

#[test]
fn a_compact_summary_turn_listed_as_worthless_is_rejected_not_worthless() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = common::seed_bodies(&mut root, &[
        &format!("{COMPACT_SUMMARY_MARKER}\n## Key Decisions\n- max_output = 65,536"),
        "ok thanks",
    ], 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH)
        .unwrap().unwrap();
    // 模型偷懒：把压缩摘要和寒暄一起判 worthless。
    let payload = serde_json::json!({
        "entities": [], "cards": [], "events": [], "assertions": [],
        "worthless_turn_uids": [uids[0], uids[1]]
    }).to_string();
    let backend = MockBackend::scripted(vec![Ok(payload)]);
    run_extract(root.memory(), root.ledger(), &plan, &backend, 9_999).unwrap();

    assert_eq!(state_of(root.ledger(), &uids[0]).unwrap(), Some(ExtractState::Rejected),
        "压缩摘要不许拿 worthless：状态必须是 rejected");
    let reason: String = root.ledger().conn().query_row(
        "SELECT reason FROM extract_state WHERE turn_uid=?1", [&uids[0]], |r| r.get(0)).unwrap();
    assert_eq!(reason, "compact_summary", "拒收理由必须单列，不能混进 schema");
    assert_eq!(state_of(root.ledger(), &uids[1]).unwrap(), Some(ExtractState::Worthless),
        "普通寒暄仍走 worthless（这一条是对照组）");
    let worthless_rows: i64 = root.ledger().conn().query_row(
        "SELECT count(*) FROM rc_extract_worthless WHERE turn_uid=?1", [&uids[0]], |r| r.get(0)).unwrap();
    assert_eq!(worthless_rows, 0, "压缩摘要不许写进 worthless 证据表");
    assert_eq!(counter(&root, "compact_summary_turns"), 1);
    assert_eq!(counter(&root, "compact_summary_worthless"), 1);
}

#[test]
fn a_compact_summary_turn_that_yields_a_card_is_extracted() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = common::seed_bodies(&mut root, &[
        &format!("{COMPACT_SUMMARY_MARKER}\n## Key Decisions\n- context_length = 512000"),
    ], 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH)
        .unwrap().unwrap();
    let payload = serde_json::json!({
        "entities": [],
        "cards": [{"kind":"decision","title":"上下文窗口已配置 512K","body":"来自压缩摘要的决定",
                   "valid_to_ms":null,"provenance":[uids[0]]}],
        "events": [], "assertions": [], "worthless_turn_uids": []
    }).to_string();
    let backend = MockBackend::scripted(vec![Ok(payload)]);
    run_extract(root.memory(), root.ledger(), &plan, &backend, 9_999).unwrap();
    assert_eq!(state_of(root.ledger(), &uids[0]).unwrap(), Some(ExtractState::Extracted),
        "压缩摘要产出了 card ⇒ 它是有效抽取，不是 worthless");
    assert_eq!(counter(&root, "compact_summary_worthless"), 0);
}

#[test]
fn the_prompt_names_the_four_sections_and_forbids_worthless() {
    use rc_distill::batch::{Batch, TurnRow};
    let b = Batch {
        session_key: "s".into(), device_id: "d".into(), source_agent: "a".into(),
        project_hint: None,
        rows: vec![TurnRow {
            turn_uid: "a".repeat(64), role: "agent".into(),
            body: format!("{COMPACT_SUMMARY_MARKER}\n## Key Decisions\n- x"),
            created_at_ms: 1, ingested_at_ms: 1,
            speaker: None,
            speaker_anchor_id: None,
        }],
    };
    let u = extract_user(&b);
    // W23a：方括号里改成批内位置别名，所以"两句都引"那句话从 turn ids 改说 refs；
    // 断的仍是同一件事——同一事实在两处出现时必须两个都引，且四类点名与 NEVER 都在场。
    for needle in ["Key Decisions", "Preferences", "Pending", "User Messages",
                   "cite both refs", "NEVER"] {
        assert!(u.contains(needle), "压缩摘要指令缺 `{needle}`：{u}");
    }
    assert!(is_compact_summary(&b.rows[0].body));
    assert!(!is_compact_summary("这句话里提到 [[compact_summary]] 但标记不在行首或独立行"),
        "句中出现标记不认（避免把讨论标记的轮误判成摘要）");
}
