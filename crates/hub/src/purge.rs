//! 清除。**两段式清除**。
//!
//! 清除不许单行删除：参数只接受 `session_key` 或 `device_id`，
//! 不接受正文、不接受 id 列表——"删掉某一句话"在账本上无法表达，
//! 而能表达的那一面（按 id 删）会让人以为系统支持单行删除。
//!
//! 恢复自 B5a 起住 `hub::restore`；本模块只留 purge。

use rc_contract::safe_token;
use rc_ledger::transcript::tombstone_sha256;
use rc_store::{Root, StoreError};

/// 清除的口径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PurgeScope {
    Session(String),
    Device(String),
}

fn key_of(scope: &PurgeScope) -> &str {
    match scope {
        PurgeScope::Session(k) | PurgeScope::Device(k) => k,
    }
}

/// 列名只从这一处闭集合里出：用户给的键**只经 `?1` 绑定**，永远不拼进 SQL。
fn column_of(scope: &PurgeScope) -> &'static str {
    match scope {
        PurgeScope::Session(_) => "session_key",
        PurgeScope::Device(_) => "device_id",
    }
}

/// 审计行的 `subject` 过 `safe_token`：不合 token 形状的键**脱敏成 `(redacted)`**。
/// 不在这里拒绝——转录层的 `device_id`/`session_key` 在生产里可以是任意非空文本
/// （导入面只查非空），拒绝等于让操作员永远清不掉一条恶意命名的会话。
fn subject_of(scope: &PurgeScope) -> Option<&str> {
    Some(safe_token(key_of(scope)))
}

fn count_target(root: &Root, scope: &PurgeScope) -> Result<i64, StoreError> {
    let sql = format!("SELECT count(*) FROM turns WHERE {} = ?1", column_of(scope));
    Ok(root
        .transcript()
        .conn()
        .query_row(&sql, [key_of(scope)], |r| r.get(0))?)
}

fn scope_kind(scope: &PurgeScope) -> &'static str {
    match scope {
        PurgeScope::Session(_) => "session",
        PurgeScope::Device(_) => "device",
    }
}

/// 在转录库里给 purge 分配一个 commit_seq 并写墓碑，然后物理删行；三件事同一事务。
/// 返回 `(commit_seq, epoch)`；`n == 0` 时调用方不分配提交号。
fn delete_with_tombstone(
    root: &Root,
    scope: &PurgeScope,
    n: i64,
    now_ms: i64,
) -> Result<(i64, i64), StoreError> {
    let conn = root.transcript().conn();
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT OR IGNORE INTO center_state(id, last_commit_seq, epoch) VALUES(1, 0, 1)",
        [],
    )?;
    tx.execute(
        "UPDATE center_state SET last_commit_seq = last_commit_seq + 1 WHERE id = 1",
        [],
    )?;
    let (commit_seq, epoch): (i64, i64) = tx.query_row(
        "SELECT last_commit_seq, epoch FROM center_state WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let kind = scope_kind(scope);
    let key = key_of(scope);
    let sha = tombstone_sha256(kind, key, n as u64);
    tx.execute(
        "INSERT INTO tombstone(commit_seq, epoch, scope_kind, scope_key,
                               turns_removed, content_sha256, created_at_ms)
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
        rusqlite::params![commit_seq, epoch, kind, key, n, sha, now_ms],
    )?;
    let column = column_of(scope);
    let deleted = tx.execute(
        &format!("DELETE FROM turns WHERE {column} = ?1"),
        rusqlite::params![key],
    )?;
    if deleted as i64 != n {
        return Err(StoreError::Sql(
            "purge deleted a different number of turns than it counted".to_string(),
        ));
    }
    tx.commit()?;
    Ok((commit_seq, epoch))
}

/// 第一段：**只写抑制账**，转录行数不变。
///
/// 为什么分两段：物理删行不可逆，而"这批内容不该再被蒸"这件事是可逆的。
/// 第一段让操作员先看见"会清掉多少"，第二段才真的删。
pub fn suppress(root: &mut Root, scope: &PurgeScope, now_ms: i64) -> Result<i64, StoreError> {
    let n = count_target(root, scope)?;
    rc_ledger::device::append_audit(
        root.ledger_mut(),
        now_ms,
        "operator",
        "purge",
        subject_of(scope),
        "suppressed",
    )?;
    rc_ledger::counters::bump(
        root.ledger_mut(),
        "maintenance",
        "purge",
        "pending_redistill_turns",
        n,
        now_ms,
    )?;
    Ok(n)
}

/// 第二段：分配提交号 + 写墓碑 + 物理删 + 记数。`sessions` 行保留：
/// 它是归属的一半；删了它，同一 `session_key` 的下一批会另起一行，没人会发现。
pub fn purge(root: &mut Root, scope: &PurgeScope, now_ms: i64) -> Result<i64, StoreError> {
    let n = count_target(root, scope)?;
    if n > 0 {
        delete_with_tombstone(root, scope, n, now_ms)?;
    }
    rc_ledger::device::append_audit(
        root.ledger_mut(),
        now_ms,
        "operator",
        "purge",
        subject_of(scope),
        "deleted",
    )?;
    rc_ledger::counters::bump(root.ledger_mut(), "maintenance", "purge", "purged_turns", n, now_ms)?;
    Ok(n)
}
