//! 波二刀 A/B（设计 `2026-11-08-g1-reuse-lesson-design.md` §波次二）：**复用反馈**的
//! 写口与读口（`card_reuse_event`）。刀 A 做写入口；刀 B 做**消费两臂**——
//! rank 臂的批量聚合（[`reuse_ledger`]，`rc-search::rank` 消费）与审计臂的程序判定/入队
//! （[`audit_candidates`] / [`ingest_audit`]，`hub review reuse-ingest` 消费）。
//!
//! ## 两条语义纪律
//!
//! - **不复用 `card_verdict_event` / 表态**：那张表是**背书来源账**（"谁盖了 verdict"），
//!   表态是"我认为真假"，本表是"它帮没帮上忙"。两轴语义不同源，混一格就是一份闭集装两义。
//! - **append-only**：每次反馈追加一行，绝不 upsert/覆盖。同一张卡同一种 outcome 写两次
//!   = 两行——"用过两次、两次都成"本身就是刀 B rank 臂要读的那个读数（`worked >= 2`）。
//!   写入在**一个事务**里：核卡存在 → 追加事件，任何一步失败整笔回滚。
//!
//! ## 载体：canonical 正路
//!
//! DDL 的唯一家是 [`rc_store::ddl::CARD_REUSE_EVENT_SQL`]，它进 `MEMORY_SQL` 重放列 ⇒
//! **开库即存在**；表名进 `rc_memory::schema::MEMORY_TABLES` 与归属表 ⇒ hub 的 rows 吸收、
//! backup 摘要与 restore 整替都带着反馈走。`MEMORY_VERSION` 不 bump（`IF NOT EXISTS`）。
//!
//! 两处与 `value_gate` 同形的兼容保留：
//! - 写路径的 [`ensure`] 幂等补表是**双保险**（重放同一根 `CARD_REUSE_EVENT_SQL`，
//!   不另写一份 DDL）；先 `DROP TABLE` 再写的夹具因此不必改。
//! - 读路径 [`reuse_counts`] / [`recent_events`] **缺表 = 零读数**：刀 A 之前开的只读旧副本
//!   没有这张表（只读面不跑 `migrate_memory`），仍要能跑而不是报错。
//!
//! ## 值域的唯一家
//!
//! [`ReuseOutcome`] 的 `slug()` 是闭集的唯一声明处：schema 里的 CHECK、工具面的校验、
//! 写口的校验都引用它，不各抄一份字面量。`from_slug` **不 trim、不折叠大小写**——
//! 与 `VoteStance::from_slug` 同一把尺子（宽容读法会让 `bad_outcome` 测不到）。

use crate::keys::{MemKey, MemTable};
use crate::MemError;
use rc_store::MemoryHandle;
use rusqlite::Connection;
use std::collections::HashMap;

/// 复用结果闭集（设计 §波次二第 1 条逐字五档：成/败/部分成/条件不符/已失效）。
///
/// 值域的**唯一家**：`slug()` 的五个词与 `rc_store::ddl::CARD_REUSE_EVENT_SQL` 的 CHECK
/// 逐个同词（`tests/reuse_event.rs` 钉子逐字对撞）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReuseOutcome {
    Worked,
    Failed,
    Partial,
    Mismatch,
    Stale,
}

impl ReuseOutcome {
    pub const ALL: [ReuseOutcome; 5] = [
        ReuseOutcome::Worked,
        ReuseOutcome::Failed,
        ReuseOutcome::Partial,
        ReuseOutcome::Mismatch,
        ReuseOutcome::Stale,
    ];

    pub fn slug(self) -> &'static str {
        match self {
            ReuseOutcome::Worked => "worked",
            ReuseOutcome::Failed => "failed",
            ReuseOutcome::Partial => "partial",
            ReuseOutcome::Mismatch => "mismatch",
            ReuseOutcome::Stale => "stale",
        }
    }

    /// `slug()` 的逆。**精确匹配**（不 trim、不折叠大小写）——见档头"值域的唯一家"。
    pub fn from_slug(raw: &str) -> Option<ReuseOutcome> {
        ReuseOutcome::ALL.into_iter().find(|o| o.slug() == raw)
    }
}

