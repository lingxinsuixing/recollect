//! 逐条硬检第一刀：`subject_not_in_evidence`（工单 `2026-10-28-gate-subject`）。
//!
//! 夹具走真链路：`seed_bodies` 把轮真导入转录库，`rc_memory::review::enqueue` + `get`
//! 造出与生产同形的 `Item`；锚/别名与 `mention.speaker_anchor_id` 用测试内直插（按 DDL
//! 必填列），不为测试改生产 API。每条用例只制造一个违规，断言失败码。
//!
//! 六个放行/拒绝格各有一条用例，测试名即读数（判据 5 的对照表）：
//! - `主语名逐字出现在证据里_放行`
//! - `只有别名出现在证据里_放行`
//! - `主语名与别名都不出现_拒收`
//! - `证据轮说话人就是主语锚_名字不在正文也放行`
//! - `主语是代词_放行`
//! - `锚不可见仍先报_anchor_not_visible`（这一码不掩盖更早的判定）
//!
//! 还有一格是 fail-open 本身：`human轮证据_主语不在正文也放行`。规则对 human 轮
//! 不判主语（说话人身份等身份层），所以上面六个判据格的证据轮一律用第 2 轮（agent）；
//! 用 human 轮的话，放行与拒收都读不到规则本身。
//!
//! I2 起 human 轮分成两半（`human轮*` 四条用例钉住）：**有 claim**（`session_principal_claim`
//! 里真有章）时不再自动放行，按常规三支 + "主语锚 == 主人锚"判；**没有 claim**、或章是
//! `unknown` 时维持 I1 的 fail-open。`sessions.owner_principal_id` 的回落列不算 claim——
//! 那是 I1 的读回落，不是盖章。

use rc_distill::review::{gate, GateOutcome};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::review::{enqueue, get, Item, NewItem};
use rc_memory::{Scope, ScopeKind};
use rc_store::Root;
use serde_json::{json, Value};

mod common;
use common::seed_bodies;

const SESSION: &str = "sess-batch-A";
const DEVICE: &str = "dev-batch-A";
const NOW_MS: i64 = 1_800_000_000_000;

/// 主语名逐字出现；正文里同时有引文与数字。
const NAME_BODY: &str = "张三说下周把发布推迟到 12 月 3 日";
/// 只有别名"小陈"出现（canonical"张三"不在正文里）。
const ALIAS_BODY: &str = "小陈说下周把发布推迟到 12 月 3 日";
/// 名字与别名都不在正文里（只有引文）。
const NO_NAME_BODY: &str = "下周把发布推迟到 12 月 3 日";

const QUOTE: &str = "把发布推迟到 12 月 3 日";
/// 对照组的第一轮：human 轮，不进证据。规则对 human 轮 fail-open，所以放行/拒收的
/// 对照格必须把证据放在**第 2 轮（agent）**，否则读到的是 fail-open 而不是规则本身。
const FILLER_BODY: &str = "先对一下本周的进度";

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
        "run_id": "run-gate-subject-test",
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

fn fact_item(root: &Root, uid: &str, subject: &str, quote: &str) -> Item {
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
            "quote": quote,
            "evidence": [{"turn_uid": uid, "anchor_role": "human"}],
            "text": "把发布推迟到 12 月 3 日"
        }]),
    );
    enqueue_item(root, "fact", "decision", &turns, &payload)
}

fn event_item(root: &Root, uid: &str, subject: &str, quote: &str) -> Item {
    let turns = vec![uid.to_string()];
    let mut payload = payload_value(
        "event",
        "",
        &turns,
        json!([{
            "subject_anchor_id": subject,
            "object_anchor_id": null,
            "predicate": "action",
            "value": null,
            "category": "event",
            "mode": "stated",
            "time_text": "今天",
            "quote": quote,
            "evidence": [{"turn_uid": uid, "anchor_role": "human"}],
            "text": "把发布推迟到 12 月 3 日"
        }]),
    );
    payload["extras"] = json!([{
        "participants": [subject],
        "value_json": null,
        "place_anchor_id": null
    }]);
    enqueue_item(root, "event", "", &turns, &payload)
}

