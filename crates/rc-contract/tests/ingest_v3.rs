//! v3 入库契约的字段/解析闸。校验器测试在 Task 2 继续追加。

use rc_contract::identity_v3::{
    char_count, compute_content_hash, compute_event_uid, compute_revision_id, compute_session_uid,
    compute_thread_uid, compute_turn_uid_from_anchor, normalize_text_v3, text_sha256,
};
use rc_contract::ingest_v3::*;
use serde_json::{json, Value};

const SOURCE_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct Ids {
    session_uid: String,
    thread_uid: String,
    turn_uid: String,
    revision_id: String,
    event_uids: Vec<String>,
    texts: Vec<String>,
}

fn fixture() -> (V3File, Ids) {
    let session_uid = compute_session_uid("test", "d3", "s3", None);
    let thread_uid = compute_thread_uid(&session_uid, "main");
    let revision_id = compute_revision_id(SOURCE_SHA, 1, INGEST_V3_FORMAT_VERSION);

    let raw_texts = [" 把回访表补齐 ", "已列缺字段", "找到 3 条"];
    let texts: Vec<String> = raw_texts.iter().map(|t| normalize_text_v3(t)).collect();
    let content_hashes = [
        compute_content_hash("human_msg", &texts[0], None, None, None),
        compute_content_hash("tool_call", "", Some(("c1", "search")), None, None),
        compute_content_hash("tool_result", "", None, Some(("c1", "ok", &texts[2])), None),
        compute_content_hash("agent_msg", &texts[1], None, None, None),
    ];
    let event_uids = vec![
        compute_event_uid(&thread_uid, "n1", &content_hashes[0], 0),
        compute_event_uid(&thread_uid, "n2", &content_hashes[1], 0),
        compute_event_uid(&thread_uid, "n3", &content_hashes[2], 0),
        compute_event_uid(&thread_uid, "n4", &content_hashes[3], 0),
    ];
    let turn_uid = compute_turn_uid_from_anchor(&thread_uid, &event_uids[0]);

    let source_ref = |native: &str| SourceRef {
        adapter: "test".into(),
        adapter_version: 1,
        path: "fixture.jsonl".into(),
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

    let events = vec![
        V3Event {
            seq: 1,
            parent_seq: None,
            parent_event_uid: None,
            kind: EventKind::HumanMsg,
            speaker_id: "A".into(),
            ts_ms: Some(1_761_474_800_000),
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
        },
        V3Event {
            seq: 2,
            parent_seq: Some(1),
            parent_event_uid: Some(event_uids[0].clone()),
            kind: EventKind::ToolCall,
            speaker_id: "agent".into(),
            ts_ms: Some(1_761_474_801_000),
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
                arguments: json!({"q": "回访表"}),
            }),
            tool_result: None,
            doc: None,
            summary: false,
            covers: vec![],
            source_ref: source_ref("n2"),
            quality: None,
        },
        V3Event {
            seq: 3,
            parent_seq: Some(2),
            parent_event_uid: Some(event_uids[1].clone()),
            kind: EventKind::ToolResult,
            speaker_id: "tool".into(),
            ts_ms: Some(1_761_474_802_000),
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
                details: Some(json!({"count": 3})),
                artifacts: vec![],
            }),
            doc: None,
            summary: false,
            covers: vec![],
            source_ref: source_ref("n3"),
            quality: None,
        },
        V3Event {
            seq: 4,
            parent_seq: Some(1),
            parent_event_uid: Some(event_uids[0].clone()),
            kind: EventKind::AgentMsg,
            speaker_id: "agent".into(),
            ts_ms: Some(1_761_474_803_000),
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
        },
    ];

    let file = V3File {
        format_version: INGEST_V3_FORMAT_VERSION,
        ingest_version: INGEST_V3_FORMAT_VERSION,
        sessions: vec![V3Session {
            session_key: "s3".into(),
            session_uid: session_uid.clone(),
            parent_session_key: None,
            agent_instance_id: None,
            source_agent: "test".into(),
            device_id: "d3".into(),
            environment: Environment {
                machine_id: "m3".into(),
                machine: Some("windows".into()),
                hostname: None,
                os: None,
                cwd: Some("D:/proj".into()),
                git_branch: None,
                harness: Harness {
                    name: "test".into(),
                    version: "1".into(),
                },
                locale: None,
                timezone: Some("UTC".into()),
            },
            roster: Roster {
                agents: vec![RosterAgent {
                    id: "agent".into(),
                    name: Some("test-agent".into()),
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
                owner_agent_id: "agent".into(),
                scope: Scope {
                    kind: ScopeKind::Project,
                    key: "s3".into(),
                },
                visibility: Visibility::PrivateAgent,
                retention: "default".into(),
            },
            threads: vec![Thread {
                thread_key: "main".into(),
                thread_uid: thread_uid.clone(),
                parent_thread_key: None,
                kind: ThreadKind::Main,
                agent_id: "agent".into(),
            }],
            turns: vec![Turn {
                turn_key: "t1".into(),
                turn_uid: turn_uid.clone(),
                thread_uid: thread_uid.clone(),
                derived: true,
                start_seq: 1,
                end_seq: 4,
            }],
            participants: vec![
                Participant {
                    id: "A".into(),
                    kind: ParticipantKind::Human,
                    name: None,
                    principal: None,
                    parent_id: None,
                    role: None,
                    model: None,
                },
                Participant {
                    id: "agent".into(),
                    kind: ParticipantKind::Agent,
                    name: None,
                    principal: None,
                    parent_id: None,
                    role: Some(ParticipantRole::Main),
                    model: None,
                },
                Participant {
                    id: "tool".into(),
                    kind: ParticipantKind::Tool,
                    name: None,
                    principal: None,
                    parent_id: None,
                    role: None,
                    model: None,
                },
                Participant {
                    id: "system".into(),
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
                adapter: "test".into(),
                adapter_version: 1,
                path: "fixture.jsonl".into(),
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

    (
        file,
        Ids {
            session_uid,
            thread_uid,
            turn_uid,
            revision_id,
            event_uids,
            texts,
        },
    )
}

fn to_value(file: &V3File) -> Value {
    serde_json::to_value(file).expect("fixture must serialize")
}

fn event_mut(v: &mut Value, idx: usize) -> &mut Value {
    &mut v["sessions"][0]["events"][idx]
}

#[test]
fn fixture_round_trips_and_identity_fields_are_exposed() {
    let (file, ids) = fixture();
    let v = to_value(&file);
    let back: V3File = serde_json::from_value(v.clone()).expect("must parse");
    assert_eq!(back, file);

    assert_eq!(v["format_version"], 3);
    assert_eq!(v["sessions"][0]["session_uid"], ids.session_uid);
    assert_eq!(v["sessions"][0]["threads"][0]["thread_uid"], ids.thread_uid);
    assert_eq!(v["sessions"][0]["turns"][0]["turn_uid"], ids.turn_uid);
    assert_eq!(v["sessions"][0]["source"]["revision_id"], ids.revision_id);
    assert_eq!(v["sessions"][0]["events"][0]["event_uid"], ids.event_uids[0]);
    assert_eq!(v["sessions"][0]["events"][0]["final"], true);
    assert_eq!(v["sessions"][0]["events"][0]["text"], ids.texts[0]);
}

#[test]
fn unknown_field_is_rejected_at_parse() {
    let (file, _) = fixture();
    let mut v = to_value(&file);
    event_mut(&mut v, 0)["extra"] = json!(1);
    assert!(serde_json::from_value::<V3File>(v).is_err());
}

#[test]
fn invalid_closed_set_is_rejected_at_parse() {
    let (file, _) = fixture();
    let mut v = to_value(&file);
    event_mut(&mut v, 0)["trust"] = json!("robot");
    assert!(serde_json::from_value::<V3File>(v).is_err());

    let mut v = to_value(&file);
    event_mut(&mut v, 0)["quality"] = json!({"flags": ["not_a_flag"], "quarantine": true});
    assert!(serde_json::from_value::<V3File>(v).is_err());
}

#[test]
fn missing_required_identity_field_is_rejected_at_parse() {
    for field in [
        "event_uid",
        "thread_uid",
        "turn_uid",
        "text_sha256",
        "norm_version",
        "native_key",
        "content_hash",
        "revision_id",
    ] {
        let (file, _) = fixture();
        let mut v = to_value(&file);
        event_mut(&mut v, 0)
            .as_object_mut()
            .expect("event object")
            .remove(field);
        assert!(
            serde_json::from_value::<V3File>(v).is_err(),
            "missing {field} must be rejected"
        );
    }
}

#[test]
fn optional_v3_fields_have_explicit_defaults() {
    let (file, _) = fixture();
    let mut v = to_value(&file);
    let event = event_mut(&mut v, 0);
    for field in ["occurrence", "refs", "modality", "covers", "quality", "attempt"] {
        event.as_object_mut().expect("event object").remove(field);
    }
    let back: V3File = serde_json::from_value(v).expect("defaults must parse");
    let ev = &back.sessions[0].events[0];
    assert_eq!(ev.occurrence, 0);
    assert!(ev.refs.is_empty());
    assert_eq!(ev.modality, Modality::Actual);
    assert!(ev.covers.is_empty());
    assert!(ev.quality.is_none());
    assert_eq!(ev.attempt, 0);
    assert!(ev.final_);
}

#[test]
fn evidence_eligibility_excludes_summary_injected_quarantine_and_non_final() {
    let (file, _) = fixture();
    let base = file.sessions[0].events[0].clone();
    assert!(is_evidence_eligible(&base));

    let mut summary = base.clone();
    summary.kind = EventKind::Summary;
    summary.summary = true;
    assert!(!is_evidence_eligible(&summary));

    let mut injected = base.clone();
    injected.trust = Trust::Injected;
    assert!(!is_evidence_eligible(&injected));

    let mut quarantined = base.clone();
    quarantined.quality = Some(EventQuality {
        flags: vec![QualityFlag::MalformedJson],
        quarantine: true,
        reason: Some("half line".into()),
        retryable: true,
    });
    assert!(!is_evidence_eligible(&quarantined));

    let mut non_final = base;
    non_final.final_ = false;
    assert!(!is_evidence_eligible(&non_final));
}

fn report_of(v: &Value) -> V3Report {
    let file: V3File = serde_json::from_value(v.clone()).expect("fixture must parse");
    validate_ingest_v3(&file)
}

fn assert_error(v: Value, code: &str) {
    let report = report_of(&v);
    assert!(!report.ok, "expected {code}, got ok; warnings={:?}", report.warnings);
    assert!(
        report.errors.iter().any(|issue| issue.code == code),
        "expected error {code}, got {:?}",
        report.errors
    );
}

fn assert_warning(v: &Value, code: &str) {
    let report = report_of(v);
    assert!(
        report.warnings.iter().any(|issue| issue.code == code),
        "expected warning {code}, got {:?}",
        report.warnings
    );
}

fn valid_policy() -> Value {
    json!({
        "default_deny": true,
        "require_grant_for_cross_agent": true,
        "enforcement_points": ["hub", "mcp", "expand", "export", "search"],
        "service_principals": [
            {"id": "extractor", "kind": "extractor",
             "capabilities": ["read_transcript", "derive"]},
            {"id": "backup", "kind": "backup",
             "capabilities": ["backup_read", "backup_write"]},
            {"id": "consumer", "kind": "consumer",
             "capabilities": ["read_memory"]}
        ]
    })
}

fn valid_manifest() -> Value {
    json!({
        "envelope_version": 1,
        "keys": [
            {"key_id": "key-hub", "kind": "hub", "version": 1, "fingerprint": "11111111",
             "status": "active", "created_at_ms": 1700000000000i64, "custody": "kms"},
            {"key_id": "key-device", "kind": "device", "version": 1, "fingerprint": "22222222",
             "status": "active", "created_at_ms": 1700000000000i64, "custody": "file"},
            {"key_id": "key-agent", "kind": "agent", "version": 1, "fingerprint": "33333333",
             "status": "active", "created_at_ms": 1700000000000i64, "custody": "file",
             "owner_agent_id": "agent"},
            {"key_id": "key-core", "kind": "core", "version": 1, "fingerprint": "44444444",
             "status": "active", "created_at_ms": 1700000000000i64, "custody": "file",
             "owner_agent_id": "agent"},
            {"key_id": "key-secret", "kind": "secret", "version": 1, "fingerprint": "55555555",
             "status": "active", "created_at_ms": 1700000000000i64, "custody": "file",
             "owner_agent_id": "agent"},
            {"key_id": "key-backup", "kind": "backup", "version": 1, "fingerprint": "66666666",
             "status": "active", "created_at_ms": 1700000000000i64, "custody": "offline"}
        ],
        "backup": {"separate_channel": true, "recovery_required": true,
                   "key_ids": ["key-backup"]}
    })
}

#[test]
fn base_fixture_passes_core_gate_with_p0_warnings() {
    let (file, _) = fixture();
    let report = validate_ingest_v3(&file);
    assert!(report.ok, "errors: {:?}", report.errors);
    assert_eq!(report.stats.sessions, 1);
    assert_eq!(report.stats.events, 4);
    assert_eq!(report.stats.human_msgs, 1);
    assert_eq!(report.stats.agent_msgs, 1);
    assert_eq!(report.stats.tool_calls, 1);
    assert_eq!(report.stats.tool_results, 1);
    assert_eq!(report.stats.time_unknown, 0);
    assert!(report
        .warnings
        .iter()
        .any(|issue| issue.code == "policy_absent"));
    assert!(report
        .warnings
        .iter()
        .any(|issue| issue.code == "key_manifest_absent"));
}

#[test]
fn production_gate_requires_policy_and_key_manifest() {
    let (file, _) = fixture();
    let report = validate_ingest_v3_with(&file, true);
    assert!(!report.ok);
    assert!(report.errors.iter().any(|issue| issue.code == "policy_absent"));
    assert!(report
        .errors
        .iter()
        .any(|issue| issue.code == "key_manifest_absent"));
}

#[test]
fn valid_p0_blocks_pass_production_gate() {
    let (file, _) = fixture();
    let mut v = to_value(&file);
    v["policy"] = valid_policy();
    v["key_manifest"] = valid_manifest();
    let parsed: V3File = serde_json::from_value(v).expect("p0 fixture must parse");
    let report = validate_ingest_v3_with(&parsed, true);
    assert!(report.ok, "errors: {:?}", report.errors);
    assert!(report.warnings.is_empty(), "warnings: {:?}", report.warnings);
}

#[test]
fn version_and_identity_mismatches_are_rejected() {
    let (file, _) = fixture();
    let mut v = to_value(&file);
    v["format_version"] = json!(2);
    assert_error(v, "format_version");

    for (field, code) in [
        ("event_uid", "event_uid_mismatch"),
        ("thread_uid", "unknown_thread"),
        ("turn_uid", "unknown_turn"),
        ("content_hash", "content_hash_mismatch"),
        ("text_sha256", "text_sha256_mismatch"),
        ("revision_id", "revision_mismatch"),
    ] {
        let (file, _) = fixture();
        let mut v = to_value(&file);
        event_mut(&mut v, 0)[field] = json!("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
        assert_error(v, code);
    }
}

#[test]
fn span_rules_are_enforced() {
    let (file, _) = fixture();
    let mut v = to_value(&file);
    event_mut(&mut v, 0)["span"]["end"] = json!(999);
    assert_error(v, "bad_span");

    let (file, _) = fixture();
    let mut v = to_value(&file);
    event_mut(&mut v, 0)["span"]["text_sha256"] = json!("0".repeat(64));
    assert_error(v, "span_hash_mismatch");

    let (file, _) = fixture();
    let mut v = to_value(&file);
    event_mut(&mut v, 0)["text"] = json!(" 把回访表补齐 ");
    assert_error(v, "text_not_normalized");
}

#[test]
fn sequence_speaker_and_tool_pair_rules_are_enforced() {
    let (file, _) = fixture();
    let mut v = to_value(&file);
    event_mut(&mut v, 3)["seq"] = json!(5);
    assert_error(v, "seq_not_contiguous");

    let (file, _) = fixture();
    let mut v = to_value(&file);
    event_mut(&mut v, 0)["speaker_id"] = json!("nobody");
    assert_error(v, "unknown_speaker");

    let (file, _) = fixture();
    let mut v = to_value(&file);
    event_mut(&mut v, 2)["tool_result"]["call_id"] = json!("c9");
    assert_error(v, "orphan_tool_result");

    let (file, _) = fixture();
    let mut v = to_value(&file);
    event_mut(&mut v, 1)["tool"]["name"] = json!("unknown_tool");
    assert_warning(&v, "tool_not_in_roster");
}

#[test]
fn parent_and_ref_resolution_rules_are_enforced() {
    let (file, ids) = fixture();
    let mut v = to_value(&file);
    event_mut(&mut v, 1)["parent_event_uid"] = json!(ids.event_uids[3]);
    assert_error(v, "parent_mismatch");

    let (file, _) = fixture();
    let mut v = to_value(&file);
    event_mut(&mut v, 0)["refs"] = json!([{"kind": "doc", "id": "doc-missing"}]);
    assert_error(v, "unresolved_doc_ref");

    let (file, _) = fixture();
    let mut v = to_value(&file);
    event_mut(&mut v, 0)["refs"] = json!([{"kind": "secret", "id": "sec-agent-0001"}]);
    assert_error(v, "unresolved_secret_ref");
}

#[test]
fn summary_rules_are_enforced() {
    let (file, _) = fixture();
    let mut v = to_value(&file);
    let thread_uid = v["sessions"][0]["threads"][0]["thread_uid"]
        .as_str()
        .unwrap()
        .to_string();
    let summary_text = "摘要";
    let summary_hash = compute_content_hash("summary", summary_text, None, None, None);
    let summary_uid = compute_event_uid(&thread_uid, "n4", &summary_hash, 0);
    let summary = event_mut(&mut v, 3);
    summary["kind"] = json!("summary");
    summary["summary"] = json!(true);
    summary["text"] = json!(summary_text);
    summary["text_sha256"] = json!(text_sha256(summary_text));
    summary["span"] = json!({"start": 0, "end": 2, "text_sha256": text_sha256(summary_text)});
    summary["content_hash"] = json!(summary_hash);
    summary["event_uid"] = json!(summary_uid);
    assert_error(v, "summary_covers_missing");
}

#[test]
fn secret_material_and_ownership_rules_are_enforced() {
    let (file, _) = fixture();
    let mut v = to_value(&file);
    event_mut(&mut v, 0)["text"] = json!("key=sk-abcdefghij");
    event_mut(&mut v, 0)["text_sha256"] = json!(text_sha256("key=sk-abcdefghij"));
    event_mut(&mut v, 0)["span"] = json!({"start": 0, "end": 27,
        "text_sha256": text_sha256("key=sk-abcdefghij")});
    assert_error(v, "secret_material");

    let (file, _) = fixture();
    let mut v = to_value(&file);
    v["sessions"][0]["ownership"]["owner_agent_id"] = json!("A");
    assert_error(v, "owner_not_agent");
}

#[test]
fn policy_rules_are_enforced() {
    let (file, _) = fixture();
    let mut v = to_value(&file);
    v["policy"] = valid_policy();
    v["policy"]["default_deny"] = json!(false);
    assert_error(v, "policy_default_allow");

    let (file, _) = fixture();
    let mut v = to_value(&file);
    v["policy"] = valid_policy();
    v["policy"]["enforcement_points"] = json!(["hub", "mcp", "expand", "export"]);
    assert_error(v, "missing_enforcement_point");

    let (file, _) = fixture();
    let mut v = to_value(&file);
    v["policy"] = valid_policy();
    v["policy"]["service_principals"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": "extractor", "kind": "consumer", "capabilities": ["read_memory"]}));
    assert_error(v, "duplicate_service_principal");
}

#[test]
fn key_manifest_rules_are_enforced() {
    let (file, _) = fixture();
    let mut v = to_value(&file);
    v["key_manifest"] = valid_manifest();
    v["key_manifest"]["backup"]["separate_channel"] = json!(false);
    assert_error(v, "backup_key_not_separate");

    let (file, _) = fixture();
    let mut v = to_value(&file);
    v["key_manifest"] = valid_manifest();
    let keys = v["key_manifest"]["keys"].as_array_mut().unwrap();
    let mut duplicate = keys[2].clone();
    duplicate["key_id"] = json!("key-agent-2");
    keys.push(duplicate);
    assert_error(v, "duplicate_key_version");

    let (file, _) = fixture();
    let mut v = to_value(&file);
    v["key_manifest"] = valid_manifest();
    v["key_manifest"]["keys"][5]["custody"] = json!("file");
    assert_error(v, "backup_key_custody");
}

#[test]
fn quality_rules_are_enforced() {
    let (file, _) = fixture();
    let mut v = to_value(&file);
    event_mut(&mut v, 0)["quality"] =
        json!({"flags": ["malformed_json"], "quarantine": false});
    assert_error(v, "hard_flag_not_quarantined");

    let (file, _) = fixture();
    let mut v = to_value(&file);
    event_mut(&mut v, 0)["quality"] =
        json!({"flags": ["malformed_json"], "quarantine": true, "reason": ""});
    assert_error(v, "quarantine_reason_required");
}

#[test]
fn replication_trust_rules_are_enforced() {
    let (file, _) = fixture();
    let mut v = to_value(&file);
    v["replication"] = json!({
        "epoch": 2, "commit_seq": 5, "prev_commit_seq": 5, "center_id": "center-1",
        "fencing_token": 2, "signed_manifest_sha256": "a".repeat(64)
    });
    assert_error(v, "commit_seq_not_monotonic");

    let (file, _) = fixture();
    let mut v = to_value(&file);
    v["replication"] = json!({
        "epoch": 2, "commit_seq": 5, "prev_commit_seq": 4, "center_id": "center-1",
        "fencing_token": 1, "signed_manifest_sha256": "a".repeat(64)
    });
    assert_error(v, "fencing_mismatch");

    let (file, _) = fixture();
    let mut v = to_value(&file);
    v["replication"] = json!({
        "epoch": 2, "commit_seq": 5, "prev_commit_seq": 4, "center_id": "center-1",
        "fencing_token": 2, "signed_manifest_sha256": "a".repeat(64),
        "promotion_cert": {
            "new_center_id": "center-1", "new_pubkey_fingerprint": "abcdef12",
            "prev_epoch": 0, "new_epoch": 2, "signed_by_prev_center": true,
            "signature": "c2lnbmF0dXJlLWNlcnQ="
        }
    });
    assert_error(v, "promotion_epoch_chain");
}

#[test]
fn revoked_secret_ref_is_rejected_but_quarantine_record_keeps_no_body() {
    let (file, _) = fixture();
    let mut v = to_value(&file);
    v["sessions"][0]["secrets"] = json!([{
        "secret_id": "sec-agent-0001", "alias": "minimax_key", "version": 1,
        "fingerprint": "abcdef12", "status": "revoked", "owner_agent_id": "agent"
    }]);
    event_mut(&mut v, 0)["refs"] =
        json!([{"kind": "secret", "id": "sec-agent-0001", "alias": "minimax_key"}]);
    assert_error(v, "revoked_secret_ref");

    let (file, _) = fixture();
    let mut v = to_value(&file);
    event_mut(&mut v, 0)["quality"] = json!({
        "flags": ["malformed_json"], "quarantine": true, "reason": "half line"
    });
    let report = report_of(&v);
    assert!(report.ok, "errors: {:?}", report.errors);
    assert_eq!(report.stats.quarantined_events, 1);
    assert_eq!(report.quarantined.len(), 1);
    assert!(report.quarantined[0].reason_present);
    assert_eq!(report.quarantined[0].flags, vec![QualityFlag::MalformedJson]);
}

#[test]
fn backup_key_manifest_cross_checks_are_enforced() {
    let (file, _) = fixture();
    let mut v = to_value(&file);
    v["key_manifest"] = valid_manifest();
    v["key_manifest"]["backup"]["key_ids"] = json!([]);
    assert_error(v, "backup_key_missing");

    let (file, _) = fixture();
    let mut v = to_value(&file);
    v["key_manifest"] = valid_manifest();
    v["key_manifest"]["keys"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "key_id": "key-backup-2", "kind": "backup", "version": 1,
            "fingerprint": "77777777", "status": "active",
            "created_at_ms": 1700000000000i64, "custody": "offline"
        }));
    assert_error(v, "backup_key_unlisted");

    let (file, _) = fixture();
    let mut v = to_value(&file);
    v["key_manifest"] = valid_manifest();
    v["key_manifest"]["keys"][5]["fingerprint"] = json!("33333333");
    assert_error(v, "backup_key_fingerprint_shared");
}

#[test]
fn dropped_and_session_quality_counts_are_checked() {
    let (file, _) = fixture();
    let mut v = to_value(&file);
    v["sessions"][0]["dropped"]["other"] = json!({"bad key!": 1});
    assert_error(v, "bad_dropped_key");

    let (file, _) = fixture();
    let mut v = to_value(&file);
    v["sessions"][0]["quality"]["quarantined_events"] = json!(1);
    assert_warning(&v, "quarantined_count_mismatch");
}

