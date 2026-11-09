//! C（中心）的写入口：判定族里"形态"那一半——verdict、退场账、被取代指针。
//! 判定族不可重算（G4 的那条例外），所以这一侧的每一次落库形状都要被点名：
//! 解除退场只有 `resolved_at_ms` 一种写法，不新增"反悔"表（守卫 11 ⑤）。

use crate::keys::{AttentionKind, MemKey, MemTable, Verdict};
use crate::MemError;
use rc_store::MemoryHandle;

/// 单行更新的执法（`rowcount != 1` 即红这三个执法点之一）：
/// 0 = 那个句柄在库里不存在，>1 = 主键形状被人改坏 —— 两种都不是"成功且什么都没发生"。
fn exactly_one(n: usize, target: &MemKey) -> Result<(), MemError> {
    match n {
        1 => Ok(()),
        0 => Err(MemError::NotFound(target.clone())),
        n => Err(MemError::Sql(format!(
            "single-row write touched {n} rows for {}", target.key_string()))),
    }
}

/// C 能落 verdict 的只有 `card`：`anchor` 没有这一列（canonical DDL），
/// 指向它的句柄不是"写坏了"而是"根本没有这个形态"，所以报 NotFound 而不是 Sql。
fn fact_table(target: &MemKey) -> Result<MemTable, MemError> {
    if target.table == MemTable::Anchor {
        return Err(MemError::NotFound(target.clone()));
    }
    Ok(target.table)
}

/// **唯一**的一条 `SET verdict`（spec §6 S4 的静态闸门）：两条权限的差别落在**参数**上，
/// 不落在语句上。
/// - `from = None` ⇒ 全量改写（audit 的降级权，任意 `to` 都写得下）；
/// - `from = Some(v)` ⇒ 单向 CAS，只改 `verdict` 恰为 `v` 的那些行（review 的晋升权），
///   对不上就是 0 行——这是**读数**，不是错误。
///
/// 多一条 `SET verdict` 语句的那天，"verdict 只有一个写入口"就只剩注释在维持；
/// `grep -n "UPDATE .*SET verdict" crates/*/src`（只数 SQL 语句，不数注释）今天只命中下面这一处。
/// `fact_table` 在这里判一次：`anchor` 没有这一列 ⇒ `NotFound`（"没有这个形态"，
/// 不是"写坏了"）。
fn update_verdict(m: &MemoryHandle, target: &MemKey, from: Option<Verdict>, to: Verdict)
    -> Result<usize, MemError> {
    let table = fact_table(target)?;
    let n = m.conn().execute(
        &format!(
            "UPDATE {} SET verdict=?1 WHERE id=?2 AND (?3 IS NULL OR verdict=?3)",
            table.prefix()),
        // `from = None` 绑成 SQL NULL ⇒ `?3 IS NULL` 恒真 ⇒ 退化成全量改写。
        // 一个占位符同时承载两种权限，CAS 的 "from" 因此没有第二份真源。
        rusqlite::params![to.as_str(), target.id, from.map(|v| v.as_str())])?;
    Ok(n)
}

pub fn set_verdict(m: &MemoryHandle, target: &MemKey, verdict: Verdict, now_ms: i64)
    -> Result<(), MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    // 时钟在这里没有落点：verdict 是列，DDL 没给它时间戳同伴列（改 DDL 不属于本任务）。
    // 形参位保留是刻意的——阶段 2 的复核账要盖时间，那时它有去处，而不是现在删掉再改签名。
    let _ = now_ms;
    let n = update_verdict(m, target, None, verdict)?;
    exactly_one(n, target)
}

/// `card_verdict_event.source` 的闭集（两个拼写家）。DDL 的 CHECK 是同一份词的 schema
/// 落点，读侧（`issues::verdict_source_counts`）拿这两个常量当绑定参数——任何一处手写
/// 第三个字面量，桶就会在某次改名之后各数各的。
pub const VERDICT_SOURCE_AUDIT: &str = "audit";
pub const VERDICT_SOURCE_REVIEW_APPLY: &str = "review_apply";

