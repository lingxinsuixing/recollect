//! 抽取落库的出处判据：每一条写出去的行都带得出至少一条出处（出口判据 ②），
//! 且出处指向的 uid 必须真在批内（V6 的两侧各读一次做差）。
use rc_distill::batch::Batch;
use rc_distill::DistillError;
use rc_distill::extract::{run_extract, select_batch, state_of, BatchPlan, MAX_TURNS_PER_BATCH};
use rc_ledger::{water, water::ExtractState};
use rc_llm::{mock::MockBackend, LlmBackend, LlmFailure, Prompt};
use rc_memory::BatchCtx;
use rc_store::Root;
use std::cell::Cell;
use std::collections::HashSet;

mod common;
use common::{content_rows, payload, seed, worthless_payload};

#[test]
fn every_written_row_carries_at_least_one_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, 3, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH).unwrap().unwrap();
    assert_eq!(plan.rows_ref().len(), 3);
    assert_eq!(plan.ctx.source_agent, "agent-batch-A", "上下文是从会话行读出来的，不是造的");

    let backend = MockBackend::scripted(vec![Ok(payload(&uids))]);
    let out = run_extract(root.memory(), root.ledger(), &plan, &backend, 3_000).unwrap();
    assert_eq!(out.rows_written, 4);
    assert_eq!(out.worthless, 0);
    assert_eq!(backend.calls(), 1);

    let rows = content_rows(&root);
    let with_prov: i64 = root.memory().conn().query_row(
        "SELECT count(DISTINCT mem_table || ':' || mem_id) FROM provenance", [], |r| r.get(0)).unwrap();
    assert_eq!(rows, 4, "四条内容 ⇒ 四行");
    assert_eq!(with_prov, rows, "少一行出处 = X 的写入面漏了闸门（出口判据 ②）");
}

#[test]
fn provenance_never_points_outside_the_batch() {
    // V6：`provenance.turn_uid` 在 memory.db、`turns.turn_uid` 在 transcript.db，
    // **一条 SQL 查不到两个库**（§1，永不 ATTACH）。所以这里是两侧各读一次、在 Rust 里做差。
    // 这一条判据不许被"优化"成一条子查询——那不是慢，那是查不出来。
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, 2, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), 40).unwrap().unwrap();
    let backend = MockBackend::scripted(vec![Ok(payload(&uids))]);
    run_extract(root.memory(), root.ledger(), &plan, &backend, 3_000).unwrap();

    let batch: HashSet<String> = plan.rows_ref().iter().map(|r| r.turn_uid.clone()).collect();
    let refs: HashSet<String> = root.memory().conn()
        .prepare("SELECT DISTINCT turn_uid FROM provenance").unwrap()
        .query_map([], |r| r.get::<_, String>(0)).unwrap()
        .collect::<Result<_, _>>().unwrap();
    let orphans: Vec<&String> = refs.iter().filter(|u| !batch.contains(*u)).collect();
    assert!(orphans.is_empty(), "出处指向批外的 uid：{orphans:?}");
    assert_eq!(refs.len(), 2, "两条轮都被引用（断引用集合的大小，不然空集也算过）");
}

