//! 三票审核：全阶段**唯一**写 C 侧列的作业（`verdict` / `retirement`，加上 `apply_merges` 的
//! `superseded_by`）。判同只出意图，抽取只写 P 与 X——所以"谁能改一条记忆的判定"这一问在本
//! crate 只有一个答案文件，评审只需要盯 `audit.rs`（§2.15 末段那条定案的执法形状）。
//!
//! 两条形状纪律：① 落笔前先把证据读全（§2.18 连带 0：票 1 那句问题必须能用手上的东西回答）；
//! ② 三票不短路、判定顺序票 1 > 票 2 > 票 3、写只有一次（§2.18 连带 2 与 3）。

use crate::dedupe::{scored_siblings, MergePlan};
use crate::prompt::{audit_system, audit_user};
use crate::DistillError;
use rc_llm::{LlmBackend, Prompt};
use rc_memory::{lookup, provenance_of, retire, scope_of, set_verdict_from, supersede,
                unreviewed_rows, MemError, MemKey, RowFacts, Scope, Verdict,
                VERDICT_SOURCE_AUDIT};
use rc_store::{MemoryHandle, TranscriptHandle};
use serde_json::Value;

/// 一次 pass 审几行。**调用算术（§2.15 末段 + §1.0-8 阶段 3 重算）**：
/// 一轮最多 = 抽取 1 次 + 判同 4 次 + 审核 `MAX_PER_PASS = 4` 行 × 3 票 = 12 次 + 推导 1 次 = 18 次。
/// 这个数是**一轮的成本上界**，不是闸：本地调用次数上限已按 2026-11-06 的裁定砍掉。
pub const MAX_PER_PASS: usize = 4;

/// 票 1 最多带几条转录原文。3 是 `rc_search::gate::MAX_FORM_LINES` 那个"人能一次读完几行"的
/// 同族取值，不是新的经验数字：**它定的是 prompt 体积上界，不是证据完整度**。
/// 一条行引用了 9 轮，票 1 只拿到按 turn_uid 升序**攒满 3 条读得到的为止**（读不到的跳过且不占
/// 名额，见 `evidence_turns`），于是"能不能重读出来"这个问题的
/// 含义被固定成"能不能在我给你的这几行里重读出来"——这是刻意的窄，宽了 prompt 就没界。
pub const MAX_EVIDENCE_TURNS: usize = 3;

/// 票 3 带几条邻居。与判同的 `MAX_DEDUPE_CANDIDATES` 同值不同名：两个数定的是两件不同的事
/// （一次成对判定 vs 一行的矛盾检查），下一次会因不同的理由被改（守卫 3 管的是同一份**尺子**，
/// 不是同一个**数字**——尺子在 `scored_siblings` 那里只有一份）。
pub const MAX_AUDIT_SIBLINGS: usize = 3;

/// 一票。三个变体就是三票的全部，新增第四票必须让所有 `match` 编译不过才算表态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vote { Provable, LongLived, Consistent }

impl Vote {
    /// user 段里唯一的票号写法（P-39）。
    pub fn slug(self) -> &'static str {
        match self {
            Vote::Provable => "vote1-provable",
            Vote::LongLived => "vote2-long-lived",
            Vote::Consistent => "vote3-consistent",
        }
    }
    /// 三条**不同维度**的陈述（§2.18 表格第三列）。同义改写就是 P-39 那一型。
    pub fn question(self) -> &'static str {
        match self {
            Vote::Provable => "can this row be re-read, word for word in meaning, from the transcript lines rendered below?",
            Vote::LongLived => "one week from today, will this row still matter on its own, without the conversation that produced it?",
            Vote::Consistent => "is this row free of any flat contradiction with the neighbour rows rendered below about the same project?",
        }
    }
    pub(crate) fn renders_evidence(self) -> bool { matches!(self, Vote::Provable) }
    pub(crate) fn renders_siblings(self) -> bool { matches!(self, Vote::Consistent) }
}

/// 三票的**顺序**就是这个数组的顺序：判定按票 1 > 票 2 > 票 3 排，先判的才有消音权。
/// `pub` 是为了用例能断"长度 = 每行调用次数"而不用抄一份 `[..; 3]`（抄了就漂）。
pub const VOTES: [Vote; 3] = [Vote::Provable, Vote::LongLived, Vote::Consistent];

