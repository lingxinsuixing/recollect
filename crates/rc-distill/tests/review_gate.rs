//! S3a-2：程序闸的表驱动用例（设计 `2026-10-26-review-queue-s3` §5）。
//!
//! 夹具走真链路：`seed_bodies` 把轮真导入转录库，`rc_memory::review::enqueue` + `get`
//! 造出与生产同形的 `Item`；锚用测试内 `insert_anchor` 直插 `anchor` 表（按 DDL 必填列），
//! 不为测试改生产 API。每条用例只制造一个违规，断言失败码；另有一条钉顺序、一条钉无泄漏。

use rc_distill::review::{gate, GateOutcome};
use rc_memory::review::{enqueue, get, Item, NewItem};
use rc_store::Root;
use serde_json::{json, Value};

mod common;
use common::seed_bodies;

const SESSION: &str = "sess-batch-A";
const DEVICE: &str = "dev-batch-A";
const NOW_MS: i64 = 1_800_000_000_000;

/// 同时含人名、项目名与两个数字串，够 fact/event/naming/time 四组正向用例共用。
const BODY: &str = "张三决定把发布推迟到 12 月 3 日，项目代号极光";

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
        "run_id": "run-gate-test",
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

#[allow(clippy::too_many_arguments)]
fn enqueue_item(
    root: &Root,
    source: &str,
    pass: &str,
    family: &str,
    session: &str,
    device: &str,
    turns: &[String],
    payload: &Value,
) -> Item {
    let outcome = enqueue(
        root.memory(),
        &NewItem {
            source: source.to_string(),
            pass_id: pass.to_string(),
            family: family.to_string(),
            session_key: session.to_string(),
            device_id: device.to_string(),
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

fn fact_item(
    root: &Root,
    uid: &str,
    subject: &str,
    quote: &str,
    text: &str,
) -> Item {
    fact_item_fields(root, uid, subject, quote, text, "decide", "decision", "stated")
}

#[allow(clippy::too_many_arguments)]
fn fact_item_fields(
    root: &Root,
    uid: &str,
    subject: &str,
    quote: &str,
    text: &str,
    predicate: &str,
    category: &str,
    mode: &str,
) -> Item {
    let turns = vec![uid.to_string()];
    let payload = payload_value(
        "fact",
        "decision",
        &turns,
        json!([{
            "subject_anchor_id": subject,
            "object_anchor_id": null,
            "predicate": predicate,
            "value": "把发布推迟",
            "category": category,
            "mode": mode,
            "time_text": "12 月 3 日",
            "quote": quote,
            "evidence": [{"turn_uid": uid, "anchor_role": "human"}],
            "text": text
        }]),
    );
    enqueue_item(root, "distill", "fact", "decision", SESSION, DEVICE, &turns, &payload)
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
            // C2：`time_text` 必须接地（闸第 9 步）；这条夹具的正文里没有「今天」，留空。
            "time_text": null,
            "quote": quote,
            "evidence": [{"turn_uid": uid, "anchor_role": "human"}],
            "text": quote
        }]),
    );
    payload["extras"] = json!([{
        "participants": [subject],
        "value_json": null,
        "place_anchor_id": null
    }]);
    enqueue_item(root, "distill", "event", "", SESSION, DEVICE, &turns, &payload)
}

fn naming_item(root: &Root, uid: &str, canonical: &str, alias: Option<&str>) -> Item {
    naming_item_with_span(root, uid, canonical, alias, None)
}

/// 只有锚、没有 mention：用来钉"canonical 既不在正文、也不等于任何 surface"。
fn naming_anchor_only(root: &Root, uid: &str, canonical: &str) -> Item {
    let turns = vec![uid.to_string()];
    let payload = payload_value(
        "naming",
        "",
        &turns,
        json!({
            "anchors": [{
                "reference": "a1",
                "kind": "person",
                "canonical": canonical,
                "aliases": [],
                "reality_status": "real",
                "evidence": [{"turn_uid": uid, "anchor_role": "human"}],
                "stable": true
            }],
            "mentions": []
        }),
    );
    enqueue_item(root, "distill", "naming", "", SESSION, DEVICE, &turns, &payload)
}

