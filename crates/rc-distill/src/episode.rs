//! episode 归组内核（派发档 I-3b · 纯程序、本轮零 LLM）。
//!
//! 一句话：按 D9 的 S0 实测阈值，把"同一件事的几步"从活 fact 里挑出来，交给
//! `rc_memory::writers_x::put_episodes`（存储层已在树、已验收，本层一行 SQL 都不碰库写）。
//! 本模块只做**判定与账本**，不取执行钟、不造出处、不写第二族行。
//!
//! 四件事由派发档钉死，改动任何一件都要先改派发档：
//! ① **剥自锚**（[`shared_participants`]）：共同参与者 = `participants_json` 的交集
//!    减去**说话 agent 自己的锚**。根因是 `writers_fact.rs` 的 `put_fact_rows_with_extras` 无条件把 subject 并进参与者，
//!    而现网 35/36 行的 subject 就是 agent 自锚 `qoder` ⇒ 含自锚时 `|∩|≥1` 覆盖 597/630 对
//!    （94.8%，等于没有过滤器），剥完只剩 3 对（S0 §2.2/§2.3 实测）。
//! ② **两根时间闸**（[`BLOCK_SPAN_CAP_MS`] / [`MERGE_WINDOW_MS`]）：块内跨度 ≤ 15min、
//!    邻近轴的 Δt ≤ 10min。两根各管一条轴，理由见那两个常量。
//! ③ **三段置信带**（[`Band`]）：高置信（剥自锚后有共同参与者）⇒ 直接合；中间带（只有出处邻近：
//!    同证据轮 / 会话内轮距 ≤ 1）⇒ 交裁决接口位（[`MidBandAdjudicator`]：I-3b 的
//!    [`DeferMidBand`] 一律拒，I-12 起可接真座位 [`crate::adjudicate::LlmMidBand`]；
//!    不配座位 = 结构上零模型调用）；低置信（都不成立）⇒ 不合。
//! ④ **时间轴只有两口钟**（[`fact_time`]）：`occurred_at_ms`（事件时间，已知）→
//!    `created_at_ms`（发话时刻）。**绝不许用 `turns.ingested_at_ms`**（导入钟比发话晚中位 26.85h、
//!    最长 9.98 天，S0 §1.3），也绝不许用 `valid_from_ms`（那是状态生效轴，覆盖率 0/36，
//!    且它已经躺在 `reader::FACT_VIEW_COLUMNS` 的 `at_ms` 里——本层刻意不读那一格）。
//!
//! 夹具纪律（沿用 `rc-memory/tests/episode.rs`）：正反例只用通用占位名；**现网专名一律从副本库里读**，
//! 源码里一个也不抄。

use rc_memory::reader::{self, GroupFact};
use rc_memory::{put_episodes, BatchCtx, EpisodeContent};
use rc_store::{MemoryHandle, TranscriptHandle};
use std::collections::{BTreeSet, HashMap};

/// 一趟扫描最多读多少行活 fact（与 `derive::DERIVE_MAX_ROWS_PER_BATCH` 同族但**不同值域**：
/// 那一格是"一次模型调用的批宽"，本仓本轮不调模型，这一格是"一次全量重算的内存上界"）。
/// 归组是**整段重算**（新行可能顶掉旧段，见 `put_episodes` 的 supersede 那一支），所以它必须
/// 带着窗内的全部行进判定；超出这一格的余量走 `lag_rows`，下一趟接着扫。
pub const EPISODE_MAX_FACTS_PER_PASS: usize = 40;

/// D9 的块内跨度闸：一段经历里最早与最晚成员的**时间轴距离**上限（S0 §6.4）。
///
/// 它是 single-linkage 的唯一解药：S0 §5.4 实测"只按 Δt 合并"时 5min 就会把 36 行吸成 5 块、
/// 10min 时最大块 11 行。15min 这一档的实测落点：恰好剔掉两条 6h 边（`76db×2c15` 6.45h、
/// `fe65×2c15` 6.22h），**保住** 13.5min 那条（`76db×fe65`，S0 认定"看着确实同一件事"）。
/// 收到 10min 会把 13.5min 那条一起砍掉 ⇒ 阳性对照失效；放宽到 30min 起读数与 15min 完全相同，
/// 但 225 对那一档（含自锚、Δt≤1h）的形状说明再宽就是吞会话。
pub const BLOCK_SPAN_CAP_MS: i64 = 15 * 60 * 1000;

