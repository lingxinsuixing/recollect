//! 核心陪伴层（`core.db`）的加密载荷、密钥文件与 per-agent 动态表。
//!
//! 分工照文件分：`payload.rs` 是 §9.2 的逻辑结构（演进只发生在密文内，物理表形状固定），
//! `crypto.rs` 是每 agent 独立 key、XChaCha20-Poly1305 与 AAD，`store.rs` 是 `core.db`
//! 的注册表、`core_doc_<h>`/`core_rev_<h>`/`core_note_<h>`、访问日志与注入日志，`render.rs` 是 §9.1/§9.3
//! 的 20K 常驻简报（固定分区顺序、区上限与确定性淘汰），`ops.rs` 是 §9.4/§10.3 的
//! core 工具操作层（人格提案、主用户、交互、情绪、未闭环与 pinned 经历）。
//!
//! 信任边界照设计 §10.8：同一设备上的 agent 属于同一 principal。这里做的是**防误用/防外泄**：
//! 载荷 AEAD 让没带 key 的文件副本打不开；进程只加载 caller 自己的 key；原始 `agent_id`
//! 从不拼进 SQL（表名只由 `sha256(agent_id)` 的前 16 hex 派生）。不做 token、L2 与 SQLCipher。

pub mod crypto;
pub mod ops;
pub mod payload;
pub mod render;
pub mod store;

pub use crypto::{
    aad, agent_table_suffix, is_valid_core_table, key_path, load_key, load_or_create_key, open,
    seal, table_names, AgentKey, KEY_LEN, NONCE_LEN,
};
pub use ops::{
    apply_patch, doc_search, proposal_id, DocSearchHit, HandoffClose, HandoffWrite,
    InteractionTouch, NotebookAppend, NotebookEntry, NotebookList, PersonaPatch, PersonaProposal,
    AFFECT_KINDS, LOOP_DONE, LOOP_DROPPED, LOOP_OPEN, PROPOSAL_APPLIED, PROPOSAL_PROPOSED,
    PROPOSAL_REJECTED, SUBJECT_LIMIT,
};
pub use payload::{
    Affect, ChineseExpression, CorePayload, HandoffItem, Interaction, LoopItem, Persona,
    PinnedEpisode, Relationship, Subject, CHINESE_EXPRESSION_RULES, HANDOFF_CLOSE_REASONS,
    HANDOFF_KINDS, HANDOFF_OPEN_LIMIT, HANDOFF_TEXT_MAX, HANDOFF_TTL_MS, HANDOFF_TOTAL_LIMIT,
    SCHEMA_VERSION,
};
pub use render::{
    estimate_tokens, render_brief, render_brief_with, render_brief_with_notebook, render_delta,
    render_due, render_handoff_prev, Brief, HandoffPrevSection, CAP_CHINESE_EXPRESSION,
    CAP_HANDOFF, CAP_HANDOFF_PREV, DEFAULT_BUDGET, DEFAULT_INCLUDE, SEC_AFFECT,
    SEC_CHINESE_EXPRESSION, SEC_HANDOFF, SEC_HANDOFF_PREV, SEC_L0, SEC_LOOPS, SEC_NOTEBOOK,
    SEC_PERSONA, SEC_PINNED, SEC_RECENT, SEC_RELATIONSHIP, SEC_SUBJECT,
};
pub use store::{AgentRecord, CoreStore};

/// core 层错误。载荷内容不进错误文本：这里只出现表名、文件名与结构性问题。
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite: {0}")]
    Sql(String),
    /// AEAD 失败不区分"错 key"与"被篡改"——两者都只说明这段密文不该被这条路径打开。
    #[error("crypto: the payload could not be sealed or opened")]
    Crypto,
    #[error("agent id is not a safe token")]
    BadAgentId,
    #[error("table name is not a core_doc_/core_rev_ table")]
    BadTableName,
    #[error("the key file is not exactly 32 bytes")]
    BadKeyFile,
    /// 读写路径只加载已存在的 key：缺失时绝不能静默生成新 key（旧密文会永久不可读）。
    #[error(
        "key file missing; restore it or re-register the agent after archiving the old core.db"
    )]
    KeyMissing,
    #[error("agent is not registered")]
    Unregistered,
    #[error("no such registration")]
    NotFound,
    /// 未关闭的交接条已达上限。载荷只有条数：正文不进错误文本。
    #[error("handoff is full: {0} open items")]
    HandoffFull(usize),
    #[error("payload: {0}")]
    BadPayload(String),
}

impl From<rusqlite::Error> for CoreError {
    fn from(e: rusqlite::Error) -> Self {
        CoreError::Sql(e.to_string())
    }
}
