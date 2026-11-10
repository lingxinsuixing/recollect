//! S3c-3a：`review.rows.v1` 载荷与 rows 重放（派发档 §3/§4）。
//!
//! 夹具与 `tests/review_replay.rs` 同一套：真 `Root::open(tempdir)` + `common::seed_bodies`
//! 落两轮转录，不写 `review_item`（重放只读 `Item` 的字段，不要求它已入队）。
//!
//! 覆盖：
//! 1. golden：2 行、一行带 conflict ⇒ 载荷形状/份数/owner 六键/batch_key，再逐项重放回
//!    `RowsDraft`（行值、证据轮、conflict 相等）；
//! 2. `batch_key` 是 rows 专用键（S3c-fix：行集合），同批两次构造相同、与轮顺序无关；
//! 3. 坏形状：schema 错、pass 错、row 缺 id、evidence 不在 batch ⇒ `PayloadShape`；
//! 4. `pass_id=time` 仍 `Unsupported`（rows 不再是）。
//!
//! 正反例只用通用占位名（张三/某项目），不含任何真实语料专名或 holdout。
use rc_distill::batch::{Batch, TurnRow};
use rc_distill::review::{self, replay, RowsRowInput, ReplayDrafts, ReplayFailure, ReplayGap};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::review::{rows_batch_key, Item, ItemMeta, NewItem, ReviewState};
use rc_memory::{BatchCtx, Scope, ScopeKind};
use rc_store::Root;
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

mod common;
use common::seed_bodies;

const NOW: i64 = 1_700_000_000_000;
const FROM: i64 = 1_000;
const SESSION: &str = "sess-batch-A";
const DEVICE: &str = "dev-batch-A";
const SOURCE_AGENT: &str = "agent-batch-A";
const RUN_ID: &str = "run-rows-1";

// ── 夹具 ──────────────────────────────────────────────────────────────────────

fn batch_of(uids: &[String], bodies: &[&str]) -> Batch {
    assert_eq!(uids.len(), bodies.len());
    Batch {
        session_key: SESSION.into(),
        device_id: DEVICE.into(),
        source_agent: SOURCE_AGENT.into(),
        project_hint: None,
        rows: uids
            .iter()
            .zip(bodies)
            .enumerate()
            .map(|(i, (uid, body))| TurnRow {
                turn_uid: uid.clone(),
                role: if i % 2 == 0 { "human".into() } else { "agent".into() },
                body: (*body).into(),
                created_at_ms: FROM + i as i64,
                ingested_at_ms: FROM + i as i64,
                speaker: None,
                speaker_anchor_id: None,
            })
            .collect(),
    }
}

fn fixture_owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p1".into()),
        owner_agent_id: "alice".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}

fn fixture_scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-x".into() }
}

fn fixture_ctx() -> BatchCtx {
    BatchCtx { source_agent: SOURCE_AGENT.into(), device_id: DEVICE.into() }
}

/// fact 行的原列：三格身份 + 全部 NOT NULL 列（键集合与 hub 的 `allowed_columns("fact")` 同向）。
fn row_values(id: &str, text_body: &str, evidence: &str) -> Map<String, Value> {
    json!({
        "id": id,
        "subject_anchor_id": "anchor-zhang",
        "predicate": "decide",
        "category": "decision",
        "mode": "stated",
        "text": text_body,
        "evidence_turn_uid": evidence,
        "scope_kind": "project",
        "scope_key": "proj-x",
        "source_agent": SOURCE_AGENT,
        "device_id": DEVICE,
        "created_at_ms": FROM,
        "norm_version": 1,
    })
    .as_object()
    .expect("夹具的列值必须是对象")
    .clone()
}

fn fixture_rows(uids: &[String]) -> Vec<RowsRowInput> {
    vec![
        RowsRowInput {
            values: row_values("fact-1", "发布推迟到 12 月 3 日", &uids[0]),
            evidence_turn_uid: uids[0].clone(),
            conflict: None,
        },
        RowsRowInput {
            values: row_values("fact-2", "张三负责跟进发布", &uids[1]),
            evidence_turn_uid: uids[1].clone(),
            // 既有事实的异值：同一主语同一谓词的另一个值。
            conflict: Some(json!({"value_text": "pending", "turn_uid": uids[0]})),
        },
    ]
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: Root,
    uids: Vec<String>,
    batch: Batch,
    ctx: BatchCtx,
    owner: OwnerCtx,
    scope: Scope,
}