fn naming_item_with_span(
    root: &Root,
    uid: &str,
    canonical: &str,
    alias: Option<&str>,
    span: Option<(i64, i64)>,
) -> Item {
    let turns = vec![uid.to_string()];
    let aliases: Vec<String> = alias.into_iter().map(str::to_string).collect();
    let mut mention = json!({
        "surface": canonical,
        "turn_uid": uid,
        "speaker_ref": null,
        "discourse_role": "speaker",
        "resolution": "resolved",
        "confidence": 0.9
    });
    if let Some((start, end)) = span {
        mention["span_start"] = json!(start);
        mention["span_end"] = json!(end);
    }
    let payload = payload_value(
        "naming",
        "",
        &turns,
        json!({
            "anchors": [{
                "reference": "a1",
                "kind": "person",
                "canonical": canonical,
                "aliases": aliases,
                "reality_status": "real",
                "evidence": [{"turn_uid": uid, "anchor_role": "human"}],
                "stable": true
            }],
            "mentions": [mention]
        }),
    );
    enqueue_item(root, "distill", "naming", "", SESSION, DEVICE, &turns, &payload)
}

fn time_item(
    root: &Root,
    uid: &str,
    start_ms: Option<i64>,
    end_ms: Option<i64>,
    kind: &str,
) -> Item {
    let turns = vec![uid.to_string()];
    let payload = payload_value(
        "time",
        "",
        &turns,
        json!([{
            "turn_uid": uid,
            "text": "12 月 3 日",
            "start_ms": start_ms,
            "end_ms": end_ms,
            "precision": "day",
            "basis": "explicit",
            "kind": kind
        }]),
    );
    enqueue_item(root, "distill", "time", "", SESSION, DEVICE, &turns, &payload)
}

/// 一个**对夹具 owner 可见**的锚：`anchor` 行 + `resource_owner` 行（owner 就是行 owner
/// `agent-batch-A`，private_agent）。没有归属行的锚在新闸下按 fail-closed 拒收（
/// `anchor_not_visible`），所以夹具必须同时写归属——这与生产里命名 pass 的写入形状一致。
///
/// `canonical` 取 `BODY` 里的人名"张三"而不是锚 id：`subject_not_in_evidence` 之后，
/// 主语锚的 canonical/别名必须真出现在证据正文里（本文件的 BODY 里只有"张三"这个名字）。
fn insert_anchor(root: &Root, id: &str, status: &str) {
    insert_owned_anchor(root, id, status, "agent-batch-A", "private_agent", "project", "sess-batch-A");
}

fn insert_owned_anchor(
    root: &Root,
    id: &str,
    status: &str,
    owner_agent: &str,
    visibility: &str,
    scope_kind: &str,
    scope_key: &str,
) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                                status, created_at_ms, updated_at_ms, norm_version)
             VALUES(?1, 'person', '张三', 'real', 'project', 'p', ?2, 1, 1, 1)",
            rusqlite::params![id, status],
        )
        .unwrap();
    root.memory()
        .conn()
        .execute(
            "INSERT INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                        created_by_agent, visibility, retention, scope_kind,
                                        scope_key, source_agent, device_id, created_at_ms)
             VALUES('anchor', ?1, NULL, ?2, ?2, ?3, 'default', ?4, ?5, ?2, ?6, 1)",
            rusqlite::params![id, owner_agent, visibility, scope_kind, scope_key, DEVICE],
        )
        .unwrap();
}

fn run(root: &Root, item: &Item) -> GateOutcome {
    gate(root.memory(), root.transcript(), item, NOW_MS).unwrap()
}

fn assert_code(root: &Root, item: &Item, expected: &'static str) {
    let outcome = run(root, item);
    assert!(!outcome.passed, "应当失败（期望 {expected}）");
    assert_eq!(outcome.code, Some(expected));
}

