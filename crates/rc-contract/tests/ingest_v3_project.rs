//! G17：v3 → v1 下行投影的契约层测试。
//!
//! 夹具全合成：人名（user-a/agent-b/tool-c/system-d）、会话名（dsh-sess-01）、
//! 正文（-7311 系列尾号）都不是真实语料。v3 形状以本 crate 的 `ingest_v3` 类型为准，
//! 与 `tests/ingest_v3.rs` 的合法夹具同构，外加一条 system 事件（HarnessNotice）。

use rc_contract::format::TurnTier;
use rc_contract::identity_v3::{
    char_count, compute_content_hash, compute_event_uid, compute_revision_id, compute_session_uid,
    compute_thread_uid, compute_turn_uid_from_anchor, normalize_text_v3, text_sha256,
};
use rc_contract::ingest_v3::*;
use rc_contract::{parse, project_v3_to_v1, RejectCode};
use std::collections::BTreeMap;

const SOURCE_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const TS_BASE: i64 = 1_761_474_800_000;

struct Fx {
    json: Vec<u8>,
    event_uids: Vec<String>,
    texts: Vec<String>,
}

fn fixture() -> Fx {
    let (file, event_uids, texts) = build();
    Fx {
        json: serde_json::to_vec(&file).expect("fixture must serialize"),
        event_uids,
        texts,
    }
}

