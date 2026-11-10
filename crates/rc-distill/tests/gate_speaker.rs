//! C1：闸的身份轴与名字命中（`gate.rs` 第 2/3 支与 alias 词边界）。
//!
//! 三条判据各一条用例：
//! - **名字按词出现**：短别名（ASCII 子串、单个汉字）不算"主语名在证据里"——否则这一支
//!   对任意正文恒真，闸等于不开；
//! - **alias 子串**：`lin` 命中 `lincoln` 的那一格必须被词边界挡住；
//! - **身份轴**：`batch.source_agent` 与 `owner.owner_agent_id` 不同时，第 5 支按
//!   `source_agent` 判（提示词与会话自锚用的都是它）。
//!
//! 夹具与 `gate_subject.rs` 同形（真导入转录 + 真入队 `Item`，锚/归属/提及直插），
//! 只多一格：`payload` 的 `batch.source_agent` 可换。
use rc_distill::review::{gate, GateOutcome};
use rc_memory::review::{enqueue, get, Item, NewItem};
use rc_store::Root;
use serde_json::{json, Value};

mod common;
use common::seed_bodies;

const SESSION: &str = "sess-batch-A";
const DEVICE: &str = "dev-batch-A";
const NOW_MS: i64 = 1_800_000_000_000;

/// 正文里既没有 `Lin Wei`，也没有 `曹操`：只剩引文与数字。
const QUOTE: &str = "把发布推迟到 12 月 3 日";
const FILLER_BODY: &str = "先对一下本周的进度";
/// `lin` 是 `lincoln` 的子串，但不是它的一个词。
const ASCII_SUBSTRING_BODY: &str = "lincoln说下周把发布推迟到 12 月 3 日";
/// `曹` 是 `曹操` 的第一个字，但不是"张三"。
const SINGLE_CHAR_BODY: &str = "曹操说下周把发布推迟到 12 月 3 日";
/// 只有引文、没有「本 agent 的名字」的正文：身份轴用例靠它把第 2 支挡在门外。
const QUOTE_BODY: &str = "把发布推迟到 12 月 3 日，先对一下本周的进度";

fn owner() -> Value {
    json!({
        "owner_principal_id": null,
        "owner_agent_id": "agent-batch-A",
        "visibility": "private_agent",
        "retention": "default",
        "scope_kind": "project",
        "scope_key": SESSION
    })
}

fn payload_value(pass: &str, family: &str, turns: &[String], items: Value, source: &str) -> Value {
    json!({
        "schema": "review.pass.v1",
        "pass_id": pass,
        "family": family,
        "run_id": "run-gate-speaker-test",
        "batch": {
            "turn_uids": turns,
            "source_agent": source,
            "device_id": DEVICE,
            "session_key": SESSION,
            "source_ms": 1_000
        },
        "owner": owner(),
        "items": items
    })
}

fn enqueue_item(root: &Root, turns: &[String], payload: &Value) -> Item {
    let outcome = enqueue(
        root.memory(),
        &NewItem {
            source: "distill".to_string(),
            pass_id: "fact".to_string(),
            family: "decision".to_string(),
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

fn fact_item(root: &Root, uid: &str, subject: &str, source: &str) -> Item {
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
        source,
    );
    enqueue_item(root, &turns, &payload)
}

/// 锚行 + 别名 + 归属行。`scope_key` 与 `owner` 块一致，归属 agent 是本行 owner。
fn insert_anchor(root: &Root, id: &str, kind: &str, canonical: &str, aliases: &[&str]) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                                status, created_at_ms, updated_at_ms, norm_version)
             VALUES(?1, ?2, ?3, 'real', 'project', 'p', 'active', 1, 1, 1)",
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
                    'default', 'project', ?2, 'agent-batch-A', ?3, 1)",
            rusqlite::params![id, SESSION, DEVICE],
        )
        .unwrap();
}

/// 证据放在 agent 轮：`seed_bodies` 按 i 交替造 human/agent，第 1 轮是 agent。
fn setup_agent(body: &str) -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[FILLER_BODY, body], 1_000);
    (dir, root, uids)
}

fn run(root: &Root, item: &Item) -> GateOutcome {
    gate(root.memory(), root.transcript(), item, NOW_MS).unwrap()
}

// ── alias 命中加词边界 ───────────────────────────────────────────────────────

/// ASCII 别名 `lin` 出现在 `lincoln` 里：不是"这个名字被提到"，不许放行第 2 支。
/// BASE 上 `body.contains("lin")` 恒真 ⇒ 这条用例在 BASE 上失败（放行）。
#[test]
fn ascii别名只命中更长词的子串_不放行() {
    let (_dir, root, uids) = setup_agent(ASCII_SUBSTRING_BODY);
    insert_anchor(&root, "anchor-lin", "person", "Lin Wei", &["lin"]);
    let item = fact_item(&root, &uids[1], "anchor-lin", "agent-batch-A");
    let outcome = run(&root, &item);
    assert!(
        !outcome.passed && outcome.code == Some("subject_not_in_evidence"),
        "`lin` 是 `lincoln` 的子串但不是它的词：必须拒收（code={:?}）",
        outcome.code
    );
}

