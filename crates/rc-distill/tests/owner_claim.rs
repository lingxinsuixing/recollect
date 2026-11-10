// crates/rc-distill/tests/owner_claim.rs
//! W37-A2 / G13②：`select_batch` 的主人一格改经 `resolve_principal`（设计
//! `2026-11-07-w37-readgate-design.md` §4）。
//!
//! 断点回顾：旧实现裸读 `sessions.owner_principal_id` 列——那一列是 append-only 旧锚，
//! 章后补的会话它保持 NULL ⇒ 章后补的会话蒸出的行**永久缺章**（存量 226 行的主因之一）。
//! 修复后：章（claim）优先、sessions 列回落（`resolve_principal` 的既定两级），
//! 读到的 id 再过 `principal_is_known`（`unknown`/空白不是人）。
//!
//! 各用例的钉位（防"单源化顺手改行为"）：
//! - `claim_stamped_after_the_fact_reaches_the_extract_batch`：**变异钉**。把 select_batch
//!   改回裸读列（变异证明 A）⇒ 只有这一条红；
//! - `a_known_column_without_a_claim_still_falls_back`：旧行为钉（列回落），单源化不许动它；
//! - `the_claim_wins_when_the_column_disagrees`：章是真相，列是旧锚；
//! - `an_unknown_claim_never_becomes_the_owner`：两级都过 `principal_is_known`。
use rc_distill::extract::{run_extract, select_batch, MAX_TURNS_PER_BATCH};
use rc_ledger::transcript::{claim_principal, NewPrincipalClaim};
use rc_llm::mock::MockBackend;
use rc_store::Root;

mod common;
use common::{payload, seed};

/// 给夹具会话盖一条章。`source` 走闭集字面量；`claimed_by` 与生产 env 盖章同一格字面量。
fn stamp(root: &Root, principal_id: &str, source: &str) {
    claim_principal(
        root.transcript(),
        NewPrincipalClaim {
            session_key: "sess-batch-A",
            device_id: "dev-batch-A",
            principal_id,
            principal_name: Some("示例主人"),
            source,
            claimed_by: "env:RECOLLECT_PRINCIPAL_NAME",
            reason: None,
        },
        5_000,
    )
    .unwrap();
}

/// 把 `sessions.owner_principal_id` 列抹成 NULL。
///
/// 生产库里"缺章 226 行"的形状就是这一格：章是后来补的（P1-7 的镜像同步落地之前，
/// 或列被运维清过），`claim` 表有真章、列还是 NULL。夹具用生产唯一写入口盖章之后
/// 再抹列，正是为了把这个形状钉在 `select_batch` 的读面上。
fn wipe_column(root: &Root) {
    root.transcript()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id=NULL
              WHERE session_key='sess-batch-A' AND device_id='dev-batch-A'",
            [],
        )
        .unwrap();
}

/// 列上写一个指定值（模拟 hermes 导出器/运维手填的旧锚）。
fn set_column(root: &Root, value: Option<&str>) {
    root.transcript()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id=?1
              WHERE session_key='sess-batch-A' AND device_id='dev-batch-A'",
            rusqlite::params![value],
        )
        .unwrap();
}

/// 章是后补的、列还是 NULL ⇒ `select_batch` 读到章上的 id，蒸出的行不再缺章。
/// **变异钉**：把 select_batch 改回裸读列 ⇒ 本条当场红（列是 NULL，读回 None）。
#[test]
fn claim_stamped_after_the_fact_reaches_the_extract_batch() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, 3, 1_000);
    stamp(&root, "p-claim", "agent_runtime");
    wipe_column(&root);

    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH)
        .unwrap()
        .unwrap();
    assert_eq!(
        plan.owner.owner_principal_id.as_deref(),
        Some("p-claim"),
        "章后补的会话不许再蒸出缺章行（G13② 的正文）"
    );
    // 下游形状一格不变：owner_agent/visibility/retention 仍从会话行来（S1b-C3 原样）。
    assert_eq!(plan.owner.owner_agent_id, "agent-batch-A");
    assert_eq!(plan.owner.visibility, rc_memory::owner::Visibility::PrivateAgent);
    assert_eq!(plan.owner.retention, "default");

    // 端到端臂：同一批蒸出的 4 行在 `resource_owner` 上全部带章（缺章率的直接读数）。
    let backend = MockBackend::scripted(vec![Ok(payload(&uids))]);
    run_extract(root.memory(), root.ledger(), &plan, &backend, 3_000).unwrap();
    let n: i64 = root
        .memory()
        .conn()
        .query_row(
            "SELECT count(*) FROM resource_owner WHERE owner_principal_id='p-claim'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 4, "实体 + 卡片 + 事件 + 断言，4 行全部带章");
}

/// 旧行为钉（不许顺手改掉）：没有章、只有列上的已知 id ⇒ 列回落照旧。
/// 变异证明 A 的反臂：改回裸读列时本条仍绿——钉住的是"回落"这一格没丢。
#[test]
fn a_known_column_without_a_claim_still_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    seed(&mut root, 3, 1_000);
    set_column(&root, Some("p-col"));

    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH)
        .unwrap()
        .unwrap();
    assert_eq!(plan.owner.owner_principal_id.as_deref(), Some("p-col"));
}

/// 章与列打架 ⇒ 章赢（claim 表是真相，列是 append-only 旧锚）。
#[test]
fn the_claim_wins_when_the_column_disagrees() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    seed(&mut root, 3, 1_000);
    stamp(&root, "p-claim", "agent_runtime");
    // 盖章入口会同步镜像列（P1-7）；这里把列改成另一个 id，构造生产里"两处不一致"的库。
    set_column(&root, Some("p-other"));

    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH)
        .unwrap()
        .unwrap();
    assert_eq!(plan.owner.owner_principal_id.as_deref(), Some("p-claim"));
}

/// 两级都过 `principal_is_known`：现行章是 `unknown`（Agent 答的"不知道"）⇒
/// 不算身份，落到 `None`（最保守），不许把 `unknown` 冒充主人写进归属行。
#[test]
fn an_unknown_claim_never_becomes_the_owner() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    seed(&mut root, 3, 1_000);
    stamp(&root, "unknown", "unknown");
    wipe_column(&root);

    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH)
        .unwrap()
        .unwrap();
    assert_eq!(plan.owner.owner_principal_id, None);
}
