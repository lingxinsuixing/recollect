//! 锚合并方案二（D8，2026-11-05 拍板）：**不重键、零搬运、读侧 from→to 指向解析**。
//!
//! 与 [`crate::rebind`]（重键路径，本单禁碰）并存：方案二合并两格锚时**不动任何
//! mention/fact/state 行**——旧锚保留、`anchor_merge(from_id,to_id,…)` 一行就是指向、
//! 读侧解析时沿链走到终点。这样合并可逆（删指向行即回滚），且没有内容寻址重键的
//! 不可逆风险（`merge.rs` 档头写明的那笔账在这里不存在）。
//!
//! 三块职责：
//! 1. **解析**（[`resolve`]）：输入 anchor id——锚活着直接返回；否则沿 `anchor_merge`
//!    链走到活锚终点。**防环**（自环/多环/链式都终止，visited 集 + 迭代，不递归）。
//!    **读数诚实**：解析失败是 [`RedirectResolve::Broken`] 而不是 Missing——消费方对
//!    Broken 回落到旧锚自己的行（可见），绝不静默成"没有"。
//! 2. **写侧**（[`merge_redirect`]）：事务内 ①校验（from≠to、两锚在场、from 活着、
//!    写后不成环）②写 `anchor_merge` 指向行（`WHERE NOT EXISTS` 兜底）③旧锚
//!    `status='merged'`（既有状态值，闸与目录已不认它为活跃）④旧锚正名并入 to 锚
//!    的 `alias`（**来源三格**：`valid_from_ms/valid_to_ms/source_turn_uid`——合并是
//!    程序动作、没有出处轮，三格按 NULL 语义写 `valid_from_ms=NULL`：与
//!    `rebind::merge_alias` 同一条"不许伪造日期"的纪律，`display_name` 按
//!    `COALESCE(valid_from_ms,0) DESC` 取名，NULL 保证幸存锚自己的名字照旧赢）
//!    ⑤`memory_op(op='MERGE')` 台账。
//! 3. **读数面**（[`broken_redirects`]）：哪些指向行解析不达活锚——hub 合并命令输出，
//!    解析失败在这里可见、可运维。
//!
//! 禁碰清单：`merge.rs`/`rebind.rs` 的重键路径一行不改（两条路径并存，后续合并默认走
//! 指向）；`identity.rs`/`keys.rs`/DDL 不动（`anchor_merge` 表已存在）。
use crate::MemError;
use rc_store::MemoryHandle;
use rusqlite::params;

/// 解析结果（读数诚实：Broken 不是 Missing，两者的消费形状在档头写明）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectResolve {
    /// 锚活着（`active`/`provisional`），id 即终点，零跳。
    Alive(String),
    /// 沿链解析到活锚终点（`hops >= 1`；A→B、B→C ⇒ A 解析到 C）。
    Merged { from: String, to: String, hops: usize },
    /// 解析失败：链断在某处（`reason` ∈ `missing_to` / `cycle` / `no_redirect`，
    /// `stopped_at` 是走不下去的那一格）。消费方按"旧锚自己的行"渲染——可见，不静默。
    Broken { from: String, stopped_at: String, reason: &'static str },
    /// 库里没有这个锚。
    Missing,
}

impl RedirectResolve {
    /// 消费方要的"可用终点"：Alive/Merged → 终点；Broken → **原 id 自己**（回落旧锚
    /// 自己的行=可见不静默）；Missing → 空串（调用方按"查无"消费，与既有行为同形）。
    pub fn terminal_or_self(&self) -> &str {
        match self {
            Self::Alive(id) => id,
            Self::Merged { to, .. } => to,
            Self::Broken { from, .. } => from,
            Self::Missing => "",
        }
    }
}

/// 锚的状态；不存在给 `None`。
fn anchor_status(conn: &rusqlite::Connection, id: &str) -> Result<Option<String>, MemError> {
    match conn.query_row("SELECT status FROM anchor WHERE id=?1", params![id], |r| r.get(0)) {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(MemError::from(e)),
    }
}

