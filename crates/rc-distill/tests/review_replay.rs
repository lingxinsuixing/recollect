//! S3b-1：审核重放的 golden 等价与 gap 分支（设计 `2026-10-26-review-queue-s3` §4/§5/§8）。
//!
//! 断的是 `review::replay`：只凭 `review_item.payload_json` + 转录库，能不能重建出
//! **逐字节相同**的审核输入（`fact_review_user` / `event_review_user`）与写库参数。
//!
//! 夹具风格与 `tests/review_wiring.rs` 相同：真 `Root::open(tempdir)` + `common::seed_bodies`，
//! 正反例只用通用占位名（张三/李四/某项目），不含任何真实语料专名。
//!
//! 覆盖：
//! 1. fact 手工载荷 golden（含 canonical、object 有/无两态）；
//! 2. event 手工载荷 golden（extras 三格逐项回到 `EventDraft`）；
//! 3. 旧载荷缺新键 ⇒ `PayloadShape`（`turn_uid` 与 `subject_canonical` 各一条）；
//! 4. 轮缺失 ⇒ `TurnMissing`；
//! 5. owner 非法 ⇒ `OwnerShape`（`visibility` 与 `scope_kind` 各一条）；
//! 6. 新键不影响程序闸（同一夹具上 `gate.passed == true`）；
//! 7. `pass_id=time`（及其它未知 pass）⇒ `Unsupported`；
//! 8. 真 runner（fact/event）写出的载荷能被重放成**它自己发给审核模型的那段 user**。

use rc_distill::batch::{Batch, TurnRow};
use rc_distill::pass_event::{run_event, select_event_batch};
use rc_distill::pass_event_contract::EventDraft;
use rc_distill::pass_fact::{run_fact, select_fact_batch};
use rc_distill::pass_fact_contract::FactDraft;
use rc_distill::pass_prompt::{event_review_user, fact_review_user, FactFamily, ReviewInputContext};
use rc_distill::review::replay::{replay, Replay, ReplayDrafts, ReplayFailure, ReplayGap};
use rc_distill::review::run::review_context;
use rc_distill::review;
use rc_ledger::pass_state::PassState;
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::review::{enqueue, get, Item, ItemMeta, ReviewState};
use rc_memory::writers_v4::record_turn_speakers;
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{AnchorRole, BatchCtx, Provenance, Scope, ScopeKind};
use rc_store::{MemoryHandle, Root};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::VecDeque;

mod common;
use common::{adopt_rows, seed_bodies};

const NOW: i64 = 1_700_000_000_000;
const FROM: i64 = 1_000;
const SESSION: &str = "sess-batch-A";
const DEVICE: &str = "dev-batch-A";
const SOURCE_AGENT: &str = "agent-batch-A";

const PROMISE_BODY: &str = "我下周三之前把对账单发给你";
/// t2 是 **agent** 轮：正文要真写着主语"张三"（旧夹具的"好的，收到"里没有主语名）。
const RECEIVED_BODY: &str = "张三已经收到对账单";
const THANKS_BODY: &str = "谢谢";
const CONFIG_BODY: &str = "昨晚在机房我把配置改回去，服务就恢复了";
const RELEASE_BODY: &str = "今天上午我把发布记录更新了";

// ── 夹具 ──────────────────────────────────────────────────────────────────────

/// 会话归属的四格（与 `review_wiring.rs` 同一组值）：owner 从会话读，不信载荷。
fn set_owner(root: &mut Root) {
    root.transcript_mut()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id='p1', owner_agent_id='alice',
                    visibility='project', retention='keep',
                    scope_kind='project', scope_key='proj-x', ownership_src='operator'
             WHERE session_key='sess-batch-A' AND device_id='dev-batch-A'",
            [],
        )
        .unwrap();
}

fn add_anchor(root: &Root, scope: &Scope, kind: &str, canonical: &str, uid: &str, at: i64) -> String {
    let key = put_entity(
        root.memory(),
        &EntityContent {
            kind: kind.into(),
            title: canonical.into(),
            scope: scope.clone(),
            aliases: vec![],
        },
        &[Provenance { turn_uid: uid.to_string(), anchor_role: AnchorRole::Human }],
        at,
    )
    .unwrap();
    adopt_rows(root, "anchor", "alice", Some("p1"), "project", "project", "proj-x");
    key.id
}

/// 手工批：`seed_bodies` 的落库形状（角色按位置交替、created/ingested 都是 `FROM + i`）。
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

struct Fixture {
    _dir: tempfile::TempDir,
    root: Root,
    uids: Vec<String>,
    batch: Batch,
    ctx: BatchCtx,
    owner: OwnerCtx,
    scope: Scope,
    zhang: String,
    li: String,
    place: String,
    record: String,
}

