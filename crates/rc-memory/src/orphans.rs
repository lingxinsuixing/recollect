//! 段 0：连带抑制前置扫描。
//!
//! **单挂点**：每轮 cycle 的**第一件事**，挂在 `hub::run_cycle_once` 顶部（`out` 建好之后、
//! `select_batch` 之前），与有没有活行、有没有推导批**无关**；失败即整轮终止
//! （`CycleExit::Fault`）。覆盖面是三死法 × 引用行 + 任一端死 × 边。
//! 它原先挂在 `rc_derive::derive_pass` 里，而 `rc-distill::derive::run_derive` 在
//! `batch.is_empty()` 上提前返回（不调 backend、不推游标），hub 又在事实批契约丢弃那一轮整个
//! 不跑推导 ⇒ 空闲轮次与契约丢弃轮次段 0 根本不执行；更糟的是段 0 退场的行立刻退出活行集合
//! ⇒ **它把自己饿死**（池子清空那一轮起永不再执行，此后新死的依据再也扫不到）。
//! 上提之后自锁**结构上不成立**：执行条件不再是"有没有活行"。
//! 依据行**缺失**按 fail-closed 处理（与 `reader::dead_by_id_expr` 注释同源理由：
//! 内容寻址是公共 API，幽灵 id 是合法形状）。单独计数 `missing_targets`（与
//! `dangling_attestations` 同族——可观察、可升读数）。
//!
//! **为什么不让 `retire` / `attest` 在写入侧校验行存在**（守卫 13 的反向论证）：
//! 那会关掉 `dangling_attestations` 的唯一生产者，把"悬空表态"从可观察变成不可能，
//! 而架构 §6 说的是它**存在**、要被人看见。这里同样的反向论证用在段 0：把幽灵 id
//! 当 dead 处理，等于**主动让那一类幽灵 id 退场**——而不是把幽灵 id 屏蔽掉。
//!
//! **幂等**靠两条：① 已退场的行不再进扫描集合（两族各一道 `NOT EXISTS`，见下面
//! `suppress_orphans` 里那两处）⇒ 第二轮起不重发 `retire`，账本既不增行、
//! `created_at_ms` 也**冻结在第一次退场那一轮**（`retire` 的 `ON CONFLICT DO UPDATE`
//! 会把它改写成当轮 `now_ms`，不重发才冻得住）；② `ON CONFLICT` 本身仍然兜底：
//! 万一同一行被两条路径各退一次，重写的是同一条，不长出新行。
//!
//! **① 的适用范围是"那条账还没被结掉"**：`resolve_retirement` 是公开写入口，账一结掉这一行
//! 就重新落进扫描集合，再死一次会被重新计一次、`created_at_ms` 也重新起算。**所以"每轮只报
//! 一次"与"冻结"说的是未解决的账，不是这张表**——别把它读成一条永久保证。

use crate::keys::{MemKey, MemTable};
use crate::reader::{dead_by_id_expr, retirement_pred_on};
use crate::writers_c::{retire, retire_edge};
use rc_store::MemoryHandle;
use rusqlite::params;

/// 一次段 0 的扫描报告。
///
/// 三格全部独立计数（不汇总、不折算率——率是显示层除法，守卫 11 同族）。
/// `from_provenance` 含 `missing_targets`；后者是前者的子集。
///
/// **三格数的是"本轮触发了几次退场"，也就是**状态变化了几行**：两族的 SELECT 都带
/// `NOT EXISTS (… retirement …)`，一条引用行 / 一条边自己拿到未解决的退场账之后就不再进扫描
/// 集合 ⇒ 同一条**在它的账被结掉之前只报一次**，第二轮起那一格是 0（改口径前它每轮各报一次，
/// 于是运维看见的是一个恒定不降的数，而恒定读数按 P-44 算最坏失败）。这一口径由
/// `crates/rc-memory/tests/orphans.rs` 那两条"跑两遍"的用例钉住（族① 与族② 各一条）。
/// 这三格在本阶段**只有 `orphans.rs` 的单测读者**（不升 `CycleOut`——`CycleOut.suppressed`
/// 那一格已有生产者，是**抽取**的读数：全树 `out.suppressed +=` 只有 `run_cycle_once` 里
/// `run_extract` 那一支一处，dedupe 的返回类型根本没有这个字段），所以这个语义此刻不咬人。
/// 条件文本住 `reader::retirement_pred_on` 这一个家，**不许**在 `orphans.rs` 里另抄一份
/// `resolved_at_ms IS NULL`（守卫 3）。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SuppressReport {
    /// `provenance` 行（`ref_kind != 'turn'` 且 `mem_table <> 'fact_link'`）被连带退场的
    /// 条数（族①）。第二个谓词是计数不许撒谎的执法处——边自己的 provenance 行归族② 管。
    pub from_provenance: usize,
    /// 边因任一端 dead 被连带退场的条数（族②）。
    pub from_edges: usize,
    /// 依据行**不存在**（fail-closed）被连带退场的条数。是 `from_provenance` 的子集。
    pub missing_targets: usize,
}