fn assert_code_is_pass(root: &Root, item: &Item) {
    let outcome = run(root, item);
    assert!(outcome.passed, "应当通过（code={:?}）", outcome.code);
    assert_eq!(outcome.code, None);
}

fn setup() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[BODY], 1_000);
    (dir, root, uids)
}

// ── 正向：四个中心 pass 各一条 ─────────────────────────────────────────────────

#[test]
fn fact_引文逐字且锚存在时通过() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let item = fact_item(
        &root,
        &uids[0],
        "anchor-zhang",
        "把发布推迟到 12 月 3 日",
        "张三决定把发布推迟到 12 月 3 日",
    );
    let outcome = run(&root, &item);
    assert!(outcome.passed, "code={:?}", outcome.code);
    assert_eq!(outcome.code, None);
    assert_eq!(outcome.turns_checked, 1);
    assert_eq!(outcome.items_checked, 1);
}

#[test]
fn event_引文逐字且参与者锚存在时通过() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "provisional");
    let item = event_item(&root, &uids[0], "anchor-zhang", BODY);
    let outcome = run(&root, &item);
    assert!(outcome.passed, "code={:?}", outcome.code);
    assert_eq!(outcome.items_checked, 1);
}

#[test]
fn naming_名字逐字且span在范围内时通过() {
    let (_dir, root, uids) = setup();
    let item = naming_item_with_span(&root, &uids[0], "张三", Some("极光"), Some((0, 2)));
    let outcome = run(&root, &item);
    assert!(outcome.passed, "code={:?}", outcome.code);
    assert_eq!(outcome.items_checked, 2, "1 个锚 + 1 条 mention");
}

#[test]
fn time_区间合法且不在未来时通过() {
    let (_dir, root, uids) = setup();
    // 两个数字都落在 2000–2100 且 start <= end、start 不在未来。
    let item = time_item(
        &root,
        &uids[0],
        Some(1_700_000_000_000),
        Some(1_700_086_400_000),
        "event_time",
    );
    let outcome = run(&root, &item);
    assert!(outcome.passed, "code={:?}", outcome.code);
}

// ── 反向：失败码逐条 ──────────────────────────────────────────────────────────

#[test]
fn payload_shape_pass_id_是_rows_时拒收() {
    let (_dir, root, _uids) = setup();
    let turns: Vec<String> = Vec::new();
    let payload = payload_value("rows", "", &turns, json!([]));
    let item = enqueue_item(&root, "rows", "rows", "", SESSION, DEVICE, &turns, &payload);
    assert_code(&root, &item, "payload_shape");
}

#[test]
fn turn_missing_批内轮不在转录时拒收() {
    let (_dir, root, _uids) = setup();
    let unknown = "a".repeat(64);
    let turns = vec![unknown];
    let payload = payload_value("fact", "decision", &turns, json!([]));
    let item = enqueue_item(&root, "distill", "fact", "decision", SESSION, DEVICE, &turns, &payload);
    assert_code(&root, &item, "turn_missing");
}

#[test]
fn turn_session_mismatch_会话对不上时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let turns = vec![uids[0].clone()];
    let payload = payload_value(
        "fact",
        "decision",
        &turns,
        json!([{
            "subject_anchor_id": "anchor-zhang",
            "object_anchor_id": null,
            "predicate": "decide",
            "value": null,
            "category": "decision",
            "mode": "stated",
            "time_text": null,
            "quote": "把发布推迟到 12 月 3 日",
            "evidence": [{"turn_uid": uids[0], "anchor_role": "human"}],
            "text": "张三决定"
        }]),
    );
    let item = enqueue_item(
        &root,
        "distill",
        "fact",
        "decision",
        "sess-other",
        DEVICE,
        &turns,
        &payload,
    );
    assert_code(&root, &item, "turn_session_mismatch");
}

