//! 波三刀 A：第 6 pass（lesson）的库面——**从"记错"起步**的最小闭环。
//!
//! 设计真源：`design/2026-11-08-g1-reuse-lesson-design.md` §波次三。这一段只回答一件事：
//! 一次抽取**已经被判错**的那几轮，能不能折成一条可复用的经验卡。它不做泛化的"经验总结"：
//! 候选源只有"程序可识别的错"，而且这个错在库里已经是一条**成熟的事实**——
//! `memory.db.review_item` 的 `state='rejected'` 行（审核驳回或程序闸整批驳回之后，
//! 这一批一条记忆行都没有）。`program_code`（闸的码）/ `review_reject`（审核的驳回）
//! 就是程序侧的错因，`turn_uids_json` 就是"哪几轮犯了错"的最小证据。
//!
//! 四步与 event 同形（选批 → 窄 prompt → 独立审核 → 落卡），差别只有两处：
//!
//! 1. **幂等落在两处，各管一段**。`extract_pass_state`（波三修 1 起 `lesson` 进
//!    `PASS_IDS`）回答"这一轮已经有结论了"：每个终局（空产出 / 契约拒收 / 审核完成
//!    / 隔离）都把这一批的**每一轮**写成一条终态行，选批据此把已盖棺的轮从候选里剔掉；
//!    队列这一侧由 `review_item.batch_key` 的唯一位承担——它管的是"已入队但还没到终态"
//!    的那一段（`--no-review` 留下的 `pending_review`、审核链上的 `approved`），
//!    这一类 item 没有终态行可读，不靠队列键就会被下一夜重选、重烧一次抽取。
//!    两处都不是对方的备份：前者是状态机（按轮），后者是队列唯一键（按批）。
//! 2. **程序闸不跑**（与 rows 同一支）：lesson 的载荷不是 fact/event 的 `items` 形状，
//!    `review::gate` 对它是 `payload_shape` ⇒ 隔离。审核输入由 `review::replay` 的
//!    `Lesson` 分支自己重建并校验（轮存在、批一致、owner 六键），闸的"确定性前置"这一
//!    职责由重放承担；EDV default-reject 仍由审核判决兑现（只写 pass 的草稿）。
//!
//! 空批是**合法零成本**：没有候选 ⇒ `Ok(None)`，一次模型都不调、一行都不写。抽取判空
//! （模型回 `lessons=[]`）也是合法产出（设计 §波次三判据：无错轮产出 0 卡），但那是一次
//! **跑过的批**：它落 `empty` 终态，下一夜不再重选同一组。

use crate::batch::Batch;
use crate::pass_lesson_contract::{parse_lesson_output, LessonDraft, LessonPassError, PASS_LESSON};
use crate::pass_prompt::{lesson_system, lesson_user};
use crate::review::run::{promote_applied_cards, run_item, RunError};
use crate::review::ReviewMode;
use crate::DistillError;
use rc_ledger::pass_state::{self, PassState};
use rc_ledger::water;
use rc_llm::{LlmBackend, Prompt};
use rc_memory::owner::{self, OwnerCtx};
use rc_memory::review::{enqueue, get, ReviewState};
use rc_memory::{BatchCtx, Scope};
use rc_store::{LedgerHandle, MemoryHandle, StoreError, TranscriptHandle};
use rusqlite::{params, Row};
use serde_json::json;
use std::collections::{HashMap, HashSet};

/// lesson 抽取的 pipeline 名（指纹的第一格，与 fact/event 并列）。
pub const LESSON_PIPELINE: &str = "pass-lesson-v0.1";

/// 调用方给 0 时的默认窗宽。比 event 的 20 小：一段错误通常只有几轮，
/// 10 轮已够看清"犯了什么错"，再长就变成给模型读整段会话。
pub const DEFAULT_LIMIT: usize = 10;

/// 一批最多带几轮。上界是**输入形状**的上界，不是经验数字：一段错几轮就够，
/// 多了只会稀释"哪一轮犯了什么错"。
pub const LESSON_MAX_TURNS: usize = 10;