/// 追加型背书账的一行：`(card_id, verdict, source, at_ms, norm_version)`。
/// **只追加、不覆盖**——同一张卡被盖几次就是几行，读侧按 `max(id)` 认最新一次，
/// 历史因此留得住。`source` 闭集之外的值由 DDL 的 CHECK 拒（`Sql`），这里不设第二套校验。
fn append_verdict_event(tx: &rusqlite::Transaction<'_>, card_id: &str, verdict: Verdict,
                        source: &'static str, now_ms: i64) -> Result<(), MemError> {
    tx.execute(
        "INSERT INTO card_verdict_event(card_id, verdict, source, at_ms, norm_version)
         VALUES(?1,?2,?3,?4,?5)",
        rusqlite::params![card_id, verdict.as_str(), source, now_ms,
                          rc_store::capability::NORM_VERSION as i64])?;
    Ok(())
}

/// 带**来源**的落笔：先改 `card.verdict`，同一事务里再追加一条背书事件。
///
/// 与 [`set_verdict`] 的区别只在"记不记来源"这一个参数上：审计侧（audit）走这一支，
/// 把"这一次是 audit 盖的"写进事件表；不带来源的 [`set_verdict`] 保持原样、**不写事件**
/// ——否则来源会退化成噪音，"谁盖的"这一问又回到不可追溯。
///
/// **事务边界**：本函数自己开一个事务（与 `writers_rows` 的写法同例），
/// 改列与记账同生共死；行数不是 1 时事务随 `?` 回滚，不会留下半条账。
/// 调用方因此**不许**在已开着的事务里调它（嵌套 `BEGIN` 会报错）。
pub fn set_verdict_from(m: &MemoryHandle, target: &MemKey, verdict: Verdict,
                        source: &'static str, now_ms: i64) -> Result<(), MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    let tx = m.conn().unchecked_transaction()?;
    let n = update_verdict(m, target, None, verdict)?;
    exactly_one(n, target)?;
    // `anchor` 没有 verdict 列 ⇒ 上面已经 NotFound；走到这里的一定是 card。
    append_verdict_event(&tx, &target.id, verdict, source, now_ms)?;
    tx.commit()?;
    Ok(())
}

/// review 的晋升：`unreviewed → passed`，**单向 CAS**（`from = Some(unreviewed)`）。
///
/// 三条理由：
/// - **只许单向**：`failed`/`low` 是 audit 的降级结论，review 不许把它们翻案；反过来
///   audit 也走 [`set_verdict`] 覆盖，两边的权限只差 [`update_verdict`] 的 `from` 参数，
///   谁也不借谁的手。
/// - **0 行不是错误**：那张卡已经不是 `unreviewed`（audit 先降级、或另一路已提升、或
///   重放重复调用）——与 [`resolve_retirement`] 把 `rowcount != 1` 放宽到 0 同一把尺子。
///   返回受影响行数而不是 `exactly_one`，让调用方拿到"这一条到底动没动"的读数。
/// - **同一条语句**：本函数与 [`set_verdict`] 共用 [`update_verdict`] 的唯一一条
///   `SET verdict`，差别只在 `from`（这里是 `Some(Verdict::Unreviewed)`）。"verdict 只有
///   一个写入口模块"由 `grep -n "UPDATE .*SET verdict" crates/*/src` 只命中一条语句来维持。
///
/// 只认 `card`：`anchor` 没有这一列（见 [`fact_table`]），指向它的句柄是"根本没有这个
/// 形态"，报 `NotFound` 而不是 `Sql`。**事务边界**：本函数自己开事务（改列与记账同生共死，
/// 与 `writers_rows` 同例）；调用方因此不许在已开着的事务里调它。
pub fn promote_verdict(m: &MemoryHandle, target: &MemKey, now_ms: i64) -> Result<usize, MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    // 时钟的落点是背书事件那一行（`at_ms`）：verdict 列自己没有同伴列，来源与时刻
    // 都记在 `card_verdict_event` 上。
    if target.table != MemTable::Card {
        return Err(MemError::NotFound(target.clone()));
    }
    let tx = m.conn().unchecked_transaction()?;
    let n = update_verdict(m, target, Some(Verdict::Unreviewed), Verdict::Passed)?;
    // 主键让 >1 不可能；真出现就是表形状被改过，折成错误而不是静默当成"提升了一行"。
    if n > 1 {
        return Err(MemError::Sql(format!(
            "verdict promote touched {n} rows for {}", target.key_string())));
    }
    // CAS 没动（`n == 0`）**不写**事件：那一次没有发生任何背书，记账就是伪造。
    // 只有真提升的那一次才追加 `review_apply`。
    if n == 1 {
        append_verdict_event(&tx, &target.id, Verdict::Passed, VERDICT_SOURCE_REVIEW_APPLY, now_ms)?;
    }
    tx.commit()?;
    Ok(n)
}

