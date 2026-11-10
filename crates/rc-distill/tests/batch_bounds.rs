use rc_distill::extract::{select_batch, MAX_TURNS_PER_BATCH};
use rc_ledger::water;
use rc_store::Root;
mod common;
use common::seed;

#[test]
fn an_empty_database_has_no_candidates() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    assert!(select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH).unwrap().is_none());
}

#[test]
fn a_41_turn_session_is_cut_at_40_and_the_rest_waits_for_the_next_round() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, MAX_TURNS_PER_BATCH + 1, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH).unwrap().unwrap();
    assert_eq!(plan.rows_ref().len(), MAX_TURNS_PER_BATCH, "P-22 的绝对上限");
    assert_eq!(plan.cursor_after.turn_uid, uids[MAX_TURNS_PER_BATCH - 1],
               "游标只推到本批最后一条，多出来的那条留给下一轮");
    assert!(!plan.rows_ref().iter().any(|r| r.turn_uid == uids[MAX_TURNS_PER_BATCH]),
            "第 41 条不许出现在批里");
}

#[test]
fn the_prompt_batch_follows_created_at_while_the_cursor_stays_ingested() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = common::seed_created_reversed(&mut root, 5, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH)
        .unwrap().unwrap();
    let got: Vec<String> = plan.rows_ref().iter().map(|r| r.turn_uid.clone()).collect();
    let mut want = uids.clone();
    want.reverse();
    assert_eq!(got, want, "批内 prompt 必须按源 created_at 排，不是 ingested/uid 的哈希序");
    assert_eq!(plan.cursor_after.turn_uid, uids[4], "游标仍按到达钟最后一条");
}

#[test]
fn same_millisecond_rows_are_cut_by_uid_lexicographic_order() {
    // §8 的 tie-break 在批次选取侧的同一条尺子：同一 ms 的两条按 uid 字典序切。
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = common::seed_same_ms(&mut root, 4, 7_000);
    let plan = select_batch(root.transcript(), root.ledger(), 2).unwrap().unwrap();
    assert_eq!(plan.rows_ref().len(), 2);
    let got: Vec<String> = plan.rows_ref().iter().map(|r| r.turn_uid.clone()).collect();
    let mut want = uids.clone();
    want.sort();
    assert_eq!(got, want[..2], "排序键必须是 (ingested_at_ms, turn_uid) 而不是插入序");
}

#[test]
fn a_window_fully_filled_by_rejected_rows_keeps_paging_to_the_next_turn() {
    // 坑 P-74：一次过量读（旧形状 `LIMIT limit*4`）会被"连续四批都被标 rejected"铺满，
    // 于是第 161 条之后的新内容永远进不了窗口 ⇒ 会话永久停摆，比 P-32 的重烧配额更坏。
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, MAX_TURNS_PER_BATCH * 5, 20_000);
    for u in &uids[..MAX_TURNS_PER_BATCH * 4] {
        water::set_extract_state(root.ledger(), u, water::ExtractState::Rejected,
                                 Some("schema"), 25_000).unwrap();
    }
    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH).unwrap().unwrap();
    assert_eq!(plan.rows_ref().len(), MAX_TURNS_PER_BATCH, "债务之后必须还能凑满一批");
    assert_eq!(plan.rows_ref()[0].turn_uid, uids[MAX_TURNS_PER_BATCH * 4],
               "批的第一条必须是第 161 条：债务行被跳过而不是把窗口堵死");
    assert_eq!(plan.cursor_after.turn_uid, uids[MAX_TURNS_PER_BATCH * 5 - 1],
               "游标仍只推到本批最后一条（第 200 条）");
}
