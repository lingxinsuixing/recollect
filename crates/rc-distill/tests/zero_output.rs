//! "写零行 = 错误"的四个执法点（§7）加上**反面的那一个**（全被抑制 ≠ 零产出）。
//! 前四条断同一组读数：记忆行数 == 0、游标没推进、`extract_state` 落了 `rejected/schema`。
//! 第五条件断的是相反方向：写成 0 行、抑制 4 行、游标**推**、状态是 `extracted`。
//! 五条缺一条，P-04 就只挡住一半（挡住的与放走的方向恰好相反）。
use rc_distill::extract::{run_extract, select_batch, state_of};
use rc_distill::DistillError;
use rc_ledger::water;
use rc_llm::{mock::MockBackend, LlmFailure};
use rc_store::Root;
mod common;
use common::seed;

#[test]
fn five_keys_all_empty_is_an_error_not_a_finished_batch() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, 2, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), 40).unwrap().unwrap();
    let empty = r#"{"entities":[],"cards":[],"events":[],"assertions":[],"worthless_turn_uids":[]}"#;
    let backend = MockBackend::scripted(vec![Ok(empty.to_string())]);
    let e = run_extract(root.memory(), root.ledger(), &plan, &backend, 3_000).unwrap_err();
    assert!(matches!(e, DistillError::Contract(rc_distill::contract::ContractError::ZeroOutput)), "{e}");
    assert_eq!(e.reason_slug(), Some("schema"));
    assert_eq!(common::content_rows(&root), 0);
    assert!(water::read(root.ledger(), "distill", &plan.water_scope).unwrap().is_none(),
            "零产出的那一批不许把游标推上去");
    for u in &uids {
        assert_eq!(state_of(root.ledger(), u).unwrap(), Some(water::ExtractState::Rejected));
    }
}

#[test]
fn a_missing_key_drops_the_whole_batch() {
    // §7 的五键判据：少一个 `events` 键 ⇒ 整批丢弃（不是"events 当空数组"）。
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, 2, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), 40).unwrap().unwrap();
    let mut v = serde_json::json!({"entities":[],"cards":[{"kind":"lesson","title":"甲",
        "body":"乙","valid_to_ms":null,"provenance":[uids[0]]}],"assertions":[],
        "worthless_turn_uids":[]});
    let raw = v.as_object_mut().unwrap().into_iter().map(|(k, x)| format!("\"{k}\":{x}"))
        .collect::<Vec<_>>().join(",");
    let backend = MockBackend::scripted(vec![Ok(format!("{{{raw}}}") )]);
    let e = run_extract(root.memory(), root.ledger(), &plan, &backend, 3_000).unwrap_err();
    assert!(matches!(e, DistillError::Contract(rc_distill::contract::ContractError::MissingKey("events"))), "{e}");
    assert_eq!(common::content_rows(&root), 0, "合法的那几条卡片也不许单独落库");
    assert!(water::read(root.ledger(), "distill", &plan.water_scope).unwrap().is_none());
    let _ = v;
}

#[test]
fn malformed_json_is_not_read_as_an_empty_object() {
    // 三处执法点之一：JSON 解析**永不**返回空对象。`serde_json::from_str::<Value>` 的
    // Err 分支必须显式走 `NotAnObject`，不许 `unwrap_or(json!({}))`——那一行会把
    // "模型没答"变成"答了个空"，而空数组五键在这一层是错误。
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    seed(&mut root, 2, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), 40).unwrap().unwrap();
    for raw in ["not json at all", "[]", "null", "{\"entities\":"] {
        let backend = MockBackend::scripted(vec![Ok(raw.to_string())]);
        let e = run_extract(root.memory(), root.ledger(), &plan, &backend, 3_000).unwrap_err();
        assert!(matches!(e, DistillError::Contract(rc_distill::contract::ContractError::NotAnObject)),
                "{raw} ⇒ {e}");
    }
    assert_eq!(common::content_rows(&root), 0);
    assert!(water::read(root.ledger(), "distill", &plan.water_scope).unwrap().is_none());
}