/// `hub review backfill-verdict --revert` 的回滚：`passed → unreviewed`，**单向 CAS**
/// （`from = Some(Passed)`）。
///
/// 为什么不是 `set_verdict`：那一条是 audit 的降级权（任意 `to` 都写得下），拿它回滚会把
/// `failed`/`low` 一起改写——回滚一份清单只该取消清单里那次提升，不该顺手把 audit 的结论
/// 抹平。CAS 的 `from` 因此是这条语句唯一的权限差：0 行 = 那张卡已不是 `passed`（从没提升、
/// 已被 audit 降级、或另一次回滚已经退过），是**读数**不是错误（与 [`promote_verdict`]、
/// [`resolve_retirement`] 同一把尺子）。
///
/// 只认 `card`（`anchor` 没有 verdict 列 ⇒ `NotFound`，不是 `Sql`）。语句仍是
/// [`update_verdict`] 那**唯一**一条 `SET verdict`，本函数不新增第二个写入口。
pub fn revert_verdict(m: &MemoryHandle, target: &MemKey, now_ms: i64) -> Result<usize, MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    // 时钟仍没有落点（verdict 是列，DDL 没给它时间戳同伴列；形参位理由同 promote_verdict）。
    let _ = now_ms;
    if target.table != MemTable::Card {
        return Err(MemError::NotFound(target.clone()));
    }
    let n = update_verdict(m, target, Some(Verdict::Passed), Verdict::Unreviewed)?;
    if n > 1 {
        return Err(MemError::Sql(format!(
            "verdict revert touched {n} rows for {}", target.key_string())));
    }
    Ok(n)
}

pub fn retire(m: &MemoryHandle, target: &MemKey, reason: &str, now_ms: i64) -> Result<(), MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    // 一条账一个 (mem_table, mem_id)：重复退场是**重写这同一条**（含把 resolved_at_ms 清回 NULL），
    // 不是长出第二条。ON CONFLICT 是这条不变量的唯一执法处。
    let n = m.conn().execute(
        "INSERT INTO retirement(mem_table, mem_id, reason, created_at_ms, resolved_at_ms)
         VALUES(?1,?2,?3,?4,NULL)
         ON CONFLICT(mem_table, mem_id)
         DO UPDATE SET reason=?3, created_at_ms=?4, resolved_at_ms=NULL",
        rusqlite::params![target.table.prefix(), target.id, reason, now_ms])?;
    exactly_one(n, target)
}

/// **事务边界**：本函数不自己开事务；调用方若已在同一连接上开着事务（`attest` 那一支），
/// 这条 UPDATE 加入那个事务，失败随它回滚。
pub fn resolve_retirement(m: &MemoryHandle, target: &MemKey, now_ms: i64) -> Result<(), MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    let n = m.conn().execute(
        "UPDATE retirement SET resolved_at_ms=?1
         WHERE mem_table=?2 AND mem_id=?3 AND resolved_at_ms IS NULL",
        rusqlite::params![now_ms, target.table.prefix(), target.id])?;
    // 这一条把 `rowcount != 1 即红` 显式放宽到 0：命中 0 行就是"已经没有未解决的退场账"，
    // 而**幂等**（解除两次不许长出第二条账）要求的正是第二次也报成功。
    // >1 仍然红（PK 让它不可能，真出现就是表形状被人改过）。
    if n > 1 {
        return Err(MemError::Sql(format!(
            "retirement resolve touched {n} rows for {}", target.key_string())));
    }
    Ok(())
}

