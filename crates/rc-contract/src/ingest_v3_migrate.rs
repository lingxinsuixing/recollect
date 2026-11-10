//! v2 → v3 迁移。v2 只作为输入；输出必须过 v3 闸。
//!
//! 迁移的目标不是“补出源里没有的事实”，而是把 v2 已有事件无损搬进 v3 骨架：
//! `seq`、正文、说话人、工具对、文档指针、父链保持；身份按 v3 规则重算；
//! 环境/roster/threads/turns 从 v2 已有字段派生；取不到的字段留空或标 `derived`。

use crate::identity_v3::{
    char_count, compute_content_hash, compute_event_uid, compute_revision_id, compute_session_uid,
    compute_thread_uid, compute_turn_uid_from_anchor, normalize_text_v3, text_sha256,
};
use crate::ingest_v2::{
    validate_ingest, EventKind as V2EventKind, IngestEvent as V2Event, IngestFile as V2File,
    IngestSession as V2Session, ParticipantKind as V2ParticipantKind, ToolStatus as V2ToolStatus,
};
use crate::ingest_v3::*;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct MigrateOptions {
    pub source_path: String,
    pub source_sha256: String,
    pub adapter_version: u32,
    pub ingest_version: u32,
    /// 固定它可让迁移输出字节级可复现；不填则 v3 事件不带观察时间。
    pub observed_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MigrationIssue {
    pub code: &'static str,
    pub session: Option<String>,
    pub seq: Option<u32>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyPointerV2 {
    pub session_key: String,
    pub device_id: String,
    pub seq: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyResolution {
    pub event_uid: String,
    pub seq: u32,
    pub source_ref: SourceRef,
    pub read_only: bool,
}

impl V3Session {
    /// v2 指针只读解析：只按源会话键 + 设备 + seq 找事件，不改写任何编号。
    pub fn resolve_legacy_pointer(&self, pointer: &LegacyPointerV2) -> Option<LegacyResolution> {
        if pointer.session_key != self.session_key || pointer.device_id != self.device_id {
            return None;
        }
        self.events
            .iter()
            .find(|event| event.seq == pointer.seq)
            .map(|event| LegacyResolution {
                event_uid: event.event_uid.clone(),
                seq: event.seq,
                source_ref: event.source_ref.clone(),
                read_only: true,
            })
    }
}

pub fn migrate_v2_to_v3(
    v2: &V2File,
    options: &MigrateOptions,
) -> Result<V3File, Vec<MigrationIssue>> {
    let mut options_issues = Vec::new();
    validate_options(options, &mut options_issues);
    if !options_issues.is_empty() {
        return Err(options_issues);
    }

    let v2_report = validate_ingest(v2);
    if !v2_report.ok {
        return Err(v2_report
            .errors
            .into_iter()
            .map(|issue| MigrationIssue {
                code: issue.code,
                session: issue.session,
                seq: issue.seq,
                message: issue.message,
            })
            .collect());
    }

    let sessions: Vec<V3Session> = v2
        .sessions
        .iter()
        .map(|session| migrate_session(session, options))
        .collect();
    let file = V3File {
        format_version: INGEST_V3_FORMAT_VERSION,
        ingest_version: options.ingest_version,
        sessions,
        policy: None,
        key_manifest: None,
        replication: None,
    };

    let v3_report = validate_ingest_v3(&file);
    if !v3_report.ok {
        return Err(v3_report
            .errors
            .into_iter()
            .map(|issue| MigrationIssue {
                code: "migrate_invalid",
                session: issue.session,
                seq: issue.seq,
                message: format!("{}: {}", issue.code, issue.message),
            })
            .collect());
    }
    Ok(file)
}

fn validate_options(options: &MigrateOptions, issues: &mut Vec<MigrationIssue>) {
    if !is_hex64(&options.source_sha256) {
        issues.push(MigrationIssue {
            code: "bad_source_sha256",
            session: None,
            seq: None,
            message: "source_sha256 must be 64 hex characters".into(),
        });
    }
    if options.adapter_version == 0 {
        issues.push(MigrationIssue {
            code: "bad_adapter_version",
            session: None,
            seq: None,
            message: "adapter_version must be >= 1".into(),
        });
    }
    if options.ingest_version != INGEST_V3_FORMAT_VERSION {
        issues.push(MigrationIssue {
            code: "bad_ingest_version",
            session: None,
            seq: None,
            message: format!(
                "ingest_version must be {}",
                INGEST_V3_FORMAT_VERSION
            ),
        });
    }
    if options.source_path.trim().is_empty() {
        issues.push(MigrationIssue {
            code: "empty_source_path",
            session: None,
            seq: None,
            message: "source_path must be non-empty".into(),
        });
    }
    if let Some(observed) = options.observed_at_ms {
        if !(TS_MIN..=TS_MAX).contains(&observed) {
            issues.push(MigrationIssue {
                code: "bad_observed_at",
                session: None,
                seq: None,
                message: "observed_at_ms is outside 2000..2100".into(),
            });
        }
    }
}

fn migrate_session(session: &V2Session, options: &MigrateOptions) -> V3Session {
    let session_uid = compute_session_uid(
        &session.source_agent,
        &session.device_id,
        &session.session_key,
        None,
    );
    let revision_id = compute_revision_id(
        &options.source_sha256,
        options.adapter_version,
        options.ingest_version,
    );
    let agent_ids: Vec<String> = session
        .participants
        .iter()
        .filter(|participant| participant.kind == V2ParticipantKind::Agent)
        .map(|participant| participant.id.clone())
        .collect();
    let main_agent_id = agent_ids
        .first()
        .cloned()
        .unwrap_or_else(|| "agent".into());

    let participants: Vec<Participant> = session
        .participants
        .iter()
        .map(|participant| Participant {
            id: participant.id.clone(),
            kind: map_participant_kind(participant.kind),
            name: participant.name.clone(),
            principal: participant.principal.clone(),
            parent_id: None,
            role: if participant.kind == V2ParticipantKind::Agent {
                Some(ParticipantRole::Main)
            } else {
                None
            },
            model: None,
        })
        .collect();

    let roster_agents: Vec<RosterAgent> = session
        .participants
        .iter()
        .filter(|participant| participant.kind == V2ParticipantKind::Agent)
        .map(|participant| RosterAgent {
            id: participant.id.clone(),
            name: participant.name.clone(),
            parent_id: None,
            role: ParticipantRole::Main,
            model: None,
        })
        .collect();

    let mut tool_names: Vec<String> = Vec::new();
    for event in &session.events {
        if let Some(tool) = &event.tool {
            if !tool_names.iter().any(|name| name == &tool.name) {
                tool_names.push(tool.name.clone());
            }
        }
    }
    let roster_tools: Vec<RosterTool> = tool_names
        .into_iter()
        .map(|name| RosterTool {
            name,
            source: ToolSource::Unknown,
            version: None,
            definition_ref: None,
        })
        .collect();

    let environment = Environment {
        machine_id: non_empty(&session.machine).unwrap_or_else(|| "unknown".into()),
        machine: non_empty(&session.machine),
        hostname: None,
        os: None,
        cwd: non_empty(&session.project_hint),
        git_branch: None,
        harness: Harness {
            name: session.source_agent.clone(),
            version: "unknown".into(),
        },
        locale: None,
        timezone: non_empty(&session.timezone),
    };

    let ownership = Ownership {
        principal_id: None,
        owner_agent_id: main_agent_id.clone(),
        scope: Scope {
            kind: ScopeKind::Project,
            key: non_empty(&session.project_hint).unwrap_or_else(|| "unknown".into()),
        },
        visibility: Visibility::PrivateAgent,
        retention: "default".into(),
    };

    let (docs, doc_id_by_path, invalid_doc_sha) = build_docs(session);

    let mut thread_keys: Vec<String> = Vec::new();
    for event in &session.events {
        let key = thread_key_of(event);
        if !thread_keys.iter().any(|existing| existing == &key) {
            thread_keys.push(key);
        }
    }
    if !thread_keys.iter().any(|key| key == "main") {
        thread_keys.insert(0, "main".into());
    }
    let mut thread_uid_by_key: HashMap<String, String> = HashMap::new();
    for key in &thread_keys {
        thread_uid_by_key.insert(key.clone(), compute_thread_uid(&session_uid, key));
    }
    let threads: Vec<Thread> = thread_keys
        .iter()
        .map(|key| Thread {
            thread_key: key.clone(),
            thread_uid: thread_uid_by_key[key].clone(),
            parent_thread_key: if key == "main" {
                None
            } else {
                Some("main".into())
            },
            kind: thread_kind(key),
            agent_id: main_agent_id.clone(),
        })
        .collect();

    let mut pre_events = build_pre_events(session, &thread_uid_by_key, &doc_id_by_path);
    let turns = build_turns(&mut pre_events, &thread_uid_by_key);
    apply_summary_covers(&mut pre_events);

    let event_uid_by_seq: HashMap<u32, String> = pre_events
        .iter()
        .map(|event| (event.seq, event.event_uid.clone()))
        .collect();
    let events: Vec<V3Event> = pre_events
        .into_iter()
        .map(|event| finalize_event(event, session, options, &revision_id, &event_uid_by_seq))
        .collect();

    let mut session_flags: Vec<QualityFlag> = Vec::new();
    for event in &events {
        if let Some(quality) = &event.quality {
            for flag in &quality.flags {
                if !session_flags.contains(flag) {
                    session_flags.push(*flag);
                }
            }
        }
    }
    let mut dropped = Dropped::default();
    if invalid_doc_sha > 0 {
        dropped
            .other
            .insert("invalid_doc_sha".into(), invalid_doc_sha);
    }

    V3Session {
        session_key: session.session_key.clone(),
        session_uid,
        parent_session_key: None,
        agent_instance_id: None,
        source_agent: session.source_agent.clone(),
        device_id: session.device_id.clone(),
        environment,
        roster: Roster {
            agents: roster_agents,
            tools: roster_tools,
        },
        ownership,
        threads,
        turns,
        participants,
        docs,
        secrets: vec![],
        dropped,
        quality: SessionQuality {
            flags: session_flags,
            quarantined_events: 0,
            rejected_events: 0,
            retryable: false,
        },
        source: SourceMeta {
            adapter: session.source_agent.clone(),
            adapter_version: options.adapter_version,
            path: options.source_path.clone(),
            sha256: options.source_sha256.clone(),
            cursor: None,
            revision_id,
            revision_of: None,
        },
        events,
    }
}

struct PreEvent {
    seq: u32,
    parent_seq: Option<u32>,
    kind: EventKind,
    speaker_id: String,
    ts_ms: Option<i64>,
    text: String,
    tool: Option<ToolCall>,
    tool_result: Option<ToolResult>,
    doc: Option<EventDoc>,
    summary: bool,
    covers: Vec<SummaryCover>,
    thread_key: String,
    thread_uid: String,
    turn_uid: String,
    native_key: String,
    occurrence: u32,
    content_hash: String,
    event_uid: String,
    source_ref_v2: Option<String>,
    quality_flags: Vec<QualityFlag>,
}

fn build_docs(session: &V2Session) -> (Vec<DocEntry>, HashMap<String, String>, u32) {
    let mut docs = Vec::new();
    let mut doc_id_by_path = HashMap::new();
    let mut invalid_doc_sha = 0;
    for event in &session.events {
        let Some(doc) = &event.doc else { continue };
        if doc_id_by_path.contains_key(&doc.path) {
            continue;
        }
        let doc_id = format!("doc-{}", &text_sha256(&doc.path)[..16]);
        let sha256 = match &doc.sha256 {
            Some(sha) if is_hex64(sha) => Some(sha.clone()),
            Some(_) => {
                invalid_doc_sha += 1;
                None
            }
            None => None,
        };
        doc_id_by_path.insert(doc.path.clone(), doc_id.clone());
        docs.push(DocEntry {
            doc_id,
            path: doc.path.clone(),
            sha256,
            mime: None,
            size: None,
            title: None,
        });
    }
    (docs, doc_id_by_path, invalid_doc_sha)
}

fn build_pre_events(
    session: &V2Session,
    thread_uid_by_key: &HashMap<String, String>,
    doc_id_by_path: &HashMap<String, String>,
) -> Vec<PreEvent> {
    let mut pre_events = Vec::new();
    let mut occurrences: HashMap<(String, String), u32> = HashMap::new();
    let mut last_ts: Option<i64> = None;

    for event in &session.events {
        let kind = map_event_kind(event.kind);
        let thread_key = thread_key_of(event);
        let thread_uid = thread_uid_by_key[&thread_key].clone();
        let text = normalize_text_v3(&event.text);
        let tool = event.tool.as_ref().map(|tool| ToolCall {
            call_id: tool.call_id.clone(),
            name: tool.name.clone(),
            arguments: tool.arguments.clone(),
        });
        let tool_result = event.tool_result.as_ref().map(|result| ToolResult {
            call_id: result.call_id.clone(),
            status: map_tool_status(result.status),
            text: result.text.clone(),
            details: None,
            artifacts: result
                .artifact_ref
                .as_ref()
                .filter(|value| !value.trim().is_empty())
                .map(|value| {
                    vec![ArtifactRef {
                        artifact_id: value.clone(),
                        mime: None,
                        size: None,
                        sha256: None,
                        path: None,
                    }]
                })
                .unwrap_or_default(),
        });
        let doc = event.doc.as_ref().map(|doc| EventDoc {
            doc_id: doc_id_by_path[&doc.path].clone(),
            span: doc.span.as_ref().map(|span| Span {
                start: span.start,
                end: span.end,
                text_sha256: None,
            }),
        });

        let tool_call_pair = tool
            .as_ref()
            .map(|tool| (tool.call_id.as_str(), tool.name.as_str()));
        let tool_result_pair = tool_result.as_ref().map(|result| {
            (
                result.call_id.as_str(),
                result.status.as_str(),
                result.text.as_deref().unwrap_or(""),
            )
        });
        let doc_pair = doc.as_ref().map(|doc| {
            (
                doc.doc_id.as_str(),
                doc.span.as_ref().map(|span| (span.start, span.end)),
            )
        });
        let content_hash = compute_content_hash(
            kind.as_str(),
            &text,
            tool_call_pair,
            tool_result_pair,
            doc_pair,
        );

        let base_native = event
            .source_ref
            .clone()
            .unwrap_or_else(|| format!("seq-{}", event.seq));
        let native_key = format!("v2:{base_native}");
        let occurrence_key = (thread_key.clone(), native_key.clone());
        let occurrence = match occurrences.get(&occurrence_key) {
            Some(current) => {
                let next = current + 1;
                occurrences.insert(occurrence_key, next);
                next
            }
            None => {
                occurrences.insert(occurrence_key, 0);
                0
            }
        };
        let event_uid = compute_event_uid(&thread_uid, &native_key, &content_hash, occurrence);

        let mut quality_flags = Vec::new();
        if let Some(ts) = event.ts_ms {
            if let Some(previous) = last_ts {
                if ts < previous {
                    quality_flags.push(QualityFlag::ClockSkew);
                }
            }
            last_ts = Some(ts);
        }

        pre_events.push(PreEvent {
            seq: event.seq,
            parent_seq: event.parent_seq,
            kind,
            speaker_id: event.speaker_id.clone(),
            ts_ms: event.ts_ms,
            text,
            tool,
            tool_result,
            doc,
            summary: event.summary,
            covers: Vec::new(),
            thread_key,
            thread_uid,
            turn_uid: String::new(),
            native_key,
            occurrence,
            content_hash,
            event_uid,
            source_ref_v2: event.source_ref.clone(),
            quality_flags,
        });
    }
    pre_events
}

fn build_turns(
    events: &mut [PreEvent],
    _thread_uid_by_key: &HashMap<String, String>,
) -> Vec<Turn> {
    struct OpenTurn {
        thread_uid: String,
        start_seq: u32,
        end_seq: u32,
        anchor_uid: String,
        turn_key: String,
    }
    let mut open: HashMap<String, OpenTurn> = HashMap::new();
    let mut turns: Vec<Turn> = Vec::new();
    let mut counter = 0usize;

    for event in events.iter_mut() {
        let start_new = match open.get(&event.thread_key) {
            None => true,
            Some(_) => event.kind == EventKind::HumanMsg,
        };
        if start_new {
            if let Some(old) = open.remove(&event.thread_key) {
                turns.push(Turn {
                    turn_key: old.turn_key,
                    turn_uid: compute_turn_uid_from_anchor(&old.thread_uid, &old.anchor_uid),
                    thread_uid: old.thread_uid,
                    derived: true,
                    start_seq: old.start_seq,
                    end_seq: old.end_seq,
                });
            }
            let turn_key = format!("derived-{counter}");
            counter += 1;
            open.insert(
                event.thread_key.clone(),
                OpenTurn {
                    thread_uid: event.thread_uid.clone(),
                    start_seq: event.seq,
                    end_seq: event.seq,
                    anchor_uid: event.event_uid.clone(),
                    turn_key,
                },
            );
        }
        let open_turn = open
            .get_mut(&event.thread_key)
            .expect("a turn was just opened for this thread");
        open_turn.end_seq = event.seq;
        event.turn_uid =
            compute_turn_uid_from_anchor(&open_turn.thread_uid, &open_turn.anchor_uid);
    }
    for old in open.into_values() {
        turns.push(Turn {
            turn_key: old.turn_key,
            turn_uid: compute_turn_uid_from_anchor(&old.thread_uid, &old.anchor_uid),
            thread_uid: old.thread_uid,
            derived: true,
            start_seq: old.start_seq,
            end_seq: old.end_seq,
        });
    }
    turns.sort_by_key(|turn| turn.start_seq);
    turns
}

fn apply_summary_covers(events: &mut [PreEvent]) {
    let mut covers: Vec<(usize, Option<SummaryCover>)> = Vec::new();
    for (index, summary) in events.iter().enumerate() {
        if summary.kind != EventKind::Summary {
            continue;
        }
        let mut first = None;
        let mut last = None;
        for event in events.iter() {
            if event.thread_key == summary.thread_key && event.seq < summary.seq {
                if first.is_none() {
                    first = Some(event.seq);
                }
                last = Some(event.seq);
            }
        }
        let cover = match (first, last) {
            (Some(start), Some(end)) => Some(SummaryCover {
                thread_uid: summary.thread_uid.clone(),
                seq_start: start,
                seq_end: end,
            }),
            _ => None,
        };
        covers.push((index, cover));
    }
    for (index, cover) in covers {
        let event = &mut events[index];
        match cover {
            Some(cover) => {
                event.covers = vec![cover];
                push_quality(&mut event.quality_flags, QualityFlag::CoversDerived);
            }
            None => push_quality(&mut event.quality_flags, QualityFlag::CoversUnknown),
        }
    }
}

fn finalize_event(
    event: PreEvent,
    session: &V2Session,
    options: &MigrateOptions,
    revision_id: &str,
    event_uid_by_seq: &HashMap<u32, String>,
) -> V3Event {
    let speaker_kind = session
        .participants
        .iter()
        .find(|participant| participant.id == event.speaker_id)
        .map(|participant| participant.kind);
    let text_sha256 = text_sha256(&event.text);
    let span = if event.text.is_empty() {
        None
    } else {
        Some(Span {
            start: 0,
            end: char_count(&event.text),
            text_sha256: Some(text_sha256.clone()),
        })
    };
    let refs = match (&event.doc, event.kind) {
        (Some(_), EventKind::DocRef | EventKind::Attachment) => Vec::new(),
        (Some(doc), _) => vec![EventRef {
            kind: RefKind::Doc,
            id: doc.doc_id.clone(),
            alias: None,
            version: None,
            span: doc.span.clone(),
        }],
        (None, _) => Vec::new(),
    };
    let system_kind = if event.kind == EventKind::System {
        Some(SystemKind::HarnessNotice)
    } else {
        None
    };
    let quality = if event.quality_flags.is_empty() {
        None
    } else {
        Some(EventQuality {
            flags: event.quality_flags,
            quarantine: false,
            reason: None,
            retryable: false,
        })
    };
    V3Event {
        seq: event.seq,
        parent_seq: event.parent_seq,
        parent_event_uid: event
            .parent_seq
            .and_then(|seq| event_uid_by_seq.get(&seq).cloned()),
        kind: event.kind,
        speaker_id: event.speaker_id,
        ts_ms: event.ts_ms,
        observed_at_ms: options.observed_at_ms,
        valid_from_ms: None,
        valid_to_ms: None,
        thread_uid: event.thread_uid,
        turn_uid: event.turn_uid,
        event_uid: event.event_uid,
        native_key: event.native_key,
        content_hash: event.content_hash,
        occurrence: event.occurrence,
        revision_id: revision_id.to_string(),
        revision_of: None,
        duplicate_of: None,
        attempt: 0,
        final_: true,
        text: event.text,
        text_sha256,
        span,
        norm_version: INGEST_V3_NORM_VERSION,
        refs,
        trust: map_trust(event.kind, speaker_kind),
        speech_act: map_speech_act(event.kind),
        modality: Modality::Actual,
        system_kind,
        tool: event.tool,
        tool_result: event.tool_result,
        doc: event.doc,
        summary: event.summary,
        covers: event.covers,
        source_ref: SourceRef {
            adapter: session.source_agent.clone(),
            adapter_version: options.adapter_version,
            path: options.source_path.clone(),
            native_id: event.source_ref_v2,
            line: None,
            native_step: None,
        },
        quality,
    }
}

fn push_quality(flags: &mut Vec<QualityFlag>, flag: QualityFlag) {
    if !flags.contains(&flag) {
        flags.push(flag);
    }
}

fn non_empty(value: &Option<String>) -> Option<String> {
    value
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn thread_key_of(event: &V2Event) -> String {
    event
        .thread
        .as_ref()
        .map(|thread| thread.trim())
        .filter(|thread| !thread.is_empty())
        .unwrap_or("main")
        .to_string()
}

fn thread_kind(key: &str) -> ThreadKind {
    match key {
        "main" => ThreadKind::Main,
        "injected" => ThreadKind::Injected,
        "sidechain" => ThreadKind::Sidechain,
        other if other.starts_with("sub") => ThreadKind::Subagent,
        _ => ThreadKind::Sidechain,
    }
}

fn map_participant_kind(kind: V2ParticipantKind) -> ParticipantKind {
    match kind {
        V2ParticipantKind::Human => ParticipantKind::Human,
        V2ParticipantKind::Agent => ParticipantKind::Agent,
        V2ParticipantKind::Tool => ParticipantKind::Tool,
        V2ParticipantKind::System => ParticipantKind::System,
    }
}

fn map_event_kind(kind: V2EventKind) -> EventKind {
    match kind {
        V2EventKind::HumanMsg => EventKind::HumanMsg,
        V2EventKind::AgentMsg => EventKind::AgentMsg,
        V2EventKind::ToolCall => EventKind::ToolCall,
        V2EventKind::ToolResult => EventKind::ToolResult,
        V2EventKind::DocRef => EventKind::DocRef,
        V2EventKind::Summary => EventKind::Summary,
        V2EventKind::System => EventKind::System,
        V2EventKind::Attachment => EventKind::Attachment,
    }
}

fn map_tool_status(status: V2ToolStatus) -> ToolStatus {
    match status {
        V2ToolStatus::Ok => ToolStatus::Ok,
        V2ToolStatus::Error => ToolStatus::Error,
        V2ToolStatus::Unknown => ToolStatus::Unknown,
    }
}

fn map_trust(kind: EventKind, speaker_kind: Option<V2ParticipantKind>) -> Trust {
    match kind {
        EventKind::HumanMsg => Trust::User,
        EventKind::AgentMsg | EventKind::ToolCall => Trust::Agent,
        EventKind::ToolResult => Trust::Tool,
        EventKind::DocRef | EventKind::Attachment => match speaker_kind {
            Some(V2ParticipantKind::System) => Trust::Harness,
            Some(V2ParticipantKind::Tool) => Trust::Tool,
            Some(V2ParticipantKind::Agent) => Trust::Agent,
            _ => Trust::User,
        },
        EventKind::Summary => match speaker_kind {
            Some(V2ParticipantKind::System) => Trust::Harness,
            _ => Trust::Agent,
        },
        EventKind::System => Trust::Harness,
    }
}

fn map_speech_act(kind: EventKind) -> SpeechAct {
    match kind {
        EventKind::ToolCall => SpeechAct::Request,
        _ => SpeechAct::Assert,
    }
}

fn is_hex64(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
