//! P2 状态 pass 的库面（方案 `design/2026-10-31-state-pass-p2.md` §3–§6）。
//!
//! 一条真实 pass 做四件事：从转录里**选**一批还没跑过 state 的轮 → 抽取调用 →
//! 把"窄审 + 判定 + 写库"整段交给 [`crate::review::run::run_item`] → 逐轮/整批落
//! `extract_pass_state`。
//!
//! 七条纪律（与 event runner 同源，多一条 inferred 分岔）：
//! 1. **选批只认 `(turn, 'state', '')`**：state 是单一 pass，`family` 恒空串；
//!    naming/fact/event/time 的表态行在这里一次都不读。可重选判据与 event 同向
//!    （[`crate::pass_event::reselectable`]，同一把尺子不另写一份）。
//! 2. **主体只有已过审的锚**：候选池 = 库里活着且对本行 owner 可见的锚（`crate::candidates`）。
//!    锚只在 naming 审核之后才落库，所以调度把 `distill --pass state` 排在 `review naming`
//!    **之后**（方案 §6）——排在前面的话每一批都会因为"候选池是空的"而拒收。
//! 3. **审核是第二次独立调用**：抽取的 parser 过了才开口；审核用自己的 system/user 与
//!    canary，只读「行 + 引文 + 目标轮」。审核失败/不确定 ⇒ 该行不写；LLM 不可用 ⇒
//!    每轮 `pending`，**不放行**。
//! 4. **`mode` 由程序判**：值逐字在引文里 ⇒ `expressed`（进审核与落库）；需要换词才说得
//!    出来 ⇒ `inferred` ⇒ **隔离**（`quarantined(inferred_deferred)`）并计数，不落库、不静默。
//!    这一格是方案 §4 第五条的可执行形态，也是 §7 要报的"隔离占比"读数的来源。
//! 5. **状态先写 running 再调模型**：LLM 故障按类型分派（[`unit_level`]）——单元级
//!    （超时 / 答复不合格）逐格 `pending`（60s 退避）、试满上限就隔离，整批没到上限时按
//!    `Ok(Pending)` 交回；座位级（传输 / 额度）每轮 `pending` 并返回 `Err`，不隔离；
//!    抽取信封拒收 ⇒ 每轮 `rejected(<slug>)`；审核契约拒收 ⇒ 每轮 `quarantined(review_<slug>)`；
//!    写库失败 ⇒ 有草稿的轮 `pending(review_pending)`。除单元级那一支外，失败都返回 `Err`，不写半批。
//! 6. **每轮终态由该轮的判决决定**，优先级固定：程序闸拒收/冲突隔离 > 不确定 > 已写出 >
//!    审核全拒 > 草稿被丢/模型漏轮 > 空。
//! 7. **模型面写不出的东西由程序补**：`id`/`mode`/`confidence`/`valid_from_ms`/`subject_kind`/
//!    `text` 全在写库层（[`rc_memory::put_state_rows`]），`scope/owner/source_agent/device_id`
//!    只从 [`StatePlan`] 来。
//!
//! 与 event 的**形状差别**只有三处：载荷条数一格（`states[]`）、`inferred` 的分岔、
//! 同维度异值只有**迟到的那一条**走隔离（[`crate::review::gate::STATE_CONFLICT_CODE`]）——
//! 更新的那一条自 D10 起放行给审核，替代由写库层在同一事务里做
//! （`design/2026-11-01-state-conflict-adjudication.md` §3/§4）。

use std::collections::{HashMap, HashSet};

use rc_ledger::pass_state::{self, PassKey, PassState};
use rc_ledger::water;
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::owner::{self, OwnerCtx};
use rc_memory::review::{
    claim, enqueue, get, release, ReviewState, ReviewVerdict as QueueVerdict,
};
use rc_memory::{BatchCtx, Scope};
use rc_store::{LedgerHandle, MemoryHandle, StoreError, TranscriptHandle};
use rusqlite::types::ValueRef;
use rusqlite::{params, Row};
use serde_json::json;

use crate::batch::{
    count_system_turns_in_span, load_extract_ctx, load_turn_speakers, Batch, ExtractCtx, TurnRow,
};
use crate::pass_event::{append_agent_anchor, append_principal_anchor};
use crate::pass_prompt::{
    is_compaction_body, is_runtime_notice, state_system, state_user, COMPACT_SUMMARY_REASON,
    ExistingState, FactCandidate, PASS_STATE,
};
use crate::pass_state_contract::{parse_state_output_lenient, StateDraft};
use crate::review::gate::STATE_CONFLICT_CODE;
use crate::review::run::{run_item, RunError};
use crate::review::{self, ReviewMode};
use crate::DistillError;

/// 指纹里的 pipeline 那一段。v0.1 = 方案 §3 的第一版判据（主体泛化 + 值逐字）。
pub const STATE_PIPELINE: &str = "pass-state-v0.1";

/// 调用方给 0 时的默认窗宽（与 event 同值：批上限 8 的口径由 hub 的默认窗宽管，这里只兜零值）。
pub const DEFAULT_LIMIT: usize = 20;

/// LLM 故障的退避（与 event/fact 同一格）。
pub const RETRY_BACKOFF_MS: i64 = 60_000;

/// 故障分类（W19c2）：坏的是**这一格**，还是**座位**。
///
/// - 单元级（`true`）：`Timeout`（这一次开口太慢）与 `SchemaMismatch`（模型给的答复不合格）——
///   同一格再试未必变好，但别的格未必坏 ⇒ 逐格记账（[`pass_state::note_attempt_or_quarantine`]），
///   到上限的那一格隔离，其余按"还在退避"交回。这一批既不隔离整批、也不算整族失败。
/// - 座位级（`false`）：`BackendError`（传输）与 `QuotaExhausted`（额度）——坏的是一个座位，
///   整族再试也一样 ⇒ 记一次失败并按失败交回，由日路径的连失败闸收手。**不隔离**：
///   座位死掉时把这些单元逐个判成终态，等于把可恢复的活埋进书里。
///
/// 判据只读失败的类型，不读返回值的其余部分——四态枚举就是全部输入。
pub fn unit_level(failure: LlmFailure) -> bool {
    matches!(failure, LlmFailure::Timeout | LlmFailure::SchemaMismatch)
}

