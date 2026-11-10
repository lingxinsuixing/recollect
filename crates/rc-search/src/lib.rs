//! 检索层：把"命中"变成"有全序的候选"（`score` + `rank`），再由 §8 的闸门决定开不开口（`gate`）。
//!
//! 排序那一半只做一件事：把"相关性"变成一个**跨架构逐位可复现的整数**，并给它一个**全序**，
//! 候选池里一个开关都不判。闸门那一半（任务 6 已住进来）判 `Switches` 的**六个字段**、
//! 其中**五个沉默开关的合取**（`attested` 只驱动阶梯，V10）与 `decide` 的**五段格序**；
//! 错误分型在任务 7。分开写是因为这两半各有一套测试，
//! 混在一起时"排序漂了"和"闸门松了"会互相掩护。
//!
//! 这里**没有**分数落库的任何通路：`Scored.score` 只活在内存里（判据 10c 断的那件事）。

pub mod gate;
pub mod rank;
pub mod score;
pub mod transcript;

pub use gate::{
    cells_of, decide, fit, probe_view, Cells, Decision, Form, GateCell, ProbeView, Switches,
    MAX_FORM_LINES,
};
pub use rank::{rank, rank_counted, ReuseSignal, Scored, SearchError};
pub use score::{score_row, FLOOR};
// W38a 刀 A：转录检索面（turn_shadow FTS + 会话归属读闸）。
pub use transcript::{
    search_turns, session_turns, visible_sessions, Coverage, SessionReplay, SessionSummary,
    SessionTurn, TranscriptSearchError, TurnHit, TurnSearch,
};
