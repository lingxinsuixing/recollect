use rc_contract::{BatchReport, Locator};
use rc_store::{LedgerHandle, StoreError};
use rusqlite::params;

/// `import_log` 的唯一 INSERT。CLI 的 `import` 与 HTTP 的 `/v1/ingest` 共用它：
/// 两个入口各写一次 SQL，`batch_reject_code` 与 `turns_rejected` 迟早在一边漂。
#[derive(Debug, Clone, Copy)]
pub struct ImportLogRow<'a> {
    pub started_at_ms: i64,
    pub finished_at_ms: i64,
    pub device_id: Option<&'a str>,
    pub exchange_sha256: &'a str,
    pub sessions_in: i64,
    pub turns_accepted: i64,
    pub turns_rejected: i64,
    pub batch_reject_code: &'a str,
}

pub fn log_ingest(l: &LedgerHandle, row: &ImportLogRow<'_>) -> Result<(), StoreError> {
    l.conn().execute(
        "INSERT INTO import_log(started_at_ms, finished_at_ms, device_id, exchange_sha256,
                                sessions_in, turns_accepted, turns_rejected, batch_reject_code)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![row.started_at_ms, row.finished_at_ms, row.device_id, row.exchange_sha256,
                row.sessions_in, row.turns_accepted, row.turns_rejected, row.batch_reject_code],
    )?;
    Ok(())
}

pub fn log_import(
    l: &LedgerHandle,
    started_at_ms: i64,
    finished_at_ms: i64,
    device_id: Option<&str>,
    exchange_sha256: &str,
    rep: &BatchReport,
) -> Result<(), StoreError> {
    // code 走 RejectCode::slug()：和 hub 的报告同一个字符串，否则审计行与当场输出对不上。
    let batch = rep.batch_reject.map(|c| c.slug()).unwrap_or_default();
    // 一条 turn 可以有多个分型（缺 created_at_ms 且正文为空）⇒ 计数读数是"被拒 turn 数"，
    // 和 hub 报告里的 turns_rejected 同一个口径——去重算法住在 BatchReport，两边各写一遍就是漂移。
    let row = ImportLogRow {
        started_at_ms,
        finished_at_ms,
        device_id,
        exchange_sha256,
        sessions_in: rep.accepted.iter().map(|a| &a.session.session_key)
            .collect::<std::collections::HashSet<_>>().len() as i64,
        turns_accepted: rep.accepted.len() as i64,
        turns_rejected: rep.rejected_turn_count() as i64,
        batch_reject_code: &batch,
    };
    log_ingest(l, &row)
}

#[allow(dead_code)]
fn loc_str(l: &Locator) -> String {
    match l.turn_index {
        Some(t) => format!("s{}t{}", l.session_index, t),
        None => format!("s{}", l.session_index),
    }
}
