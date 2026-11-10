// 见阶段 0 计划：本 crate 的住户从 Task 2 起逐个填。
pub mod counters;
pub mod device;
pub mod import_log;
pub mod invite;
pub mod llm_state;
pub mod pass_state;
pub mod review_mark;
pub mod transcript;
pub mod water;

use rc_store::{LedgerHandle, TranscriptHandle};

pub use import_log::log_import;
pub use transcript::{insert_batch, rebuild_turn_shadow, InsertOutcome};

#[doc(hidden)]
pub fn count_turns_on(h: &TranscriptHandle) -> i64 {
    h.conn().query_row("SELECT count(*) FROM turns", [], |r| r.get(0)).unwrap()
}
#[doc(hidden)]
pub fn count_sessions(h: &TranscriptHandle) -> i64 {
    h.conn().query_row("SELECT count(*) FROM sessions", [], |r| r.get(0)).unwrap()
}
#[doc(hidden)]
pub fn count_import_rows(h: &LedgerHandle) -> i64 {
    h.conn().query_row("SELECT count(*) FROM import_log", [], |r| r.get(0)).unwrap()
}
