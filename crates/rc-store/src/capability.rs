use crate::{LedgerHandle, MemoryHandle, StoreError, TranscriptHandle};
use rusqlite::Connection;
use serde::Serialize;

/// 架构 §12：改分词/归一 analyzer 必须 +1（⇒ 全量重建影子）。
pub const NORM_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct CapReport {
    pub schema_version: u32,
    pub arch: String,
    pub os: String,
    pub target: String,
    pub sqlite_version: String,
    pub fts5: bool,
    pub cjk_analyzer: String,
    pub norm_version: u32,
    pub transcript_schema: i32,
    pub ledger_schema: i32,
    pub memory_schema: Option<i32>,
}

impl CapReport {
    pub fn schema_version() -> u32 {
        1
    }
}

/// 只读探测：不建表、不改库，信任编译期选项而不是运行时行为。
/// 运行时那一半的证据在 `tests\capability.rs` 的真分词用例。
pub fn fts5_available(conn: &Connection) -> bool {
    let mut st = conn.prepare("PRAGMA compile_options").unwrap();
    let has = st
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(|x| x.ok())
        // PRAGMA compile_options 去掉 SQLITE_ 前缀：FTS5 在这里报成 "ENABLE_FTS5"。
        .any(|o| o == "ENABLE_FTS5");
    has
}

pub fn analyzer_id() -> String {
    "jieba-rs".to_string()
}

pub fn cjk_tokens(text: &str) -> Vec<String> {
    let jieba = jieba_rs::Jieba::new();
    jieba
        .cut(text, false)
        .into_iter()
        // jieba-rs 0.11 的 cut 返回 Token 结构，词面在 .word 上。
        .map(|s| s.word.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn probe(t: &TranscriptHandle, l: &LedgerHandle, m: &MemoryHandle) -> Result<CapReport, StoreError> {
    let conn = t.conn();
    let sqlite_version = rusqlite::version().to_string();
    Ok(CapReport {
        schema_version: CapReport::schema_version(),
        arch: std::env::consts::ARCH.to_string(),
        os: std::env::consts::OS.to_string(),
        // 构建时的 target 由 build.rs 写进 env。
        target: option_env!("TARGET").unwrap_or("unknown").to_string(),
        fts5: fts5_available(conn),
        cjk_analyzer: analyzer_id(),
        norm_version: NORM_VERSION,
        sqlite_version,
        transcript_schema: crate::ddl::schema_version(conn)?,
        ledger_schema: crate::ddl::schema_version(l.conn())?,
        // Option 保留：中心库报 Some(版本)；当前唯一构造路径恒 Some，None 是给将来的
        // 副本半装状态预留（架构 §6）。
        memory_schema: Some(crate::ddl::schema_version(m.conn())?),
    })
}