/// 审核判决的两格与写库失败那一格（与 event 同词；reason 列按 pass_id 分行，不冲突）。
const REVIEW_UNCERTAIN_REASON: &str = "review_uncertain";
const REVIEW_REJECT_REASON: &str = "review_reject";
/// 推导草稿的隔离词（方案 §4 第五条）。它不是抽取的 slug：这一条 draft 形状全对，
/// 只是值需要换词才说得出来 ⇒ 程序判它是推导。
const INFERRED_REASON: &str = "inferred_deferred";
/// 单条草稿被宽松 parser 丢掉后该轮的处置词。
const DROPPED_REASON: &str = "dropped";
/// 模型漏掉的轮（既没有合法草稿覆盖、也没被 `empty_turns` 宣告）。
const UNCOVERED_REASON: &str = "uncovered";

/// `state_assertion` 的 `text` 组装时用的中文维度名（消费语言契约：LLM 面用中文完整句）。
const DIMENSION_LABELS: [(&str, &str); 9] = [
    ("emotion", "情绪"),
    ("mood", "心情"),
    ("sentiment", "态度"),
    ("affect_episode", "情绪片段"),
    ("trust", "信任"),
    ("relationship", "关系"),
    ("health", "健康"),
    ("config", "配置"),
    ("progress", "进度"),
];

/// 一批 state 的计划。字段与 [`crate::pass_event::EventPlan`] 同形，多一格 `existing`：
/// 候选主体在当前库里的活状态（方案 §3 的 input 第三格）。
#[derive(Debug, Clone)]
pub struct StatePlan {
    pub batch: Batch,
    pub ctx_turns: ExtractCtx,
    pub system_turns_skipped: usize,
    pub ctx: BatchCtx,
    pub owner: OwnerCtx,
    pub scope: Scope,
    pub candidates: Vec<FactCandidate>,
    /// 候选主体已有的当前状态，只进 prompt。
    pub existing: Vec<ExistingState>,
}

/// 一批的读数。`drafts` 是**保留**的合法草稿数（`expressed`），`inferred` 是分岔去隔离的条数
/// （它们既不是 `drafts` 也不是 `dropped`），`dropped + inferred` 与 `drafts` 之和才是模型原始条数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateOutcome {
    pub turns: usize,
    pub drafts: usize,
    pub dropped: usize,
    pub uncovered: usize,
    /// 程序判为推导、进隔离的条数（方案 §7 的"隔离占比"分子）。
    pub inferred: usize,
    pub written: usize,
    /// 写库层的第二道幂等命中数（库里已有同 `(主体, 维度, 值)` 的活行）。
    pub duplicate: usize,
    pub rejected: usize,
    pub uncertain: usize,
    /// 真插入的 `state_assertion` 行数（重放为 0）。
    pub states: usize,
    pub mentions: usize,
    /// 程序闸判出的**迟到的**同维度异值条数（走隔离，不落库、不改旧行）。
    /// D10 之后同维度异值不再整类进这里：更晚的那一条算"来顶掉旧值"，照常送审、由写库层替代，
    /// 只有更早的那一条（迟到的旧话）留在这一格里等人裁。
    pub conflicts: usize,
    pub system_turns_skipped: usize,
    pub state: PassState,
    /// 这一批是不是"单元级失败试满上限、整批落成隔离"（W19c2）。**只在那一支为真**。
    ///
    /// 它与 `state` 不是一回事：`Quarantined` 另有来源（审核不确定、程序判出的推导件）——
    /// 那些是**质量**隔离，不是这一格卡死。日路径的连续隔离墙只认这一格。
    pub retry_exhausted: bool,
}

impl StateOutcome {
    /// 早退读数：写库面全 0，只带已经确定的那几格。
    #[allow(clippy::too_many_arguments)]
    fn early(
        turns: usize,
        drafts: usize,
        dropped: usize,
        uncovered: usize,
        inferred: usize,
        conflicts: usize,
        system_turns_skipped: usize,
        state: PassState,
    ) -> StateOutcome {
        StateOutcome {
            turns,
            drafts,
            dropped,
            uncovered,
            inferred,
            written: 0,
            duplicate: 0,
            rejected: 0,
            uncertain: 0,
            states: 0,
            mentions: 0,
            conflicts,
            system_turns_skipped,
            retry_exhausted: false,
            state,
        }
    }
}

/// state 的键：`family` 恒空串（它是单一 pass，没有调用变体）。
fn key(turn_uid: &str) -> PassKey<'_> {
    PassKey { turn_uid, pass_id: PASS_STATE, family: "" }
}

/// 选批读到的会话行（九列，与 event/naming/fact 同一组列；元组位置一错就是静默串值，
/// 所以这里用命名字段而不是元组）。
pub(crate) struct StateSessionRow {
    session_key: String,
    device_id: String,
    source_agent: String,
    project_hint: Option<String>,
    owner_agent: String,
    visibility: String,
    retention: String,
    scope_kind: String,
    scope_key: String,
}

/// 选批读到的一格表态（与 event 同形）。
type StatedRow = (PassState, Option<i64>, Option<String>, Option<String>);

/// 每轮落同一个失败态。
fn mark_every_turn(
    l: &LedgerHandle,
    uids: &[String],
    state: PassState,
    reason: Option<&str>,
    now_ms: i64,
) -> Result<(), DistillError> {
    for uid in uids {
        pass_state::mark_terminal(l, key(uid), state, reason, now_ms)?;
    }
    Ok(())
}

