use rc_store::{LedgerHandle, StoreError};
use rusqlite::{params, OptionalExtension};

pub const AUDIT_ACTIONS: [&str; 9] = [
    "pair",
    "revoke",
    "pair_code_issued",
    "invite_issued",
    "invite_revoked",
    "purge",
    "restore",
    "replicate_grant",
    "replicate_revoke",
];
pub const CAPABILITY_REPLICATE: &str = "replicate";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairOutcome {
    Paired,
    CodeUnknown,
    CodeUsed,
    CodeExpired,
    DeviceExists,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevokeOutcome {
    Revoked,
    AlreadyRevoked,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRow {
    pub device_id: String,
    pub pubkey: String,
    pub token_hash: String,
    pub revoked_at_ms: Option<i64>,
}

pub fn issue_pair_code(
    l: &mut LedgerHandle,
    code_hash: &str,
    now_ms: i64,
    expires_at_ms: i64,
) -> Result<(), StoreError> {
    let tx = l.conn().unchecked_transaction()?;
    // 过期超过一天的码清掉：这张表只服务 5 分钟窗口，留着只是让重放面变大。
    tx.execute("DELETE FROM pair_code WHERE expires_at_ms < ?1", [now_ms - 86_400_000])?;
    tx.execute(
        "INSERT INTO pair_code(code_hash, created_at_ms, expires_at_ms) VALUES(?1,?2,?3)",
        params![code_hash, now_ms, expires_at_ms],
    )?;
    append_audit_on(&tx, now_ms, "operator", "pair_code_issued", None, "")?;
    tx.commit()?;
    Ok(())
}

/// 消费码 + 落设备 + 审计三件事一个事务。任何一步失败都回滚：
/// 半配对的设备比没配对更坏——它会带着一个已作废的码占住 device_id。
///
/// **顺序是"先吃码、后落设备"**（计划字面块写的是反过来）：用例
/// `token_hash_unique_violation_rolls_back_the_code_consumption` 的名字与它点名的变异
/// （"把 `tx` 改成直接 `l.conn()`"）都假设码已经吃下、靠事务回滚还回去。落设备在前时，
/// 唯一约束在 UPDATE 之前就炸，码从未被标已用，去掉事务也照样绿 ⇒ 那条用例没有牙。
pub fn pair(
    l: &mut LedgerHandle,
    code_hash: &str,
    device_id: &str,
    pubkey_b64: &str,
    token_hash: &str,
    now_ms: i64,
) -> Result<PairOutcome, StoreError> {
    let tx = l.conn().unchecked_transaction()?;
    let code: Option<(i64, Option<i64>)> = tx
        .query_row(
            "SELECT expires_at_ms, used_at_ms FROM pair_code WHERE code_hash=?1",
            [code_hash],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((expires_at_ms, used_at_ms)) = code else {
        return Ok(PairOutcome::CodeUnknown);
    };
    if used_at_ms.is_some() {
        return Ok(PairOutcome::CodeUsed);
    }
    if expires_at_ms < now_ms {
        return Ok(PairOutcome::CodeExpired);
    }
    // 条件 UPDATE 是原子判据：`used_at_ms IS NULL` 不成立 ⇒ 这个码已被别人吃掉。
    let changed = tx.execute(
        "UPDATE pair_code SET used_at_ms=?2, used_by_device=?3
         WHERE code_hash=?1 AND used_at_ms IS NULL",
        params![code_hash, now_ms, device_id],
    )?;
    if changed != 1 {
        return Ok(PairOutcome::CodeUsed);
    }
    if register_device(&tx, device_id, pubkey_b64, token_hash, now_ms)?
        == RegisterOutcome::DeviceExists
    {
        return Ok(PairOutcome::DeviceExists);
    }
    append_audit_on(&tx, now_ms, device_id, "pair", Some(device_id), "")?;
    tx.commit()?;
    Ok(PairOutcome::Paired)
}

/// 事务内"落设备"的返回值。`DeviceExists` 是**调用方必须回滚**的那一格：
/// 凭证已经在这一步之前被吃掉，回滚才是"票没被消费"的唯一实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegisterOutcome {
    Written,
    DeviceExists,
}

/// 事务内"落设备"的那一步：`pair`（配对照表）与 `invite::consume`（邀请票）共用。
///
/// **不自己开事务、不 commit**：调用方必须在同一个事务里先吃掉凭证、再调这里，
/// 这样"设备 id 已占用"（`DeviceExists`）时凭证跟着回滚，而不是被烧掉。
/// 撤销过的设备允许复活（`revoked_at_ms` 清空）；活着的设备一律拒。
pub(crate) fn register_device(
    tx: &rusqlite::Connection,
    device_id: &str,
    pubkey_b64: &str,
    token_hash: &str,
    now_ms: i64,
) -> Result<RegisterOutcome, StoreError> {
    let existing: Option<Option<i64>> = tx
        .query_row("SELECT revoked_at_ms FROM device WHERE device_id=?1", [device_id], |r| r.get(0))
        .optional()?;
    match existing {
        Some(None) => Ok(RegisterOutcome::DeviceExists),
        Some(Some(_)) => {
            tx.execute(
                "UPDATE device SET pubkey=?2, token_hash=?3, paired_at_ms=?4, revoked_at_ms=NULL
                 WHERE device_id=?1",
                params![device_id, pubkey_b64, token_hash, now_ms],
            )?;
            Ok(RegisterOutcome::Written)
        }
        None => {
            tx.execute(
                "INSERT INTO device(device_id, pubkey, token_hash, paired_at_ms)
                 VALUES(?1,?2,?3,?4)",
                params![device_id, pubkey_b64, token_hash, now_ms],
            )?;
            Ok(RegisterOutcome::Written)
        }
    }
}

pub fn find_by_token_hash(l: &LedgerHandle, token_hash: &str) -> Result<Option<DeviceRow>, StoreError> {
    l.conn()
        .query_row(
            "SELECT device_id, pubkey, token_hash, revoked_at_ms FROM device WHERE token_hash=?1",
            [token_hash],
            |r| Ok(DeviceRow {
                device_id: r.get(0)?,
                pubkey: r.get(1)?,
                token_hash: r.get(2)?,
                revoked_at_ms: r.get(3)?,
            }),
        )
        .optional()
        .map_err(StoreError::from)
}

pub fn find_by_id(l: &LedgerHandle, device_id: &str) -> Result<Option<DeviceRow>, StoreError> {
    l.conn()
        .query_row(
            "SELECT device_id, pubkey, token_hash, revoked_at_ms FROM device WHERE device_id=?1",
            [device_id],
            |r| Ok(DeviceRow {
                device_id: r.get(0)?,
                pubkey: r.get(1)?,
                token_hash: r.get(2)?,
                revoked_at_ms: r.get(3)?,
            }),
        )
        .optional()
        .map_err(StoreError::from)
}

pub fn revoke(l: &mut LedgerHandle, device_id: &str, now_ms: i64) -> Result<RevokeOutcome, StoreError> {
    let tx = l.conn().unchecked_transaction()?;
    let exists: Option<Option<i64>> = tx
        .query_row("SELECT revoked_at_ms FROM device WHERE device_id=?1", [device_id], |r| r.get(0))
        .optional()?;
    let Some(revoked) = exists else {
        return Ok(RevokeOutcome::Unknown);
    };
    if revoked.is_some() {
        return Ok(RevokeOutcome::AlreadyRevoked);
    }
    tx.execute("UPDATE device SET revoked_at_ms=?2 WHERE device_id=?1", params![device_id, now_ms])?;
    append_audit_on(&tx, now_ms, "operator", "revoke", Some(device_id), "")?;
    tx.commit()?;
    Ok(RevokeOutcome::Revoked)
}

pub fn append_audit(
    l: &LedgerHandle,
    at_ms: i64,
    actor: &str,
    action: &str,
    subject: Option<&str>,
    detail: &str,
) -> Result<(), StoreError> {
    l.conn().execute(
        "INSERT INTO audit_log(at_ms, actor, action, subject, detail) VALUES(?1,?2,?3,?4,?5)",
        params![at_ms, actor, action, subject, detail],
    )?;
    Ok(())
}

/// 给已配对设备授一条能力。设备不存在 ⇒ `false`，不凭空造一行授权。
/// 重复授予幂等（并清掉旧撤销时刻）；`capability` 必须是表 CHECK 里的闭集。
pub fn grant_capability(
    l: &mut LedgerHandle,
    device_id: &str,
    capability: &str,
    now_ms: i64,
) -> Result<bool, StoreError> {
    let tx = l.conn().unchecked_transaction()?;
    let exists: Option<i64> = tx
        .query_row("SELECT 1 FROM device WHERE device_id=?1", [device_id], |r| r.get(0))
        .optional()?;
    if exists.is_none() {
        return Ok(false);
    }
    tx.execute(
        "INSERT INTO device_capability(device_id, capability, granted_at_ms, revoked_at_ms)
         VALUES(?1,?2,?3,NULL)
         ON CONFLICT(device_id, capability) DO UPDATE SET
           granted_at_ms=excluded.granted_at_ms,
           revoked_at_ms=NULL",
        params![device_id, capability, now_ms],
    )?;
    append_audit_on(
        &tx,
        now_ms,
        "operator",
        "replicate_grant",
        Some(device_id),
        capability,
    )?;
    tx.commit()?;
    Ok(true)
}

/// 撤销一条能力。返回是否真的从“有效”变成“撤销”；未知/已撤销都是 `false`。
pub fn revoke_capability(
    l: &mut LedgerHandle,
    device_id: &str,
    capability: &str,
    now_ms: i64,
) -> Result<bool, StoreError> {
    let tx = l.conn().unchecked_transaction()?;
    let changed = tx.execute(
        "UPDATE device_capability SET revoked_at_ms=?1
         WHERE device_id=?2 AND capability=?3 AND revoked_at_ms IS NULL",
        params![now_ms, device_id, capability],
    )?;
    if changed > 0 {
        append_audit_on(
            &tx,
            now_ms,
            "operator",
            "replicate_revoke",
            Some(device_id),
            capability,
        )?;
    }
    tx.commit()?;
    Ok(changed > 0)
}

pub fn has_capability(
    l: &LedgerHandle,
    device_id: &str,
    capability: &str,
) -> Result<bool, StoreError> {
    let found: Option<i64> = l
        .conn()
        .query_row(
            "SELECT 1 FROM device_capability
             WHERE device_id=?1 AND capability=?2 AND revoked_at_ms IS NULL",
            params![device_id, capability],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// 审计写入的事务内版本：调用方已经开了一个事务（配对/邀请票），审计必须**同生共死**——
/// 设备落库而审计失败、或反过来，都会让"谁在什么时候进来的"这句话失去落款。
pub(crate) fn append_audit_on(
    conn: &rusqlite::Connection,
    at_ms: i64,
    actor: &str,
    action: &str,
    subject: Option<&str>,
    detail: &str,
) -> Result<(), StoreError> {
    debug_assert!(AUDIT_ACTIONS.contains(&action), "未登记的审计动作：{action}");
    conn.execute(
        "INSERT INTO audit_log(at_ms, actor, action, subject, detail) VALUES(?1,?2,?3,?4,?5)",
        params![at_ms, actor, action, subject, detail],
    )?;
    Ok(())
}