fn fixture_fact() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let bodies = [PROMISE_BODY, RECEIVED_BODY, THANKS_BODY];
    let uids = seed_bodies(&mut root, &bodies, FROM);
    set_owner(&mut root);
    let scope = fixture_scope();
    let zhang = add_anchor(&root, &scope, "person", "张三", &uids[0], NOW - 1_000);
    let li = add_anchor(&root, &scope, "person", "李四", &uids[2], NOW - 2_000);
    let place = add_anchor(&root, &scope, "place", "某项目", &uids[2], NOW - 3_000);
    let record = add_anchor(&root, &scope, "work", "发布记录", &uids[1], NOW - 4_000);
    let batch = batch_of(&uids, &bodies);
    Fixture {
        _dir: dir,
        root,
        uids,
        batch,
        ctx: BatchCtx { source_agent: SOURCE_AGENT.into(), device_id: DEVICE.into() },
        owner: fixture_owner(),
        scope,
        zhang,
        li,
        place,
        record,
    }
}

fn fixture_event() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let bodies = [CONFIG_BODY, RELEASE_BODY];
    let uids = seed_bodies(&mut root, &bodies, FROM);
    set_owner(&mut root);
    let scope = fixture_scope();
    let zhang = add_anchor(&root, &scope, "person", "张三", &uids[0], NOW - 1_000);
    let li = add_anchor(&root, &scope, "person", "李四", &uids[1], NOW - 2_000);
    let place = add_anchor(&root, &scope, "place", "某项目", &uids[0], NOW - 3_000);
    let record = add_anchor(&root, &scope, "work", "发布记录", &uids[1], NOW - 4_000);
    let batch = batch_of(&uids, &bodies);
    Fixture {
        _dir: dir,
        root,
        uids,
        batch,
        ctx: BatchCtx { source_agent: SOURCE_AGENT.into(), device_id: DEVICE.into() },
        owner: fixture_owner(),
        scope,
        zhang,
        li,
        place,
        record,
    }
}

fn fact_drafts(f: &Fixture) -> Vec<FactDraft> {
    vec![
        // object 有：三格（object_anchor_id / object_canonical / value）都非空。
        FactDraft {
            reference: "f1".into(),
            turn_uid: f.uids[0].clone(),
            subject_anchor_id: f.zhang.clone(),
            object_anchor_id: Some(f.li.clone()),
            subject_canonical: "张三".into(),
            object_canonical: Some("李四".into()),
            predicate: "promise".into(),
            value: Some("把对账单发给李四".into()),
            category: "promise".into(),
            mode: "stated".into(),
            time_text: "下周三之前".into(),
            quote: PROMISE_BODY.into(),
            evidence_turn_uids: vec![f.uids[0].clone()],
            text: "张三承诺在下周三之前把对账单发给李四。".into(),
        },
        // object 无：object_anchor_id / object_canonical 都是 None，value 仍可在。
        FactDraft {
            reference: "f2".into(),
            turn_uid: f.uids[1].clone(),
            subject_anchor_id: f.zhang.clone(),
            object_anchor_id: None,
            subject_canonical: "张三".into(),
            object_canonical: None,
            predicate: "promise".into(),
            value: Some("收到对账单".into()),
            category: "promise".into(),
            mode: "stated".into(),
            time_text: String::new(),
            quote: RECEIVED_BODY.into(),
            evidence_turn_uids: vec![f.uids[1].clone()],
            text: "张三收到对账单。".into(),
        },
    ]
}

fn event_drafts(f: &Fixture) -> Vec<EventDraft> {
    vec![
        // place 有、object 无（字面量进 object_text）。
        EventDraft {
            reference: "e1".into(),
            turn_uid: f.uids[0].clone(),
            category: "state_change".into(),
            actor_anchor_id: f.zhang.clone(),
            object_anchor_id: None,
            object_text: Some("配置文件".into()),
            participants_anchor_ids: vec![f.zhang.clone()],
            place_anchor_id: Some(f.place.clone()),
            trigger: "改回去".into(),
            action: "restore_config".into(),
            result: "done".into(),
            mode: "stated".into(),
            time_text: "昨晚".into(),
            quote: CONFIG_BODY.into(),
            evidence_turn_uids: vec![f.uids[0].clone()],
            text: "张三昨晚把配置改回去，服务恢复。".into(),
            actor_canonical: "张三".into(),
            object_canonical: None,
            participant_canonicals: vec!["张三".into()],
            place_canonical: Some("某项目".into()),
        },
        // place 无、object 有、两名参与者（顺序与 canonical 一一对应）。
        EventDraft {
            reference: "e2".into(),
            turn_uid: f.uids[1].clone(),
            category: "event".into(),
            actor_anchor_id: f.zhang.clone(),
            object_anchor_id: Some(f.record.clone()),
            object_text: None,
            participants_anchor_ids: vec![f.zhang.clone(), f.li.clone()],
            place_anchor_id: None,
            trigger: "更新".into(),
            action: "update_release_log".into(),
            result: "done".into(),
            mode: "stated".into(),
            time_text: "今天上午".into(),
            quote: RELEASE_BODY.into(),
            evidence_turn_uids: vec![f.uids[1].clone()],
            text: "张三今天上午更新了发布记录。".into(),
            actor_canonical: "张三".into(),
            object_canonical: Some("发布记录".into()),
            participant_canonicals: vec!["张三".into(), "李四".into()],
            place_canonical: None,
        },
    ]
}

