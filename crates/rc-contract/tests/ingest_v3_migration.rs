//! v2 → v3 迁移的 round-trip、旧指针与版本闸。

use rc_contract::identity_v3::{compute_revision_id, normalize_text_v3};
use rc_contract::ingest_v2::IngestFile;
use rc_contract::ingest_v3::*;
use rc_contract::ingest_v3_migrate::{migrate_v2_to_v3, LegacyPointerV2, MigrateOptions};
use serde_json::{json, Value};

const SOURCE_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn options() -> MigrateOptions {
    MigrateOptions {
        source_path: "fixture.v2.json".into(),
        source_sha256: SOURCE_SHA.into(),
        adapter_version: 1,
        ingest_version: INGEST_V3_FORMAT_VERSION,
        observed_at_ms: None,
    }
}

fn base_v2() -> Value {
    json!({
        "format_version": 2,
        "sessions": [{
            "session_key": "s1",
            "source_agent": "test",
            "device_id": "d1",
            "machine": "windows",
            "project_hint": "D:/proj",
            "timezone": "Asia/Shanghai",
            "participants": [
                {"id": "A", "kind": "human", "name": "user", "principal": null},
                {"id": "agent", "kind": "agent", "name": "test-agent", "principal": null},
                {"id": "tool", "kind": "tool", "name": "tool", "principal": null},
                {"id": "system", "kind": "system", "name": "system", "principal": null}
            ],
            "events": [
                {"seq": 1, "parent_seq": null, "kind": "human_msg", "speaker_id": "A",
                 "ts_ms": 1761474800000i64, "text": "把回访表补齐", "tool": null,
                 "tool_result": null, "doc": null, "summary": false, "thread": "main",
                 "source_ref": "u1"},
                {"seq": 2, "parent_seq": 1, "kind": "tool_call", "speaker_id": "agent",
                 "ts_ms": 1761474801000i64, "text": "", "tool": {"call_id": "c1",
                 "name": "search", "arguments": {"q": "回访表"}}, "tool_result": null,
                 "doc": null, "summary": false, "thread": "main", "source_ref": "u2"},
                {"seq": 3, "parent_seq": 2, "kind": "tool_result", "speaker_id": "tool",
                 "ts_ms": 1761474802000i64, "text": "",
                 "tool_result": {"call_id": "c1", "status": "ok", "text": "找到 3 条",
                 "artifact_ref": null}, "tool": null, "doc": null, "summary": false,
                 "thread": "main", "source_ref": "u3"},
                {"seq": 4, "parent_seq": 1, "kind": "agent_msg", "speaker_id": "agent",
                 "ts_ms": 1761474803000i64, "text": "已列缺字段", "tool": null,
                 "tool_result": null, "doc": null, "summary": false, "thread": "main",
                 "source_ref": "u4"}
            ]
        }]
    })
}

fn migrated(v: &Value) -> V3File {
    let v2: IngestFile = serde_json::from_value(v.clone()).expect("v2 fixture must parse");
    migrate_v2_to_v3(&v2, &options()).expect("migration must succeed")
}

fn push_event(v: &mut Value, event: Value) {
    v["sessions"][0]["events"]
        .as_array_mut()
        .expect("events array")
        .push(event);
}

#[test]
fn migration_preserves_event_skeleton_and_passes_v3_gate() {
    let file = migrated(&base_v2());
    let report = validate_ingest_v3(&file);
    assert!(report.ok, "errors: {:?}", report.errors);
    let session = &file.sessions[0];
    assert_eq!(session.events.len(), 4);
    assert_eq!(session.events[0].kind, EventKind::HumanMsg);
    assert_eq!(session.events[0].text, normalize_text_v3("把回访表补齐"));
    assert_eq!(session.events[1].kind, EventKind::ToolCall);
    assert_eq!(session.events[2].kind, EventKind::ToolResult);
    assert_eq!(session.events[2].parent_seq, Some(2));
    assert_eq!(
        session.events[2].parent_event_uid.as_deref(),
        Some(session.events[1].event_uid.as_str())
    );
    assert_eq!(session.events[0].source_ref.native_id.as_deref(), Some("u1"));
    assert_eq!(session.threads.len(), 1);
    assert_eq!(session.threads[0].kind, ThreadKind::Main);
    assert_eq!(session.turns.len(), 1);
    assert!(session.turns[0].derived);
    assert_eq!(session.turns[0].start_seq, 1);
    assert_eq!(session.turns[0].end_seq, 4);
    assert_eq!(session.source.adapter_version, 1);
    assert_eq!(
        session.source.revision_id,
        compute_revision_id(SOURCE_SHA, 1, INGEST_V3_FORMAT_VERSION)
    );
    assert!(session.source.sha256 == SOURCE_SHA);
    assert_eq!(session.ownership.owner_agent_id, "agent");
    assert_eq!(session.environment.machine_id, "windows");
}

#[test]
fn migration_is_deterministic() {
    let a = migrated(&base_v2());
    let b = migrated(&base_v2());
    assert_eq!(
        serde_json::to_value(&a).unwrap(),
        serde_json::to_value(&b).unwrap()
    );
}