/// rows 的期望键（S3c-fix）：`session`/`device`/**行 id 集合**。与轮集合无关——
/// hub 的两片常常共享同一份轮集合，键必须由"这一片审的是哪几行"决定。
fn expected_rows_key(rows: &[RowsRowInput]) -> String {
    let ids: Vec<String> = rows
        .iter()
        .map(|row| row.values["id"].as_str().unwrap_or_default().to_string())
        .collect();
    rows_batch_key(SESSION, DEVICE, &ids)
}

fn setup() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let bodies = ["我决定发布推迟到 12 月 3 日", "张三负责跟进发布"];
    let uids = seed_bodies(&mut root, &bodies, FROM);
    let batch = batch_of(&uids, &bodies);
    Fixture {
        _dir: dir,
        root,
        uids,
        batch,
        ctx: fixture_ctx(),
        owner: fixture_owner(),
        scope: fixture_scope(),
    }
}

/// 载荷 → 待重放的 `Item`。重放只读这些字段，`id`/指纹与判定无关。
fn item_of(new: &NewItem) -> Item {
    Item {
        meta: ItemMeta {
            id: 0,
            source: new.source.clone(),
            pass_id: new.pass_id.clone(),
            family: new.family.clone(),
            session_key: new.session_key.clone(),
            device_id: new.device_id.clone(),
            state: ReviewState::PendingReview,
            program_code: None,
            attempt: 0,
            created_at_ms: NOW,
            updated_at_ms: NOW,
            applied_at_ms: None,
        },
        turn_uids_json: new.turn_uids_json.clone(),
        owner_json: new.owner_json.clone(),
        payload_json: new.payload_json.clone(),
        payload_sha256: "manual".into(),
        conflict_json: None,
    }
}

/// 手工载荷 → `Item`（time 的 Unsupported 用例与改写后的坏形状用例用它）。
fn manual_item(payload: &Value, pass_id: &str, family: &str) -> Item {
    Item {
        meta: ItemMeta {
            id: 0,
            source: "distill".into(),
            pass_id: pass_id.into(),
            family: family.into(),
            session_key: SESSION.into(),
            device_id: DEVICE.into(),
            state: ReviewState::PendingReview,
            program_code: None,
            attempt: 0,
            created_at_ms: NOW,
            updated_at_ms: NOW,
            applied_at_ms: None,
        },
        turn_uids_json: payload["batch"]["turn_uids"].to_string(),
        owner_json: payload["owner"].to_string(),
        payload_json: payload.to_string(),
        payload_sha256: "manual".into(),
        conflict_json: None,
    }
}

fn rewrite(mut item: Item, f: impl FnOnce(&mut Value)) -> Item {
    let mut payload: Value = serde_json::from_str(&item.payload_json).unwrap();
    f(&mut payload);
    item.payload_json = payload.to_string();
    item
}

fn expect_gap(result: Result<review::Replay, ReplayFailure>, expected: ReplayGap) {
    match result {
        Err(ReplayFailure::Gap(gap)) => assert_eq!(gap, expected),
        Err(ReplayFailure::Store(e)) => panic!("期望 gap {expected:?}，得到 Store: {e}"),
        Ok(_) => panic!("期望 gap {expected:?}，却重放成功"),
    }
}

// ── 1. 载荷形状 + golden ─────────────────────────────────────────────────────