/// 审核驳回（非程序闸）落在 `program_code` 空串时的错误族词，与 `review` 的驳回码同词。
pub const REVIEW_REJECT_CODE: &str = "review_reject";

/// 候选源里认的 pass 闭集（"谁的错"认抽取 pass 与审核侧的行 pass `rows`；`report`/`reuse`
/// 是消费者反馈，它们本身就是"对某条记忆有意见"，不是抽取失误；`time` 的错是解析器版本
/// 问题，不折成经验卡）。
pub const LESSON_SOURCE_PASSES: [&str; 5] = ["naming", "fact", "event", "state", "rows"];

/// 一批 lesson 的计划：这一段的轮、程序错因、归属与 scope。
///
/// 全部来自程序（`review_item` 的驳回行 + `sessions` 的归属列 + `turns` 的正文），
/// 模型一个字段都递不进来。
#[derive(Debug, Clone)]
pub struct LessonPlan {
    pub batch: Batch,
    /// 程序侧的错误族：闸的码（`number_not_grounded` 等）或 `review_reject`。
    pub error_code: String,
    pub ctx: BatchCtx,
    pub owner: OwnerCtx,
    pub scope: Scope,
    /// 这一组候选覆盖了几条被驳回的 `review_item`（读数；不进契约）。
    pub source_items: usize,
    /// 组内被挡在批外的 `role='system'` 轮数（工具输出不是 utterance；与 event 同一条读数）。
    pub system_turns_skipped: usize,
}

/// lesson 运行的失败面。
///
/// 不复用 `DistillError` 的新变体：那个枚举的读者在 `rc-cycle` 有一处闭集匹配，加一格就
/// 要改这把刀的文件集之外。三支各归各的调用面（与 hub 的退出码一一对应）：
/// `Llm` ⇒ 3、`Contract` ⇒ 2、`Store` ⇒ 3。
#[derive(Debug)]
pub enum LessonRunError {
    Llm(rc_llm::LlmFailure),
    /// 抽取或审核的契约拒收（两支共用一个变体：都只是"这段回复读不出来"）。
    Contract(LessonPassError),
    /// 写库/队列/句柄错误（含引擎的 Store 与不可达的接线错误）。
    Store(DistillError),
}

/// 一次 lesson 运行的读数。`cards` 是真插入的 lesson 卡数（重放为 0），
/// `rejected`/`uncertain` 是审核两条非 pass 判决的草稿数（三者之和 = `drafts`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LessonOutcome {
    pub turns: usize,
    pub drafts: usize,
    pub cards: usize,
    pub rejected: usize,
    pub uncertain: usize,
    pub duplicate: usize,
    pub state: PassState,
}

impl LessonOutcome {
    fn empty(turns: usize) -> Self {
        LessonOutcome {
            turns,
            drafts: 0,
            cards: 0,
            rejected: 0,
            uncertain: 0,
            duplicate: 0,
            state: PassState::Empty,
        }
    }
}

