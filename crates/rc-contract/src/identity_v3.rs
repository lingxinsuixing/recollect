//! 身份与编号 v3：**身份稳定、位置可变**。
//!
//! - `session_uid/thread_uid/turn_uid/event_uid/revision_id` 是稳定身份；
//! - `seq/TRN/E VT` 的展示号由调用方按顺序生成，不参与身份哈希；
//! - 模型只看到短号；完整身份只给程序。
//!
//! 这些函数不改 v2 行为；v2 仍用 `identity::compute_turn_uid`。

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hash_fields(prefix: &str, fields: &[&str]) -> String {
    let mut h = Sha256::new();
    h.update(prefix.as_bytes());
    for field in fields {
        h.update((field.len() as u64).to_le_bytes());
        h.update(field.as_bytes());
    }
    hex(&h.finalize())
}

fn hash_owned_fields(prefix: &str, fields: &[String]) -> String {
    let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
    hash_fields(prefix, &refs)
}

/// v3 的存库文本口径：换行统一为 LF、NFC、去首尾空白。
/// 转换器必须先调用它，再计算 `text_sha256` 与 `span`；否则同一内容在不同语言里会漂移。
pub fn normalize_text_v3(raw: &str) -> String {
    let lf = raw.replace("\r\n", "\n").replace('\r', "\n");
    let nfc: String = lf.chars().nfc().collect();
    nfc.trim().to_string()
}

pub fn text_sha256(text: &str) -> String {
    hex(&Sha256::digest(text.as_bytes()))
}

pub fn char_count(text: &str) -> u64 {
    text.chars().count() as u64
}

/// Unicode 标量值偏移，0-based、end-exclusive。Rust 的 `chars()` 与 Python 的切片
/// 都按码点计数，所以同一段中文在两侧拿到的是同一个区间。
pub fn char_slice(text: &str, start: u64, end: u64) -> Option<&str> {
    let start = usize::try_from(start).ok()?;
    let end = usize::try_from(end).ok()?;
    if start > end {
        return None;
    }
    let mut offsets: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    offsets.push(text.len());
    if end >= offsets.len() {
        return None;
    }
    Some(&text[offsets[start]..offsets[end]])
}

/// 源有原生 turn 键时的稳定身份。`turn_key` 是源原生 id，不是展示序号。
pub fn compute_turn_uid_native(thread_uid: &str, turn_key: &str) -> String {
    hash_fields("rc.turn.native.v1", &[thread_uid, turn_key])
}

/// 事件内容哈希的 v3 口径：只覆盖语义上稳定、跨语言可复现的字段。
/// `arguments`/`details` 保留在载荷里，但不进身份；源文件变化由 `revision_id` 负责。
pub fn compute_content_hash(
    kind: &str,
    text: &str,
    tool_call: Option<(&str, &str)>,
    tool_result: Option<(&str, &str, &str)>,
    doc: Option<(&str, Option<(u64, u64)>)>,
) -> String {
    let mut fields = vec![kind.to_owned(), text.to_owned()];
    if let Some((call_id, name)) = tool_call {
        fields.push("tool_call".into());
        fields.push(call_id.into());
        fields.push(name.into());
    }
    if let Some((call_id, status, result_text)) = tool_result {
        fields.push("tool_result".into());
        fields.push(call_id.into());
        fields.push(status.into());
        fields.push(result_text.into());
    }
    if let Some((doc_id, span)) = doc {
        fields.push("doc".into());
        fields.push(doc_id.into());
        if let Some((start, end)) = span {
            fields.push("span".into());
            fields.push(start.to_string());
            fields.push(end.to_string());
        }
    }
    hash_owned_fields("rc.event_content.v1", &fields)
}

/// 全局唯一的会话身份：产品 + 设备 + 源会话键 + 可选实例。
/// `session_key` 保留为源原生键；跨设备解析用这个。
pub fn compute_session_uid(
    source_agent: &str,
    device_id: &str,
    session_key: &str,
    agent_instance_id: Option<&str>,
) -> String {
    hash_fields(
        "rc.session.v1",
        &[source_agent, device_id, session_key, agent_instance_id.unwrap_or("")],
    )
}

/// 线程身份：主线程 `main`，子线程用源 native id 或显式 thread_key。
pub fn compute_thread_uid(session_uid: &str, thread_key: &str) -> String {
    hash_fields("rc.thread.v1", &[session_uid, thread_key])
}

/// 轮次身份：源有原生 turn id 就用它；没有就用“本轮第一条 user 消息的 event_uid”作锚。
pub fn compute_turn_uid_from_anchor(thread_uid: &str, anchor_event_uid: &str) -> String {
    hash_fields("rc.turn.v1", &[thread_uid, anchor_event_uid])
}

/// 事件身份：线程 + 源原生键 + 内容哈希 + occurrence（重复出现的区分）。
/// 同一源、同一 revision 重放得到同一 `event_uid`；`seq` 只负责顺序。
pub fn compute_event_uid(
    thread_uid: &str,
    native_key: &str,
    content_hash: &str,
    occurrence: u32,
) -> String {
    hash_fields(
        "rc.event.v1",
        &[thread_uid, native_key, content_hash, &occurrence.to_string()],
    )
}