/// 合法 v3 文件：human(1) → tool_call(2) → tool_result(3) → agent(4) → system(5)，
/// 全部落在 main 线程的同一条 turn 上。工具对与 system 事件就是"富字段"的代表。
fn build() -> (V3File, Vec<String>, Vec<String>) {
    let session_uid = compute_session_uid("dsh-adapter", "dev-dsh-01", "dsh-sess-01", None);
    let thread_uid = compute_thread_uid(&session_uid, "main");
    let revision_id = compute_revision_id(SOURCE_SHA, 1, INGEST_V3_FORMAT_VERSION);

    let raw_texts = [
        "把回访表补齐-7311",
        "已列缺字段-7312",
        "找到 3 条-7314",
        "会话续接-7313",
    ];
    let texts: Vec<String> = raw_texts.iter().map(|t| normalize_text_v3(t)).collect();
    let content_hashes = [
        compute_content_hash("human_msg", &texts[0], None, None, None),
        compute_content_hash("tool_call", "", Some(("c1", "search")), None, None),
        compute_content_hash("tool_result", "", None, Some(("c1", "ok", &texts[2])), None),
        compute_content_hash("agent_msg", &texts[1], None, None, None),
        compute_content_hash("system", &texts[3], None, None, None),
    ];
    let event_uids: Vec<String> = (0..5)
        .map(|i| compute_event_uid(&thread_uid, &format!("n{}", i + 1), &content_hashes[i], 0))
        .collect();
    let turn_uid = compute_turn_uid_from_anchor(&thread_uid, &event_uids[0]);

    let source_ref = |native: &str| SourceRef {
        adapter: "dsh-adapter".into(),
        adapter_version: 1,
        path: "dsh-fixture.jsonl".into(),
        native_id: Some(native.into()),
        line: None,
        native_step: None,
    };
    let span = |text: &str| {
        Some(Span {
            start: 0,
            end: char_count(text),
            text_sha256: Some(text_sha256(text)),
        })
    };

    let mut events = Vec::new();
    // 1: human
    events.push(V3Event {
            seq: 1,
            parent_seq: None,
            parent_event_uid: None,
            kind: EventKind::HumanMsg,
            speaker_id: "user-a".into(),
            ts_ms: Some(TS_BASE),
            observed_at_ms: None,
            valid_from_ms: None,
            valid_to_ms: None,
            thread_uid: thread_uid.clone(),
            turn_uid: turn_uid.clone(),
            event_uid: event_uids[0].clone(),
            native_key: "n1".into(),
            content_hash: content_hashes[0].clone(),
            occurrence: 0,
            revision_id: revision_id.clone(),
            revision_of: None,
            duplicate_of: None,
            attempt: 0,
            final_: true,
            text: texts[0].clone(),
            text_sha256: text_sha256(&texts[0]),
            span: span(&texts[0]),
            norm_version: INGEST_V3_NORM_VERSION,
            refs: vec![],
            trust: Trust::User,
            speech_act: SpeechAct::Assert,
            modality: Modality::Actual,
            system_kind: None,
            tool: None,
            tool_result: None,
            doc: None,
            summary: false,
            covers: vec![],
            source_ref: source_ref("n1"),
            quality: None,
        });
    // 2: tool_call
    events.push(V3Event {
            seq: 2,
            parent_seq: Some(1),
            parent_event_uid: Some(event_uids[0].clone()),
            kind: EventKind::ToolCall,
            speaker_id: "agent-b".into(),
            ts_ms: Some(TS_BASE + 1000),
            observed_at_ms: None,
            valid_from_ms: None,
            valid_to_ms: None,
            thread_uid: thread_uid.clone(),
            turn_uid: turn_uid.clone(),
            event_uid: event_uids[1].clone(),
            native_key: "n2".into(),
            content_hash: content_hashes[1].clone(),
            occurrence: 0,
            revision_id: revision_id.clone(),
            revision_of: None,
            duplicate_of: None,
            attempt: 0,
            final_: true,
            text: String::new(),
            text_sha256: text_sha256(""),
            span: None,
            norm_version: INGEST_V3_NORM_VERSION,
            refs: vec![],
            trust: Trust::Agent,
            speech_act: SpeechAct::Request,
            modality: Modality::Actual,
            system_kind: None,
            tool: Some(ToolCall {
                call_id: "c1".into(),
                name: "search".into(),
                arguments: serde_json::json!({"q": "回访表"}),
            }),
            tool_result: None,
            doc: None,
            summary: false,
            covers: vec![],
            source_ref: source_ref("n2"),
            quality: None,
        });
    // 3: tool_result
    events.push(V3Event {
            seq: 3,
            parent_seq: Some(2),
            parent_event_uid: Some(event_uids[1].clone()),
            kind: EventKind::ToolResult,
            speaker_id: "tool-c".into(),
            ts_ms: Some(TS_BASE + 2000),
            observed_at_ms: None,
            valid_from_ms: None,
            valid_to_ms: None,
            thread_uid: thread_uid.clone(),
            turn_uid: turn_uid.clone(),
            event_uid: event_uids[2].clone(),
            native_key: "n3".into(),
            content_hash: content_hashes[2].clone(),
            occurrence: 0,
            revision_id: revision_id.clone(),
            revision_of: None,
            duplicate_of: None,
            attempt: 0,
            final_: true,
            text: String::new(),
            text_sha256: text_sha256(""),
            span: None,
            norm_version: INGEST_V3_NORM_VERSION,
            refs: vec![],
            trust: Trust::Tool,
            speech_act: SpeechAct::Assert,
            modality: Modality::Actual,
            system_kind: None,
            tool: None,
            tool_result: Some(ToolResult {
                call_id: "c1".into(),
                status: ToolStatus::Ok,
                text: Some(texts[2].clone()),
                details: Some(serde_json::json!({"count": 3})),
                artifacts: vec![],
            }),
            doc: None,
            summary: false,
            covers: vec![],
            source_ref: source_ref("n3"),
            quality: None,
        });
    // 4: agent
    events.push(V3Event {
            seq: 4,
            parent_seq: Some(1),
            parent_event_uid: Some(event_uids[0].clone()),
            kind: EventKind::AgentMsg,
            speaker_id: "agent-b".into(),
            ts_ms: Some(TS_BASE + 3000),
            observed_at_ms: None,
            valid_from_ms: None,
            valid_to_ms: None,
            thread_uid: thread_uid.clone(),
            turn_uid: turn_uid.clone(),
            event_uid: event_uids[3].clone(),
            native_key: "n4".into(),
            content_hash: content_hashes[3].clone(),
            occurrence: 0,
            revision_id: revision_id.clone(),
            revision_of: None,
            duplicate_of: None,
            attempt: 0,
            final_: true,
            text: texts[1].clone(),
            text_sha256: text_sha256(&texts[1]),
            span: span(&texts[1]),
            norm_version: INGEST_V3_NORM_VERSION,
            refs: vec![],
            trust: Trust::Agent,
            speech_act: SpeechAct::Assert,
            modality: Modality::Actual,
            system_kind: None,
            tool: None,
            tool_result: None,
            doc: None,
            summary: false,
            covers: vec![],
            source_ref: source_ref("n4"),
            quality: None,
        });
    // 5: system（HarnessNotice：v1 有 role=system 这个家）
    events.push(V3Event {
            seq: 5,
            parent_seq: Some(1),
            parent_event_uid: Some(event_uids[0].clone()),
            kind: EventKind::System,
            speaker_id: "system-d".into(),
            ts_ms: Some(TS_BASE + 4000),
            observed_at_ms: None,
            valid_from_ms: None,
            valid_to_ms: None,
            thread_uid: thread_uid.clone(),
            turn_uid: turn_uid.clone(),
            event_uid: event_uids[4].clone(),
            native_key: "n5".into(),
            content_hash: content_hashes[4].clone(),
            occurrence: 0,
            revision_id: revision_id.clone(),
            revision_of: None,
            duplicate_of: None,
            attempt: 0,
            final_: true,
            text: texts[3].clone(),
            text_sha256: text_sha256(&texts[3]),
            span: span(&texts[3]),
            norm_version: INGEST_V3_NORM_VERSION,
            refs: vec![],
            trust: Trust::Harness,
            speech_act: SpeechAct::Assert,
            modality: Modality::Actual,
            system_kind: Some(SystemKind::HarnessNotice),
            tool: None,
            tool_result: None,
            doc: None,
            summary: false,
            covers: vec![],
            source_ref: source_ref("n5"),
            quality: None,
        });

    let file = V3File {
        format_version: INGEST_V3_FORMAT_VERSION,
        ingest_version: INGEST_V3_FORMAT_VERSION,
        sessions: vec![V3Session {
            session_key: "dsh-sess-01".into(),
            session_uid,
            parent_session_key: None,
            agent_instance_id: None,
            source_agent: "dsh-adapter".into(),
            device_id: "dev-dsh-01".into(),
            environment: Environment {
                machine_id: "m-dsh".into(),
                machine: Some("windows".into()),
                hostname: None,
                os: None,
                cwd: Some("D:/proj-dsh".into()),
                git_branch: None,
                harness: Harness {
                    name: "dsh-adapter".into(),
                    version: "1".into(),
                },
                locale: None,
                timezone: Some("UTC".into()),
            },
            roster: Roster {
                agents: vec![RosterAgent {
                    id: "agent-b".into(),
                    name: None,
                    parent_id: None,
                    role: ParticipantRole::Main,
                    model: None,
                }],
                tools: vec![RosterTool {
                    name: "search".into(),
                    source: ToolSource::Harness,
                    version: None,
                    definition_ref: None,
                }],
            },
            ownership: Ownership {
                principal_id: None,
                owner_agent_id: "agent-b".into(),
                scope: Scope {
                    kind: ScopeKind::Project,
                    key: "dsh-proj".into(),
                },
                visibility: Visibility::PrivateAgent,
                retention: "default".into(),
            },
            threads: vec![Thread {
                thread_key: "main".into(),
                thread_uid: thread_uid.clone(),
                parent_thread_key: None,
                kind: ThreadKind::Main,
                agent_id: "agent-b".into(),
            }],
            turns: vec![Turn {
                turn_key: "t1".into(),
                turn_uid: turn_uid.clone(),
                thread_uid: thread_uid.clone(),
                derived: true,
                start_seq: 1,
                end_seq: 5,
            }],
            participants: vec![
                Participant {
                    id: "user-a".into(),
                    kind: ParticipantKind::Human,
                    name: None,
                    principal: None,
                    parent_id: None,
                    role: None,
                    model: None,
                },
                Participant {
                    id: "agent-b".into(),
                    kind: ParticipantKind::Agent,
                    name: None,
                    principal: None,
                    parent_id: None,
                    role: Some(ParticipantRole::Main),
                    model: None,
                },
                Participant {
                    id: "tool-c".into(),
                    kind: ParticipantKind::Tool,
                    name: None,
                    principal: None,
                    parent_id: None,
                    role: None,
                    model: None,
                },
                Participant {
                    id: "system-d".into(),
                    kind: ParticipantKind::System,
                    name: None,
                    principal: None,
                    parent_id: None,
                    role: None,
                    model: None,
                },
            ],
            docs: vec![],
            secrets: vec![],
            dropped: Dropped::default(),
            quality: SessionQuality::default(),
            source: SourceMeta {
                adapter: "dsh-adapter".into(),
                adapter_version: 1,
                path: "dsh-fixture.jsonl".into(),
                sha256: SOURCE_SHA.into(),
                cursor: None,
                revision_id: revision_id.clone(),
                revision_of: None,
            },
            events,
        }],
        policy: None,
        key_manifest: None,
        replication: None,
    };
    (file, event_uids, texts)
}