/// D9 的时间窗：中间带（出处邻近轴）的 Δt 上限（S0 §6.2）。
///
/// **它管的是邻近轴，不是参与者轴**：S0 §6.2 的原句是"时间窗 Δt≤10min，且必须与出处轴同时生效"，
/// 而 §6.4 明确把 13.5min 那条（派发档指定的**必须合**的阳性对照）交给 15min 跨度闸保住。
/// 两根闸若都压在强边上，13.5min 与 14.7min 两条边都会被 10min 砍掉 ⇒ 夹具当场不可满足。
/// 这条差异按派发档要求**如实登记**（报告 §"偏离"），阈值一个字没动。
pub const MERGE_WINDOW_MS: i64 = 10 * 60 * 1000;

/// D9 的兜底信号：同一会话内两轮之间的轮距上限（S0 §4.5/§4.7：轮距 ≤1 的 9 对是
/// `Δt≤10min ∧ 同session`（128 对）的**真子集**，是今天唯一不靠参与者就有密度的信号）。
pub const MAX_TURN_DISTANCE: i64 = 1;

/// 证据轮在会话里的位置。**会话内**按 `(created_at_ms, rowid)` 定序（与 S0 §4 的测量同尺；
/// 同刻并列按 `rowid` 破平，理由与后果见 [`turn_positions`]）。
///
/// 为什么不用 `commit_seq`：S0 §4.1 实测它是**导入批号**（全库 distinct = 4 = `ingest_log` 批数），
/// 不是轮序。为什么不用 `ingested_at_ms`：派发档明令（导入钟）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnPosition {
    pub session_key: String,
    pub seq: i64,
}

/// 一轮的两段判定：它凭什么算"同一件事"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    /// 高置信：剥自锚后仍有共同参与者 ⇒ 本轮直接合。
    Strong,
    /// 中间带：只有出处邻近（同证据轮 / 会话内轮距 ≤ [`MAX_TURN_DISTANCE`]）⇒ 本轮不合，交裁决接口位。
    Proximity,
}

/// 一对候选。`a`/`b` 是**这一次扫描里的下标**（不是 fact id）：块算法与写侧都按下标取行，
/// 而下标由 `(created_at_ms, id)` 升序的扫描固定给出 ⇒ 同一库两次扫描给出同一对下标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pair {
    pub a: usize,
    pub b: usize,
    /// 两根成员时刻之差的绝对值（[`fact_time`] 那一根轴）。
    pub delta_ms: i64,
    pub band: Band,
    /// 剥自锚后的共同参与者锚 id（`Band::Strong` 的那一格证据）。
    pub shared: Vec<String>,
    pub same_turn: bool,
    /// 会话内轮距；跨会话或任一轮读不到 ⇒ `None`。
    pub turn_distance: Option<i64>,
}

/// 中间置信带的裁决接口位（派发档 §2.7 定的形状，I-12 换了它的**入参**）。
/// I-3b 落地时它的实现只有 [`DeferMidBand`]（一律拒 ⇒ 结构上零模型调用）；I-12 接上真座位
/// 之后有了第二个实现（[`crate::adjudicate::LlmMidBand`]）。入参带 `facts` 的理由：
/// [`Pair`] 只有扫描窗内的**下标**（I-3b 的契约，写侧按下标取行），而"这两条是否同一段经历"
/// 这个语义问题必须读成员正文——文本只能从这一窗里取，别处没有。
pub trait MidBandAdjudicator {
    /// `true` = 这一对认定是同一件事，落进合并集；`false` = 不合。
    /// `facts` 是本次扫描窗的全部活 fact，`pair.a`/`pair.b` 是其中的下标。
    fn accepts(&self, facts: &[GroupFact], pair: &Pair) -> bool;
}

/// 一律不合的实现（零座位时的裁决器：**不配座位 = 结构上不可能有模型调用**）。
/// 它不是占位死码：`run_episode_pass` 每次都带着它走，`mid_band_deferred` 那一格就是它的读数。
pub struct DeferMidBand;

impl MidBandAdjudicator for DeferMidBand {
    fn accepts(&self, _facts: &[GroupFact], _pair: &Pair) -> bool { false }
}