/// 选一批 lesson 候选的最小形状（只读）：`(session_key, device_id, error_code)` 分组，
/// 组内轮集合按载荷里的出现顺序去重。结构直接内联在函数里——它只活在这一处。
/// 从 `memory.db.review_item(state='rejected')` 选一批 lesson 候选。
///
/// 分组键是 `(session_key, device_id, error_code)`——"同一会话 / 同一错误族"就是设计
/// §波次三的"同会话同 slug 二次失败"那一格。组内轮集合按载荷里的出现顺序去重，
/// 跨条目稳定（`review_item.state='rejected'` 的行不会改写）。
///
/// 幂等（波三修 1，两处各管一段）：
/// - **终态行**：组内已经在 `extract_pass_state(pass_id='lesson')` 落过终态的轮先剔掉
///   （[`pass_state::settled_turns`]）——空产出/契约拒收/审核完成之后，这些轮不再进任何
///   一批，同组第二次选批回到 `None`（零模型调用）。剩下还有新轮时照常开新批：后到的
///   rejected 行不会被旧终态钉死。
/// - **队列唯一键**：剩余批的 `batch_key` 若已有一条 `review_item(pass_id='lesson')`
///   （任何状态），这一批跳过——那是"已入队、还没到终态"的一段（待审 / 已批准待补写），
///   状态机没有它的结论可读。
///
/// 旧库/旧行兼容：没有 lesson 终态行的库照常可选（过滤是空集）；有旧 lesson item 的组
/// 仍被队列键挡住，不会因为升级重烧。
/// 无候选 ⇒ `Ok(None)`（空批合法）。
///
/// 选批**不写库**：与 `select_event_batch` 不同，这里没有"确保某种锚"的副作用，
/// dry-run 与真实选批看到的是同一批。
/// 选批时的分组元组：`((session_key, device_id, error_code), 去重后的轮, 源 item 数)`。
type LessonGroup = ((String, String, String), Vec<String>, usize);