fn mutated(fx: &Fx, edit: impl FnOnce(&mut serde_json::Value)) -> Vec<u8> {
    let mut v: serde_json::Value = serde_json::from_slice(&fx.json).expect("fixture is json");
    edit(&mut v);
    serde_json::to_vec(&v).expect("mutation must serialize")
}

#[test]
fn projects_only_the_core_session_and_turn_grid() {
    let fx = fixture();
    let out = project_v3_to_v1(&fx.json).expect("合法 v3 必须能投影");
    assert_eq!(out.file.format_version, 1);
    assert_eq!(out.file.sessions.len(), 1);
    let s = &out.file.sessions[0];
    assert_eq!(s.session_key, "dsh-sess-01");
    assert_eq!(s.device_id, "dev-dsh-01");
    assert_eq!(s.source_agent, "dsh-adapter");
    // environment.machine 是 v1 真有的格，跟着投影；cwd 不是（v1 只收逻辑名）。
    assert_eq!(s.machine.as_deref(), Some("windows"));
    assert_eq!(s.project_hint, None);
    assert_eq!(s.turns.len(), 3, "核心三角色入投影，工具对整行丢弃");

    let t0 = &s.turns[0];
    assert_eq!(t0.role, "human");
    assert_eq!(t0.body, fx.texts[0]);
    assert_eq!(t0.turn_uid.as_deref(), Some(fx.event_uids[0].as_str()), "v1 turn_uid = v3 event_uid");
    assert_eq!(t0.tier, Some(TurnTier::IdBearing));
    assert_eq!(t0.seq, Some(1), "v3 seq 作为指针随行保留");
    assert_eq!(t0.created_at_ms, Some(TS_BASE));
    assert_eq!(t0.speaker, None, "投影面最小：speaker 不在工单核心格里");

    let t1 = &s.turns[1];
    assert_eq!(t1.role, "agent");
    assert_eq!(t1.body, fx.texts[1]);
    assert_eq!(t1.created_at_ms, Some(TS_BASE + 3000));

    let t2 = &s.turns[2];
    assert_eq!(t2.role, "system");
    assert_eq!(t2.body, fx.texts[3]);
    assert_eq!(t2.created_at_ms, Some(TS_BASE + 4000));
}