#[test]
fn rows_payload_replays_to_the_same_rows() {
    let f = setup();
    let rows = fixture_rows(&f.uids);
    let owner_json = review::owner_json(&f.owner, &f.scope);
    let payload = review::rows_payload(RUN_ID, &f.batch, &f.ctx, &f.owner, &f.scope, &rows);

    // —— 信封 ——
    assert_eq!(payload["schema"], json!("review.rows.v1"));
    assert_eq!(payload["pass_id"], json!("rows"));
    assert_eq!(payload["family"], json!(""));
    assert_eq!(payload["run_id"], json!(RUN_ID));
    assert_eq!(payload["batch"]["turn_uids"], json!(f.uids));
    assert_eq!(payload["batch"]["source_agent"], json!(SOURCE_AGENT));
    assert_eq!(payload["batch"]["device_id"], json!(DEVICE));
    assert_eq!(payload["batch"]["session_key"], json!(SESSION));
    assert_eq!(payload["batch"]["source_ms"], json!(FROM + 1));

    // —— owner 六键 ——
    let owner_keys: BTreeSet<&str> = payload["owner"]
        .as_object()
        .expect("owner 必须是对象")
        .keys()
        .map(String::as_str)
        .collect();
    let expected_keys: BTreeSet<&str> = [
        "owner_principal_id",
        "owner_agent_id",
        "visibility",
        "retention",
        "scope_kind",
        "scope_key",
    ]
    .into_iter()
    .collect();
    assert_eq!(owner_keys, expected_keys);
    assert_eq!(payload["owner"]["visibility"], json!("project"));
    assert_eq!(payload["owner"]["scope_kind"], json!("project"));
    assert_eq!(payload["owner"]["scope_key"], json!("proj-x"));

    // —— items：份数与逐键形状 ——
    let items = payload["items"].as_array().expect("items 必须是数组");
    assert_eq!(items.len(), 2);
    let item_keys: BTreeSet<&str> = items[0]
        .as_object()
        .expect("item 必须是对象")
        .keys()
        .map(String::as_str)
        .collect();
    let expected_item_keys: BTreeSet<&str> =
        ["row", "evidence_turn_uid", "conflict"].into_iter().collect();
    assert_eq!(item_keys, expected_item_keys);
    assert_eq!(items[0]["row"], json!(rows[0].values));
    assert_eq!(items[0]["evidence_turn_uid"], json!(f.uids[0]));
    assert_eq!(items[0]["conflict"], Value::Null);
    assert_eq!(items[1]["conflict"], json!({"value_text": "pending", "turn_uid": f.uids[0]}));

    // —— NewItem：source/pass/family/batch_key ——
    let new = review::rows_new_item(&f.batch, owner_json.clone(), &payload);
    assert_eq!(new.source, "distill");
    assert_eq!(new.pass_id, "rows");
    assert_eq!(new.family, "");
    assert_eq!(new.session_key, SESSION);
    assert_eq!(new.device_id, DEVICE);
    let expected_key = expected_rows_key(&rows);
    assert_eq!(new.batch_key, expected_key);

    // —— 重放 golden ——
    let replayed = replay(f.root.transcript(), &item_of(&new)).expect("rows 载荷必须可重放");
    assert_eq!(replayed.batch.session_key, f.batch.session_key);
    assert_eq!(replayed.batch.device_id, f.batch.device_id);
    assert_eq!(replayed.batch.source_agent, f.batch.source_agent);
    assert_eq!(replayed.batch.project_hint, f.batch.project_hint);
    let batch_rows = |batch: &Batch| -> Vec<(String, String, String, i64, i64)> {
        batch
            .rows
            .iter()
            .map(|row| {
                (
                    row.turn_uid.clone(),
                    row.role.clone(),
                    row.body.clone(),
                    row.created_at_ms,
                    row.ingested_at_ms,
                )
            })
            .collect()
    };
    assert_eq!(batch_rows(&f.batch), batch_rows(&replayed.batch));
    assert_eq!(replayed.ctx.source_agent, f.ctx.source_agent);
    assert_eq!(replayed.ctx.device_id, f.ctx.device_id);
    assert_eq!(replayed.owner.owner_principal_id, f.owner.owner_principal_id);
    assert_eq!(replayed.owner.owner_agent_id, f.owner.owner_agent_id);
    assert_eq!(replayed.owner.visibility, f.owner.visibility);
    assert_eq!(replayed.owner.retention, f.owner.retention);
    assert_eq!(replayed.scope.kind.as_str(), f.scope.kind.as_str());
    assert_eq!(replayed.scope.key, f.scope.key);

    let ReplayDrafts::Rows(got) = replayed.drafts else {
        panic!("rows 载荷必须重放成 rows 草稿");
    };
    assert_eq!(got.len(), rows.len());
    assert_eq!(got[0].values, rows[0].values);
    assert_eq!(got[0].evidence_turn_uid, rows[0].evidence_turn_uid);
    assert_eq!(got[0].conflict, None);
    assert_eq!(got[1].values, rows[1].values);
    assert_eq!(got[1].evidence_turn_uid, rows[1].evidence_turn_uid);
    assert_eq!(got[1].conflict, rows[1].conflict);
}