/// 一行审核所需的全部上下文。`row` 是被审的那一行，`evidence` / `siblings` 按票渲染。
#[derive(Debug, Clone)]
pub struct AuditCase {
    pub row: RowFacts,
    /// `(role, body)`，按 `provenance_of` 的 `ORDER BY turn_uid` 依次往下读，**攒满
    /// `MAX_EVIDENCE_TURNS` 条读得到的为止**；读不到的出处跳过且不占名额（见 `evidence_turns`）。
    /// 所以"前 3 条"只是没有空洞时的形状，不是规则本身：一条行引用了 5 轮而头两轮已被 purge，
    /// 票 1 照样拿到 3 条可读的，而不是 2 条。
    /// 取哪几条仍是确定的——同一份库跑两遍，升序的读得到者被同一条规则截在同一处，
    /// 而 §2.18 要的那格正是确定性（两个操作员看到同几行），不是连号。
    pub evidence: Vec<(String, String)>,
    pub siblings: Vec<RowFacts>,
    /// R2-1：邻居池**不可用**（不是"查过、没有"）。唯一的来源是探针自己没有
    /// `resource_owner` 行——[`rc_memory::reader::rows_in_scope`] 的 `?6=1` 在那一格
    /// 把整池清空（fail-closed），于是 `siblings` 为空但**什么都没查过**。
    ///
    /// 这一个 bool 就是为了让渲染能把两种"没有"分开：不可用 ⇒ `NEIGHBORS: unavailable …`，
    /// 有归属但确实没有 ⇒ 普通空段。程序不改判决（空池本身不等于 reject）——这是信息与
    /// 判据的修正，不是又一道硬闸。
    pub siblings_unavailable: bool,
}