/// 一趟归组的判定结果（不含任何写侧计数：写没写成要看库，不能由判定自报）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EpisodePlan {
    /// 关联信号成立的全部候选对（强边 + 中间带）。
    pub pairs: Vec<Pair>,
    /// 过了两根时间闸、要落库的边（强边 + 裁决器放行并入的中间带边）。
    pub merged_edges: Vec<Pair>,
    /// 通过跨度闸的块（每块 = 成员下标，按扫描序升序；只有 ≥2 成员的块在这里）。
    pub blocks: Vec<Vec<usize>>,
    /// 被跨度闸剔掉的边（读数出口：被闸挡下的对数不许蒸发，P-5 同律）。
    pub span_pruned: Vec<Pair>,
    /// 中间带里裁决器说"不合"的对（本轮 = 全部中间带对）。
    pub deferred: Vec<Pair>,
}

/// 一次写入口调用的账本。**每一格都是行数或对数，不是计划条数**（重跑时写侧全 0）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EpisodeReadout {
    /// 关联信号成立的对数（[`EpisodePlan::pairs`]）。
    pub candidates: usize,
    /// **本次落库**的块里的强边对数（已存在的段重算 ⇒ 0，与 `episodes_written` 同进退）。
    pub merged_pairs: usize,
    /// 跨度闸剔掉的强边对数。
    pub span_pruned: usize,
    /// 中间带里本轮没合的对数。
    pub mid_band_deferred: usize,
    pub episodes_written: usize,
    pub members_linked: usize,
    pub cards_written: usize,
    /// 本次被顶掉的旧 `episode` 行数（镜像卡的顶替数由写入口另记，见 `EpisodeWriteOutcome`）。
    pub superseded: usize,
    pub dropped_members: usize,
}

/// 一趟扫描的完整回执。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodePass {
    pub readout: EpisodeReadout,
    /// 本趟扫描窗的**末尾行** `(created_at_ms, id)`；空库 = `None`。
    /// 它是"游标动没动"的读数来源（收批判据），一次运行内的水位，**不落库**：
    /// `job_watermark.family` 的 CHECK 是闭集（`rc-store/src/ddl.rs` 里 `job_watermark` 建表那条，只有
    /// `distill|derive|anomaly_scan`），新增一族是一次 schema 变更，而 DDL 不在本轮落点。
    /// 跨运行的收敛靠幂等（内容寻址 id + `INSERT OR IGNORE`），不靠这一格。
    pub window_end: Option<(i64, String)>,
    /// 窗之后还剩几行活 fact（[`reader::count_group_facts_after`]）。
    pub lag_rows: usize,
    /// 本趟实际扫到的行数。
    pub scanned: usize,
}

/// 一行 fact 的**归组时间轴**：事件时间已知用 `occurred_at_ms`，否则用发话时刻 `created_at_ms`。
///
/// 派发档 §2.6 的两口钟，一支不落：现网 4/36 有事件时间（S0 §1.1），其余 32 行落发话钟。
/// `episode.start_ms/end_ms` 是写入口从成员 `occurred_at_ms`/`occurred_end_ms` 另算的包络
/// （母档 D4-补），与本函数无关——本函数只服务"这两句话挨不挨得近"这一问。
pub fn fact_time(f: &GroupFact) -> i64 {
    f.occurred_at_ms.unwrap_or(f.created_at_ms)
}

/// 这一行里"说话 agent 自己的锚"（剥自锚的被减数）。两支：
/// ① `speaker_anchor_id`——C1 起它是**程序**按"转录角色 + 会话身份"盖的章（`writers_v4`），
///    agent 轮就是该 agent 的自锚；
/// ② `subject_anchor_id` **当且仅当它是 agent 锚且 canonical 等于本行的 `source_agent`**——
///    这一支专门对付根因（`writers_fact.rs` 的 `put_fact_rows_with_extras` 把 subject 无条件塞进参与者，
///    于是"agent 说了一句关于自己的话"在参与者轴上长得像"两条都提到了同一方"）。
///
/// **人不会被剥**：subject 是 person 锚（某个 person 锚，不是 agent 自锚）⇒ 留着，
/// 它是合法的共同参与者。锚行已不在（`subject_kind` 读成 `None`）⇒ 不剥（宁可少剥，不凭猜）。
pub fn self_anchors(f: &GroupFact) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    if let Some(id) = &f.speaker_anchor_id { out.insert(id.clone()); }
    if f.subject_kind.as_deref() == Some("agent")
        && f.subject_canonical.as_deref() == Some(f.source_agent.as_str())
        && !f.subject_anchor_id.is_empty() {
        out.insert(f.subject_anchor_id.clone());
    }
    out
}