/// 锚行 + 归属行。`canonical` 是名字本身（不是 id）——这正是本条规则要读的那一格。
fn insert_anchor(
    root: &Root,
    id: &str,
    canonical: &str,
    aliases: &[&str],
    owner_agent: &str,
    visibility: &str,
) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                                status, created_at_ms, updated_at_ms, norm_version)
             VALUES(?1, 'person', ?2, 'real', 'project', 'p', 'active', 1, 1, 1)",
            rusqlite::params![id, canonical],
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
             VALUES('anchor', ?1, NULL, ?2, ?2, ?3, 'default', 'project', 'sess-batch-A',
                    ?2, ?4, 1)",
            rusqlite::params![id, owner_agent, visibility, DEVICE],
        )
        .unwrap();
}

/// 本行 owner 自己可见的锚。
fn insert_visible_anchor(root: &Root, id: &str, canonical: &str, aliases: &[&str]) {
    insert_anchor(root, id, canonical, aliases, "agent-batch-A", "private_agent");
}

/// 这一轮的说话人就是该锚：`mention.speaker_anchor_id` 是唯一写着"谁说的"的列。
fn insert_speaker_mention(root: &Root, turn_uid: &str, anchor_id: &str) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO mention(id, anchor_id, turn_uid, surface, span_start, span_end,
                                 speaker_anchor_id, discourse_role, resolution, confidence,
                                 source_agent, device_id, created_at_ms, norm_version)
             VALUES(?1, ?2, ?3, '我', NULL, NULL, ?2, 'speaker', 'resolved', 0.9,
                    'agent-batch-A', ?4, 1, 1)",
            rusqlite::params![format!("mention-{turn_uid}"), anchor_id, turn_uid, DEVICE],
        )
        .unwrap();
}

/// 单轮 human 夹具：`seed_bodies` 的第 0 轮是 human。
fn setup(body: &str) -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[body], 1_000);
    (dir, root, uids)
}

/// 证据放在 agent 轮：`seed_bodies` 按 i 交替造 human/agent，第 1 轮是 agent。
/// 返回的 `uids[1]` 才是被测证据轮；`uids[0]` 只是让 agent 轮排在第 2 位的陪衬。
fn setup_agent(body: &str) -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[FILLER_BODY, body], 1_000);
    (dir, root, uids)
}

fn run(root: &Root, item: &Item) -> GateOutcome {
    gate(root.memory(), root.transcript(), item, NOW_MS).unwrap()
}

fn assert_pass(root: &Root, item: &Item) {
    let outcome = run(root, item);
    assert!(outcome.passed, "应当放行（code={:?}）", outcome.code);
    assert_eq!(outcome.code, None);
}

fn assert_code(root: &Root, item: &Item, expected: &'static str) {
    let outcome = run(root, item);
    assert!(!outcome.passed, "应当失败（期望 {expected}）");
    assert_eq!(outcome.code, Some(expected));
}

// ── 放行 / 拒绝六格 ───────────────────────────────────────────────────────────

#[test]
fn 主语名逐字出现在证据里_放行() {
    let (_dir, root, uids) = setup_agent(NAME_BODY);
    insert_visible_anchor(&root, "anchor-demo", "张三", &[]);
    let item = fact_item(&root, &uids[1], "anchor-demo", QUOTE);
    assert_pass(&root, &item);
}

#[test]
fn 只有别名出现在证据里_放行() {
    let (_dir, root, uids) = setup_agent(ALIAS_BODY);
    insert_visible_anchor(&root, "anchor-demo", "张三", &["小陈"]);
    let item = fact_item(&root, &uids[1], "anchor-demo", QUOTE);
    assert_pass(&root, &item);
}

#[test]
fn 主语名与别名都不出现_拒收() {
    let (_dir, root, uids) = setup_agent(NO_NAME_BODY);
    insert_visible_anchor(&root, "anchor-demo", "张三", &["老曹"]);
    let item = fact_item(&root, &uids[1], "anchor-demo", QUOTE);
    assert_code(&root, &item, "subject_not_in_evidence");
}