// ── 2. batch_key：是行集合、同批两次相同、与 batch 的轮顺序无关 ───────────────

#[test]
fn batch_key_ignores_turn_order_and_is_stable() {
    let f = setup();
    let rows = fixture_rows(&f.uids);
    let payload = review::rows_payload(RUN_ID, &f.batch, &f.ctx, &f.owner, &f.scope, &rows);
    let owner_json = review::owner_json(&f.owner, &f.scope);

    let first = review::rows_new_item(&f.batch, owner_json.clone(), &payload);
    let second = review::rows_new_item(&f.batch, owner_json.clone(), &payload);
    assert_eq!(first.batch_key, second.batch_key, "同批两次构造必须是同一个 key");

    // 同一份载荷、batch 的轮顺序颠倒 ⇒ 键不变（键只看载荷里的行集合与 session/device）。
    let mut reversed = f.batch.clone();
    reversed.rows.reverse();
    let reversed_item = review::rows_new_item(&reversed, owner_json, &payload);
    assert_eq!(
        reversed_item.batch_key, first.batch_key,
        "batch_key 只认排序去重后的行集合，不认 batch 里的轮顺序"
    );
    assert_eq!(first.batch_key, expected_rows_key(&rows));
}

// ── 3. 坏形状 ─────────────────────────────────────────────────────────────────

#[test]
fn wrong_schema_is_a_shape_gap() {
    let f = setup();
    let rows = fixture_rows(&f.uids);
    let payload = review::rows_payload(RUN_ID, &f.batch, &f.ctx, &f.owner, &f.scope, &rows);
    let item = manual_item(&payload, "rows", "");
    let tampered = rewrite(item, |payload| {
        payload["schema"] = json!("review.pass.v1");
    });
    expect_gap(replay(f.root.transcript(), &tampered), ReplayGap::PayloadShape);
}

#[test]
fn wrong_pass_is_a_shape_gap() {
    let f = setup();
    let rows = fixture_rows(&f.uids);
    let payload = review::rows_payload(RUN_ID, &f.batch, &f.ctx, &f.owner, &f.scope, &rows);
    let item = manual_item(&payload, "rows", "");
    let tampered = rewrite(item, |payload| {
        payload["pass_id"] = json!("fact");
    });
    expect_gap(replay(f.root.transcript(), &tampered), ReplayGap::PayloadShape);
}

#[test]
fn row_without_id_is_a_shape_gap() {
    let f = setup();
    let rows = fixture_rows(&f.uids);
    let payload = review::rows_payload(RUN_ID, &f.batch, &f.ctx, &f.owner, &f.scope, &rows);
    let item = manual_item(&payload, "rows", "");
    let tampered = rewrite(item, |payload| {
        payload["items"][0]["row"].as_object_mut().unwrap().remove("id");
    });
    expect_gap(replay(f.root.transcript(), &tampered), ReplayGap::PayloadShape);
}

#[test]
fn evidence_turn_outside_the_batch_is_a_shape_gap() {
    let f = setup();
    let rows = fixture_rows(&f.uids);
    let payload = review::rows_payload(RUN_ID, &f.batch, &f.ctx, &f.owner, &f.scope, &rows);
    let item = manual_item(&payload, "rows", "");
    let ghost = "f".repeat(64);
    let tampered = rewrite(item, |payload| {
        payload["items"][0]["evidence_turn_uid"] = json!(ghost);
    });
    expect_gap(replay(f.root.transcript(), &tampered), ReplayGap::PayloadShape);
}

// ── 4. time 仍 Unsupported ────────────────────────────────────────────────────

#[test]
fn time_pass_is_still_unsupported() {
    let f = setup();
    let items = json!([{
        "turn_uid": f.uids[0],
        "kind": "event_time",
        "text": "昨晚",
        "precision": "day",
        "basis": "none",
        "start_ms": NOW,
        "end_ms": null,
    }]);
    let payload = review::pass_payload(
        "time",
        "",
        "run-time-1",
        &f.batch,
        &f.ctx,
        &f.owner,
        &f.scope,
        items,
        None,
    );
    let item = manual_item(&payload, "time", "");
    expect_gap(replay(f.root.transcript(), &item), ReplayGap::Unsupported);
}