pub fn select_lesson_batch(
    t: &TranscriptHandle,
    l: &LedgerHandle,
    m: &MemoryHandle,
    limit: usize,
    _now_ms: i64,
) -> Result<Option<LessonPlan>, DistillError> {
    let limit = if limit == 0 { DEFAULT_LIMIT } else { limit.min(LESSON_MAX_TURNS) };
    let mut groups: Vec<LessonGroup> = Vec::new();
    let mut index: HashMap<(String, String, String), usize> = HashMap::new();
    {
        let mut st = m.conn().prepare(
            "SELECT session_key, device_id, pass_id,
                    COALESCE(NULLIF(program_code,''), ?1) AS code,
                    turn_uids_json
               FROM review_item
              WHERE state='rejected' AND session_key<>'' AND device_id<>''
              ORDER BY created_at_ms, id",
        )?;
        let rows = st.query_map(params![REVIEW_REJECT_CODE], |r: &Row| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        for row in rows {
            let (session_key, device_id, pass_id, code, turns_json) = row?;
            // 未知 pass（含 report/reuse）不是抽取失误，跳过而不是猜一个族。
            if !LESSON_SOURCE_PASSES.contains(&pass_id.as_str()) {
                continue;
            }
            let turns: Vec<String> = match serde_json::from_str(&turns_json) {
                Ok(turns) => turns,
                // 载荷读不出是数据不一致；这一条不选（不是整批失败——其它组还能跑）。
                Err(_) => continue,
            };
            let key = (session_key, device_id, code);
            match index.get(&key).copied() {
                Some(at) => {
                    groups[at].1.extend(turns);
                    groups[at].2 += 1;
                }
                None => {
                    index.insert(key.clone(), groups.len());
                    groups.push((key, turns, 1));
                }
            }
        }
    }
    // 已入队的 lesson 批（任何状态）：`batch_key` 是队列一侧的唯一键。它只负责
    // "入队了但还没有终态"的那一段（`pending_review`/`approved`）；终态由 pass_state 行回答
    // （见循环里的 `settled_turns` 过滤）。两处各管一段，见函数文档的"幂等"两条。
    let queued: HashSet<String> = {
        let mut st = m.conn().prepare(
            "SELECT batch_key FROM review_item
              WHERE pass_id=?1 AND batch_key IS NOT NULL",
        )?;
        let rows = st.query_map(params![PASS_LESSON], |r| r.get::<_, String>(0))?;
        let mut set = HashSet::new();
        for row in rows {
            set.insert(row?);
        }
        set
    };

    for ((session_key, device_id, error_code), seen_turns, source_items) in groups {
        // 轮集合去重保序（批内位置即契约：prompt 的 `t1..tN` 与 payload 的轮清单同源）。
        let wanted: Vec<String> = {
            let mut seen: HashSet<String> = HashSet::new();
            let mut out = Vec::new();
            for uid in seen_turns {
                if seen.insert(uid.clone()) {
                    out.push(uid);
                }
            }
            out
        };
        // 终态闸：已经落过 lesson 终态的轮不再进批（空产出/契约拒收/审核完成之后，
        // 同一组第二次来到这里就该是空集 ⇒ `Ok(None)`，一次模型都不调）。
        // 后到的 rejected 行带来新轮 ⇒ 只剩新轮进批 ⇒ 新批的 `batch_key` 也不同。
        let settled = pass_state::settled_turns(l, PASS_LESSON, &wanted)?;
        let wanted: Vec<String> =
            wanted.into_iter().filter(|uid| !settled.contains(uid)).collect();
        if wanted.is_empty() {
            continue;
        }
        // 转录回查：只取目标的角色/正文/时刻，顺序与 event 的选批同一条（created_at, uid）。
        let mut rows: Vec<crate::batch::TurnRow> = Vec::new();
        let mut system_turns_skipped = 0usize;
        {
            let mut st = t.conn().prepare(
                "SELECT turn_uid, role, body, created_at_ms, ingested_at_ms
                   FROM turns
                  WHERE session_key=?1 AND device_id=?2
                  ORDER BY created_at_ms, turn_uid",
            )?;
            let got = st.query_map(params![session_key, device_id], |r: &Row| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })?;
            for row in got {
                let (turn_uid, role, body, created_at_ms, ingested_at_ms) = row?;
                if !wanted.iter().any(|uid| uid == &turn_uid) {
                    continue;
                }
                if role == "system" {
                    // 工具输出不是一次 utterance：不选它（与 event 的批边界同一条）。
                    system_turns_skipped += 1;
                    continue;
                }
                rows.push(crate::batch::TurnRow {
                    turn_uid,
                    role,
                    body,
                    created_at_ms,
                    ingested_at_ms,
                    speaker: None,
                    speaker_anchor_id: None,
                });
                if rows.len() == limit {
                    break;
                }
            }
        }
        if rows.is_empty() {
            continue;
        }
        let turn_refs: Vec<&str> = rows.iter().map(|row| row.turn_uid.as_str()).collect();
        let batch_key =
            rc_memory::review::batch_key(PASS_LESSON, "", &session_key, &device_id, &turn_refs);
        if queued.contains(&batch_key) {
            continue;
        }
        // 归属：与 event 的会话读面逐列相同（旧会话 = 最保守的默认）。
        let session = t
            .conn()
            .query_row(
                "SELECT source_agent, project_hint,
                        COALESCE(owner_agent_id, source_agent) AS owner_agent,
                        COALESCE(visibility, 'private_agent') AS visibility,
                        COALESCE(retention, 'default') AS retention,
                        COALESCE(scope_kind, 'project') AS scope_kind,
                        COALESCE(scope_key, project_hint, session_key) AS scope_key
                   FROM sessions
                  WHERE session_key=?1 AND device_id=?2",
                params![session_key, device_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                    ))
                },
            )
            .ok();
        let Some((
            source_agent,
            project_hint,
            owner_agent,
            visibility,
            retention,
            scope_kind,
            scope_key,
        )) = session
        else {
            // 会话行没了（转录被清理）：这一组不可归属，跳过而不是造一条私有行。
            continue;
        };
        let principal = rc_ledger::transcript::resolve_principal(t, &session_key, &device_id)?;
        let owner = OwnerCtx {
            owner_principal_id: principal.as_ref().map(|p| p.principal_id.clone()),
            owner_agent_id: owner_agent,
            visibility: owner::parse_visibility(&visibility),
            retention: owner::normalize_retention(&retention),
        };
        let scope = Scope { kind: owner::normalize_scope_kind(&scope_kind), key: scope_key };
        return Ok(Some(LessonPlan {
            batch: Batch {
                session_key,
                device_id: device_id.clone(),
                source_agent: source_agent.clone(),
                project_hint,
                rows,
            },
            error_code,
            ctx: BatchCtx { source_agent, device_id },
            owner,
            scope,
            source_items,
            system_turns_skipped,
        }));
    }
    Ok(None)
}

