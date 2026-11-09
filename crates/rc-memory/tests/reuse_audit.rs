//! 波二刀 B：审计臂的存储面（派发档测试钉④⑤⑥）。
//!
//! 四格钉：
//! 1. `failed`/`mismatch` ⇒ `review_item` 入队，`source='reuse'` / `pass_id='reuse'`，
//!    载荷带卡句柄与事件 id，`turn_uids_json` 取卡的出处轮（与 `report` 先例同形）；
//! 2. `worked`/`partial` 不进队列（闭集钉死）；
//! 3. 同一张卡重复报 `failed` 不重复入队（查重按目标，照 `raise_attention` 先例）；
//! 4. 缺表 = 零候选（刀 A 之前的只读旧副本仍可跑）。
//!
//! 水位那一半（审核轴水位必须镜像）在 `crates/hub/tests/reuse_audit_cli.rs`——水位住
//! `ledger.db`，`rc-memory` 没有它的写入口。
//!
//! 夹具全部合成（守卫 12）。

use rc_memory::reuse::{
    audit_candidates, ingest_audit, record_reuse, ReuseOutcome, AUDIT_OUTCOMES, WORKED_TIER_MIN,
};
use rc_memory::review::{get, list, ReviewState};
use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_store::{MemoryHandle, Root};

const NOW: i64 = 1_700_000_600_000;
const AGENT: &str = "agent-reuse-audit";
const TURN: &str = "c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-reuse-audit".into() }
}

fn human_prov() -> Vec<Provenance> {
    vec![Provenance { turn_uid: TURN.into(), anchor_role: AnchorRole::Human }]
}

fn memory_handle(dir: &std::path::Path) -> MemoryHandle {
    Root::open(dir).unwrap().into_memory()
}

/// 一张有两跳出处的卡（`provenance_chain` 的 `Turn` 步要能取到）。
fn seed_card(m: &MemoryHandle, title: &str) -> String {
    let key = put_card(
        m,
        &FactContent {
            kind: "note".into(),
            title: title.into(),
            body: "审计臂合成正文".into(),
            scope: scope(),
            valid_to_ms: None,
        },
        &human_prov(),
        NOW,
    )
    .unwrap();
    key.id
}

fn reuse_items(m: &MemoryHandle) -> Vec<rc_memory::review::ItemMeta> {
    list(m, None, Some("reuse"), 0).unwrap()
}

/// 钉④：`failed` 入队——source/pass 两格是 `reuse`，载荷带卡句柄与事件，出处轮进 turn_uids。
#[test]
fn failed_进复核队列_source_是_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let m = memory_handle(dir.path());
    let card = seed_card(&m, "复用失败甲卡");
    let event_id = record_reuse(&m, &card, "failed", Some("答案方向错了 \"引号\" \\ 反斜杠"),
        NOW, AGENT).unwrap();

    let out = ingest_audit(&m, NOW).unwrap();
    assert_eq!((out.candidates, out.queued, out.deduped), (1, 1, 0));
    assert_eq!(out.items.len(), 1);
    assert!(out.items[0].fresh);

    let items = reuse_items(&m);
    assert_eq!(items.len(), 1, "应有且只有一条 reuse 待审项");
    let meta = &items[0];
    assert_eq!(meta.source, "reuse");
    assert_eq!(meta.pass_id, "reuse");
    assert_eq!(meta.state, ReviewState::PendingReview);
    // 设备号取自卡那一行（反馈表没有设备列）；不编造。
    let card_device: String = m
        .conn()
        .query_row("SELECT device_id FROM card WHERE id=?1", [&card], |r| r.get(0))
        .unwrap();
    assert_eq!(meta.device_id, card_device);

    let item = get(&m, meta.id).unwrap().unwrap();
    // 载荷是**确定性**的：字面逐字节比对（rc-memory 的依赖集里没有 serde_json；
    // 说明里的引号/反斜杠走 `writers_fact::json_string` 的转义）。
    let want_payload = format!(
        "{{\"schema\":\"reuse.v1\",\"card\":\"card:{card}\",\"outcome\":\"failed\",\"event_id\":{event_id},\"event_at_ms\":{NOW},\"events\":1,\"note\":\"答案方向错了 \\\"引号\\\" \\\\ 反斜杠\"}}"
    );
    assert_eq!(item.payload_json, want_payload);
    // 出处轮从卡的 provenance 取（report 先例：目标照原样进 turn_uids_json）。
    assert_eq!(item.turn_uids_json, format!("[\"{TURN}\"]"));
    assert_eq!(item.owner_json, format!("{{\"reporter\":\"{AGENT}\"}}"));
}

/// 钉④（第二档）：`mismatch` 同样入队。
#[test]
fn mismatch_也进复核队列() {
    let dir = tempfile::tempdir().unwrap();
    let m = memory_handle(dir.path());
    let card = seed_card(&m, "复用条件不符乙卡");
    record_reuse(&m, &card, "mismatch", None, NOW, AGENT).unwrap();
    let out = ingest_audit(&m, NOW).unwrap();
    assert_eq!((out.candidates, out.queued), (1, 1));
    let item = get(&m, reuse_items(&m)[0].id).unwrap().unwrap();
    assert!(item.payload_json.contains("\"outcome\":\"mismatch\""), "{}", item.payload_json);
}

