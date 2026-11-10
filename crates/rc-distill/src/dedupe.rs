//! 判同：本轮产出的每一行，与**库里**同表同归属的邻居比一次。
//! 本模块一行都不写（Q8）：它只交 `Vec<MergePlan>`，落 `superseded_by` 的 `apply_merges` 住在
//! `audit.rs`（任务 8）。"谁能写 C 的列"在本阶段因此只有一个答案，评审只需要盯一个文件。
//!
//! 候选的来源是记忆库直读，不是检索面那个排名入口（坑 P-35）：抽取刚写完的那几行还没进影子索引，
//! 用 `rank` = 新行永远看不见自己的重复，而它一个错误码都不响。
//! 切词与算分也不在本层实现——`rc_index::analyzer` 与 `rc_search::score_row` 是全系统那把尺子（守卫 3）。

use crate::extract::BatchPlan;
use crate::prompt::{judge_system, judge_user};
use crate::DistillError;
use rc_index::analyzer::{query_tokens, row_doc};
use rc_llm::{LlmBackend, Prompt};
use rc_memory::{rows_in_scope, scope_of, MemKey, MemTable, RowFacts, Scope};
use rc_search::{score_row, FLOOR};
use rc_store::{LedgerHandle, MemoryHandle};
use serde_json::Value;
use std::collections::HashSet;

/// 一轮最多判几个案例。**调用算术（§2.15 末段 + §1.0-8 阶段 3 重算，与 `audit.rs` 的
/// `MAX_PER_PASS` 旁边同一段）**：
/// 一轮最多 = 抽取 1 次 + 判同 `MAX_DEDUPE_CASES = 4` 次 + 审核 4 行 × 3 票 = 12 次 + 推导 1 次 = 18 次。
/// 这个数是**一轮的成本上界**，不是闸（本地调用次数上限已按 2026-11-06 的裁定砍掉）；
/// `quota_exhausted` 那一格读的是**供应商**有没有报额度耗尽，不是本轮数了几次。
pub const MAX_DEDUPE_CASES: usize = 4;

/// 每个案例带进 prompt 的候选上限。截断发生在**算分之后**（§2.15 第 3 条）——
/// 在比之前丢行就是漏并，所以 `rows_in_scope` 不带 LIMIT，这里才是那道闸。
pub const MAX_DEDUPE_CANDIDATES: usize = 3;

/// 别名 → 行的唯一映射（`r1` 恒为探针）。
#[derive(Debug, Clone)]
pub struct CaseRows {
    pub probe: RowFacts,
    pub candidates: Vec<RowFacts>,
}

impl CaseRows {
    pub fn rows(&self) -> impl Iterator<Item = &RowFacts> {
        std::iter::once(&self.probe).chain(self.candidates.iter())
    }
    /// `r1`..`rN` → 行。`r0`、非 `r` 前缀、数字超出范围都算认不出（⇒ `UnknownAlias`）。
    pub fn find_alias(&self, alias: &str) -> Option<&RowFacts> {
        let n: usize = alias.strip_prefix('r')?.parse().ok()?;
        if n == 0 {
            return None;
        }
        self.rows().nth(n - 1)
    }
}

/// 一条"谁顶掉谁"的意图。**它不是写**：落笔的人叫 `apply_merges`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePlan {
    pub winner: MemKey,
    pub loser: MemKey,
}

/// 判同答复的形状错误。六支都不带载荷（守卫 12：模型给的文本不进运维面）。
#[derive(Debug, thiserror::Error)]
pub enum JudgeError {
    #[error("judge reply is not a JSON object")] NotAnObject,
    #[error("judge reply has no 'same' key")] MissingSame,
    #[error("judge reply 'same' is not a boolean")] SameNotBool,
    #[error("judge reply has no 'keep' key")] MissingKeep,
    #[error("judge reply 'keep' is not a string")] KeepNotString,
    #[error("judge reply 'keep' names an alias that was never rendered")] UnknownAlias,
}

/// 形状错 ⇒ `Err`，**一条 plan 都不产**（P-36 同族）。`same:false` 是一个业务结论，
/// 它走 `Ok(vec![])` 那条路；把形状错折进那一条，症状就变成"判同正常工作但永远不并"。
pub fn parse_judge(reply: &str, case: &CaseRows) -> Result<Vec<MergePlan>, JudgeError> {
    let v: Value = serde_json::from_str(reply).map_err(|_| JudgeError::NotAnObject)?;
    let o = v.as_object().ok_or(JudgeError::NotAnObject)?;
    let same = o.get("same").ok_or(JudgeError::MissingSame)?
        .as_bool().ok_or(JudgeError::SameNotBool)?;
    if !same {
        return Ok(Vec::new());
    }
    let keep = o.get("keep").ok_or(JudgeError::MissingKeep)?
        .as_str().ok_or(JudgeError::KeepNotString)?;
    let winner = case.find_alias(keep).ok_or(JudgeError::UnknownAlias)?.key.clone();
    // `keep` 自己永不进 loser 列表（P-37）：下面那一个 `.filter(|r| r.key != winner)` 是那条判据
    // 在本层唯一的实现处——漏掉就是把 `r1` 判成它自己的 loser，而判同这一层不出第二条缝拦这件事。
    Ok(case.rows()
        .filter(|r| r.key != winner)
        .map(|r| MergePlan { winner: winner.clone(), loser: r.key.clone() })
        .collect())
}