/// 写口的两种拒法。**不进 `MemError`**：值域外的 outcome 不是账层故障，而 `MemError` 的
/// 每一支都在 `rc_agent` 的分型表里有对应格（加一支就要动那张表，那是另一条工单的面）。
#[derive(Debug)]
pub enum ReuseError {
    /// `outcome` 不在五档闭集里。不载荷原文：错误面不回显调用方的字节（守卫 12）。
    UnknownOutcome,
    /// 账层错误：卡不存在（`NotFound`）、只读副本（`ReadOnly`）、SQL。
    Mem(MemError),
}

impl From<MemError> for ReuseError {
    fn from(e: MemError) -> Self {
        ReuseError::Mem(e)
    }
}

/// SQL 错误走 `MemError` 的**消毒构造点**（`sqlite_text`：只留第一行、截 120 字符、
/// 出现路径形状整段换占位）——这里不再写第二份消毒。
impl From<rusqlite::Error> for ReuseError {
    fn from(e: rusqlite::Error) -> Self {
        ReuseError::Mem(MemError::from(e))
    }
}

impl std::fmt::Display for ReuseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReuseError::UnknownOutcome => write!(f, "outcome is not one of the five reuse values"),
            ReuseError::Mem(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ReuseError {}

fn table_exists(conn: &Connection) -> Result<bool, MemError> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='card_reuse_event'",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// 幂等地补表（**双保险**）。正路是 `MEMORY_SQL` 开库即建；这里重放的是同一根
/// [`rc_store::ddl::CARD_REUSE_EVENT_SQL`]，不是第二份 DDL。只在写路径调。
fn ensure(conn: &Connection) -> Result<(), MemError> {
    conn.execute_batch(rc_store::ddl::CARD_REUSE_EVENT_SQL)?;
    Ok(())
}

/// 记一次复用反馈。返回新事件的行 id（`card_reuse_event.id`）。
///
/// 顺序是判定的一部分：
/// ① 只读副本在**打开写之前**就拒（与 `attest` 同一支：让 SQLite 去撞只读连接会把
///    资格问题报成 SQL 出错）；
/// ② 闭集先过（[`ReuseOutcome::from_slug`]）——值域外一行都不写；
/// ③ 事务内**先认卡再写**（悬空反馈比没有反馈更坏，与 `vote` 的"先认行再写"同律）；
/// ④ append-only：只有 `INSERT`，没有 `UPDATE`/`ON CONFLICT`。
///
/// `norm_version` 由本函数盖（`rc_store::capability::NORM_VERSION`），调用方递不进来。
pub fn record_reuse(
    m: &MemoryHandle,
    card_id: &str,
    outcome: &str,
    note: Option<&str>,
    at_ms: i64,
    by_agent: &str,
) -> Result<i64, ReuseError> {
    if m.is_read_only() {
        return Err(ReuseError::Mem(MemError::ReadOnly));
    }
    let outcome = ReuseOutcome::from_slug(outcome).ok_or(ReuseError::UnknownOutcome)?;
    ensure(m.conn())?;
    let tx = m.conn().unchecked_transaction()?;
    let exists: i64 = tx.query_row(
        "SELECT count(*) FROM card WHERE id=?1",
        rusqlite::params![card_id],
        |r| r.get(0),
    )?;
    if exists == 0 {
        return Err(ReuseError::Mem(MemError::NotFound(MemKey {
            table: MemTable::Card,
            id: card_id.to_string(),
        })));
    }
    tx.execute(
        "INSERT INTO card_reuse_event(card_id, outcome, note, at_ms, by_agent, norm_version)
         VALUES(?1,?2,?3,?4,?5,?6)",
        rusqlite::params![
            card_id,
            outcome.slug(),
            note,
            at_ms,
            by_agent,
            rc_store::capability::NORM_VERSION as i64
        ],
    )?;
    let id = tx.last_insert_rowid();
    tx.commit()?;
    Ok(id)
}

/// 一张卡的反馈计数（刀 B 的 rank 臂读 `worked`，审计臂读 `failed`/`mismatch`）。
/// 缺表（刀 A 之前的只读旧副本）= 全零。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReuseCounts {
    pub worked: i64,
    pub failed: i64,
    pub partial: i64,
    pub mismatch: i64,
    pub stale: i64,
}

