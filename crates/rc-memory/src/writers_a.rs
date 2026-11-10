//! A（宿主 agent）的写入口：只有表态这一条缝。
//! 这里能写的列是 `attestation` 的 stance / anchor_turn_uid / reason（见 `schema.rs` 的归属登记），
//! `actor` 与 `created_at_ms` 由本函数盖章——调用方递不进来。

use crate::keys::{MemKey, Stance};
use crate::writers_c::resolve_retirement;
use crate::MemError;
use rc_store::MemoryHandle;

pub fn attest(m: &MemoryHandle, target: &MemKey, stance: Stance,
              anchor: Option<&str>, reason: Option<&str>, now_ms: i64) -> Result<(), MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    // 三件事一个事务：核锚 → 解除旧表态 + 解除退场账 → 插新表态。
    // 任何一步失败都整笔回滚，不许留下"旧表态已解除、新表态没落"的半截态。
    let tx = m.conn().unchecked_transaction()?;
    if stance.is_positive() {
        // §2：正面表态必带解析到人工轮的锚；rejected 与退场免锚。
        // 锚的 role 现查会话库由调用侧（mcp，它能持两柄）做，这里收"已解析过的 uid"，
        // 并**当场核对出处表**：这条锚必须是该记忆的一条人工轮出处，否则它指不到东西（V6）。
        let Some(uid) = anchor else { return Err(MemError::AnchorNotHuman); };
        let n: i64 = tx.query_row(
            "SELECT count(*) FROM provenance WHERE mem_table=?1 AND mem_id=?2
             AND turn_uid=?3 AND anchor_role='human'",
            rusqlite::params![target.table.prefix(), target.id, uid], |r| r.get(0))?;
        if n == 0 { return Err(MemError::AnchorNotHuman); }
    }
    // §9：任何新表态都让此前同（mem_table, mem_id）下尚未解除的表态解除。
    // "解除时刻 = 新表态时刻"是这条 UPDATE 在维护的事实，所以"被解除的 → 在场的 →
    // 解除时刻即新表态时刻"这条时间序天然成立（`attestations_of` 按 created_at_ms
    // 升序，新表 NULL 永远落在最后）。E50：否决是终态——`stance <> 'rejected'`
    // 让被否决的那一条永远留在场，后一次表态不许把它复活。
    tx.execute(
        "UPDATE attestation SET resolved_at_ms=?1
         WHERE mem_table=?2 AND mem_id=?3 AND resolved_at_ms IS NULL
           AND stance <> 'rejected'",
        rusqlite::params![now_ms, target.table.prefix(), target.id])?;
    if stance.is_positive() {
        // C24-N1：正面表态 = 新证据到场 ⇒ 退场账解除。`resolve_retirement` 仍是**唯一落笔**；
        // 它不自己开事务，这一句因此加入上面那个事务（同一个连接），INSERT 失败时一起回滚。
        resolve_retirement(m, target, now_ms)?;
    }
    tx.execute(
        "INSERT INTO attestation(mem_table, mem_id, stance, actor, anchor_turn_uid, reason, created_at_ms)
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
        rusqlite::params![target.table.prefix(), target.id, stance.as_str(), "agent",
                          anchor, reason, now_ms])?;
    tx.commit()?;
    Ok(())
}