// ── 载荷构造（与 runner 的写法逐格对齐，含 S3b-1 的新键）─────────────────────

fn fact_item(draft: &FactDraft, batch: &Batch) -> Value {
    let evidence: Vec<Value> = draft
        .evidence_turn_uids
        .iter()
        .map(|uid| {
            let role = batch.row(uid).map(|row| row.role.clone()).unwrap_or_default();
            json!({"turn_uid": uid, "anchor_role": role})
        })
        .collect();
    json!({
        "turn_uid": draft.turn_uid,
        "subject_anchor_id": draft.subject_anchor_id,
        "object_anchor_id": draft.object_anchor_id,
        "subject_canonical": draft.subject_canonical,
        "object_canonical": draft.object_canonical,
        "predicate": draft.predicate,
        "value": draft.value,
        "category": draft.category,
        "mode": draft.mode,
        "time_text": draft.time_text,
        "quote": draft.quote,
        "evidence": evidence,
        "text": draft.text,
    })
}

fn event_item(draft: &EventDraft, batch: &Batch) -> Value {
    let evidence: Vec<Value> = draft
        .evidence_turn_uids
        .iter()
        .map(|uid| {
            let role = batch.row(uid).map(|row| row.role.clone()).unwrap_or_default();
            json!({"turn_uid": uid, "anchor_role": role})
        })
        .collect();
    json!({
        "turn_uid": draft.turn_uid,
        "subject_anchor_id": draft.actor_anchor_id,
        "object_anchor_id": draft.object_anchor_id,
        "actor_canonical": draft.actor_canonical,
        "object_canonical": draft.object_canonical,
        "participants_canonical": draft.participant_canonicals,
        "place_canonical": draft.place_canonical,
        "predicate": "action",
        "value": draft.object_text,
        "category": draft.category,
        "mode": draft.mode,
        "time_text": draft.time_text,
        "quote": draft.quote,
        "evidence": evidence,
        "text": draft.text,
    })
}

fn event_extra(draft: &EventDraft) -> Value {
    json!({
        "participants": draft.participants_anchor_ids,
        "value_json": {
            "result": draft.result,
            "action": draft.action,
            "trigger": draft.trigger,
            "object_text": draft.object_text,
        },
        "place_anchor_id": draft.place_anchor_id,
    })
}

fn enqueue_payload(
    f: &Fixture,
    pass_id: &str,
    family: &str,
    items: Value,
    extras: Option<Value>,
) -> Item {
    let payload = review::pass_payload(
        pass_id,
        family,
        "fp-replay-1",
        &f.batch,
        &f.ctx,
        &f.owner,
        &f.scope,
        items,
        extras,
    );
    let new = review::new_item(
        pass_id,
        family,
        &f.batch,
        review::owner_json(&f.owner, &f.scope),
        &payload,
    );
    let queued = enqueue(f.root.memory(), &new, NOW).unwrap();
    assert!(queued.fresh, "夹具的载荷必须是新建，不是命中旧项");
    get(f.root.memory(), queued.id).unwrap().expect("入队后必须读得回来")
}

/// 改载荷后重新序列化（gap 用例的输入面）。
fn rewrite_payload(mut item: Item, f: impl FnOnce(&mut Value)) -> Item {
    let mut payload: Value = serde_json::from_str(&item.payload_json).unwrap();
    f(&mut payload);
    item.payload_json = payload.to_string();
    item
}

fn expect_gap(result: Result<Replay, ReplayFailure>, expected: ReplayGap) {
    match result {
        Err(ReplayFailure::Gap(gap)) => assert_eq!(gap, expected),
        Err(ReplayFailure::Store(e)) => panic!("期望 gap {expected:?}，得到 Store: {e}"),
        Ok(_) => panic!("期望 gap {expected:?}，却重放成功"),
    }
}