#[test]
fn a_transport_failure_leaves_no_state_at_all() {
    // 故障矩阵第一条与状态机的接缝：LlmFailure **不写** `extract_state`。
    // 写了 rejected 就等于"这批永久放弃"，而 §6 的第四态说的是"恢复即续"。
    // 两者只能有一个真，这里是后者（放弃的那一族是契约违规，不是超时）。
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    seed(&mut root, 2, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), 40).unwrap().unwrap();
    let backend = MockBackend::scripted(vec![Err(LlmFailure::Timeout)]);
    let e = run_extract(root.memory(), root.ledger(), &plan, &backend, 3_000).unwrap_err();
    assert!(matches!(e, DistillError::Llm(LlmFailure::Timeout)), "{e}");
    assert_eq!(common::content_rows(&root), 0);
    assert!(water::read(root.ledger(), "distill", &plan.water_scope).unwrap().is_none());
    for u in plan.rows_ref() {
        assert_eq!(state_of(root.ledger(), &u.turn_uid).unwrap(), None,
                   "故障期的轮次必须还是'从没蒸过'，下一轮才可能重蒸");
    }
    // 下一轮：同一个批、同一个候选集合（差集没被 rejected 污染）。
    let again = select_batch(root.transcript(), root.ledger(), 40).unwrap().unwrap();
    assert_eq!(again.rows_ref().len(), 2);
}

#[test]
fn everything_suppressed_is_a_success_not_a_zero_output() {
    // P-04 的反向判据（本轮对拍新翻出来的分界）：全撞抑制账的一批是**成功**。
    // 若实现拿 `keys.is_empty()` 当 ZeroOutput，这里会 Err ⇒ 游标不推 ⇒ 下一轮重蒸同一批，
    // 而那正是 P-32 的活锁：同一批永远被选中、配额永远重烧。
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, 2, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), 40).unwrap().unwrap();
    let backend = MockBackend::scripted(vec![Ok(common::payload(&uids))]);
    let first = run_extract(root.memory(), root.ledger(), &plan, &backend, 3_000).unwrap();
    assert_eq!((first.rows_written, first.suppressed), (4, 0), "第一批正常写四条");

    // 把这四条全部记上退场账（模拟"审核判死"之后的重蒸），再喂同一份 payload。
    for f in rc_memory::facts(root.memory()).unwrap() {
        rc_memory::retire(root.memory(), &f.key, "audit:vote1", 4_000).unwrap();
    }
    let second_plan = select_batch(root.transcript(), root.ledger(), 40);
    assert!(second_plan.unwrap().is_none(), "已蒸过的批本来就不该被重选（P-23）");
    // 运维清了一次状态（模拟 `hub` 之外的重置路径）。清状态**不会**把游标倒回去，所以这批不会被
    // 重新选中；今天还能重蒸这一批的只剩一条路：**重放同一个 `plan`**——`water::advance` 的倒退
    // 判据是严格小于，等值走 upsert ⇒ 幂等（任务 3 Step 3），这正是"推进 = 前进或幂等"的另一半。
    for u in &uids {
        water::set_extract_state(root.ledger(), u, water::ExtractState::Pending, None, 5_000).unwrap();
    }
    assert!(select_batch(root.transcript(), root.ledger(), 40).unwrap().is_none(),
            "重置状态不倒游标：游标已经推过这批，重选读不到它们");
    let backend2 = MockBackend::scripted(vec![Ok(common::payload(&uids))]);
    let out = run_extract(root.memory(), root.ledger(), &plan, &backend2, 6_000).unwrap();
    assert_eq!((out.rows_written, out.suppressed), (0, 4), "四条全被抑制：写成 0、抑制 4");
    assert!(water::read(root.ledger(), "distill", &plan.water_scope).unwrap().is_some(),
            "成功就必须推水位——这是它与 ZeroOutput 唯一的读数分界");
    assert_eq!(common::content_rows(&root), 4, "原行还在，但没有一条被追加出处（守卫 13）");
}

#[test]
fn a_pending_state_row_is_spoken_for_even_with_the_cursor_at_zero() {
    // 这一条补的是上一条**遮不住**的那一格：上一条的 `None` 是游标推过造成的读数，
    // 所以它证不了"过滤器认不认 `Pending`"。这里游标停在 0（一条都没蒸过），三条转录里
    // 中间那条有一行 `pending` 状态 ⇒ 批必须跳过它、留下另外两条。
    // 判据的形状因此被钉住：过滤看的是**状态行在场**，不是状态值（P-23 的字面）。
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, 3, 9_000);
    water::set_extract_state(root.ledger(), &uids[1], water::ExtractState::Pending, None, 9_500).unwrap();
    let plan = select_batch(root.transcript(), root.ledger(), 40).unwrap().unwrap();
    let got: Vec<String> = plan.rows_ref().iter().map(|r| r.turn_uid.clone()).collect();
    assert_eq!(got, vec![uids[0].clone(), uids[2].clone()], "带 Pending 状态行的那条不许进批");
    assert_eq!(plan.cursor_after.turn_uid, uids[2], "游标只到批内最后一条");
}
