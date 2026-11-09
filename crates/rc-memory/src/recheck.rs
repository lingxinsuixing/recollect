//! 复核面（裁定 19 的机械形）：给"之后又出现了针对同一行的表态"的告警盖一戳。
//! 只有一支 `UPDATE`，而且它只碰 `rechecked_at_ms` 那一列——`resolved_at_ms` 的家在
//! `anomaly::sweep`（谓词不再成立才算解决），复核把两列一起写就是拿"看过了"冒充"不存在了"。
use crate::MemError;
use rc_store::MemoryHandle;

/// 盖戳：把"未解决、从没复核过、且 `attestation` 里有一行比它更晚"的告警各盖 `now_ms`。
/// 返回被盖的行数。
///
/// 四条守卫各有名字，撤任何一条都有一条用例点名它是红的：
/// ① `AND rechecked_at_ms IS NULL` = 幂等（撤 ⇒ 第二遍把同一行改成更晚的戳）；
/// ② `WHERE resolved_at_ms IS NULL` = 已解除的行不参与（撤 ⇒ 复核去盖死行的戳）；
/// ③ `a.created_at_ms > attention.created_at_ms` 的**严格**大于（改成 `>=` ⇒ 同一批事实
///    被读成"之后又有人说话"）；
/// ④ `EXISTS` 里的 `a.mem_table = attention.mem_table AND a.mem_id = attention.mem_id`
///    = 表态必须落在**同一行**上（折成恒真 ⇒ 一条表态把全库的活告警都盖了戳）。
///
/// 为什么谓词是"更晚的表态"而不是"有人点过复核按钮"：阶段 3 没有交互面，唯一能证明
/// "这一行被后来的事实动过"的账就是 `attestation`（A40 的闭包与表态都往它写）。
/// 判据与 `kind` 无关是有意的：`contradiction`（T11）与 `alias_ambiguity` /
/// `duplicate_title`（T13）三族共用这一支，谁都不许另造第二张复核账。
pub fn recheck_attested(m: &MemoryHandle, now_ms: i64) -> Result<usize, MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    let n = m.conn().execute(
        "UPDATE attention SET rechecked_at_ms = ?1
         WHERE resolved_at_ms IS NULL AND rechecked_at_ms IS NULL
           AND EXISTS (SELECT 1 FROM attestation a
                       WHERE a.mem_table = attention.mem_table AND a.mem_id = attention.mem_id
                         AND a.created_at_ms > attention.created_at_ms)",
        rusqlite::params![now_ms])?;
    Ok(n)
}

/// 待复核数 = 未解决**且**没盖过戳的告警行。它是 `status` 顶层的一格，不是第二个真值源：
/// 与 `attention_open` 相减得到的是"挂着且已经被复核过"的**累计**条数，不是"这一轮动过几条"
/// （后者没有家），所以两格必须同形（恒为数、不经缝）。
/// 这是读操作，只读副本上照读——它与 `recheck_attested` 的只读守卫正好相反，别"顺手"补一条。
pub fn recheck_pending(m: &MemoryHandle) -> Result<usize, MemError> {
    let n: i64 = m.conn().query_row(
        "SELECT count(*) FROM attention WHERE resolved_at_ms IS NULL AND rechecked_at_ms IS NULL",
        [], |r| r.get(0))?;
    Ok(n as usize)
}