#[test]
fn turn_tombstoned_墓碑命中时拒收() {
    let (_dir, root, uids) = setup();
    root.transcript()
        .conn()
        .execute(
            "INSERT INTO tombstone(commit_seq, epoch, scope_kind, scope_key, turns_removed,
                                   content_sha256, created_at_ms)
             VALUES(1, 1, 'session', ?1, 1, 'deadbeef', 1)",
            [SESSION],
        )
        .unwrap();
    let turns = vec![uids[0].clone()];
    let payload = payload_value("fact", "decision", &turns, json!([]));
    let item = enqueue_item(&root, "distill", "fact", "decision", SESSION, DEVICE, &turns, &payload);
    assert_code(&root, &item, "turn_tombstoned");
}

#[test]
fn compaction_evidence_证据轮是摘要块时拒收() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &["[[compact_summary]] 张三决定发布新版本"], 1_000);
    let item = fact_item(&root, &uids[0], "anchor-zhang", "张三决定发布", "张三决定发布");
    assert_code(&root, &item, "compaction_evidence");
}

#[test]
fn ref_missing_主体锚不存在时拒收() {
    let (_dir, root, uids) = setup();
    let item = fact_item(
        &root,
        &uids[0],
        "anchor-missing",
        "把发布推迟到 12 月 3 日",
        "张三决定",
    );
    assert_code(&root, &item, "ref_missing");
}

#[test]
fn ref_retired_主体锚已退场时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-old", "retired");
    let item = fact_item(
        &root,
        &uids[0],
        "anchor-old",
        "把发布推迟到 12 月 3 日",
        "张三决定",
    );
    assert_code(&root, &item, "ref_retired");
}

// ── 6b. anchor_not_visible：引用锚必须对该行 owner 可见 ─────────────────────────

/// fact 的主语锚是**另一个 owner 的私有锚**：锚在、活着，但行 owner 看不见它 ⇒
/// `anchor_not_visible`（不是 `ref_missing`，也不是放行）。
#[test]
fn anchor_not_visible_主语是他人的私有锚时拒收() {
    let (_dir, root, uids) = setup();
    insert_owned_anchor(
        &root,
        "anchor-private-other",
        "active",
        "agent-other",
        "private_agent",
        "project",
        "sess-batch-A",
    );
    let item = fact_item(
        &root,
        &uids[0],
        "anchor-private-other",
        "把发布推迟到 12 月 3 日",
        "张三决定",
    );
    assert_code(&root, &item, "anchor_not_visible");
}

/// 客体锚是他人私有锚：与主语同一条规则（两个引用格都要过）。
#[test]
fn anchor_not_visible_客体是他人的私有锚时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    insert_owned_anchor(
        &root,
        "anchor-private-other",
        "active",
        "agent-other",
        "private_agent",
        "project",
        "sess-batch-A",
    );
    let turns = vec![uids[0].clone()];
    let payload = payload_value(
        "fact",
        "decision",
        &turns,
        json!([{
            "subject_anchor_id": "anchor-zhang",
            "object_anchor_id": "anchor-private-other",
            "predicate": "decide",
            "value": "把发布推迟",
            "category": "decision",
            "mode": "stated",
            "time_text": "12 月 3 日",
            "quote": "把发布推迟到 12 月 3 日",
            "evidence": [{"turn_uid": uids[0], "anchor_role": "human"}],
            "text": "张三决定"
        }]),
    );
    let item =
        enqueue_item(&root, "distill", "fact", "decision", SESSION, DEVICE, &turns, &payload);
    assert_code(&root, &item, "anchor_not_visible");
}

/// 别人的锚，但 visibility=project 且 scope 与本行全等 ⇒ 共享可见，照常通过。
/// 这一条挡住"顺手把可见性判成 owner-only"的收紧。
#[test]
fn anchor_共享可见_他人的项目锚通过() {
    let (_dir, root, uids) = setup();
    insert_owned_anchor(
        &root,
        "anchor-shared",
        "active",
        "agent-other",
        "project",
        "project",
        "sess-batch-A",
    );
    let item = fact_item(
        &root,
        &uids[0],
        "anchor-shared",
        "把发布推迟到 12 月 3 日",
        "张三决定",
    );
    assert_code_is_pass(&root, &item);
}