/// 边的退场——`mem_table` 字面量 `'fact_link'`。**不**新增 `MemTable` 变体（裁定 3）：
/// 边不可指认句柄，所以它没有 `MemKey`、也不进 `MemKey::parse`。retirement.mem_table
/// 无 CHECK（DDL 已核：`retirement` 建表语句五列，除 `NOT NULL` 与主键外无别的约束），字面量合法落库。
///
/// 与 `retire` 同形：ON CONFLICT 重写、含 `resolved_at_ms=NULL`，守卫也同那条 `n != 1`
/// （`retire` 那一支把它写成 `exactly_one`，语义相同）。**这里不抄 `resolve_retirement` 的
/// `n > 1` 放宽**：那一支是 `UPDATE … WHERE`，"已经没有未解决的退场账"命中 0 行是常态，幂等要求
/// 第二次也报成功；本支是 `INSERT … ON CONFLICT DO UPDATE`，插入与重写**都**返回 1，`n == 0`
/// 不可达 —— 放宽了就等于放掉一个永远不会来的形状。`n > 1` 仍然红（主键让它不可能）。
///
/// **那三处读侧不受它影响**（`src` 侧 `retirement` 的三个读者全部按 `MemTable::prefix()` 过滤
/// `mem_table`：`judgement_cols` / `alive_where` / `dead_by_id_expr`），而 `'fact_link'`
/// 匹配不上任何一条（`MemTable` 两个变体 `Anchor`/`Card` 的 `prefix()` 只有 `anchor`/`card`）
/// ⇒ `fact_link` 里的边既不被删，也不被那三处过滤掉。
/// 它是**给两跳溯源用的前置账**（溯源要问"这条边退没退场"），第一读者是 `reader::edges`
/// 的 `retired` 格（C20 / F5 接上）。**别把这句话读成"边已经退场了"**——恰恰相反，边不进推导批：
/// `derive_pool_union` 建在 `ROW_TABLES` 那两张可指认表上（`card`/`anchor`），
/// `fact_link` 不在其中 ⇒ 边**从来不进** `select_batch` 的推导池；src 侧读 `fact_link`
/// 的地方只有 `suppress_orphans` 族② 自己那一条 SELECT 与 `reader::edges`，没有第三处。
///
/// **两个消费者都已经接上（T6c 落地）**：① 段 0 的族② 过滤让"这条边自己已有未解决退场账"
/// 这一条可判——`NOT EXISTS (SELECT 1 FROM retirement …)` 走的就是本函数写的账；② 时间戳语义
/// 见下一段。
///
/// **读侧（C20 / F5 接上）**：`reader::edges` 读 `fact_link` 并以本函数写的账算 `retired`，
/// `reader::edge_provenance` 读边的三族 provenance 行；段 0 族② 仍是另一读者。
///
/// **时间戳语义**：`ON CONFLICT DO UPDATE` 那一支本来每轮把 `created_at_ms` 改写成**当轮**的
/// `now_ms`，于是这条账一度记的是"最后一次被段 0 扫到的时刻"而不是"什么时候死的"。**T6c 把
/// 那一族收口了**：族② 的 SELECT 带 `NOT EXISTS`，一条边拿到未解决的退场账之后就不再进扫描
/// 集合 ⇒ 第二轮起不再重发 ⇒ `created_at_ms` **冻结在第一次退场那一轮**。
/// **一个例外**：账被 `resolve_retirement` 结掉之后那一行重新落进扫描集合，再死一次会被重新
/// 记账并把时间戳改写成新一轮——所以"冻结"是对**未解决**的账说的，不是对这张表说的。
/// `retire` / `retire_edge` 的既有语句逐字未动（被别的用例钉住）⇒ **同形不等于同义，
/// 本函数只认领语义，执法点在过滤那一侧**。
pub fn retire_edge(m: &MemoryHandle, edge_id: &str, reason: &str, now_ms: i64) -> Result<(), MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    let n = m.conn().execute(
        "INSERT INTO retirement(mem_table, mem_id, reason, created_at_ms, resolved_at_ms)
         VALUES(?1,?2,?3,?4,NULL)
         ON CONFLICT(mem_table, mem_id)
         DO UPDATE SET reason=?3, created_at_ms=?4, resolved_at_ms=NULL",
        rusqlite::params!["fact_link", edge_id, reason, now_ms]
    )?;
    if n != 1 {
        return Err(MemError::Sql(format!(
            "retire_edge touched {n} rows for fact_link:{edge_id}")));
    }
    Ok(())
}

