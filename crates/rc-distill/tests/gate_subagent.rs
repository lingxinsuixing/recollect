//! D 子 agent 接线：闸不得把子 agent 的 `tool_result` 轮当作 human 证据。
//!
//! 夹具走真链路：v1 JSON（带 `speaker`）→ `parse` → `normalize` → 转录库；子 agent 的
//! agent 锚与轮级 mention 用测试内直插（按 DDL 必填列，与 `gate_subject.rs` 同形），
//! 不为测试改生产 API。然后跑真 `gate`。
//!
//! 判据：证据轮（role=agent、speaker=子 agent）不能触发 human 的 fail-open / 第 4 支
//! （主语锚 == 主人锚）。若把它当 human，这条"主语是主人、名字不在正文"的 fact 会放行；
//! 正确行为是 `subject_not_in_evidence`。
//!
//! BASE 上 `parse` 会把未知字段 `speaker` 整行拒收——这一条在 BASE 上是**运行期红**。
use rc_contract::{normalize, parse};
use rc_distill::review::{gate, GateOutcome};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::review::{enqueue, get, Item, NewItem};
use rc_memory::writers_v4::ensure_principal_anchor;
use rc_memory::{Scope, ScopeKind};
use rc_store::Root;
use serde_json::{json, Value};

const SESSION: &str = "sess-subagent";
const DEVICE: &str = "dev-subagent";
const SOURCE_AGENT: &str = "qoder";
const SUBAGENT: &str = "subagent:call-01";
const PRINCIPAL: &str = "p-sub";
const NOW_MS: i64 = 1_800_000_000_000;
const BODY: &str = "子 agent 报告：把发布推迟到 12 月 3 日，先记下来。";
const QUOTE: &str = "把发布推迟到 12 月 3 日";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-subagent".into() }
}

fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some(PRINCIPAL.into()),
        owner_agent_id: SOURCE_AGENT.into(),
        visibility: Visibility::Project,
        retention: "default".into(),
    }
}

fn payload_owner() -> Value {
    json!({
        "owner_principal_id": PRINCIPAL,
        "owner_agent_id": SOURCE_AGENT,
        "visibility": "project",
        "retention": "default",
        "scope_kind": "project",
        "scope_key": "proj-subagent"
    })
}

/// 一条带 speaker 的 agent 轮，真走进库链（`parse` 在 BASE 上就会因未知字段失败）。
fn seed_subagent_turn(root: &mut Root) -> String {
    let raw = json!({
        "format_version": 1,
        "sessions": [{
            "session_key": SESSION,
            "device_id": DEVICE,
            "source_agent": SOURCE_AGENT,
            "project_hint": "proj-subagent",
            "turns": [{
                "role": "agent",
                "body": BODY,
                "created_at_ms": 1_000i64,
                "speaker": SUBAGENT
            }]
        }]
    })
    .to_string();
    let file = parse(raw.as_bytes()).expect("speaker 是 v1 的 additive 可选字段");
    let rep = normalize(file, NOW_MS);
    assert_eq!(rep.accepted.len(), 1);
    let acc = &rep.accepted[0];
    assert_eq!(acc.turn.role, "agent", "子 agent 的结论是 agent 轮，不是 human");
    let uid = acc.turn.turn_uid.clone();
    rc_ledger::transcript::insert_batch(
        root.transcript_mut(),
        &acc.session,
        std::slice::from_ref(&acc.turn),
        NOW_MS,
    )
    .unwrap();
    // 子 agent 的 agent 锚 + 轮级 mention（真实写入面在 `record_turn_speakers`，
    // 这里直插是为了让本测试在 BASE 上仍能编译：红点在 `parse`）。
    let anchor_id = rc_memory::compute_mem_id(
        rc_memory::MemTable::Anchor,
        "agent",
        SUBAGENT,
        "",
        &scope(),
    );
    root.memory()
        .conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                                status, created_at_ms, updated_at_ms, norm_version)
             VALUES(?1,'agent',?2,'real','project','proj-subagent','active',1,1,1)",
            rusqlite::params![anchor_id, SUBAGENT],
        )
        .unwrap();
    let mention_id = rc_memory::identity::compute_mention_id(&uid, &anchor_id, SUBAGENT, None, None);
    root.memory()
        .conn()
        .execute(
            "INSERT INTO mention(id, anchor_id, turn_uid, surface, span_start, span_end,
                                 speaker_anchor_id, discourse_role, resolution, confidence,
                                 source_agent, device_id, created_at_ms, norm_version)
             VALUES(?1,?2,?3,?4,NULL,NULL,?2,'speaker','resolved',1.0,?5,?6,?7,1)",
            rusqlite::params![mention_id, anchor_id, uid, SUBAGENT, SOURCE_AGENT, DEVICE, NOW_MS],
        )
        .unwrap();
    uid
}