/// 他人的私有锚即使 scope 全等也不可见：private_agent 是 owner-only，scope 抹不平。
#[test]
fn anchor_not_visible_私有锚_scope_相同也拒收() {
    let (_dir, root, uids) = setup();
    insert_owned_anchor(
        &root,
        "anchor-private-same-scope",
        "active",
        "agent-other",
        "private_agent",
        "project",
        "sess-batch-A",
    );
    let item = fact_item(
        &root,
        &uids[0],
        "anchor-private-same-scope",
        "把发布推迟到 12 月 3 日",
        "张三决定",
    );
    assert_code(&root, &item, "anchor_not_visible");
}

/// 事件路径：participants 里的他人私有锚 ⇒ 同一个码（event 的四个引用格共用一条规则）。
#[test]
fn anchor_not_visible_事件参与者是他人的私有锚时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    insert_owned_anchor(
        &root,
        "anchor-private-other",
        "active",
        "agent-other",
        "private_agent",
        "project",
        "sess-batch-A",
    );
    let turns = vec![uids[0].clone()];
    let mut payload = payload_value(
        "event",
        "",
        &turns,
        json!([{
            "subject_anchor_id": "anchor-zhang",
            "object_anchor_id": null,
            "predicate": "action",
            "value": null,
            "category": "event",
            "mode": "stated",
            "time_text": "今天",
            "quote": "把发布推迟到 12 月 3 日",
            "evidence": [{"turn_uid": uids[0], "anchor_role": "human"}],
            "text": "张三决定"
        }]),
    );
    payload["extras"] = json!([{
        "participants": ["anchor-zhang", "anchor-private-other"],
        "value_json": null,
        "place_anchor_id": null
    }]);
    let item = enqueue_item(&root, "distill", "event", "", SESSION, DEVICE, &turns, &payload);
    assert_code(&root, &item, "anchor_not_visible");
}

/// 事件路径的阳性面：共享可见的 place 锚 + 自有参与者 ⇒ 通过。
#[test]
fn anchor_事件引用共享可见锚时通过() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    insert_owned_anchor(
        &root,
        "anchor-place-shared",
        "active",
        "agent-other",
        "project",
        "project",
        "sess-batch-A",
    );
    let turns = vec![uids[0].clone()];
    let mut payload = payload_value(
        "event",
        "",
        &turns,
        json!([{
            "subject_anchor_id": "anchor-zhang",
            "object_anchor_id": null,
            "predicate": "action",
            "value": null,
            "category": "event",
            "mode": "stated",
            // C2：正文里没有「今天」⇒ 留空，避免被闸第 9 步判 time_text_ungrounded。
            "time_text": null,
            "quote": "把发布推迟到 12 月 3 日",
            "evidence": [{"turn_uid": uids[0], "anchor_role": "human"}],
            "text": "张三决定"
        }]),
    );
    payload["extras"] = json!([{
        "participants": ["anchor-zhang"],
        "value_json": null,
        "place_anchor_id": "anchor-place-shared"
    }]);
    let item = enqueue_item(&root, "distill", "event", "", SESSION, DEVICE, &turns, &payload);
    assert_code_is_pass(&root, &item);
}

#[test]
fn quote_not_verbatim_引文不在正文时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let item = fact_item(&root, &uids[0], "anchor-zhang", "根本不存在的引文", "张三决定");
    assert_code(&root, &item, "quote_not_verbatim");
}

#[test]
fn quote_too_long_引文超_120_字时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let quote = "长".repeat(121);
    let item = fact_item(&root, &uids[0], "anchor-zhang", &quote, "张三决定");
    assert_code(&root, &item, "quote_too_long");
}

#[test]
fn ellipsis_引文含省略号时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let item = fact_item(&root, &uids[0], "anchor-zhang", "把发布…日", "张三决定");
    assert_code(&root, &item, "ellipsis");
}

#[test]
fn number_not_grounded_正文数字不在引文里时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let item = fact_item(
        &root,
        &uids[0],
        "anchor-zhang",
        "张三决定把发布推迟",
        "张三决定把发布推迟 12 个版本",
    );
    assert_code(&root, &item, "number_not_grounded");
}

