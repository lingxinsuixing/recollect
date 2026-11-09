//! 推导批的"读侧"——选下一批要喂给 LLM 的活行。
//! **名字与 `rc_distill::extract::select_batch` 同形而不同物**：那一支选的是会话轮次（返回
//! `Batch`），这一支选的是记忆活行（返回 `Vec<RowFacts>`）。两条都不许被 glob 再导出，调用点
//! 必须带模块路径——否则 hub 那一屏同时出现两个 `select_batch` 时会读错批的是哪一族。
//! 现阶段是 `reader::rows_since` 的纯转发；T5 在编排层加水位（`family='derive'`/`scope=''`），
//! 这里只负责"给一组 from / limit ⇒ 一组行"。

use rc_memory::{reader, MemError, RowFacts};
use rc_store::MemoryHandle;

/// 选下一批要喂给 LLM 的活行。`from = None` ⇒ 从头；`from = Some((ms, key))` ⇒ 严格大于起点。
/// `limit` 是行限（不是分页大小）——一批跑不完，**余量留下一轮**（P-24）。
pub fn select_batch(
    m: &MemoryHandle, from: Option<(i64, String)>, limit: usize,
) -> Result<Vec<RowFacts>, MemError> {
    reader::rows_since(m, from, limit)
}