/// 程序闸拒收的 draft 的**证据轮**落 `rejected(<码>)`；冲突那一格落 `quarantined`（方案 §4：
/// 冲突不覆盖旧值，也不冒充"这条坏了"——它是"两条都可能是对的，等人裁"）。
fn mark_program_turns(
    l: &LedgerHandle,
    uids: &[String],
    turns: &[(String, &'static str)],
    now_ms: i64,
) -> Result<(), DistillError> {
    for (uid, code) in turns {
        if !uids.iter().any(|batch_uid| batch_uid == uid) {
            continue;
        }
        let state = if *code == STATE_CONFLICT_CODE {
            PassState::Quarantined
        } else {
            PassState::Rejected
        };
        pass_state::mark_terminal(l, key(uid), state, Some(code), now_ms)?;
    }
    Ok(())
}

/// 抽取完成、窄审还没由**本进程**执行时的逐轮终态。
/// 推导草稿的轮（`inferred_uids`）在这里落隔离——审核还没问过它，但它也不该重抽。
#[allow(clippy::too_many_arguments)]
fn mark_awaiting_or_extraction_terminal(
    l: &LedgerHandle,
    uids: &[String],
    draft_turns: &HashSet<&str>,
    inferred_uids: &HashSet<String>,
    uncovered: &HashSet<String>,
    dropped: usize,
    dropped_reasons: &HashMap<String, &'static str>,
    now_ms: i64,
) -> Result<(), DistillError> {
    for uid in uids {
        if draft_turns.contains(uid.as_str()) {
            pass_state::mark_awaiting_review(l, key(uid), now_ms)?;
        } else if inferred_uids.contains(uid) {
            pass_state::mark_terminal(
                l, key(uid), PassState::Quarantined, Some(INFERRED_REASON), now_ms,
            )?;
        } else if uncovered.contains(uid) {
            let reason = if dropped > 0 {
                dropped_reasons.get(uid).copied().unwrap_or(DROPPED_REASON)
            } else {
                UNCOVERED_REASON
            };
            pass_state::mark_terminal(l, key(uid), PassState::Rejected, Some(reason), now_ms)?;
        } else {
            pass_state::mark_terminal(l, key(uid), PassState::Empty, None, now_ms)?;
        }
    }
    Ok(())
}

/// 覆盖缺口的批内短 id 映射回真实 `turn_uid`（与 event 同一条纪律：短 id 不进台账）。
fn uncovered_uids(short: &[String], batch: &Batch) -> HashSet<String> {
    let mut out = HashSet::new();
    for (index, row) in batch.rows.iter().enumerate() {
        let reference = format!("t{}", index + 1);
        if short.iter().any(|item| item == &reference) {
            out.insert(row.turn_uid.clone());
        }
    }
    out
}

/// 被丢草稿的 `(真实轮, slug)`。
fn dropped_reasons(short: &[(String, &'static str)], batch: &Batch) -> HashMap<String, &'static str> {
    let mut out = HashMap::new();
    for (reference, slug) in short {
        for (index, row) in batch.rows.iter().enumerate() {
            if format!("t{}", index + 1) == *reference {
                out.entry(row.turn_uid.clone()).or_insert(*slug);
            }
        }
    }
    out
}

/// 推导草稿的 `(真实轮)`：同一轮既有一条直陈又有一条推导时，**直陈赢**——那一轮进审核队列，
/// 不因"模型还多写了一条推导"而被隔离。
fn inferred_uids(short: &[String], batch: &Batch, kept: &HashSet<String>) -> HashSet<String> {
    let mut out = HashSet::new();
    for (index, row) in batch.rows.iter().enumerate() {
        let reference = format!("t{}", index + 1);
        if short.iter().any(|item| item == &reference) && !kept.contains(&row.turn_uid) {
            out.insert(row.turn_uid.clone());
        }
    }
    out
}

/// 判空：模型说这一批什么都没有（保留、丢弃、缺口都没有，剩下的只有隔离）。
fn judged_empty(kept: usize, dropped: usize, uncovered: usize) -> bool {
    kept == 0 && dropped == 0 && uncovered == 0
}

/// 候选主体在当前库里的活状态（只取候选池里的锚，上限 40 行）。
///
/// 为什么要给模型看：同一主体同一维度的**换值**是冲突（方案 §4 第六条），而模型看不见现值
/// 就会把换值当成新状态写进来；看见了，它可以在那一轮写 `empty_turns`。
fn load_existing_states(
    m: &MemoryHandle,
    candidates: &[FactCandidate],
) -> Result<Vec<ExistingState>, DistillError> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let mut out: Vec<ExistingState> = Vec::new();
    {
        let mut st = m.conn().prepare(
            "SELECT subject_anchor_id, dimension, value_after FROM state_assertion
              WHERE valid_to_ms IS NULL
              ORDER BY valid_from_ms DESC, id LIMIT 120",
        )?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        for row in rows {
            let (anchor_id, dimension, value) = row?;
            let Some(candidate) = candidates.iter().find(|c| c.anchor_id == anchor_id) else {
                continue;
            };
            out.push(ExistingState {
                subject_canonical: candidate.canonical.clone(),
                dimension,
                value,
            });
            if out.len() >= 40 {
                break;
            }
        }
    }
    Ok(out)
}

/// 选一批该跑 state 的轮次。判据（含会话清单的排序：最新到达钟倒序，同刻按
/// `(session_key, device_id)`）与 [`crate::pass_event::select_event_batch`] 逐格同向，
/// 三处差别写在这里：
/// - 状态键是 `(turn, 'state', '')`——它读不到、也改不动 event 的那一行；
/// - 主体只能是有锚的实体，而自锚/主人锚两格与 event 同样在场，所以这里**不设**
///   "候选池空就跳过"的分支（那是一条永远不会红的死代码）；
/// - 多读一段库里活着的当前状态进 prompt（[`load_existing_states`]）。
pub fn select_state_batch(
    t: &TranscriptHandle,
    l: &LedgerHandle,
    m: &MemoryHandle,
    limit: usize,
    now_ms: i64,
) -> Result<Option<StatePlan>, DistillError> {
    let limit = if limit == 0 { DEFAULT_LIMIT } else { limit };
    let current_version = pass_state::prompt_version(&state_system());
    let stated: HashMap<String, StatedRow> = {
        let mut st = l.conn().prepare(
            "SELECT turn_uid, state, next_retry_at_ms, reason, prompt_version
             FROM extract_pass_state
             WHERE pass_id=?1 AND family=''",
        )?;
        let got = st.query_map(params![PASS_STATE], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        let mut map = HashMap::new();
        for row in got {
            let (uid, raw, next_retry, reason, version) = row?;
            let state = PassState::parse(&raw)
                .ok_or_else(|| StoreError::Sql(format!("pass_state: unreadable state {raw}")))?;
            map.insert(uid, (state, next_retry, reason, version));
        }
        map
    };

    let mut sessions: Vec<StateSessionRow> = Vec::new();
    {
        let mut st = t.conn().prepare(
            "SELECT session_key, device_id, source_agent, project_hint,
                    COALESCE(owner_agent_id, source_agent) AS owner_agent,
                    COALESCE(visibility, 'private_agent') AS visibility,
                    COALESCE(retention, 'default') AS retention,
                    COALESCE(scope_kind, 'project') AS scope_kind,
                    COALESCE(scope_key, project_hint, session_key) AS scope_key
             FROM sessions
             ORDER BY (SELECT MAX(t.ingested_at_ms)
                         FROM turns t
                        WHERE t.session_key = sessions.session_key
                          AND t.device_id   = sessions.device_id) DESC,
                      session_key, device_id",
        )?;
        let rows = st.query_map([], |r: &Row| {
            let hint: Option<String> = match r.get_ref(3)? {
                ValueRef::Null => None,
                ValueRef::Text(v) => Some(String::from_utf8_lossy(v).to_string()),
                _ => None,
            };
            Ok(StateSessionRow {
                session_key: r.get(0)?,
                device_id: r.get(1)?,
                source_agent: r.get(2)?,
                project_hint: hint,
                owner_agent: r.get(4)?,
                visibility: r.get(5)?,
                retention: r.get(6)?,
                scope_kind: r.get(7)?,
                scope_key: r.get(8)?,
            })
        })?;
        for r in rows {
            sessions.push(r?);
        }
    }

    for s in sessions {
        let StateSessionRow {
            session_key,
            device_id,
            source_agent,
            project_hint,
            owner_agent,
            visibility,
            retention,
            scope_kind,
            scope_key,
        } = s;
        let principal = rc_ledger::transcript::resolve_principal(t, &session_key, &device_id)?;
        let owner_principal_id = principal.as_ref().map(|p| p.principal_id.clone());
        let principal_name = principal.and_then(|p| p.principal_name);
        let mut rows: Vec<TurnRow> = Vec::new();
        {
            let mut st = t.conn().prepare(
                "SELECT turn_uid, role, body, created_at_ms, ingested_at_ms
                 FROM turns
                 WHERE session_key=?1 AND device_id=?2 AND role <> 'system'
                 ORDER BY created_at_ms, turn_uid",
            )?;
            let got = st.query_map(params![session_key, device_id], |r: &Row| {
                Ok(TurnRow {
                    turn_uid: r.get(0)?,
                    role: r.get(1)?,
                    body: r.get(2)?,
                    created_at_ms: r.get(3)?,
                    ingested_at_ms: r.get(4)?,
                    speaker: None,
                    speaker_anchor_id: None,
                })
            })?;
            for r in got {
                let row = r?;
                if !crate::pass_event::reselectable(stated.get(&row.turn_uid), now_ms, &current_version) {
                    continue;
                }
                if is_compaction_body(&row.body) {
                    pass_state::mark_terminal(
                        l, key(&row.turn_uid), PassState::Rejected,
                        Some(COMPACT_SUMMARY_REASON), now_ms,
                    )?;
                    continue;
                }
                if is_runtime_notice(&row.body) {
                    pass_state::mark_terminal(
                        l, key(&row.turn_uid), PassState::Rejected,
                        Some(crate::pass_prompt::NOTICE_REASON), now_ms,
                    )?;
                    continue;
                }
                rows.push(row);
                if rows.len() == limit {
                    break;
                }
            }
        }
        if rows.is_empty() {
            continue;
        }
        let ctx = BatchCtx { source_agent, device_id };
        let ctx_turns = load_extract_ctx(t, &session_key, &ctx.device_id, &rows)?;
        let system_turns_skipped =
            count_system_turns_in_span(t, &session_key, &ctx.device_id, &rows)?;
        // 借用顺序：说话人回填要 `&mut rows`，所以它必须排在 `load_extract_ctx`
        // 与 `count_system_turns_in_span`（两者只借 `&rows`）之后。
        let speakers = load_turn_speakers(m, &mut rows)?;
        let owner = OwnerCtx {
            owner_principal_id,
            owner_agent_id: owner_agent,
            visibility: owner::parse_visibility(&visibility),
            retention: owner::normalize_retention(&retention),
        };
        let scope = Scope { kind: owner::normalize_scope_kind(&scope_kind), key: scope_key };
        let mut candidates =
            crate::candidates::load_candidates(m, &owner.owner_agent_id, &scope)?;
        let pool_len = candidates.len();
        append_agent_anchor(m, &mut candidates, &ctx, &owner, &scope, now_ms)?;
        append_principal_anchor(
            m, &mut candidates, &owner, &scope, principal_name.as_deref(), now_ms,
        )?;
        let tail = candidates.len() - pool_len;
        crate::candidates::insert_speaker_candidates(
            m, &mut candidates, tail, &owner.owner_agent_id, &scope, &speakers,
        )?;
        crate::candidates::insert_mentioned_candidates(
            m, &mut candidates, tail, &owner.owner_agent_id, &scope, &rows,
        )?;
        crate::candidates::order_by_evidence(m, &mut candidates, &rows)?;
        let existing = load_existing_states(m, &candidates)?;
        return Ok(Some(StatePlan {
            batch: Batch {
                session_key,
                device_id: ctx.device_id.clone(),
                source_agent: ctx.source_agent.clone(),
                project_hint,
                rows,
            },
            ctx_turns,
            system_turns_skipped,
            ctx,
            owner,
            scope,
            candidates,
            existing,
        }));
    }
    Ok(None)
}

/// 组装 `state_assertion.text`（程序写的消费句，见 [`crate::pass_prompt::STATE_DIMENSIONS`] 那一格
/// 的注释：状态行的正文不是模型的第二件事）。
pub(crate) fn compose_text(subject_canonical: &str, dimension: &str, value_after: &str) -> String {
    let label = DIMENSION_LABELS
        .iter()
        .find(|(slug, _)| *slug == dimension)
        .map(|(_, label)| *label)
        .unwrap_or(dimension);
    format!("{subject_canonical}：{label}为「{value_after}」。")
}

/// 一条草稿的载荷行：必须**只凭自身就能重放**审核输入（与 event 同一条纪律）。
fn payload_item(draft: &StateDraft, batch: &Batch) -> serde_json::Value {
    let evidence: Vec<serde_json::Value> = draft
        .evidence_turn_uids
        .iter()
        .map(|uid| {
            let role = batch.row(uid).map(|row| row.role.clone()).unwrap_or_default();
            json!({"turn_uid": uid, "anchor_role": role})
        })
        .collect();
    json!({
        "turn_uid": draft.turn_uid,
        "subject_anchor_id": draft.subject_anchor_id,
        "subject_canonical": draft.subject_canonical,
        // 状态的谓词格由程序盖（与 event 的 `predicate=action` 同一处真相）。
        "predicate": "state",
        "dimension": draft.dimension,
        "value_after": draft.value_after,
        "mode": draft.mode,
        "quote": draft.quote,
        "evidence": evidence,
        "text": compose_text(&draft.subject_canonical, &draft.dimension, &draft.value_after),
    })
}

/// 跑一批 state（带审核模式）。`mode=EnqueueOnly` 只改一支：闸 pass 且 item 是
/// `pending_review` 的那一批不再问审核模型、不写记忆行。
#[allow(clippy::too_many_arguments)]
pub fn run_state_with(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    l: &LedgerHandle,
    plan: &StatePlan,
    backend: &dyn LlmBackend,
    now_ms: i64,
    mode: ReviewMode,
) -> Result<StateOutcome, DistillError> {
    let uids: Vec<String> = plan.batch.rows.iter().map(|row| row.turn_uid.clone()).collect();
    if uids.is_empty() {
        // 空批不该从 `select_state_batch` 出来；直接调用方给的空批没有"批内 system 轮"可言。
        return Ok(StateOutcome::early(0, 0, 0, 0, 0, 0, 0, PassState::Empty));
    }

    let system = state_system();
    let user = state_user(&plan.batch, &plan.candidates, &plan.ctx_turns, &plan.existing);
    let fingerprint = water::extraction_fingerprint(
        STATE_PIPELINE,
        backend.slug().unwrap_or("unknown"),
        &system,
        &user,
    );
    let prompt_version = pass_state::prompt_version(&system);
    let run_id = format!("state-{now_ms}");
    for uid in &uids {
        pass_state::mark_running_with_prompt_version(
            l, key(uid), &fingerprint, Some(&prompt_version), &run_id, now_ms)?;
    }

    // ---- 第一次调用：抽取 ----------------------------------------------------
    let prompt = Prompt { system, user, derive: false };
    let raw = match backend.complete(&prompt) {
        Ok(raw) => raw,
        Err(failure) => {
            // 运维失败那一支：轮是 pending，不是 rejected。
            // 座位级（传输 / 额度）坏的是座位，不是这一批数据 ⇒ 记一次失败后 `Err`，
            // 交给日路径的连失败闸；**不隔离**（W19c2）。
            if !unit_level(failure) {
                for uid in &uids {
                    pass_state::note_attempt(l, key(uid), RETRY_BACKOFF_MS, now_ms)?;
                }
                return Err(DistillError::Llm(failure));
            }
            // 单元级（超时 / 答复不合格）：试满 `MAX_PASS_ATTEMPTS` 次的那一格改为隔离，
            // 整批都隔离了就按**成功**交回（`state=quarantined`、落库计数全 0）；
            // 只有一部分到上限 ⇒ `state=pending`：那些轮在退避里，整族继续选下一批。
            let mut all_quarantined = true;
            for uid in &uids {
                all_quarantined &= pass_state::note_attempt_or_quarantine(
                    l, key(uid), RETRY_BACKOFF_MS, now_ms)? == PassState::Quarantined;
            }
            if all_quarantined {
                // 整批都到了上限 ⇒ 这是"重试耗尽"那一支：只有这一格能让日路径的
                // 连续隔离墙推进（W19c2）。`state=quarantined` 另有来源，脚本不读它。
                let mut out = StateOutcome::early(
                    uids.len(), 0, 0, 0, 0, 0,
                    plan.system_turns_skipped, PassState::Quarantined,
                );
                out.retry_exhausted = true;
                return Ok(out);
            }
            return Ok(StateOutcome::early(
                uids.len(), 0, 0, 0, 0, 0,
                plan.system_turns_skipped, PassState::Pending,
            ));
        }
    };
    let lenient = match parse_state_output_lenient(&raw, &plan.batch, &plan.candidates) {
        Ok(lenient) => lenient,
        Err(error) => {
            let _ = pass_state::record_artifact(
                l, PASS_STATE, "", "extract", &run_id, &uids[0], &raw, now_ms);
            mark_every_turn(l, &uids, PassState::Rejected, Some(error.slug()), now_ms)?;
            return Err(DistillError::State(error));
        }
    };
    let dropped = lenient.dropped.len();
    let drafts: Vec<StateDraft> = lenient.drafts;
    let uncovered = uncovered_uids(&lenient.uncovered, &plan.batch);
    let dropped_map = dropped_reasons(&lenient.dropped_turns, &plan.batch);
    let kept_turns: HashSet<String> = drafts.iter().map(|d| d.turn_uid.clone()).collect();
    let inferred_set = inferred_uids(&lenient.inferred_turns, &plan.batch, &kept_turns);
    let inferred = inferred_set.len();
    if judged_empty(drafts.len(), dropped, uncovered.len()) {
        // 空批是合法结果：不调审核、不入队，逐轮按判空落终态。
        mark_awaiting_or_extraction_terminal(
            l, &uids, &HashSet::new(), &inferred_set, &uncovered, dropped, &dropped_map, now_ms,
        )?;
        let batch_state = if !inferred_set.is_empty() {
            PassState::Quarantined
        } else {
            PassState::Empty
        };
        return Ok(StateOutcome::early(
            uids.len(), 0, dropped, uncovered.len(), inferred, 0,
            plan.system_turns_skipped, batch_state,
        ));
    }
    if dropped > 0 {
        for uid in dropped_map.keys() {
            if pass_state::record_artifact(
                l, PASS_STATE, "", "extract", &run_id, uid, &raw, now_ms).is_ok() {
                break;
            }
        }
    }
    let draft_turns: HashSet<&str> = drafts.iter().map(|draft| draft.turn_uid.as_str()).collect();

    // ---- 待审队列：入队 + 程序闸 --------------------------------------------
    let mut item_id: Option<i64> = None;
    let mut claimable = false;
    // 批级早退也要能报出冲突条数（在早退分支里赋值，正常路径在引擎读数之后算）。
    let mut conflicts = 0usize;
    let mut program_turns: Vec<(String, &'static str)> = Vec::new();
    if !drafts.is_empty() {
        let payload_items: Vec<serde_json::Value> =
            drafts.iter().map(|draft| payload_item(draft, &plan.batch)).collect();
        let payload = review::pass_payload(
            PASS_STATE,
            "",
            &fingerprint,
            &plan.batch,
            &plan.ctx,
            &plan.owner,
            &plan.scope,
            serde_json::Value::Array(payload_items),
            None,
        );
        let owner_json = review::owner_json(&plan.owner, &plan.scope);
        let queued_item = review::new_item(PASS_STATE, "", &plan.batch, owner_json, &payload);
        let queued = enqueue(m, &queued_item, now_ms)?;
        let item = get(m, queued.id)?.ok_or_else(|| {
            rc_memory::MemError::Sql("review: enqueued state item vanished".to_string())
        })?;
        match queued.state {
            ReviewState::PendingReview => {
                let report = review::gate_report(m, t, &item, now_ms)?;
                program_turns = report.rejected_turns();
                if !review::program_decided(m, queued.id)? {
                    if report.batch_code.is_some() || report.all_failed() {
                        let code = report.code.unwrap_or("payload_shape");
                        review::record_program(
                            m, queued.id, QueueVerdict::Reject, code,
                            &item.payload_sha256, now_ms,
                        )?;
                        // 批级前置坏 ⇒ 整批同一个词（与 event 逐字相同的一条早退）。
                        // 全是坏 draft ⇒ 逐轮按**各自**的码落：只有"迟到的同维度异值"那一格走隔离
                        // （D10 §3 的第二支），其余仍是 `rejected(<码>)`。**其余批内轮也必须到终态**
                        // （债 #15：它们已经 mark_running，这里早退不推就停 `running`、reason=NULL，
                        // 只能等 reclaim_running 兜底）——映射与正常路径的"隔离(推导) > 丢/漏 > 空"
                        // 同序；被拒草稿的轮上面已经落定，这里跳过，不改它们的码。
                        let batch_state = if report.batch_code.is_some() {
                            mark_every_turn(l, &uids, PassState::Rejected, Some(code), now_ms)?;
                            PassState::Rejected
                        } else {
                            let turns = report.rejected_turns();
                            conflicts = turns
                                .iter()
                                .filter(|(_, one)| *one == STATE_CONFLICT_CODE)
                                .count();
                            mark_program_turns(l, &uids, &turns, now_ms)?;
                            for uid in &uids {
                                if turns.iter().any(|(marked, _)| marked == uid) {
                                    continue;
                                }
                                let (turn_state, reason) = if inferred_set.contains(uid) {
                                    (PassState::Quarantined, Some(INFERRED_REASON))
                                } else if uncovered.contains(uid) {
                                    let reason = if dropped > 0 {
                                        dropped_map.get(uid).copied().unwrap_or(DROPPED_REASON)
                                    } else {
                                        UNCOVERED_REASON
                                    };
                                    (PassState::Rejected, Some(reason))
                                } else {
                                    (PassState::Empty, None)
                                };
                                pass_state::mark_terminal(l, key(uid), turn_state, reason, now_ms)?;
                            }
                            if conflicts == turns.len() && conflicts > 0 {
                                PassState::Quarantined
                            } else {
                                PassState::Rejected
                            }
                        };
                        return Ok(StateOutcome::early(
                            uids.len(), drafts.len(), dropped, uncovered.len(), inferred,
                            conflicts, plan.system_turns_skipped, batch_state,
                        ));
                    }
                    review::record_program(
                        m, queued.id, QueueVerdict::Pass, review::PROGRAM_PASS,
                        &item.payload_sha256, now_ms,
                    )?;
                }
                mark_program_turns(l, &uids, &program_turns, now_ms)?;
                if mode == ReviewMode::EnqueueOnly {
                    mark_awaiting_or_extraction_terminal(
                        l, &uids, &draft_turns, &inferred_set, &uncovered, dropped,
                        &dropped_map, now_ms,
                    )?;
                    mark_program_turns(l, &uids, &program_turns, now_ms)?;
                    return Ok(StateOutcome::early(
                        uids.len(), drafts.len(), dropped, uncovered.len(), inferred, 0,
                        plan.system_turns_skipped, PassState::Pending,
                    ));
                }
                item_id = Some(queued.id);
                claimable = true;
            }
            ReviewState::Approved | ReviewState::Applied => {
                item_id = Some(queued.id);
                claimable = queued.state == ReviewState::Approved;
            }
            ReviewState::Rejected => {
                let code = item.meta.program_code.as_deref().unwrap_or(REVIEW_REJECT_REASON);
                mark_every_turn(l, &uids, PassState::Rejected, Some(code), now_ms)?;
                return Ok(StateOutcome::early(
                    uids.len(), drafts.len(), 0, 0, inferred, 0,
                    plan.system_turns_skipped, PassState::Rejected,
                ));
            }
            ReviewState::Quarantined => {
                mark_every_turn(
                    l, &uids, PassState::Quarantined, Some(REVIEW_UNCERTAIN_REASON), now_ms,
                )?;
                return Ok(StateOutcome::early(
                    uids.len(), drafts.len(), 0, 0, inferred, 0,
                    plan.system_turns_skipped, PassState::Quarantined,
                ));
            }
            ReviewState::Stale => {
                mark_every_turn(l, &uids, PassState::Rejected, Some("stale"), now_ms)?;
                return Ok(StateOutcome::early(
                    uids.len(), drafts.len(), 0, 0, inferred, 0,
                    plan.system_turns_skipped, PassState::Rejected,
                ));
            }
        }
    }

    // ---- 占位（H1b）----------------------------------------------------------
    let mut lease_owner: Option<String> = None;
    if let (Some(id), true) = (item_id, claimable) {
        let owner = format!("distill-{}-{now_ms}", std::process::id());
        if claim(m, id, &owner, review::REVIEW_LEASE_MS, now_ms)? {
            lease_owner = Some(owner);
        } else {
            mark_awaiting_or_extraction_terminal(
                l, &uids, &draft_turns, &inferred_set, &uncovered, dropped, &dropped_map, now_ms,
            )?;
            mark_program_turns(l, &uids, &program_turns, now_ms)?;
            return Ok(StateOutcome::early(
                uids.len(), drafts.len(), dropped, uncovered.len(), inferred, 0,
                plan.system_turns_skipped, PassState::Pending,
            ));
        }
    }

    // ---- 第二次调用 + 判定 + 写库：整段交给 review::run -----------------------
    let run = match item_id {
        Some(id) => {
            let result = run_item(m, t, l, id, backend, now_ms);
            if let Some(owner) = lease_owner.as_deref() {
                let _ = release(m, id, owner);
            }
            match result {
                Ok(run) => Some(run),
                Err(RunError::Llm(failure)) => {
                    // 审核开不了口：轮回到 pending 等退避。座位级（传输 / 额度）记一次失败
                    // 后 `Err`，不隔离（W19c2）。
                    if !unit_level(failure) {
                        for uid in &uids {
                            pass_state::note_attempt(l, key(uid), RETRY_BACKOFF_MS, now_ms)?;
                        }
                        return Err(DistillError::Llm(failure));
                    }
                    // 单元级：试满上限的那一格隔离，整批隔离了就按成功交回；否则按 pending
                    // 交回——退避中的那几轮不会被立刻重选，整族继续（与抽取那一支同一条政策）。
                    let mut all_quarantined = true;
                    for uid in &uids {
                        all_quarantined &= pass_state::note_attempt_or_quarantine(
                            l, key(uid), RETRY_BACKOFF_MS, now_ms)? == PassState::Quarantined;
                    }
                    if all_quarantined {
                        // 整批都到了上限 ⇒ 这是"重试耗尽"那一支（W19c2）。
                        let mut out = StateOutcome::early(
                            uids.len(), 0, 0, 0, 0, 0,
                            plan.system_turns_skipped, PassState::Quarantined,
                        );
                        out.retry_exhausted = true;
                        return Ok(out);
                    }
                    return Ok(StateOutcome::early(
                        uids.len(), 0, 0, 0, 0, 0,
                        plan.system_turns_skipped, PassState::Pending,
                    ));
                }
                Err(RunError::Review(e)) => {
                    let reason = match &e {
                        DistillError::State(err) => err.review_reason(),
                        _ => "store".to_string(),
                    };
                    mark_every_turn(l, &uids, PassState::Quarantined, Some(&reason), now_ms)?;
                    return Err(e);
                }
                Err(RunError::Store(e)) => {
                    for uid in &uids {
                        if draft_turns.contains(uid.as_str()) {
                            pass_state::mark_awaiting_review(l, key(uid), now_ms)?;
                        }
                    }
                    mark_program_turns(l, &uids, &program_turns, now_ms)?;
                    return Err(e);
                }
                Err(RunError::Conflict) => {
                    // 另一写者已推进状态：本进程什么都没做成，读数全 0（与 event 同一条口径，
                    // 连"隔离了几条"都不报——那一格是它的成绩，不是我的）。
                    return Ok(StateOutcome::early(0, 0, 0, 0, 0, 0, 0, PassState::Pending));
                }
                // 状态不对 / 找不到 item / 别的 pass 的分派：接线错误，不猜终态、不动台账。
                Err(RunError::Unsupported)
                | Err(RunError::MissingItem)
                | Err(RunError::ReviewNaming(_))
                | Err(RunError::ReviewRows(_))
                // 波三刀 A：lesson 的审核 parser 只由 lesson item 产生，state 走不到 —— 接线错误。
                | Err(RunError::ReviewLesson(_)) => {
                    return Err(DistillError::Memory(rc_memory::MemError::Sql(
                        "review run: unexpected state".to_string(),
                    )));
                }
            }
        }
        None => None,
    };
    let (item_state, verdicts, fail_closed_reason, written, duplicate, states, mentions,
         engine_program_turns) = match run {
        Some(run) => (
            run.item_state,
            run.verdicts,
            run.fail_closed_reason,
            run.written,
            run.duplicate,
            // 状态 pass 复用引擎的 `facts` 格放**插入的状态行数**（与 naming 复用同一格放
            // 锚数同一条先例；detail 里那一格对 state 写的是 `states=`，见 run.rs）。
            run.facts,
            run.mentions,
            run.program_rejected_turns,
        ),
        None => (ReviewState::Approved, Some(Vec::new()), None, 0, 0, 0, 0, Vec::new()),
    };
    if !engine_program_turns.is_empty() {
        program_turns = engine_program_turns;
    }
    conflicts = program_turns
        .iter()
        .filter(|(_, code)| *code == STATE_CONFLICT_CODE)
        .count();

    let Some(verdicts) = verdicts else {
        if item_state == ReviewState::Applied {
            mark_every_turn(l, &uids, PassState::Done, None, now_ms)?;
            let mut out = StateOutcome::early(
                uids.len(), drafts.len(), dropped, uncovered.len(), inferred, conflicts,
                plan.system_turns_skipped, PassState::Done,
            );
            out.mentions = mentions;
            out.duplicate = duplicate;
            out.written = written;
            out.states = states;
            return Ok(out);
        }
        mark_every_turn(
            l, &uids, PassState::Quarantined,
            Some(fail_closed_reason.unwrap_or(review::REVIEW_VERDICTS_MISSING)), now_ms,
        )?;
        return Ok(StateOutcome::early(
            uids.len(), drafts.len(), dropped, uncovered.len(), inferred, conflicts,
            plan.system_turns_skipped, PassState::Quarantined,
        ));
    };

    // ---- 按判决分流 ----------------------------------------------------------
    let by_ref: HashMap<&str, QueueVerdict> = verdicts
        .iter()
        .map(|(reference, verdict)| (reference.as_str(), *verdict))
        .collect();
    let mut rejected = 0usize;
    let mut uncertain = 0usize;
    let mut all_draft_turns: HashSet<String> = HashSet::new();
    let mut pass_turns: HashSet<String> = HashSet::new();
    let mut uncertain_turns: HashSet<String> = HashSet::new();
    // 判决的键是**位置** ref（`s1..sN` = 载荷 items 顺序，见 `review::replay::draft_refs`），
    // 不是模型自己写的 `ref`：模型可以写 s7 排在第一位，位置才是两侧唯一的共同真相。
    for (index, draft) in drafts.iter().enumerate() {
        all_draft_turns.insert(draft.turn_uid.clone());
        let reference = format!("s{}", index + 1);
        match by_ref.get(reference.as_str()).copied().unwrap_or(QueueVerdict::Reject) {
            QueueVerdict::Pass => {
                pass_turns.insert(draft.turn_uid.clone());
            }
            QueueVerdict::Reject => rejected += 1,
            QueueVerdict::Uncertain => {
                uncertain += 1;
                uncertain_turns.insert(draft.turn_uid.clone());
            }
        }
    }

    // ---- 每轮终态：不确定 > 已写出 > 审核全拒 > 隔离(推导) > 丢/漏 > 空 -------
    let mut states_per_turn: Vec<PassState> = Vec::with_capacity(uids.len());
    for uid in &uids {
        if program_turns.iter().any(|(turn, _)| turn == uid) {
            states_per_turn.push(if inferred_set.contains(uid) {
                PassState::Quarantined
            } else {
                PassState::Rejected
            });
            continue;
        }
        let (turn_state, reason) = if uncertain_turns.contains(uid) {
            (PassState::Quarantined, Some(REVIEW_UNCERTAIN_REASON))
        } else if pass_turns.contains(uid) {
            (PassState::Done, None)
        } else if all_draft_turns.contains(uid) {
            (PassState::Rejected, Some(REVIEW_REJECT_REASON))
        } else if inferred_set.contains(uid) {
            (PassState::Quarantined, Some(INFERRED_REASON))
        } else if uncovered.contains(uid) {
            let reason = if dropped > 0 {
                dropped_map.get(uid).copied().unwrap_or(DROPPED_REASON)
            } else {
                UNCOVERED_REASON
            };
            (PassState::Rejected, Some(reason))
        } else {
            (PassState::Empty, None)
        };
        pass_state::mark_terminal(l, key(uid), turn_state, reason, now_ms)?;
        states_per_turn.push(turn_state);
    }

    let state = if states_per_turn.contains(&PassState::Quarantined) {
        PassState::Quarantined
    } else if states_per_turn.iter().all(|s| *s == PassState::Empty) {
        PassState::Empty
    } else if states_per_turn.contains(&PassState::Done) {
        PassState::Done
    } else {
        PassState::Rejected
    };

    Ok(StateOutcome {
        turns: uids.len(),
        drafts: drafts.len(),
        dropped,
        uncovered: uncovered.len(),
        inferred,
        written,
        duplicate,
        rejected,
        uncertain,
        states,
        mentions,
        conflicts,
        system_turns_skipped: plan.system_turns_skipped,
        retry_exhausted: false,
        state,
    })
}