#[derive(Debug, PartialEq, Eq)]
enum TargetStatus {
    Alive,
    Dead,
    Missing,
}

fn target_status(m: &MemoryHandle, table: MemTable, id: &str)
    -> Result<TargetStatus, crate::MemError> {
    // 第一步：行是否存在？不存在 ⇒ missing（fail-closed）。
    let row_exists: bool = m.conn().query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {} WHERE id=?1)", table.prefix()),
        params![id], |r| r.get(0)
    )?;
    if !row_exists { return Ok(TargetStatus::Missing); }
    // 第二步：行存在 ⇒ `dead_by_id_expr` 三支（退场账 / 否决 / 被顶掉）。
    // 这一格**复用** reader 的条件文本（守卫 3：同一族条件文本只有一个家）。
    let is_dead: bool = m.conn().query_row(
        &format!("SELECT ({}) FROM {} WHERE id=?1", dead_by_id_expr(table), table.prefix()),
        params![id], |r| r.get(0)
    )?;
    Ok(if is_dead { TargetStatus::Dead } else { TargetStatus::Alive })
}

/// 段 0 主体。**每轮 cycle 开头调一次**，挂点在 `hub::run_cycle_once` 顶部（`out` 建好之后、
/// `extract::select_batch` 之前）：执行的充分条件是"cycle 跑起来了"，**不是**"这一轮有活行"。
/// 失败即整轮终止，类别串复用 `FAULT_STORE_WRITE`（与同函数里 `water::note_attempt`、
/// `cycle_failure` 的 `DistillError::Memory` / `Store` 两支同形）。
///
/// **为什么挂点在这一层而不是推导 pass 里**：挂推导 pass 时它有两道结构性漏——①
/// `rc-distill::derive::run_derive` 在 `batch.is_empty()` 上提前返回 ⇒ 空批轮次一次都不跑；
/// ② hub 在事实批契约丢弃那一轮整个不跑推导（`if !contract_discarded { … }`）⇒ 丢弃轮次也
/// 不跑。两道叠上就是"段 0 只在有活行的轮次跑"。更要命的是它**把自己饿死**：
/// `reader::rows_since` 走 `derive_pool_union()`（推导池 = 活行 − 推导行），而段 0 退场的行
/// 立刻退出活行集合 ⇒ 池子清空那一轮起就永远空批 ⇒ 此后新死的依据再也扫不到。
/// 上提到 cycle 开头之后，这三条都不再是执行条件。
///
/// 族①：`provenance WHERE ref_kind != 'turn' AND mem_table <> 'fact_link'` 的每一行
/// ⇒ 解析 (ref_kind, turn_uid) 为 (目标表, 目标 id) ⇒ `target_status` ⇒ Dead 或 Missing
/// ⇒ retire 这条引用行。**`mem_table <> 'fact_link'` 那半边是计数不许撒谎的执法处**：
/// 边自己的 provenance 行落在这一支里，而 `retire_citing_row` 写不出它的账（边不可指认句柄、
/// 裁定 3）⇒ 它归族② 管。
///
/// 族②：`fact_link` 每一行 ⇒ `target_status(from_id)` 或 `target_status(to_id)` 为 Dead / Missing
/// ⇒ `retire_edge(edge_id)`。
///
/// 两族共用同一把"死判据"的尺子（`target_status`），所以三死法 × 引用行 + 任一端死 × 边
/// 的所有变体都由一处判定。残值（`from_id` / `to_id` 里的表前缀解析不出 MemTable）跳过——
/// 库可能因 schema 升级留下旧字符串，段 0 不为这种残留报错（架构 §6 连带④ 同律：
/// 残值走低格，不上抛）。`fact_link` 的 `from_id` / `to_id` 两列**没有 CHECK**
/// （DDL 已核：带 CHECK 字面量集合的是 `relation` 那一列）⇒ 这一支是真能走到的。
///
/// **`ref_kind` 的残值支**：DDL 与迁移都带 `REF_KINDS` 那十个值
/// （`CHECK(ref_kind IN ('turn','card','fact','anchor','relation','state_assertion',
/// 'utterance','episode','attribute','mention'))`），而除 `'turn'` 外只有 `card`/`anchor`
/// 被 `MemTable::parse` 认得，其余七个（`fact`/`relation`/`state_assertion`/`utterance`/
/// `episode`/`attribute`/`mention`）正是残值支要接住的存量本体行 ⇒ 这一支守卫是真能走到的
/// （守卫本身留着无害，**别把它写成"恒不成立"**）。
pub fn suppress_orphans(m: &MemoryHandle, now_ms: i64) -> Result<SuppressReport, crate::MemError> {
    let mut report = SuppressReport::default();

    // ===== 族① =====
    // 第二个谓词（`mem_table <> 'fact_link'`）不是优化，是**计数不许撒谎**的执法处。
    // `writers_x::put_edges` 现在给每条新边插 **from 端、to 端与每条依据各一条** provenance 行
    //（全部 `mem_table='fact_link'`），而 `ref_kind` 是**各自被引事实表**的前缀 ⇒ 整族都落在
    // `ref_kind != 'turn'` 这一支里。但这类引用行要经
    // `retire_citing_row` 落账，而那一支要 `MemTable::parse("fact_link")`——边不可指认句柄
    //（裁定 3），枚举里没有它 ⇒ `None` ⇒ `return Ok(())`，**账本零行**，而调用方照样
    // `report.from_provenance += 1`。计数说"连带退场 N 条"、账本 0 行，且同一条边还被族② 的
    // `from_edges` 再记一次 ⇒ 两本账。
    //
    // 为什么选"SELECT 过滤"而不是"让 `retire_citing_row` 返回 bool、真写了账才计数"：这族
    // provenance 行的判死键仍只看 from 端那一条（`ref_kind` = from 端表前缀、`turn_uid` = `from_id`），
    // 而族② 的 from 端检查**逐字相同**
    //（`ref_kind` = from 端表前缀、`turn_uid` = `from_id`）⇒ 留着就是同一件事扫两遍，
    // 补一个 bool 只是把"扫两遍"变成"扫两遍但有一遍不计数"，判据仍是两份。
    // 那个 `p` 别名是**承重**的，不是排版：子查询里未限定的 `mem_table` 会绑到**内层
    // `retirement` 自己**，于是 `rt.mem_table=mem_table` 退化成自比较、这道过滤恒真。
    // 它**不产生任何编译诊断**，只是静默地少做了事——已退场的行照样每轮被重扫回去。
    let rows: Vec<(String, String, String, String)> = {
        let mut st = m.conn().prepare(&format!(
            "SELECT mem_table, mem_id, ref_kind, turn_uid FROM provenance p \
             WHERE ref_kind != 'turn' AND p.mem_table <> 'fact_link' \
               AND NOT EXISTS (SELECT 1 FROM retirement rt WHERE {})",
            retirement_pred_on("rt", "p.mem_table", "p.mem_id")))?;
        let got = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(st);
        got
    };
    for (citing_table, citing_id, ref_kind, target_id) in rows {
        // 残值守卫：`ref_kind` 解析不出 MemTable ⇒ 跳过。canonical 的 `ref_kind` 值域比
        // `MemTable` 宽（fact/attribute/relation/state_assertion/utterance/episode/mention），
        // 这些本体行 M3 才写入、本阶段不处理 ⇒ 残值支是真能走到的；低格、不上抛。
        let Some(target_table) = MemTable::parse(&ref_kind) else { continue };
        match target_status(m, target_table, &target_id)? {
            TargetStatus::Alive => continue,
            TargetStatus::Dead => {
                retire_citing_row(m, &citing_table, &citing_id, now_ms)?;
                report.from_provenance += 1;
            }
            TargetStatus::Missing => {
                retire_citing_row(m, &citing_table, &citing_id, now_ms)?;
                report.from_provenance += 1;
                report.missing_targets += 1;
            }
        }
    }

    // ===== 族② =====
    let rows: Vec<(String, String, String)> = {
        let mut st = m.conn().prepare(&format!(
            "SELECT id, from_id, to_id FROM fact_link \
             WHERE NOT EXISTS (SELECT 1 FROM retirement rt WHERE {})",
            retirement_pred_on("rt", "'fact_link'", "id")))?;
        let got = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(st);
        got
    };
    for (edge_id, from_key, to_key) in rows {
        let endpoints = (parse_endpoint(&from_key), parse_endpoint(&to_key));
        let (Some((from_table, from_id)), Some((to_table, to_id))) = endpoints else { continue };
        if matches!(target_status(m, from_table, &from_id)?, TargetStatus::Alive)
            && matches!(target_status(m, to_table, &to_id)?, TargetStatus::Alive) {
            continue;
        }
        retire_edge(m, &edge_id, "basis_dead", now_ms)?;
        report.from_edges += 1;
    }

    Ok(report)
}

/// `"<table>:<id>"` → `(MemTable, id)`；表前缀认不出 ⇒ `None`（残值走低格）。
fn parse_endpoint(key: &str) -> Option<(MemTable, String)> {
    let (table, id) = key.split_once(':')?;
    Some((MemTable::parse(table)?, id.to_string()))
}

/// `citing_table` 是字面量字符串，构造 `MemKey` 时需要 `MemTable` 枚举——失败则跳过。
/// 残值的去处与族② from_kind/to_kind 残值同律（低格、不上抛）。
fn retire_citing_row(m: &MemoryHandle, citing_table: &str, citing_id: &str, now_ms: i64)
    -> Result<(), crate::MemError> {
    let Some(table) = MemTable::parse(citing_table) else { return Ok(()) };
    let key = MemKey { table, id: citing_id.to_string() };
    retire(m, &key, "basis_dead", now_ms)
}