#[test]
fn category_不在七值闭集时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let item = fact_item_fields(
        &root,
        &uids[0],
        "anchor-zhang",
        "把发布推迟到 12 月 3 日",
        "张三决定",
        "decide",
        "bogus",
        "stated",
    );
    assert_code(&root, &item, "category");
}

#[test]
fn predicate_与_family_闭集不符时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let item = fact_item_fields(
        &root,
        &uids[0],
        "anchor-zhang",
        "把发布推迟到 12 月 3 日",
        "张三决定",
        "promise",
        "decision",
        "stated",
    );
    assert_code(&root, &item, "predicate");
}

#[test]
fn mode_不是_stated_时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let item = fact_item_fields(
        &root,
        &uids[0],
        "anchor-zhang",
        "把发布推迟到 12 月 3 日",
        "张三决定",
        "decide",
        "decision",
        "inferred",
    );
    assert_code(&root, &item, "mode");
}

#[test]
fn pronoun_主体名字是代词时拒收() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &["他今天上线了"], 1_000);
    let item = naming_item(&root, &uids[0], "他", None);
    assert_code(&root, &item, "pronoun");
}

#[test]
fn not_verbatim_名字不在该批正文时拒收() {
    let (_dir, root, uids) = setup();
    let item = naming_anchor_only(&root, &uids[0], "李四");
    assert_code(&root, &item, "not_verbatim");
}

#[test]
fn span_out_of_range_mention_越界时拒收() {
    let (_dir, root, uids) = setup();
    let item = naming_item_with_span(&root, &uids[0], "张三", None, Some((0, 10_000)));
    assert_code(&root, &item, "span_out_of_range");
}

#[test]
fn time_range_早于_2000_年时拒收() {
    let (_dir, root, uids) = setup();
    let item = time_item(&root, &uids[0], Some(1_000), None, "validity");
    assert_code(&root, &item, "time_range");
}

#[test]
fn time_future_事件时间在未来时拒收() {
    let (_dir, root, uids) = setup();
    let item = time_item(
        &root,
        &uids[0],
        Some(NOW_MS + 1_000),
        Some(NOW_MS + 2_000),
        "event_time",
    );
    assert_code(&root, &item, "time_future");
}

#[test]
fn valid_range_start_大于_end_时拒收() {
    let (_dir, root, uids) = setup();
    let item = time_item(
        &root,
        &uids[0],
        Some(1_700_086_400_000),
        Some(1_700_000_000_000),
        "validity",
    );
    assert_code(&root, &item, "valid_range");
}

#[test]
fn kind_mismatch_time_kind_不在闭集时拒收() {
    let (_dir, root, uids) = setup();
    let item = time_item(&root, &uids[0], None, None, "bogus");
    assert_code(&root, &item, "kind_mismatch");
}

// ── 判定顺序与无泄漏 ──────────────────────────────────────────────────────────

#[test]
fn 双错载荷返回顺序靠前的码() {
    let (_dir, root, uids) = setup();
    // 同时犯：ref_missing（组 6）与 quote_too_long（组 7）。前者必须赢。
    let quote = "长".repeat(121);
    let item = fact_item(&root, &uids[0], "anchor-missing", &quote, "张三决定");
    assert_code(&root, &item, "ref_missing");
}

#[test]
fn 失败码格式化后不含_turn_uid_与引文() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let probe = "探针唯一串ABC987654";
    let item = fact_item(&root, &uids[0], "anchor-zhang", probe, probe);
    let outcome = run(&root, &item);
    assert_eq!(outcome.code, Some("quote_not_verbatim"));
    let rendered = format!("{} {:?}", outcome.code.unwrap(), outcome);
    assert!(!rendered.contains(&uids[0]), "失败码不得携带 turn_uid");
    assert!(!rendered.contains(probe), "失败码不得携带引文片段");
    assert!(!rendered.contains(BODY), "失败码不得携带轮正文");
}