/// 把这一批的终态写到 `extract_pass_state` 的**每一轮**上（`pass_id='lesson'`、`family=''`）。
///
/// 为什么按轮写：那张表的主键是 `(turn_uid, pass_id, family)`，没有"批"这一维；批的身份由
/// `review_item.batch_key` 承担。选批问的是"这一轮的 lesson 有没有结论"
/// （[`pass_state::settled_turns`]），所以终局必须落到轮上。`plan.batch.rows` 就是这一批
/// 真正进过 prompt 的轮（选批已把带终态的轮剔掉），不会误盖别的轮。
fn mark_lesson_terminal(
    l: &LedgerHandle,
    plan: &LessonPlan,
    state: PassState,
    reason: Option<&str>,
    now_ms: i64,
) -> Result<(), StoreError> {
    for row in &plan.batch.rows {
        pass_state::mark_terminal(
            l,
            pass_state::PassKey { turn_uid: &row.turn_uid, pass_id: PASS_LESSON, family: "" },
            state,
            reason,
            now_ms,
        )?;
    }
    Ok(())
}

/// 跑一批 lesson：抽取一次 → 入队 → 独立审核（`review::run` 的 `Pass::Lesson`）→ 落卡。
///
/// 每个终局都把这一批的每一轮写成 `extract_pass_state(pass_id='lesson')` 的终态行
/// （[`mark_lesson_terminal`]）——那是选批的幂等判据（见 [`select_lesson_batch`]）：
/// - 空产出（`lessons=[]`）⇒ `empty`，`cards=0`；
/// - 抽取契约拒收 ⇒ 模型原文的限长副本落 `extract_pass_artifact(stage=extract)`，
///   这一批落 `rejected(<slug>)`，返回 `Err(Contract)`；
/// - 审核 reject/uncertain ⇒ 草稿**直接丢**（EDV default-reject），`cards=0`，
///   这一批落 `rejected(review_reject)` / `quarantined(review_uncertain)`；
/// - 运行成功 ⇒ `done`/`rejected`/`quarantined` 与 `LessonOutcome.state` 同一格。
///
/// 不落终态的三支（"还没跑成"或"还没到结论"，下一次选批仍该看它）：
/// - 抽取 LLM 故障 ⇒ `Err(Llm)`，一行不写；队列键那时也还没落，下一夜会重选；
/// - 审核侧 LLM 故障 ⇒ `Err(Llm)`（`RunError::Llm`），item 停在 `pending_review`；队列键那时
///   已落（`enqueue` 在审核之前），靠 `batch_key` 挡重选，等 `hub review run` 补审；
/// - 入队后停在 `pending_review`（`--no-review` 或引擎没走完）⇒ 不写终态，
///   `batch_key` 挡住重选，等 `hub review run` 补审。
///
/// `mode=EnqueueOnly`（`--no-review`）：入队后不再问审核模型、不写卡，返回 `Pending`。
pub fn run_lesson(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    l: &LedgerHandle,
    plan: &LessonPlan,
    backend: &dyn LlmBackend,
    now_ms: i64,
    mode: ReviewMode,
) -> Result<LessonOutcome, LessonRunError> {
    if plan.batch.rows.is_empty() {
        return Ok(LessonOutcome::empty(0));
    }
    let system = lesson_system();
    let user = lesson_user(&plan.batch, &plan.error_code);
    let fingerprint = water::extraction_fingerprint(
        LESSON_PIPELINE,
        backend.slug().unwrap_or("unknown"),
        &system,
        &user,
    );
    let raw = backend
        .complete(&Prompt { system: system.clone(), user: user.clone(), derive: false })
        .map_err(LessonRunError::Llm)?;
    let drafts = match parse_lesson_output(&raw, &plan.batch, &plan.error_code) {
        Ok(drafts) => drafts,
        Err(e) => {
            // 诊断补件：写失败用 `let _ =`，不许盖住真正的解析错误。
            let first_uid = plan.batch.rows[0].turn_uid.clone();
            let run_id = format!("lesson-{now_ms}");
            let _ = pass_state::record_artifact(
                l, PASS_LESSON, "", "extract", &run_id, &first_uid, &raw, now_ms);
            // 契约拒收是这一批的**结论**：落 `rejected(<slug>)`，下一夜不再重选同一组
            // （波三修 1；此前这里一行不写，同一组每夜重烧一次）。
            mark_lesson_terminal(
                l, plan, PassState::Rejected, Some(e.slug()), now_ms,
            )
            .map_err(|err| LessonRunError::Store(DistillError::Store(err)))?;
            return Err(LessonRunError::Contract(e));
        }
    };
    if drafts.is_empty() {
        // 合法的空产出：这一段没有可复用的规则，零卡、不进审核队列。
        // 但它是一次跑过的批：落 `empty` 终态，同组第二次选批直接空集。
        mark_lesson_terminal(l, plan, PassState::Empty, None, now_ms)
            .map_err(|err| LessonRunError::Store(DistillError::Store(err)))?;
        return Ok(LessonOutcome::empty(plan.batch.rows.len()));
    }
    let items: Vec<serde_json::Value> = drafts.iter().map(payload_item).collect();
    let payload = crate::review::pass_payload(
        PASS_LESSON,
        "",
        &fingerprint,
        &plan.batch,
        &plan.ctx,
        &plan.owner,
        &plan.scope,
        serde_json::Value::Array(items),
        None,
    );
    let owner_json = crate::review::owner_json(&plan.owner, &plan.scope);
    let queued_item = crate::review::new_item(PASS_LESSON, "", &plan.batch, owner_json, &payload);
    let queued = enqueue(m, &queued_item, now_ms)
        .map_err(|e| LessonRunError::Store(DistillError::Memory(e)))?;
    let item = get(m, queued.id)
        .map_err(|e| LessonRunError::Store(DistillError::Memory(e)))?
        .ok_or_else(|| {
            LessonRunError::Store(DistillError::Memory(rc_memory::MemError::Sql(
                "review: enqueued item vanished".to_string(),
            )))
        })?;
    let mut outcome = LessonOutcome {
        turns: plan.batch.rows.len(),
        drafts: drafts.len(),
        cards: 0,
        rejected: 0,
        uncertain: 0,
        duplicate: 0,
        state: PassState::Pending,
    };
    match queued.state {
        ReviewState::PendingReview => {
            if mode == ReviewMode::EnqueueOnly {
                // 只入队：有草稿的那一份在 `review_item` 里等 `hub review run`。
                return Ok(outcome);
            }
        }
        // 已判过的 item 走引擎的判决重建（approved 重试 / applied 只读重放）。
        ReviewState::Approved | ReviewState::Applied => {}
        ReviewState::Rejected => {
            outcome.state = PassState::Rejected;
            outcome.rejected = drafts.len();
            // 队列里已是终态结论（上一轮已判过）：终态行照写，选批据此封口。
            mark_lesson_terminal(
                l, plan, PassState::Rejected, Some(REVIEW_REJECT_CODE), now_ms,
            )
            .map_err(|err| LessonRunError::Store(DistillError::Store(err)))?;
            return Ok(outcome);
        }
        ReviewState::Quarantined => {
            outcome.state = PassState::Quarantined;
            outcome.uncertain = drafts.len();
            mark_lesson_terminal(
                l,
                plan,
                PassState::Quarantined,
                Some(crate::review::REVIEW_UNCERTAIN),
                now_ms,
            )
            .map_err(|err| LessonRunError::Store(DistillError::Store(err)))?;
            return Ok(outcome);
        }
        ReviewState::Stale => {
            outcome.state = PassState::Rejected;
            outcome.rejected = drafts.len();
            mark_lesson_terminal(
                l, plan, PassState::Rejected, Some(REVIEW_REJECT_CODE), now_ms,
            )
            .map_err(|err| LessonRunError::Store(DistillError::Store(err)))?;
            return Ok(outcome);
        }
    }
    let run = match run_item(m, t, l, item.meta.id, backend, now_ms) {
        Ok(run) => run,
        Err(RunError::Llm(failure)) => return Err(LessonRunError::Llm(failure)),
        // 审核侧的契约拒收：引擎已把 item 记为 uncertain 并落 artifact。抽取不必再来一遍，
        // 所以这一批也落隔离终态（reason 与 hub 审核档同一个串）。
        Err(RunError::ReviewLesson(e)) => {
            mark_lesson_terminal(
                l,
                plan,
                PassState::Quarantined,
                Some(&e.review_reason()),
                now_ms,
            )
            .map_err(|err| LessonRunError::Store(DistillError::Store(err)))?;
            return Err(LessonRunError::Contract(e));
        }
        Err(RunError::Review(other)) => return Err(LessonRunError::Store(other)),
        Err(RunError::Store(e)) => return Err(LessonRunError::Store(e)),
        // 队列竞态：另一写者已推进状态，本进程没做成 ⇒ 按 pending 交回（不是失败）。
        Err(RunError::Conflict) => {
            outcome.state = PassState::Pending;
            return Ok(outcome);
        }
        Err(RunError::Unsupported) | Err(RunError::MissingItem) | Err(RunError::ReviewNaming(_))
        | Err(RunError::ReviewRows(_)) => {
            return Err(LessonRunError::Store(DistillError::Memory(
                rc_memory::MemError::Sql("review run: unexpected state".to_string()),
            )))
        }
    };
    // S4 同一条：内联审核与 `hub review run` 是同一支 apply，卡也必须在这里提升。
    // 失败不改变这一批的结果（卡停在 `unreviewed` 仍有 audit 兜底）。
    let _ = promote_applied_cards(m, &run, now_ms);
    for (_, verdict) in run.verdicts.as_deref().unwrap_or(&[]) {
        match verdict {
            rc_memory::ReviewVerdict::Pass => {}
            rc_memory::ReviewVerdict::Reject => outcome.rejected += 1,
            rc_memory::ReviewVerdict::Uncertain => outcome.uncertain += 1,
        }
    }
    outcome.cards = run.cards;
    outcome.duplicate = run.duplicate;
    outcome.state = match run.item_state {
        ReviewState::Applied => PassState::Done,
        ReviewState::Rejected => PassState::Rejected,
        ReviewState::Quarantined => PassState::Quarantined,
        ReviewState::Stale => PassState::Rejected,
        ReviewState::PendingReview | ReviewState::Approved => PassState::Pending,
    };
    // 只有终态才落行：`pending`（没走完/等审核）不是结论，选批的队列键那一侧会挡住重选。
    let reason = match outcome.state {
        PassState::Rejected => Some(REVIEW_REJECT_CODE),
        PassState::Quarantined => Some(crate::review::REVIEW_UNCERTAIN),
        _ => None,
    };
    if matches!(
        outcome.state,
        PassState::Done | PassState::Rejected | PassState::Quarantined
    ) {
        mark_lesson_terminal(l, plan, outcome.state, reason, now_ms)
            .map_err(|err| LessonRunError::Store(DistillError::Store(err)))?;
    }
    Ok(outcome)
}

/// 载荷里的一条 lesson：四键 + 两条**程序**加的格（`error_code`/`turn_uid`）。
///
/// 模型写不进这两格：它们由 [`parse_lesson_output`] 从批上下文填（`error_code`）与
/// 代表轮填（`turn_uid`），重放按同一套重建（`review::replay::lesson_drafts`）。
fn payload_item(draft: &LessonDraft) -> serde_json::Value {
    json!({
        "title": draft.title,
        "description": draft.description,
        "content": draft.content,
        "error_kind": draft.error_kind,
        "error_code": draft.error_code,
        "turn_uid": draft.turn_uid,
    })
}