/// 钉⑤：`worked` / `partial` / `stale` 都不进复核队列（闭集之外的三档，钉死）。
#[test]
fn worked_partial_stale_不入队() {
    let dir = tempfile::tempdir().unwrap();
    let m = memory_handle(dir.path());
    let card = seed_card(&m, "复用正向丙卡");
    record_reuse(&m, &card, "worked", None, NOW, AGENT).unwrap();
    record_reuse(&m, &card, "worked", None, NOW, AGENT).unwrap();
    record_reuse(&m, &card, "partial", None, NOW, AGENT).unwrap();
    record_reuse(&m, &card, "stale", None, NOW, AGENT).unwrap();
    let out = ingest_audit(&m, NOW).unwrap();
    assert_eq!((out.candidates, out.queued, out.deduped), (0, 0, 0));
    assert!(reuse_items(&m).is_empty(), "正向反馈不许进人工复核队列");
    assert!(AUDIT_OUTCOMES.iter().all(|o| *o != ReuseOutcome::Worked && *o != ReuseOutcome::Partial));
    assert_eq!(WORKED_TIER_MIN, 2);
}

/// 钉⑥：同一张卡重复报 `failed` 不重复入队——第二次 ingest 命中去重（在途项），
/// 即便事件不同（事件 id/说明变了）也只保留一条待审项。
#[test]
fn 同一卡重复报_failed_不重复入队() {
    let dir = tempfile::tempdir().unwrap();
    let m = memory_handle(dir.path());
    let card = seed_card(&m, "复用失败去重丁卡");
    record_reuse(&m, &card, "failed", Some("第一次"), NOW, AGENT).unwrap();
    record_reuse(&m, &card, "failed", Some("第二次"), NOW, AGENT).unwrap();
    assert_eq!(audit_candidates(&m).unwrap().len(), 1, "每卡一条候选（取最新那条）");

    let first = ingest_audit(&m, NOW).unwrap();
    assert_eq!((first.candidates, first.queued, first.deduped), (1, 1, 0));
    let again = ingest_audit(&m, NOW).unwrap();
    assert_eq!((again.candidates, again.queued, again.deduped), (1, 0, 1), "重复报不许新建");
    assert_eq!(reuse_items(&m).len(), 1, "库里只许有一条 reuse 待审项");
    // 第二次的记账指向同一条 item（不是新建了一条同载荷的）。
    assert_eq!(again.items[0].review_item_id, first.items[0].review_item_id);
}

/// 终态不占位：人工判完（rejected）之后，**同一条事件**再 ingest 命中的是载荷去重
/// （`fresh=false`，不新建行），而**新的一条** failed 事件才会重新开一条。
#[test]
fn 终态之后同载荷不新建_新事件可重开() {
    let dir = tempfile::tempdir().unwrap();
    let m = memory_handle(dir.path());
    let card = seed_card(&m, "复用失败终态戊卡");
    record_reuse(&m, &card, "failed", None, NOW, AGENT).unwrap();
    let out = ingest_audit(&m, NOW).unwrap();
    let id = out.items[0].review_item_id;
    rc_memory::review::record_decision(
        &m,
        id,
        &rc_memory::review::Decision {
            stage: rc_memory::review::ReviewStage::Human,
            verdict: rc_memory::review::ReviewVerdict::Reject,
            reason_code: "wrong_card".into(),
            rules_version: "review-gate-v0.1".into(),
            model: String::new(),
            prompt_version: String::new(),
            input_sha256: String::new(),
            review_input_sha256: String::new(),
            decider: rc_memory::review::Decider::Human,
            detail: "operator".into(),
        },
        NOW,
    )
    .unwrap();
    // 同一条事件：载荷相同 ⇒ 不新建（仍是一条 rejected 行）。
    let same = ingest_audit(&m, NOW).unwrap();
    assert_eq!((same.queued, same.deduped), (0, 1));
    assert_eq!(reuse_items(&m).len(), 1);
    // 新的一条 failed（说明不同 ⇒ 载荷不同）⇒ 重新开一条 pending。
    record_reuse(&m, &card, "failed", Some("又坏了"), NOW, AGENT).unwrap();
    let fresh = ingest_audit(&m, NOW).unwrap();
    assert_eq!((fresh.candidates, fresh.queued), (1, 1));
    assert_eq!(reuse_items(&m).len(), 2);
    assert!(reuse_items(&m).iter().any(|i| i.state == ReviewState::PendingReview));
}

/// 缺表（刀 A 之前的只读旧副本）⇒ 零候选、不入队。
#[test]
fn 缺复用表等于零候选() {
    let dir = tempfile::tempdir().unwrap();
    let m = memory_handle(dir.path());
    m.conn().execute_batch("DROP TABLE card_reuse_event").unwrap();
    let out = ingest_audit(&m, NOW).unwrap();
    assert_eq!((out.candidates, out.queued, out.deduped), (0, 0, 0));
    assert!(reuse_items(&m).is_empty());
}
