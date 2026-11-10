//! §11 派生层与基础事实层的对撞检测。
//!
//! 两条判据都**只读三处互相独立的账**：`attestation`（谁表过态）、`retirement`（哪条退场账成立）、
//! 事实行自己的 `verdict`（审核判论）。为什么非得是两张账对撞：`RowFacts.rejected` / `retired`
//! 这类读数本来就是 `reader` 从 `attestation` / `retirement` 算出来的，拿表态去撞"由同一表态算出的列"
//! 是恒假守卫——那种判据移除与否都不会让任何用例红，等于没有判据。
//!
//! 扫描是**全量**的，不维护增量索引：本阶段库量下两条带索引的 JOIN 远比一次 LLM 调用便宜，
//! 与 hub 的"重建是全量、增量归阶段 4 容量面"同一口径。降频发生在**轮**这一层
//! （`FULL_SCAN_EVERY` 住在 hub，不在这里），所以本模块对"多久扫一次"一无所知。

use crate::keys::{AttentionKind, MemTable};
use crate::reader;
use crate::writers_c::{raise_attention, resolve_attention};
use crate::writers_x;
use crate::MemError;
use rc_store::MemoryHandle;

/// 一次扫描找到的矛盾目标。**只有 `(表, id)` 这一对指认，不带任何内容列**：告警是指针，
/// 正文留在事实行里——运维面（`status` 的数、doctor 的一行、hub 的类别串）因此结构上
/// 拿不到记忆正文，"告警行外带正文"这一族失效不是靠约定挡的，是靠这里没有可带的东西挡的。
pub type Contradiction = (MemTable, String);

/// 一轮扫描写了两本什么账。**只有计数**（守卫 12：hub 的 stdout 只放行读数，多印一行就是越界），
/// 读者是本 crate 的用例（只升、只解、既不升也不解三形），hub 那一层不打印它。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepReport {
    pub raised: usize,
    pub resolved: usize,
}