/// 探针（被审的那一行）有没有归属行。读法与 [`rc_memory::reader::rows_in_scope`] 的
/// `?6` 同一格：`resource_owner` 里 `(mem_table, mem_id)` 的那一行在不在。
///
/// 只读、只查存在性：没有归属行时票 3 的池不是"空"，是**不可用**（见 [`AuditCase`]）。
fn probe_has_owner(m: &MemoryHandle, key: &MemKey) -> Result<bool, DistillError> {
    match m.conn().query_row(
        "SELECT 1 FROM resource_owner WHERE mem_table=?1 AND mem_id=?2",
        rusqlite::params![key.table.prefix(), key.id],
        |_| Ok(()),
    ) {
        Ok(()) => Ok(true),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum AuditError {
    #[error("vote reply is not a JSON object")] NotAnObject,
    #[error("vote reply has no 'holds' key")] MissingHolds,
    #[error("vote reply 'holds' is not a boolean")] HoldsNotBool,
}

/// `holds:false` 是一个业务结论（⇒ `Ok(false)`），`{"holds":"true"}` 是形状错（⇒ `Err`）。
/// 把后者折成 `false` 的代价见坑 P-36：一次拼写错误让一条真记忆永久进抑制账。
pub fn parse_vote(reply: &str) -> Result<bool, AuditError> {
    let v: Value = serde_json::from_str(reply).map_err(|_| AuditError::NotAnObject)?;
    let o = v.as_object().ok_or(AuditError::NotAnObject)?;
    o.get("holds").ok_or(AuditError::MissingHolds)?
        .as_bool().ok_or(AuditError::HoldsNotBool)
}

#[derive(Debug, Default)]
pub struct AuditOutcome {
    pub rows_audited: usize,
    pub passed: usize,
    pub low: usize,
    pub failed: usize,
}

#[derive(Debug, Default)]
pub struct ApplyReport {
    pub merged: usize,
    pub refused: usize,
}

/// 出处 → 转录原文。**跨库两条查询，没有 JOIN**（守卫 1：`turns` 在 transcript.db、
/// `provenance` 在 memory.db；一条 SQL 打不通两个文件——跨库连接是被 §2.17 / 坑 P-07
/// 禁掉的形状）。
/// 只有 `QueryReturnedNoRows` 才算"这一条出处读不到"（被 purge、幽灵 uid）——**跳过且不计数**；
/// 其余 SQLite 错误原样上抛。分界的理由：`provenance` 没有指向 `turns` 的外键（DDL 注释里明写
/// "turn_uid 必须存在"这条校验属抽取侧），所以缺行是合法历史状态；而库被锁、schema 不对是运维
/// 失败，把它一并折进"读不到"，`run_audit` 的"证据为空 ⇒ 降档"那一支就把运维失败读成了业务判定——
/// `verdict` 一旦离开 unreviewed 就永久离开审核游标，下一轮不会再来问一遍（坑 P-71、坑 P-43）。
/// 全部跳过 ⇒ 空表，由 `run_audit` 那一支处置（降档，不消音）。
/// 缺表与缺行是两件事，这一格要说清：**缺行**（`QueryReturnedNoRows`）是合法历史状态，跳过；
/// **读不出来**（`no such table`、库被锁、schema 不对）是运维失败，上抛。
/// `tests/audit.rs` 的 `an_unreadable_turns_table_is_an_error_not_a_demoted_row` 用
/// `DROP TABLE turns` 造后者——`conn()` 是 `pub`，所以这一格测得到，不是只能写在注释里。
fn evidence_turns(t: &TranscriptHandle, m: &MemoryHandle, key: &MemKey)
    -> Result<Vec<(String, String)>, DistillError> {
    let mut out = Vec::new();
    for (uid, _role) in provenance_of(m, key)? {
        if out.len() == MAX_EVIDENCE_TURNS {
            break;
        }
        match t.conn()
            .query_row("SELECT role, body FROM turns WHERE turn_uid=?1", rusqlite::params![&uid],
                       |r| Ok((r.get(0)?, r.get(1)?)))
        {
            Ok(hit) => out.push(hit),
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(out)
}

/// 一行 = 一次读证据 + 一次读邻居。`build_case` 与 `run_audit` 分开的唯一理由：
/// P-43 那两条"证据进没进 prompt"的断言必须能在**不开口**的情况下构造出 case。
/// 邻居的归属从行自己读（`scope_of`），不要求它落在本轮批次里——票 3 问的是库里的事。
pub fn build_case(t: &TranscriptHandle, m: &MemoryHandle, row: &RowFacts) -> Result<AuditCase, DistillError> {
    let evidence = evidence_turns(t, m, &row.key)?;
    let scope: Scope = scope_of(m, &row.key)?;
    // R2-1：先问"探针有没有归属行"，再取邻居。没有归属 ⇒ 池在 SQL 里已被 `?6=1` 清空，
    // 这一票的上下文是**不可用**，不是"查过没有"。
    let siblings_unavailable = !probe_has_owner(m, &row.key)?;
    let siblings = scored_siblings(m, row.key.table, &scope, &row.key.id,
                                   &row.title, &row.body, MAX_AUDIT_SIBLINGS)?;
    Ok(AuditCase { row: row.clone(), evidence, siblings, siblings_unavailable })
}

/// 一行三票。**三次调用全部发生之后**才动笔（§2.18 连带 3），所以中途任何一次形状错都落在
/// **出错那一行**一个字节都没写的那一侧（用例 `a_shape_error_writes_nothing_at_all`）。
/// 这是**逐行**的边界，不是整轮的：`run_audit` 用 `?` 直抛、`writers_c` 每写即时提交，
/// 所以同一轮里在出错行之前那些行已经落了库（用例的前提是库里只有一行，见那里的注释）。
/// 返回写下的那一档，是为了让用例能断"分配律"而不必回读三列。
pub(crate) fn audit_row(t: &TranscriptHandle, m: &MemoryHandle, backend: &dyn LlmBackend,
                 row: &RowFacts, now_ms: i64) -> Result<Verdict, DistillError> {
    let case = build_case(t, m, row)?;
    // §2.18 连带 0 的末尾：一票都问不了的时候，判不了 ≠ 不成立。
    // 走降档而不是消音，走的也不是错误：`retirement` 一旦写下就永久拦住同内容的重蒸（守卫 13），
    // 而"我读不到证据"与"证据反对它"是两件相反的事。
    if case.evidence.is_empty() {
        // W15a：来源记 `audit`——降档也是 audit 盖的，`low` 的背书同样要可追溯。
        set_verdict_from(m, &row.key, Verdict::Low, VERDICT_SOURCE_AUDIT, now_ms)?;
        return Ok(Verdict::Low);
    }
    let sys = audit_system();
    let mut holds = [false; 3];
    for (i, vote) in VOTES.iter().enumerate() {
        let reply = backend.complete(&Prompt { system: sys.clone(),
                                               user: audit_user(&case, *vote), derive: false })?;
        holds[i] = parse_vote(&reply)?;
    }
    let verdict = if !holds[0] { Verdict::Failed }
        else if !holds[1] || !holds[2] { Verdict::Low }
        else { Verdict::Passed };
    // 只有票 1 有消音的权力（§2.18 连带 1）：票 2 / 票 3 不成立只降档，不写账。
    if verdict == Verdict::Failed {
        retire(m, &row.key, "audit:vote1", now_ms)?;   // 这条 reason slug 在生产码里只出现一次（测试夹具另计）
    }
    // W15a：写入口与来源一起点名——audit 盖的档位（含 `passed`）在事件表里留一条 `audit`。
    set_verdict_from(m, &row.key, verdict, VERDICT_SOURCE_AUDIT, now_ms)?;
    Ok(verdict)
}

pub fn run_audit(t: &TranscriptHandle, m: &MemoryHandle, backend: &dyn LlmBackend, now_ms: i64)
    -> Result<AuditOutcome, DistillError> {
    let mut out = AuditOutcome::default();
    for row in unreviewed_rows(m, MAX_PER_PASS)? {
        out.rows_audited += 1;
        match audit_row(t, m, backend, &row, now_ms)? {
            Verdict::Passed => out.passed += 1,
            Verdict::Low => out.low += 1,
            Verdict::Failed => out.failed += 1,
            // `Unreviewed` 不可能从这里出来（`audit_row` 的四个出口只有三档）。
            // 这一支的存在是让 `match` 穷尽，而穷尽是守卫：`Verdict` 加一档时这里编译不过。
            Verdict::Unreviewed => return Err(DistillError::Memory(
                MemError::Sql("audit returned the unreviewed verdict".into()))),
        }
    }
    Ok(out)
}

/// 判同计划的落笔（任务 7 只出意图，这里才是写）。**逐条 plan 落笔前重读两行**（P-42）：
/// 缓存判同时刻的 `RowFacts` 就等于接受"两条互指的 plan 都落笔"，而那两行会一起消失且
/// 永远回不来——`alive_where` 把它们双双排除（写侧那一边由 `dead_by_id_expr` 的第三支接住：
/// 行存在且 `superseded_by` 非 NULL），此后每次重蒸都算命中抑制账，
/// 而写入语句是 `ON CONFLICT(id) DO NOTHING`，一个错误都不报。
pub fn apply_merges(m: &MemoryHandle, plans: &[MergePlan], now_ms: i64) -> Result<ApplyReport, DistillError> {
    let mut out = ApplyReport::default();
    for plan in plans {
        if plan.loser == plan.winner {
            out.refused += 1;   // P-37 的第二道：构造点已经挡了，这里不许假设上游守约
            continue;
        }
        let loser = lookup(m, &plan.loser)?
            .ok_or_else(|| DistillError::Memory(MemError::NotFound(plan.loser.clone())))?;
        let winner = lookup(m, &plan.winner)?
            .ok_or_else(|| DistillError::Memory(MemError::NotFound(plan.winner.clone())))?;
        // 顺序要有：两行都**存在**才谈"跨不跨表"。把这道闸放在 lookup 之前，一个表名打错的
        // 幽灵句柄就会被读成"这条计划不合式"（静默 `refused`），而它真正的意思是"这行不存在"。
        if plan.loser.table != plan.winner.table {
            // 顶替指针写在 **loser 那张表**上，而 `alive_where` 的第二支只看 `IS NULL`：
            // 一个跨表的 id 落到那里就永远读不到对应行，那一行永久消失（同一个 P-37 的立场——
            // 真实的 `MergePlan` 构造点永远同表，可这一层照旧不假设上游守约）。
            out.refused += 1;
            continue;
        }
        if loser.superseded_by.is_some() || winner.superseded_by.is_some() {
            out.refused += 1;   // 环与重复记账共同的形状
            continue;
        }
        supersede(m, &plan.loser, &plan.winner.id, now_ms)?;
        out.merged += 1;
    }
    Ok(out)
}
