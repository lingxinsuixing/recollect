//! §7.3a：逐条判决与证据的落库面（`review_verdict`）。
//!
//! 为什么需要这张表：`review_decision.detail` 只有一句摘要（`p=7;r=0;u=0;pass=f1..f7;
//! uncertain=-`），"哪几条草稿被判 uncertain、为什么、引用了哪段原文"在库上不可查。这一张
//! 把每一侧（`proposer`/`challenger`/`final`）的逐条判决各存一行：`evidence` 是模型给的
//! **原文**（不 trim、不改写），`evidence_state` 是证据闸的程序侧裁定。
//!
//! ## 这张表为什么不在 `MEMORY_TABLES` 里
//!
//! 它是审查侧表的**证据附件**，不是记忆内容：只被 [`crate::review`] 一侧写、只给 §7.3 的
//! 重跑队列读。登记进 canonical 表清单会同时改到 `hub::rows` 的白名单/`INSERT_ORDER` 与
//! `hub::restore` 的整替面——那是另一条工单的面。归属守卫（`schema.rs`）因此对它是
//! **无登记**状态，本文件自己钉死写入面：唯一的写入口是 [`replace_item_verdicts`]。
//!
//! ## 迁移
//!
//! 与 `rc-store::ddl` 的同一套写法：一串 `CREATE ... IF NOT EXISTS`，每次写入前重放一遍
//! （`apply` 的重放语义），旧库开库即自愈，**不 bump `MEMORY_VERSION`**。DDL 住在本文件，
//! 是因为表名只在这里出现（守卫 2 的同一条：派生/附件表由唯一能命名它的模块建）。
//!
//! 列的语义：
//! - `item_id`：外键到 `review_item(id)`。`ON DELETE CASCADE` 与该库既有外键同一条语义
//!   ——item 被删（整替/清理）时它的逐条判决一并退场，不留悬空引用；
//! - `pass_id`/`turn_uid`：该条草稿属于哪个 pass、哪一轮；
//! - `ref`/`draft_index`：载荷内的草稿引用（`f1`…）与它在 `items[]` 里的下标（0 起）；
//! - `side`：`proposer`/`challenger` 是两次调用的原判，`final` 是裁决后的结果；
//! - `verdict`：`pass`/`reject`/`uncertain`（被证据闸降级的条目落 `uncertain`）；
//! - `code`：模型的 code（或 `final` 裁决出来的 code）；`pass`/`uncertain` 常为空；
//! - `evidence`：模型给的那段原文，**原样**；`final` 行与无证据需求时为空；
//! - `evidence_state`：闭集 `ok`/`missing`/`not_in_quote`/`none`——`none` 表示这一行没有
//!   证据需求（`uncertain` 可以不带证据；`final` 行恒 `none`）；
//! - `prompt_version`/`model`/`created_at_ms`：这次审核的版本、模型与落库钟。
//!
//! 写入是**整条 item 覆盖**：一次审核的逐条判决是一个整体，重跑同一 item（隔离项复审）
//! 用新的一批替掉旧的，不堆历史（历史在 `review_decision` 与 artifact 里）。这样"每个
//! (ref, side) 恰一行"是结构性的，§7.3 不必自己去重。

use crate::review::ReviewVerdict;
use crate::MemError;
use rc_store::MemoryHandle;
use rusqlite::{params_from_iter, TransactionBehavior};

/// 这张表的 DDL（幂等，每次写入前重放一遍）。
///
/// `pass_id` 的闭集只覆盖五个抽取 pass（`naming`/`fact`/`event`/`time`/`rows`），
/// 比 `review_item` 的九值少 `state`/`report`/`reuse`/`lesson`；`side`/`verdict`/`evidence_state`
/// 是本文档上文列的闭集。两张索引：按 item 顺序读（§7.3 的 `item_verdicts`）、按判决面筛
/// （"final=uncertain 的草稿清单"）。
const VERDICT_SQL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS review_verdict(
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        item_id INTEGER NOT NULL REFERENCES review_item(id) ON DELETE CASCADE,
        pass_id TEXT NOT NULL CHECK(pass_id IN ('naming','fact','event','time','rows')),
        turn_uid TEXT NOT NULL DEFAULT '',
        \"ref\" TEXT NOT NULL,
        draft_index INTEGER NOT NULL,
        side TEXT NOT NULL CHECK(side IN ('proposer','challenger','final')),
        verdict TEXT NOT NULL CHECK(verdict IN ('pass','reject','uncertain')),
        code TEXT,
        evidence TEXT,
        evidence_state TEXT NOT NULL
            CHECK(evidence_state IN ('ok','missing','not_in_quote','none')),
        prompt_version TEXT NOT NULL DEFAULT '',
        model TEXT NOT NULL DEFAULT '',
        created_at_ms INTEGER NOT NULL)",
    "CREATE INDEX IF NOT EXISTS review_verdict_by_item
        ON review_verdict(item_id, id)",
    "CREATE INDEX IF NOT EXISTS review_verdict_by_ruling
        ON review_verdict(side, verdict, item_id)",
];