#[test]
fn 证据轮说话人就是主语锚_名字不在正文也放行() {
    let (_dir, root, uids) = setup_agent(NO_NAME_BODY);
    insert_visible_anchor(&root, "anchor-demo", "张三", &[]);
    insert_speaker_mention(&root, &uids[1], "anchor-demo");
    let item = fact_item(&root, &uids[1], "anchor-demo", QUOTE);
    assert_pass(&root, &item);
}

#[test]
fn 主语是代词_放行() {
    // 正文里没有"我"：放行只能来自"主语不是锚"这一支，不是名字恰好命中。
    let (_dir, root, uids) = setup_agent(NO_NAME_BODY);
    insert_visible_anchor(&root, "anchor-self", "我", &[]);
    let item = fact_item(&root, &uids[1], "anchor-self", QUOTE);
    assert_pass(&root, &item);
}

#[test]
fn human轮证据_主语不在正文也放行() {
    // fail-open 的那一格：human 轮的说话人身份要等身份层，今天没有可判定的锚，
    // 所以"名字不在正文里"也不许拒。这一条与上面的 `_拒收` 只差轮的角色。
    let (_dir, root, uids) = setup(NO_NAME_BODY);
    insert_visible_anchor(&root, "anchor-demo", "张三", &["老曹"]);
    let item = fact_item(&root, &uids[0], "anchor-demo", QUOTE);
    assert_pass(&root, &item);
}

#[test]
fn 锚不可见仍先报_anchor_not_visible() {
    // 名字也不在正文里：若顺序错了，这里会读到 subject_not_in_evidence。
    let (_dir, root, uids) = setup(NO_NAME_BODY);
    insert_anchor(&root, "anchor-private-other", "张三", &["老曹"], "agent-other", "private_agent");
    let item = fact_item(&root, &uids[0], "anchor-private-other", QUOTE);
    assert_code(&root, &item, "anchor_not_visible");
}

// ── 事件路径与"不吞后续检查" ──────────────────────────────────────────────────

#[test]
fn event_主语名不在证据里_同一码拒收() {
    let (_dir, root, uids) = setup_agent(NO_NAME_BODY);
    insert_visible_anchor(&root, "anchor-demo", "张三", &[]);
    let item = event_item(&root, &uids[1], "anchor-demo", QUOTE);
    assert_code(&root, &item, "subject_not_in_evidence");
}

#[test]
fn 主语放行后引文检查照常_quote_not_verbatim() {
    let (_dir, root, uids) = setup(NAME_BODY);
    insert_visible_anchor(&root, "anchor-demo", "张三", &[]);
    let item = fact_item(&root, &uids[0], "anchor-demo", "根本不存在的引文");
    assert_code(&root, &item, "quote_not_verbatim");
}

// ── I2：身份已知的 human 轮不再自动放行 ───────────────────────────────────────

/// 给会话盖章：`resolve_principal` 从 claim 表读到的就是这一条。
fn claim_principal(root: &Root, principal_id: &str, principal_name: Option<&str>) {
    claim_with_source(root, principal_id, principal_name, "human_claim");
}

fn claim_with_source(root: &Root, principal_id: &str, principal_name: Option<&str>, source: &str) {
    rc_ledger::transcript::claim_principal(
        root.transcript(),
        rc_ledger::transcript::NewPrincipalClaim {
            session_key: SESSION,
            device_id: DEVICE,
            principal_id,
            principal_name,
            source,
            claimed_by: "agent-batch-A",
            reason: None,
        },
        10_000,
    )
    .unwrap();
}

/// 主人锚：走生产的 `ensure_principal_anchor`（夹具不自己拼 id——闸门那一支比的正是
/// 这条内容寻址算法算出来的 id）。返回它的 `anchor_id`。
fn insert_principal_anchor(root: &Root, principal_id: &str, name: Option<&str>) -> String {
    let owner = OwnerCtx {
        owner_principal_id: Some(principal_id.to_string()),
        owner_agent_id: "agent-batch-A".to_string(),
        visibility: Visibility::PrivateAgent,
        retention: "default".to_string(),
    };
    let scope = Scope { kind: ScopeKind::Project, key: SESSION.to_string() };
    rc_memory::writers_v4::ensure_principal_anchor(
        root.memory(),
        principal_id,
        name,
        &owner,
        &scope,
        10_000,
    )
    .unwrap()
    .expect("身份已知时必须建锚")
    .id
}

