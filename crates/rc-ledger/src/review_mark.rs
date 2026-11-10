//! H7c2：审查轴水位（设计派发档 `2026-10-26-h7/H7c2-review-watermark.md` §2）。
//!
//! 每个 pass 一行：`max_decision_id` + 三态计数（`pending_review`/`quarantined`/`applied`）。
//! 它住水位库（`ledger.db`）而不是记忆库，理由与 `extract_pass_state` 同一条：
//! 这是"过程读数"，不是记忆内容。**零正文**——载荷、判定 detail、原因码一个字节都不进；
//! 计数由调用方从 `rc_memory::review::stats` 取**当前值**（全 pass 扫描，不是增量）。
//!
//! 复制面：`hub::replicate` 的账本快照带这张表（`review_watermarks`），接管后
//! `hub reconcile memory` 拿它与本地 `review_decision` 的最大 id 对比。
//!
//! 写入口只有 [`put`]（`INSERT OR REPLACE`，同 pass 覆盖）；读入口只有 [`all`]
//! （`pass_id` 升序，与快照导出同一顺序）。

use rc_store::{LedgerHandle, StoreError};
use rusqlite::params;

/// 一个 pass 的审查水位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewWatermark {
    pub pass_id: String,
    /// 写这一刻本地 `review_decision` 的最大 id（0 = 还没有判定）。
    pub max_decision_id: i64,
    pub pending_review: i64,
    pub quarantined: i64,
    pub applied: i64,
    pub updated_at_ms: i64,
}

/// 写/覆盖一个 pass 的水位。同 `pass_id` 覆盖（`INSERT OR REPLACE`）。
pub fn put(l: &LedgerHandle, w: &ReviewWatermark) -> Result<(), StoreError> {
    l.conn().execute(
        "INSERT OR REPLACE INTO review_watermark(
            pass_id, max_decision_id, pending_review, quarantined, applied, updated_at_ms)
         VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            w.pass_id,
            w.max_decision_id,
            w.pending_review,
            w.quarantined,
            w.applied,
            w.updated_at_ms
        ],
    )?;
    Ok(())
}

/// 全部水位，`pass_id` 升序（快照导出的顺序与这里同一把尺子）。
pub fn all(l: &LedgerHandle) -> Result<Vec<ReviewWatermark>, StoreError> {
    let mut st = l.conn().prepare(
        "SELECT pass_id, max_decision_id, pending_review, quarantined, applied, updated_at_ms
           FROM review_watermark ORDER BY pass_id",
    )?;
    let rows = st
        .query_map([], |r| {
            Ok(ReviewWatermark {
                pass_id: r.get(0)?,
                max_decision_id: r.get(1)?,
                pending_review: r.get(2)?,
                quarantined: r.get(3)?,
                applied: r.get(4)?,
                updated_at_ms: r.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}