fn assert_batch_replayed(original: &Batch, replayed: &Batch) {
    assert_eq!(original.session_key, replayed.session_key);
    assert_eq!(original.device_id, replayed.device_id);
    assert_eq!(original.source_agent, replayed.source_agent);
    assert_eq!(original.project_hint, replayed.project_hint);
    let rows = |batch: &Batch| -> Vec<(String, String, String, i64, i64)> {
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
    assert_eq!(rows(original), rows(replayed), "转录回查必须逐行、逐顺序一致");
}

fn assert_owner_replayed(f: &Fixture, replayed: &Replay) {
    assert_eq!(replayed.ctx.source_agent, f.ctx.source_agent);
    assert_eq!(replayed.ctx.device_id, f.ctx.device_id);
    assert_eq!(replayed.owner.owner_principal_id, f.owner.owner_principal_id);
    assert_eq!(replayed.owner.owner_agent_id, f.owner.owner_agent_id);
    assert_eq!(replayed.owner.visibility, f.owner.visibility);
    assert_eq!(replayed.owner.retention, f.owner.retention);
    assert_eq!(replayed.scope.kind.as_str(), f.scope.kind.as_str());
    assert_eq!(replayed.scope.key, f.scope.key);
}

/// C6a：审核输入的批级信息（与引擎同一处解析）。
fn review_ctx(f: &Fixture, batch: &Batch) -> ReviewInputContext {
    review_context(
        f.root.memory(),
        f.batch.session_key.as_str(),
        batch,
        &f.owner,
        &f.scope,
    )
    .unwrap()
}

// ── 回放 backend（runner 用例）───────────────────────────────────────────────

struct ScriptBackend {
    replies: RefCell<VecDeque<Result<String, LlmFailure>>>,
    prompts: RefCell<Vec<Prompt>>,
}

impl ScriptBackend {
    fn two(extraction: String, review: String) -> Self {
        ScriptBackend {
            replies: RefCell::new(vec![Ok(extraction), Ok(review)].into()),
            prompts: RefCell::new(Vec::new()),
        }
    }

    /// fact 一片 = 抽取 + proposer + challenger（§7.2；两档同形回复）。
    fn three(extraction: String, review: String) -> Self {
        ScriptBackend {
            replies: RefCell::new(vec![Ok(extraction), Ok(review.clone()), Ok(review)].into()),
            prompts: RefCell::new(Vec::new()),
        }
    }
}

impl LlmBackend for ScriptBackend {
    fn complete(&self, prompt: &Prompt) -> Result<String, LlmFailure> {
        self.prompts.borrow_mut().push(prompt.clone());
        self.replies.borrow_mut().pop_front().unwrap_or(Err(LlmFailure::Timeout))
    }
    fn slug(&self) -> Option<&str> {
        Some("fake-replay-model")
    }
}

fn only_item_id(m: &MemoryHandle) -> i64 {
    m.conn()
        .query_row("SELECT id FROM review_item", [], |r| r.get::<_, i64>(0))
        .unwrap()
}

// ── 1. fact golden ────────────────────────────────────────────────────────────

#[test]
fn fact_payload_replays_to_the_same_review_prompt() {
    let f = fixture_fact();
    let drafts = fact_drafts(&f);
    let items: Vec<Value> = drafts.iter().map(|d| fact_item(d, &f.batch)).collect();
    let item = enqueue_payload(&f, "fact", "promise", Value::Array(items), None);

    let replayed = replay(f.root.transcript(), &item).expect("新载荷必须可重放");
    assert_batch_replayed(&f.batch, &replayed.batch);
    assert_owner_replayed(&f, &replayed);

    let ReplayDrafts::Fact(got) = replayed.drafts else {
        panic!("fact 载荷必须重放成 fact 草稿");
    };
    // 逐字段相等：reference 由位置重建、canonical 从新键直取、object 有/无两态都回到原值。
    assert_eq!(got, drafts);
    assert_eq!(
        fact_review_user(FactFamily::Promise, &drafts, &f.batch, &review_ctx(&f, &f.batch)),
        fact_review_user(FactFamily::Promise, &got, &replayed.batch, &review_ctx(&f, &replayed.batch)),
        "重放后的审核 prompt 必须逐字节等价"
    );
}

// ── 2. event golden ───────────────────────────────────────────────────────────

#[test]
fn event_payload_replays_to_the_same_review_prompt() {
    let f = fixture_event();
    let drafts = event_drafts(&f);
    let items: Vec<Value> = drafts.iter().map(|d| event_item(d, &f.batch)).collect();
    let extras: Vec<Value> = drafts.iter().map(event_extra).collect();
    let item = enqueue_payload(
        &f,
        "event",
        "",
        Value::Array(items),
        Some(Value::Array(extras)),
    );

    let replayed = replay(f.root.transcript(), &item).expect("新载荷必须可重放");
    assert_batch_replayed(&f.batch, &replayed.batch);
    assert_owner_replayed(&f, &replayed);

    let ReplayDrafts::Event(got) = replayed.drafts else {
        panic!("event 载荷必须重放成 event 草稿");
    };
    // extras 三格（participants / place_anchor_id / value_json 的 action/result/object_text）
    // 逐项回到 EventDraft，canonical 四格从 items 的新键直取。
    assert_eq!(got, drafts);
    assert_eq!(
        event_review_user(&drafts, &f.batch, &review_ctx(&f, &f.batch)),
        event_review_user(&got, &replayed.batch, &review_ctx(&f, &replayed.batch)),
        "重放后的审核 prompt 必须逐字节等价"
    );
}

// ── 3. 旧载荷缺新键 ───────────────────────────────────────────────────────────

#[test]
fn payload_without_the_new_keys_is_a_shape_gap() {
    let f = fixture_fact();
    let drafts = fact_drafts(&f);
    let items: Vec<Value> = drafts.iter().map(|d| fact_item(d, &f.batch)).collect();
    let item = enqueue_payload(&f, "fact", "promise", Value::Array(items), None);

    let without_turn = rewrite_payload(item.clone(), |payload| {
        payload["items"][0].as_object_mut().unwrap().remove("turn_uid");
    });
    expect_gap(replay(f.root.transcript(), &without_turn), ReplayGap::PayloadShape);

    let without_canonical = rewrite_payload(item, |payload| {
        payload["items"][0].as_object_mut().unwrap().remove("subject_canonical");
    });
    expect_gap(
        replay(f.root.transcript(), &without_canonical),
        ReplayGap::PayloadShape,
    );
}

// ── 4. 轮缺失 ─────────────────────────────────────────────────────────────────

#[test]
fn turn_missing_from_the_transcript_is_a_turn_gap() {
    let f = fixture_fact();
    let drafts = fact_drafts(&f);
    let items: Vec<Value> = drafts.iter().map(|d| fact_item(d, &f.batch)).collect();
    let item = enqueue_payload(&f, "fact", "promise", Value::Array(items), None);

    let ghost = "a".repeat(64);
    let mut item = rewrite_payload(item, |payload| {
        payload["batch"]["turn_uids"] = json!([ghost]);
    });
    // 闸/重放都要求 `turn_uids_json` 与 `batch.turn_uids` 排序后相等；不跟着改是另一种 gap。
    item.turn_uids_json = json!([ghost]).to_string();
    expect_gap(replay(f.root.transcript(), &item), ReplayGap::TurnMissing);
}

// ── 4b. 轮换了会话 ────────────────────────────────────────────────────────────

#[test]
fn turn_from_another_session_is_a_turn_mismatch_gap() {
    let f = fixture_fact();
    let drafts = fact_drafts(&f);
    let items: Vec<Value> = drafts.iter().map(|d| fact_item(d, &f.batch)).collect();
    let item = enqueue_payload(&f, "fact", "promise", Value::Array(items), None);

    // 轮还在库里，但 payload 声明的会话与那一行不符 ⇒ 不许"同一 uid 换了会话"地重放。
    let item = rewrite_payload(item, |payload| {
        payload["batch"]["session_key"] = json!("sess-somewhere-else");
    });
    expect_gap(replay(f.root.transcript(), &item), ReplayGap::TurnMismatch);
}

// ── 4c. 证据角色与转录现状不符（H4a）────────────────────────────────────────

/// 证据角色必须仍等于转录行现在的 `role`：`f1` 的证据轮在夹具里是 `human`，载荷改成
/// `agent` ⇒ `TurnMismatch`。基准那一次重放成功，保证 gap 不是"载荷本来就坏"。
#[test]
fn evidence_role_that_no_longer_matches_the_transcript_is_a_turn_mismatch() {
    let f = fixture_fact();
    let drafts = fact_drafts(&f);
    let items: Vec<Value> = drafts.iter().map(|d| fact_item(d, &f.batch)).collect();
    let item = enqueue_payload(&f, "fact", "promise", Value::Array(items), None);
    assert!(
        replay(f.root.transcript(), &item.clone()).is_ok(),
        "基准载荷必须能重放"
    );

    let tampered = rewrite_payload(item, |payload| {
        payload["items"][0]["evidence"][0]["anchor_role"] = json!("agent");
    });
    expect_gap(replay(f.root.transcript(), &tampered), ReplayGap::TurnMismatch);
}

/// naming 的 `anchors[].evidence[].anchor_role` 走同一条闸：角色一致 ⇒ 可重放，
/// 改成相反角色 ⇒ `TurnMismatch`。
#[test]
fn naming_anchor_evidence_role_must_still_match_the_transcript() {
    let f = fixture_fact();
    let naming_items = |role: &str| {
        json!({
            "anchors": [{
                "reference": "a1",
                "kind": "person",
                "canonical": "张三",
                "aliases": [],
                "reality_status": "real",
                "stable": true,
                "evidence": [{"turn_uid": f.uids[0], "anchor_role": role}]
            }],
            "mentions": [],
            "issues": [],
            "unresolved": []
        })
    };
    let item = enqueue_payload(&f, "naming", "", naming_items("human"), None);
    let replayed = replay(f.root.transcript(), &item).expect("角色一致的 naming 载荷必须可重放");
    let ReplayDrafts::Naming(a1) = replayed.drafts else {
        panic!("naming 载荷必须重放成 A1 计划");
    };
    assert_eq!(a1.anchors[0].evidence[0].anchor_role, AnchorRole::Human);

    let tampered = rewrite_payload(item, |payload| {
        payload["items"]["anchors"][0]["evidence"][0]["anchor_role"] = json!("agent");
    });
    expect_gap(replay(f.root.transcript(), &tampered), ReplayGap::TurnMismatch);
}

// ── 4d. batch.source_ms 与转录现状不符（H4a）────────────────────────────────

/// `batch.source_ms` 必须仍等于重建批内 `created_at_ms` 的最大值；+1 就是"源时刻过期"。
#[test]
fn source_ms_that_no_longer_matches_the_transcript_is_a_turn_mismatch() {
    let f = fixture_fact();
    let drafts = fact_drafts(&f);
    let items: Vec<Value> = drafts.iter().map(|d| fact_item(d, &f.batch)).collect();
    let item = enqueue_payload(&f, "fact", "promise", Value::Array(items), None);
    assert!(
        replay(f.root.transcript(), &item.clone()).is_ok(),
        "基准载荷必须能重放"
    );

    let tampered = rewrite_payload(item, |payload| {
        let bumped = payload["batch"]["source_ms"].as_i64().unwrap() + 1;
        payload["batch"]["source_ms"] = json!(bumped);
    });
    expect_gap(replay(f.root.transcript(), &tampered), ReplayGap::TurnMismatch);
}

// ── 5. owner 非法 ─────────────────────────────────────────────────────────────

#[test]
fn illegal_owner_values_are_an_owner_gap() {
    let f = fixture_fact();
    let drafts = fact_drafts(&f);
    let items: Vec<Value> = drafts.iter().map(|d| fact_item(d, &f.batch)).collect();
    let item = enqueue_payload(&f, "fact", "promise", Value::Array(items), None);

    let bad_visibility = rewrite_payload(item.clone(), |payload| {
        payload["owner"]["visibility"] = json!("friends");
    });
    expect_gap(
        replay(f.root.transcript(), &bad_visibility),
        ReplayGap::OwnerShape,
    );

    let bad_scope = rewrite_payload(item, |payload| {
        payload["owner"]["scope_kind"] = json!("galaxy");
    });
    expect_gap(replay(f.root.transcript(), &bad_scope), ReplayGap::OwnerShape);
}

// ── 6. 新键不影响程序闸 ───────────────────────────────────────────────────────

#[test]
fn gate_still_passes_with_the_new_keys() {
    let f = fixture_fact();
    let drafts = fact_drafts(&f);
    let items: Vec<Value> = drafts.iter().map(|d| fact_item(d, &f.batch)).collect();
    let item = enqueue_payload(&f, "fact", "promise", Value::Array(items), None);

    let outcome = review::gate(f.root.memory(), f.root.transcript(), &item, NOW).unwrap();
    assert!(
        outcome.passed,
        "新键不得改变闸门结论（失败码 {:?}）",
        outcome.code
    );
    assert_eq!(outcome.code, None);
    assert_eq!(outcome.items_checked, drafts.len());
}

// ── 7. time 不支持（rows 自 S3c-3a 起重放，见 `review_rows_replay.rs`）────────

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

#[test]
fn time_pass_is_unsupported() {
    let f = fixture_fact();
    let time_items = json!([{
        "turn_uid": f.uids[0],
        "kind": "event_time",
        "text": "昨晚",
        "precision": "day",
        "basis": "none",
        "start_ms": NOW,
        "end_ms": null,
    }]);
    let time_payload = review::pass_payload(
        "time",
        "",
        "fp-replay-time",
        &f.batch,
        &f.ctx,
        &f.owner,
        &f.scope,
        time_items,
        None,
    );
    let time_item = manual_item(&time_payload, "time", "");
    expect_gap(
        replay(f.root.transcript(), &time_item),
        ReplayGap::Unsupported,
    );
}

// ── 8. 真 runner 写出的载荷 = 它自己发给审核模型的那段 user ──────────────────

fn fact_reply() -> String {
    json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a1", "predicate": "promise",
            "object": "a2", "value": "把对账单发给你", "category": "promise",
            "mode": "stated", "time_text": "下周三之前", "quote": PROMISE_BODY,
            "evidence": ["t1"], "text": "张三承诺在下周三之前把对账单发给李四。"
        }],
        "empty_turns": ["t2", "t3"]
    })
    .to_string()
}