#[derive(Debug, Default)]
pub struct DedupeOutcome {
    pub cases_judged: usize,
    pub skipped_entity: usize,
    pub probes_over_cap: usize,
    pub plans: Vec<MergePlan>,
}

/// 本轮的探针 = `rc_extract_row` 里这一批去重后的句柄，按 `(table_kind, mem_id)` 升序。
/// 读 ledger.db 而不是 memory.db：派生表的语义才是"本轮的"（P-24）。
/// 顺序因此**不是写入顺序**而是内容寻址 id 的字典序——这正是要把顺序定在 SQL 里的理由：
/// 两个操作员跑同一份库，探针顺序必须一样，`keep:"r1"` 才有确定含义。
fn probes(l: &LedgerHandle, plan: &BatchPlan) -> Result<Vec<(MemTable, String)>, DistillError> {
    let sql = "SELECT DISTINCT table_kind, mem_id FROM rc_extract_row \
               WHERE session_key=?1 AND device_id=?2 ORDER BY table_kind, mem_id";
    let mut st = l.conn().prepare(sql)?;
    let rows = st.query_map(rusqlite::params![plan.batch.session_key, plan.batch.device_id],
                             |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (kind, id) = row?;
        let table = MemTable::parse(&kind).ok_or_else(|| {
            DistillError::Memory(rc_memory::MemError::Sql("extract evidence holds an unknown table slug".into()))
        })?;
        out.push((table, id));
    }
    Ok(out)
}

/// "同表同归属的活邻居，按分数取前 `cap` 条"。判同与审核的票 3 是同一个问题的两个读者，
/// 所以这一段的尺子只能有一份（守卫 3）：切词 `rc_index::analyzer`、算分 `rc_search::score_row`，
/// 阈值 `FLOOR`，截断在**算分之后**（§2.15 第 3 条）。
/// `cap` 是形参不是常量：判同用 `MAX_DEDUPE_CANDIDATES`，票 3 用 `MAX_AUDIT_SIBLINGS`，
/// 两个数下一次会因不同的理由被改。
pub(crate) fn scored_siblings(
    m: &MemoryHandle,
    table: MemTable,
    scope: &Scope,
    exclude_id: &str,
    probe_title: &str,
    probe_body: &str,
    cap: usize,
) -> Result<Vec<RowFacts>, DistillError> {
    let siblings = rows_in_scope(m, table, scope, exclude_id)?;
    let q: HashSet<String> = query_tokens(&format!("{probe_title} {probe_body}"))
        .into_iter().collect();
    let mut scored: Vec<(i64, RowFacts)> = Vec::new();
    for c in siblings {
        let (toks, len) = row_doc(&c.title, &c.body);
        let s = score_row(&q, &toks.split(' ').map(str::to_string).collect::<Vec<String>>(), len);
        if s >= FLOOR {
            scored.push((s, c));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.key.cmp(&b.1.key)));
    scored.truncate(cap);
    Ok(scored.into_iter().map(|(_, r)| r).collect())
}

pub fn run_dedupe(
    m: &rc_store::MemoryHandle,
    l: &LedgerHandle,
    plan: &BatchPlan,
    backend: &dyn LlmBackend,
    now_ms: i64,
) -> Result<DedupeOutcome, DistillError> {
    // 判同不写任何东西，所以它没有可打时间戳的地方（见 Interfaces 那条说明）。
    let _ = now_ms;
    let mut out = DedupeOutcome::default();
    let sys = judge_system();
    for (table, id) in probes(l, plan)? {
        let key = MemKey { table, id };
        let probe = rc_memory::lookup(m, &key)?
            .ok_or_else(|| DistillError::Memory(rc_memory::MemError::NotFound(key.clone())))?;
        // 派生表说这行在本轮写成了，库里却没有 ⇒ 这是库不一致，不是判同的业务结论。
        if table == MemTable::Anchor {
            out.skipped_entity += 1;   // P-30：跳过并计数，不报错也不静消失
            continue;
        }
        if out.cases_judged >= MAX_DEDUPE_CASES {
            out.probes_over_cap += 1;  // A16：这一段没有下一轮补判，被看见是它唯一的处置
            continue;
        }
        let scope = scope_of(m, &key)?;
        let candidates = scored_siblings(m, table, &scope, &key.id, &probe.title, &probe.body,
                                         MAX_DEDUPE_CANDIDATES)?;
        if candidates.is_empty() {
            continue;   // 没有邻居就不开口（P-19 在判同面的形状）
        }
        let case = CaseRows { probe, candidates };
        let reply = backend.complete(&Prompt { system: sys.clone(),
                                               user: judge_user(&case), derive: false })?;
        out.cases_judged += 1;
        out.plans.extend(parse_judge(&reply, &case)?);
    }
    Ok(out)
}
