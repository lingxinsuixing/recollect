//! Print fixed v3 identity vectors for the Python converter self-test.
//!
//! Run: cargo run -q -p rc-contract --example identity_vectors_v3

use rc_contract::identity_v3::{
    compute_content_hash, compute_event_uid, compute_revision_id, compute_session_uid,
    compute_thread_uid, compute_turn_uid_from_anchor, compute_turn_uid_native, text_sha256,
};

const SOURCE_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn main() {
    let session_uid = compute_session_uid("test-agent", "device-1", "session-1", Some("inst-1"));
    let thread_uid = compute_thread_uid(&session_uid, "main");
    let anchor = "event-anchor";
    let turn_uid_anchor = compute_turn_uid_from_anchor(&thread_uid, anchor);
    let turn_uid_native = compute_turn_uid_native(&thread_uid, "turn-7");
    let content_text = "检查一下 MCP";
    let content_hash = compute_content_hash(
        "human_msg",
        content_text,
        Some(("call-1", "search")),
        Some(("call-1", "ok", "找到 3 条")),
        Some(("doc-abc", Some((0, 4)))),
    );
    let event_uid = compute_event_uid(&thread_uid, "native-1", &content_hash, 2);
    let revision_id = compute_revision_id(SOURCE_SHA, 1, 3);
    let text_hash = text_sha256(content_text);

    let out = serde_json::json!({
        "session_uid_args": ["test-agent", "device-1", "session-1", "inst-1"],
        "thread_uid_args": [session_uid, "main"],
        "turn_uid_anchor_args": [thread_uid, anchor],
        "turn_uid_native_args": [thread_uid, "turn-7"],
        "event_uid_args": [thread_uid, "native-1", content_hash, 2],
        "revision_id_args": [SOURCE_SHA, 1, 3],
        "text": content_text,
        "content_kind": "human_msg",
        "content_text": content_text,
        "content_tool_call": ["call-1", "search"],
        "content_tool_result": ["call-1", "ok", "找到 3 条"],
        "content_doc_id": "doc-abc",
        "content_doc_span": [0, 4],
        "expected": {
            "session_uid": compute_session_uid("test-agent", "device-1", "session-1", Some("inst-1")),
            "thread_uid": compute_thread_uid(&session_uid, "main"),
            "turn_uid_anchor": turn_uid_anchor,
            "turn_uid_native": turn_uid_native,
            "event_uid": event_uid,
            "revision_id": revision_id,
            "text_sha256": text_hash,
            "content_hash": content_hash,
        },
    });
    println!("{}", serde_json::to_string_pretty(&out).expect("vectors serialize"));
}