fn fact_review_reply() -> String {
    json!({
        "schema_version": 0, "pass": "fact_review", "family": "promise",
        "verdicts": [{"ref": "f1", "verdict": "pass", "code": null, "evidence": "把对账单发给你"}]
    })
    .to_string()
}

fn event_reply() -> String {
    json!({
        "schema_version": 0, "pass": "event",
        "events": [{
            "ref": "e1", "turn": "t1", "category": "state_change", "actor": "a1",
            "trigger": "改回去", "action": "restore_config", "object": null, "object_text": "配置文件",
            "participants": ["a1"], "place": null, "result": "done", "mode": "stated",
            "time_text": "昨晚", "quote": CONFIG_BODY, "evidence": ["t1"],
            "text": "张三昨晚把配置改回去，服务恢复。"
        }],
        "empty_turns": ["t2"]
    })
    .to_string()
}

fn event_review_reply() -> String {
    json!({
        "schema_version": 0, "pass": "event_review",
        "verdicts": [{"ref": "e1", "verdict": "pass", "code": null}]
    })
    .to_string()
}

#[test]
fn fact_runner_payload_replays_to_the_prompt_it_sent() {
    let f = fixture_fact();
    let plan = select_fact_batch(
        f.root.transcript(),
        f.root.ledger(),
        f.root.memory(),
        0,
        FactFamily::Promise,
        NOW,
    )
    .unwrap()
    .expect("夹具里 3 轮都还没跑 fact/promise");
    let backend = ScriptBackend::three(fact_reply(), fact_review_reply());
    let out = run_fact(
        f.root.memory(),
        f.root.transcript(),
        f.root.ledger(),
        &plan,
        &backend,
        NOW,
    )
    .unwrap();
    assert_eq!(out.state, PassState::Done);
    assert_eq!((out.drafts, out.written), (1, 1));

    let id = only_item_id(f.root.memory());
    let item = get(f.root.memory(), id).unwrap().unwrap();
    let replayed = replay(f.root.transcript(), &item).expect("runner 写出的载荷必须可重放");
    let ReplayDrafts::Fact(got) = replayed.drafts else {
        panic!("fact runner 的载荷必须重放成 fact 草稿");
    };

    // 写库/审核参数与 runner 当场用的 plan 是同一份（owner/scope/ctx 与批）。
    assert_eq!(replayed.ctx.source_agent, plan.ctx.source_agent);
    assert_eq!(replayed.ctx.device_id, plan.ctx.device_id);
    assert_eq!(replayed.owner.owner_principal_id, plan.owner.owner_principal_id);
    assert_eq!(replayed.owner.owner_agent_id, plan.owner.owner_agent_id);
    assert_eq!(replayed.owner.visibility, plan.owner.visibility);
    assert_eq!(replayed.owner.retention, plan.owner.retention);
    assert_eq!(replayed.scope.kind.as_str(), plan.scope.kind.as_str());
    assert_eq!(replayed.scope.key, plan.scope.key);
    assert_batch_replayed(&plan.batch, &replayed.batch);

    // golden：重放算出的审核 user 与 runner 当场发出去的窄审调用逐字节相同。
    // §7.2 起一片发 proposer + challenger 两次，两档读同一份 user。
    let prompts = backend.prompts.borrow();
    assert_eq!(prompts.len(), 3, "一次抽取 + proposer + challenger");
    let want_user =
        fact_review_user(FactFamily::Promise, &got, &replayed.batch, &review_ctx(&f, &replayed.batch));
    assert_eq!(want_user, prompts[1].user, "重放必须复现 proposer 的 user");
    assert_eq!(want_user, prompts[2].user, "challenger 读同一片、同一套 ref");
}

