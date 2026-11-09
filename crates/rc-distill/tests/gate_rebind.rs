//! A-rebind（工单 2026-10-29-r2-1-rebind，任务 B）：event 的 actor 程序回填。
//!
//! 台账 §10.14 / item 141：证据轮全是 agent 轮、正文里只有"我/实施者"时，模型会把 actor
//! 填成正文里出现的某个工具/包名（`rc-distill`），闸 6c 于是整条拒——14 条真事件就此丢了。
//! 裁定（2026-10-29）收紧后采纳：**三条同时成立**才回填成会话自锚，否则保持原来的整条拒。
//!
//! 本文件钉三个格 + 一条"fact 不动"：
//! 1. agent 轮 + 唯一自锚 + actor 不在正文 ⇒ 放行（回填在写侧，见 `review_run.rs`）；
//! 2. agent 轮 + 多个 person 锚、没有唯一自锚 ⇒ 仍拒（不猜）；
//! 3. 证据含 human 轮 ⇒ 不回填、仍拒；
//! 4. 同一条 fact 载荷 ⇒ fact 的 6c 一个字不变（回填只属于 event）。
//!
//! 夹具与 `gate_subject.rs` 同一条真链路：`seed_bodies` 导轮、`enqueue` + `get` 造 `Item`、
//! 锚/归属直插（按 DDL 必填列）。`seed_bodies` 按序交替 human/agent，所以被测证据一律用
//! `uids[1]`（agent 轮）。

use rc_distill::review::{gate, GateOutcome};
use rc_memory::review::{enqueue, get, Item, NewItem};
use rc_store::Root;
use serde_json::{json, Value};

mod common;
use common::seed_bodies;

const SESSION: &str = "sess-batch-A";
const DEVICE: &str = "dev-batch-A";
const NOW_MS: i64 = 1_800_000_000_000;

/// 陪衬轮（第 0 轮 human）：让 agent 轮排在第 2 位。
const FILLER_BODY: &str = "先对一下本周的进度";
/// agent 轮正文：没有行动者姓名，也没有引文之外的东西。
const NO_NAME_BODY: &str = "下周把发布推迟到 12 月 3 日";
const QUOTE: &str = "把发布推迟到 12 月 3 日";
/// 模型选错的那一格：正文里出现过的包名（item 141 的形状）。
const PACKAGE: &str = "rc-distill";

fn owner() -> Value {
    json!({
        "owner_principal_id": null,
        "owner_agent_id": "agent-batch-A",
        "visibility": "private_agent",
        "retention": "default",
        "scope_kind": "project",
        "scope_key": "sess-batch-A"
    })
}

fn payload_value(pass: &str, family: &str, turns: &[String], items: Value) -> Value {
    json!({
        "schema": "review.pass.v1",
        "pass_id": pass,
        "family": family,
        "run_id": "run-gate-rebind-test",
        "batch": {
            "turn_uids": turns,
            "source_agent": "agent-batch-A",
            "device_id": DEVICE,
            "session_key": SESSION,
            "source_ms": 1_000
        },
        "owner": owner(),
        "items": items
    })
}

fn enqueue_item(
    root: &Root,
    pass: &str,
    family: &str,
    turns: &[String],
    payload: &Value,
) -> Item {
    let outcome = enqueue(
        root.memory(),
        &NewItem {
            source: "distill".to_string(),
            pass_id: pass.to_string(),
            family: family.to_string(),
            session_key: SESSION.to_string(),
            device_id: DEVICE.to_string(),
            turn_uids_json: serde_json::to_string(turns).unwrap(),
            owner_json: owner().to_string(),
            payload_json: payload.to_string(),
            batch_key: String::new(),
        },
        10_000,
    )
    .unwrap();
    get(root.memory(), outcome.id).unwrap().unwrap()
}