/// 两行的**共同参与者（剥自锚后）**，按 a 的出现序定序（同输入同输出，不靠哈希序）。
pub fn shared_participants(a: &GroupFact, b: &GroupFact) -> Vec<String> {
    let drop = self_anchors(a);
    let drop_b = self_anchors(b);
    a.participants.iter()
        .filter(|id| !drop.contains(*id) && !drop_b.contains(*id))
        .filter(|id| b.participants.iter().any(|x| x == *id))
        .cloned()
        .collect()
}

/// 证据轮的位置表：`turn_uid` → `(session_key, 会话内序)`。
/// 一次读全表（会话库的轮数是 10² 量级，而归组要的是任意两行的轮距，逐行查是 N+1）。
///
/// **序是 `(created_at_ms, rowid)`**，与 S0 §4.1/§4.2 的测量同一把尺子：会话内同刻并列是真风险
/// （实测 57 组、最大一组 5 轮同刻），而 `turn_uid` 是哈希、拿它破平等于用随机数排话语序；
/// `rowid` 至少是插入序。登记两条：① 这条序与 `batch::neighbor_of` 用的 `(created_at_ms, turn_uid)`
/// **不同**（那一处给的是抽取 prompt 的邻轮，本轮不改它——改它会挪动邻轮选择，那是另一条工单）；
/// ② S0 债 3 的"rowid 在导入/压缩后是否稳定"仍未证。
/// **这一格有可观测后果**：现网 `76db98675c23 × 11105ccfac64` 按本序是轮距 1（进中间带、被标出来），
/// 按 uid 序是轮距 2（连候选都不算）——两条都不落库，但"被闸挡下的对数"读数会变，
/// 所以必须与 S0 同尺，否则 §6 的阈值无从对账。
pub fn turn_positions(t: &TranscriptHandle) -> Result<HashMap<String, TurnPosition>, rc_store::StoreError> {
    let mut st = t.conn().prepare(
        "SELECT turn_uid, session_key FROM turns \
         ORDER BY session_key, created_at_ms, rowid",
    )?;
    let mut out = HashMap::new();
    let mut last_session: Option<String> = None;
    let mut seq: i64 = 0;
    for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (uid, session_key) = row?;
        seq = match &last_session {
            Some(prev) if *prev == session_key => seq + 1,
            _ => { last_session = Some(session_key.clone()); 1 }
        };
        out.insert(uid, TurnPosition { session_key, seq });
    }
    Ok(out)
}

/// 两行的会话内轮距：跨会话、或任一侧的轮不在会话库里 ⇒ `None`（**不**折成"很大"或 0）。
fn turn_distance(
    positions: &HashMap<String, TurnPosition>, a_uid: &str, b_uid: &str,
) -> Option<i64> {
    let (Some(a), Some(b)) = (positions.get(a_uid), positions.get(b_uid)) else { return None };
    if a.session_key != b.session_key { return None; }
    Some((a.seq - b.seq).abs())
}