impl ReuseCounts {
    pub fn total(&self) -> i64 {
        self.worked + self.failed + self.partial + self.mismatch + self.stale
    }

    /// 按闭集取值。闭集外的 outcome 在库里不可能存在（CHECK 执法），这里只做映射。
    pub fn of(&self, outcome: ReuseOutcome) -> i64 {
        match outcome {
            ReuseOutcome::Worked => self.worked,
            ReuseOutcome::Failed => self.failed,
            ReuseOutcome::Partial => self.partial,
            ReuseOutcome::Mismatch => self.mismatch,
            ReuseOutcome::Stale => self.stale,
        }
    }
}

/// 五格聚合的 SELECT 列文本（**唯一一份**）：[`reuse_counts`] 按卡取、[`reuse_ledger`]
/// 全库分组取，两条查询的列序必须逐位相同（五格顺序 = `ReuseCounts` 的字段序）。抄第二份
/// 就是"同一格在两条查询里各有一个定义"，列序漂一格会不报错地读错相邻那一格。
const COUNT_COLUMNS: &str = "COALESCE(SUM(CASE WHEN outcome='worked'   THEN 1 ELSE 0 END),0), \
                             COALESCE(SUM(CASE WHEN outcome='failed'   THEN 1 ELSE 0 END),0), \
                             COALESCE(SUM(CASE WHEN outcome='partial'  THEN 1 ELSE 0 END),0), \
                             COALESCE(SUM(CASE WHEN outcome='mismatch' THEN 1 ELSE 0 END),0), \
                             COALESCE(SUM(CASE WHEN outcome='stale'    THEN 1 ELSE 0 END),0)";

fn read_counts(r: &rusqlite::Row<'_>, base: usize) -> rusqlite::Result<ReuseCounts> {
    Ok(ReuseCounts {
        worked: r.get(base)?,
        failed: r.get(base + 1)?,
        partial: r.get(base + 2)?,
        mismatch: r.get(base + 3)?,
        stale: r.get(base + 4)?,
    })
}

/// 按卡聚合的读口（单一实现，刀 B 与工具面共用）。**一次查询**出五格：
/// 每格是 `SUM(CASE WHEN outcome='…' THEN 1 ELSE 0 END)`，闭集外的行不存在。
pub fn reuse_counts(m: &MemoryHandle, card_id: &str) -> Result<ReuseCounts, MemError> {
    if !table_exists(m.conn())? {
        return Ok(ReuseCounts::default());
    }
    m.conn()
        .query_row(
            &format!("SELECT {COUNT_COLUMNS} FROM card_reuse_event WHERE card_id=?1"),
            rusqlite::params![card_id],
            |r| read_counts(r, 0),
        )
        .map_err(MemError::from)
}

/// 全库五格聚合（`card_id → ReuseCounts`），一次 `GROUP BY` 的批量读口。
///
/// 为什么要这一支而不是逐卡调 [`reuse_counts`]：刀 B 的 rank 臂每次召回都要拿候选卡的
/// 复用读数，逐卡一次 SQL 会把一次召回变成 N 次（`rank_counted` 已经为 archive 付过
/// "每次调用装载一次"的代价账，这里同一条）。**缺表 = 空 map**：刀 A 之前开的只读
/// 旧副本没有这张表，仍要能跑。
pub fn reuse_ledger(m: &MemoryHandle) -> Result<HashMap<String, ReuseCounts>, MemError> {
    if !table_exists(m.conn())? {
        return Ok(HashMap::new());
    }
    let mut st = m
        .conn()
        .prepare(&format!(
            "SELECT card_id, {COUNT_COLUMNS} FROM card_reuse_event GROUP BY card_id"
        ))?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, read_counts(r, 1)?)))?;
    let mut out = HashMap::new();
    for row in rows {
        let (card_id, counts) = row?;
        out.insert(card_id, counts);
    }
    Ok(out)
}