/// 一条 event 草稿。`evidence` 由调用方给（它决定"全 agent"还是"含 human"）。
fn event_item_with(root: &Root, turns: &[String], subject: &str, evidence: &[(&str, &str)]) -> Item {
    let evidence: Vec<Value> = evidence
        .iter()
        .map(|(uid, role)| json!({"turn_uid": uid, "anchor_role": role}))
        .collect();
    let mut payload = payload_value(
        "event",
        "",
        turns,
        json!([{
            "subject_anchor_id": subject,
            "object_anchor_id": null,
            "predicate": "action",
            "value": null,
            "category": "event",
            "mode": "stated",
            "time_text": null,
            "quote": QUOTE,
            "evidence": evidence,
            "text": "把发布推迟到 12 月 3 日"
        }]),
    );
    payload["extras"] = json!([{
        "participants": [subject],
        "value_json": null,
        "place_anchor_id": null
    }]);
    enqueue_item(root, "event", "", turns, &payload)
}

fn event_item(root: &Root, uid: &str, subject: &str) -> Item {
    let turns = vec![uid.to_string()];
    event_item_with(root, &turns, subject, &[(uid, "agent")])
}

fn fact_item(root: &Root, uid: &str, subject: &str) -> Item {
    let turns = vec![uid.to_string()];
    let payload = payload_value(
        "fact",
        "decision",
        &turns,
        json!([{
            "subject_anchor_id": subject,
            "object_anchor_id": null,
            "predicate": "decide",
            "value": null,
            "category": "decision",
            "mode": "stated",
            "time_text": "12 月 3 日",
            "quote": QUOTE,
            "evidence": [{"turn_uid": uid, "anchor_role": "agent"}],
            "text": "把发布推迟到 12 月 3 日"
        }]),
    );
    enqueue_item(root, "fact", "decision", &turns, &payload)
}

/// 锚行 + 别名 + 归属行。`kind` 由调用方给（agent 自锚与包名锚只差这一格）。
fn insert_anchor(root: &Root, id: &str, kind: &str, canonical: &str, aliases: &[&str]) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                                status, created_at_ms, updated_at_ms, norm_version)
             VALUES(?1, ?2, ?3, 'real', 'project', 'sess-batch-A', 'active', 1, 1, 1)",
            rusqlite::params![id, kind, canonical],
        )
        .unwrap();
    for alias in aliases {
        root.memory()
            .conn()
            .execute(
                "INSERT INTO alias(anchor_id, alias) VALUES(?1, ?2)",
                rusqlite::params![id, alias],
            )
            .unwrap();
    }
    root.memory()
        .conn()
        .execute(
            "INSERT INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                        created_by_agent, visibility, retention, scope_kind,
                                        scope_key, source_agent, device_id, created_at_ms)
             VALUES('anchor', ?1, NULL, 'agent-batch-A', 'agent-batch-A', 'private_agent',
                    'default', 'project', 'sess-batch-A', 'agent-batch-A', ?2, 1)",
            rusqlite::params![id, DEVICE],
        )
        .unwrap();
}

/// 会话自锚：`kind='agent'`、`canonical` = 会话的 `source_agent`（`append_agent_anchor`
/// 盖章的那一格，与 `speaker_ref` 的 `[self]` 同一套判据）。
fn insert_self_anchor(root: &Root) {
    insert_anchor(root, "anchor-self", "agent", "agent-batch-A", &[]);
}

fn setup_agent() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[FILLER_BODY, NO_NAME_BODY], 1_000);
    (dir, root, uids)
}

fn run(root: &Root, item: &Item) -> GateOutcome {
    gate(root.memory(), root.transcript(), item, NOW_MS).unwrap()
}

// ── 三支 ──────────────────────────────────────────────────────────────────────

/// ① agent 轮 + 唯一自锚 + actor 不在正文 ⇒ 放行（回填）。
#[test]
fn event_agent轮_actor不在正文_唯一自锚时放行() {
    let (_dir, root, uids) = setup_agent();
    insert_anchor(&root, "anchor-pkg", "work", PACKAGE, &[]);
    insert_self_anchor(&root);
    let item = event_item(&root, &uids[1], "anchor-pkg");
    let outcome = run(&root, &item);
    assert!(outcome.passed, "应当由 A-rebind 放行（code={:?}）", outcome.code);
}