/// 单个汉字别名 `曹` 命中 `曹操`：单字名在任意中文正文里几乎必然出现，认它等于恒真。
#[test]
fn 单字汉字别名_不放行() {
    let (_dir, root, uids) = setup_agent(SINGLE_CHAR_BODY);
    insert_anchor(&root, "anchor-demo", "person", "张三", &["曹"]);
    let item = fact_item(&root, &uids[1], "anchor-demo", "agent-batch-A");
    let outcome = run(&root, &item);
    assert!(
        !outcome.passed && outcome.code == Some("subject_not_in_evidence"),
        "单字别名不许命中任意正文（code={:?}）",
        outcome.code
    );
}

/// 对照：同一个 ASCII 别名**成词**出现时仍放行（词边界不是"一律不认"）。
#[test]
fn ascii别名成词出现_仍放行() {
    let (_dir, root, uids) = setup_agent("lin 说下周把发布推迟到 12 月 3 日");
    insert_anchor(&root, "anchor-lin", "person", "Lin Wei", &["lin"]);
    let item = fact_item(&root, &uids[1], "anchor-lin", "agent-batch-A");
    let outcome = run(&root, &item);
    assert!(outcome.passed, "成词命中的别名照常放行（code={:?}）", outcome.code);
}

/// 对照：两个汉字的别名仍按子串认（中文没有词边界，别把正常的别名判死）。
#[test]
fn 两字汉字别名_子串出现_仍放行() {
    let (_dir, root, uids) = setup_agent("小明把发布推迟到 12 月 3 日之前说清楚");
    insert_anchor(&root, "anchor-ming", "person", "张三", &["小明"]);
    let item = fact_item(&root, &uids[1], "anchor-ming", "agent-batch-A");
    let outcome = run(&root, &item);
    assert!(outcome.passed, "两字别名按子串判（code={:?}）", outcome.code);
}

// ── 身份轴：source_agent vs owner_agent_id ───────────────────────────────────

/// 第 5 支判"主语 == 本 agent 锚"时读 `batch.source_agent`。
///
/// 夹具把两列**故意分开**：`source_agent=agent-src`（提示词与会话自锚用它），
/// `owner.owner_agent_id=agent-batch-A`（归属/可见性用它）。主语锚是 `agent-src` 那一格：
/// BASE 上按 owner 名比 ⇒ 判不出来 ⇒ 误拒；C1 按 source_agent 判 ⇒ 放行。
#[test]
fn 第五支按source_agent判本agent锚() {
    let (_dir, root, uids) = setup_agent(QUOTE_BODY);
    insert_anchor(&root, "anchor-agent-src", "agent", "agent-src", &[]);
    let item = fact_item(&root, &uids[1], "anchor-agent-src", "agent-src");
    let outcome = run(&root, &item);
    assert!(
        outcome.passed,
        "身份轴是 source_agent：主语锚就是本 agent 那一格（code={:?}）",
        outcome.code
    );
}

/// 对照：`owner_agent_id` 同名的锚**不再**因为 owner 名相同而被当成"本 agent"。
///
/// 这一格钉住"哪一列是权威"：owner 名相同但 source_agent 不同 ⇒ 不是本 agent 的锚，
/// 名字又不在正文里 ⇒ 拒收。
#[test]
fn owner名字相同但source_agent不同_不放行第5支() {
    let (_dir, root, uids) = setup_agent(QUOTE_BODY);
    insert_anchor(&root, "anchor-owner-name", "agent", "agent-batch-A", &[]);
    let item = fact_item(&root, &uids[1], "anchor-owner-name", "agent-src");
    let outcome = run(&root, &item);
    assert!(
        !outcome.passed && outcome.code == Some("subject_not_in_evidence"),
        "owner 名不是身份轴：只有 source_agent 那一格算本 agent（code={:?}）",
        outcome.code
    );
}

/// C2：`time_text` 必须接地。它不再由契约拒收（形状校验只查可空），而是闸的第 9 步：
/// 非空短语既不是 `quote` 的子串、也不在任何证据轮正文里 ⇒ `time_text_ungrounded`。
#[test]
fn time_text不在证据里_闸拒且码是time_text_ungrounded() {
    let (_dir, root, uids) = setup_agent(QUOTE_BODY);
    insert_anchor(&root, "anchor-agent-src", "agent", "agent-batch-A", &[]);
    let item = fact_item_time(&root, &uids[1], "anchor-agent-src", "agent-batch-A", "明天上午");
    let outcome = run(&root, &item);
    assert!(
        !outcome.passed && outcome.code == Some("time_text_ungrounded"),
        "编造的时间短语必须由闸拒（code={:?}）",
        outcome.code
    );
}

/// 同一条草稿，`time_text` 换成正文里的短语 ⇒ 放行。
#[test]
fn time_text在正文里_闸放行() {
    let (_dir, root, uids) = setup_agent(QUOTE_BODY);
    insert_anchor(&root, "anchor-agent-src", "agent", "agent-batch-A", &[]);
    let item = fact_item_time(&root, &uids[1], "anchor-agent-src", "agent-batch-A", "12 月 3 日");
    let outcome = run(&root, &item);
    assert!(outcome.passed, "正文里的短语不该被拒（code={:?}）", outcome.code);
}

/// 与 `fact_item` 同形，只把 `time_text` 变成参数（C2 用例需要换短语）。
fn fact_item_time(root: &Root, uid: &str, subject: &str, source: &str, time_text: &str) -> Item {
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
            "time_text": time_text,
            "quote": QUOTE,
            "evidence": [{"turn_uid": uid, "anchor_role": "agent"}],
            "text": "把发布推迟到 12 月 3 日"
        }]),
        source,
    );
    enqueue_item(root, &turns, &payload)
}