/// 一条反馈事件（读口形状）。`note` 是调用方给的短语，不是记忆正文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReuseEvent {
    pub id: i64,
    pub outcome: String,
    pub note: Option<String>,
    pub at_ms: i64,
    pub by_agent: String,
}

/// 一张卡最近的反馈事件（`id` 降序，最新在前）。缺表 = 空集。
pub fn recent_events(
    m: &MemoryHandle,
    card_id: &str,
    limit: usize,
) -> Result<Vec<ReuseEvent>, MemError> {
    if !table_exists(m.conn())? {
        return Ok(Vec::new());
    }
    let mut st = m.conn().prepare(
        "SELECT id, outcome, note, at_ms, by_agent FROM card_reuse_event
         WHERE card_id=?1 ORDER BY id DESC LIMIT ?2",
    )?;
    let rows = st.query_map(rusqlite::params![card_id, limit as i64], |r| {
        Ok(ReuseEvent {
            id: r.get(0)?,
            outcome: r.get(1)?,
            note: r.get(2)?,
            at_ms: r.get(3)?,
            by_agent: r.get(4)?,
        })
    })?;
    let mut out = Vec::new();
    for e in rows {
        out.push(e?);
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────────────
// 刀 B·rank 臂的阈值（读取侧，`rc-search::rank` 消费）
// ─────────────────────────────────────────────────────────────────────────────

/// rank 臂的升档阈值：一张卡的 `worked` 计数 `>= 2` 才升一档，`worked == 1` **不升**
/// （设计 §波次二第 3 条 + 派发档"`worked=1` 不加成（钉）"）。
///
/// 选 2 的理由是可解释的：一次"用上了"可能只是恰好，两次才是"这张卡反复帮上忙"——
/// 阈值住在复用域（这张表自己的语义），读取侧只引用它，不另抄一个数字。
pub const WORKED_TIER_MIN: i64 = 2;

// ─────────────────────────────────────────────────────────────────────────────
// 刀 B·审计臂：`failed` / `mismatch` 进复核队列（A9 管线，source='reuse'）
// ─────────────────────────────────────────────────────────────────────────────

/// 审计臂的程序判定闭集：这两档进复核队列（`worked`/`partial`/`stale` **不进**，钉死）。
/// 值域仍由 [`ReuseOutcome::slug`] 提供，SQL 用绑定参数引用它——不在 SQL 里再抄一份字面量。
pub const AUDIT_OUTCOMES: [ReuseOutcome; 2] = [ReuseOutcome::Failed, ReuseOutcome::Mismatch];

/// 一条待入复核队列的复用反馈（每张卡取**最新一条** failed/mismatch）。
///
/// "每卡一条"是查重口径的一半（另一半在 [`ingest_audit`] 的在途检查）：同一张卡连着
/// 报三次 failed，是"这张卡坏了三次"这一条证据，不是三条待审项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReuseAuditCandidate {
    pub card_id: String,
    pub outcome: ReuseOutcome,
    /// 最新那条 failed/mismatch 事件的 id（入队载荷里带上它，人审可回溯到原始反馈）。
    pub event_id: i64,
    pub at_ms: i64,
    pub by_agent: String,
    pub note: Option<String>,
    /// 这张卡的 failed/mismatch 事件总数（`worked`/`partial`/`stale` 不计）。
    pub event_count: i64,
}

/// 程序判定后的候选集：有 failed/mismatch 反馈、且卡还在的行，每卡一条（最新那条）。
/// 缺表 = 空集。认不出的 outcome slug 直接跳过（CHECK 让它在库里不可能存在；真出现了
/// 也不许折成 failed——那是把一条不知道是什么的反馈送去人审）。
pub fn audit_candidates(m: &MemoryHandle) -> Result<Vec<ReuseAuditCandidate>, MemError> {
    if !table_exists(m.conn())? {
        return Ok(Vec::new());
    }
    let mut st = m.conn().prepare(
        "SELECT e.card_id, e.outcome, e.id, e.at_ms, e.by_agent, e.note,
                (SELECT COUNT(*) FROM card_reuse_event x
                  WHERE x.card_id = e.card_id AND x.outcome IN (?1, ?2)) AS n
           FROM card_reuse_event e
          WHERE e.outcome IN (?1, ?2)
            AND e.id = (SELECT MAX(x.id) FROM card_reuse_event x
                         WHERE x.card_id = e.card_id AND x.outcome IN (?1, ?2))
          ORDER BY e.card_id",
    )?;
    let slug_failed = AUDIT_OUTCOMES[0].slug();
    let slug_mismatch = AUDIT_OUTCOMES[1].slug();
    let rows = st.query_map(rusqlite::params![slug_failed, slug_mismatch], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, i64>(6)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (card_id, outcome, event_id, at_ms, by_agent, note, event_count) = row?;
        let Some(outcome) = ReuseOutcome::from_slug(&outcome) else { continue };
        // 卡被物理删了（反馈外键只在写时挡）：悬空反馈不送审——没有行就没有可审的对象。
        if crate::reader::lookup(m, &MemKey { table: MemTable::Card, id: card_id.clone() })?
            .is_none()
        {
            continue;
        }
        out.push(ReuseAuditCandidate {
            card_id,
            outcome,
            event_id,
            at_ms,
            by_agent,
            note,
            event_count,
        });
    }
    Ok(out)
}

/// 一次审计臂入队的逐卡记账。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReuseAuditItem {
    /// `card:<hex>`（与工具面句柄同形，读数里直接可比）。
    pub card_id: String,
    pub review_item_id: i64,
    /// true = 本次新建；false = 命中去重（在途项或同载荷）。
    pub fresh: bool,
}