/// 纯程序判定：把扫描窗内的行配成候选对、分好三段、过完两根闸。
///
/// 顺序是判定的一部分：**先分带**（谁有资格合），**再按时间闸筛边**，**最后做跨度收敛**
/// （[`cap_spans`]）。把跨度闸提到分带之前，会让一条 6h 的强边先把两行粘住、再被拆，
/// 而拆完的那一对在带判据里已经不是候选——同库两趟给出不同账本。
///
/// 不跨归属是构造性的：只配 `(world_id, scope_kind, scope_key)` 三格全同的两行
/// （`card_scope`/`fact_scope` 两条索引都是这个三元组，只比后两格会把两个世界合成一段经历，
/// 与 `writers_x::shared_member_scope` 同一把尺子）。
///
/// **"同 session"这一格（D9 第 5 条）**：邻近带要求同一会话（见下面的 `same_session`）。
/// 它在本现网是**空约束**——S0 §3.6 实测跨会话 fact 对 646 个、min = 1.71d，
/// 任何 `Δt ≤ 1d` 的窗都自动排除了全部跨会话对，网格上"要求"与"不要求"的读数逐格相同。
/// 留着它的理由是它免费且防未来（一旦出现跨会话但同分钟的语料，它是唯一护栏），
/// **不许**声称它有判别力。强边一侧同样要求同会话（同一把尺子），因此这一段对两支都成立。
pub fn plan_pairs(facts: &[GroupFact], positions: &HashMap<String, TurnPosition>) -> Vec<Pair> {
    let mut out: Vec<Pair> = Vec::new();
    for (i, a) in facts.iter().enumerate() {
        for (j, b) in facts.iter().enumerate().skip(i + 1) {
            if a.scope.kind != b.scope.kind || a.scope.key != b.scope.key
                || a.world_id != b.world_id { continue; }
            let delta_ms = (fact_time(a) - fact_time(b)).abs();
            let same_turn = a.evidence_turn_uid == b.evidence_turn_uid;
            let distance = turn_distance(positions, &a.evidence_turn_uid, &b.evidence_turn_uid);
            // D9 第 5 条：两支都要求同会话（它是本现网的**空约束**，见上面的注释）。
            // "同一个证据轮"就是同一个会话——那是定义，不是推断，所以它不需要位置表也成立；
            // 除此之外位置读不到（轮已不在会话库里）⇒ `None` ⇒ 这一对不进候选（fail-closed）。
            if !(same_turn || distance.is_some()) { continue; }
            let shared = shared_participants(a, b);
            if !shared.is_empty() {
                out.push(Pair { a: i, b: j, delta_ms, band: Band::Strong, shared,
                                same_turn, turn_distance: distance });
                continue;
            }
            // 中间带：出处邻近。派发档 §2.7 把它实现成"标出来、本轮不合"，
            // 时间面仍要成立（Δt ≤ [`MERGE_WINDOW_MS`]——S0 §6.2 那句"必须与出处轴同时生效"）。
            let proximate = same_turn
                || distance.map(|d| d <= MAX_TURN_DISTANCE).unwrap_or(false);
            if proximate && delta_ms <= MERGE_WINDOW_MS {
                out.push(Pair { a: i, b: j, delta_ms, band: Band::Proximity, shared,
                                same_turn, turn_distance: distance });
            }
        }
    }
    out
}

/// 跨度收敛（S0 §6.4 的 `measure-8-spancap.py` 那一条算法）：反复剔掉**超长块里 Δt 最大**的那条边，
/// 直到每个块的跨度 ≤ [`BLOCK_SPAN_CAP_MS`]。
///
/// 为什么是"剔最长边"而不是"不建这个块"：现网那条 6.47h 的五成员块是 single-linkage 一步步链出来的
/// （链上边 1.2min → 55.9s → … → 6.22h），整块作废会把链上那些真相邻的边一起杀掉。
/// 并列最长边按 `(b, a)` 定序后取**后出现的那条**（见 `largest_edge`），同库两次收敛到同一个块集。
/// 返回 `(留下的边, 被剔的边)`。
fn cap_spans(facts: &[GroupFact], strong: &[Pair]) -> (Vec<Pair>, Vec<Pair>) {
    let mut kept: Vec<Pair> = strong.to_vec();
    let mut pruned: Vec<Pair> = Vec::new();
    while let Some(block) = over_span_block(facts, &kept) {
        // 只在这个块的内部边上动刀：跨块的边不存在（块就是连通闭包）。
        let worst = largest_edge(&kept, &block);
        pruned.push(worst.clone());
        kept.retain(|e| !(e.a == worst.a && e.b == worst.b));
    }
    (kept, pruned)
}

/// 第一个超长块（成员下标集）。块集按"最小成员下标"定序后取第一个命中的，于是并列时也有唯一答案。
fn over_span_block(facts: &[GroupFact], edges: &[Pair]) -> Option<Vec<usize>> {
    for block in blocks_of(facts.len(), edges) {
        let span = facts.iter().enumerate()
            .filter(|(i, _)| block.contains(i))
            .map(|(_, f)| fact_time(f))
            .fold(None, |acc: Option<(i64, i64)>, ms| {
                Some(match acc { Some((lo, hi)) => (lo.min(ms), hi.max(ms)), None => (ms, ms) })
            })
            .map(|(lo, hi)| hi - lo)
            .unwrap_or(0);
        if span > BLOCK_SPAN_CAP_MS { return Some(block); }
    }
    None
}