/// 修订身份：源文件哈希 + adapter 版本 + ingest 版本。
/// 源变了就换 revision；旧事件与旧指针保留。
pub fn compute_revision_id(source_sha256: &str, adapter_version: u32, ingest_version: u32) -> String {
    hash_fields(
        "rc.revision.v1",
        &[source_sha256, &adapter_version.to_string(), &ingest_version.to_string()],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_hex64(s: &str) -> bool {
        s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
    }

    #[test]
    fn session_uid_is_stable_and_field_sensitive() {
        let a = compute_session_uid("claude-code", "dev-1", "sess-1", None);
        let b = compute_session_uid("claude-code", "dev-1", "sess-1", None);
        assert_eq!(a, b, "同输入必须同身份");
        assert!(is_hex64(&a));
        assert_ne!(a, compute_session_uid("qoder", "dev-1", "sess-1", None));
        assert_ne!(a, compute_session_uid("claude-code", "dev-2", "sess-1", None));
        assert_ne!(a, compute_session_uid("claude-code", "dev-1", "sess-2", None));
        assert_ne!(a, compute_session_uid("claude-code", "dev-1", "sess-1", Some("inst-1")));
    }

    #[test]
    fn thread_and_turn_are_scoped_by_parent() {
        let session = compute_session_uid("claude-code", "dev-1", "sess-1", None);
        let main = compute_thread_uid(&session, "main");
        let sub = compute_thread_uid(&session, "sub-1");
        assert_ne!(main, sub);
        assert!(is_hex64(&main));
        assert_ne!(
            compute_turn_uid_from_anchor(&main, "event-a"),
            compute_turn_uid_from_anchor(&sub, "event-a"),
            "同一锚在不同线程不是同一轮"
        );
    }

    #[test]
    fn event_uid_changes_with_content_and_occurrence() {
        let thread = compute_thread_uid(&compute_session_uid("qoder", "d", "s", None), "main");
        let a = compute_event_uid(&thread, "uuid-1", "hash-1", 0);
        assert_eq!(a, compute_event_uid(&thread, "uuid-1", "hash-1", 0));
        assert_ne!(a, compute_event_uid(&thread, "uuid-1", "hash-2", 0));
        assert_ne!(a, compute_event_uid(&thread, "uuid-1", "hash-1", 1));
        assert_ne!(a, compute_event_uid(&thread, "uuid-2", "hash-1", 0));
        assert!(is_hex64(&a));
    }

    #[test]
    fn revision_changes_with_source_or_versions() {
        let a = compute_revision_id("sha-a", 1, 1);
        assert_eq!(a, compute_revision_id("sha-a", 1, 1));
        assert_ne!(a, compute_revision_id("sha-b", 1, 1));
        assert_ne!(a, compute_revision_id("sha-a", 2, 1));
        assert_ne!(a, compute_revision_id("sha-a", 1, 2));
        assert!(is_hex64(&a));
    }

    #[test]
    fn text_normalization_is_lf_nfc_trim() {
        assert_eq!(normalize_text_v3("  a\r\nb\r  "), "a\nb");
        // e + combining acute -> single NFC scalar
        assert_eq!(normalize_text_v3("e\u{301}"), "\u{e9}");
        assert_eq!(normalize_text_v3("检查一下 MCP"), "检查一下 MCP");
    }

    #[test]
    fn text_hash_has_a_known_answer() {
        assert_eq!(text_sha256("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn char_offsets_are_code_points_and_end_exclusive() {
        let text = "检查一下 MCP";
        assert_eq!(char_count(text), 8);
        assert_eq!(char_slice(text, 0, 2), Some("检查"));
        assert_eq!(char_slice(text, 2, 4), Some("一下"));
        assert_eq!(char_slice(text, 8, 8), Some(""));
        assert_eq!(char_slice(text, 0, 9), None);
        assert_eq!(char_slice(text, 3, 2), None);
    }

    #[test]
    fn native_turn_uid_is_thread_scoped() {
        let a = compute_turn_uid_native("thr-a", "u1");
        assert_eq!(a, compute_turn_uid_native("thr-a", "u1"));
        assert_ne!(a, compute_turn_uid_native("thr-b", "u1"));
        assert_ne!(a, compute_turn_uid_native("thr-a", "u2"));
        assert!(is_hex64(&a));
    }

    #[test]
    fn content_hash_is_stable_and_field_sensitive() {
        let base = compute_content_hash("human_msg", "你好", None, None, None);
        assert_eq!(base, compute_content_hash("human_msg", "你好", None, None, None));
        assert_ne!(base, compute_content_hash("agent_msg", "你好", None, None, None));
        assert_ne!(base, compute_content_hash("human_msg", "再见", None, None, None));
        assert_ne!(
            base,
            compute_content_hash("human_msg", "你好", Some(("c1", "search")), None, None)
        );
        assert_ne!(
            base,
            compute_content_hash("human_msg", "你好", None, Some(("c1", "ok", "x")), None)
        );
        assert_ne!(
            base,
            compute_content_hash("human_msg", "你好", None, None, Some(("doc-1", Some((0, 2)))))
        );
        assert!(is_hex64(&base));
    }
}