/// 7. 有 claim + human 轮 + 主语就是主人锚、名字不在正文 ⇒ 放行。
///
/// human 轮的"我"现在有可判的一格：说话人 = 会话主人 = 这一格 person 锚。
#[test]
fn 有claim且主语是主人锚_名字不在正文也放行() {
    let (_dir, root, uids) = setup(NO_NAME_BODY);
    claim_principal(&root, "p-cao", Some("张三"));
    let principal_anchor = insert_principal_anchor(&root, "p-cao", Some("张三"));
    let item = fact_item(&root, &uids[0], &principal_anchor, QUOTE);
    assert_pass(&root, &item);
}

/// 8. 有 claim + human 轮 + 主语是**别的** person 锚且名字不在正文 ⇒ 拒。
///
/// 身份已知时 human 轮与 agent 轮同一把尺子：说不出来源的主语不再被放行。
#[test]
fn 有claim且主语是别的person锚_名字不在正文_拒收() {
    let (_dir, root, uids) = setup(NO_NAME_BODY);
    claim_principal(&root, "p-cao", Some("张三"));
    insert_visible_anchor(&root, "anchor-li", "李四", &[]);
    let item = fact_item(&root, &uids[0], "anchor-li", QUOTE);
    assert_code(&root, &item, "subject_not_in_evidence");
}

/// 9. 没有 claim（连回落列都没有）+ human 轮 ⇒ 仍放行：I1 的 fail-open 保留。
///
/// 身份未知时硬拒会砍掉"用户自己的决定"整类，这是刻意的。
#[test]
fn 没有claim的human轮_仍放行() {
    let (_dir, root, uids) = setup(NO_NAME_BODY);
    insert_visible_anchor(&root, "anchor-li", "李四", &[]);
    let item = fact_item(&root, &uids[0], "anchor-li", QUOTE);
    assert_pass(&root, &item);
}

/// 8c. agent 证据 + 主语是**本 agent 锚** + 名字不在正文 ⇒ 放行。
///
/// 与 human 轮的主人对成对：agent 轮的说话人是本 agent，agent 回执"明白…"带出的草稿
/// 主语是 `agent-batch-A`（夹具的 owner_agent_id），正文里当然不会有这个名字。
/// 2026-10-28 冒烟实测：缺这一支时这类草稿被误判成 `subject_not_in_evidence`。
#[test]
fn agent轮_主语是本agent锚_名字不在正文也放行() {
    let (_dir, root, uids) = setup_agent(NO_NAME_BODY);
    // 借用 `insert_anchor` 的可见性/归属行，再把 kind 改成 agent。
    insert_visible_anchor(&root, "anchor-agent", "agent-batch-A", &[]);
    root.memory()
        .conn()
        .execute("UPDATE anchor SET kind='agent' WHERE id='anchor-agent'", [])
        .unwrap();
    let item = fact_item(&root, &uids[1], "anchor-agent", QUOTE);
    assert_pass(&root, &item);
}

