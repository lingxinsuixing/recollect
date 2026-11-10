//! 邀请票的账本面（S4b1）：发行、作废、消费。
//!
//! 票的**签名**在 `rc-net::invite`，这里只管"这张票还能不能用一次"。两者的接缝是
//! `id`：出票方随机生成它、写进被签名的 canonical，再把同一串 `id` 登记到这张表。
//!
//! 纪律与 `device::pair` 逐条相同：
//! - 凭证（票/码）与设备写在**同一个事务**里，先吃票、后落设备；
//! - 条件 `UPDATE ... WHERE used_at_ms IS NULL` 是原子判据，两个节点同时提交只有一发成功；
//! - `DeviceExists` ⇒ 整个事务回滚，票**不**被消费（否则一台占着 device_id 的活设备
//!   能靠重复配对把别人的票烧光）。

use crate::device::{self, RegisterOutcome};
use rc_store::{LedgerHandle, StoreError};
use rusqlite::{params, OptionalExtension};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteOutcome {
    Paired,
    /// 表里没有这一行：票是伪造的，或者根本没在这台中心出过。
    Unknown,
    Used,
    Expired,
    Revoked,
    /// 设备 id 已被一台活设备占着；票**没有**被消费。
    DeviceExists,
}

pub fn issue(l: &mut LedgerHandle, id: &str, now_ms: i64, expires_at_ms: i64) -> Result<(), StoreError> {
    let tx = l.conn().unchecked_transaction()?;
    // 过期超过一天的票清掉：它们已经不可用（`consume` 也会按过期拒），留着只是让
    // "这台中心发过哪些票"这张表无限长大。窗口与 `issue_pair_code` 同量级。
    tx.execute("DELETE FROM invite WHERE expires_at_ms < ?1", [now_ms - 86_400_000])?;
    tx.execute(
        "INSERT INTO invite(id, created_at_ms, expires_at_ms) VALUES(?1,?2,?3)",
        params![id, now_ms, expires_at_ms],
    )?;
    // 出票留一条审计：之后"这张票是什么时候、由谁签出去的"只在这里有落款。
    // `subject` 是票 id，`detail` 留空——**票文本（或它的签名）绝不进审计**：
    // 审计行会被导出、被复制、被贴进工单，而票本身是一次性秘密。
    device::append_audit_on(&tx, now_ms, "operator", "invite_issued", Some(id), "")?;
    tx.commit()?;
    Ok(())
}

/// 作废一张票。返回 `false` 只表示**没有这条**；已经作废过的再作废是幂等的 `true`
/// （第一次的 `revoked_at_ms` 保留——那是"什么时候被叫停"的唯一读数）。
///
/// 审计与第一次作废同生共死：重复作废**不再写第二行**（幂等），否则"撤销了几次"就会
/// 被读成"撤了几张票"。
pub fn revoke(l: &mut LedgerHandle, id: &str, now_ms: i64) -> Result<bool, StoreError> {
    let tx = l.conn().unchecked_transaction()?;
    let existing: Option<Option<i64>> = tx
        .query_row("SELECT revoked_at_ms FROM invite WHERE id=?1", [id], |r| r.get(0))
        .optional()?;
    let Some(revoked_at_ms) = existing else {
        return Ok(false);
    };
    if revoked_at_ms.is_some() {
        return Ok(true);
    }
    tx.execute(
        "UPDATE invite SET revoked_at_ms=COALESCE(revoked_at_ms, ?2) WHERE id=?1",
        params![id, now_ms],
    )?;
    device::append_audit_on(&tx, now_ms, "operator", "invite_revoked", Some(id), "")?;
    tx.commit()?;
    Ok(true)
}

/// 消费票 + 落/复活设备 + 审计，一个事务。
///
/// 判定顺序是 used → revoked → expired：已用的票不再关心它是否过期（"重放"是更准确的说法）；
/// 已撤销的票比"过期"更该被点名（运维刚叫停的东西，报 `Expired` 会让人以为等一等就行）。
pub fn consume(
    l: &mut LedgerHandle,
    id: &str,
    device_id: &str,
    pubkey_b64: &str,
    token_hash: &str,
    now_ms: i64,
) -> Result<InviteOutcome, StoreError> {
    let tx = l.conn().unchecked_transaction()?;
    let row: Option<(i64, Option<i64>, Option<i64>)> = tx
        .query_row(
            "SELECT expires_at_ms, used_at_ms, revoked_at_ms FROM invite WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((expires_at_ms, used_at_ms, revoked_at_ms)) = row else {
        return Ok(InviteOutcome::Unknown);
    };
    if used_at_ms.is_some() {
        return Ok(InviteOutcome::Used);
    }
    if revoked_at_ms.is_some() {
        return Ok(InviteOutcome::Revoked);
    }
    // 过期判据的方向必须是"宁拒勿放"：钟在 unix epoch 之前时 `now_ms` 会被折成 0，
    // 而单看 `expires_at_ms < now_ms` 会把每一张票都读成"还没过期"——坏钟不该让
    // 一张旧票复活。`now_ms <= 0` 一律按过期处理。
    if now_ms <= 0 || expires_at_ms < now_ms {
        return Ok(InviteOutcome::Expired);
    }
    let changed = tx.execute(
        "UPDATE invite SET used_at_ms=?2, used_by_device=?3
         WHERE id=?1 AND used_at_ms IS NULL",
        params![id, now_ms, device_id],
    )?;
    if changed != 1 {
        // 上面刚读到的 `used_at_ms` 是 NULL，而条件 UPDATE 没改动任何行 ⇒ 中间有人抢先了。
        return Ok(InviteOutcome::Used);
    }
    if device::register_device(&tx, device_id, pubkey_b64, token_hash, now_ms)?
        == RegisterOutcome::DeviceExists
    {
        // 早退 ⇒ `tx` 未 commit ⇒ 上面那一发 UPDATE 一起回滚。
        return Ok(InviteOutcome::DeviceExists);
    }
    device::append_audit_on(&tx, now_ms, device_id, "pair", Some(device_id), "")?;
    tx.commit()?;
    Ok(InviteOutcome::Paired)
}