/// A44 软边的退场——`mem_table` 字面量 `'fact_link'`。与 `retire_edge` 逐字同形：
/// 边不可指认句柄（裁定 3），所以这里收 id 而不是 `MemKey`；`retirement.mem_table` 无 CHECK。
/// 读侧 `event_link_of` 用 `NOT EXISTS (… resolved_at_ms IS NULL)` 过滤，撤销立即生效。
pub fn retire_event_link(m: &MemoryHandle, link_id: &str, reason: &str, now_ms: i64) -> Result<(), MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    let n = m.conn().execute(
        "INSERT INTO retirement(mem_table, mem_id, reason, created_at_ms, resolved_at_ms)
         VALUES(?1,?2,?3,?4,NULL)
         ON CONFLICT(mem_table, mem_id)
         DO UPDATE SET reason=?3, created_at_ms=?4, resolved_at_ms=NULL",
        rusqlite::params!["fact_link", link_id, reason, now_ms]
    )?;
    if n != 1 {
        return Err(MemError::Sql(format!(
            "retire_event_link touched {n} rows for fact_link:{link_id}")));
    }
    Ok(())
}

pub fn supersede(m: &MemoryHandle, target: &MemKey, by_id: &str, now_ms: i64) -> Result<(), MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    let _ = now_ms;   // 同上：superseded_by 是列，DDL 没给它时间戳同伴列（形参位留着）
    let table = fact_table(target)?;
    // 慢路径的落点：只写"被哪条顶掉了"那个 id。**不校验 by_id 存在**——顶替可以指向一条
    // 还没落库的重建结果，而"指向已消失内容的句柄查不到"是 reader 那一侧的承诺（§6 连带②），
    // 不是写入侧在这里预先拦的事。
    let n = m.conn().execute(
        &format!("UPDATE {} SET superseded_by=?1 WHERE id=?2", table.prefix()),
        rusqlite::params![by_id, target.id])?;
    exactly_one(n, target)
}

/// 升一条告警。**写前查重是这里唯一的执法处**：`attention` 的主键是自增 `id`，同一目标天然允许
/// 有多条历史（解一次升一次就是两条行），所以 `ON CONFLICT` 帮不上忙——"同一目标只许挂着**一条**
/// 未解决"这条不变量只能由这条带 `NOT EXISTS` 的 `INSERT … SELECT` 守。
/// 返回 `true` = 本轮真的新增了未解决告警；`false` = 那条已经挂着未解决，幂等、什么都不做。
/// `rechecked_at_ms` 在这里**恒写 NULL**：本函数只说"扫描看见了"，复核那一列是复核者盖的戳，
/// 别让扫描替复核表态（读了它就把"看过了"混进"还挂着不挂着"的判据里）。
pub fn raise_attention(m: &MemoryHandle, kind: AttentionKind, table: MemTable,
                       id: &str, now_ms: i64) -> Result<bool, MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    let n = m.conn().execute(
        "INSERT INTO attention(kind, mem_table, mem_id, created_at_ms, resolved_at_ms,
                               rechecked_at_ms)
         SELECT ?1, ?2, ?3, ?4, NULL, NULL
         WHERE NOT EXISTS (SELECT 1 FROM attention
                           WHERE kind = ?1 AND mem_table = ?2 AND mem_id = ?3
                             AND resolved_at_ms IS NULL)",
        rusqlite::params![kind.as_str(), table.prefix(), id, now_ms])?;
    // 只可能是 0 或 1：`WHERE NOT EXISTS` 与这条 INSERT 是同一语句、同一连接的同一事务，
    // 本产物没有并发写库面（§1 永不共库），而 `WHERE` 至多放行一行。
    // 真出现 >1 就是表形状被改过，把它折成错误而不是静默当成"升了一条"。
    if n > 1 {
        return Err(MemError::Sql(format!(
            "attention raise inserted {n} rows for {}:{id}", kind.as_str())));
    }
    Ok(n == 1)
}