/// 一条判决属于哪一侧。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VerdictSide {
    Proposer,
    Challenger,
    /// 裁决后的结果（`decide_pair` 的胜出侧），每条草稿恰一行。
    Final,
}

impl VerdictSide {
    pub fn slug(self) -> &'static str {
        match self {
            VerdictSide::Proposer => "proposer",
            VerdictSide::Challenger => "challenger",
            VerdictSide::Final => "final",
        }
    }

    pub fn parse(slug: &str) -> Option<Self> {
        match slug {
            "proposer" => Some(VerdictSide::Proposer),
            "challenger" => Some(VerdictSide::Challenger),
            "final" => Some(VerdictSide::Final),
            _ => None,
        }
    }
}

/// 证据闸对一条判决的裁定。闭集；`Absent` 的落库词是 `none`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EvidenceState {
    /// 证据在场且落在该条草稿的 `quote` 内。
    Ok,
    /// `evidence` 缺失、null 或空串（程序侧降级原因 `evidence_missing`）。
    Missing,
    /// `evidence` 不是该条 `quote` 的子串（程序侧降级原因 `evidence_not_in_quote`）。
    NotInQuote,
    /// 这一行没有证据需求：`uncertain` 允许不带证据，`final` 行恒此值。
    Absent,
}

impl EvidenceState {
    pub fn slug(self) -> &'static str {
        match self {
            EvidenceState::Ok => "ok",
            EvidenceState::Missing => "missing",
            EvidenceState::NotInQuote => "not_in_quote",
            EvidenceState::Absent => "none",
        }
    }

    pub fn parse(slug: &str) -> Option<Self> {
        match slug {
            "ok" => Some(EvidenceState::Ok),
            "missing" => Some(EvidenceState::Missing),
            "not_in_quote" => Some(EvidenceState::NotInQuote),
            "none" => Some(EvidenceState::Absent),
            _ => None,
        }
    }
}

/// 一条待落库的逐条判决。`evidence` 是模型给的原文（不 trim、不改写）；`None` 与空串
/// 是两件事，本层原样落，读侧也原样回。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewVerdict {
    pub pass_id: String,
    pub turn_uid: String,
    /// 载荷内的草稿引用（`f1`…）。
    pub reference: String,
    /// 该草稿在载荷 `items[]` 里的下标（0 起）。
    pub draft_index: i64,
    pub side: VerdictSide,
    pub verdict: ReviewVerdict,
    pub code: Option<String>,
    pub evidence: Option<String>,
    pub evidence_state: EvidenceState,
    pub prompt_version: String,
    pub model: String,
}

/// 一条落库后的逐条判决（只读视图）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictRow {
    pub id: i64,
    pub item_id: i64,
    pub pass_id: String,
    pub turn_uid: String,
    pub reference: String,
    pub draft_index: i64,
    pub side: VerdictSide,
    pub verdict: ReviewVerdict,
    pub code: Option<String>,
    pub evidence: Option<String>,
    pub evidence_state: EvidenceState,
    pub prompt_version: String,
    pub model: String,
    pub created_at_ms: i64,
}

/// 查询列序；`row_of` 的下标与它逐位对应。
const ROW_COLUMNS: &str = "id, item_id, pass_id, turn_uid, \"ref\", draft_index, side, verdict, \
                           code, evidence, evidence_state, prompt_version, model, created_at_ms";

fn invalid(what: &str) -> MemError {
    MemError::Sql(format!("review_verdict: invalid {what}"))
}

