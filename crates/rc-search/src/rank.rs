//! 候选池：所有 FTS 命中 + 算分 + 全序。**这里一个开关都不判**（§8 的五个开关在任务 6）。
//! 分开是为了让"低分项仍可 `recall`"（G6③）成立：过滤发生在闸门，不在检索。

use crate::score::score_row;
use crate::score::FLOOR;
use rc_index::{search, SearchHit};
use rc_memory::gate::{GateIndex, Viewer};
use rc_memory::keys::MemKey;
use rc_memory::reader::{self, RowFacts};
use rc_store::MemoryHandle;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

/// 检索面的错误。**没有一支是"没有结果"的形状**（G7：沉默必须可枚举）——
/// 索引层读不出来、事实行读不回来、候选键在库里没有，全都原样往上抛，一个都不许折成 `Ok(vec![])`。
///
/// 但这句话只封住了**影子比记忆新**那一族（`NoFacts`：影子里有一条、库里没有 ⇒ 响亮报错，
/// 由 `rank` 里 `by_key.get(..).ok_or_else(..)` 抛出，测试见 `tests/score.rs::候选键必须在记忆库里找得到_否则响亮报错`）。
/// **镜像方向不成立**：库里写了一行、影子还没被 `rebuild` 收到它时（新写入后没重建），
/// `search` 少返回一条命中、`rank` 照样返回 `Ok`，那条记忆在检索面**静默缺席**。
/// 这不是疏漏，是本轮的边界：`rc-index` 的 `search` 只比 `marker.norm_version`
/// （`crates/rc-index/src/rebuild.rs:176`），从不比 `rows_indexed`；`rows_indexed` 今天只被 `hub` 的
/// index 打印与 `rc-agent::status` 的透传当选数看（`crates/hub/src/main.rs`、`crates/rc-agent/src/tools.rs`），
/// 读路径上无人拿它跟内容层对账。本轮**不加执法**——
/// 对账要在每次查询里再读一遍全库（把 M8 的开销翻倍），且闸门语义属于任务 6。
/// 这一族的归宿是**阶段 4 的 doctor 对账格（A38）**：拿 `rows_indexed` 与 `reader::facts().len()` 对账，
/// 载荷只许是数字、不许带正文（守卫 12）。原句的"任务 11"收口从未落地——2026-10-12 盘点回树实证。
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error(transparent)]
    Index(#[from] rc_index::IndexError),
    #[error(transparent)]
    Store(#[from] rc_store::StoreError),
    /// 事实行读不回来 ⇒ 候选池组不出来。`MemError` 的 Display 在构造点已过守卫 12 的消毒
    /// （`crates/rc-memory/src/lib.rs` 的 `sqlite_text`），所以这里仍是 `transparent`。
    /// 偏离 brief 的变体清单：`reader::facts` 返回的就是 `MemError`，`?` 只能有一支接得住它。
    #[error(transparent)]
    Memory(#[from] rc_memory::MemError),
    /// 偏离 brief 原文（`Sql(rusqlite::Error)`），依裁定 R18 同因：rusqlite 的 `Display` 会把
    /// `InvalidPath(路径)` 与整条 SQL 原文吐进运维面（守卫 12），而用户查询片段就躺在那段文本里。
    /// 载荷因此只许是**消毒后的类别串**。本档无人构造它——`rank` 的 SQL 全部经 `rc-index`/
    /// `rc-memory` 的读数面；留着它是任务 6/7 的接口声明。
    #[error("sqlite: {0}")]
    Sql(String),
    /// 载荷只许是 `key_string()`：它是身份值，不含正文（守卫 12）。
    /// 这一支存在的理由见 `tests/score.rs::候选键必须在记忆库里找得到_否则响亮报错`。
    #[error("index holds key {0} that memory does not")]
    NoFacts(String),
}

#[derive(Debug, Clone)]
pub struct Scored {
    pub key: MemKey,
    /// ×1024 的定点整数。它**只在内存里存在**，永不落库、永不进 `export`、永不进 agent 面输出。
    /// 波二刀 B 起它**只装词面相关度**（公式见 `score::score_row`）：复用反馈的升档走
    /// [`Scored::reuse`]（全序首键），不混进这个数——否则 `score = 1024*matched/row_len`
    /// 这条公式不变量（`tests/order.rs`）与 doctor/export 的"相关度"读数会同时变成半真。
    pub score: i64,
    pub matched: i64,
    pub row_len: usize,
    /// 内容 + 判定族读数（`verdict`/`valid_to_ms`/`superseded_by`/`retired`/`attestation`/`cooldown_until_ms`）。
    /// 任务 6 的五个开关全部只读这个字段——闸门不许自己再去查一遍库（守卫 3：一份查询实现）。
    pub facts: RowFacts,
    /// 波二刀 B：复用反馈信号（只从 `card_reuse_event` 读一次）。锚行恒 `Default`。
    pub reuse: ReuseSignal,
}

/// 波二刀 B（设计 §波次二第 3 条 rank 臂）：一张卡的复用信号，读取侧的判定输入。
///
/// 两格各有各的消费者，都只在 [`rank_counted`] 里从 `card_reuse_event` 装载一次
/// （[`rc_memory::reuse::reuse_ledger`] 的批量读口，不逐卡查）：
/// - `worked`：**全序首键**——`worked >= WORKED_TIER_MIN`（2）的卡升一档；
/// - `stale`：`Switches::not_stale` 的第四臂 ⇒ `GateCell::Stale`（读取侧 retire 候选）。
///
/// 为什么信号挂在 `Scored` 而不是塞进 `RowFacts`：`RowFacts` 是全仓二十多处结构体字面量
/// 共用的行事实形状（`reader.rs` 的字段文档），而这一格今天只有检索面有读者。挂在这里
/// 仍是"判定与渲染同一份数据"（守卫 3）：`gate::decide` 读同一个 `Scored`。
///
/// 为什么升档是"分层"而不是给 `score` 加常数：常数会引入一个不可解释的系数，且让
/// `score` 的公式不变量失义；分层只动全序的首键，0/1 两档由
/// [`rc_memory::reuse::WORKED_TIER_MIN`] 一个阈值定义，`worked == 1` 不升档（钉死）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReuseSignal {
    pub worked: i64,
    pub stale: bool,
}

impl ReuseSignal {
    /// 0/1 两档。阈值引用复用域常量（`rc_memory::reuse::WORKED_TIER_MIN`），这里不抄数字。
    pub fn worked_tier(self) -> i64 {
        i64::from(self.worked >= rc_memory::reuse::WORKED_TIER_MIN)
    }
}

impl Scored {
    pub fn above_floor(&self) -> bool {
        self.score >= FLOOR
    }
}

/// 全序：**复用升档（`worked >= 2`）优先**，然后分数降序，同分按句柄字典序升序。
/// **句柄唯一，所以这个序是全序不是偏序**，`sort` 与 `sort_unstable` 给同一个结果——
/// 这一点的载体是 `tests/order.rs::sort_unstable_给同一个结果`（同一批句柄分别 `sort`
/// 与 `sort_unstable` 后逐位相同）。另一条 `真实夹具里的三条同分卡_20_次重复逐_id_相同`
/// 只调 `rank`（内部只有 `sort`），钉的是"同一份快照两次运行顺序相同"，**不碰 `sort_unstable`**。
/// 手写 `Ord` 而不是 `#[derive]` + `Reverse` 包裹：派生出来的字段顺序一旦有人加字段就会静默改变排序键，
/// 而排序键是这个系统里少数"改了会让两次运行结果不同"的东西。
///
/// 刀 B 的首键（`reuse.worked_tier()`）只对**卡**有值：锚行恒 `Default`，锚与卡之间因此
/// 按原有两键比较——与刀 B 之前逐位相同。
impl Ord for Scored {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .reuse
            .worked_tier()
            .cmp(&self.reuse.worked_tier())
            .then_with(|| other.score.cmp(&self.score))
            .then_with(|| self.key.key_string().cmp(&other.key.key_string()))
    }
}
impl PartialOrd for Scored {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for Scored {
    fn eq(&self, other: &Self) -> bool {
        self.key.key_string() == other.key.key_string()
            && self.score == other.score
            && self.reuse == other.reuse
    }
}
impl Eq for Scored {}

/// 候选池：所有 FTS 命中 + 算分 + 全序。这里不判任何开关，也不吞任何错误。
///
/// W37-A1（读闸进检索层）：`viewer` 是**必给**的读者身份——不给（`None`）就是一条
/// 显式声明的"这一路不过读闸"，只许中心 CLI/测试/旧调用用（与 `gate::facts_visible`
/// 的 `viewer == None` 同一条约定）。`Some(v)` ⇒ 装载一次 [`GateIndex`]，读闸拒掉的行
/// 从候选池里静默缺席（与"查不到"同形，§4.2 不泄露存在性）：G16 的 similar 旁路
/// （`rc-agent/tools.rs::similar_hint`）与 hook probe（`gate::probe_view`）因此不再
/// 跨 agent 泄 id+标题。检索本身（FTS 命中、算分、排序）一个字不动——
/// 过滤发生在候选集，不在排序键（G6③）。
///
/// 被闸掉的命中行**不是** `NoFacts`：`NoFacts` 说的是"影子比记忆新"（索引里有、库里没有），
/// 而"库里在、读者看不见"必须仍是静默缺席。所以存在性查全量 map、可见性单独判。
///
/// G1 刀 A 起本函数是 [`rank_counted`] 的薄包装（先例：`put_timeline_rows_counted`）：
/// archive 消费落在计数版里，三个调用方（recall 查询臂 / probe_view / similar_hint）
/// 共享同一处过滤，不许各消费者各自滤一遍。
pub fn rank(
    m: &MemoryHandle,
    viewer: Option<&Viewer>,
    now_ms: i64,
    query: &str,
) -> Result<Vec<Scored>, SearchError> {
    Ok(rank_counted(m, viewer, now_ms, query)?.0)
}

/// G1 刀 A（设计 `2026-11-08-g1-reuse-lesson-design.md` §波次一）：候选池 + **archive
/// 消费与计数**。第二返回值 = 本查询候选集里被 `sink='archive'` 挡下的行数
/// （不是全库归档总数；不进候选集的归档行不数）。计数与过滤**同一处产生**——
/// recall 信封的 `value_mix.archived` 直接取它，不许在消费面再查一遍库。
///
/// 顺序钉：**读闸在前、archive 在后**。被身份闸拒的行静默缺席、不算归档计数
/// （价值降档与身份沉默是两族；混计会让 value_mix 泄露存在性）。archive 只挡
/// 查询主循环：显式寻址（recall handles 臂 / view=card/expand）不走这里，原文全留。
pub fn rank_counted(
    m: &MemoryHandle,
    viewer: Option<&Viewer>,
    now_ms: i64,
    query: &str,
) -> Result<(Vec<Scored>, usize), SearchError> {
    let hits: Vec<SearchHit> = search(m, query)?;
    let facts = reader::facts(m)?;
    let by_key: HashMap<String, RowFacts> =
        facts.into_iter().map(|f| (f.key.key_string(), f)).collect();
    // 一次装载（"每调用面一次"，设计 §4.1 的代价账）：`None` ⇒ 不装载、不判。
    let gate = match viewer {
        Some(v) => Some(GateIndex::load(m, v, now_ms)?),
        None => None,
    };
    // G1 刀 A：归档集一次装载（缺表 = 空集，老库/只读副本都能跑）。
    let archived = rc_memory::value_gate::archived_card_ids(m)?;
    // 刀 B：复用读数一次装载（同一条代价账：缺表 = 空 map）。信号只对卡有意义——
    // 锚不是"被用上的对象"（工具面同样拒收锚句柄）。
    let reuse = rc_memory::reuse::reuse_ledger(m)?;
    let qset: HashSet<String> = rc_index::analyzer::query_tokens(query).into_iter().collect();
    let mut out = Vec::with_capacity(hits.len());
    let mut archived_count = 0usize;
    for h in hits {
        let facts = by_key
            .get(&h.key.key_string())
            .ok_or_else(|| SearchError::NoFacts(h.key.key_string()))?
            .clone();
        // 读闸（W37-A1）：被拒的行静默缺席，不进候选池、不进 `NoFacts`。
        if let Some(g) = &gate {
            if g.decide(&facts).is_err() {
                continue;
            }
        }
        // G1 刀 A：archive 消费（单一读口）。只有卡会被盖章（锚不是消费行）；
        // 被挡的行不进候选池、不进排序，计数在这里与过滤同时产生。
        if facts.key.table == rc_memory::MemTable::Card && archived.contains(&facts.key.id) {
            archived_count += 1;
            continue;
        }
        // 刀 B：复用信号（读闸与 archive 之后——被挡的行不装信号，也不数）。
        let reuse_signal = if facts.key.table == rc_memory::MemTable::Card {
            reuse
                .get(&facts.key.id)
                .map(|c| ReuseSignal { worked: c.worked, stale: c.stale > 0 })
                .unwrap_or_default()
        } else {
            ReuseSignal::default()
        };
        let score = score_row(&qset, &h.tokens, h.row_len);
        // `matched` 在这里独立重算了一遍（而不是从 `score` 反推）：它是 `doctor` 与 `export` 的读数
        // （"命中几个词"比"1024 分制的几分"可读得多），而反推会引入除法误差。
        // 这是两处计算、但算的是同一个定义——`tests/order.rs::分数恒等于公式_两半不许各自漂移`
        // 用 `score == 1024 * matched / row_len` 把"两处漂移"钉死。
        let matched = qset
            .iter()
            .filter(|t| h.tokens.iter().any(|r| r == *t))
            .count() as i64;
        out.push(Scored {
            key: h.key,
            score,
            matched,
            row_len: h.row_len,
            facts,
            reuse: reuse_signal,
        });
    }
    out.sort();
    Ok((out, archived_count))
}