#[test]
fn event_runner_payload_replays_to_the_prompt_it_sent() {
    let f = fixture_event();
    let plan = select_event_batch(f.root.transcript(), f.root.ledger(), f.root.memory(), 0, NOW)
        .unwrap()
        .expect("夹具里的轮还没跑 event");
    let backend = ScriptBackend::two(event_reply(), event_review_reply());
    let out = run_event(
        f.root.memory(),
        f.root.transcript(),
        f.root.ledger(),
        &plan,
        &backend,
        NOW,
    )
    .unwrap();
    assert_eq!(out.state, PassState::Done);
    assert_eq!((out.drafts, out.written), (1, 1));

    let id = only_item_id(f.root.memory());
    let item = get(f.root.memory(), id).unwrap().unwrap();
    let replayed = replay(f.root.transcript(), &item).expect("runner 写出的载荷必须可重放");
    let ReplayDrafts::Event(got) = replayed.drafts else {
        panic!("event runner 的载荷必须重放成 event 草稿");
    };

    assert_eq!(replayed.ctx.source_agent, plan.ctx.source_agent);
    assert_eq!(replayed.ctx.device_id, plan.ctx.device_id);
    assert_eq!(replayed.owner.owner_principal_id, plan.owner.owner_principal_id);
    assert_eq!(replayed.owner.owner_agent_id, plan.owner.owner_agent_id);
    assert_eq!(replayed.owner.visibility, plan.owner.visibility);
    assert_eq!(replayed.owner.retention, plan.owner.retention);
    assert_eq!(replayed.scope.kind.as_str(), plan.scope.kind.as_str());
    assert_eq!(replayed.scope.key, plan.scope.key);
    assert_batch_replayed(&plan.batch, &replayed.batch);

    let prompts = backend.prompts.borrow();
    assert_eq!(prompts.len(), 2, "一次抽取 + 一次窄审");
    assert_eq!(
        event_review_user(&got, &replayed.batch, &review_ctx(&f, &replayed.batch)),
        prompts[1].user,
        "重放必须复现 runner 当时发给审核模型的 user"
    );
}