/// 解除一条告警。**幂等**：命中 0 行 = 已经没有未解决的这一条，报成功（与 `resolve_retirement`
/// 同一把尺子，那里放宽到 0 的理由逐字适用：主键让 >1 不可能，真出现就是表形状被改过）。
/// 这里**不写** `rechecked_at_ms`，也不许把它抹掉：解除时刻与复核时刻是两件事，两件事混进一列
/// 就查不出是谁解的；已经盖上的一戳被解除顺手抹平，就再分不清"复核过才解"与"从没复核就解"
/// （证人：`tests/recheck.rs` 的 `盖过戳之后被解除_那一戳必须留着`）。
/// 触发方只有机械判据这一族（`anomaly::sweep` 的解除支 + `anomaly::sweep_family` 的解除支，
/// 后者服务 `alias_ambiguity` 与 `duplicate_title` 两族）；复核面从不解除——它的 `WHERE` 里
/// 就带着 `resolved_at_ms IS NULL`，只做"给活行盖戳"这一件事。
pub fn resolve_attention(m: &MemoryHandle, kind: AttentionKind, table: MemTable,
                         id: &str, now_ms: i64) -> Result<(), MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    let n = m.conn().execute(
        "UPDATE attention SET resolved_at_ms = ?1
         WHERE kind = ?2 AND mem_table = ?3 AND mem_id = ?4 AND resolved_at_ms IS NULL",
        rusqlite::params![now_ms, kind.as_str(), table.prefix(), id])?;
    if n > 1 {
        return Err(MemError::Sql(format!(
            "attention resolve touched {n} rows for {}:{id}", kind.as_str())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{set_verdict, update_verdict};
    use crate::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind, Verdict};
    use rc_store::Root;

    /// S4 的静态闸门把两条权限收进了**同一条** `SET verdict`（[`update_verdict`]），
    /// 差别只在 `from` 参数。这里在参数面上把两半一起钉住：
    ///
    /// ① 全量改写（`set_verdict`，`from=None`）不是"只会写 passed"：`low`/`failed`/`passed`
    ///    都能落库——audit 的降级权就靠这一支；把 `from` 抄成 `Some(...)` 时这里立刻红。
    /// ② CAS 的 `from` 对不上 ⇒ 0 行且值不动；对得上 ⇒ 1 行。
    ///    少了②，"CAS 退化成全量改写"（`to` 写进去就改）也能让①绿。
    ///
    /// 为什么住单元测试而不是 `tests/`：`update_verdict` 是私有语句，公开面只露出
    /// `from=None` 与 `from=Some(unreviewed → passed)` 两种组合，"from 对不上"的那一格
    /// 在公开面上只能借 promote 偶然覆盖。语句本身的可参数化性质在这里才算被钉住。
    #[test]
    fn one_set_verdict_statement_serves_full_overwrite_and_cas() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let m = root.memory();
        let k = put_card(
            m,
            &FactContent {
                kind: "decision".into(),
                title: "单条 SET verdict 的靶".into(),
                body: "这条卡的判决只由 writers_c 改。".into(),
                scope: Scope { kind: ScopeKind::Project, key: "proj-verdict".into() },
                valid_to_ms: None,
            },
            &[Provenance { turn_uid: "a".repeat(64), anchor_role: AnchorRole::Human }],
            1,
        )
        .unwrap();
        let verdict = || -> String {
            m.conn()
                .query_row("SELECT verdict FROM card WHERE id=?1", [&k.id], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(verdict(), "unreviewed", "put_card 的出厂判决变了（本用例的前提）");

        // ① 全量改写：任意值都写得下，不是只认识 passed。
        for (v, want) in [
            (Verdict::Low, "low"),
            (Verdict::Failed, "failed"),
            (Verdict::Passed, "passed"),
            (Verdict::Unreviewed, "unreviewed"),
        ] {
            set_verdict(m, &k, v, 2).unwrap();
            assert_eq!(verdict(), want, "set_verdict 没写进 {want}（全量改写那一支退化了）");
        }

        // ② CAS：from 对不上 ⇒ 0 行、值不动（"拒绝"是 0 行这个读数，不是错误）。
        assert_eq!(
            update_verdict(m, &k, Some(Verdict::Low), Verdict::Passed).unwrap(),
            0,
            "from 对不上却改了行"
        );
        assert_eq!(verdict(), "unreviewed", "CAS 落空时不许顺手改写");
        // 同一句、from 对上 ⇒ 1 行（证明 `?3` 真的参与了判定，不是被忽略的占位）。
        assert_eq!(
            update_verdict(m, &k, Some(Verdict::Unreviewed), Verdict::Passed).unwrap(),
            1,
            "from 对得上却没改行"
        );
        assert_eq!(verdict(), "passed");
    }
}
