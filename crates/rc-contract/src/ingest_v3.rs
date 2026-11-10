//! 入库契约 v3：在 v2 的无损事件流上补身份、角色、对象、语义、质量与系统层 P0。
//!
//! 引擎只认 v1 的 `ExchangeFile`；v3 是现役生产者信封，`hub import` 经
//! [`crate::ingest_v3_project::project_v3_to_v1`] 把它投回 v1 后走同一套校验/落库。
//! 各 Agent 侧转换器把自家会话映射到这里；v2 模块保留作迁移与回归夹具，
//! 本模块不读写 v2 结构；版本闸只接受 `format_version=3`。

use crate::identity_v3::{
    char_count, char_slice, compute_revision_id, compute_session_uid, compute_thread_uid,
    compute_turn_uid_from_anchor, compute_turn_uid_native, compute_content_hash,
    normalize_text_v3, text_sha256,
};
use crate::ingest_v2::{pii_hits, secret_hits};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

pub const INGEST_V3_FORMAT_VERSION: u32 = 3;
pub const INGEST_V3_NORM_VERSION: u16 = 1;

pub const TS_MIN: i64 = 946_684_800_000; // 2000-01-01
pub const TS_MAX: i64 = 4_102_444_800_000; // 2100-01-01

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct V3File {
    pub format_version: u32,
    pub ingest_version: u32,
    pub sessions: Vec<V3Session>,
    #[serde(default)]
    pub policy: Option<Policy>,
    #[serde(default)]
    pub key_manifest: Option<KeyManifest>,
    #[serde(default)]
    pub replication: Option<ReplicationStamp>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct V3Session {
    pub session_key: String,
    pub session_uid: String,
    #[serde(default)]
    pub parent_session_key: Option<String>,
    #[serde(default)]
    pub agent_instance_id: Option<String>,
    pub source_agent: String,
    pub device_id: String,
    pub environment: Environment,
    #[serde(default)]
    pub roster: Roster,
    pub ownership: Ownership,
    #[serde(default)]
    pub threads: Vec<Thread>,
    #[serde(default)]
    pub turns: Vec<Turn>,
    pub participants: Vec<Participant>,
    #[serde(default)]
    pub docs: Vec<DocEntry>,
    #[serde(default)]
    pub secrets: Vec<SecretEntry>,
    #[serde(default)]
    pub dropped: Dropped,
    #[serde(default)]
    pub quality: SessionQuality,
    pub source: SourceMeta,
    pub events: Vec<V3Event>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Environment {
    pub machine_id: String,
    #[serde(default)]
    pub machine: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub git_branch: Option<String>,
    pub harness: Harness,
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Harness {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Roster {
    #[serde(default)]
    pub agents: Vec<RosterAgent>,
    #[serde(default)]
    pub tools: Vec<RosterTool>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RosterAgent {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub role: ParticipantRole,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RosterTool {
    pub name: String,
    pub source: ToolSource,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub definition_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ownership {
    #[serde(default)]
    pub principal_id: Option<String>,
    pub owner_agent_id: String,
    pub scope: Scope,
    pub visibility: Visibility,
    /// `default` / `keep` / `days:<n>`；保留期是资源属性，不是事件属性。
    pub retention: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub kind: ScopeKind,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Thread {
    pub thread_key: String,
    pub thread_uid: String,
    #[serde(default)]
    pub parent_thread_key: Option<String>,
    pub kind: ThreadKind,
    pub agent_id: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    pub turn_key: String,
    pub turn_uid: String,
    pub thread_uid: String,
    pub derived: bool,
    pub start_seq: u32,
    pub end_seq: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Participant {
    pub id: String,
    pub kind: ParticipantKind,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub principal: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub role: Option<ParticipantRole>,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocEntry {
    pub doc_id: String,
    pub path: String,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretEntry {
    pub secret_id: String,
    pub alias: String,
    pub version: u32,
    pub fingerprint: String,
    pub status: SecretStatus,
    pub owner_agent_id: String,
    #[serde(default)]
    pub key_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Dropped {
    #[serde(default)]
    pub reasoning_blocks: u32,
    #[serde(default)]
    pub images: u32,
    #[serde(default)]
    pub duplicate_events: u32,
    #[serde(default)]
    pub malformed_lines: u32,
    #[serde(default)]
    pub unsupported_blocks: u32,
    #[serde(default)]
    pub orphan_tool_results: u32,
    #[serde(default)]
    pub retry_results: u32,
    /// 源特有的丢弃原因只放计数；键必须是安全 token，不得放正文。
    #[serde(default)]
    pub other: BTreeMap<String, u32>,
}

impl Dropped {
    pub fn total(&self) -> u64 {
        let fixed = u64::from(self.reasoning_blocks)
            + u64::from(self.images)
            + u64::from(self.duplicate_events)
            + u64::from(self.malformed_lines)
            + u64::from(self.unsupported_blocks)
            + u64::from(self.orphan_tool_results)
            + u64::from(self.retry_results);
        fixed + self.other.values().map(|v| u64::from(*v)).sum::<u64>()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionQuality {
    #[serde(default)]
    pub flags: Vec<QualityFlag>,
    #[serde(default)]
    pub quarantined_events: u32,
    #[serde(default)]
    pub rejected_events: u32,
    #[serde(default)]
    pub retryable: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventQuality {
    pub flags: Vec<QualityFlag>,
    #[serde(default)]
    pub quarantine: bool,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub retryable: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceMeta {
    pub adapter: String,
    pub adapter_version: u32,
    pub path: String,
    pub sha256: String,
    #[serde(default)]
    pub cursor: Option<String>,
    pub revision_id: String,
    #[serde(default)]
    pub revision_of: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct V3Event {
    pub seq: u32,
    #[serde(default)]
    pub parent_seq: Option<u32>,
    #[serde(default)]
    pub parent_event_uid: Option<String>,
    pub kind: EventKind,
    pub speaker_id: String,
    #[serde(default)]
    pub ts_ms: Option<i64>,
    #[serde(default)]
    pub observed_at_ms: Option<i64>,
    #[serde(default)]
    pub valid_from_ms: Option<i64>,
    #[serde(default)]
    pub valid_to_ms: Option<i64>,
    pub thread_uid: String,
    pub turn_uid: String,
    pub event_uid: String,
    pub native_key: String,
    pub content_hash: String,
    #[serde(default)]
    pub occurrence: u32,
    pub revision_id: String,
    #[serde(default)]
    pub revision_of: Option<String>,
    #[serde(default)]
    pub duplicate_of: Option<String>,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default = "default_true", rename = "final")]
    pub final_: bool,
    #[serde(default)]
    pub text: String,
    pub text_sha256: String,
    #[serde(default)]
    pub span: Option<Span>,
    pub norm_version: u16,
    #[serde(default)]
    pub refs: Vec<EventRef>,
    pub trust: Trust,
    pub speech_act: SpeechAct,
    #[serde(default)]
    pub modality: Modality,
    #[serde(default)]
    pub system_kind: Option<SystemKind>,
    #[serde(default)]
    pub tool: Option<ToolCall>,
    #[serde(default)]
    pub tool_result: Option<ToolResult>,
    #[serde(default)]
    pub doc: Option<EventDoc>,
    #[serde(default)]
    pub summary: bool,
    #[serde(default)]
    pub covers: Vec<SummaryCover>,
    pub source_ref: SourceRef,
    #[serde(default)]
    pub quality: Option<EventQuality>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Span {
    pub start: u64,
    pub end: u64,
    #[serde(default)]
    pub text_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventRef {
    pub kind: RefKind,
    pub id: String,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub version: Option<u32>,
    #[serde(default)]
    pub span: Option<Span>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    pub call_id: String,
    pub status: ToolStatus,
    #[serde(default)]
    pub text: Option<String>,
    /// 程序抽的结构化骨架（Claude/Qoder 的 toolUseResult、MiniMax/DSH 的 details）。
    #[serde(default)]
    pub details: Option<serde_json::Value>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactRef>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    pub artifact_id: String,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventDoc {
    pub doc_id: String,
    #[serde(default)]
    pub span: Option<Span>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryCover {
    pub thread_uid: String,
    pub seq_start: u32,
    pub seq_end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRef {
    pub adapter: String,
    pub adapter_version: u32,
    pub path: String,
    #[serde(default)]
    pub native_id: Option<String>,
    #[serde(default)]
    pub line: Option<u64>,
    #[serde(default)]
    pub native_step: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub default_deny: bool,
    pub require_grant_for_cross_agent: bool,
    pub enforcement_points: Vec<EnforcementPoint>,
    #[serde(default)]
    pub service_principals: Vec<ServicePrincipal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServicePrincipal {
    pub id: String,
    pub kind: ServicePrincipalKind,
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KeyManifest {
    pub envelope_version: u16,
    pub keys: Vec<KeyEntry>,
    pub backup: BackupKeyPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KeyEntry {
    pub key_id: String,
    pub kind: KeyKind,
    pub version: u32,
    pub fingerprint: String,
    pub status: KeyStatus,
    pub created_at_ms: i64,
    #[serde(default)]
    pub rotated_at_ms: Option<i64>,
    pub custody: KeyCustody,
    #[serde(default)]
    pub owner_agent_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackupKeyPolicy {
    pub separate_channel: bool,
    pub recovery_required: bool,
    pub key_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicationStamp {
    pub epoch: u64,
    pub commit_seq: u64,
    pub prev_commit_seq: u64,
    pub center_id: String,
    pub fencing_token: u64,
    pub signed_manifest_sha256: String,
    #[serde(default)]
    pub promotion_cert: Option<PromotionCert>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionCert {
    pub new_center_id: String,
    pub new_pubkey_fingerprint: String,
    pub prev_epoch: u64,
    pub new_epoch: u64,
    pub signed_by_prev_center: bool,
    pub signature: String,
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
pub enum ParticipantRole {
    Main,
    Subagent,
    Reviewer,
    Worker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadKind {
    Main,
    Sidechain,
    Subagent,
    Injected,
    Worker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSource {
    Harness,
    Mcp,
    Builtin,
    Plugin,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    PrivateAgent,
    Person,
    Project,
    Org,
    Public,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    Project,
    Agent,
    Org,
    Global,
    Session,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefKind {
    Doc,
    Secret,
    Agent,
    Tool,
    Machine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    User,
    Tool,
    Agent,
    Subagent,
    Injected,
    Harness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeechAct {
    Request,
    Approve,
    Command,
    Plan,
    Assert,
    Question,
    Promise,
    Refuse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    #[default]
    Actual,
    Planned,
    Hypothetical,
    Quoted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemKind {
    InjectedNotice,
    HarnessNotice,
    Policy,
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

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::HumanMsg => "human_msg",
            EventKind::AgentMsg => "agent_msg",
            EventKind::ToolCall => "tool_call",
            EventKind::ToolResult => "tool_result",
            EventKind::DocRef => "doc_ref",
            EventKind::Summary => "summary",
            EventKind::System => "system",
            EventKind::Attachment => "attachment",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Ok,
    Error,
    Unknown,
}

impl ToolStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolStatus::Ok => "ok",
            ToolStatus::Error => "error",
            ToolStatus::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityFlag {
    Truncated,
    MalformedJson,
    EncodingLoss,
    MissingToolResult,
    TypeCoerced,
    EmptyContent,
    ClockSkew,
    ContentJoined,
    ParentDropped,
    EmptyToolResultPlaceholder,
    CoversUnknown,
    CoversDerived,
    UnsupportedBlock,
    PiiRedacted,
    DuplicateContent,
    InjectedContent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretStatus {
    Active,
    Rotated,
    Revoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyKind {
    Hub,
    Device,
    Agent,
    Core,
    Secret,
    Backup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyStatus {
    Active,
    Retired,
    Revoked,
    Destroyed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyCustody {
    File,
    Operator,
    Kms,
    Offline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnforcementPoint {
    Hub,
    Mcp,
    Expand,
    Export,
    Search,
    Backup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServicePrincipalKind {
    Extractor,
    Backup,
    Operator,
    Center,
    Consumer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    ReadTranscript,
    ReadMemory,
    WriteMemory,
    Derive,
    UseSecret,
    Export,
    BackupRead,
    BackupWrite,
    Admin,
}

/// 摘要、隔离、注入与非 final 的事件不得作为证据。
/// 这是契约层唯一的“证据资格”判据；抽取侧必须调用它，不得自己重写一套。
pub fn is_evidence_eligible(event: &V3Event) -> bool {
    !event.summary
        && event.kind != EventKind::Summary
        && event.final_
        && event.trust != Trust::Injected
        && !event.quality.as_ref().map(|q| q.quarantine).unwrap_or(false)
}

impl QualityFlag {
    pub fn as_str(self) -> &'static str {
        match self {
            QualityFlag::Truncated => "truncated",
            QualityFlag::MalformedJson => "malformed_json",
            QualityFlag::EncodingLoss => "encoding_loss",
            QualityFlag::MissingToolResult => "missing_tool_result",
            QualityFlag::TypeCoerced => "type_coerced",
            QualityFlag::EmptyContent => "empty_content",
            QualityFlag::ClockSkew => "clock_skew",
            QualityFlag::ContentJoined => "content_joined",
            QualityFlag::ParentDropped => "parent_dropped",
            QualityFlag::EmptyToolResultPlaceholder => "empty_tool_result_placeholder",
            QualityFlag::CoversUnknown => "covers_unknown",
            QualityFlag::CoversDerived => "covers_derived",
            QualityFlag::UnsupportedBlock => "unsupported_block",
            QualityFlag::PiiRedacted => "pii_redacted",
            QualityFlag::DuplicateContent => "duplicate_content",
            QualityFlag::InjectedContent => "injected_content",
        }
    }

    /// 这些错误意味着正文本身不可信；必须进隔离区，不能只打标记。
    pub fn is_hard_quality_failure(self) -> bool {
        matches!(
            self,
            QualityFlag::Truncated | QualityFlag::MalformedJson | QualityFlag::EncodingLoss
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct V3Stats {
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
    pub quarantined_events: usize,
    pub dropped_events: u64,
    pub quality_flags: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct V3Issue {
    pub code: &'static str,
    pub session: Option<String>,
    pub seq: Option<u32>,
    pub field: Option<&'static str>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuarantineRecord {
    pub session: String,
    pub seq: u32,
    pub event_uid: String,
    pub flags: Vec<QualityFlag>,
    /// 只报告“是否有原因”，不把原因正文带出闸；正文可能含密钥/PII。
    pub reason_present: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct V3Report {
    pub ok: bool,
    pub errors: Vec<V3Issue>,
    pub warnings: Vec<V3Issue>,
    pub stats: V3Stats,
    pub quarantined: Vec<QuarantineRecord>,
}

impl V3Report {
    pub fn new() -> Self {
        V3Report {
            ok: false,
            errors: Vec::new(),
            warnings: Vec::new(),
            stats: V3Stats::default(),
            quarantined: Vec::new(),
        }
    }

    pub fn error(
        &mut self,
        code: &'static str,
        session: Option<&str>,
        seq: Option<u32>,
        field: Option<&'static str>,
        message: impl Into<String>,
    ) {
        self.errors.push(V3Issue {
            code,
            session: session.map(str::to_owned),
            seq,
            field,
            message: message.into(),
        });
    }

    pub fn warn(
        &mut self,
        code: &'static str,
        session: Option<&str>,
        seq: Option<u32>,
        field: Option<&'static str>,
        message: impl Into<String>,
    ) {
        self.warnings.push(V3Issue {
            code,
            session: session.map(str::to_owned),
            seq,
            field,
            message: message.into(),
        });
    }
}

impl Default for V3Report {
    fn default() -> Self {
        Self::new()
    }
}

pub fn validate_ingest_v3(file: &V3File) -> V3Report {
    validate_ingest_v3_with(file, false)
}

/// `require_p0` 面向生产入口：要求 `policy` 与 `key_manifest` 存在。
/// `replication` 是备份/复制包的字段，边缘入库天然没有它，因此不在这里强制。
pub fn validate_ingest_v3_with(file: &V3File, require_p0: bool) -> V3Report {
    let mut rep = V3Report::new();
    if file.format_version != INGEST_V3_FORMAT_VERSION {
        rep.error(
            "format_version",
            None,
            None,
            Some("format_version"),
            format!(
                "expected {}, got {}",
                INGEST_V3_FORMAT_VERSION, file.format_version
            ),
        );
        return rep;
    }
    if file.ingest_version != INGEST_V3_FORMAT_VERSION {
        rep.error(
            "ingest_version",
            None,
            None,
            Some("ingest_version"),
            format!(
                "expected {}, got {}",
                INGEST_V3_FORMAT_VERSION, file.ingest_version
            ),
        );
    }
    if file.sessions.is_empty() {
        rep.error(
            "no_sessions",
            None,
            None,
            Some("sessions"),
            "at least one session is required",
        );
        return rep;
    }

    if file.policy.is_none() {
        if require_p0 {
            rep.error(
                "policy_absent",
                None,
                None,
                Some("policy"),
                "production ingest requires a policy block",
            );
        } else {
            rep.warn(
                "policy_absent",
                None,
                None,
                Some("policy"),
                "no policy block; enforcement points are not declared",
            );
        }
    }
    if file.key_manifest.is_none() {
        if require_p0 {
            rep.error(
                "key_manifest_absent",
                None,
                None,
                Some("key_manifest"),
                "production ingest requires a key manifest",
            );
        } else {
            rep.warn(
                "key_manifest_absent",
                None,
                None,
                Some("key_manifest"),
                "no key manifest; secret lifecycle is not declared",
            );
        }
    }

    if let Some(policy) = &file.policy {
        validate_policy(policy, &mut rep);
    }
    if let Some(manifest) = &file.key_manifest {
        validate_key_manifest(manifest, &mut rep);
    }
    if let Some(replication) = &file.replication {
        validate_replication(replication, &mut rep);
    }

    let session_keys: HashSet<&str> = file
        .sessions
        .iter()
        .map(|s| s.session_key.as_str())
        .collect();
    let mut seen_sessions: HashSet<&str> = HashSet::new();
    for session in &file.sessions {
        rep.stats.sessions += 1;
        if !seen_sessions.insert(session.session_key.as_str()) {
            rep.error(
                "duplicate_session_key",
                Some(&session.session_key),
                None,
                Some("session_key"),
                "session_key must be unique",
            );
        }
        validate_session(
            session,
            file.ingest_version,
            file.key_manifest.as_ref(),
            &session_keys,
            &mut rep,
        );
    }

    rep.ok = rep.errors.is_empty();
    rep
}

fn is_hex64(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_fingerprint8(value: &str) -> bool {
    value.len() == 8
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn is_safe_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':' | b'/'))
}

fn is_signature_shape(value: &str) -> bool {
    value.len() >= 16
        && value.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'_' | b'-')
        })
}

fn valid_retention(value: &str) -> bool {
    if matches!(value, "default" | "keep") {
        return true;
    }
    match value.strip_prefix("days:") {
        Some(days) => days.parse::<u32>().map(|n| n > 0).unwrap_or(false),
        None => false,
    }
}

fn check_time_range(
    value: Option<i64>,
    field: &'static str,
    session: &str,
    seq: Option<u32>,
    rep: &mut V3Report,
) {
    if let Some(ts) = value {
        if !(TS_MIN..=TS_MAX).contains(&ts) {
            rep.error(
                "bad_timestamp",
                Some(session),
                seq,
                Some(field),
                format!("{field} {ts} outside 2000..2100"),
            );
        }
    }
}

fn scan_pairs(
    pairs: Vec<(&'static str, String)>,
    session: &str,
    seq: Option<u32>,
    rep: &mut V3Report,
) {
    for (field, value) in pairs {
        for kind in secret_hits(&value) {
            rep.stats.secret_hits += 1;
            rep.error(
                "secret_material",
                Some(session),
                seq,
                Some(field),
                format!("high-confidence secret pattern: {kind}"),
            );
        }
        rep.stats.pii_hits += pii_hits(&value);
    }
}

fn recompute_event_content_hash(event: &V3Event) -> String {
    let tool_call = event
        .tool
        .as_ref()
        .map(|tool| (tool.call_id.as_str(), tool.name.as_str()));
    let tool_result = event.tool_result.as_ref().map(|result| {
        (
            result.call_id.as_str(),
            result.status.as_str(),
            result.text.as_deref().unwrap_or(""),
        )
    });
    let doc = event.doc.as_ref().map(|doc| {
        (
            doc.doc_id.as_str(),
            doc.span.as_ref().map(|span| (span.start, span.end)),
        )
    });
    compute_content_hash(
        event.kind.as_str(),
        &event.text,
        tool_call,
        tool_result,
        doc,
    )
}

fn quality_flags(event: &V3Event) -> Option<&EventQuality> {
    event.quality.as_ref()
}

fn has_quality_flag(event: &V3Event, flag: QualityFlag) -> bool {
    quality_flags(event)
        .map(|quality| quality.flags.contains(&flag))
        .unwrap_or(false)
}

fn is_quarantined(event: &V3Event) -> bool {
    quality_flags(event)
        .map(|quality| quality.quarantine)
        .unwrap_or(false)
}

struct SessionCtx<'a> {
    participants: HashMap<&'a str, &'a Participant>,
    roster_agents: HashMap<&'a str, &'a RosterAgent>,
    roster_tools: HashSet<&'a str>,
    threads_by_uid: HashMap<&'a str, &'a Thread>,
    turns_by_uid: HashMap<&'a str, &'a Turn>,
    docs: HashMap<&'a str, &'a DocEntry>,
    secrets: HashMap<&'a str, &'a SecretEntry>,
    events_by_uid: HashMap<&'a str, &'a V3Event>,
    events_by_seq: HashMap<u32, &'a V3Event>,
}

fn validate_session(
    session: &V3Session,
    ingest_version: u32,
    key_manifest: Option<&KeyManifest>,
    session_keys: &HashSet<&str>,
    rep: &mut V3Report,
) {
    let key = session.session_key.as_str();
    for (field, value) in [
        ("session_key", &session.session_key),
        ("session_uid", &session.session_uid),
        ("source_agent", &session.source_agent),
        ("device_id", &session.device_id),
    ] {
        if value.trim().is_empty() {
            rep.error(
                "empty_field",
                Some(key),
                None,
                Some(field),
                format!("{field} must be non-empty"),
            );
        }
    }

    let expected_session_uid = compute_session_uid(
        &session.source_agent,
        &session.device_id,
        &session.session_key,
        session.agent_instance_id.as_deref(),
    );
    if session.session_uid != expected_session_uid {
        rep.error(
            "session_uid_mismatch",
            Some(key),
            None,
            Some("session_uid"),
            "session_uid does not match source_agent/device_id/session_key",
        );
    }

    if let Some(parent) = &session.parent_session_key {
        if parent.trim().is_empty() {
            rep.error(
                "empty_field",
                Some(key),
                None,
                Some("parent_session_key"),
                "parent_session_key must be non-empty when present",
            );
        } else if parent == key {
            rep.error(
                "parent_session_self",
                Some(key),
                None,
                Some("parent_session_key"),
                "a session cannot be its own parent",
            );
        } else if !session_keys.contains(parent.as_str()) {
            rep.warn(
                "parent_session_external",
                Some(key),
                None,
                Some("parent_session_key"),
                "parent session is not in this file",
            );
        }
    }
    if let Some(instance) = &session.agent_instance_id {
        if instance.trim().is_empty() {
            rep.error(
                "empty_field",
                Some(key),
                None,
                Some("agent_instance_id"),
                "agent_instance_id must be non-empty when present",
            );
        }
    }

    validate_source(&session.source, ingest_version, key, rep);
    validate_environment(&session.environment, key, rep);

    let mut participants: HashMap<&str, &Participant> = HashMap::new();
    for participant in &session.participants {
        if participant.id.trim().is_empty() {
            rep.error(
                "empty_participant_id",
                Some(key),
                None,
                Some("participants"),
                "participant id must be non-empty",
            );
            continue;
        }
        if participants.insert(participant.id.as_str(), participant).is_some() {
            rep.error(
                "duplicate_participant",
                Some(key),
                None,
                Some("participants"),
                format!("duplicate participant id {}", participant.id),
            );
        }
    }
    if !session
        .participants
        .iter()
        .any(|p| p.kind == ParticipantKind::Human)
    {
        rep.error(
            "no_human_participant",
            Some(key),
            None,
            Some("participants"),
            "at least one human participant is required",
        );
    }
    if !session
        .participants
        .iter()
        .any(|p| p.kind == ParticipantKind::Agent)
    {
        rep.error(
            "no_agent_participant",
            Some(key),
            None,
            Some("participants"),
            "at least one agent participant is required",
        );
    }
    if session.events.is_empty() {
        rep.error(
            "no_events",
            Some(key),
            None,
            Some("events"),
            "at least one event is required",
        );
        return;
    }
    validate_participants(session, &participants, key, rep);

    let mut roster_agents: HashMap<&str, &RosterAgent> = HashMap::new();
    for agent in &session.roster.agents {
        if agent.id.trim().is_empty() {
            rep.error(
                "empty_roster_agent",
                Some(key),
                None,
                Some("roster.agents"),
                "roster agent id must be non-empty",
            );
            continue;
        }
        if roster_agents.insert(agent.id.as_str(), agent).is_some() {
            rep.error(
                "duplicate_roster_agent",
                Some(key),
                None,
                Some("roster.agents"),
                format!("duplicate roster agent id {}", agent.id),
            );
        }
    }
    let roster_tools = validate_roster(session, &roster_agents, key, rep);
    validate_ownership(
        &session.ownership,
        &participants,
        &roster_agents,
        key,
        rep,
    );

    let mut threads_by_uid: HashMap<&str, &Thread> = HashMap::new();
    for thread in &session.threads {
        if thread.thread_key.trim().is_empty() {
            rep.error(
                "empty_thread",
                Some(key),
                None,
                Some("threads"),
                "thread_key must be non-empty",
            );
        }
        if thread.thread_uid.trim().is_empty() {
            rep.error(
                "empty_thread_uid",
                Some(key),
                None,
                Some("threads"),
                "thread_uid must be non-empty",
            );
        }
        if threads_by_uid
            .insert(thread.thread_uid.as_str(), thread)
            .is_some()
        {
            rep.error(
                "duplicate_thread_uid",
                Some(key),
                None,
                Some("threads"),
                "thread_uid must be unique",
            );
        }
    }
    validate_threads(session, &roster_agents, &threads_by_uid, key, rep);

    let mut docs: HashMap<&str, &DocEntry> = HashMap::new();
    for doc in &session.docs {
        if doc.doc_id.trim().is_empty() {
            rep.error(
                "empty_doc_id",
                Some(key),
                None,
                Some("docs"),
                "doc_id must be non-empty",
            );
            continue;
        }
        if !is_safe_token(&doc.doc_id) {
            rep.error(
                "bad_doc_id",
                Some(key),
                None,
                Some("docs"),
                "doc_id is not a safe token",
            );
        }
        if docs.insert(doc.doc_id.as_str(), doc).is_some() {
            rep.error(
                "duplicate_doc_id",
                Some(key),
                None,
                Some("docs"),
                format!("duplicate doc_id {}", doc.doc_id),
            );
        }
    }
    validate_docs(session, key, rep);

    let mut secrets: HashMap<&str, &SecretEntry> = HashMap::new();
    for secret in &session.secrets {
        if secret.secret_id.trim().is_empty() {
            rep.error(
                "empty_secret_id",
                Some(key),
                None,
                Some("secrets"),
                "secret_id must be non-empty",
            );
            continue;
        }
        if secrets.insert(secret.secret_id.as_str(), secret).is_some() {
            rep.error(
                "duplicate_secret_id",
                Some(key),
                None,
                Some("secrets"),
                format!("duplicate secret_id {}", secret.secret_id),
            );
        }
    }
    validate_secrets(session, &roster_agents, key_manifest, key, rep);

    let mut events_by_uid: HashMap<&str, &V3Event> = HashMap::new();
    let mut events_by_seq: HashMap<u32, &V3Event> = HashMap::new();
    for event in &session.events {
        if events_by_uid.insert(event.event_uid.as_str(), event).is_some() {
            rep.error(
                "duplicate_event_uid",
                Some(key),
                Some(event.seq),
                Some("event_uid"),
                "event_uid must be unique",
            );
        }
        if events_by_seq.insert(event.seq, event).is_some() {
            rep.error(
                "duplicate_seq",
                Some(key),
                Some(event.seq),
                Some("seq"),
                "seq must be unique",
            );
        }
    }

    let mut turns_by_uid: HashMap<&str, &Turn> = HashMap::new();
    for turn in &session.turns {
        if turns_by_uid.insert(turn.turn_uid.as_str(), turn).is_some() {
            rep.error(
                "duplicate_turn_uid",
                Some(key),
                None,
                Some("turns"),
                "turn_uid must be unique",
            );
        }
    }
    validate_turns(
        session,
        &threads_by_uid,
        &turns_by_uid,
        &events_by_seq,
        key,
        rep,
    );

    let ctx = SessionCtx {
        participants,
        roster_agents,
        roster_tools,
        threads_by_uid,
        turns_by_uid,
        docs,
        secrets,
        events_by_uid,
        events_by_seq,
    };
    validate_events(session, &ctx, key, rep);
    validate_session_quality(session, key, rep);
}

fn validate_source(source: &SourceMeta, ingest_version: u32, key: &str, rep: &mut V3Report) {
    for (field, value) in [
        ("source.adapter", &source.adapter),
        ("source.path", &source.path),
        ("source.revision_id", &source.revision_id),
    ] {
        if value.trim().is_empty() {
            rep.error(
                "empty_field",
                Some(key),
                None,
                Some(field),
                format!("{field} must be non-empty"),
            );
        }
    }
    if source.adapter_version == 0 {
        rep.error(
            "bad_adapter_version",
            Some(key),
            None,
            Some("source.adapter_version"),
            "adapter_version must be >= 1",
        );
    }
    if !is_hex64(&source.sha256) {
        rep.error(
            "bad_source_sha256",
            Some(key),
            None,
            Some("source.sha256"),
            "source.sha256 must be 64 hex characters",
        );
    }
    if !is_hex64(&source.revision_id) {
        rep.error(
            "bad_revision_id",
            Some(key),
            None,
            Some("source.revision_id"),
            "revision_id must be 64 hex characters",
        );
    } else {
        let expected = compute_revision_id(&source.sha256, source.adapter_version, ingest_version);
        if source.revision_id != expected {
            rep.error(
                "revision_mismatch",
                Some(key),
                None,
                Some("source.revision_id"),
                "revision_id does not match source_sha256/adapter_version/ingest_version",
            );
        }
    }
    if let Some(revision_of) = &source.revision_of {
        if !is_hex64(revision_of) {
            rep.error(
                "bad_revision_of",
                Some(key),
                None,
                Some("source.revision_of"),
                "revision_of must be 64 hex characters",
            );
        } else if revision_of == &source.revision_id {
            rep.error(
                "revision_self",
                Some(key),
                None,
                Some("source.revision_of"),
                "revision_of cannot be the current revision",
            );
        }
    }
    if let Some(cursor) = &source.cursor {
        if cursor.trim().is_empty() {
            rep.error(
                "empty_field",
                Some(key),
                None,
                Some("source.cursor"),
                "cursor must be non-empty when present",
            );
        }
    }
}

fn validate_environment(environment: &Environment, key: &str, rep: &mut V3Report) {
    for (field, value) in [
        ("environment.machine_id", &environment.machine_id),
        ("environment.harness.name", &environment.harness.name),
        ("environment.harness.version", &environment.harness.version),
    ] {
        if value.trim().is_empty() {
            rep.error(
                "empty_field",
                Some(key),
                None,
                Some(field),
                format!("{field} must be non-empty"),
            );
        }
    }
    for (field, value) in [
        ("environment.machine", &environment.machine),
        ("environment.hostname", &environment.hostname),
        ("environment.os", &environment.os),
        ("environment.cwd", &environment.cwd),
        ("environment.git_branch", &environment.git_branch),
        ("environment.locale", &environment.locale),
        ("environment.timezone", &environment.timezone),
    ] {
        if let Some(value) = value {
            if value.trim().is_empty() {
                rep.error(
                    "empty_field",
                    Some(key),
                    None,
                    Some(field),
                    format!("{field} must be non-empty when present"),
                );
            }
        }
    }
}

fn validate_participants(
    session: &V3Session,
    participants: &HashMap<&str, &Participant>,
    key: &str,
    rep: &mut V3Report,
) {
    for participant in &session.participants {
        if participant.kind == ParticipantKind::Agent {
            match participant.role {
                Some(ParticipantRole::Main) => {
                    if participant.parent_id.is_some() {
                        rep.error(
                            "main_agent_has_parent",
                            Some(key),
                            None,
                            Some("participants"),
                            "role=main must not have parent_id",
                        );
                    }
                }
                Some(ParticipantRole::Subagent)
                | Some(ParticipantRole::Reviewer)
                | Some(ParticipantRole::Worker) => {
                    if participant.parent_id.is_none() {
                        rep.error(
                            "agent_parent_missing",
                            Some(key),
                            None,
                            Some("participants"),
                            "non-main agent requires parent_id",
                        );
                    }
                }
                None => {
                    rep.error(
                        "agent_role_missing",
                        Some(key),
                        None,
                        Some("participants"),
                        "agent participant requires role",
                    );
                }
            }
            if let Some(parent) = &participant.parent_id {
                if parent == &participant.id {
                    rep.error(
                        "agent_parent_self",
                        Some(key),
                        None,
                        Some("participants"),
                        "agent cannot be its own parent",
                    );
                } else {
                    match participants.get(parent.as_str()) {
                        None => rep.error(
                            "agent_parent_unknown",
                            Some(key),
                            None,
                            Some("participants"),
                            "agent parent_id is not a participant",
                        ),
                        Some(parent) if parent.kind != ParticipantKind::Agent => rep.error(
                            "agent_parent_not_agent",
                            Some(key),
                            None,
                            Some("participants"),
                            "agent parent_id must reference an agent",
                        ),
                        Some(_) => {}
                    }
                }
            }
        } else {
            if participant.parent_id.is_some() {
                rep.error(
                    "parent_on_non_agent",
                    Some(key),
                    None,
                    Some("participants"),
                    "parent_id is only meaningful for agents",
                );
            }
            if participant.role.is_some() {
                rep.error(
                    "role_on_non_agent",
                    Some(key),
                    None,
                    Some("participants"),
                    "role is only meaningful for agents",
                );
            }
            if participant.model.is_some() {
                rep.error(
                    "model_on_non_agent",
                    Some(key),
                    None,
                    Some("participants"),
                    "model is only meaningful for agents",
                );
            }
        }
        if let Some(model) = &participant.model {
            if model.trim().is_empty() {
                rep.error(
                    "empty_field",
                    Some(key),
                    None,
                    Some("participants.model"),
                    "participant model must be non-empty when present",
                );
            }
        }
        if let Some(principal) = &participant.principal {
            if principal.trim().is_empty() {
                rep.error(
                    "empty_field",
                    Some(key),
                    None,
                    Some("participants.principal"),
                    "participant principal must be non-empty when present",
                );
            }
        }
    }

    for participant in &session.participants {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut current = participant.parent_id.as_deref();
        while let Some(parent) = current {
            if !seen.insert(parent) {
                rep.error(
                    "agent_parent_cycle",
                    Some(key),
                    None,
                    Some("participants"),
                    "participant parent chain contains a cycle",
                );
                break;
            }
            current = participants
                .get(parent)
                .and_then(|participant| participant.parent_id.as_deref());
        }
    }
}

fn validate_roster<'a>(
    session: &'a V3Session,
    roster_agents: &HashMap<&str, &RosterAgent>,
    key: &str,
    rep: &mut V3Report,
) -> HashSet<&'a str> {
    for agent in &session.roster.agents {
        if agent.role == ParticipantRole::Main {
            if agent.parent_id.is_some() {
                rep.error(
                    "main_roster_agent_has_parent",
                    Some(key),
                    None,
                    Some("roster.agents"),
                    "role=main roster agent must not have parent_id",
                );
            }
        } else if agent.parent_id.is_none() {
            rep.error(
                "roster_agent_parent_missing",
                Some(key),
                None,
                Some("roster.agents"),
                "non-main roster agent requires parent_id",
            );
        }
        if let Some(parent) = &agent.parent_id {
            if parent == &agent.id {
                rep.error(
                    "roster_agent_parent_self",
                    Some(key),
                    None,
                    Some("roster.agents"),
                    "roster agent cannot be its own parent",
                );
            } else if !roster_agents.contains_key(parent.as_str()) {
                rep.error(
                    "roster_agent_parent_unknown",
                    Some(key),
                    None,
                    Some("roster.agents"),
                    "roster agent parent_id is not in roster.agents",
                );
            }
        }
        if let Some(model) = &agent.model {
            if model.trim().is_empty() {
                rep.error(
                    "empty_field",
                    Some(key),
                    None,
                    Some("roster.agents.model"),
                    "roster agent model must be non-empty when present",
                );
            }
        }
    }

    for agent in &session.roster.agents {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut current = agent.parent_id.as_deref();
        while let Some(parent) = current {
            if !seen.insert(parent) {
                rep.error(
                    "roster_agent_parent_cycle",
                    Some(key),
                    None,
                    Some("roster.agents"),
                    "roster agent parent chain contains a cycle",
                );
                break;
            }
            current = roster_agents
                .get(parent)
                .and_then(|agent| agent.parent_id.as_deref());
        }
    }

    for participant in &session.participants {
        if participant.kind != ParticipantKind::Agent {
            continue;
        }
        match roster_agents.get(participant.id.as_str()) {
            None => rep.error(
                "agent_not_in_roster",
                Some(key),
                None,
                Some("roster.agents"),
                "agent participant is missing from roster.agents",
            ),
            Some(agent) => {
                if participant.role != Some(agent.role) {
                    rep.error(
                        "participant_roster_role_mismatch",
                        Some(key),
                        None,
                        Some("participants.role"),
                        "participant role does not match roster agent role",
                    );
                }
                if participant.parent_id.as_deref() != agent.parent_id.as_deref() {
                    rep.error(
                        "participant_roster_parent_mismatch",
                        Some(key),
                        None,
                        Some("participants.parent_id"),
                        "participant parent_id does not match roster agent parent_id",
                    );
                }
                if participant.model.as_deref() != agent.model.as_deref()
                    && participant.model.is_some()
                {
                    rep.warn(
                        "participant_roster_model_mismatch",
                        Some(key),
                        None,
                        Some("participants.model"),
                        "participant model differs from roster agent model",
                    );
                }
            }
        }
    }

    let mut tools: HashSet<&str> = HashSet::new();
    for tool in &session.roster.tools {
        if tool.name.trim().is_empty() {
            rep.error(
                "empty_roster_tool",
                Some(key),
                None,
                Some("roster.tools"),
                "roster tool name must be non-empty",
            );
            continue;
        }
        if !tools.insert(tool.name.as_str()) {
            rep.error(
                "duplicate_roster_tool",
                Some(key),
                None,
                Some("roster.tools"),
                format!("duplicate roster tool {}", tool.name),
            );
        }
        if tool
            .version
            .as_deref()
            .map(|value| value.trim().is_empty())
            .unwrap_or(false)
        {
            rep.error(
                "empty_field",
                Some(key),
                None,
                Some("roster.tools.version"),
                "roster tool version must be non-empty when present",
            );
        }
        if tool
            .definition_ref
            .as_deref()
            .map(|value| value.trim().is_empty())
            .unwrap_or(false)
        {
            rep.error(
                "empty_field",
                Some(key),
                None,
                Some("roster.tools.definition_ref"),
                "roster tool definition_ref must be non-empty when present",
            );
        }
    }
    tools
}

fn validate_ownership(
    ownership: &Ownership,
    participants: &HashMap<&str, &Participant>,
    roster_agents: &HashMap<&str, &RosterAgent>,
    key: &str,
    rep: &mut V3Report,
) {
    if ownership.owner_agent_id.trim().is_empty() {
        rep.error(
            "empty_field",
            Some(key),
            None,
            Some("ownership.owner_agent_id"),
            "owner_agent_id must be non-empty",
        );
    } else {
        let owner = participants.get(ownership.owner_agent_id.as_str());
        if owner.map(|p| p.kind) != Some(ParticipantKind::Agent)
            && !roster_agents.contains_key(ownership.owner_agent_id.as_str())
        {
            rep.error(
                "owner_not_agent",
                Some(key),
                None,
                Some("ownership.owner_agent_id"),
                "owner_agent_id must reference an agent",
            );
        }
    }
    if ownership.scope.key.trim().is_empty() {
        rep.error(
            "empty_scope_key",
            Some(key),
            None,
            Some("ownership.scope.key"),
            "scope.key must be non-empty",
        );
    }
    if !valid_retention(&ownership.retention) {
        rep.error(
            "bad_retention",
            Some(key),
            None,
            Some("ownership.retention"),
            "retention must be default, keep, or days:<n>",
        );
    }
    if ownership
        .principal_id
        .as_deref()
        .map(|value| value.trim().is_empty())
        .unwrap_or(false)
    {
        rep.error(
            "empty_field",
            Some(key),
            None,
            Some("ownership.principal_id"),
            "principal_id must be non-empty when present",
        );
    }
}

fn validate_threads(
    session: &V3Session,
    roster_agents: &HashMap<&str, &RosterAgent>,
    threads_by_uid: &HashMap<&str, &Thread>,
    key: &str,
    rep: &mut V3Report,
) {
    if session.threads.is_empty() {
        rep.error(
            "no_threads",
            Some(key),
            None,
            Some("threads"),
            "at least one thread is required",
        );
        return;
    }
    let mut keys: HashSet<&str> = HashSet::new();
    let mut main_count = 0usize;
    for thread in &session.threads {
        if !keys.insert(thread.thread_key.as_str()) {
            rep.error(
                "duplicate_thread_key",
                Some(key),
                None,
                Some("threads"),
                format!("duplicate thread_key {}", thread.thread_key),
            );
        }
        if thread.kind == ThreadKind::Main {
            main_count += 1;
        }
        if !roster_agents.contains_key(thread.agent_id.as_str()) {
            rep.error(
                "thread_agent_unknown",
                Some(key),
                None,
                Some("threads.agent_id"),
                "thread agent_id is not in roster.agents",
            );
        }
        if let Some(parent) = &thread.parent_thread_key {
            if parent == &thread.thread_key {
                rep.error(
                    "thread_parent_self",
                    Some(key),
                    None,
                    Some("threads"),
                    "thread cannot be its own parent",
                );
            } else if !session
                .threads
                .iter()
                .any(|candidate| &candidate.thread_key == parent)
            {
                rep.error(
                    "thread_parent_unknown",
                    Some(key),
                    None,
                    Some("threads"),
                    "parent_thread_key is not in threads",
                );
            }
        }
        let expected = compute_thread_uid(&session.session_uid, &thread.thread_key);
        if thread.thread_uid != expected {
            rep.error(
                "thread_uid_mismatch",
                Some(key),
                None,
                Some("threads.thread_uid"),
                "thread_uid does not match session_uid/thread_key",
            );
        }
    }
    if main_count != 1 {
        rep.error(
            "main_thread_count",
            Some(key),
            None,
            Some("threads"),
            format!("expected exactly one main thread, got {main_count}"),
        );
    }
    for thread in &session.threads {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut current = thread.parent_thread_key.as_deref();
        while let Some(parent) = current {
            if !seen.insert(parent) {
                rep.error(
                    "thread_parent_cycle",
                    Some(key),
                    None,
                    Some("threads"),
                    "thread parent chain contains a cycle",
                );
                break;
            }
            current = session
                .threads
                .iter()
                .find(|candidate| candidate.thread_key == parent)
                .and_then(|candidate| candidate.parent_thread_key.as_deref());
        }
    }
    if threads_by_uid.len() != session.threads.len() {
        // 重复 uid 已在建表时报过，这里只防止后续事件引用歧义。
        rep.warn(
            "duplicate_thread_uid_effect",
            Some(key),
            None,
            Some("threads.thread_uid"),
            "duplicate thread_uid makes event resolution ambiguous",
        );
    }
}

fn validate_turns(
    session: &V3Session,
    threads_by_uid: &HashMap<&str, &Thread>,
    turns_by_uid: &HashMap<&str, &Turn>,
    events_by_seq: &HashMap<u32, &V3Event>,
    key: &str,
    rep: &mut V3Report,
) {
    if session.turns.is_empty() {
        rep.error(
            "no_turns",
            Some(key),
            None,
            Some("turns"),
            "at least one turn is required",
        );
        return;
    }
    let mut keys: HashSet<&str> = HashSet::new();
    let mut intervals: HashMap<&str, Vec<(u32, u32)>> = HashMap::new();
    for turn in &session.turns {
        if turn.turn_key.trim().is_empty() {
            rep.error(
                "empty_turn_key",
                Some(key),
                None,
                Some("turns"),
                "turn_key must be non-empty",
            );
        }
        if !keys.insert(turn.turn_key.as_str()) {
            rep.error(
                "duplicate_turn_key",
                Some(key),
                None,
                Some("turns"),
                format!("duplicate turn_key {}", turn.turn_key),
            );
        }
        if turn.start_seq == 0 || turn.start_seq > turn.end_seq {
            rep.error(
                "bad_turn_interval",
                Some(key),
                None,
                Some("turns"),
                "turn interval must satisfy 1 <= start_seq <= end_seq",
            );
            continue;
        }
        if turn.end_seq as usize > session.events.len() {
            rep.error(
                "bad_turn_interval",
                Some(key),
                None,
                Some("turns"),
                "turn end_seq is outside the event list",
            );
        }
        let thread = match threads_by_uid.get(turn.thread_uid.as_str()) {
            None => {
                rep.error(
                    "turn_thread_unknown",
                    Some(key),
                    None,
                    Some("turns.thread_uid"),
                    "turn thread_uid is not in threads",
                );
                continue;
            }
            Some(thread) => *thread,
        };
        for seq in [turn.start_seq, turn.end_seq] {
            match events_by_seq.get(&seq) {
                None => rep.error(
                    "turn_range_missing",
                    Some(key),
                    Some(seq),
                    Some("turns"),
                    "turn endpoint seq does not exist",
                ),
                Some(event) if event.thread_uid != thread.thread_uid => rep.error(
                    "turn_endpoint_thread_mismatch",
                    Some(key),
                    Some(seq),
                    Some("turns"),
                    "turn endpoint belongs to a different thread",
                ),
                Some(_) => {}
            }
        }
        if turn.derived {
            let anchor = session
                .events
                .iter()
                .find(|event| {
                    event.thread_uid == thread.thread_uid && event.seq >= turn.start_seq
                });
            match anchor {
                None => rep.error(
                    "turn_anchor_missing",
                    Some(key),
                    Some(turn.start_seq),
                    Some("turns"),
                    "derived turn has no anchor event",
                ),
                Some(anchor) => {
                    let expected = compute_turn_uid_from_anchor(
                        &thread.thread_uid,
                        &anchor.event_uid,
                    );
                    if turn.turn_uid != expected {
                        rep.error(
                            "turn_uid_mismatch",
                            Some(key),
                            Some(anchor.seq),
                            Some("turns.turn_uid"),
                            "derived turn_uid does not match its anchor event",
                        );
                    }
                }
            }
        } else {
            let expected = compute_turn_uid_native(&thread.thread_uid, &turn.turn_key);
            if turn.turn_uid != expected {
                rep.error(
                    "turn_uid_mismatch",
                    Some(key),
                    None,
                    Some("turns.turn_uid"),
                    "native turn_uid does not match thread_uid/turn_key",
                );
            }
        }
        intervals
            .entry(thread.thread_uid.as_str())
            .or_default()
            .push((turn.start_seq, turn.end_seq));
    }
    for values in intervals.values_mut() {
        values.sort_unstable();
        for pair in values.windows(2) {
            if pair[0].1 >= pair[1].0 {
                rep.error(
                    "turn_overlap",
                    Some(key),
                    None,
                    Some("turns"),
                    "turn intervals overlap within a thread",
                );
            }
        }
    }
    if turns_by_uid.len() != session.turns.len() {
        rep.warn(
            "duplicate_turn_uid_effect",
            Some(key),
            None,
            Some("turns.turn_uid"),
            "duplicate turn_uid makes event resolution ambiguous",
        );
    }
}

fn validate_docs(session: &V3Session, key: &str, rep: &mut V3Report) {
    for doc in &session.docs {
        if doc.path.trim().is_empty() {
            rep.error(
                "empty_doc_path",
                Some(key),
                None,
                Some("docs.path"),
                "doc path must be non-empty",
            );
        }
        if let Some(sha) = &doc.sha256 {
            if !is_hex64(sha) {
                rep.error(
                    "bad_doc_sha256",
                    Some(key),
                    None,
                    Some("docs.sha256"),
                    "doc sha256 must be 64 hex characters",
                );
            }
        }
        if doc.mime.as_deref().map(str::trim).unwrap_or("").is_empty()
            && doc.mime.is_some()
        {
            rep.error(
                "empty_field",
                Some(key),
                None,
                Some("docs.mime"),
                "doc mime must be non-empty when present",
            );
        }
        if doc.size == Some(0) {
            rep.error(
                "bad_doc_size",
                Some(key),
                None,
                Some("docs.size"),
                "doc size must be > 0 when present",
            );
        }
        if doc.title.as_deref().map(str::trim).unwrap_or("").is_empty()
            && doc.title.is_some()
        {
            rep.error(
                "empty_field",
                Some(key),
                None,
                Some("docs.title"),
                "doc title must be non-empty when present",
            );
        }
    }
}

fn validate_secrets(
    session: &V3Session,
    roster_agents: &HashMap<&str, &RosterAgent>,
    key_manifest: Option<&KeyManifest>,
    key: &str,
    rep: &mut V3Report,
) {
    let keys: HashMap<&str, &KeyEntry> = key_manifest
        .map(|manifest| {
            manifest
                .keys
                .iter()
                .map(|entry| (entry.key_id.as_str(), entry))
                .collect()
        })
        .unwrap_or_default();
    for secret in &session.secrets {
        if !valid_secret_id(&secret.secret_id) {
            rep.error(
                "bad_secret_id",
                Some(key),
                None,
                Some("secrets.secret_id"),
                "secret_id must look like sec-<agent>-<seq>",
            );
        }
        if secret.alias.trim().is_empty() {
            rep.error(
                "empty_secret_alias",
                Some(key),
                None,
                Some("secrets.alias"),
                "secret alias must be non-empty",
            );
        }
        if secret.version == 0 {
            rep.error(
                "bad_secret_version",
                Some(key),
                None,
                Some("secrets.version"),
                "secret version must be >= 1",
            );
        }
        if !is_fingerprint8(&secret.fingerprint) {
            rep.error(
                "bad_secret_fingerprint",
                Some(key),
                None,
                Some("secrets.fingerprint"),
                "secret fingerprint must be 8 lowercase hex characters",
            );
        }
        if !roster_agents.contains_key(secret.owner_agent_id.as_str()) {
            rep.error(
                "secret_owner_unknown",
                Some(key),
                None,
                Some("secrets.owner_agent_id"),
                "secret owner_agent_id is not in roster.agents",
            );
        }
        if let Some(key_id) = &secret.key_id {
            if !is_safe_token(key_id) {
                rep.error(
                    "bad_key_id",
                    Some(key),
                    None,
                    Some("secrets.key_id"),
                    "secret key_id is not a safe token",
                );
            } else if let Some(entry) = keys.get(key_id.as_str()) {
                if entry.kind == KeyKind::Backup {
                    rep.error(
                        "backup_key_used_for_secret",
                        Some(key),
                        None,
                        Some("secrets.key_id"),
                        "backup keys cannot be used for event secrets",
                    );
                }
                match entry.status {
                    KeyStatus::Revoked | KeyStatus::Destroyed => rep.error(
                        "secret_key_unusable",
                        Some(key),
                        None,
                        Some("secrets.key_id"),
                        "secret references a revoked or destroyed key",
                    ),
                    KeyStatus::Retired => rep.warn(
                        "secret_key_retired",
                        Some(key),
                        None,
                        Some("secrets.key_id"),
                        "secret references a retired key",
                    ),
                    KeyStatus::Active => {}
                }
            } else if key_manifest.is_some() {
                rep.error(
                    "secret_key_unknown",
                    Some(key),
                    None,
                    Some("secrets.key_id"),
                    "secret key_id is not in key_manifest",
                );
            }
        } else if key_manifest.is_some() {
            rep.warn(
                "secret_key_missing",
                Some(key),
                None,
                Some("secrets.key_id"),
                "secret has no key_id while a key manifest is present",
            );
        }
    }
}

fn valid_secret_id(secret_id: &str) -> bool {
    let Some(rest) = secret_id.strip_prefix("sec-") else {
        return false;
    };
    let Some((agent, seq)) = rest.rsplit_once('-') else {
        return false;
    };
    !agent.is_empty()
        && !seq.is_empty()
        && seq.bytes().all(|b| b.is_ascii_digit())
        && is_safe_token(secret_id)
}

fn is_reason_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn validate_events(
    session: &V3Session,
    ctx: &SessionCtx<'_>,
    key: &str,
    rep: &mut V3Report,
) {
    let mut calls: HashMap<String, u32> = HashMap::new();
    let mut warned_tools: HashSet<String> = HashSet::new();
    let mut occurrences: HashSet<(&str, &str, u32)> = HashSet::new();
    let mut attempts: HashSet<(&str, &str, u32, u32)> = HashSet::new();
    let mut finals: HashMap<(&str, &str, u32), u32> = HashMap::new();
    let mut last_ts: Option<i64> = None;
    let mut source_mismatch_warned = false;

    for (index, event) in session.events.iter().enumerate() {
        let seq = Some(event.seq);
        rep.stats.events += 1;
        match event.kind {
            EventKind::HumanMsg => rep.stats.human_msgs += 1,
            EventKind::AgentMsg => rep.stats.agent_msgs += 1,
            EventKind::ToolCall => rep.stats.tool_calls += 1,
            EventKind::ToolResult => rep.stats.tool_results += 1,
            EventKind::DocRef => rep.stats.doc_refs += 1,
            EventKind::Summary => rep.stats.summaries += 1,
            EventKind::Attachment => rep.stats.attachments += 1,
            EventKind::System => {}
        }
        if event.ts_ms.is_none() {
            rep.stats.time_unknown += 1;
        }
        let expected_seq = (index + 1) as u32;
        if event.seq != expected_seq {
            rep.error(
                "seq_not_contiguous",
                Some(key),
                seq,
                Some("seq"),
                format!("expected {expected_seq}, got {}", event.seq),
            );
        }

        for (field, value) in [
            ("source_ref.adapter", &event.source_ref.adapter),
            ("source_ref.path", &event.source_ref.path),
        ] {
            if value.trim().is_empty() {
                rep.error(
                    "empty_field",
                    Some(key),
                    seq,
                    Some(field),
                    format!("{field} must be non-empty"),
                );
            }
        }
        if event.source_ref.adapter_version == 0 {
            rep.error(
                "bad_adapter_version",
                Some(key),
                seq,
                Some("source_ref.adapter_version"),
                "adapter_version must be >= 1",
            );
        }
        if event.source_ref.line == Some(0) {
            rep.error(
                "bad_source_line",
                Some(key),
                seq,
                Some("source_ref.line"),
                "source line must be >= 1",
            );
        }
        for (field, value) in [
            ("source_ref.native_id", &event.source_ref.native_id),
            ("source_ref.native_step", &event.source_ref.native_step),
        ] {
            if value
                .as_deref()
                .map(|value| value.trim().is_empty())
                .unwrap_or(false)
            {
                rep.error(
                    "empty_field",
                    Some(key),
                    seq,
                    Some(field),
                    format!("{field} must be non-empty when present"),
                );
            }
        }
        if !source_mismatch_warned
            && (event.source_ref.adapter != session.source.adapter
                || event.source_ref.path != session.source.path
                || event.source_ref.adapter_version != session.source.adapter_version)
        {
            rep.warn(
                "source_ref_mismatch",
                Some(key),
                seq,
                Some("source_ref"),
                "event source_ref differs from session source",
            );
            source_mismatch_warned = true;
        }

        if let Some(parent_seq) = event.parent_seq {
            if parent_seq == 0 || parent_seq >= event.seq {
                rep.error(
                    "bad_parent",
                    Some(key),
                    seq,
                    Some("parent_seq"),
                    format!("parent_seq {parent_seq} must be 1..< seq"),
                );
            } else if let (Some(parent), Some(parent_uid)) =
                (ctx.events_by_seq.get(&parent_seq), &event.parent_event_uid)
            {
                if parent.event_uid != *parent_uid {
                    rep.error(
                        "parent_mismatch",
                        Some(key),
                        seq,
                        Some("parent_event_uid"),
                        "parent_event_uid does not match parent_seq",
                    );
                }
            }
        }
        if let Some(parent_uid) = &event.parent_event_uid {
            if !is_hex64(parent_uid) {
                rep.error(
                    "bad_parent_uid",
                    Some(key),
                    seq,
                    Some("parent_event_uid"),
                    "parent_event_uid must be 64 hex characters",
                );
            } else {
                match ctx.events_by_uid.get(parent_uid.as_str()) {
                    None => rep.error(
                        "parent_uid_unresolved",
                        Some(key),
                        seq,
                        Some("parent_event_uid"),
                        "parent_event_uid is not in this session",
                    ),
                    Some(parent) => {
                        if parent.seq >= event.seq {
                            rep.error(
                                "parent_after_child",
                                Some(key),
                                seq,
                                Some("parent_event_uid"),
                                "parent event must come before its child",
                            );
                        }
                        if let Some(parent_seq) = event.parent_seq {
                            if parent_seq != parent.seq {
                                rep.error(
                                    "parent_mismatch",
                                    Some(key),
                                    seq,
                                    Some("parent_event_uid"),
                                    "parent_event_uid does not match parent_seq",
                                );
                            }
                        }
                    }
                }
            }
        }

        if event.thread_uid.trim().is_empty() {
            rep.error(
                "empty_thread_uid",
                Some(key),
                seq,
                Some("thread_uid"),
                "thread_uid must be non-empty",
            );
        } else if !ctx.threads_by_uid.contains_key(event.thread_uid.as_str()) {
            rep.error(
                "unknown_thread",
                Some(key),
                seq,
                Some("thread_uid"),
                "thread_uid is not in threads",
            );
        }
        match ctx.turns_by_uid.get(event.turn_uid.as_str()) {
            None => rep.error(
                "unknown_turn",
                Some(key),
                seq,
                Some("turn_uid"),
                "turn_uid is not in turns",
            ),
            Some(turn) => {
                if turn.thread_uid != event.thread_uid {
                    rep.error(
                        "turn_thread_mismatch",
                        Some(key),
                        seq,
                        Some("turn_uid"),
                        "event thread_uid does not match its turn",
                    );
                }
                if event.seq < turn.start_seq || event.seq > turn.end_seq {
                    rep.error(
                        "event_outside_turn",
                        Some(key),
                        seq,
                        Some("turn_uid"),
                        "event seq is outside its turn interval",
                    );
                }
            }
        }

        if event.native_key.trim().is_empty() {
            rep.error(
                "empty_native_key",
                Some(key),
                seq,
                Some("native_key"),
                "native_key must be non-empty",
            );
        }
        if !is_hex64(&event.content_hash) {
            rep.error(
                "bad_content_hash",
                Some(key),
                seq,
                Some("content_hash"),
                "content_hash must be 64 hex characters",
            );
        } else {
            let expected = recompute_event_content_hash(event);
            if event.content_hash != expected {
                rep.error(
                    "content_hash_mismatch",
                    Some(key),
                    seq,
                    Some("content_hash"),
                    "content_hash does not match the event payload",
                );
            }
        }
        if !is_hex64(&event.event_uid) {
            rep.error(
                "bad_event_uid",
                Some(key),
                seq,
                Some("event_uid"),
                "event_uid must be 64 hex characters",
            );
        } else {
            let expected = crate::identity_v3::compute_event_uid(
                &event.thread_uid,
                &event.native_key,
                &event.content_hash,
                event.occurrence,
            );
            if event.event_uid != expected {
                rep.error(
                    "event_uid_mismatch",
                    Some(key),
                    seq,
                    Some("event_uid"),
                    "event_uid does not match thread_uid/native_key/content_hash/occurrence",
                );
            }
        }
        if event.revision_id != session.source.revision_id {
            rep.error(
                "revision_mismatch",
                Some(key),
                seq,
                Some("revision_id"),
                "event revision_id does not match session source revision_id",
            );
        }
        if let Some(revision_of) = &event.revision_of {
            if !is_hex64(revision_of) {
                rep.error(
                    "bad_revision_of",
                    Some(key),
                    seq,
                    Some("revision_of"),
                    "revision_of must be 64 hex characters",
                );
            } else if revision_of == &event.event_uid {
                rep.error(
                    "revision_self",
                    Some(key),
                    seq,
                    Some("revision_of"),
                    "revision_of cannot be the event itself",
                );
            }
        }
        if let Some(duplicate_of) = &event.duplicate_of {
            if !is_hex64(duplicate_of) {
                rep.error(
                    "bad_duplicate_of",
                    Some(key),
                    seq,
                    Some("duplicate_of"),
                    "duplicate_of must be 64 hex characters",
                );
            } else if duplicate_of == &event.event_uid {
                rep.error(
                    "duplicate_self",
                    Some(key),
                    seq,
                    Some("duplicate_of"),
                    "duplicate_of cannot be the event itself",
                );
            } else {
                match ctx.events_by_uid.get(duplicate_of.as_str()) {
                    Some(original) if original.seq >= event.seq => rep.error(
                        "duplicate_after_canonical",
                        Some(key),
                        seq,
                        Some("duplicate_of"),
                        "duplicate must point to an earlier event",
                    ),
                    Some(_) => {}
                    None => rep.warn(
                        "duplicate_target_unresolved",
                        Some(key),
                        seq,
                        Some("duplicate_of"),
                        "duplicate target is not in this session",
                    ),
                }
            }
        }

        let occurrence_key = (
            event.thread_uid.as_str(),
            event.native_key.as_str(),
            event.occurrence,
        );
        if !occurrences.insert(occurrence_key) {
            rep.error(
                "duplicate_occurrence",
                Some(key),
                seq,
                Some("occurrence"),
                "same thread/native_key/occurrence appeared twice",
            );
        }
        let attempt_key = (
            event.thread_uid.as_str(),
            event.native_key.as_str(),
            event.occurrence,
            event.attempt,
        );
        if !attempts.insert(attempt_key) {
            rep.error(
                "duplicate_attempt",
                Some(key),
                seq,
                Some("attempt"),
                "same thread/native_key/occurrence/attempt appeared twice",
            );
        }
        if event.final_ {
            if finals
                .insert(
                    (
                        event.thread_uid.as_str(),
                        event.native_key.as_str(),
                        event.occurrence,
                    ),
                    event.seq,
                )
                .is_some()
            {
                rep.error(
                    "multiple_final",
                    Some(key),
                    seq,
                    Some("final"),
                    "only one final event is allowed per occurrence",
                );
            }
        } else if event.attempt == 0 {
            rep.warn(
                "non_final_without_attempt",
                Some(key),
                seq,
                Some("final"),
                "non-final event has attempt=0",
            );
        }

        let normalized = normalize_text_v3(&event.text);
        if event.text != normalized {
            rep.error(
                "text_not_normalized",
                Some(key),
                seq,
                Some("text"),
                "text must be LF/NFC/trimmed before hashing",
            );
        }
        if !is_hex64(&event.text_sha256) {
            rep.error(
                "bad_text_sha256",
                Some(key),
                seq,
                Some("text_sha256"),
                "text_sha256 must be 64 hex characters",
            );
        } else if event.text_sha256 != text_sha256(&event.text) {
            rep.error(
                "text_sha256_mismatch",
                Some(key),
                seq,
                Some("text_sha256"),
                "text_sha256 does not match text",
            );
        }
        match &event.span {
            Some(span) => {
                if span.start > span.end || span.end > char_count(&event.text) {
                    rep.error(
                        "bad_span",
                        Some(key),
                        seq,
                        Some("span"),
                        "span must be inside the event text",
                    );
                } else {
                    match char_slice(&event.text, span.start, span.end) {
                        None => rep.error(
                            "bad_span",
                            Some(key),
                            seq,
                            Some("span"),
                            "span cannot be sliced from the event text",
                        ),
                        Some(slice) => match &span.text_sha256 {
                            None => rep.error(
                                "span_hash_missing",
                                Some(key),
                                seq,
                                Some("span.text_sha256"),
                                "event span requires text_sha256",
                            ),
                            Some(hash) if !is_hex64(hash) => rep.error(
                                "bad_span_hash",
                                Some(key),
                                seq,
                                Some("span.text_sha256"),
                                "span text_sha256 must be 64 hex characters",
                            ),
                            Some(hash) if hash != &text_sha256(slice) => rep.error(
                                "span_hash_mismatch",
                                Some(key),
                                seq,
                                Some("span.text_sha256"),
                                "span text_sha256 does not match the slice",
                            ),
                            Some(_) => {}
                        },
                    }
                }
            }
            None => {
                if !event.text.is_empty() {
                    rep.warn(
                        "span_missing",
                        Some(key),
                        seq,
                        Some("span"),
                        "non-empty text has no span",
                    );
                }
            }
        }
        if event.norm_version != INGEST_V3_NORM_VERSION {
            rep.error(
                "norm_version",
                Some(key),
                seq,
                Some("norm_version"),
                format!(
                    "expected {}, got {}",
                    INGEST_V3_NORM_VERSION, event.norm_version
                ),
            );
        }

        check_time_range(event.ts_ms, "ts_ms", key, seq, rep);
        check_time_range(event.observed_at_ms, "observed_at_ms", key, seq, rep);
        check_time_range(event.valid_from_ms, "valid_from_ms", key, seq, rep);
        check_time_range(event.valid_to_ms, "valid_to_ms", key, seq, rep);
        if let (Some(from), Some(to)) = (event.valid_from_ms, event.valid_to_ms) {
            if from > to {
                rep.error(
                    "bad_validity_window",
                    Some(key),
                    seq,
                    Some("valid_from_ms"),
                    "valid_from_ms must be <= valid_to_ms",
                );
            }
        }
        if let (Some(ts), Some(observed)) = (event.ts_ms, event.observed_at_ms) {
            if observed < ts {
                rep.warn(
                    "observed_before_event",
                    Some(key),
                    seq,
                    Some("observed_at_ms"),
                    "observed_at_ms is before ts_ms",
                );
            }
        }
        if let Some(ts) = event.ts_ms {
            if let Some(previous) = last_ts {
                if ts < previous {
                    rep.warn(
                        "time_order",
                        Some(key),
                        seq,
                        Some("ts_ms"),
                        format!("ts_ms went backwards: {previous} -> {ts}; seq stays authoritative"),
                    );
                }
            }
            last_ts = Some(ts);
        }

        match ctx.participants.get(event.speaker_id.as_str()) {
            None => rep.error(
                "unknown_speaker",
                Some(key),
                seq,
                Some("speaker_id"),
                format!("speaker_id {} is not a participant", event.speaker_id),
            ),
            Some(participant) => {
                if !speaker_kind_ok(event.kind, participant.kind) {
                    rep.error(
                        "speaker_kind_mismatch",
                        Some(key),
                        seq,
                        Some("speaker_id"),
                        format!(
                            "participant {} cannot speak a {:?} event",
                            event.speaker_id, event.kind
                        ),
                    );
                }
                validate_event_payload(
                    event,
                    ctx,
                    &mut calls,
                    &mut warned_tools,
                    key,
                    rep,
                );
                validate_event_trust_speech(event, participant, key, rep);
            }
        }
        validate_event_refs(event, ctx, &calls, &session.environment.machine_id, key, rep);
        if let Some(record) = validate_event_quality(event, key, rep) {
            rep.quarantined.push(record);
        }
        scan_event(event, key, rep);
    }
}

fn speaker_kind_ok(kind: EventKind, participant: ParticipantKind) -> bool {
    match kind {
        EventKind::HumanMsg => participant == ParticipantKind::Human,
        EventKind::AgentMsg | EventKind::ToolCall => participant == ParticipantKind::Agent,
        EventKind::ToolResult => {
            matches!(participant, ParticipantKind::Tool | ParticipantKind::System)
        }
        EventKind::DocRef | EventKind::Attachment => matches!(
            participant,
            ParticipantKind::Agent | ParticipantKind::Tool | ParticipantKind::System
        ),
        EventKind::Summary | EventKind::System => {
            matches!(participant, ParticipantKind::System | ParticipantKind::Agent)
        }
    }
}

fn expected_trust(event: &V3Event, participant: &Participant) -> Option<Trust> {
    let agent_trust = if participant.role == Some(ParticipantRole::Subagent) {
        Trust::Subagent
    } else {
        Trust::Agent
    };
    match event.kind {
        EventKind::HumanMsg => Some(Trust::User),
        EventKind::AgentMsg | EventKind::ToolCall => Some(agent_trust),
        EventKind::ToolResult => Some(Trust::Tool),
        EventKind::DocRef | EventKind::Attachment => match participant.kind {
            ParticipantKind::System => Some(Trust::Harness),
            ParticipantKind::Tool => Some(Trust::Tool),
            ParticipantKind::Agent => Some(agent_trust),
            ParticipantKind::Human => None,
        },
        EventKind::Summary => match participant.kind {
            ParticipantKind::System => Some(Trust::Harness),
            ParticipantKind::Agent => Some(agent_trust),
            _ => None,
        },
        EventKind::System => event.system_kind.map(|kind| match kind {
            SystemKind::InjectedNotice => Trust::Injected,
            SystemKind::HarnessNotice | SystemKind::Policy => Trust::Harness,
        }),
    }
}

fn speech_act_ok(kind: EventKind, speech_act: SpeechAct) -> bool {
    match kind {
        EventKind::ToolCall => matches!(speech_act, SpeechAct::Request | SpeechAct::Command),
        EventKind::ToolResult
        | EventKind::DocRef
        | EventKind::Attachment
        | EventKind::Summary
        | EventKind::System => speech_act == SpeechAct::Assert,
        EventKind::HumanMsg | EventKind::AgentMsg => true,
    }
}

fn validate_event_trust_speech(
    event: &V3Event,
    participant: &Participant,
    key: &str,
    rep: &mut V3Report,
) {
    let seq = Some(event.seq);
    if let Some(expected) = expected_trust(event, participant) {
        if event.trust != expected {
            rep.error(
                "trust_kind_mismatch",
                Some(key),
                seq,
                Some("trust"),
                format!("trust {:?} does not match the event kind/speaker", event.trust),
            );
        }
    }
    if !speech_act_ok(event.kind, event.speech_act) {
        rep.error(
            "speech_act_kind_mismatch",
            Some(key),
            seq,
            Some("speech_act"),
            "speech_act is not allowed for this event kind",
        );
    }
}

fn validate_event_payload(
    event: &V3Event,
    ctx: &SessionCtx<'_>,
    calls: &mut HashMap<String, u32>,
    warned_tools: &mut HashSet<String>,
    key: &str,
    rep: &mut V3Report,
) {
    let seq = Some(event.seq);
    match event.kind {
        EventKind::HumanMsg
        | EventKind::AgentMsg
        | EventKind::System
        | EventKind::Summary => {
            if event.text.trim().is_empty() && !is_quarantined(event) {
                rep.error(
                    "empty_text",
                    Some(key),
                    seq,
                    Some("text"),
                    format!("{:?} requires text", event.kind),
                );
            }
            if event.tool.is_some() || event.tool_result.is_some() || event.doc.is_some() {
                rep.error(
                    "extra_payload",
                    Some(key),
                    seq,
                    None,
                    "this event kind cannot carry tool/tool_result/doc",
                );
            }
            if event.kind == EventKind::System {
                if event.system_kind.is_none() {
                    rep.error(
                        "missing_system_kind",
                        Some(key),
                        seq,
                        Some("system_kind"),
                        "system event requires system_kind",
                    );
                }
            } else if event.system_kind.is_some() {
                rep.error(
                    "system_kind_on_non_system",
                    Some(key),
                    seq,
                    Some("system_kind"),
                    "system_kind is only allowed on system events",
                );
            }
        }
        EventKind::ToolCall => {
            match &event.tool {
                None => rep.error(
                    "missing_tool",
                    Some(key),
                    seq,
                    Some("tool"),
                    "tool_call requires tool",
                ),
                Some(call) => {
                    if call.call_id.trim().is_empty() {
                        rep.error(
                            "empty_call_id",
                            Some(key),
                            seq,
                            Some("tool.call_id"),
                            "call_id must be non-empty",
                        );
                    } else if calls.insert(call.call_id.clone(), event.seq).is_some() {
                        rep.error(
                            "duplicate_call_id",
                            Some(key),
                            seq,
                            Some("tool.call_id"),
                            format!("duplicate call_id {}", call.call_id),
                        );
                    }
                    if call.name.trim().is_empty() {
                        rep.error(
                            "empty_tool_name",
                            Some(key),
                            seq,
                            Some("tool.name"),
                            "tool name must be non-empty",
                        );
                    } else if !ctx.roster_tools.contains(call.name.as_str())
                        && warned_tools.insert(call.name.clone())
                    {
                        rep.warn(
                            "tool_not_in_roster",
                            Some(key),
                            seq,
                            Some("tool.name"),
                            "tool name is not declared in roster.tools",
                        );
                    }
                    if call.arguments.is_null() {
                        rep.error(
                            "null_tool_arguments",
                            Some(key),
                            seq,
                            Some("tool.arguments"),
                            "tool arguments must not be null",
                        );
                    }
                }
            }
            if event.tool_result.is_some() || event.doc.is_some() {
                rep.error(
                    "extra_payload",
                    Some(key),
                    seq,
                    None,
                    "tool_call cannot carry tool_result/doc",
                );
            }
            if !event.text.trim().is_empty() {
                rep.warn(
                    "tool_call_text",
                    Some(key),
                    seq,
                    Some("text"),
                    "tool_call carries text; content should normally be in the call payload",
                );
            }
            if event.system_kind.is_some() {
                rep.error(
                    "system_kind_on_non_system",
                    Some(key),
                    seq,
                    Some("system_kind"),
                    "system_kind is only allowed on system events",
                );
            }
        }
        EventKind::ToolResult => {
            match &event.tool_result {
                None => rep.error(
                    "missing_tool_result",
                    Some(key),
                    seq,
                    Some("tool_result"),
                    "tool_result requires tool_result",
                ),
                Some(result) => {
                    if result.call_id.trim().is_empty() {
                        rep.error(
                            "empty_call_id",
                            Some(key),
                            seq,
                            Some("tool_result.call_id"),
                            "call_id must be non-empty",
                        );
                    } else {
                        match calls.get(result.call_id.as_str()) {
                            None => rep.error(
                                "orphan_tool_result",
                                Some(key),
                                seq,
                                Some("tool_result.call_id"),
                                format!(
                                    "no earlier tool_call with call_id {}",
                                    result.call_id
                                ),
                            ),
                            Some(call_seq) if *call_seq >= event.seq => rep.error(
                                "tool_result_before_call",
                                Some(key),
                                seq,
                                Some("tool_result.call_id"),
                                "tool_result must come after its call",
                            ),
                            Some(_) => {}
                        }
                    }
                    let has_text = result
                        .text
                        .as_deref()
                        .map(|text| !text.trim().is_empty())
                        .unwrap_or(false);
                    let has_details = result
                        .details
                        .as_ref()
                        .map(|details| !details.is_null())
                        .unwrap_or(false);
                    let has_artifacts = !result.artifacts.is_empty();
                    if !has_text && !has_details && !has_artifacts {
                        rep.error(
                            "empty_tool_result",
                            Some(key),
                            seq,
                            Some("tool_result"),
                            "tool_result needs text, details, or artifacts",
                        );
                    }
                    if let Some(details) = &result.details {
                        if details.is_null() {
                            rep.error(
                                "null_tool_details",
                                Some(key),
                                seq,
                                Some("tool_result.details"),
                                "tool details must not be null when present",
                            );
                        }
                    }
                    let mut artifact_ids: HashSet<&str> = HashSet::new();
                    for artifact in &result.artifacts {
                        if artifact.artifact_id.trim().is_empty() {
                            rep.error(
                                "empty_artifact_id",
                                Some(key),
                                seq,
                                Some("tool_result.artifacts"),
                                "artifact_id must be non-empty",
                            );
                        } else if !artifact_ids.insert(artifact.artifact_id.as_str()) {
                            rep.error(
                                "duplicate_artifact_id",
                                Some(key),
                                seq,
                                Some("tool_result.artifacts"),
                                format!("duplicate artifact_id {}", artifact.artifact_id),
                            );
                        }
                        if let Some(sha) = &artifact.sha256 {
                            if !is_hex64(sha) {
                                rep.error(
                                    "bad_artifact_sha256",
                                    Some(key),
                                    seq,
                                    Some("tool_result.artifacts.sha256"),
                                    "artifact sha256 must be 64 hex characters",
                                );
                            }
                        }
                        if artifact.size == Some(0) {
                            rep.error(
                                "bad_artifact_size",
                                Some(key),
                                seq,
                                Some("tool_result.artifacts.size"),
                                "artifact size must be > 0 when present",
                            );
                        }
                        if artifact
                            .mime
                            .as_deref()
                            .map(|value| value.trim().is_empty())
                            .unwrap_or(false)
                        {
                            rep.error(
                                "empty_field",
                                Some(key),
                                seq,
                                Some("tool_result.artifacts.mime"),
                                "artifact mime must be non-empty when present",
                            );
                        }
                        if artifact
                            .path
                            .as_deref()
                            .map(|value| value.trim().is_empty())
                            .unwrap_or(false)
                        {
                            rep.error(
                                "empty_field",
                                Some(key),
                                seq,
                                Some("tool_result.artifacts.path"),
                                "artifact path must be non-empty when present",
                            );
                        }
                        if artifact.path.is_none() && artifact.sha256.is_none() {
                            rep.warn(
                                "artifact_without_locator",
                                Some(key),
                                seq,
                                Some("tool_result.artifacts"),
                                "artifact has neither path nor sha256",
                            );
                        }
                    }
                }
            }
            if event.tool.is_some() || event.doc.is_some() {
                rep.error(
                    "extra_payload",
                    Some(key),
                    seq,
                    None,
                    "tool_result cannot carry tool/doc",
                );
            }
            if event.system_kind.is_some() {
                rep.error(
                    "system_kind_on_non_system",
                    Some(key),
                    seq,
                    Some("system_kind"),
                    "system_kind is only allowed on system events",
                );
            }
        }
        EventKind::DocRef => {
            match &event.doc {
                None => rep.error(
                    "missing_doc",
                    Some(key),
                    seq,
                    Some("doc"),
                    "doc_ref requires doc",
                ),
                Some(doc) => {
                    if doc.doc_id.trim().is_empty() {
                        rep.error(
                            "empty_doc_id",
                            Some(key),
                            seq,
                            Some("doc.doc_id"),
                            "doc_id must be non-empty",
                        );
                    } else if !ctx.docs.contains_key(doc.doc_id.as_str()) {
                        rep.error(
                            "unresolved_doc_ref",
                            Some(key),
                            seq,
                            Some("doc.doc_id"),
                            format!("doc_id {} is not in docs", doc.doc_id),
                        );
                    }
                    validate_doc_span(doc.span.as_ref(), key, seq, rep);
                }
            }
            if event.tool.is_some() || event.tool_result.is_some() {
                rep.error(
                    "extra_payload",
                    Some(key),
                    seq,
                    None,
                    "doc_ref cannot carry tool/tool_result",
                );
            }
            if event.system_kind.is_some() {
                rep.error(
                    "system_kind_on_non_system",
                    Some(key),
                    seq,
                    Some("system_kind"),
                    "system_kind is only allowed on system events",
                );
            }
        }
        EventKind::Attachment => {
            if event.doc.is_none() && event.text.trim().is_empty() {
                rep.error(
                    "empty_attachment",
                    Some(key),
                    seq,
                    None,
                    "attachment needs doc or text",
                );
            }
            if let Some(doc) = &event.doc {
                if doc.doc_id.trim().is_empty() {
                    rep.error(
                        "empty_doc_id",
                        Some(key),
                        seq,
                        Some("doc.doc_id"),
                        "doc_id must be non-empty",
                    );
                } else if !ctx.docs.contains_key(doc.doc_id.as_str()) {
                    rep.error(
                        "unresolved_doc_ref",
                        Some(key),
                        seq,
                        Some("doc.doc_id"),
                        format!("doc_id {} is not in docs", doc.doc_id),
                    );
                }
                validate_doc_span(doc.span.as_ref(), key, seq, rep);
            }
            if event.tool.is_some() || event.tool_result.is_some() {
                rep.error(
                    "extra_payload",
                    Some(key),
                    seq,
                    None,
                    "attachment cannot carry tool/tool_result",
                );
            }
            if event.system_kind.is_some() {
                rep.error(
                    "system_kind_on_non_system",
                    Some(key),
                    seq,
                    Some("system_kind"),
                    "system_kind is only allowed on system events",
                );
            }
        }
    }
    validate_summary(event, ctx, key, rep);
}

fn validate_doc_span(span: Option<&Span>, key: &str, seq: Option<u32>, rep: &mut V3Report) {
    if let Some(span) = span {
        if span.start > span.end {
            rep.error(
                "bad_doc_span",
                Some(key),
                seq,
                Some("doc.span"),
                "span.end must be >= span.start",
            );
        }
        if let Some(hash) = &span.text_sha256 {
            if !is_hex64(hash) {
                rep.error(
                    "bad_span_hash",
                    Some(key),
                    seq,
                    Some("doc.span.text_sha256"),
                    "span text_sha256 must be 64 hex characters",
                );
            }
        }
    }
}

fn validate_summary(event: &V3Event, ctx: &SessionCtx<'_>, key: &str, rep: &mut V3Report) {
    let seq = Some(event.seq);
    if event.kind == EventKind::Summary {
        if !event.summary {
            rep.error(
                "summary_flag_missing",
                Some(key),
                seq,
                Some("summary"),
                "kind=summary requires summary=true",
            );
        }
        if event.covers.is_empty() {
            if has_quality_flag(event, QualityFlag::CoversUnknown) {
                rep.warn(
                    "summary_covers_unknown",
                    Some(key),
                    seq,
                    Some("covers"),
                    "summary has no known covered interval",
                );
            } else {
                rep.error(
                    "summary_covers_missing",
                    Some(key),
                    seq,
                    Some("covers"),
                    "summary requires at least one covered interval",
                );
            }
            return;
        }
        for cover in &event.covers {
            if cover.seq_start > cover.seq_end {
                rep.error(
                    "bad_cover_interval",
                    Some(key),
                    seq,
                    Some("covers"),
                    "covers.seq_start must be <= seq_end",
                );
                continue;
            }
            let thread = match ctx.threads_by_uid.get(cover.thread_uid.as_str()) {
                None => {
                    rep.error(
                        "cover_thread_unknown",
                        Some(key),
                        seq,
                        Some("covers.thread_uid"),
                        "cover thread_uid is not in threads",
                    );
                    continue;
                }
                Some(thread) => *thread,
            };
            for endpoint in [cover.seq_start, cover.seq_end] {
                match ctx.events_by_seq.get(&endpoint) {
                    None => rep.error(
                        "cover_range_missing",
                        Some(key),
                        seq,
                        Some("covers"),
                        "cover endpoint does not exist",
                    ),
                    Some(covered) if covered.thread_uid != thread.thread_uid => rep.error(
                        "cover_thread_mismatch",
                        Some(key),
                        seq,
                        Some("covers"),
                        "cover endpoint belongs to a different thread",
                    ),
                    Some(_) => {}
                }
            }
            if cover.seq_end >= event.seq {
                rep.error(
                    "cover_after_summary",
                    Some(key),
                    seq,
                    Some("covers"),
                    "summary cannot cover itself or later events",
                );
            }
        }
        if has_quality_flag(event, QualityFlag::CoversDerived) {
            rep.warn(
                "summary_covers_derived",
                Some(key),
                seq,
                Some("covers"),
                "summary covered interval was derived from preceding events",
            );
        }
    } else {
        if event.summary {
            rep.error(
                "summary_flag_on_non_summary",
                Some(key),
                seq,
                Some("summary"),
                "summary=true is only allowed on kind=summary",
            );
        }
        if !event.covers.is_empty() {
            rep.error(
                "covers_on_non_summary",
                Some(key),
                seq,
                Some("covers"),
                "covers is only allowed on kind=summary",
            );
        }
    }
}

fn validate_event_refs(
    event: &V3Event,
    ctx: &SessionCtx<'_>,
    calls: &HashMap<String, u32>,
    machine_id: &str,
    key: &str,
    rep: &mut V3Report,
) {
    let seq = Some(event.seq);
    let mut seen: HashSet<(RefKind, &str)> = HashSet::new();
    for reference in &event.refs {
        if reference.id.trim().is_empty() {
            rep.error(
                "empty_ref_id",
                Some(key),
                seq,
                Some("refs"),
                "ref id must be non-empty",
            );
            continue;
        }
        if !seen.insert((reference.kind, reference.id.as_str())) {
            rep.error(
                "duplicate_ref",
                Some(key),
                seq,
                Some("refs"),
                format!("duplicate ref {}:{}", ref_kind_name(reference.kind), reference.id),
            );
        }
        match reference.kind {
            RefKind::Doc => {
                if !ctx.docs.contains_key(reference.id.as_str()) {
                    rep.error(
                        "unresolved_doc_ref",
                        Some(key),
                        seq,
                        Some("refs"),
                        "doc ref does not resolve in docs",
                    );
                }
                if reference.alias.is_some() || reference.version.is_some() {
                    rep.error(
                        "unexpected_ref_field",
                        Some(key),
                        seq,
                        Some("refs"),
                        "doc ref cannot carry alias/version",
                    );
                }
                validate_doc_span(reference.span.as_ref(), key, seq, rep);
            }
            RefKind::Secret => {
                if reference.span.is_some() {
                    rep.error(
                        "unexpected_ref_span",
                        Some(key),
                        seq,
                        Some("refs"),
                        "secret ref cannot carry span",
                    );
                }
                match ctx.secrets.get(reference.id.as_str()) {
                    None => rep.error(
                        "unresolved_secret_ref",
                        Some(key),
                        seq,
                        Some("refs"),
                        "secret ref does not resolve in secrets",
                    ),
                    Some(secret) => {
                        if let Some(alias) = &reference.alias {
                            if alias != &secret.alias {
                                rep.error(
                                    "secret_alias_mismatch",
                                    Some(key),
                                    seq,
                                    Some("refs.alias"),
                                    "secret ref alias does not match the secret entry",
                                );
                            }
                        }
                        if let Some(version) = reference.version {
                            if version > secret.version {
                                rep.error(
                                    "secret_version_mismatch",
                                    Some(key),
                                    seq,
                                    Some("refs.version"),
                                    "secret ref version is newer than the secret entry",
                                );
                            } else if version < secret.version {
                                rep.warn(
                                    "secret_version_historical",
                                    Some(key),
                                    seq,
                                    Some("refs.version"),
                                    "secret ref points at an older version",
                                );
                            }
                        }
                        if secret.status == SecretStatus::Revoked {
                            rep.error(
                                "revoked_secret_ref",
                                Some(key),
                                seq,
                                Some("refs"),
                                "secret ref points at a revoked secret",
                            );
                        } else if secret.status == SecretStatus::Rotated {
                            rep.warn(
                                "rotated_secret_ref",
                                Some(key),
                                seq,
                                Some("refs"),
                                "secret ref points at a rotated secret",
                            );
                        }
                    }
                }
            }
            RefKind::Agent => {
                if reference.alias.is_some()
                    || reference.version.is_some()
                    || reference.span.is_some()
                {
                    rep.error(
                        "unexpected_ref_field",
                        Some(key),
                        seq,
                        Some("refs"),
                        "agent ref cannot carry alias/version/span",
                    );
                }
                if !ctx.roster_agents.contains_key(reference.id.as_str()) {
                    rep.error(
                        "unresolved_agent_ref",
                        Some(key),
                        seq,
                        Some("refs"),
                        "agent ref does not resolve in roster.agents",
                    );
                }
            }
            RefKind::Tool => {
                if reference.alias.is_some()
                    || reference.version.is_some()
                    || reference.span.is_some()
                {
                    rep.error(
                        "unexpected_ref_field",
                        Some(key),
                        seq,
                        Some("refs"),
                        "tool ref cannot carry alias/version/span",
                    );
                }
                if !ctx.roster_tools.contains(reference.id.as_str()) {
                    match calls.get(reference.id.as_str()) {
                        Some(call_seq) if *call_seq >= event.seq => rep.error(
                            "future_tool_ref",
                            Some(key),
                            seq,
                            Some("refs"),
                            "tool ref points at a later call",
                        ),
                        Some(_) => {}
                        None => rep.error(
                            "unresolved_tool_ref",
                            Some(key),
                            seq,
                            Some("refs"),
                            "tool ref is neither a roster tool nor an earlier call",
                        ),
                    }
                }
            }
            RefKind::Machine => {
                if reference.alias.is_some()
                    || reference.version.is_some()
                    || reference.span.is_some()
                {
                    rep.error(
                        "unexpected_ref_field",
                        Some(key),
                        seq,
                        Some("refs"),
                        "machine ref cannot carry alias/version/span",
                    );
                }
                if reference.id.as_str() != machine_id {
                    rep.error(
                        "unresolved_machine_ref",
                        Some(key),
                        seq,
                        Some("refs"),
                        "machine ref does not match the session machine_id",
                    );
                }
            }
        }
    }
}

fn ref_kind_name(kind: RefKind) -> &'static str {
    match kind {
        RefKind::Doc => "doc",
        RefKind::Secret => "secret",
        RefKind::Agent => "agent",
        RefKind::Tool => "tool",
        RefKind::Machine => "machine",
    }
}

fn validate_event_quality(
    event: &V3Event,
    key: &str,
    rep: &mut V3Report,
) -> Option<QuarantineRecord> {
    let quality = event.quality.as_ref()?;
    let seq = Some(event.seq);
    let mut seen: HashSet<QualityFlag> = HashSet::new();
    for flag in &quality.flags {
        if !seen.insert(*flag) {
            rep.error(
                "duplicate_quality_flag",
                Some(key),
                seq,
                Some("quality.flags"),
                "quality flags must be unique",
            );
        }
        let name = flag.as_str().to_string();
        *rep.stats.quality_flags.entry(name).or_insert(0) += 1;
    }
    if quality.flags.is_empty() && !quality.quarantine {
        rep.warn(
            "empty_quality_block",
            Some(key),
            seq,
            Some("quality"),
            "quality block has no flags and no quarantine",
        );
    }
    if quality.quarantine && quality.flags.is_empty() {
        rep.error(
            "quarantine_flags_required",
            Some(key),
            seq,
            Some("quality.flags"),
            "quarantine requires at least one quality flag",
        );
    }
    if quality
        .reason
        .as_deref()
        .map(|reason| reason.trim().is_empty())
        .unwrap_or(quality.quarantine)
    {
        rep.error(
            "quarantine_reason_required",
            Some(key),
            seq,
            Some("quality.reason"),
            "quarantined events require a non-empty reason",
        );
    }
    if quality
        .flags
        .iter()
        .any(|flag| flag.is_hard_quality_failure())
        && !quality.quarantine
    {
        rep.error(
            "hard_flag_not_quarantined",
            Some(key),
            seq,
            Some("quality.quarantine"),
            "truncated/malformed/encoding-loss events must be quarantined",
        );
    }
    if quality.quarantine {
        rep.stats.quarantined_events += 1;
        Some(QuarantineRecord {
            session: key.to_string(),
            seq: event.seq,
            event_uid: event.event_uid.clone(),
            flags: quality.flags.clone(),
            reason_present: quality
                .reason
                .as_deref()
                .map(|reason| !reason.trim().is_empty())
                .unwrap_or(false),
        })
    } else {
        None
    }
}

fn scan_event(event: &V3Event, key: &str, rep: &mut V3Report) {
    let mut pairs: Vec<(&'static str, String)> = vec![("text", event.text.clone())];
    if let Some(tool) = &event.tool {
        pairs.push(("tool.arguments", tool.arguments.to_string()));
    }
    if let Some(result) = &event.tool_result {
        if let Some(text) = &result.text {
            pairs.push(("tool_result.text", text.clone()));
        }
        if let Some(details) = &result.details {
            pairs.push(("tool_result.details", details.to_string()));
        }
        for artifact in &result.artifacts {
            pairs.push(("artifact.path", artifact.path.clone().unwrap_or_default()));
        }
    }
    for reference in &event.refs {
        if let Some(alias) = &reference.alias {
            pairs.push(("ref.alias", alias.clone()));
        }
    }
    if let Some(quality) = &event.quality {
        if let Some(reason) = &quality.reason {
            pairs.push(("quality.reason", reason.clone()));
        }
    }
    scan_pairs(pairs, key, Some(event.seq), rep);
}

fn scan_session(session: &V3Session, key: &str, rep: &mut V3Report) {
    let mut pairs: Vec<(&'static str, String)> = Vec::new();
    for (field, value) in [
        ("environment.machine", &session.environment.machine),
        ("environment.hostname", &session.environment.hostname),
        ("environment.os", &session.environment.os),
        ("environment.cwd", &session.environment.cwd),
        ("environment.git_branch", &session.environment.git_branch),
        ("environment.locale", &session.environment.locale),
        ("environment.timezone", &session.environment.timezone),
        ("ownership.principal_id", &session.ownership.principal_id),
    ] {
        if let Some(value) = value {
            pairs.push((field, value.clone()));
        }
    }
    pairs.push(("ownership.scope.key", session.ownership.scope.key.clone()));
    for agent in &session.roster.agents {
        if let Some(name) = &agent.name {
            pairs.push(("roster.agent.name", name.clone()));
        }
        if let Some(model) = &agent.model {
            pairs.push(("roster.agent.model", model.clone()));
        }
    }
    for tool in &session.roster.tools {
        if let Some(version) = &tool.version {
            pairs.push(("roster.tool.version", version.clone()));
        }
        if let Some(definition_ref) = &tool.definition_ref {
            pairs.push(("roster.tool.definition_ref", definition_ref.clone()));
        }
    }
    for doc in &session.docs {
        pairs.push(("docs.path", doc.path.clone()));
        if let Some(title) = &doc.title {
            pairs.push(("docs.title", title.clone()));
        }
    }
    for secret in &session.secrets {
        pairs.push(("secrets.alias", secret.alias.clone()));
    }
    scan_pairs(pairs, key, None, rep);
}

fn validate_session_quality(session: &V3Session, key: &str, rep: &mut V3Report) {
    let mut flags: HashSet<QualityFlag> = HashSet::new();
    for flag in &session.quality.flags {
        if !flags.insert(*flag) {
            rep.error(
                "duplicate_session_quality_flag",
                Some(key),
                None,
                Some("quality.flags"),
                "session quality flags must be unique",
            );
        }
    }
    let quarantined = session
        .events
        .iter()
        .filter(|event| is_quarantined(event))
        .count();
    if session.quality.quarantined_events as usize != quarantined {
        rep.warn(
            "quarantined_count_mismatch",
            Some(key),
            None,
            Some("quality.quarantined_events"),
            format!(
                "session reports {} quarantined events but {} events are quarantined",
                session.quality.quarantined_events, quarantined
            ),
        );
    }
    for (reason, count) in &session.dropped.other {
        if !is_reason_key(reason) {
            rep.error(
                "bad_dropped_key",
                Some(key),
                None,
                Some("dropped.other"),
                "dropped reason keys must be ASCII tokens",
            );
        }
        if *count == 0 {
            rep.warn(
                "zero_dropped_count",
                Some(key),
                None,
                Some("dropped.other"),
                format!("dropped reason {reason} has a zero count"),
            );
        }
    }
    rep.stats.dropped_events += session.dropped.total();
    scan_session(session, key, rep);
}

fn check_file_time(value: i64, field: &'static str, rep: &mut V3Report) {
    if !(TS_MIN..=TS_MAX).contains(&value) {
        rep.error(
            "bad_timestamp",
            None,
            None,
            Some(field),
            format!("{field} is outside 2000..2100"),
        );
    }
}

fn validate_policy(policy: &Policy, rep: &mut V3Report) {
    if !policy.default_deny {
        rep.error(
            "policy_default_allow",
            None,
            None,
            Some("policy.default_deny"),
            "policy must default to deny",
        );
    }
    if !policy.require_grant_for_cross_agent {
        rep.error(
            "policy_grant_not_required",
            None,
            None,
            Some("policy.require_grant_for_cross_agent"),
            "cross-agent access must require an explicit grant",
        );
    }
    let mut points: HashSet<EnforcementPoint> = HashSet::new();
    for point in &policy.enforcement_points {
        if !points.insert(*point) {
            rep.error(
                "duplicate_enforcement_point",
                None,
                None,
                Some("policy.enforcement_points"),
                "enforcement points must be unique",
            );
        }
    }
    for required in [
        EnforcementPoint::Hub,
        EnforcementPoint::Mcp,
        EnforcementPoint::Expand,
        EnforcementPoint::Export,
        EnforcementPoint::Search,
    ] {
        if !points.contains(&required) {
            rep.error(
                "missing_enforcement_point",
                None,
                None,
                Some("policy.enforcement_points"),
                format!("required enforcement point {required:?} is missing"),
            );
        }
    }

    let mut principals: HashSet<&str> = HashSet::new();
    for principal in &policy.service_principals {
        if principal.id.trim().is_empty() {
            rep.error(
                "empty_service_principal",
                None,
                None,
                Some("policy.service_principals"),
                "service principal id must be non-empty",
            );
            continue;
        }
        if !is_safe_token(&principal.id) {
            rep.error(
                "bad_service_principal",
                None,
                None,
                Some("policy.service_principals"),
                "service principal id is not a safe token",
            );
        }
        if !principals.insert(principal.id.as_str()) {
            rep.error(
                "duplicate_service_principal",
                None,
                None,
                Some("policy.service_principals"),
                format!("duplicate service principal {}", principal.id),
            );
        }
        if principal.capabilities.is_empty() {
            rep.error(
                "empty_capabilities",
                None,
                None,
                Some("policy.service_principals.capabilities"),
                "service principal needs at least one capability",
            );
        }
        let mut caps: HashSet<Capability> = HashSet::new();
        for capability in &principal.capabilities {
            if !caps.insert(*capability) {
                rep.error(
                    "duplicate_capability",
                    None,
                    None,
                    Some("policy.service_principals.capabilities"),
                    "service principal capabilities must be unique",
                );
            }
        }
        match principal.kind {
            ServicePrincipalKind::Backup => {
                if !caps.contains(&Capability::BackupRead)
                    || !caps.contains(&Capability::BackupWrite)
                {
                    rep.error(
                        "backup_capabilities",
                        None,
                        None,
                        Some("policy.service_principals.capabilities"),
                        "backup principal requires backup_read and backup_write",
                    );
                }
                if caps.contains(&Capability::UseSecret) {
                    rep.error(
                        "secret_use_not_allowed",
                        None,
                        None,
                        Some("policy.service_principals.capabilities"),
                        "backup principal cannot use secrets",
                    );
                }
            }
            ServicePrincipalKind::Extractor => {
                if !caps.contains(&Capability::ReadTranscript)
                    || (!caps.contains(&Capability::WriteMemory)
                        && !caps.contains(&Capability::Derive))
                {
                    rep.error(
                        "extractor_capabilities",
                        None,
                        None,
                        Some("policy.service_principals.capabilities"),
                        "extractor principal requires read_transcript and derive/write_memory",
                    );
                }
            }
            ServicePrincipalKind::Consumer => {
                if caps.contains(&Capability::Admin) {
                    rep.error(
                        "consumer_admin",
                        None,
                        None,
                        Some("policy.service_principals.capabilities"),
                        "consumer principal cannot hold admin",
                    );
                }
            }
            ServicePrincipalKind::Operator | ServicePrincipalKind::Center => {
                if caps.contains(&Capability::UseSecret) {
                    rep.error(
                        "secret_use_not_allowed",
                        None,
                        None,
                        Some("policy.service_principals.capabilities"),
                        "operator/center principal cannot use secrets directly",
                    );
                }
            }
        }
    }
}

fn validate_key_manifest(manifest: &KeyManifest, rep: &mut V3Report) {
    if manifest.envelope_version == 0 {
        rep.error(
            "bad_envelope_version",
            None,
            None,
            Some("key_manifest.envelope_version"),
            "envelope_version must be >= 1",
        );
    }
    if manifest.keys.is_empty() {
        rep.error(
            "empty_key_manifest",
            None,
            None,
            Some("key_manifest.keys"),
            "key manifest needs at least one key",
        );
    }
    let mut ids: HashSet<&str> = HashSet::new();
    let mut versions: HashMap<(KeyKind, Option<String>), HashSet<u32>> = HashMap::new();
    let mut active: HashMap<(KeyKind, Option<String>), usize> = HashMap::new();
    let mut backup_ids: HashSet<&str> = HashSet::new();
    let mut backup_fingerprints: HashSet<&str> = HashSet::new();
    let mut other_fingerprints: HashSet<&str> = HashSet::new();

    for key in &manifest.keys {
        if key.key_id.trim().is_empty() {
            rep.error(
                "empty_key_id",
                None,
                None,
                Some("key_manifest.keys"),
                "key_id must be non-empty",
            );
            continue;
        }
        if !is_safe_token(&key.key_id) {
            rep.error(
                "bad_key_id",
                None,
                None,
                Some("key_manifest.keys"),
                "key_id is not a safe token",
            );
        }
        if !ids.insert(key.key_id.as_str()) {
            rep.error(
                "duplicate_key_id",
                None,
                None,
                Some("key_manifest.keys"),
                format!("duplicate key_id {}", key.key_id),
            );
        }
        if key.version == 0 {
            rep.error(
                "bad_key_version",
                None,
                None,
                Some("key_manifest.keys.version"),
                "key version must be >= 1",
            );
        }
        if !is_fingerprint8(&key.fingerprint) {
            rep.error(
                "bad_key_fingerprint",
                None,
                None,
                Some("key_manifest.keys.fingerprint"),
                "key fingerprint must be 8 lowercase hex characters",
            );
        }
        if let Some(owner) = &key.owner_agent_id {
            if owner.trim().is_empty() || !is_safe_token(owner) {
                rep.error(
                    "bad_key_owner",
                    None,
                    None,
                    Some("key_manifest.keys.owner_agent_id"),
                    "key owner_agent_id must be a non-empty safe token",
                );
            }
        }
        match key.kind {
            KeyKind::Agent | KeyKind::Core | KeyKind::Secret => {
                if key.owner_agent_id.is_none() {
                    rep.error(
                        "key_owner_missing",
                        None,
                        None,
                        Some("key_manifest.keys.owner_agent_id"),
                        "agent/core/secret keys require owner_agent_id",
                    );
                }
            }
            KeyKind::Hub | KeyKind::Device | KeyKind::Backup => {}
        }
        check_file_time(key.created_at_ms, "key_manifest.keys.created_at_ms", rep);
        if let Some(rotated) = key.rotated_at_ms {
            check_file_time(rotated, "key_manifest.keys.rotated_at_ms", rep);
            if rotated < key.created_at_ms {
                rep.error(
                    "key_rotated_before_created",
                    None,
                    None,
                    Some("key_manifest.keys.rotated_at_ms"),
                    "rotated_at_ms must not be before created_at_ms",
                );
            }
        }
        match key.status {
            KeyStatus::Active => {
                if key.rotated_at_ms.is_some() {
                    rep.error(
                        "active_key_rotated",
                        None,
                        None,
                        Some("key_manifest.keys.rotated_at_ms"),
                        "an active key cannot already have rotated_at_ms",
                    );
                }
            }
            KeyStatus::Retired | KeyStatus::Revoked | KeyStatus::Destroyed => {
                if key.rotated_at_ms.is_none() {
                    rep.error(
                        "retired_key_without_rotation",
                        None,
                        None,
                        Some("key_manifest.keys.rotated_at_ms"),
                        "retired/revoked/destroyed keys require rotated_at_ms",
                    );
                }
            }
        }
        let owner = key.owner_agent_id.clone();
        if !versions
            .entry((key.kind, owner.clone()))
            .or_default()
            .insert(key.version)
        {
            rep.error(
                "duplicate_key_version",
                None,
                None,
                Some("key_manifest.keys.version"),
                "duplicate key version for the same kind/owner",
            );
        }
        if key.status == KeyStatus::Active {
            let count = active.entry((key.kind, owner)).or_insert(0);
            *count += 1;
            if *count > 1 {
                rep.error(
                    "multiple_active_keys",
                    None,
                    None,
                    Some("key_manifest.keys.status"),
                    "at most one active key is allowed per kind/owner",
                );
            }
        }
        if key.kind == KeyKind::Backup {
            backup_ids.insert(key.key_id.as_str());
            if !matches!(key.custody, KeyCustody::Offline | KeyCustody::Kms) {
                rep.error(
                    "backup_key_custody",
                    None,
                    None,
                    Some("key_manifest.keys.custody"),
                    "backup keys must use offline or KMS custody",
                );
            }
            backup_fingerprints.insert(key.fingerprint.as_str());
        } else {
            other_fingerprints.insert(key.fingerprint.as_str());
        }
    }

    if !manifest.backup.separate_channel {
        rep.error(
            "backup_key_not_separate",
            None,
            None,
            Some("key_manifest.backup.separate_channel"),
            "backup keys must travel on a separate channel",
        );
    }
    if !manifest.backup.recovery_required {
        rep.error(
            "backup_recovery_not_required",
            None,
            None,
            Some("key_manifest.backup.recovery_required"),
            "backup recovery must be required",
        );
    }
    if manifest.backup.key_ids.is_empty() {
        rep.error(
            "backup_key_missing",
            None,
            None,
            Some("key_manifest.backup.key_ids"),
            "backup policy needs at least one backup key",
        );
    }
    let mut listed: HashSet<&str> = HashSet::new();
    for key_id in &manifest.backup.key_ids {
        if !listed.insert(key_id.as_str()) {
            rep.error(
                "duplicate_backup_key",
                None,
                None,
                Some("key_manifest.backup.key_ids"),
                "backup key_ids must be unique",
            );
        }
        if !backup_ids.contains(key_id.as_str()) {
            rep.error(
                "backup_key_unknown",
                None,
                None,
                Some("key_manifest.backup.key_ids"),
                "backup key_ids must reference keys with kind=backup",
            );
        }
    }
    for key_id in &backup_ids {
        if !listed.contains(key_id) {
            rep.error(
                "backup_key_unlisted",
                None,
                None,
                Some("key_manifest.backup.key_ids"),
                "every backup key must be listed in backup.key_ids",
            );
        }
    }
    for fingerprint in &backup_fingerprints {
        if other_fingerprints.contains(fingerprint) {
            rep.error(
                "backup_key_fingerprint_shared",
                None,
                None,
                Some("key_manifest.keys.fingerprint"),
                "backup keys must not share a fingerprint with non-backup keys",
            );
        }
    }
}

fn validate_replication(replication: &ReplicationStamp, rep: &mut V3Report) {
    if replication.epoch == 0 {
        rep.error(
            "replication_epoch",
            None,
            None,
            Some("replication.epoch"),
            "epoch must be >= 1",
        );
    }
    if replication.commit_seq == 0 {
        rep.error(
            "replication_commit_seq",
            None,
            None,
            Some("replication.commit_seq"),
            "commit_seq must be >= 1",
        );
    }
    if replication.prev_commit_seq >= replication.commit_seq {
        rep.error(
            "commit_seq_not_monotonic",
            None,
            None,
            Some("replication.prev_commit_seq"),
            "prev_commit_seq must be < commit_seq",
        );
    }
    if replication.center_id.trim().is_empty() || !is_safe_token(&replication.center_id) {
        rep.error(
            "bad_center_id",
            None,
            None,
            Some("replication.center_id"),
            "center_id must be a non-empty safe token",
        );
    }
    if replication.fencing_token != replication.epoch {
        rep.error(
            "fencing_mismatch",
            None,
            None,
            Some("replication.fencing_token"),
            "fencing_token must equal epoch",
        );
    }
    if !is_hex64(&replication.signed_manifest_sha256) {
        rep.error(
            "bad_signed_manifest",
            None,
            None,
            Some("replication.signed_manifest_sha256"),
            "signed_manifest_sha256 must be 64 hex characters",
        );
    }
    if let Some(cert) = &replication.promotion_cert {
        if !cert.signed_by_prev_center {
            rep.error(
                "promotion_not_signed",
                None,
                None,
                Some("replication.promotion_cert.signed_by_prev_center"),
                "promotion certificate must be signed by the previous center",
            );
        }
        if cert.new_center_id != replication.center_id {
            rep.error(
                "promotion_center_mismatch",
                None,
                None,
                Some("replication.promotion_cert.new_center_id"),
                "promotion certificate names a different center",
            );
        }
        if cert.new_epoch != replication.epoch {
            rep.error(
                "promotion_epoch_mismatch",
                None,
                None,
                Some("replication.promotion_cert.new_epoch"),
                "promotion new_epoch must equal replication.epoch",
            );
        }
        if cert.prev_epoch.checked_add(1) != Some(cert.new_epoch) {
            rep.error(
                "promotion_epoch_chain",
                None,
                None,
                Some("replication.promotion_cert.prev_epoch"),
                "promotion must advance the epoch by exactly one",
            );
        }
        if !is_fingerprint8(&cert.new_pubkey_fingerprint) {
            rep.error(
                "bad_promotion_fingerprint",
                None,
                None,
                Some("replication.promotion_cert.new_pubkey_fingerprint"),
                "new center fingerprint must be 8 lowercase hex characters",
            );
        }
        if !is_signature_shape(&cert.signature) {
            rep.error(
                "bad_promotion_signature",
                None,
                None,
                Some("replication.promotion_cert.signature"),
                "promotion signature must be a non-empty signature-shaped token",
            );
        }
    }
}