/// 条款①「死后又被确认」：同一目标上挂着**未解决**的退场账，又压着一条**未解决**的正面表态
/// （`confirmed` / `corroborated`），且那条表态**晚于**退场账。
/// 行已经因退场账而不活（`alive_where` 为假），确认却还活着 ⇒ 两本账互相指认对方错。
/// 逐 `ROW_TABLES` 查（`r.mem_table = ?1` 绑表名），所以边的退场账天然进不来（见 `ROW_TABLES` 的注释）。
///
/// **C24-N1 之后产品写路径造不出这一形状**：`writers_a::attest` 的正面表态在同一事务里
/// 解除退场账，于是两条账不可能同时留着。判据仍然保留——旧库/库外写入可以带着这个形状升级上来，
/// 那一支的用例是 `旧库里的退场之后又被确认_仍是一条矛盾`（直写 SQL 取证）。
fn confirmed_after_retirement(m: &MemoryHandle, table: MemTable) -> Result<Vec<String>, MemError> {
    let prefix = table.prefix();
    let sql = "SELECT r.mem_id FROM retirement r
               JOIN attestation a ON a.mem_table = r.mem_table AND a.mem_id = r.mem_id
               WHERE r.mem_table = ?1 AND r.resolved_at_ms IS NULL
                 AND a.resolved_at_ms IS NULL
                 AND a.stance IN ('confirmed','corroborated')
                 AND a.created_at_ms > r.created_at_ms
               ORDER BY r.mem_id";
    let mut st = m.conn().prepare(sql)?;
    let rows = st.query_map([prefix], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 条款②「判它不成立、退场账却被解除、行还活着」：目标上有一条退场账（**已解除**——为什么不必
/// 在 SQL 里再筛一遍，见函数体里的注释），事实行 `verdict = 'failed'`，且行按
/// `reader::alive_where` **还活着**。
/// ⇒ 审核判它失败、有人解了它的退场账，于是它重新进入 `recall` 的候选集。
///
/// 事实行的别名必须是 `f`：`alive_where` 展开出来的三支谓词按 `f.id` / `f.superseded_by` 指认事实行。
/// 外层退场账的别名用 `r`，与 `alive_where` 内部子查询的 `rt` 是两个作用域，不遮蔽也不冲突：
/// `alive_where` 只筛"没有未解决的退场账"，而这一支要的正是"已解除的那一条"，两者可以同时成立。
///
/// **生产者**（C24-N1 接上）：`writers_a::attest` 对正面表态在同一事务里调
/// `writers_c::resolve_retirement`，于是"判失败 + 退场账已解除 + 行按 `alive_where` 还活着"
/// 这条形状由生产码写得出来。它现在是 `contradiction` 那一族在生产侧的主要来源；
/// 条款① 反而只剩旧库/库外写入的形状（见上一条的注释）。
/// 判据要"已解除的退场账"，而 `{alive}` 里"一条未解决的退场账都没有"已经保证从 `retirement`
/// join 出来还能通过的行，其 `r` 必然已解除——所以下面不在 SQL 里再筛 `resolved_at_ms`。
/// 它不是恒假守卫：撤掉 `f.verdict = 'failed'` 那半句，本 crate 的对应用例就红。
fn revived_despite_failed(m: &MemoryHandle, table: MemTable) -> Result<Vec<String>, MemError> {
    // `anchor` 表**没有 `verdict` 列**（DDL 与 OWNERSHIP 两处对过表：verdict 只归 card），
    // 所以这一支对 anchor 恒空的理由是"那一列不存在"，不是"没有矛盾"——这两件事在运维面上是两种形状：
    // 前者本函数就不该开口，后者才是扫描说"这里干净"。写成早退而不是让 SQL 报 `no such column`，
    // 是因为一条 SQL 语法错误会把整轮 `sweep` 折成失败（`writers_c::fact_table` 对同一件事的选择是
    // `NotFound`，那是写侧按句柄的形状；读侧这里没有句柄，也不该把"这一族不适用"报成错误）。
    if table == MemTable::Anchor { return Ok(Vec::new()); }
    // 这里**不写** `r.resolved_at_ms IS NOT NULL`（"只挑已解除的退场账"）不是漏掉半边判据：
    // `{alive}` 里的 `retirement_pred` 已经要求这一行**一条未解决的退场账都没有**，
    // 于是能从 `retirement` join 出来还通过它的 `r`，必然已是解除态——写出来就是同一条件的第二个家
    // （守卫 3）。实测两处都写着时撤掉任一处都测不到什么（撤这一句九条全绿），
    // 所以判据只由 `{alive}` 那一支执法，本函数不再自筛一遍。
    let prefix = table.prefix();
    let sql = format!(
        "SELECT r.mem_id FROM retirement r JOIN {prefix} f ON f.id = r.mem_id
         WHERE r.mem_table = ?1 AND f.verdict = 'failed' AND {alive}
         ORDER BY r.mem_id",
        alive = reader::alive_where(table));
    let mut st = m.conn().prepare(&sql)?;
    let rows = st.query_map([prefix], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 当前所有矛盾目标，按 `(表前缀, id)` 稳定排序。
/// 排序**不承载语义**（告警说的是哪些目标，与顺序无关），它存在的理由是让 `sweep` 的升解顺序确定、
/// 于是 `raised` / `resolved` 两个计数可被用例逐字断言。
/// 这里明写 `sort_by(prefix)` 而不是 `out.sort()`：`MemTable` 只 derive 到 `Eq`，**没有 `Ord`**，
/// 所以 `sort()` 编译不过；也不该顺手给它补一个 `derive(Ord)`——那会把 variant 的**声明序**
/// （`Anchor, Card`）当成序，而本函数要的是 `prefix()` 的**文本序**
/// （`anchor < card`），两者不是一回事，让类型序去冒充文本序就是给下一个读者埋坑。
pub fn find_contradictions(m: &MemoryHandle) -> Result<Vec<Contradiction>, MemError> {
    let mut out: Vec<Contradiction> = Vec::new();
    for table in reader::ROW_TABLES {
        for id in confirmed_after_retirement(m, table)? { out.push((table, id)); }
        for id in revived_despite_failed(m, table)? { out.push((table, id)); }
    }
    out.sort_by(|a, b| a.0.prefix().cmp(b.0.prefix())
                             .then_with(|| a.1.cmp(&b.1)));
    // 这一支今天**去不到任何东西**，而且结构性地去不到：`retirement` 的主键是
    // `(mem_table, mem_id)` ⇒ 同一目标至多一条退场账；`writers_a::attest` 落新表态前先把同目标
    // 未解决的非 `rejected` 表态解掉 ⇒ 同一目标至多一条未解决的正面表态。于是条款① 每目标至多
    // 出一行，而条款② 要的"按 `alive_where` 还活着"与条款① 要的"挂着未解决退场账"互斥，
    // 两支也不可能同指一个目标。（实测：删掉这一行，`cargo test -p rc-memory --test anomaly`
    // 九条全绿；给同一目标连表两次态也逼不出第二行，因为第二笔会把第一笔解掉。）
    // 留着它守的是将来的放宽：那两条"至多一条"今天只住在那两条 SQL/DDL 里，谁把它们放宽，
    // 这一行就决定本函数返回的是**目标清单**还是**命中清单**——`sweep` 要的是前者。
    // 它不改变 `raised`：`writers_c::raise_attention` 自己带未解决查重，重复目标第二次返回 `false`。
    out.dedup();
    Ok(out)
}

/// 未解决告警数——`status` 的 `attention_open` 那一格与 doctor 那一行的**唯一**读法。
/// 数的是 `resolved_at_ms IS NULL` 的**全族**行，不按 `kind` 过滤：告警挂着就是需要人看，
/// 与它是哪一类无关；别名重影与同题聚合两族已落表，这一格自动把它们算进来，
/// 不必也不许在这里加 `kind` 过滤——那会把"有几条要处理"变成"有几条矛盾"，格名与谓词各说一件事。
pub fn open_count(m: &MemoryHandle) -> Result<usize, MemError> {
    let n: i64 = m.conn().query_row(
        "SELECT count(*) FROM attention WHERE resolved_at_ms IS NULL", [], |r| r.get(0))?;
    Ok(n as usize)
}

/// 一轮全量扫描：现算判据 → 升新账 → 解掉不再成立的旧账，返回两个计数。
/// 时间戳由调用方给——本 crate 不读系统时钟（判据 12a：时钟只住在三个 bin 的 `main.rs`），
/// 于是"哪一轮扫的"是账上的一个数，而不是本机表的读数。
///
/// 解除面**只针对 `Contradiction` 这一族**：本函数只对矛盾这条判据负责，别的族各有各的判据与
/// 解除路径，在这里顺手"解掉一切不再成立的"会把别人的族解错——加新族就加新函数，别扩这一支。
/// 解除是**机械**的：谓词不再成立 ⇒ 下一轮扫描解掉，不需要谁去认证它消失了
/// （这也是"告警一定能闭"的唯一证明；反向的那一半由 `writers_c::raise_attention` 的查重守）。
pub fn sweep(m: &MemoryHandle, now_ms: i64) -> Result<SweepReport, MemError> {
    let hits = find_contradictions(m)?;
    let mut raised = 0usize;
    for (table, id) in &hits {
        if raise_attention(m, AttentionKind::Contradiction, *table, id, now_ms)? { raised += 1; }
    }
    let mut resolved = 0usize;
    for (table, id) in open_attention_targets(m, AttentionKind::Contradiction)? {
        if !hits.contains(&(table, id.clone())) {
            resolve_attention(m, AttentionKind::Contradiction, table, &id, now_ms)?;
            resolved += 1;
        }
    }
    Ok(SweepReport { raised, resolved })
}

/// 某一族当前挂着未解决的告警目标（`sweep` 的解除面用它）。
/// **别的读者别拿它当"矛盾清单"**：它是上一轮写下的账，不是本轮判据的现算结果——
/// 现算结果只有 `find_contradictions` 一个家（守卫 3），运维面上也没有这一支的读者。
pub(crate) fn open_attention_targets(m: &MemoryHandle, kind: AttentionKind)
    -> Result<Vec<Contradiction>, MemError>
{
    let mut st = m.conn().prepare(
        "SELECT mem_table, mem_id FROM attention
         WHERE kind = ?1 AND resolved_at_ms IS NULL ORDER BY mem_table, mem_id")?;
    let rows = st.query_map([kind.as_str()], |r| {
        let t: String = r.get(0)?;
        let id: String = r.get(1)?;
        Ok((t, id))
    })?.collect::<Result<Vec<_>, _>>()?;
    // `MemTable::parse` 返回 `None` 的行**跳过**，不 `unwrap` 也不报错：一张写着未知表名的告警
    // 是库被外部改过的证据，而那不在本函数的职责里（它的职责是给 `sweep` 一个能与
    // `find_contradictions` 对比的形状）。跳过 = 这一族的解除面本轮不动它，
    // 比"照名字解掉"安全：我们不知道那个名字指什么，解错了就是静默销毁别人的账。
    Ok(rows.into_iter().filter_map(|(t, id)| MemTable::parse(&t).map(|mt| (mt, id))).collect())
}

/// §13 一轮重影作业的产出。`linked` 是本轮**真的补写**的别名行数（幂等重放是 0），
/// 另两格沿用本文件的 `SweepReport`。三格的读者都只有用例：运维面这一层不打印它
/// （守卫 12），告警的存在与否由 `open_count` 说。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GhostReport {
    pub linked: usize,
    pub ambiguity: SweepReport,
    pub duplicate_title: SweepReport,
}

/// 一族的一轮升解。两支（`alias_ambiguity` / `duplicate_title`）共用它而互不侵犯：
/// `raise_attention` 与 `resolve_attention` 都按 `(kind, mem_table, mem_id)` 定位，
/// 所以"别把别人的族解掉"在这一支里是**结构**而不是注释——`t6` 那条断言读的就是它。
///
/// 矛盾那一族的 `sweep` 没有并到这一支来（T11 段末"加新族时加新函数，别扩这一支"的兑现）：
/// 它的命中集是两条条款 `dedup` 之后的结果、解除面按 `Contradiction` 定位，形状一样但语义
/// 不同家。并函数就是把两族的账记成一件事。
fn sweep_family(m: &MemoryHandle, kind: AttentionKind, hits: &[(MemTable, String)], now_ms: i64)
    -> Result<SweepReport, MemError> {
    let mut raised = 0usize;
    for (table, id) in hits {
        if raise_attention(m, kind, *table, id, now_ms)? { raised += 1; }
    }
    let mut resolved = 0usize;
    for (table, id) in open_attention_targets(m, kind)? {
        if !hits.contains(&(table, id.clone())) {
            resolve_attention(m, kind, table, &id, now_ms)?;
            resolved += 1;
        }
    }
    Ok(SweepReport { raised, resolved })
}

/// §13 A40 的一轮重影作业：先闭包、再报两族。
///
/// 顺序的两条理由（写在这里是因为它会被人顺手调换）：
/// ① 闭包必须在前，而且这**不是**口径偏好：闭包写把**对侧标题**挂成本方的别名，于是它能为
///    同一串别名新增第二个认领方。证人 = `tests/ghosts.rs` 的 `t9_闭包给同一串新增认领方_所以必须先写后读`
///    （两条单边指认串成链后 "老王" 由两家认领，Q2 本轮升 2 行）。把 Q2 的扫描挪到闭包之前
///    会让这一轮报"库是干净的"而库里已经在吵——先写后读让"报告说的是写完之后的库"这条不变式成立。
/// ② 两族各扫各的判据、各解各的账（`sweep_family` 的 kind 定位）。
///
/// **第一个动作是写**（`link_alias_pair`），所以 `ReadOnly` 在这里当场拒，不留到半途。
/// 与 `sweep`（矛盾那一族）同批被 hub 调用，共用 `last_anomaly_scan_at_ms` 那一格心跳——
/// 两族共用一次扫描的**节奏**，不共用判据，也不共用解除面。
pub fn sweep_ghosts(m: &MemoryHandle, now_ms: i64) -> Result<GhostReport, MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    let mut linked = 0usize;
    for (a, b) in reader::unclosed_alias_pairs(m)? {
        linked += writers_x::link_alias_pair(m, &a, &b)?;
    }
    let ambiguity_hits: Vec<(MemTable, String)> = reader::ambiguous_alias_claimants(m)?
        .into_iter().map(|(k, _alias)| (k.table, k.id)).collect();
    let ambiguity = sweep_family(m, AttentionKind::AliasAmbiguity, &ambiguity_hits, now_ms)?;
    let duplicate_hits: Vec<(MemTable, String)> = reader::duplicate_title_rows(m)?
        .into_iter().map(|k| (k.table, k.id)).collect();
    let duplicate_title = sweep_family(m, AttentionKind::DuplicateTitle, &duplicate_hits, now_ms)?;
    Ok(GhostReport { linked, ambiguity, duplicate_title })
}