#[test]
fn 同一输入两次判定一致() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let item = fact_item(
        &root,
        &uids[0],
        "anchor-zhang",
        "把发布推迟到 12 月 3 日",
        "张三决定把发布推迟到 12 月 3 日",
    );
    assert_eq!(run(&root, &item), run(&root, &item));
}

// ── H1c：批次身份 ────────────────────────────────────────────────────────────

fn batch_of(order: &[&str]) -> rc_distill::Batch {
    rc_distill::Batch {
        session_key: "sess-batch-A".into(),
        device_id: "dev-batch-A".into(),
        source_agent: "agent-batch-A".into(),
        project_hint: None,
        rows: order
            .iter()
            .map(|uid| rc_distill::TurnRow {
                turn_uid: (*uid).into(),
                role: "human".into(),
                body: "x".into(),
                created_at_ms: 1,
                ingested_at_ms: 1,
                speaker: None,
                speaker_anchor_id: None,
            })
            .collect(),
    }
}

/// §1 的手工重算（离线算出的常量）：`sha256("fact" \x1f "decision" \x1f "sess-batch-A" \x1f
/// "dev-batch-A" \x1f "turn-0001" \x1f "turn-0002")`。
const EXPECTED_BATCH_KEY: &str =
    "92b999c89779106a858e08b3b5040550cdc1588538e1b449452fb008ce09cf11";

/// H1c：`review::new_item` 的键 = §1 的手工重算；同一批两次构造（items/顺序不变）键相同；
/// 轮集合换了顺序或模型输出走样（payload 不同）都不换键；session/device/family/pass 各自换键。
#[test]
fn new_item_batch_key_matches_the_hand_recomputation_and_ignores_output() {
    let payload_v1 = payload_value("fact", "decision", &["turn-0001".to_string()], json!([]));
    let payload_v2 = payload_value("fact", "decision", &["turn-0001".to_string()], json!([{"ref": "f1"}]));
    let owner_json = owner().to_string();

    let a = rc_distill::review::new_item(
        "fact", "decision", &batch_of(&["turn-0001", "turn-0002"]), owner_json.clone(), &payload_v1,
    );
    let b = rc_distill::review::new_item(
        "fact", "decision", &batch_of(&["turn-0002", "turn-0001"]), owner_json.clone(), &payload_v2,
    );
    assert_eq!(a.batch_key, EXPECTED_BATCH_KEY, "必须与 §1 的手工重算逐位一致");
    assert_eq!(a.batch_key, b.batch_key, "顺序与输出都不影响批次身份");
    assert_ne!(a.payload_json, b.payload_json, "两次的载荷确实不同（键仍相同）");

    let again = rc_distill::review::new_item(
        "fact", "decision", &batch_of(&["turn-0001", "turn-0002"]), owner_json.clone(), &payload_v1,
    );
    assert_eq!(a.batch_key, again.batch_key, "同一批两次构造键相同");

    for (what, key) in [
        ("family", rc_distill::review::new_item(
            "fact", "promise", &batch_of(&["turn-0001", "turn-0002"]), owner_json.clone(), &payload_v1).batch_key),
        ("pass", rc_distill::review::new_item(
            "event", "decision", &batch_of(&["turn-0001", "turn-0002"]), owner_json.clone(), &payload_v1).batch_key),
        ("turns", rc_distill::review::new_item(
            "fact", "decision", &batch_of(&["turn-0001"]), owner_json.clone(), &payload_v1).batch_key),
        ("session", {
            let mut batch = batch_of(&["turn-0001", "turn-0002"]);
            batch.session_key = "sess-other".into();
            rc_distill::review::new_item("fact", "decision", &batch, owner_json.clone(), &payload_v1).batch_key
        }),
        ("device", {
            let mut batch = batch_of(&["turn-0001", "turn-0002"]);
            batch.device_id = "dev-other".into();
            rc_distill::review::new_item("fact", "decision", &batch, owner_json.clone(), &payload_v1).batch_key
        }),
    ] {
        assert_ne!(a.batch_key, key, "换 {what} 必须换键");
    }
}