fn table_exists(m: &MemoryHandle) -> Result<bool, MemError> {
    let n: i64 = m.conn().query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='review_verdict'",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// 幂等地补表。只在写路径调（只读副本没有写入面）。
fn ensure(m: &MemoryHandle) -> Result<(), MemError> {
    for sql in VERDICT_SQL {
        m.conn().execute_batch(sql)?;
    }
    Ok(())
}

fn check(row: &NewVerdict) -> Result<(), MemError> {
    if row.reference.trim().is_empty() {
        return Err(invalid("ref"));
    }
    if row.draft_index < 0 {
        return Err(invalid("draft_index"));
    }
    Ok(())
}

/// 用一批逐条判决**整条覆盖**该 item 的旧判决（同一事务：先删后插）。
///
/// 返回落库行数。空批也是合法输入（清空该 item 的逐条判决），返回 0。
/// 写失败原样上抛 `MemError`：调用方（`rc-distill::review::run`）把它折成 `RunError::Store`，
/// 不吞。
pub fn replace_item_verdicts(
    m: &MemoryHandle,
    item_id: i64,
    rows: &[NewVerdict],
    now_ms: i64,
) -> Result<usize, MemError> {
    if m.is_read_only() {
        return Err(MemError::ReadOnly);
    }
    for row in rows {
        check(row)?;
    }
    // 补表在事务外：DDL 自带 IF NOT EXISTS，重放一次与"开库迁移"同一个动作。
    ensure(m)?;
    let tx = rusqlite::Transaction::new_unchecked(m.conn(), TransactionBehavior::Immediate)?;
    tx.execute("DELETE FROM review_verdict WHERE item_id=?1", [item_id])?;
    {
        let mut st = tx.prepare_cached(
            "INSERT INTO review_verdict(
                item_id, pass_id, turn_uid, \"ref\", draft_index, side, verdict, code,
                evidence, evidence_state, prompt_version, model, created_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        )?;
        for row in rows {
            st.execute(rusqlite::params![
                item_id,
                row.pass_id,
                row.turn_uid,
                row.reference,
                row.draft_index,
                row.side.slug(),
                row.verdict.slug(),
                row.code,
                row.evidence,
                row.evidence_state.slug(),
                row.prompt_version,
                row.model,
                now_ms,
            ])?;
        }
    }
    tx.commit()?;
    Ok(rows.len())
}

fn row_of(row: &rusqlite::Row<'_>) -> Result<VerdictRow, MemError> {
    let side: String = row.get(6)?;
    let verdict: String = row.get(7)?;
    let evidence_state: String = row.get(10)?;
    Ok(VerdictRow {
        id: row.get(0)?,
        item_id: row.get(1)?,
        pass_id: row.get(2)?,
        turn_uid: row.get(3)?,
        reference: row.get(4)?,
        draft_index: row.get(5)?,
        // 读不出闭集不是"没有判决"：把坏行折成某一格会让 §7.3 的重跑队列拿错清单。
        side: VerdictSide::parse(&side)
            .ok_or_else(|| MemError::Sql("review_verdict: unknown side".to_string()))?,
        verdict: ReviewVerdict::parse(&verdict)
            .ok_or_else(|| MemError::Sql("review_verdict: unknown verdict".to_string()))?,
        code: row.get(8)?,
        evidence: row.get(9)?,
        evidence_state: EvidenceState::parse(&evidence_state)
            .ok_or_else(|| MemError::Sql("review_verdict: unknown evidence_state".to_string()))?,
        prompt_version: row.get(11)?,
        model: row.get(12)?,
        created_at_ms: row.get(13)?,
    })
}

/// 该 item 的逐条判决（只读），按 `id` 升序 = 落库顺序（两档在前、`final` 在后）。
pub fn item_verdicts(m: &MemoryHandle, item_id: i64) -> Result<Vec<VerdictRow>, MemError> {
    item_verdicts_filtered(m, item_id, None, None)
}

/// 同 [`item_verdicts`]，多两道可选过滤（§7.3 取"final=uncertain"或"某一侧的全部判决"）。
///
/// 表还没建（只读副本/从未审过）⇒ 空集而不是 `Sql` 错误：没有这张表就是"这个库里一条
/// 逐条判决都没有"，与空表同一个答案。
pub fn item_verdicts_filtered(
    m: &MemoryHandle,
    item_id: i64,
    side: Option<VerdictSide>,
    verdict: Option<ReviewVerdict>,
) -> Result<Vec<VerdictRow>, MemError> {
    if !table_exists(m)? {
        return Ok(Vec::new());
    }
    let mut args: Vec<rusqlite::types::Value> = vec![item_id.into()];
    let mut sql = format!("SELECT {ROW_COLUMNS} FROM review_verdict WHERE item_id=?1");
    if let Some(side) = side {
        args.push(side.slug().to_string().into());
        sql.push_str(&format!(" AND side=?{}", args.len()));
    }
    if let Some(verdict) = verdict {
        args.push(verdict.slug().to_string().into());
        sql.push_str(&format!(" AND verdict=?{}", args.len()));
    }
    sql.push_str(" ORDER BY id");
    let mut st = m.conn().prepare(&sql)?;
    let mut rows = st.query(params_from_iter(args.iter()))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row_of(row)?);
    }
    Ok(out)
}
