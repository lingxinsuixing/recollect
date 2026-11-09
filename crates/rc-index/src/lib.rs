//! 索引层：记忆库的派生物。三条可测的性质——可整层丢弃、可幂等重建、重建不动任何一行记忆。
//!
//! 这一层只写 `rc_shadow` 与 `rc_index_marker`（V2 只握记忆句柄，永远不碰 `transcript.db`
//! 与 `ledger.db`，V6/守卫 3 的落点也在依赖清单上：这里没有 `rc-ledger`、`rc-contract`、`rc-search`）。
//! 表名住在 `pub(crate) mod shadow` 里，对外只露一个读数包装 [`shadow_tables`]（守卫 2）。

pub mod analyzer;
pub mod rebuild;
pub(crate) mod shadow;
pub use rebuild::{IndexError, RebuildReport, SearchHit, index_status, rebuild, search};

/// `shadow::index_layer_names` 的公开包装：索引层表清单的唯一家。
/// `hub index` 与 `doctor` 都从它读清单——两处各算一遍就是下一次漂移。
/// 它给的是**名字清单**（读数），不是按名字写这张表的许可：写入面仍然只有 [`rebuild`]。
pub fn shadow_tables(
    conn: &rusqlite::Connection,
) -> Result<Vec<String>, rusqlite::Error> {
    shadow::index_layer_names(conn)
}
