// 见阶段 0 计划：本 crate 的住户从 Task 2 起逐个填。
pub mod format;
pub mod identity;
pub mod identity_v3;
pub mod ingest_v2;
pub mod ingest_v3;
pub mod ingest_v3_migrate;
pub mod ingest_v3_project;
pub mod redact;
pub mod text;
pub mod validate;

pub use format::{ExchangeFile, RawSession, RawTurn, TurnTier, SUPPORTED_FORMAT_VERSIONS};
pub use identity_v3::{
    char_count, char_slice, compute_content_hash, compute_event_uid, compute_revision_id,
    compute_session_uid, compute_thread_uid, compute_turn_uid_from_anchor, compute_turn_uid_native,
    normalize_text_v3, text_sha256,
};
pub use ingest_v2::{
    validate_ingest, IngestEvent, IngestFile, IngestIssue, IngestReport, IngestSession, IngestStats,
    INGEST_FORMAT_VERSION,
};
pub use ingest_v3::*;
pub use ingest_v3_migrate::{
    migrate_v2_to_v3, LegacyPointerV2, LegacyResolution, MigrateOptions, MigrationIssue,
};
pub use ingest_v3_project::{project_v3_to_v1, V3Projection};
pub use identity::{compute_body_fp, compute_turn_uid, normalize_body};
pub use redact::{
    redact_text, scan as scan_credentials, CredentialKind, CredentialSpan, RedactedText,
    RedactionEntry, REDACTED_CREDENTIAL,
};
pub use text::{
    estimate_tokens, format_cn_date, format_cn_ms, format_local_date, format_local_ms, render_delta,
};
pub use validate::{
    assign_tier, is_alias_in_source, is_valid_anchor_kind, is_valid_entity_kind,
    is_valid_principal_source, normalize, normalize_with, parse, safe_token, AcceptedTurn,
    BatchReport, ANCHOR_KINDS, DISCOURSE_ROLES, ENTITY_KINDS, Locator, MENTION_RESOLUTIONS,
    NormalizedTurn, PRINCIPAL_SOURCES, REALITY_STATUSES, Reject, RejectCode, SessionId,
};
