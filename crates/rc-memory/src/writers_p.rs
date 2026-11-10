//! P（程序自己）的写入口：读路径唯一被允许碰的那一列——`cooldown_until_ms`（V4 / V7）。
//! 它是"这一轮不再提"，不是"降级"也不是"删除"：形态判定仍在 `rc-search` 那一侧读它自己算的分数。

use crate::keys::{MemKey, MemTable};
use crate::MemError;
use rc_store::MemoryHandle;

/// 待实测的常数（阶段 5 收口读数 = 同会话重复注入次数）。
/// **代价明写**：D4 定的窗口单位是会话数，本阶段的载体是时长列——因为 §1 要求这一列
/// 能被节点侧的 probe 逐行读，而文件不下行。D4 的分片文件押后，届时只做 doctor 读数。
pub const COOLDOWN_MS: i64 = 3_600_000;

/// 冷却列只存在于 `card`（`anchor` 没有这一列，见 `rc_store::ddl::MEMORY_SQL`）。
const FACT_TABLES: [MemTable; 1] = [MemTable::Card];

/// 回执就是判据：返回的是**真被写到的行数**，不是"调用成功了"。
/// 少一行意味着某个句柄在库里已经不存在，而调用侧（hook 的 stamp_cooldown）要看的就是这个差。
pub fn bump_cooldown(m: &MemoryHandle, targets: &[MemKey], now_ms: i64, window_ms: i64)
    -> Result<usize, MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    let until = now_ms + window_ms;
    let mut touched = 0usize;
    for t in targets {
        if t.table == MemTable::Anchor {
            // 锚没有冷却列 ⇒ 不是"写了 0 行"而是"这个形态不存在"，与 `set_verdict` 同一口径。
            return Err(MemError::NotFound(t.clone()));
        }
        let n = m.conn().execute(
            &format!("UPDATE {} SET cooldown_until_ms=?1 WHERE id=?2", t.table.prefix()),
            rusqlite::params![until, t.id])?;
        // id 是 PK ⇒ >1 只能是表形状被人改过；0 是"这个句柄不在了"，计入回执而不报错。
        if n > 1 {
            return Err(MemError::Sql(format!(
                "cooldown stamp wrote {n} rows for {}", t.key_string())));
        }
        touched += n;
    }
    Ok(touched)
}

/// 到期就清列。一条 UPDATE 覆盖 card，返回清掉的总行数（回执同 `bump_cooldown`）。
/// `<=` 而不是 `<`：`now_ms` 本身算到期，否则"窗口 500ms、第 500ms 来清"会多留一整轮。
pub fn clear_expired(m: &MemoryHandle, now_ms: i64) -> Result<usize, MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    let mut cleared = 0usize;
    for table in FACT_TABLES {
        cleared += m.conn().execute(
            &format!("UPDATE {} SET cooldown_until_ms=NULL
                      WHERE cooldown_until_ms IS NOT NULL AND cooldown_until_ms<=?1", table.prefix()),
            rusqlite::params![now_ms])?;
    }
    Ok(cleared)
}