///
/// 8b. 有 claim + **混证据**（human 陈述 + agent 回执）+ 主语是主人锚 ⇒ 放行。
///
/// 2026-10-28 冒烟实测的回归：一条 human 决定常带 agent 的回执作第二条证据，
/// 早期实现要求"证据**全是** human"才认人轮，于是把这类正确草稿判成
/// `subject_not_in_evidence`。判据是"任一证据轮是 human"。
#[test]
fn 有claim且混证据_主语是主人锚_放行() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    // 第 0 轮 human 陈述、第 1 轮 agent 回执（seed_bodies 按序交替 human/agent）。
    let uids = seed_bodies(&mut root, &[NO_NAME_BODY, "收到，我记下了"], 1_000);
    claim_principal(&root, "p-cao", Some("张三"));
    let principal_anchor = insert_principal_anchor(&root, "p-cao", Some("张三"));
    let turns = vec![uids[0].clone(), uids[1].clone()];
    let payload = payload_value(
        "fact",
        "decision",
        &turns,
        json!([{
            "subject_anchor_id": principal_anchor,
            "object_anchor_id": null,
            "predicate": "decide",
            "value": null,
            "category": "decision",
            "mode": "stated",
            "time_text": "12 月 3 日",
            "quote": QUOTE,
            "evidence": [
                {"turn_uid": uids[0], "anchor_role": "human"},
                {"turn_uid": uids[1], "anchor_role": "agent"}
            ],
            "text": "把发布推迟到 12 月 3 日"
        }]),
    );
    let item = enqueue_item(&root, "fact", "decision", &turns, &payload);
    assert_pass(&root, &item);
}

/// 10. 章明确写 `unknown` ⇒ 身份未知 ⇒ 仍放行（不建无名用户，也不硬拒）。
#[test]
fn 章是unknown的human轮_仍放行() {
    let (_dir, root, uids) = setup(NO_NAME_BODY);
    claim_with_source(&root, "unknown", None, "unknown");
    insert_visible_anchor(&root, "anchor-li", "李四", &[]);
    let item = fact_item(&root, &uids[0], "anchor-li", QUOTE);
    assert_pass(&root, &item);
}

/// 11. 只有 `sessions.owner_principal_id` 回落列、没有 claim ⇒ 仍放行。
///
/// 工单的收紧口径是"该会话有 claim"：回落列是 I1 的读回落，不是盖章。
#[test]
fn 只有回落列没有claim的human轮_仍放行() {
    let (_dir, root, uids) = setup(NO_NAME_BODY);
    root.transcript()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id='p-legacy'
             WHERE session_key=?1 AND device_id=?2",
            rusqlite::params![SESSION, DEVICE],
        )
        .unwrap();
    insert_visible_anchor(&root, "anchor-li", "李四", &[]);
    let item = fact_item(&root, &uids[0], "anchor-li", QUOTE);
    assert_pass(&root, &item);
}

// ── C1：主语轴只收紧 state 一支 ────────────────────────────────────────────────

/// **阴性对照**：同一份"agent 轮 + 主语是本会话自锚 + 名字不在正文"的形状，fact 与 event
/// 两支在 C1 之后仍照常放行。
///
/// 理由写在闸里：`stated` 不是 mode 轴，fact/event 的收紧会连带砍掉 2026-10-29 的 A-rebind
/// 裁定救回来的那类行。这一条钉的是"降级不许外溢到另外两支"。
#[test]
fn c1只在state一支收紧_fact与event的同形载荷仍放行() {
    let (_dir, root, uids) = setup_agent(NO_NAME_BODY);
    insert_visible_anchor(&root, "anchor-agent", "agent-batch-A", &[]);
    root.memory()
        .conn()
        .execute("UPDATE anchor SET kind='agent' WHERE id='anchor-agent'", [])
        .unwrap();
    assert_pass(&root, &fact_item(&root, &uids[1], "anchor-agent", QUOTE));
    // event 的 time_text 必须接地，所以这一条自己拼载荷（`event_item` 的 time_text 是"今天"）。
    let turns = vec![uids[1].clone()];
    let mut payload = payload_value(
        "event",
        "",
        &turns,
        json!([{
            "subject_anchor_id": "anchor-agent",
            "object_anchor_id": null,
            "predicate": "action",
            "value": null,
            "category": "event",
            "mode": "stated",
            "time_text": "12 月 3 日",
            "quote": QUOTE,
            "evidence": [{"turn_uid": uids[1], "anchor_role": "agent"}],
            "text": "把发布推迟到 12 月 3 日"
        }]),
    );
    payload["extras"] = json!([{
        "participants": ["anchor-agent"],
        "value_json": null,
        "place_anchor_id": null
    }]);
    let item = enqueue_item(&root, "event", "", &turns, &payload);
    assert_pass(&root, &item);
}