#[test]
fn content_rows_carry_the_batch_context_not_the_synthetic_stamps() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, 3, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), 40).unwrap().unwrap();
    let backend = MockBackend::scripted(vec![Ok(payload(&uids))]);
    run_extract(root.memory(), root.ledger(), &plan, &backend, 3_000).unwrap();

    // canonical `card` 带写入者两列；旧 card/event/assertion 三类内容物理上同落这一张表，
    // 所以逐行都必须带批次值（合成常量会在这里现形）。
    let n: i64 = root.memory().conn()
        .query_row("SELECT count(*) FROM card WHERE source_agent='agent-batch-A'
                    AND device_id='dev-batch-A'", [], |r| r.get(0)).unwrap();
    let total: i64 = root.memory().conn()
        .query_row("SELECT count(*) FROM card", [], |r| r.get(0)).unwrap();
    assert!(total > 0, "card 是零行产出：等式会在 0==0 上空过 ⇒ 『批次上下文进了 P 列』与『这张表根本没产出』必须分开读");
    assert_eq!(n, total, "card 里有 {n}/{total} 行的 P 列不是批次值 ⇒ 还在用合成常量");
    // canonical `anchor` 没有 source_agent/device_id 两列（本体表不记写入者）；它的归属由程序算，
    // 这里断实体行的 scope 不是合成常量，而是从 kind/title 推出来的 (entity, 项目进度)。
    let anchor_scope: (String, String) = root.memory().conn()
        .query_row("SELECT scope_kind, scope_key FROM anchor WHERE canonical='项目进度'", [],
                   |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!((anchor_scope.0.as_str(), anchor_scope.1.as_str()), ("entity", "项目进度"),
               "anchor 的归属必须是程序算的，不是模型给的、也不是合成常量");
    // anchor_role 也一样归程序：断言那一条引用了 human(uids[0]) 与 agent(uids[1]) 两型。
    let roles: Vec<String> = root.memory().conn()
        .prepare("SELECT DISTINCT anchor_role FROM provenance ORDER BY anchor_role").unwrap()
        .query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(roles, vec!["agent".to_string(), "human".to_string()],
               "锚角色是从 turns.role 查的（归属表：anchor_role 归 Program）");
}

#[test]
fn rerunning_the_same_batch_calls_the_backend_zero_times() {
    // 坑 P-23 的正身：已蒸过 = `extract_state` 里有这一行，不是去记忆表反查。
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let first = seed(&mut root, 2, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), 40).unwrap().unwrap();
    let b1 = MockBackend::scripted(vec![Ok(payload(&first))]);
    run_extract(root.memory(), root.ledger(), &plan, &b1, 3_000).unwrap();
    assert_eq!(b1.calls(), 1);

    assert!(select_batch(root.transcript(), root.ledger(), 40).unwrap().is_none(),
            "全部已表态 ⇒ 没有候选。P-19：这一轮根本不许调 LLM");
    // `EmptyBatch` 那一支**在今天的生产路径上没有生产者**：`select_batch` 里 `rows.is_empty()` 就
    // `continue`，它交不出空批。所以这一支只能手搓一个空 `BatchPlan` 来喂——它是第二道闸门，
    // 防的是未来有人把选批改成"过滤后不截断"，那时空批会真的流到 `run_extract`。
    // （不许因为它不可达就删变体：任务 9 的 `hub` 与任务 10 的缝都按名字匹配它。）
    let empty = BatchPlan {
        batch: Batch { session_key: "sess-batch-A".into(), device_id: "dev-batch-A".into(),
                       source_agent: "agent-batch-A".into(), project_hint: None, rows: vec![] },
        water_scope: "dev-batch-A/sess-batch-A".into(),
        cursor_after: plan.cursor_after.clone(),
        ctx: BatchCtx { source_agent: "agent-batch-A".into(), device_id: "dev-batch-A".into() },
        // C3：空批到不了写入口（`EmptyBatch` 在 backend 之前返回），归属只需形状合法。
        owner: rc_memory::owner::OwnerCtx::synthetic("agent-batch-A"),
    };
    let b0 = MockBackend::scripted(vec![]);
    assert!(matches!(run_extract(root.memory(), root.ledger(), &empty, &b0, 3_500),
                     Err(DistillError::EmptyBatch)),
            "空批必须走 EmptyBatch，而不是走到 backend 再回来");
    assert_eq!(b0.calls(), 0, "空批不许烧配额（EmptyBatch 在调用 backend 之前就返回）");

    let second = seed(&mut root, 2, 5_000);
    let plan2 = select_batch(root.transcript(), root.ledger(), 40).unwrap().unwrap();
    assert_eq!(plan2.rows_ref().len(), 2, "游标 + extract_state 的差集：只蒸新的那两条");
    let b2 = MockBackend::scripted(vec![Ok(payload(&second))]);
    run_extract(root.memory(), root.ledger(), &plan2, &b2, 6_000).unwrap();
    assert_eq!(b2.calls(), 1);
}

#[test]
fn worthless_is_recorded_as_a_state_not_as_an_absence() {
    // 出口判据 ② 的另一半：worthless 的轮要读得出来，而不是"查不到"。
    // 任务 3 的 `rc-ledger/tests/water.rs` 那边断的是**表**，这里断的是**产出形状**：
    // rows_written==0 但不是失败。两张尺子对着同一个性质，缺一半都算漏。
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, 2, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), 40).unwrap().unwrap();
    let backend = MockBackend::scripted(vec![Ok(worthless_payload(&uids))]);
    let out = run_extract(root.memory(), root.ledger(), &plan, &backend, 3_000).unwrap();
    assert_eq!(out.rows_written, 0);
    assert_eq!(out.worthless, 2);
    assert_eq!(content_rows(&root), 0);
    for u in &uids {
        assert_eq!(state_of(root.ledger(), u).unwrap(), Some(ExtractState::Worthless));
    }
    // 游标照样推进（这批处理完了），且下一轮没有候选。
    assert!(water::read(root.ledger(), "distill", &plan.water_scope).unwrap().is_some());
    assert!(select_batch(root.transcript(), root.ledger(), 40).unwrap().is_none());
}

/// P-17 的执法：**测试自带一个 `LlmBackend`**，在 `complete()` 被调用的那一刻读事务状态。
/// `m` 是共享借用，`run_extract` 也要共享借用 ⇒ 两个 `&MemoryHandle` 并存合法。
struct TxSpy<'a> {
    m: &'a rc_store::MemoryHandle,
    inner: MockBackend,
    autocommit_at_call: Cell<Option<bool>>,
}
impl LlmBackend for TxSpy<'_> {
    fn complete(&self, prompt: &Prompt) -> Result<String, LlmFailure> {
        self.autocommit_at_call.set(Some(self.m.conn().is_autocommit()));
        self.inner.complete(prompt)
    }
}

#[test]
fn no_transaction_is_open_during_the_llm_call() {
    // 原设计 §6："写锁绝不跨 LLM 往返"。这条断的是运行期事实，不是代码形状。
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, 3, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), 40).unwrap().unwrap();
    let spy = TxSpy { m: root.memory(), inner: MockBackend::scripted(vec![Ok(payload(&uids))]),
                      autocommit_at_call: Cell::new(None) };
    run_extract(root.memory(), root.ledger(), &plan, &spy, 3_000).unwrap();
    assert_eq!(spy.autocommit_at_call.get(), Some(true),
               "调用 LLM 时 autocommit=false ⇒ 有一个打开的写事务横跨往返");
}
