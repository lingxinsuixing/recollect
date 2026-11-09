//! 入库契约 v2 的严格闸。

use rc_contract::ingest_v2::{validate_ingest, IngestFile};
use serde_json::{json, Value};

fn base_value() -> Value {
    json!({
        "format_version": 2,
        "sessions": [{
            "session_key": "s1",
            "source_agent": "test",
            "device_id": "d1",
            "machine": null,
            "project_hint": null,
            "timezone": "Asia/Shanghai",
            "participants": [
                {"id": "A", "kind": "human", "name": null, "principal": null},
                {"id": "agent", "kind": "agent", "name": null, "principal": null},
                {"id": "tool", "kind": "tool", "name": null, "principal": null}
            ],
            "events": [
                {"seq": 1, "parent_seq": null, "kind": "human_msg", "speaker_id": "A",
                 "ts_ms": 1761474800000i64, "text": "把回访表补齐", "tool": null,
                 "tool_result": null, "doc": null, "summary": false, "thread": null,
                 "source_ref": null},
                {"seq": 2, "parent_seq": 1, "kind": "tool_call", "speaker_id": "agent",
                 "ts_ms": 1761474801000i64, "text": "", "tool": {"call_id": "c1",
                 "name": "search", "arguments": {"q": "回访表"}}, "tool_result": null,
                 "doc": null, "summary": false, "thread": null, "source_ref": null},
                {"seq": 3, "parent_seq": 2, "kind": "tool_result", "speaker_id": "tool",
                 "ts_ms": 1761474802000i64, "text": "",
                 "tool_result": {"call_id": "c1", "status": "ok", "text": "找到 3 条",
                 "artifact_ref": null}, "tool": null, "doc": null, "summary": false,
                 "thread": null, "source_ref": null},
                {"seq": 4, "parent_seq": 1, "kind": "agent_msg", "speaker_id": "agent",
                 "ts_ms": 1761474803000i64, "text": "已列缺字段", "tool": null,
                 "tool_result": null, "doc": null, "summary": false, "thread": null,
                 "source_ref": null}
            ]
        }]
    })
}

fn parse(v: Value) -> IngestFile {
    serde_json::from_value(v).expect("fixture must deserialize")
}

fn event_mut(v: &mut Value, idx: usize) -> &mut Value {
    &mut v["sessions"][0]["events"][idx]
}

#[test]
fn valid_file_passes_and_counts_tool_pair() {
    let rep = validate_ingest(&parse(base_value()));
    assert!(rep.ok, "errors: {:?}", rep.errors);
    assert!(rep.warnings.is_empty(), "warnings: {:?}", rep.warnings);
    assert_eq!(rep.stats.events, 4);
    assert_eq!(rep.stats.tool_calls, 1);
    assert_eq!(rep.stats.tool_results, 1);
    assert_eq!(rep.stats.time_unknown, 0);
}

#[test]
fn seq_gap_is_rejected() {
    let mut v = base_value();
    event_mut(&mut v, 3)["seq"] = json!(5);
    let rep = validate_ingest(&parse(v));
    assert!(!rep.ok);
    assert!(rep.errors.iter().any(|e| e.code == "seq_not_contiguous"));
}

#[test]
fn parent_must_precede_event() {
    let mut v = base_value();
    event_mut(&mut v, 1)["parent_seq"] = json!(4);
    let rep = validate_ingest(&parse(v));
    assert!(!rep.ok);
    assert!(rep.errors.iter().any(|e| e.code == "bad_parent"));
}

#[test]
fn unknown_speaker_is_rejected() {
    let mut v = base_value();
    event_mut(&mut v, 0)["speaker_id"] = json!("X");
    let rep = validate_ingest(&parse(v));
    assert!(!rep.ok);
    assert!(rep.errors.iter().any(|e| e.code == "unknown_speaker"));
}

#[test]
fn orphan_tool_result_is_rejected() {
    let mut v = base_value();
    event_mut(&mut v, 2)["tool_result"]["call_id"] = json!("c9");
    let rep = validate_ingest(&parse(v));
    assert!(!rep.ok);
    assert!(rep.errors.iter().any(|e| e.code == "orphan_tool_result"));
}

#[test]
fn empty_text_on_message_is_rejected() {
    let mut v = base_value();
    event_mut(&mut v, 0)["text"] = json!("   ");
    let rep = validate_ingest(&parse(v));
    assert!(!rep.ok);
    assert!(rep.errors.iter().any(|e| e.code == "empty_text"));
}

#[test]
fn high_confidence_secret_is_rejected() {
    let mut v = base_value();
    event_mut(&mut v, 0)["text"] = json!("key=sk-abcdefghij");
    let rep = validate_ingest(&parse(v));
    assert!(!rep.ok);
    assert!(rep.errors.iter().any(|e| e.code == "secret_material"));
    assert_eq!(rep.stats.secret_hits, 1);
}

#[test]
fn time_going_backwards_is_only_a_warning() {
    let mut v = base_value();
    event_mut(&mut v, 3)["ts_ms"] = json!(1761474700000i64);
    let rep = validate_ingest(&parse(v));
    assert!(rep.ok, "errors: {:?}", rep.errors);
    assert!(rep.warnings.iter().any(|e| e.code == "time_order"));
}

#[test]
fn summary_kind_requires_flag() {
    let mut v = base_value();
    event_mut(&mut v, 0)["kind"] = json!("summary");
    let rep = validate_ingest(&parse(v));
    assert!(!rep.ok);
    assert!(rep.errors.iter().any(|e| e.code == "summary_flag_missing"));
}

#[test]
fn unknown_field_is_rejected_at_parse() {
    let mut v = base_value();
    event_mut(&mut v, 0)["extra"] = json!(1);
    assert!(serde_json::from_value::<IngestFile>(v).is_err());
}

#[test]
fn duplicate_call_id_is_rejected() {
    let mut v = base_value();
    let dup = json!({"seq": 1, "parent_seq": null, "kind": "tool_call", "speaker_id": "agent",
        "ts_ms": 1761474800000i64, "text": "", "tool": {"call_id": "c1", "name": "search",
        "arguments": {"q": "x"}}, "tool_result": null, "doc": null, "summary": false,
        "thread": null, "source_ref": null});
    v["sessions"][0]["events"].as_array_mut().unwrap().insert(1, dup);
    let rep = validate_ingest(&parse(v));
    assert!(!rep.ok);
    assert!(rep.errors.iter().any(|e| e.code == "duplicate_call_id"));
}