#[test]
fn rich_fields_are_dropped_and_counted_per_field() {
    let fx = fixture();
    let out = project_v3_to_v1(&fx.json).expect("合法 v3 必须能投影");
    let d: &BTreeMap<String, u64> = &out.dropped;

    // 整行丢弃：工具对按 kind 计。
    assert_eq!(d.get("event.kind.tool_call"), Some(&1));
    assert_eq!(d.get("event.kind.tool_result"), Some(&1));

    // 已投影 3 行各自丢弃的恒在格（按事件数计）。
    assert_eq!(d.get("event.thread_uid"), Some(&3));
    assert_eq!(d.get("event.turn_uid"), Some(&3), "v3 的 turn 指针没有 v1 的家");
    assert_eq!(d.get("event.revision_id"), Some(&3));
    assert_eq!(d.get("event.trust"), Some(&3));
    assert_eq!(d.get("event.speech_act"), Some(&3));
    assert_eq!(d.get("event.source_ref"), Some(&3));
    assert_eq!(d.get("event.speaker_id"), Some(&3));
    assert_eq!(d.get("event.span"), Some(&3), "tool 行 span 缺席，不计");
    assert_eq!(d.get("event.system_kind"), Some(&1), "只有 system 行带这一格");
    // 值缺席的格不计（parent_seq 在投影行 4/5 上有值，计 2；行 1 没有父）。
    assert_eq!(d.get("event.parent_seq"), Some(&2));
    assert_eq!(d.get("event.observed_at_ms"), None);
    assert_eq!(d.get("event.occurrence"), None);

    // 会话层富字段。
    assert_eq!(d.get("session_uid"), Some(&1));
    assert_eq!(d.get("environment.machine_id"), Some(&1));
    assert_eq!(d.get("environment.harness"), Some(&1));
    assert_eq!(d.get("environment.cwd"), Some(&1));
    assert_eq!(d.get("environment.timezone"), Some(&1));
    assert_eq!(d.get("environment.hostname"), None, "没带的格不计");
    assert_eq!(d.get("roster.agents"), Some(&1));
    assert_eq!(d.get("roster.tools"), Some(&1));
    assert_eq!(d.get("threads"), Some(&1));
    assert_eq!(d.get("turns"), Some(&1));
    assert_eq!(d.get("participants"), Some(&4));
    assert_eq!(d.get("ownership"), Some(&1));
    assert_eq!(d.get("source"), Some(&1));
    assert_eq!(d.get("docs"), None);
}