fn claim_principal(root: &Root) {
    rc_ledger::transcript::claim_principal(
        root.transcript(),
        rc_ledger::transcript::NewPrincipalClaim {
            session_key: SESSION,
            device_id: DEVICE,
            principal_id: PRINCIPAL,
            principal_name: Some("某人"),
            source: "human_claim",
            claimed_by: SOURCE_AGENT,
            reason: None,
        },
        10_000,
    )
    .unwrap();
}

fn gate_item(root: &Root, uid: &str, subject: &str) -> Item {
    let turns = vec![uid.to_string()];
    let payload = json!({
        "schema": "review.pass.v1",
        "pass_id": "fact",
        "family": "decision",
        "run_id": "run-gate-subagent",
        "batch": {
            "turn_uids": turns,
            "source_agent": SOURCE_AGENT,
            "device_id": DEVICE,
            "session_key": SESSION,
            "source_ms": 1_000
        },
        "owner": payload_owner(),
        "items": [{
            "subject_anchor_id": subject,
            "object_anchor_id": null,
            "predicate": "decide",
            "value": null,
            "category": "decision",
            "mode": "stated",
            "time_text": "12 月 3 日",
            "quote": QUOTE,
            "evidence": [{"turn_uid": uid, "anchor_role": "agent"}],
            "text": QUOTE
        }]
    });
    let outcome = enqueue(
        root.memory(),
        &NewItem {
            source: "distill".to_string(),
            pass_id: "fact".to_string(),
            family: "decision".to_string(),
            session_key: SESSION.to_string(),
            device_id: DEVICE.to_string(),
            turn_uids_json: serde_json::to_string(&turns).unwrap(),
            owner_json: payload_owner().to_string(),
            payload_json: payload.to_string(),
            batch_key: String::new(),
        },
        10_000,
    )
    .unwrap();
    get(root.memory(), outcome.id).unwrap().unwrap()
}

fn run(root: &Root, item: &Item) -> GateOutcome {
    gate(root.memory(), root.transcript(), item, NOW_MS).unwrap()
}

#[test]
fn 子agent的tool_result轮不被当作human证据() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uid = seed_subagent_turn(&mut root);
    claim_principal(&root);
    let principal_anchor = ensure_principal_anchor(
        root.memory(),
        PRINCIPAL,
        Some("某人"),
        &owner(),
        &scope(),
        NOW_MS,
    )
    .unwrap()
    .expect("身份已知时必须建主人锚")
    .id;

    // 子 agent 的锚就是这一轮的说话人：fact 的 speaker 必须指向它，而不是主人锚。
    let sub_anchor = rc_memory::compute_mem_id(
        rc_memory::MemTable::Anchor,
        "agent",
        SUBAGENT,
        "",
        &scope(),
    );
    let speaker = rc_memory::writers_v4::speaker_anchor_id(
        root.memory(),
        SOURCE_AGENT,
        Some(PRINCIPAL),
        &[(uid.clone(), rc_memory::AnchorRole::Agent)],
        &scope(),
    )
    .unwrap();
    assert_eq!(
        speaker.as_deref(),
        Some(sub_anchor.as_str()),
        "证据轮的说话人是子 agent，不是主人"
    );

    // 若这一轮被误当 human，第 4 支（主语锚 == 主人锚）会放行；正确行为是拒收。
    let item = gate_item(&root, &uid, &principal_anchor);
    let outcome = run(&root, &item);
    assert!(
        !outcome.passed && outcome.code == Some("subject_not_in_evidence"),
        "子 agent 的轮不是 human 证据，不能触发主人锚放行（code={:?}）",
        outcome.code
    );
}