#[test]
fn legacy_pointer_resolves_read_only() {
    let file = migrated(&base_v2());
    let session = &file.sessions[0];
    let pointer = LegacyPointerV2 {
        session_key: "s1".into(),
        device_id: "d1".into(),
        seq: 3,
    };
    let resolved = session
        .resolve_legacy_pointer(&pointer)
        .expect("legacy pointer must resolve");
    assert!(resolved.read_only);
    assert_eq!(resolved.seq, 3);
    assert_eq!(resolved.event_uid, session.events[2].event_uid);
    assert_eq!(resolved.source_ref.native_id.as_deref(), Some("u3"));

    let wrong_device = LegacyPointerV2 {
        session_key: "s1".into(),
        device_id: "d2".into(),
        seq: 3,
    };
    assert!(session.resolve_legacy_pointer(&wrong_device).is_none());
    let wrong_seq = LegacyPointerV2 {
        session_key: "s1".into(),
        device_id: "d1".into(),
        seq: 99,
    };
    assert!(session.resolve_legacy_pointer(&wrong_seq).is_none());
}

#[test]
fn version_gate_and_bad_options_are_rejected() {
    let file = migrated(&base_v2());
    let mut value = serde_json::to_value(&file).unwrap();
    value["format_version"] = json!(2);
    let downgraded: V3File = serde_json::from_value(value).unwrap();
    let report = validate_ingest_v3(&downgraded);
    assert!(!report.ok);
    assert!(report.errors.iter().any(|issue| issue.code == "format_version"));

    let v2: IngestFile = serde_json::from_value(base_v2()).unwrap();
    let mut bad = options();
    bad.source_sha256 = "not-a-sha".into();
    assert!(migrate_v2_to_v3(&v2, &bad).is_err());

    let mut invalid_v2_value = base_v2();
    invalid_v2_value["format_version"] = json!(3);
    let invalid_v2: IngestFile = serde_json::from_value(invalid_v2_value).unwrap();
    assert!(migrate_v2_to_v3(&invalid_v2, &options()).is_err());
}

#[test]
fn summary_migration_derives_covers_and_marks_quality() {
    let mut v = base_v2();
    push_event(
        &mut v,
        json!({"seq": 5, "parent_seq": 4, "kind": "summary", "speaker_id": "system",
               "ts_ms": 1761474804000i64, "text": "前文摘要", "tool": null,
               "tool_result": null, "doc": null, "summary": true, "thread": "main",
               "source_ref": "u5"}),
    );
    let file = migrated(&v);
    let report = validate_ingest_v3(&file);
    assert!(report.ok, "errors: {:?}", report.errors);
    assert!(report
        .warnings
        .iter()
        .any(|issue| issue.code == "summary_covers_derived"));
    let summary = &file.sessions[0].events[4];
    assert_eq!(summary.kind, EventKind::Summary);
    assert_eq!(summary.covers.len(), 1);
    assert_eq!(summary.covers[0].seq_start, 1);
    assert_eq!(summary.covers[0].seq_end, 4);
    let quality = summary.quality.as_ref().expect("summary quality");
    assert!(quality.flags.contains(&QualityFlag::CoversDerived));
    assert!(!quality.quarantine);
}

#[test]
fn doc_ref_migrates_to_doc_registry() {
    let mut v = base_v2();
    push_event(
        &mut v,
        json!({"seq": 5, "parent_seq": 4, "kind": "doc_ref", "speaker_id": "system",
               "ts_ms": 1761474804000i64, "text": "", "tool": null,
               "tool_result": null,
               "doc": {"path": "C:/proj/spec.md", "sha256": null, "section": null,
                       "span": null},
               "summary": false, "thread": "main", "source_ref": "u5"}),
    );
    let file = migrated(&v);
    let report = validate_ingest_v3(&file);
    assert!(report.ok, "errors: {:?}", report.errors);
    let session = &file.sessions[0];
    assert_eq!(session.docs.len(), 1);
    assert!(session.docs[0].doc_id.starts_with("doc-"));
    let event = &session.events[4];
    assert_eq!(
        event.doc.as_ref().map(|doc| doc.doc_id.as_str()),
        Some(session.docs[0].doc_id.as_str())
    );
}

#[test]
fn repeated_source_ref_becomes_distinct_occurrences() {
    let mut v = base_v2();
    push_event(
        &mut v,
        json!({"seq": 5, "parent_seq": 4, "kind": "agent_msg", "speaker_id": "agent",
               "ts_ms": 1761474804000i64, "text": "第二块", "tool": null,
               "tool_result": null, "doc": null, "summary": false, "thread": "main",
               "source_ref": "u4"}),
    );
    let file = migrated(&v);
    let report = validate_ingest_v3(&file);
    assert!(report.ok, "errors: {:?}", report.errors);
    let events = &file.sessions[0].events;
    assert_eq!(events[3].native_key, events[4].native_key);
    assert_eq!(events[3].occurrence, 0);
    assert_eq!(events[4].occurrence, 1);
    assert_ne!(events[3].event_uid, events[4].event_uid);
    assert!(events[3].final_ && events[4].final_);
}
