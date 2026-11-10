//! 入库契约 v2：无损事件流（**历史信封，迁移专用**）。
//!
//! 本模块今天只服务两件事：v2→v3 的迁移（[`crate::ingest_v3_migrate`]）与回归夹具；
//! 生产入库路径不再直接消费 v2。三份契约不是三条管线，是**同一语义核心的三个信封代际**：
//! - v1（`ExchangeFile`）＝语义核心：`hub import` 的 `parse`/`normalize`/`commit` 与
//!   库表结构只认它，冻结不动；
//! - v2（本模块）＝一代更富的"无损事件流"设计，**没有出过生产者**，被 v3 取代，只留迁移（另有回归夹具）；
//! - v3（[`crate::ingest_v3`]）＝现役生产者信封；`hub import` 经
//!   [`crate::ingest_v3_project::project_v3_to_v1`] 把它投回 v1（富字段丢弃并逐字段计数），
//!   投影产物照走 v1 的同一套校验/落库。
//!
//! 引擎只认 v1 的 `ExchangeFile`；向各 Agent 侧暴露的条目请按 v3 产，不要新写 v2。

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub const INGEST_FORMAT_VERSION: u32 = 2;

const TS_MIN: i64 = 946_684_800_000; // 2000-01-01
const TS_MAX: i64 = 4_102_444_800_000; // 2100-01-01

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IngestFile {
    pub format_version: u32,
    pub sessions: Vec<IngestSession>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IngestSession {
    pub session_key: String,
    pub source_agent: String,
    pub device_id: String,
    pub machine: Option<String>,
    pub project_hint: Option<String>,
    pub timezone: Option<String>,
    pub participants: Vec<Participant>,
    pub events: Vec<IngestEvent>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Participant {
    pub id: String,
    pub kind: ParticipantKind,
    pub name: Option<String>,
    pub principal: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantKind {
    Human,
    Agent,
    Tool,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    HumanMsg,
    AgentMsg,
    ToolCall,
    ToolResult,
    DocRef,
    Summary,
    System,
    Attachment,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IngestEvent {
    pub seq: u32,
    pub parent_seq: Option<u32>,
    pub kind: EventKind,
    pub speaker_id: String,
    pub ts_ms: Option<i64>,
    #[serde(default)]
    pub text: String,
    pub tool: Option<ToolCall>,
    pub tool_result: Option<ToolResult>,
    pub doc: Option<DocRef>,
    #[serde(default)]
    pub summary: bool,
    pub thread: Option<String>,
    pub source_ref: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    pub call_id: String,
    pub status: ToolStatus,
    pub text: Option<String>,
    pub artifact_ref: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Ok,
    Error,
    Unknown,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocRef {
    pub path: String,
    pub sha256: Option<String>,
    pub section: Option<String>,
    pub span: Option<DocSpan>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocSpan {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct IngestStats {
    pub sessions: usize,
    pub events: usize,
    pub human_msgs: usize,
    pub agent_msgs: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub doc_refs: usize,
    pub summaries: usize,
    pub attachments: usize,
    pub time_unknown: usize,
    pub secret_hits: usize,
    pub pii_hits: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IngestIssue {
    pub code: &'static str,
    pub session: Option<String>,
    pub seq: Option<u32>,
    pub field: Option<&'static str>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct IngestReport {
    pub ok: bool,
    pub errors: Vec<IngestIssue>,
    pub warnings: Vec<IngestIssue>,
    pub stats: IngestStats,
}

impl IngestReport {
    pub fn error(&mut self, code: &'static str, session: &str, seq: Option<u32>, field: Option<&'static str>, message: impl Into<String>) {
        self.errors.push(IngestIssue { code, session: Some(session.to_string()), seq, field, message: message.into() });
    }
    pub fn warn(&mut self, code: &'static str, session: &str, seq: Option<u32>, field: Option<&'static str>, message: impl Into<String>) {
        self.warnings.push(IngestIssue { code, session: Some(session.to_string()), seq, field, message: message.into() });
    }
}

pub fn validate_ingest(file: &IngestFile) -> IngestReport {
    let mut rep = IngestReport { ok: false, errors: Vec::new(), warnings: Vec::new(), stats: IngestStats::default() };

    if file.format_version != INGEST_FORMAT_VERSION {
        rep.errors.push(IngestIssue {
            code: "format_version",
            session: None,
            seq: None,
            field: Some("format_version"),
            message: format!("expected {INGEST_FORMAT_VERSION}, got {}", file.format_version),
        });
        return rep;
    }
    if file.sessions.is_empty() {
        rep.errors.push(IngestIssue {
            code: "no_sessions",
            session: None,
            seq: None,
            field: Some("sessions"),
            message: "at least one session is required".into(),
        });
        return rep;
    }

    let mut seen_sessions: HashSet<&str> = HashSet::new();
    for session in &file.sessions {
        rep.stats.sessions += 1;
        validate_session(session, &mut seen_sessions, &mut rep);
    }
    rep.ok = rep.errors.is_empty();
    rep
}

fn validate_session<'a>(session: &'a IngestSession, seen_sessions: &mut HashSet<&'a str>, rep: &mut IngestReport) {
    let key = session.session_key.as_str();
    if key.trim().is_empty() {
        rep.error("empty_session_key", key, None, Some("session_key"), "session_key must be non-empty");
    } else if !seen_sessions.insert(key) {
        rep.error("duplicate_session_key", key, None, Some("session_key"), "session_key must be unique");
    }
    for (field, value) in [
        ("source_agent", &session.source_agent),
        ("device_id", &session.device_id),
    ] {
        if value.trim().is_empty() {
            rep.error("empty_field", key, None, Some(field), format!("{field} must be non-empty"));
        }
    }

    let mut kinds: HashMap<&str, ParticipantKind> = HashMap::new();
    for p in &session.participants {
        if p.id.trim().is_empty() {
            rep.error("empty_participant_id", key, None, Some("participants"), "participant id must be non-empty");
            continue;
        }
        if kinds.insert(p.id.as_str(), p.kind).is_some() {
            rep.error("duplicate_participant", key, None, Some("participants"), format!("duplicate participant id {}", p.id));
        }
    }
    if !kinds.values().any(|k| *k == ParticipantKind::Human) {
        rep.error("no_human_participant", key, None, Some("participants"), "at least one human participant is required");
    }
    if !kinds.values().any(|k| *k == ParticipantKind::Agent) {
        rep.error("no_agent_participant", key, None, Some("participants"), "at least one agent participant is required");
    }
    if session.events.is_empty() {
        rep.error("no_events", key, None, Some("events"), "at least one event is required");
        return;
    }

    let mut calls: HashMap<&str, u32> = HashMap::new();
    let mut last_ts: Option<i64> = None;
    for (index, event) in session.events.iter().enumerate() {
        let expect = (index + 1) as u32;
        rep.stats.events += 1;
        let seq = Some(event.seq);
        if event.seq != expect {
            rep.error("seq_not_contiguous", key, seq, Some("seq"), format!("expected {expect}, got {}", event.seq));
        }
        if let Some(parent) = event.parent_seq {
            if parent == 0 || parent >= event.seq {
                rep.error("bad_parent", key, seq, Some("parent_seq"), format!("parent_seq {parent} must be 1..< seq"));
            }
        }
        if let Some(thread) = &event.thread {
            if thread.trim().is_empty() {
                rep.error("empty_thread", key, seq, Some("thread"), "thread must be non-empty when present");
            }
        }
        if let Some(src) = &event.source_ref {
            if src.trim().is_empty() {
                rep.error("empty_source_ref", key, seq, Some("source_ref"), "source_ref must be non-empty when present");
            }
        }

        let speaker_kind = kinds.get(event.speaker_id.as_str()).copied();
        match speaker_kind {
            None => rep.error("unknown_speaker", key, seq, Some("speaker_id"), format!("speaker_id {} is not a participant", event.speaker_id)),
            Some(kind) => {
                let ok = match event.kind {
                    EventKind::HumanMsg => kind == ParticipantKind::Human,
                    EventKind::AgentMsg => kind == ParticipantKind::Agent,
                    EventKind::ToolCall => kind == ParticipantKind::Agent,
                    EventKind::ToolResult => {
                        matches!(kind, ParticipantKind::Tool | ParticipantKind::System)
                    }
                    EventKind::DocRef | EventKind::Attachment => {
                        matches!(kind, ParticipantKind::Agent | ParticipantKind::Tool | ParticipantKind::System)
                    }
                    EventKind::Summary | EventKind::System => {
                        matches!(kind, ParticipantKind::System | ParticipantKind::Agent)
                    }
                };
                if !ok {
                    rep.error("speaker_kind_mismatch", key, seq, Some("speaker_id"), format!("participant {} cannot speak a {:?} event", event.speaker_id, event.kind));
                }
            }
        }

        if let Some(ts) = event.ts_ms {
            if !(TS_MIN..=TS_MAX).contains(&ts) {
                rep.error("bad_timestamp", key, seq, Some("ts_ms"), format!("ts_ms {ts} outside 2000..2100"));
            }
            if let Some(prev) = last_ts {
                if ts < prev {
                    rep.warn("time_order", key, seq, Some("ts_ms"), format!("ts_ms went backwards: {prev} -> {ts}; seq stays authoritative"));
                }
            }
            last_ts = Some(ts);
        } else {
            rep.stats.time_unknown += 1;
        }

        if event.summary && event.kind != EventKind::Summary {
            rep.error("summary_flag_on_non_summary", key, seq, Some("summary"), "summary=true is only allowed on kind=summary");
        }
        if event.kind == EventKind::Summary && !event.summary {
            rep.error("summary_flag_missing", key, seq, Some("summary"), "kind=summary requires summary=true");
        }

        match event.kind {
            EventKind::HumanMsg | EventKind::AgentMsg | EventKind::System => {
                rep.stats.human_msgs += usize::from(event.kind == EventKind::HumanMsg);
                rep.stats.agent_msgs += usize::from(event.kind == EventKind::AgentMsg);
                require_text(event, key, rep);
                reject_payload(event, key, rep);
            }
            EventKind::Summary => {
                rep.stats.summaries += 1;
                require_text(event, key, rep);
                reject_payload(event, key, rep);
            }
            EventKind::ToolCall => {
                rep.stats.tool_calls += 1;
                match &event.tool {
                    None => rep.error("missing_tool", key, seq, Some("tool"), "tool_call requires tool"),
                    Some(call) => {
                        if call.call_id.trim().is_empty() {
                            rep.error("empty_call_id", key, seq, Some("tool.call_id"), "call_id must be non-empty");
                        } else if calls.insert(call.call_id.as_str(), event.seq).is_some() {
                            rep.error("duplicate_call_id", key, seq, Some("tool.call_id"), format!("duplicate call_id {}", call.call_id));
                        }
                        if call.name.trim().is_empty() {
                            rep.error("empty_tool_name", key, seq, Some("tool.name"), "tool name must be non-empty");
                        }
                        if call.arguments.is_null() {
                            rep.error("null_tool_arguments", key, seq, Some("tool.arguments"), "tool arguments must not be null");
                        }
                    }
                }
                if event.tool_result.is_some() || event.doc.is_some() {
                    rep.error("extra_payload", key, seq, None, "tool_call cannot carry tool_result/doc");
                }
            }
            EventKind::ToolResult => {
                rep.stats.tool_results += 1;
                match &event.tool_result {
                    None => rep.error("missing_tool_result", key, seq, Some("tool_result"), "tool_result requires tool_result"),
                    Some(result) => {
                        if result.call_id.trim().is_empty() {
                            rep.error("empty_call_id", key, seq, Some("tool_result.call_id"), "call_id must be non-empty");
                        } else {
                            match calls.get(result.call_id.as_str()) {
                                None => rep.error("orphan_tool_result", key, seq, Some("tool_result.call_id"), format!("no earlier tool_call with call_id {}", result.call_id)),
                                Some(call_seq) if *call_seq >= event.seq => rep.error("tool_result_before_call", key, seq, Some("tool_result.call_id"), "tool_result must come after its call"),
                                Some(_) => {}
                            }
                        }
                        if result.text.as_deref().unwrap_or("").trim().is_empty()
                            && result.artifact_ref.as_deref().unwrap_or("").trim().is_empty()
                        {
                            rep.error("empty_tool_result", key, seq, Some("tool_result"), "tool_result needs text or artifact_ref");
                        }
                    }
                }
                if event.tool.is_some() || event.doc.is_some() {
                    rep.error("extra_payload", key, seq, None, "tool_result cannot carry tool/doc");
                }
            }
            EventKind::DocRef => {
                rep.stats.doc_refs += 1;
                match &event.doc {
                    None => rep.error("missing_doc", key, seq, Some("doc"), "doc_ref requires doc"),
                    Some(doc) => {
                        if doc.path.trim().is_empty() {
                            rep.error("empty_doc_path", key, seq, Some("doc.path"), "doc path must be non-empty");
                        }
                        if let Some(span) = &doc.span {
                            if span.end < span.start {
                                rep.error("bad_doc_span", key, seq, Some("doc.span"), "span.end must be >= span.start");
                            }
                        }
                    }
                }
                if event.tool.is_some() || event.tool_result.is_some() {
                    rep.error("extra_payload", key, seq, None, "doc_ref cannot carry tool/tool_result");
                }
            }
            EventKind::Attachment => {
                rep.stats.attachments += 1;
                if event.doc.is_none() && event.text.trim().is_empty() {
                    rep.error("empty_attachment", key, seq, None, "attachment needs doc or text");
                }
                if event.tool.is_some() || event.tool_result.is_some() {
                    rep.error("extra_payload", key, seq, None, "attachment cannot carry tool/tool_result");
                }
            }
        }

        scan_content(event, key, seq, rep);
    }
}

fn require_text(event: &IngestEvent, key: &str, rep: &mut IngestReport) {
    if event.text.trim().is_empty() {
        rep.error("empty_text", key, Some(event.seq), Some("text"), format!("{:?} requires text", event.kind));
    }
}

fn reject_payload(event: &IngestEvent, key: &str, rep: &mut IngestReport) {
    if event.tool.is_some() || event.tool_result.is_some() || event.doc.is_some() {
        rep.error("extra_payload", key, Some(event.seq), None, "this event kind cannot carry tool/tool_result/doc");
    }
}

fn scan_content(event: &IngestEvent, key: &str, seq: Option<u32>, rep: &mut IngestReport) {
    let mut parts: Vec<(&'static str, String)> = vec![("text", event.text.clone())];
    if let Some(call) = &event.tool {
        parts.push(("tool.arguments", call.arguments.to_string()));
    }
    if let Some(result) = &event.tool_result {
        if let Some(t) = &result.text {
            parts.push(("tool_result.text", t.clone()));
        }
        if let Some(a) = &result.artifact_ref {
            parts.push(("tool_result.artifact_ref", a.clone()));
        }
    }
    if let Some(doc) = &event.doc {
        parts.push(("doc.path", doc.path.clone()));
        if let Some(s) = &doc.section {
            parts.push(("doc.section", s.clone()));
        }
    }
    for (field, value) in parts {
        for kind in secret_hits(&value) {
            rep.stats.secret_hits += 1;
            rep.error("secret_material", key, seq, Some(field), format!("high-confidence secret pattern: {kind}"));
        }
        rep.stats.pii_hits += pii_hits(&value);
    }
}

pub fn secret_hits(text: &str) -> Vec<&'static str> {
    let mut hits = Vec::new();
    let mut push = |kind| {
        if !hits.contains(&kind) {
            hits.push(kind);
        }
    };
    for prefix in ["sk-", "cpk-"] {
        for (idx, _) in text.match_indices(prefix) {
            let rest = &text[idx + prefix.len()..];
            let n = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').count();
            if n >= 10 {
                push("api_key");
            }
        }
    }
    for (idx, _) in text.match_indices("AKIA") {
        let rest = &text[idx + 4..];
        let n = rest.chars().take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit()).count();
        if n >= 16 {
            push("aws_key");
        }
    }
    if text.contains("-----BEGIN ") && text.contains("PRIVATE KEY-----") {
        push("private_key");
    }
    for (idx, _) in text.match_indices("Bearer ") {
        let rest = &text[idx + 7..];
        let n = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '-').count();
        if n >= 10 {
            push("bearer_token");
        }
    }
    for marker in ["password=", "password:", "密码：", "密码:", "口令：", "口令:"] {
        for (idx, _) in text.match_indices(marker) {
            let rest = text[idx + marker.len()..].trim_start();
            if !rest.is_empty() && !rest.starts_with('<') {
                push("password");
            }
        }
    }
    hits
}

pub fn pii_hits(text: &str) -> usize {
    let mut hits = 0;
    for (idx, _) in text.match_indices('@') {
        let before = text[..idx].chars().rev().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+')).count();
        let after = text[idx + 1..].chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-')).count();
        if before >= 1 && after >= 3 && text[idx + 1..].contains('.') {
            hits += 1;
        }
    }
    hits + mobile_hits(text)
}

fn mobile_hits(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut hits = 0;
    let mut i = 0;
    while i + 11 <= bytes.len() {
        if bytes[i] == b'1' && bytes[i..i + 11].iter().all(|b| b.is_ascii_digit()) {
            let before_ok = i == 0 || !bytes[i - 1].is_ascii_digit();
            let after_ok = i + 11 == bytes.len() || !bytes[i + 11].is_ascii_digit();
            if before_ok && after_ok {
                hits += 1;
                i += 11;
                continue;
            }
        }
        i += 1;
    }
    hits
}