/// 工单第 1 条的硬半句：投影产物必须过现有 v1 全套校验（parse 一字不动的那个门）。
#[test]
fn projection_output_passes_the_existing_v1_gate() {
    let fx = fixture();
    let out = project_v3_to_v1(&fx.json).expect("合法 v3 必须能投影");
    let bytes = serde_json::to_vec(&out.file).expect("v1 文件必须能序列化");
    let parsed = parse(&bytes).expect("投影产物必须原样过 v1 的 parse 闸");
    assert_eq!(parsed.sessions.len(), 1);
    assert_eq!(parsed.sessions[0].turns.len(), 3);
}

#[test]
fn v3_gate_failure_is_a_whole_batch_reject() {
    let fx = fixture();
    let bytes = mutated(&fx, |v| {
        v["sessions"][0]["session_uid"] = serde_json::json!("0".repeat(64));
    });
    let err = project_v3_to_v1(&bytes).expect_err("v3 闸不过必须整批拒绝");
    assert_eq!(err[0].code, RejectCode::V3Invalid);
    assert!(err[0].detail.contains("session_uid_mismatch"), "{}", err[0].detail);
    assert_eq!(err[0].loc.session_index, 0);
}

#[test]
fn events_without_timestamp_are_dropped_and_counted() {
    let fx = fixture();
    let bytes = mutated(&fx, |v| {
        v["sessions"][0]["events"][0]["ts_ms"] = serde_json::Value::Null;
    });
    let out = project_v3_to_v1(&bytes).expect("缺 ts 只是丢行，不是投影失败");
    assert_eq!(out.file.sessions[0].turns.len(), 2);
    assert_eq!(out.dropped.get("event.ts_ms"), Some(&1));
}

#[test]
fn non_final_core_event_is_dropped_and_counted() {
    let fx = fixture();
    let bytes = mutated(&fx, |v| {
        // serde rename：V3Event.final_ 序列化成 "final"。
        v["sessions"][0]["events"][3]["final"] = serde_json::json!(false);
    });
    let out = project_v3_to_v1(&bytes).expect("非 final 只是丢行，不是投影失败");
    assert_eq!(out.file.sessions[0].turns.len(), 2);
    assert_eq!(out.dropped.get("event.final_"), Some(&1));
}

#[test]
fn quarantined_core_event_is_dropped_and_counted() {
    let fx = fixture();
    let bytes = mutated(&fx, |v| {
        v["sessions"][0]["events"][3]["quality"] =
            serde_json::json!({"flags": ["truncated"], "quarantine": true, "reason": "fixture"});
    });
    let out = project_v3_to_v1(&bytes).expect("隔离只是丢行，不是投影失败");
    assert_eq!(out.file.sessions[0].turns.len(), 2);
    assert_eq!(out.dropped.get("event.quarantine"), Some(&1));
}

#[test]
fn malformed_v3_json_is_json_malformed() {
    let err = project_v3_to_v1(b"{ this is not json").expect_err("读不成 v3 必须拒");
    assert_eq!(err[0].code, RejectCode::JsonMalformed);
}