/// 块内 Δt 最大的那条边（并列取 `(b, a)` 序里最后一条）。
fn largest_edge(edges: &[Pair], block: &[usize]) -> Pair {
    let mut best: Option<&Pair> = None;
    for edge in edges {
        if !block.contains(&edge.a) || !block.contains(&edge.b) { continue; }
        // 字典序比较即"Δt 更大，或并列时 `(b, a)` 更靠后"——一条式子，两支判据。
        let take = match best {
            None => true,
            Some(x) => (edge.delta_ms, edge.b, edge.a) > (x.delta_ms, x.b, x.a),
        };
        if take { best = Some(edge); }
    }
    best.expect("over_span_block 只会为至少有一条边的块返回 Some")
        .clone()
}

/// 并查集连通块（single-linkage）。每块成员下标升序，块间按最小成员下标升序 ⇒ 确定性。
fn blocks_of(count: usize, edges: &[Pair]) -> Vec<Vec<usize>> {
    let mut parent: Vec<usize> = (0..count).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x { parent[x] = parent[parent[x]]; x = parent[x]; }
        x
    }
    for edge in edges {
        let (mut a, mut b) = (find(&mut parent, edge.a), find(&mut parent, edge.b));
        if a == b { continue; }
        if a > b { std::mem::swap(&mut a, &mut b); }
        parent[b] = a;
    }
    let mut grouped: HashMap<usize, Vec<usize>> = HashMap::new();
    for index in 0..count {
        let root = find(&mut parent, index);
        grouped.entry(root).or_default().push(index);
    }
    let mut out: Vec<Vec<usize>> = grouped.into_values().collect();
    out.sort();
    out
}

/// 归组一趟：扫描 → 判定 → 落库 → 记账。
///
/// `budget` = 本趟的扫描帽（调用方按 `--max-batches` 逐趟放大，见 hub 那一侧的循环注释）；
/// 窗**永远从库头开始**（整段重算，见 `reader::group_facts` 那条"为什么没有游标形参"）。
/// 落库按**块**逐段调 `put_episodes`（每段的署名取该段最早成员那一行，与 I-3a 的副本真跑同一取法），
/// 因为 `BatchCtx` 是 P 独占列（守卫 11）而不同段的署名可以不同。
pub fn run_episode_pass(
    m: &MemoryHandle, t: &TranscriptHandle, budget: usize,
    adjudicator: &dyn MidBandAdjudicator, now_ms: i64,
) -> Result<EpisodePass, crate::DistillError> {
    let facts = reader::group_facts(m, budget)?;
    let scanned = facts.len();
    let window_end = facts.last().map(|f| (f.created_at_ms, f.id.clone()));
    let lag_rows = reader::count_group_facts_after(m, window_end.clone())? as usize;
    let positions = turn_positions(t)?;
    let plan = plan_window(&facts, &positions, adjudicator);
    let mut readout = EpisodeReadout {
        candidates: plan.pairs.len(),
        span_pruned: plan.span_pruned.len(),
        mid_band_deferred: plan.deferred.len(),
        ..Default::default()
    };
    for block in plan.blocks.iter().filter(|b| b.len() > 1) {
        let ordered: Vec<&GroupFact> = block.iter().map(|i| &facts[*i]).collect();
        let ids: Vec<String> = ordered.iter().map(|f| f.id.clone()).collect();
        let ctx = BatchCtx {
            source_agent: ordered[0].source_agent.clone(),
            device_id: ordered[0].device_id.clone(),
        };
        let rows = [EpisodeContent {
            title: episode_title(ordered.len(), block_span(&ordered)),
            narrative: episode_narrative(&ordered),
            fact_ids: ids,
        }];
        let written = put_episodes(m, &rows, &ctx, now_ms)?;
        // 段已在库里（内容寻址 id 命中 `INSERT OR IGNORE`）⇒ `episodes == 0`，
        // 这一格退化成 0 而不是"少一个"：重跑的写侧计数必须是 0，否则 `lag_rows` 会一直谎报余量。
        if written.episodes == 0 { continue; }
        readout.episodes_written += written.episodes;
        readout.members_linked += written.members;
        readout.cards_written += written.cards;
        readout.superseded += written.superseded;
        readout.dropped_members += written.dropped_members;
        readout.merged_pairs += edges_within(&plan.merged_edges, block).len();
    }
    Ok(EpisodePass { readout, window_end, lag_rows, scanned })
}