/// 审计臂入队的读数（`hub review reuse-ingest` 的 `--json` 就是这组字段）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReuseAuditOut {
    /// 程序判定后的候选卡数（每卡一条）。
    pub candidates: usize,
    /// 新建的待审项数。
    pub queued: usize,
    /// 命中去重的卡数（已有在途项 / 同载荷已入队）。
    pub deduped: usize,
    pub items: Vec<ReuseAuditItem>,
}

/// 这张卡是否已有一条**在途**的 reuse 待审项（`pending_review|quarantined|approved`）。
///
/// 查重口径照 `writers_c::raise_attention` 先例——按**目标**（卡）判，不按事件逐条入队：
/// 同一张卡坏了三次是一条待审项，第二次 ingest 不新建。终态（applied/rejected/stale）
/// 不占位：人工判完之后又出现新的 failed，那是新证据，可以重新开一条。
///
/// 目标从载荷的 `$.card` 读（`json_extract`，不是 `LIKE` 的子串近似——卡 id 是定长 hex，
/// 子串匹配会把前缀关系读成同一张卡）。`json_valid` 那一格防库里别的来源留下非 JSON 载荷。
fn inflight_item_for_card(m: &MemoryHandle, handle: &str) -> Result<Option<i64>, MemError> {
    let got = m.conn().query_row(
        "SELECT id FROM review_item
          WHERE source='reuse'
            AND state IN ('pending_review','quarantined','approved')
            AND json_valid(payload_json)
            AND json_extract(payload_json,'$.card') = ?1
          ORDER BY id LIMIT 1",
        rusqlite::params![handle],
        |r| r.get::<_, i64>(0),
    );
    match got {
        Ok(id) => Ok(Some(id)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(MemError::from(e)),
    }
}

/// 候选 → `review_item` 的载荷。四件东西都从库里取，**不读时钟**（载荷必须可复算，
/// 同一条事件重跑必须算出同一份 `payload_sha256`）：
/// - `turn_uids_json`：卡的出处轮（`provenance_chain` 的 `Turn` 步——卡自己没有
///   `provenance` 行时它经 `pointers_json → fact` 兜底，与 report 先例"目标照原样进
///   turn_uids_json"同形）；一轮都取不到就回落成卡句柄本身。
/// - `owner_json`：`{"reporter": <最新那条反馈的 by_agent>}`（报告人语义，与 report 同格）。
/// - `device_id`：卡自己的设备（反馈表没有设备列，卡是设备主语的真源）。
/// - `payload_json`：`reuse.v1` 信封（卡/档/事件 id/事件钟/次数/说明）。
fn new_audit_item(
    m: &MemoryHandle,
    candidate: &ReuseAuditCandidate,
    handle: &str,
) -> Result<crate::review::NewItem, MemError> {
    use crate::writers_fact::json_string;
    let key = MemKey { table: MemTable::Card, id: candidate.card_id.clone() };
    let facts = crate::reader::lookup(m, &key)?.ok_or_else(|| MemError::NotFound(key.clone()))?;
    let mut turns: Vec<String> = Vec::new();
    for step in crate::reader::provenance_chain(m, &key)? {
        if let crate::reader::ProvenanceStep::Turn { turn_uid, .. } = step {
            if !turns.contains(&turn_uid) {
                turns.push(turn_uid);
            }
        }
    }
    if turns.is_empty() {
        turns.push(handle.to_string());
    }
    let turn_uids_json = format!(
        "[{}]",
        turns.iter().map(|t| json_string(t)).collect::<Vec<_>>().join(",")
    );
    let owner_json = format!("{{\"reporter\":{}}}", json_string(&candidate.by_agent));
    // 载荷字段全部取自库里的行（没有时钟、没有随机量）：同一条事件重跑两次算出同一份
    // `payload_json` ⇒ `enqueue` 的载荷去重（H1c 之外的那道）返回 `fresh=false`。
    let payload_json = format!(
        "{{\"schema\":\"reuse.v1\",\"card\":{},\"outcome\":{},\"event_id\":{},\
          \"event_at_ms\":{},\"events\":{},\"note\":{}}}",
        json_string(handle),
        json_string(candidate.outcome.slug()),
        candidate.event_id,
        candidate.at_ms,
        candidate.event_count,
        candidate.note.as_deref().map(json_string).unwrap_or_else(|| "null".to_string()),
    );
    Ok(crate::review::NewItem {
        source: "reuse".to_string(),
        pass_id: "reuse".to_string(),
        // reuse 不归属任何抽取 pass 的 family（`check_item` 对非 fact 要求空串）。
        family: String::new(),
        // 反馈没有会话上下文这一格；它也不是顶替语义的批次（`batch_key` 空 = 不顶替）。
        session_key: String::new(),
        device_id: facts.device_id.clone(),
        turn_uids_json,
        owner_json,
        payload_json,
        batch_key: String::new(),
    })
}

/// 审计臂：把 `failed`/`mismatch` 反馈入复核队列（A9 管线，`source='reuse'`）。
///
/// **写点在程序判定之后**：先由 [`audit_candidates`] 判"哪些卡有负向反馈"，再查重在途项，
/// 最后才 `enqueue`。`worked`/`partial` 永远不会走到这里（[`AUDIT_OUTCOMES`]）。
/// 窄审模型**不碰**这类 item：引擎的 `supported_pass` 不认 `reuse`（照 `report` 先例），
/// 入队后只走人工判定链（`hub review decide`）。
///
/// 水位不在这里写：`review_watermark` 住水位库，写入口是 hub 那一层
/// （`hub review reuse-ingest` 入队后补写 `pass_id='reuse'` 的水位，见 `feedback-
/// review-item-ingress-needs-watermark` 的教训）。
pub fn ingest_audit(m: &MemoryHandle, now_ms: i64) -> Result<ReuseAuditOut, MemError> {
    if m.is_read_only() {
        return Err(MemError::ReadOnly);
    }
    let candidates = audit_candidates(m)?;
    let mut out = ReuseAuditOut { candidates: candidates.len(), ..Default::default() };
    for candidate in candidates {
        let handle = format!("card:{}", candidate.card_id);
        if let Some(id) = inflight_item_for_card(m, &handle)? {
            out.deduped += 1;
            out.items.push(ReuseAuditItem { card_id: handle, review_item_id: id, fresh: false });
            continue;
        }
        let item = new_audit_item(m, &candidate, &handle)?;
        let res = crate::review::enqueue(m, &item, now_ms)?;
        if res.fresh {
            out.queued += 1;
        } else {
            out.deduped += 1;
        }
        out.items.push(ReuseAuditItem { card_id: handle, review_item_id: res.id, fresh: res.fresh });
    }
    Ok(out)
}