/// 同一 from 的多条指向行里取**最新一条**（`anchor_merge.id` 自增 ⇒ id 最大即最新）。
/// 多条并存是运维手误的形状：解析取一条确定性的终点，而不是在分叉上随机游走。
pub(crate) fn latest_redirect(
    conn: &rusqlite::Connection,
    from: &str,
) -> Result<Option<String>, MemError> {
    match conn.query_row(
        "SELECT to_id FROM anchor_merge WHERE from_id=?1 ORDER BY id DESC LIMIT 1",
        params![from],
        |r| r.get::<_, String>(0),
    ) {
        Ok(to) => Ok(Some(to)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(MemError::from(e)),
    }
}

fn is_alive(status: &str) -> bool {
    status == "active" || status == "provisional"
}

/// 解析一个 anchor id（读侧唯一的解析入口；档头三条职责的第 1 条）。
pub fn resolve(m: &MemoryHandle, id: &str) -> Result<RedirectResolve, MemError> {
    let conn = m.conn();
    let Some(status) = anchor_status(conn, id)? else {
        return Ok(RedirectResolve::Missing);
    };
    if is_alive(&status) {
        return Ok(RedirectResolve::Alive(id.to_string()));
    }
    // 链式行走：visited 集防环（自环 A→A、三环 A→B→C→A 都在第二次见到同一节点时终止），
    // 迭代不递归——链长度上限就是库里指向行数，不存在栈溢出或死循环。
    let mut visited: Vec<String> = vec![id.to_string()];
    let mut current = id.to_string();
    let mut hops = 0usize;
    loop {
        let Some(to) = latest_redirect(conn, &current)? else {
            return Ok(RedirectResolve::Broken {
                from: id.to_string(),
                stopped_at: current,
                reason: "no_redirect",
            });
        };
        if visited.contains(&to) {
            return Ok(RedirectResolve::Broken {
                from: id.to_string(),
                stopped_at: to,
                reason: "cycle",
            });
        }
        match anchor_status(conn, &to)? {
            None => {
                return Ok(RedirectResolve::Broken {
                    from: id.to_string(),
                    stopped_at: to,
                    reason: "missing_to",
                });
            }
            Some(s) if is_alive(&s) => {
                return Ok(RedirectResolve::Merged {
                    from: id.to_string(),
                    to,
                    hops: hops + 1,
                });
            }
            Some(_) => {
                visited.push(to.clone());
                current = to;
                hops += 1;
            }
        }
    }
}

/// 写侧拒绝面的闭集词（hub 打印与测试逐字同源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectError {
    SameAnchor,
    /// from 或 to 锚不存在。
    Missing,
    /// from 不在 `active`/`provisional`（含已 merged——二次合并显式拒绝，不留第二行指向）。
    FromNotAlive,
    /// 写后成环：to 沿现有链解析回 from。
    Cycle,
    ReadOnly,
    Store(String),
}

impl std::fmt::Display for RedirectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SameAnchor => f.write_str("same_anchor"),
            Self::Missing => f.write_str("missing_anchor"),
            Self::FromNotAlive => f.write_str("from_not_alive"),
            Self::Cycle => f.write_str("would_cycle"),
            Self::ReadOnly => f.write_str("read_only"),
            Self::Store(s) => write!(f, "{s}"),
        }
    }
}

impl From<MemError> for RedirectError {
    fn from(e: MemError) -> Self {
        Self::Store(e.to_string())
    }
}

impl From<rusqlite::Error> for RedirectError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(MemError::from(e).to_string())
    }
}

/// 一次指向合并的读数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedirectReport {
    pub from_id: String,
    pub to_id: String,
    pub run_id: String,
    /// 正名并进 to 锚 alias 真正新插入的行数（撞名 `INSERT OR IGNORE` 时为 0）。
    pub aliases_merged: usize,
}

/// 指向合并的**写侧**（档头第 2 条）。整笔一个事务：指向行、退场章、alias 并入、
/// 台账同生共死；任一步失败回滚整笔，不留半个合并。
///
/// **零搬运**：任何 mention/fact/state 行都不在这个函数的写集里（测试逐字节钉）。
pub fn merge_redirect(
    m: &MemoryHandle,
    from: &str,
    to: &str,
    reason: &str,
    actor: &str,
    now_ms: i64,
) -> Result<RedirectReport, RedirectError> {
    if m.is_read_only() {
        return Err(RedirectError::ReadOnly);
    }
    if from == to {
        return Err(RedirectError::SameAnchor);
    }
    let conn = m.conn();
    let Some(from_status) = anchor_status(conn, from)? else {
        return Err(RedirectError::Missing);
    };
    if !is_alive(&from_status) {
        return Err(RedirectError::FromNotAlive);
    }
    if anchor_status(conn, to)?.is_none() {
        return Err(RedirectError::Missing);
    }
    // 写后不成环：to 沿现有链走，回到 from 就拒绝（A→B、B→A 那种）。
    // to 已 merged 是合法形状（A→B、B→C ⇒ A 解析到 C），链式行走天然覆盖。
    let mut visited: Vec<String> = vec![from.to_string()];
    let mut current = to.to_string();
    loop {
        if visited.contains(&current) {
            return Err(RedirectError::Cycle);
        }
        visited.push(current.clone());
        match latest_redirect(conn, &current)? {
            None => break,
            Some(next) => current = next,
        }
    }

    // from 的正名（进 to 的 alias；正名在 old row 上，零搬运原则下不改它本身）。
    let from_canonical: String = conn
        .query_row("SELECT canonical FROM anchor WHERE id=?1", params![from], |r| r.get(0))
        .map_err(RedirectError::from)?;

    let run_id = format!("redirect-{now_ms}");
    let tx = conn.unchecked_transaction()?;
    // ① 指向行：`WHERE NOT EXISTS` 兜底（同一 from 的**不同** to 允许并存——最新一条赢，
    //    见 [`latest_redirect`]；同一 (from,to) 不长第二行）。
    tx.execute(
        "INSERT INTO anchor_merge(from_id, to_id, run_id, reason, created_at_ms)
         SELECT ?1, ?2, ?3, ?4, ?5
          WHERE NOT EXISTS(SELECT 1 FROM anchor_merge WHERE from_id=?1 AND to_id=?2)",
        params![from, to, run_id, reason, now_ms],
    )?;
    // ② 退场章：既有状态值，闸/目录/发现面已不认它为活跃；不物理删。
    tx.execute(
        "UPDATE anchor SET status='merged', updated_at_ms=?1 WHERE id=?2",
        params![now_ms, from],
    )?;
    // ③ 正名并入 to 锚的 alias：来源三格按"程序动作没有出处轮"的语义写
    //    （valid_from_ms=NULL ⇒ 幸存锚自己的名字照旧赢，见档头）。
    let aliases_merged = tx.execute(
        "INSERT OR IGNORE INTO alias(anchor_id, alias, valid_from_ms, valid_to_ms, source_turn_uid)
         VALUES(?1, ?2, NULL, NULL, NULL)",
        params![to, from_canonical],
    )?;
    // ④ 台账：合并发生了就必须有账（与 merge.rs 同一条纪律）。
    tx.execute(
        "INSERT INTO memory_op(run_id, op, mem_table, mem_id, before_json, after_json,
                               reason, actor, created_at_ms)
         VALUES(?1, 'MERGE', 'anchor', ?2, NULL, ?3, ?4, ?5, ?6)",
        params![run_id, from, format!("redirect:{to}"), reason, actor, now_ms],
    )?;
    tx.commit()?;
    Ok(RedirectReport {
        from_id: from.to_string(),
        to_id: to.to_string(),
        run_id,
        aliases_merged,
    })
}

