//! 轮转态落盘（A47 节 4 第 7 条）。**一台机器一行**：`machine` 就是既有的设备标识命名空间，
//! 由 `hub` 保证它是过形值（Task 7 的 hub 装配批已落地：取值那一处立刻过 `device_subject` 才递进来）——这里不校验，因为"缺身份"的处置（不读不写、响亮一句）在 hub 那一侧。
//! `read` 那句 `WHERE machine=?1` 就是四条作废规则里**①的落点**：机器不符不是"给不给种"的判断，是根本读不出那一行。
use rc_store::{LedgerHandle, StoreError};
use rusqlite::params;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub machine: String,
    pub slug: String,
    pub probe_due_at_ms: Option<i64>,
    pub probe_ok: u32,
    pub probe_failures: usize,
    pub written_at_ms: i64,
}

pub fn read(l: &LedgerHandle, machine: &str) -> Result<Option<Row>, StoreError> {
    let mut st = l.conn().prepare(
        "SELECT slug, probe_due_at_ms, probe_ok, probe_failures, written_at_ms
           FROM llm_state WHERE machine=?1")?;
    let mut rows = st.query_map([machine], |r| Ok(Row {
        machine: machine.to_string(),
        slug: r.get(0)?,
        probe_due_at_ms: r.get(1)?,
        probe_ok: r.get::<_, i64>(2)? as u32,
        probe_failures: r.get::<_, i64>(3)? as usize,
        written_at_ms: r.get(4)?,
    }))?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),   // "这台机器从没写过"与"写过但主家是 0 号"是两件事，这里不替调用方造行
    }
}

/// 覆盖写。用 `ON CONFLICT DO UPDATE` 而不是先 DELETE 再 INSERT：
/// 一台机器只有一行这件事要靠主键说，而不是靠"我先删过了"。
pub fn write(l: &LedgerHandle, row: &Row) -> Result<(), StoreError> {
    l.conn().execute(
        "INSERT INTO llm_state(machine, slug, probe_due_at_ms, probe_ok, probe_failures, written_at_ms)
         VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(machine) DO UPDATE SET
           slug=excluded.slug, probe_due_at_ms=excluded.probe_due_at_ms,
           probe_ok=excluded.probe_ok, probe_failures=excluded.probe_failures,
           written_at_ms=excluded.written_at_ms",
        params![row.machine, row.slug, row.probe_due_at_ms,
                row.probe_ok as i64, row.probe_failures as i64, row.written_at_ms],
    )?;
    Ok(())
}