/// 判定 + 跨度收敛 + 中间带裁决，纯函数（用例直接打这一格，不落库）。
///
/// 合并图 = **全部强边** + **裁决器放行的中间带边**（本轮唯一实现一律拒 ⇒ 只有强边）。
/// 顺序是判定的一部分：先分带、再合图、后收敛——反过来会让一条被跨度闸剔掉的边
/// 在带判据里已经不算候选，于是同库两趟给出不同账本。
pub fn plan_window(
    facts: &[GroupFact], positions: &HashMap<String, TurnPosition>,
    adjudicator: &dyn MidBandAdjudicator,
) -> EpisodePlan {
    let pairs = plan_pairs(facts, positions);
    let mut merge_edges: Vec<Pair> =
        pairs.iter().filter(|p| p.band == Band::Strong).cloned().collect();
    let mut deferred: Vec<Pair> = Vec::new();
    for pair in pairs.iter().filter(|p| p.band == Band::Proximity) {
        if adjudicator.accepts(facts, pair) { merge_edges.push(pair.clone()); }
        else { deferred.push(pair.clone()); }
    }
    let (kept, span_pruned) = cap_spans(facts, &merge_edges);
    let blocks = blocks_of(facts.len(), &kept).into_iter()
        .filter(|b| b.len() > 1).collect::<Vec<_>>();
    EpisodePlan { pairs, merged_edges: kept, blocks, span_pruned, deferred }
}

/// 落在同一块里的边（合并图上的，用于 `merged_pairs` 那一格）。
fn edges_within(edges: &[Pair], block: &[usize]) -> Vec<Pair> {
    edges.iter()
        .filter(|p| block.contains(&p.a) && block.contains(&p.b))
        .cloned().collect()
}

/// 块的跨度（[`fact_time`] 轴）。
fn block_span(members: &[&GroupFact]) -> i64 {
    let (mut lo, mut hi) = (i64::MAX, i64::MIN);
    for f in members {
        let ms = fact_time(f);
        lo = lo.min(ms);
        hi = hi.max(ms);
    }
    (hi - lo).max(0)
}

/// 镜像卡标题。**程序现算、零模型**（母档 §2.2：这一族的 `kind/title/text/pointers_json`
/// 由程序写是登记过的合法形状）。它必须只由成员集合决定——同一段经历两次归组要写出同一句话，
/// 否则"卡面漂了"会被读成"内容变了"（句柄虽不变，人读的却是字）。
fn episode_title(count: usize, span_ms: i64) -> String {
    format!("程序归组的一段经历（{count} 条事实，跨度 {}）", describe_span(span_ms))
}

/// 跨度的中文读法（整秒精度，不进模型面，只进卡面）。
fn describe_span(span_ms: i64) -> String {
    let secs = span_ms / 1000;
    if secs < 60 { return format!("{secs} 秒"); }
    if secs < 3600 { return format!("{} 分 {} 秒", secs / 60, secs % 60); }
    format!("{} 小时 {} 分", secs / 3600, (secs % 3600) / 60)
}

/// 镜像卡正文：逐成员一行，正文**原样引用**（本轮不调模型 ⇒ 卡面上不写任何"结论"或改写）。
/// 首行说清这一段的来路，因为这张卡是 `mode='inferred'`，读者要能看出它不是谁说的。
fn episode_narrative(members: &[&GroupFact]) -> String {
    let mut out = String::from(
        "以下事实由归组内核按共同参与者（已剥掉说话 agent 的自锚）与时间邻近判为同一件事的几步；\
         本轮未调用模型，正文是成员原话的逐条引用，不是改写。\n",
    );
    for member in members {
        out.push_str(&format!("· {}\n", clip(&member.text, 120)));
    }
    out.push_str(&format!("共 {} 条。", members.len()));
    out
}

/// 按字符数截断（中文正文按字算，不切 UTF-8 字节）。
fn clip(text: &str, limit: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit { return text.to_string(); }
    let mut head: String = chars[..limit].iter().collect();
    head.push('…');
    head
}