// ── 9. A7：轮级说话人进送审 prompt ────────────────────────────────────────────
//
// replay 只读转录库；说话人在引擎侧 replay 之后回填（`run.rs::fill_turn_speakers`，
// `load_turn_speakers` 是唯一读法）。本用例钉**整条接线**：`record_turn_speakers`
// 写下的 mention → `run_fact` 的窄审 prompt 里看得见这个名字。
// 名字用「示例主人」是 A7 派发档验收判据明写的（账号主人名，非语料专名）。

#[test]
fn turn_speaker_mention_reaches_the_review_prompt_the_engine_sends() {
    let write_and_run = |mention: bool| -> String {
        let f = fixture_fact();
        if mention {
            // D 子 agent 接线的写入面：`record_turn_speakers` 建 kind='agent' 锚
            // （canonical=示例主人）并写 `speaker/resolved` 的 mention——与生产导入侧
            // 同一条写入口。t1 是 f1 草稿的目标轮（human）。
            record_turn_speakers(
                f.root.memory(),
                &f.ctx,
                &f.owner,
                &f.scope,
                &[(f.uids[0].clone(), "示例主人".into())],
                NOW,
            )
            .unwrap();
        }
        let plan = select_fact_batch(
            f.root.transcript(),
            f.root.ledger(),
            f.root.memory(),
            0,
            FactFamily::Promise,
            NOW,
        )
        .unwrap()
        .expect("夹具里 3 轮都还没跑 fact/promise");
        let backend = ScriptBackend::three(fact_reply(), fact_review_reply());
        let out = run_fact(
            f.root.memory(),
            f.root.transcript(),
            f.root.ledger(),
            &plan,
            &backend,
            NOW,
        )
        .unwrap();
        assert_eq!(out.state, PassState::Done, "mention 不得改变批的结局");
        let prompts = backend.prompts.borrow();
        prompts[1].user.clone()
    };

    let with = write_and_run(true);
    assert!(
        with.contains("TARGET TURN [role: human | speaker: 示例主人]:"),
        "轮级说话人必须出现在送审 prompt 里（覆盖 role 推导）\n{with}"
    );

    // 阴性对照：同一夹具不写 mention，说话人回落 role 推导（两个 person 锚 ⇒ unknown），
    // 名字一次都不出现——证明 prompt 里的「示例主人」确实来自 mention 回填这一轴。
    let without = write_and_run(false);
    assert!(!without.contains("示例主人"), "没有 mention 的轮不许出现这个名字\n{without}");
    assert!(
        without.contains("TARGET TURN [role: human | speaker: unknown]:"),
        "无 mention 的 human 轮按既有渲染走 role 推导\n{without}"
    );
}