/// 自锚不是唯一（或被挡在候选池外）⇒ 不猜。这里用"多个 person 锚、没有 agent 自锚"的
/// 形状：池里有 2 个 person 候选，却没有可回填的唯一会话 agent 锚 ⇒ 仍拒。
#[test]
fn event_agent轮_多个person锚且无唯一自锚时仍拒() {
    let (_dir, root, uids) = setup_agent();
    insert_anchor(&root, "anchor-pkg", "work", PACKAGE, &[]);
    insert_anchor(&root, "anchor-cao", "person", "张三", &[]);
    insert_anchor(&root, "anchor-li", "person", "李四", &[]);
    let item = event_item(&root, &uids[1], "anchor-pkg");
    let outcome = run(&root, &item);
    assert!(!outcome.passed, "没有唯一的本会话 agent 锚 ⇒ 不猜，仍拒");
    assert_eq!(outcome.code, Some("subject_not_in_evidence"));
}

/// 多个 agent 锚同名（脏数据/手写行）⇒ 也不猜：唯一性是判据的一部分。
#[test]
fn event_agent轮_同名agent锚有两格时仍拒() {
    let (_dir, root, uids) = setup_agent();
    insert_anchor(&root, "anchor-pkg", "work", PACKAGE, &[]);
    insert_self_anchor(&root);
    insert_anchor(&root, "anchor-self-2", "agent", "agent-batch-A", &[]);
    let item = event_item(&root, &uids[1], "anchor-pkg");
    assert_eq!(run(&root, &item).code, Some("subject_not_in_evidence"));
}

/// ③ 证据含 human 轮 ⇒ 不回填、仍拒。
///
/// human 轮的身份轴是主人锚，"agent 轮的行动者是该 agent"这条确定性事实在混证据里不成立。
/// 身份未知时 6c 对 human 轮是 fail-open，所以这里给会话盖章（I2），让这一格真的走到判定。
#[test]
fn event_证据含human轮时不回填_仍拒() {
    let (_dir, root, uids) = setup_agent();
    insert_anchor(&root, "anchor-pkg", "work", PACKAGE, &[]);
    insert_self_anchor(&root);
    claim_principal(&root, "p-cao", Some("张三"));
    let turns = vec![uids[0].clone(), uids[1].clone()];
    let item = event_item_with(
        &root,
        &turns,
        "anchor-pkg",
        &[(&uids[0], "human"), (&uids[1], "agent")],
    );
    let outcome = run(&root, &item);
    assert!(!outcome.passed, "证据里有 human 轮 ⇒ 不回填");
    assert_eq!(outcome.code, Some("subject_not_in_evidence"));
}

/// ④ 只改 event：同一条 fact 载荷（同样的 agent 轮、同样的包名 actor）仍然拒。
#[test]
fn fact_的6c不受回填影响_仍然拒() {
    let (_dir, root, uids) = setup_agent();
    insert_anchor(&root, "anchor-pkg", "work", PACKAGE, &[]);
    insert_self_anchor(&root);
    let item = fact_item(&root, &uids[1], "anchor-pkg");
    let outcome = run(&root, &item);
    assert!(!outcome.passed, "A-rebind 只改 event 的 actor 闸，fact 一个字不动");
    assert_eq!(outcome.code, Some("subject_not_in_evidence"));
}

/// 会话盖章（`resolve_principal` 读的就是这一条）。
fn claim_principal(root: &Root, principal_id: &str, principal_name: Option<&str>) {
    rc_ledger::transcript::claim_principal(
        root.transcript(),
        rc_ledger::transcript::NewPrincipalClaim {
            session_key: SESSION,
            device_id: DEVICE,
            principal_id,
            principal_name,
            source: "human_claim",
            claimed_by: "agent-batch-A",
            reason: None,
        },
        10_000,
    )
    .unwrap();
}