/// 一条解析不达活锚的指向行（读数面；`reason` 词表与 [`RedirectResolve::Broken`] 同源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokenRedirect {
    pub from_id: String,
    pub stopped_at: String,
    pub reason: &'static str,
}

/// 解析失败读数（档头第 3 条）：扫描面 = **全部指向行的 from ∪ 全部退场锚**——
/// "merged 却没有指向行"（坏数据）、"指向行解析不达"（断链）、"活锚拖着一条死指针"
/// （指向已被物理删的 to）三种都要可见。逐个解析，`Broken` 的留下；`retired` 只在
/// 恰好带指向行时才进（那是它自己的账）。hub 合并命令把这份清单带上输出。
pub fn broken_redirects(m: &MemoryHandle) -> Result<Vec<BrokenRedirect>, MemError> {
    let conn = m.conn();
    let mut st = conn.prepare(
        "SELECT DISTINCT from_id FROM anchor_merge \
         UNION SELECT id FROM anchor WHERE status='merged' \
         ORDER BY 1")?;
    let ids: Vec<String> = st
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::new();
    for id in ids {
        if let RedirectResolve::Broken { stopped_at, reason, .. } = resolve(m, &id)? {
            out.push(BrokenRedirect { from_id: id, stopped_at, reason });
        }
    }
    Ok(out)
}

/// 给定一对 (from,to) 的 A/B/C 分类（**只复用 merge.rs 的发现/分类思想**——词面精确判定、
/// 不做相似度推断；不 import 也不改它）。dry-run 的诊断面：这一对今天按旧路径会落哪一档。
///
/// - 同 kind、同 scope、名字精确相连（正名归一相等，或 from 正名已在 to 的 alias 词表里）
///   ⇒ `A`；
/// - 跨 kind ⇒ `B`（重新命名是抽取线的事）；
/// - 其余（scope 不同 / 名字不相连）⇒ `C`。
pub fn classify_pair(m: &MemoryHandle, from: &str, to: &str) -> Result<(&'static str, &'static str), MemError> {
    let conn = m.conn();
    let row = |id: &str| -> Result<Option<(String, String, String, String)>, MemError> {
        match conn.query_row(
            "SELECT kind, canonical, scope_kind, scope_key FROM anchor WHERE id=?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ) {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(MemError::from(e)),
        }
    };
    let (Some(f), Some(t)) = (row(from)?, row(to)?) else {
        return Ok(("missing", "missing_anchor"));
    };
    let norm = |s: &str| s.trim().to_lowercase();
    if f.0 != t.0 {
        return Ok(("B", "cross_kind"));
    }
    if (f.2, f.3) != (t.2, t.3) {
        return Ok(("C", "scope_mismatch"));
    }
    // 名字相连：from 正名等于 to 正名，或已是 to 的别名（含此前合并并进去的）。
    let linked = norm(&f.1) == norm(&t.1) || {
        let mut st = conn.prepare(
            "SELECT 1 FROM alias WHERE anchor_id=?1 AND alias=?2 LIMIT 1",
        )?;
        st.exists(params![to, f.1])?
    };
    if linked {
        Ok(("A", "same_kind_name_match"))
    } else {
        Ok(("C", "no_name_link"))
    }
}
