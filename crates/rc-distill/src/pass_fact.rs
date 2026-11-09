//! S2b-2b：fact pass 的库面（设计 `2026-10-26-s2-prompt-v0` §2.2、§3、§4、§7）。
//!
//! 一条真实 pass 只做四件事：从转录里**选**一批还没对这一族表态的轮 → 抽取调用 →
//! 把"窄审 + 判定 + 写库"整段交给 [`crate::review::run::run_item`] → 逐轮/整批落
//! `extract_pass_state`。本片不做 event/time、不写 `extraction_issue`（S3 的审核队列统一做）、
//! 不碰旧 `extract_state`。
//!
//! 六条纪律：
//! 1. **选批只认 `extract_pass_state` 的 `(turn, 'fact', family)`**：`family` 是这一族的三值
//!    （`decision|promise|plan`），三族各自表态、互不越过；其余 pass 的键（含 naming 的空串
//!    family）在这里一次都不读。可重选判据与 naming 同向：无行 / 到期 pending / stale；
//!    P0-2 起终态行另看 `prompt_version`（非空且与当前 system prompt 版本不同 ⇒ 可重选）。
//! 2. **审核是第二次独立调用**：抽取的 parser 过了才开口；审核用自己的 system/user 与
//!    canary（`pass_prompt::fact_review_*`），只读「行 + 引文 + 目标轮」。审核失败/不确定
//!    ⇒ 该行不写；LLM 不可用 ⇒ 每轮 `pending`，**不放行**。没有"自动 pass"分支。
//! 3. **状态先写 running 再调模型**：抽取 LLM 故障 ⇒ 每轮 `pending`（60s 退避）；抽取契约
//!    拒收 ⇒ 每轮 `rejected(<slug>)`；审核契约拒收 ⇒ 每轮 `quarantined(review_<slug>)`；
//!    写库失败 ⇒ 有草稿的轮 `pending(review_pending)`（H1b：不写 `rejected(store)`，便于补审）。
//!    所有失败都返回 `Err`，不写半批——**除单元级（Timeout/SchemaMismatch）那一支**：
//!    W19c2 起试满上限 ⇒ `Ok(quarantined)`（`retry_exhausted:true`），未满 ⇒ `Ok(pending)`。
//!    `<slug>` 是 [`FactPassError::slug`] 的封闭词表（S2e-1：`json`/`quote_not_verbatim`/
//!    `uncovered`/… ），`review_<slug>` 由 [`FactPassError::review_reason`] 现拼；两者都只落库。
//! 4. **每轮终态由该轮的判决决定**：有任一 `uncertain` ⇒ `quarantined(review_uncertain)`；
//!    该轮没有草稿（被 `empty_turns` 覆盖）⇒ `empty`；有 `pass` 行 ⇒ `done`；
//!    只剩被去重的 `pass` 行 ⇒ `done`（已处理，重复不是失败）；只剩 `reject` ⇒
//!    `rejected(review_reject)`。
//! 5. **零草稿不调审核**：审核的问题是"这一行的引文支持不支持"；没有行就没有问题
//!    （设计 §4："一次只问一个 pass 的一种问题，每批 ≤10 行"）。这不是"自动 pass"：
//!    这一批没有任何行进入写库层。
//! 6. **模型面写不出的东西由程序补**：`scope/owner/source_agent/device_id/source_ms`
//!    只从 `FactPlan` 与批内轮时刻来；`evidence` 的 role 从 `turns.role` 读，不读模型。
//!
//! S2e-8b：抽取改用**宽松** parser（[`parse_fact_output_lenient`]）。整批仍拒的只有信封
//! （形状/版本/pass/family/`empty_turns` 的坏引用），单条草稿坏在哪一格只丢这一条：
//! 审核只喂保留的草稿（`f1..fN` 按保留顺序重排），读数多 `dropped`/`uncovered` 两格。
//! 真实冒烟的形态是 3 条草稿里 1 条 quote 挪了一个反引号——那一条进 `dropped`，
//! 另外两条照常审核/写库。代价是**丢弃不再按轮归因**：`LenientFacts::dropped` 只带错误变体
//! （见 `pass_fact_contract::LenientFacts`），所以每轮状态写处置词 `dropped` 而不是坏在哪一格。
//!
//! 状态取法的 S2e-8b 补充（按这条顺序判，先命中先落）：
//! `quarantined(review_uncertain)` → `done`（pass 或全被去重）→ `rejected(review_reject)`
//! → `rejected(dropped)` / `rejected(uncovered)` → `empty`（被 `empty_turns` 覆盖）。
//! 后两格的分界见 `uncovered` 与 `dropped` 的注释：契约侧的 `covered` 只由**保留**的草稿
//! 与 `empty_turns` 组成，所以"草稿被丢"的轮也会出现在 `uncovered` 里——本批真有草稿被丢时
//! 一律写 `dropped`（模型开过口），一条都没丢时才写 `uncovered`（模型真漏了）。
//!
//! 整批 `state` 的取法（ORDER 步骤 9 的第一条与步骤 8 的"空覆盖轮 ⇒ empty"在混合批下互斥，
//! 这里按语义取）：有 `quarantined` ⇒ `quarantined`；全轮 `empty` ⇒ `empty`；任一轮 `done`
//! ⇒ `done`（混合 done/empty 是正常成功批）；否则 `rejected`。与"全轮 done ⇒ done"不矛盾
//! ——那是本条的一个子集，而字面版本会把"一轮有行、其余轮空覆盖"的正常批读成 rejected。

use crate::batch::{count_system_turns_in_span, load_extract_ctx, Batch, ExtractCtx, TurnRow};
use crate::pass_fact_contract::{parse_fact_compact, parse_fact_output_lenient};
use crate::pass_event::append_principal_anchor;
use crate::pass_prompt::{
    fact_system_selected, fact_user, is_compaction_body, is_runtime_notice, FactCandidate, FactFamily,
    COMPACT_SUMMARY_REASON, MARK_SELF, NOTICE_REASON, PASS_FACT,
};
use crate::review::{self, ReviewMode};
use crate::review::run::{promote_applied_cards, run_item, RunError};
use crate::DistillError;

/// §10.47：`RECOLLECT_FACT_PROMPT=compact` 时走弱模型档（紧凑 system + keep 清单）。
fn compact_prompt() -> bool {
    std::env::var("RECOLLECT_FACT_PROMPT").as_deref() == Ok("compact")
}
use rc_ledger::pass_state::{self, PassKey, PassState};
use rc_ledger::water;
use rc_llm::{LlmBackend, Prompt};
use rc_memory::ensure_agent_anchor;
use rc_memory::owner::{self, OwnerCtx};
use rc_memory::review::{
    claim, enqueue, get, release, ReviewState, ReviewVerdict as QueueVerdict,
};
use rc_memory::{BatchCtx, Scope};
use rc_store::{LedgerHandle, MemoryHandle, StoreError, TranscriptHandle};
use rusqlite::types::ValueRef;
use rusqlite::{params, Row};
use std::collections::{BTreeSet, HashMap, HashSet};

/// 指纹里的 pipeline 前缀。真正写进状态的是 `<前缀>/<family slug>`：三族是三个不同的作业。
pub const FACT_PIPELINE_PREFIX: &str = "pass-fact-v0.1";

/// 调用方给 0 时的默认窗宽（应用层 `--batch-turns` 的缺省在 hub；这一格只兜零值）。
pub const DEFAULT_LIMIT: usize = 20;

/// LLM 故障的退避（设计 §3 的重试/退避：60s 起）。
pub const RETRY_BACKOFF_MS: i64 = 60_000;

// 候选锚上限（`CANDIDATE_LIMIT`）的家已搬到 `crate::candidates`：与 event 共用一份 SQL
// 与可见性过滤，`a1..aN` 的位置契约不变。

/// 失败理由。解析失败不再落一个笼统的 `schema`：抽取侧写 [`FactPassError::slug`] 的精确词，
/// 审核侧写 [`FactPassError::review_reason`]（`review_` + slug）。`review_uncertain`/
/// `review_reject` 是审核**判决**的两格（判决不是解析失败），`store` 是写库侧（与 naming 的
/// `STORE_FAILURE_REASON` 同词）。
const REVIEW_UNCERTAIN_REASON: &str = "review_uncertain";
const REVIEW_REJECT_REASON: &str = "review_reject";
const STORE_FAILURE_REASON: &str = "store";
/// 单条草稿被宽松 parser 丢掉后该轮的终态理由（S2e-8b）。它不是抽取的 slug：契约侧的
/// `dropped` 只带错误变体、不带轮次，所以这一列写的是**处置**（这一轮的草稿被丢），
/// 而不是坏在哪一格（那仍在 `FactPassError` 的变体里，只给测试看）。
const DROPPED_REASON: &str = "dropped";
/// 模型漏掉的轮（既没有合法草稿覆盖、也没被 `empty_turns` 宣告）。契约侧给的是批内短 id，
/// 写状态前必须映射回真实 `turn_uid`（见 [`uncovered_uids`]）。
const UNCOVERED_REASON: &str = "uncovered";

/// 一批 fact 的计划：批次、P 独占列、归属、行 scope、候选锚、这一族，以及目标轮的
/// ±1 邻轮（C6b）。
/// 全部来自程序（转录行 + `sessions` 的归属列与 `source_agent` + 记忆库的活锚），
/// 模型一个字段都递不进来。候选末尾两格是程序补的：会话自锚（S2e-7）与主人锚（I2），
/// 见 [`append_agent_anchor`] / [`append_principal_anchor`]。
#[derive(Debug, Clone)]
pub struct FactPlan {
    pub batch: Batch,
    /// 目标轮的邻轮：只进 prompt 的 CTX 段，**不是**批、不是证据、不进 `pass_state`。
    /// 邻轮读取不按 role 过滤，所以紧邻目标轮的工具输出（`role='system'`）也在这一段里
    /// ——转换器配套：它们可看、不可引。
    pub ctx_turns: ExtractCtx,
    /// 批区间内被跳过的 `role='system'` 轮数（转换器配套）：候选 SQL 已把它们挡在批外，
    /// 这一格是"被挡掉几条"的可见读数（读数的 `system_turns_skipped`）。
    pub system_turns_skipped: usize,
    pub ctx: BatchCtx,
    pub owner: OwnerCtx,
    pub scope: Scope,
    pub candidates: Vec<FactCandidate>,
    pub family: FactFamily,
}

/// 一批的读数。`drafts` 是**保留**的合法草稿数（不是模型原始条数：`drafts + dropped` 才是），
/// `dropped` 是宽松 parser 丢掉的原条数，`uncovered` 是既没有合法草稿覆盖、也没被
/// `empty_turns` 宣告的目标轮数（S2e-8b）。`written` 是审核 `pass` 且未被去重的
/// 行数（交给写库层），`duplicate` 是审核 `pass` 但**同 `(证据轮, value_text)`（有 value）
/// 或同 `(证据轮, 主语, 类别, 谓词, 宾语身份)`（空 value，C5-fix）已存在**的草稿数
/// （S2e-6 跨 family + C5 空值 + C5-fix 宾语身份），`rejected`/`uncertain` 是另两条判决的草稿数
/// （四者之和 = `drafts`）。`facts`/`cards`/`mentions` 是**真插入**的行数（重放为 0）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FactOutcome {
    pub turns: usize,
    pub drafts: usize,
    /// 宽松 parser 丢掉的草稿数（S2e-8b）。`drafts + dropped = 模型原始草稿数`。
    pub dropped: usize,
    /// 覆盖缺口的轮数（短 id 已映射回真实 `turn_uid`，S2e-8b）。
    pub uncovered: usize,
    pub written: usize,
    /// 跨 family 去重跳过的草稿数（S2e-6）。重复不写行，但必须计数可见。
    pub duplicate: usize,
    pub rejected: usize,
    pub uncertain: usize,
    pub facts: usize,
    pub cards: usize,
    pub mentions: usize,
    /// 批区间内被跳过的 `role='system'` 轮数（转换器配套；选批层面的读数，与这批跑成
    /// 什么无关）。additive：既有格一个不改，也不进 `schema_version`。
    pub system_turns_skipped: usize,
    pub state: PassState,
    /// 这一批是不是"单元级失败试满上限、整批落成隔离"（W19c2）。**只在那一支为真**。
    ///
    /// 它与 `state` 不是一回事：`Quarantined` 另有来源（审核不确定、迟到的同维度异值）——
    /// 那些是**质量**隔离，不是这一格卡死。日路径的连续隔离墙只认这一格。
    pub retry_exhausted: bool,
}

/// 选批读到的一格表态：`state`、退避到期时刻、理由、产下这一行的 prompt 版本（P0-2）。
type StatedRow = (PassState, Option<i64>, Option<String>, Option<String>);

/// 这一族今天该不该再跑。与 `pass_naming::reselectable` 同向，只是键多一格 `family`。
///
/// H5a：`pending(review_pending)` **不可重选**——那一轮的产出已经在 `review_item` 里等
/// `review run`，再选一次只会重抽一遍、白烧两次调用。其余 `pending`（含 NULL reason 的
/// 退避行）到期后照旧可选；`stale` 与"没有行"照旧可选。
///
/// P0-2：终态行另看 `prompt_version`——非空且与当前 system prompt 的版本不同 ⇒ 可重选
/// （判据住 `pass_state::prompt_version_reselectable`）。**NULL 不重选**：旧库升级后不自动
/// 全量重跑，要重跑旧轮的人显式清掉那一行。
fn reselectable(
    rec: Option<&StatedRow>,
    now_ms: i64,
    current_version: &str,
) -> bool {
    match rec {
        None => true,
        Some((PassState::Stale, _, _, _)) => true,
        Some((PassState::Pending, _, Some(reason), _))
            if reason == pass_state::REVIEW_PENDING_REASON =>
        {
            false
        }
        Some((PassState::Pending, due, _, _)) => due.is_none_or(|d| d <= now_ms),
        // running 不重入；终态看 prompt 版本（同版本不跑，换了判据才回头）。
        Some((state, _, _, version)) => pass_state::prompt_version_reselectable(
            *state,
            version.as_deref(),
            current_version,
        ),
    }
}

/// `select_fact_batch` 读到的会话行（与 naming/event 同一组九列，元组位置一错就是静默串值）。
///
/// 身份层 I1 起这里**没有** `owner_principal_id`：主人身份每会话走
/// `rc_ledger::transcript::resolve_principal`（claim 表最新一条，缺省回落 `sessions` 那一列）。
struct SessionRow {
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

/// 选一批该跑这一族 fact 的轮次。可重选判据（设计 §3）：
/// - `extract_pass_state` 没有 `(turn, 'fact', family)` 行 ⇒ 可；
/// - `state='pending'` 且 reason 是 `review_pending`（H5a）⇒ **不可**（产出已在待审区）；
/// - `state='pending'` 且 `next_retry_at_ms` 为空或已到期（`<= now_ms`）⇒ 可；
/// - `state='stale'` ⇒ 可（指纹作废，与时间无关）；
/// - 其余（`running`/`done`/`empty`/`rejected`/`quarantined`，以及未到期的 `pending`）⇒ 不可；
///   其中四个终态（`done`/`empty`/`rejected`/`quarantined`）另看 `prompt_version`：非空且与
///   当前 system prompt 版本不同 ⇒ **可**（P0-2：改判据后旧轮自动重跑）；NULL ⇒ 不可。
///
/// 会话按最新到达钟 `MAX(turns.ingested_at_ms)` 倒序、同刻按 `(session_key, device_id)` 排序，
/// 会话内按 `(created_at_ms, turn_uid)` 排序，取第一个有可选中轮次的会话的前 `limit` 轮
/// （0 按 [`DEFAULT_LIMIT`]）。没有可选的 ⇒ `Ok(None)`。
///
/// S2e-6：可选中但正文是压缩摘要块（[`is_compaction_body`]）的轮**当场**落
/// `rejected(compact_summary)` 并跳过——摘要只作上下文，不当证据，也不让同一条摘要在
/// 每次选批里被重复扫。已经落过状态行的轮不受影响（`reselectable` 先挡）。
///
/// S2e-7：**选批会写 memory.db**——给会话的 `source_agent` 确保一个 `kind='agent'` 的锚
/// （一行 `anchor` + 一行 `resource_owner`，都在同一个事务里），并把它作为候选的倒数第二格
/// `aN | agent | <source_agent>` 交给模型。I2 起同一层再补主人锚（`kind='person'`，没有身份
/// 就不建）："我…"这类 human 轮的主语从此有合法的一格。它和 `extract_pass_state` 的写入
/// 同一层：选批本来就会为压缩块落状态行。没有可选中轮次时一条都不写；两个锚都按
/// `(canonical, scope)` 幂等。
pub fn select_fact_batch(
    t: &TranscriptHandle,
    l: &LedgerHandle,
    m: &MemoryHandle,
    limit: usize,
    family: FactFamily,
    now_ms: i64,
) -> Result<Option<FactPlan>, DistillError> {
    let limit = if limit == 0 { DEFAULT_LIMIT } else { limit };
    // P0-2：当前 system prompt 的版本号——选批这一侧也要算，才能和状态行里的那一格比。
    // 只喂 system（这一族的 `fact_system_selected`），不喂批；批组成变了不算"换判据"。
    let current_version = pass_state::prompt_version(&fact_system_selected(family));
    // ① 这一族的表态（ledger.db）——**一次**读，不在会话循环里读。
    let stated: HashMap<String, StatedRow> = {
        let mut st = l.conn().prepare(
            "SELECT turn_uid, state, next_retry_at_ms, reason, prompt_version
             FROM extract_pass_state
             WHERE pass_id=?1 AND family=?2",
        )?;
        let got = st.query_map(params![PASS_FACT, family.slug()], |r| {
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
            // 坏 slug 与 `pass_state::get` 同一条纪律：读不出的状态不是"还没跑"，是数据不一致。
            let state = PassState::parse(&raw)
                .ok_or_else(|| StoreError::Sql(format!("pass_state: unreadable state {raw}")))?;
            map.insert(uid, (state, next_retry, reason, version));
        }
        map
    };

    // ② 会话清单（transcript.db）。归属列在 SQL 侧 COALESCE 默认（旧会话 = 最保守的默认）；
    //    主人身份每会话问一次 `resolve_principal`（claim 与回落规则只有那一处实现）。
    let mut sessions: Vec<SessionRow> = Vec::new();
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
            // 空串 project_hint 在契约侧是拒绝项，不能被读成 NULL。
            let hint: Option<String> = match r.get_ref(3)? {
                ValueRef::Null => None,
                ValueRef::Text(v) => Some(String::from_utf8_lossy(v).to_string()),
                _ => None,
            };
            Ok(SessionRow {
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
        let SessionRow {
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
        // 身份层 I1：主人身份取解析值（claim 最新一条；无章回落会话列；两者都无 ⇒ NULL）。
        // I2 起连同可读名一起留下：主人锚的 canonical 要用它（名字优先，回落到 id）。
        let principal = rc_ledger::transcript::resolve_principal(t, &session_key, &device_id)?;
        let owner_principal_id = principal.as_ref().map(|p| p.principal_id.clone());
        let principal_name = principal.and_then(|p| p.principal_name);
        // ③ 会话内按源会话顺序扫可选中的轮；凑够 limit 立刻 break（读取有界）。
        //    转换器配套：`role='system'`（工具调用/工具结果）**只作上下文，永不作 TARGET**，
        //    在 SQL 这一层挡掉；紧邻目标轮的那些仍进 CTX 段（见 `load_extract_ctx`）。
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
                if !reselectable(stated.get(&row.turn_uid), now_ms, &current_version) {
                    continue;
                }
                // 压缩摘要块只作上下文，不能当证据（S2e-6）：选批当场落
                // `rejected(compact_summary)`，于是它既不进 batch，也不会被下一次选批重复扫到。
                // 状态行已在场的轮走上面的 `reselectable`，这里只处理"还能选"的那些。
                if is_compaction_body(&row.body) {
                    pass_state::mark_terminal(
                        l,
                        key(&row.turn_uid, family),
                        PassState::Rejected,
                        Some(COMPACT_SUMMARY_REASON),
                        now_ms,
                    )?;
                    continue;
                }
                // §10.66 C：运行期通知不是 utterance（同 event 的同一处判据）。
                if is_runtime_notice(&row.body) {
                    pass_state::mark_terminal(
                        l,
                        key(&row.turn_uid, family),
                        PassState::Rejected,
                        Some(NOTICE_REASON),
                        now_ms,
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
        // C6b：目标轮的 ±1 邻轮。**选批之后**才取，且直接读 transcript（跨批边界），
        // 选批的 `reselectable`/压缩块隔离/连续性策略一个字都不改。
        let ctx_turns = load_extract_ctx(t, &session_key, &ctx.device_id, &rows)?;
        // 转换器配套：批区间内被跳过的 system 轮——读数 `system_turns_skipped`。
        let system_turns_skipped =
            count_system_turns_in_span(t, &session_key, &ctx.device_id, &rows)?;
        let owner = OwnerCtx {
            owner_principal_id,
            owner_agent_id: owner_agent,
            visibility: owner::parse_visibility(&visibility),
            retention: owner::normalize_retention(&retention),
        };
        let scope = Scope { kind: owner::normalize_scope_kind(&scope_kind), key: scope_key };
        // 候选顺序是硬契约：先按 active 列表排 `a1..aN`，再给会话自锚补一格（S2e-7），
        // 最后是主人锚（I2）。反过来的话自锚会比实体新（created_at=now），抢走 a1，
        // 模型引用的含义每批都在漂。候选池按会话的 owner/scope 过滤（`crate::candidates`）：
        // 别的 owner 的私有锚不进表。
        let mut candidates =
            crate::candidates::load_candidates(m, &owner.owner_agent_id, &scope)?;
        append_agent_anchor(m, &mut candidates, &ctx, &owner, &scope, now_ms)?;
        append_principal_anchor(
            m,
            &mut candidates,
            &owner,
            &scope,
            principal_name.as_deref(),
            now_ms,
        )?;
        // C3 证据内预筛：名字出现在目标轮正文的候选排前面（只重排、不删，见
        // `crate::candidates::order_by_evidence`）。放在两次 append 之后：自锚/主人锚
        // 也可能是证据里点名的那一方（"本 agent 决定…"），同样该被提到前面。
        crate::candidates::order_by_evidence(m, &mut candidates, &rows)?;
        return Ok(Some(FactPlan {
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
            family,
        }));
    }
    Ok(None)
}

/// 为会话的 `source_agent` 确保一个 agent 锚，并把它补成候选的**最后一格**（S2e-7）。
///
/// 锚是程序产物：id / scope / status / 时刻都由 `ensure_agent_anchor` 盖章，模型只引用它的
/// `a*`。这一格要解决的问题是冒烟发现 1：候选里没有会话自己时，"我/本 agent 决定/计划…"
/// 只能把主语落成"项目记忆/阶段台账"这类来源，审核要么放错、要么整批拒。
///
/// 判重只看 `anchor_id`（不是 canonical 文本）：命名 pass 已经建过同 canonical 的 agent 锚时
/// 它已在候选里，再 append 一条就是同一个锚占两个 `a*`，模型引用哪个都"对"，但候选表在说谎。
/// `reference` 是位置即契约，append 不改已有位置（自锚接在池后）。C3 的证据内预筛
/// （`crate::candidates::order_by_evidence`）随后可能把被点名的自锚提到前面，但那一步
/// 统一重编 `a1..aN`，与渲染/parser 仍同源。与 `pass_event` 的同名函数逐行相同——
/// 两个 pass 的候选表各是一份实现，改一处要同时改另一处。
///
/// C3：自锚带 [`MARK_SELF`]。判重命中时也要盖章——那条候选是上一批落进池里的同一个锚，
/// 不盖章就是"本 agent 在候选表里但没被标出来"，正是这一片要补的缺口。
fn append_agent_anchor(
    m: &MemoryHandle,
    candidates: &mut Vec<FactCandidate>,
    ctx: &BatchCtx,
    owner: &OwnerCtx,
    scope: &Scope,
    now_ms: i64,
) -> Result<(), DistillError> {
    let key = ensure_agent_anchor(m, ctx, owner, scope, now_ms)?;
    if let Some(existing) = candidates.iter_mut().find(|candidate| candidate.anchor_id == key.id) {
        existing.mark = Some(MARK_SELF);
        return Ok(());
    }
    candidates.push(FactCandidate {
        reference: format!("a{}", candidates.len() + 1),
        kind: "agent".to_string(),
        canonical: ctx.source_agent.clone(),
        anchor_id: key.id,
        mark: Some(MARK_SELF),
    });
    Ok(())
}

fn key<'a>(turn_uid: &'a str, family: FactFamily) -> PassKey<'a> {
    PassKey { turn_uid, pass_id: PASS_FACT, family: family.slug() }
}

/// 宽松 parser 的覆盖缺口是批内短 id（`t1..tN`，按序号有序、天然去重）：状态列要的是真实
/// `turn_uid`，写状态前必须映射回来（S2e-8b）。映射只有这一处实现——位置即契约，与 parser 的
/// `t*` 同一条纪律（`pass_fact_contract::FactRefs`）。认不出的引用不是"某一轮没覆盖"，
/// 跳过而不猜一轮（parser 只产 `t1..tN`，这一格不可达）。
fn uncovered_uids(short: &[String], batch: &Batch) -> HashSet<String> {
    let mut uids = HashSet::with_capacity(short.len());
    for reference in short {
        let index = reference
            .strip_prefix('t')
            .and_then(|digits| digits.parse::<usize>().ok())
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| batch.rows.get(index));
        if let Some(row) = index {
            uids.insert(row.turn_uid.clone());
        }
    }
    uids
}

/// §10.63：被丢草稿的 `(t-ref, slug)` → `turn_uid → slug`。同一轮多条坏草稿取第一条的码。
fn dropped_reasons(short: &[(String, &'static str)], batch: &Batch) -> HashMap<String, &'static str> {
    let mut out: HashMap<String, &'static str> = HashMap::with_capacity(short.len());
    for (reference, slug) in short {
        let index = reference
            .strip_prefix('t')
            .and_then(|digits| digits.parse::<usize>().ok())
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| batch.rows.get(index));
        if let Some(row) = index {
            out.entry(row.turn_uid.clone()).or_insert(*slug);
        }
    }
    out
}
fn mark_every_turn(
    l: &LedgerHandle,
    uids: &[String],
    family: FactFamily,
    state: PassState,
    reason: Option<&str>,
    now_ms: i64,
) -> Result<(), DistillError> {
    for uid in uids {
        pass_state::mark_terminal(l, key(uid, family), state, reason, now_ms)?;
    }
    Ok(())
}

/// I3：程序闸拒收的 draft 的**证据轮**落 `rejected(<码>)`（只写本批选中的轮）。
///
/// 这不是"整批 rejected"：同批通过的 draft 的轮照常走成功路径，批内没有草稿覆盖的轮
/// 仍是 `empty`。作用域按批收窄——证据轮理论上都在批内（契约保证），这里仍只动选中的轮，
/// 不替别的批写状态。
fn mark_program_rejected(
    l: &LedgerHandle,
    uids: &[String],
    family: FactFamily,
    turns: &[(String, &'static str)],
    now_ms: i64,
) -> Result<(), DistillError> {
    for (uid, code) in turns {
        if uids.iter().any(|batch_uid| batch_uid == uid) {
            pass_state::mark_terminal(l, key(uid, family), PassState::Rejected, Some(code), now_ms)?;
        }
    }
    Ok(())
}

/// 抽取完成、窄审还没由**本进程**执行（`EnqueueOnly` 或占位被他人持有）时的逐轮终态：
/// 有草稿的轮等审核（`pending(review_pending)`），没有草稿的轮沿用抽取终态
/// （`rejected(uncovered|dropped)` / `empty`）。两条早退路径共用同一份映射，读数才不会分叉。
#[allow(clippy::too_many_arguments)]
fn mark_awaiting_or_extraction_terminal(
    l: &LedgerHandle,
    uids: &[String],
    family: FactFamily,
    draft_turns: &HashSet<&str>,
    uncovered: &HashSet<String>,
    dropped: usize,
    dropped_reasons: &HashMap<String, &'static str>,
    now_ms: i64,
) -> Result<(), DistillError> {
    for uid in uids {
        if draft_turns.contains(uid.as_str()) {
            pass_state::mark_awaiting_review(l, key(uid, family), now_ms)?;
        } else if uncovered.contains(uid) {
            // §10.63 同形：写它自己坏在哪一格；映射不到（不该发生）才退回 `dropped`。
            let reason = if dropped > 0 {
                dropped_reasons.get(uid).copied().unwrap_or(DROPPED_REASON)
            } else {
                UNCOVERED_REASON
            };
            pass_state::mark_terminal(
                l,
                key(uid, family),
                PassState::Rejected,
                Some(reason),
                now_ms,
            )?;
        } else {
            pass_state::mark_terminal(l, key(uid, family), PassState::Empty, None, now_ms)?;
        }
    }
    Ok(())
}

/// 宾语身份：有锚用锚 id；没有锚用字面量（加前缀避免与 id 撞）；两者都没有 = 空串。
///
/// C5-fix：判"两条是不是同一件事"时，**宾语不同就是两条事**。event 与 fact 空值支共用
/// 这一格；批内键与库内查询也共用同一次调用（库侧取回候选行后在 Rust 里算键，不在 SQL
/// 另拼一份规则，见 [`already_written`] 与 [`crate::pass_event::already_written_event`]）。
/// 字面量先 `trim()`：`" "` 与 `""` 在"没有身份"这件事上是同一件事。
pub(crate) fn object_key(anchor_id: &str, object_text: &str) -> String {
    if !anchor_id.is_empty() {
        format!("id:{anchor_id}")
    } else if !object_text.trim().is_empty() {
        format!("text:{}", object_text.trim())
    } else {
        String::new()
    }
}

/// 两段 value 是不是同一件事：`trim` 后相等，或一方包含另一方（空串一律不算）。
///
/// 只用于**跨族**那一支（见 [`already_written`]）。台账 §10.45 的读数：三族对同一批
/// 17 轮各出 7 条，21 对跨族句里 19 对的 value 互相包含（两族对同一子句各取宽/窄跨度），
/// 只剩 2 对（f1）两族的 value 取了不相交的跨度（"我在日常会话里干活：…" vs
/// "接下来 5 天我在日常会话里干活"）。这两对**故意不并**：value 不相交时，
/// 说它们"是同一件事"没有程序证据，并了会静默丢一条看着不同的承诺
/// （C5 的不变量：值不同 = 另一条事实）。
///
/// 为什么不拿 quote 做键：`fact` 表**不存 quote**（quote 只在契约校验里当子串证据用，
/// 随后丢弃），要按 quote 判同得先加列改 schema。value_text 是已持久化的同一份证据。
fn span_overlap(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    !a.is_empty() && !b.is_empty() && (a == b || a.contains(b) || b.contains(a))
}

/// 这一条是不是已经落过（S2e-6 的跨 family 去重 + C5 的空值修正 + C5-fix 的宾语身份）。
///
/// 两条判据按 `value` 分岔，因为两种"重复"的形状不同：
/// - **有 value**（"把报价单发给李四"）：同一条证据轮上已经有同样的 `value_text` 就不再写。
///   这条**跨族**生效——同一句话被 promise 与 plan 两次抽到，只该留一行。所以键里**不含**
///   category/predicate。
/// - **没有 value**（"完成 H1a""测试 1810 绿"）：空值是最常见的形状，若仍只按 `(轮, "")` 比，
///   同一轮里的第二条事实会被静默丢掉。这一支按 `(轮, 主语, 类别, 谓词, 宾语身份)` 比槽位。
///   C5-fix：谓词/类别是族常量 ⇒ 旧键退化成 `(轮, 主语)`，同轮同主语的两条不同承诺只落一条；
///   宾语身份用 [`object_key`]（fact 草稿没有字面宾语，这一支实际只剩锚）。
///
/// `evidence_turn_uid` 是写库层取的第一条证据轮（`FactWriteItem::evidence[0]`），所以这里
/// 用的也是 `draft.evidence_turn_uids[0]`——查询键与写入键必须是同一格。
/// 所有 `COALESCE(…,'')`：`NULL` 与空串在"没有值"这件事上是同一件事；两侧都再 `TRIM`：
/// `value=" "` 在批内（Rust 侧 `trim`）与库内（`value_text` 原样存了空白）走同一支。
///
/// §10.45 增补：非空 value 这一支在 `(轮, value 逐字相等)` 之外，还按**跨族同句**判一次——
/// 同一证据轮、类别不同、两段 value 互相包含 ⇒ 同一句只留先写的那行。
/// 只看跨族是量出来的：同族两句 value 重叠时可能是同一子句下的两条槽位，不能并。
pub(crate) fn already_written(
    m: &MemoryHandle,
    evidence_uid: &str,
    value: &str,
    subject_anchor_id: &str,
    category: &str,
    predicate: &str,
    object_anchor_id: &str,
) -> Result<bool, DistillError> {
    // 与批内键同一个函数：空值支的第五格是宾语身份，不是"有没有宾语"。
    let object = object_key(object_anchor_id, "");
    let value = value.trim();
    // 库侧不在 SQL 里另拼一份键规则：取回候选行后**用同一个 `object_key` 比**。
    // 两处各写一份 SQL 表达式的话，改一处就会让批内与库内分叉（重放会长行）。
    // `value_text` 两侧都 `TRIM`：`value=" "` 在批内（Rust 侧）与库内（原样存了空白）
    // 走同一支，`NULL` 与空串在"没有值"这件事上也一样。
    let mut st = m.conn().prepare(
        "SELECT subject_anchor_id, category, predicate,
                TRIM(COALESCE(object_anchor_id,'')),
                TRIM(COALESCE(value_text,''))
           FROM fact
          WHERE evidence_turn_uid=?1",
    )?;
    let rows = st.query_map(params![evidence_uid], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    for row in rows {
        let (row_subject, row_category, row_predicate, row_object, row_value) = row?;
        if value.is_empty() {
            // 空值支只与空值行比槽位：有值的一行说的是另一件事。
            if !row_value.is_empty() {
                continue;
            }
            if row_subject == subject_anchor_id
                && row_category == category
                && row_predicate == predicate
                && object_key(&row_object, "") == object
            {
                return Ok(true);
            }
            continue;
        }
        // 有 value 的一支跨族去重（只看 (轮, value)），不看槽位与宾语。
        if row_value == value {
            return Ok(true);
        }
        if row_category != category && span_overlap(value, &row_value) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 跑一批 fact：先给批内每轮落 `running`（同指纹、同 run_id），再抽取 → 调用
/// [`run_item`]（窄审 + 判定 + 写库）→ 逐轮终态。
///
/// 失败面（设计 §3/§4）：
/// - 抽取 LLM 故障 ⇒ 每轮 `pending`、`attempt+1`、60s 退避，返回原错误；
/// - 抽取**信封**拒收（形状/版本/pass/family/`empty_turns` 的坏引用）⇒ 每轮
///   `rejected(<slug>)`（[`FactPassError::slug`] 的封闭词），并把模型原文的限长副本落
///   `extract_pass_artifact(stage=extract)`（S2e-3）；
/// - 审核 LLM 故障 ⇒ 每轮 `pending`，**一行不写**；
/// - 审核契约拒收 ⇒ 每轮 `quarantined(review_<slug>)`（[`FactPassError::review_reason`]），
///   引擎已把审核原文落 `extract_pass_artifact(stage=review)`；
/// - 写库失败 ⇒ 有草稿的轮 `pending(review_pending)`（H1b：不写 `rejected(store)`），整批已在
///   引擎的事务里回滚；`Conflict` ⇒ 不改 ledger、`Ok(Pending)`（另一写者收尾）。
///
/// S2e-6：跨 family 去重（库里已有或本批已见过 ⇒ 跳过并计 [`FactOutcome::duplicate`]）
/// 在引擎侧完成；runner 只把 `run.duplicate_uids` 映射成逐轮终态。
///
/// S2e-8b：单条草稿被宽松 parser 丢掉是**正常结果**（`Ok`），不是失败：
/// 审核只喂保留的草稿，读数分别记 [`FactOutcome::dropped`] 与 [`FactOutcome::uncovered`]，
/// 每轮状态写 `rejected(dropped)` / `rejected(uncovered)`（见模块文档的判决顺序）。
///
/// artifact 只记解析失败（引擎用 `let _ =` 写，失败不掩盖解析错误）：成功的输出与没有原文的
/// LLM 故障都不进那张表。单条丢弃也不进——它不是"整个回复读不出来"。
///
/// 空抽取（`facts=[]` + `empty_turns` 覆盖全部目标轮）是**合法空批** ⇒ 每轮 `empty`。
///
/// S3a-3a：保留草稿非空时先入 `review_item` 并跑程序闸（§5）——闸不过 ⇒ 每轮
/// `rejected(<gate code>)`、记忆 0 行、**不调审核模型**；闸 pass 则把 item 交给引擎，
/// 判决写 `review_decision`，只有 item=`approved` 才组装写库，写成功记 `apply`。
/// 零草稿不入队，完全走上面的空批路径；同一载荷重跑命中唯一键，不再重问模型、不写第二行。
///
/// S3b-2b：第二次调用（窄审）、判定记账与写库不再由 runner 自己做，全部委托给
/// [`run_item`]；runner 只保留**逐轮/批级终态映射**与错误映射（见下面的分派）。
///
/// S3b-4b：这条签名 = [`run_fact_with`] + [`ReviewMode::Inline`]，既有调用与测试一个字都不用改。
pub fn run_fact(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    l: &LedgerHandle,
    plan: &FactPlan,
    backend: &dyn LlmBackend,
    now_ms: i64,
) -> Result<FactOutcome, DistillError> {
    run_fact_with(m, t, l, plan, backend, now_ms, ReviewMode::Inline)
}

/// 带审核模式的同一条 fact pass（S3b-4b）。
///
/// `mode=Inline` 时行为与旧 [`run_fact`] 逐字节相同；`mode=EnqueueOnly` 只改**一支**：
/// 程序闸 pass 且 item 是 `pending_review` 的那一批不再问审核模型、不写记忆行——
/// 有草稿的轮落 [`pass_state::mark_awaiting_review`]（`pending(review_pending)`），
/// 没有草稿的轮沿用既有终态（`rejected(uncovered|dropped)` / `empty`），读数除
/// `turns`/`drafts`/`dropped`/`uncovered` 外全 0，`state=Pending`（待审不是失败）。
/// `Approved`/`Applied`（判决从台账重建、只差/已写库）与 `Rejected`/`Quarantined`/`Stale`
/// 的早退一字不动：前者仍交给引擎（那两支不问模型），后者仍是既有终态。
#[allow(clippy::too_many_arguments)]
pub fn run_fact_with(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    l: &LedgerHandle,
    plan: &FactPlan,
    backend: &dyn LlmBackend,
    now_ms: i64,
    mode: ReviewMode,
) -> Result<FactOutcome, DistillError> {
    let uids: Vec<String> = plan.batch.rows.iter().map(|r| r.turn_uid.clone()).collect();
    // 空批不该从 `select_fact_batch` 出来；直接调用方给空批也不该落一条没有轮次的 running。
    if uids.is_empty() {
        return Ok(FactOutcome {
            turns: 0,
            drafts: 0,
            dropped: 0,
            uncovered: 0,
            written: 0,
            duplicate: 0,
            rejected: 0,
            uncertain: 0,
            facts: 0,
            cards: 0,
            mentions: 0,
            // 空批不会从选批出来；直接调用方给的空批没有"批内 system 轮"可言。
            system_turns_skipped: plan.system_turns_skipped,
            retry_exhausted: false,
            state: PassState::Empty,
        });
    }

    let family = plan.family;
    let system = fact_system_selected(family);
    let user = fact_user(&plan.batch, &plan.candidates, family, &plan.ctx_turns);
    // 指纹只由程序算：pipeline 版本带 family（三族是三个作业）+ 当前座位 slug + 两次渲染的 system。
    // 抽取的 user 进哈希；审核的输入全部由草稿与批内轮派生 ⇒ 同一批同一 family 的指纹稳定。
    let fingerprint = water::extraction_fingerprint(
        &format!("{FACT_PIPELINE_PREFIX}/{}", family.slug()),
        backend.slug().unwrap_or("unknown"),
        &system,
        &user,
    );
    // P0-2：版本号只喂 system——改判据（system prompt）之后旧轮才回头，批组成变化不算。
    let prompt_version = pass_state::prompt_version(&system);
    let run_id = format!("fact-{}-{now_ms}", family.slug());
    for uid in &uids {
        pass_state::mark_running_with_prompt_version(
            l, key(uid, family), &fingerprint, Some(&prompt_version), &run_id, now_ms)?;
    }

    // ---- 第一次调用：抽取 ----------------------------------------------------
    let raw = match backend.complete(&Prompt { system, user, derive: false }) {
        Ok(raw) => raw,
        Err(failure) => {
            // 运维失败那一支：轮是 pending，不是 rejected。
            // 座位级（传输 / 额度）坏的是座位，不是这一批数据 ⇒ 记一次失败后 `Err`，
            // 交给日路径的连失败闸；**不隔离**（W19c2）。
            if !crate::pass_state::unit_level(failure) {
                for uid in &uids {
                    pass_state::note_attempt(l, key(uid, family), RETRY_BACKOFF_MS, now_ms)?;
                }
                return Err(DistillError::Llm(failure));
            }
            // 单元级（超时 / 答复不合格）：试满 `MAX_PASS_ATTEMPTS` 次的那一格改为隔离，
            // 整批都隔离了就按**成功**交回（`state=quarantined`、落库计数全 0）；
            // 只有一部分到上限 ⇒ `state=pending`：那些轮在退避里，整族继续选下一批。
            let mut all_quarantined = true;
            for uid in &uids {
                all_quarantined &= pass_state::note_attempt_or_quarantine(
                    l, key(uid, family), RETRY_BACKOFF_MS, now_ms)? == PassState::Quarantined;
            }
            if all_quarantined {
                return Ok(FactOutcome {
                    turns: uids.len(),
                    drafts: 0,
                    dropped: 0,
                    uncovered: 0,
                    written: 0,
                    duplicate: 0,
                    rejected: 0,
                    uncertain: 0,
                    facts: 0,
                    cards: 0,
                    mentions: 0,
                    system_turns_skipped: plan.system_turns_skipped,
                    // 整批都到了上限 ⇒ 这是"重试耗尽"那一支：只有这一格能让日路径的
                    // 连续隔离墙推进（W19c2）。
                    retry_exhausted: true,
                    state: PassState::Quarantined,
                });
            }
            return Ok(FactOutcome {
                turns: uids.len(),
                drafts: 0,
                dropped: 0,
                uncovered: 0,
                written: 0,
                duplicate: 0,
                rejected: 0,
                uncertain: 0,
                facts: 0,
                cards: 0,
                mentions: 0,
                system_turns_skipped: plan.system_turns_skipped,
                retry_exhausted: false,
                state: PassState::Pending,
            });
        }
    };
    // ---- 宽松解析（S2e-8b）：信封仍整批拒，单条坏只丢这一条 ----------------
    // §10.47：紧凑档下模型只回 `{"keep":[cN...]}`，程序补信封后走同一个宽松 parser。
    let parsed = if compact_prompt() {
        parse_fact_compact(&raw, &plan.batch, &plan.candidates, family)
    } else {
        parse_fact_output_lenient(&raw, &plan.batch, &plan.candidates, family)
    };
    let lenient = match parsed {
        Ok(lenient) => lenient,
        Err(e) => {
            // 信封/批次级声明坏了：整批拒收，模型不再被问第二次。reason 写坏在哪一格（S2e-1），
            // 不再是笼统的 `schema`——真模型冒烟要能直接读出 `json` 还是 `family`。
            // S2e-3：slug 之外再留证据——模型原文的限长副本进 `extract_pass_artifact`。
            // 写失败用 `let _ =`：artifact 是诊断的补件，不许盖住真正的解析错误。
            let _ = pass_state::record_artifact(
                l, PASS_FACT, family.slug(), "extract", &run_id, &uids[0], &raw, now_ms);
            mark_every_turn(l, &uids, family, PassState::Rejected, Some(e.slug()), now_ms)?;
            return Err(DistillError::Fact(e));
        }
    };
    // `drafts` 只数保留的合法草稿；`drafts + dropped` 才是模型原始草稿数。
    let drafts = lenient.drafts;
    let dropped = lenient.dropped.len();
    // 覆盖缺口是批内短 id（`t*`）：写状态前映射回真实 `turn_uid`（S2e-8b）。
    let uncovered = uncovered_uids(&lenient.uncovered, &plan.batch);
    // §10.63 同形：被丢草稿的原因按 t-ref 映射回真实轮；只要真有草稿被丢就把模型原文留一份。
    let dropped_reasons = dropped_reasons(&lenient.dropped_turns, &plan.batch);
    // 每个被丢草的轮各留一份原文（批 >1 时同一份回复按轮复制；§10.70 的 event 同形）。
    if dropped > 0 {
        let mut kept = false;
        for uid in dropped_reasons.keys() {
            if pass_state::record_artifact(
                l, PASS_FACT, family.slug(), "extract", &run_id, uid, &raw, now_ms).is_ok() {
                kept = true;
            }
        }
        if !kept {
            let _ = pass_state::record_artifact(
                l, PASS_FACT, family.slug(), "extract", &run_id, &uids[0], &raw, now_ms);
        }
    }
    // 有合法草稿的轮：占位失败/Store 失败时把这一格收回 `pending(review_pending)`。
    let draft_turns: HashSet<&str> = drafts.iter().map(|draft| draft.turn_uid.as_str()).collect();

    // ---- 待审队列：入队 + 程序闸（零草稿走既有空批路径，不入队）-------------
    // 程序闸：**批级前置**坏（payload_shape/turn_missing/…）或**全部** draft 都坏
    // ⇒ 整批 `rejected(<码>)`、记忆 0 行、不调审核模型（与今天一致）；
    // 混合批 ⇒ item 记 program pass，只有坏 draft 的证据轮 `rejected(<码>)`，
    // 通过的 draft 照常进窄审/写库（I3：判定单位是 draft）。
    let mut item_id: Option<i64> = None;
    // H1b：这一格是否要占位。`pending_review`/`approved` 会写库，必须先抢到租约；
    // `applied` 的引擎路径只重读台账（`run_applied` 不写库），状态闭集也不收它，不占位。
    let mut claimable = false;
    // 程序闸拒收的 draft 的证据轮 `(turn_uid, 码)`：逐轮落 rejected(<码>)，后续映射不再覆盖。
    let mut program_turns: Vec<(String, &'static str)> = Vec::new();
    if !drafts.is_empty() {
        let payload_items: Vec<serde_json::Value> = drafts
            .iter()
            .map(|draft| {
                let evidence: Vec<serde_json::Value> = draft
                    .evidence_turn_uids
                    .iter()
                    .map(|uid| {
                        let role = plan
                            .batch
                            .row(uid)
                            .map(|row| row.role.clone())
                            .unwrap_or_default();
                        serde_json::json!({"turn_uid": uid, "anchor_role": role})
                    })
                    .collect();
                // S3b-1：载荷必须**只凭自身就能重放**审核输入（`review::replay`）——
                // `turn_uid` 定位 `TARGET TURN`，两个 canonical 是 `fact_review_user`
                // 的第一、二格。写库层不读这三格（仍用内存里的草稿）。
                serde_json::json!({
                    "turn_uid": draft.turn_uid,
                    "subject_anchor_id": draft.subject_anchor_id,
                    "object_anchor_id": draft.object_anchor_id,
                    "subject_canonical": draft.subject_canonical,
                    "object_canonical": draft.object_canonical,
                    "predicate": draft.predicate,
                    "value": draft.value,
                    "category": draft.category,
                    "mode": draft.mode,
                    "time_text": draft.time_text,
                    "quote": draft.quote,
                    "evidence": evidence,
                    "text": draft.text,
                })
            })
            .collect();
        // 载荷必须可重放：`run_id` 用抽取指纹（同批同 model/prompt 稳定），不拿墙上钟——
        // 否则同一载荷每次都换 sha，`review_item_payload` 唯一键永远命不中。
        let payload = review::pass_payload(
            "fact",
            family.slug(),
            &fingerprint,
            &plan.batch,
            &plan.ctx,
            &plan.owner,
            &plan.scope,
            serde_json::Value::Array(payload_items),
            None,
        );
        let owner_json = review::owner_json(&plan.owner, &plan.scope);
        let queued_item = review::new_item("fact", family.slug(), &plan.batch, owner_json, &payload);
        let queued = enqueue(m, &queued_item, now_ms)?;
        let item = get(m, queued.id)?.ok_or_else(|| {
            rc_memory::MemError::Sql("review: enqueued item vanished".to_string())
        })?;
        match queued.state {
            ReviewState::PendingReview => {
                // I3：先算一次逐 draft 的闸。`program_decided` 只挡重复记账，不挡读闸——
                // 重跑时同样要知道"哪几条 draft 已程序拒收"。
                let report = review::gate_report(m, t, &item, now_ms)?;
                program_turns = report.rejected_turns();
                if !review::program_decided(m, queued.id)? {
                    if report.batch_code.is_some() || report.all_failed() {
                        // 批级前置坏 / 全部 draft 都坏：没有好结果可保，与今天一致整批 rejected。
                        let code = report.code.unwrap_or("payload_shape");
                        review::record_program(
                            m,
                            queued.id,
                            QueueVerdict::Reject,
                            code,
                            &item.payload_sha256,
                            now_ms,
                        )?;
                        mark_every_turn(
                            l,
                            &uids,
                            family,
                            PassState::Rejected,
                            Some(code),
                            now_ms,
                        )?;
                        return Ok(FactOutcome {
                            turns: uids.len(),
                            drafts: drafts.len(),
                            dropped: 0,
                            uncovered: 0,
                            written: 0,
                            duplicate: 0,
                            rejected: 0,
                            uncertain: 0,
                            facts: 0,
                            cards: 0,
                            mentions: 0,
                            system_turns_skipped: plan.system_turns_skipped,
                            retry_exhausted: false,
                            state: PassState::Rejected,
                        });
                    }
                    // 混合批：item 记 program pass（不是 reject——好草稿还要进审核/写库）。
                    review::record_program(
                        m,
                        queued.id,
                        QueueVerdict::Pass,
                        review::PROGRAM_PASS,
                        &item.payload_sha256,
                        now_ms,
                    )?;
                }
                // 坏草稿的轮：程序驳回是确定性终态，当场落；后续逐轮映射不覆盖它们。
                mark_program_rejected(l, &uids, family, &program_turns, now_ms)?;
                // S3b-4b：`--no-review` 只跳过"pending 的窄审"。闸已 pass、item 仍是
                // `pending_review` ⇒ 不再问模型：有草稿的轮等审核，没有草稿的轮沿用既有终态。
                // 这一段与 Inline 的最终映射同序（不确定 > 已写出 > 审核全拒 > 丢/漏 > 空），
                // 只是"不确定/已写出/审核全拒"这三格此刻都不可能发生（没有判决）。
                if mode == ReviewMode::EnqueueOnly {
                    mark_awaiting_or_extraction_terminal(
                        l, &uids, family, &draft_turns, &uncovered, dropped, &dropped_reasons, now_ms,
                    )?;
                    // 等审核的映射会把坏草稿的轮写成 pending；程序驳回不因"等补审"复活。
                    mark_program_rejected(l, &uids, family, &program_turns, now_ms)?;
                    return Ok(FactOutcome {
                        turns: uids.len(),
                        drafts: drafts.len(),
                        dropped,
                        uncovered: uncovered.len(),
                        written: 0,
                        duplicate: 0,
                        rejected: 0,
                        uncertain: 0,
                        facts: 0,
                        cards: 0,
                        mentions: 0,
                        system_turns_skipped: plan.system_turns_skipped,
                        retry_exhausted: false,
                        state: PassState::Pending,
                    });
                }
                item_id = Some(queued.id);
                claimable = true;
            }
            ReviewState::Approved | ReviewState::Applied => {
                // 上次 apply 失败 / 已写库：窄审与逐条判决都由引擎按台账重建，
                // runner 不重问模型、不猜"全部 pass"。
                item_id = Some(queued.id);
                // H1b：只有 approved 会写库；applied 是只读重放，不占位（见 `claimable`）。
                claimable = queued.state == ReviewState::Approved;
            }
            ReviewState::Rejected => {
                // 终态：不调模型、不写库，按既有状态映射逐轮终态。
                let code = item.meta.program_code.as_deref().unwrap_or(REVIEW_REJECT_REASON);
                mark_every_turn(l, &uids, family, PassState::Rejected, Some(code), now_ms)?;
                return Ok(FactOutcome {
                    turns: uids.len(),
                    drafts: drafts.len(),
                    dropped: 0,
                    uncovered: 0,
                    written: 0,
                    duplicate: 0,
                    rejected: drafts.len(),
                    uncertain: 0,
                    facts: 0,
                    cards: 0,
                    mentions: 0,
                    system_turns_skipped: plan.system_turns_skipped,
                    retry_exhausted: false,
                    state: PassState::Rejected,
                });
            }
            ReviewState::Quarantined => {
                mark_every_turn(
                    l,
                    &uids,
                    family,
                    PassState::Quarantined,
                    Some(REVIEW_UNCERTAIN_REASON),
                    now_ms,
                )?;
                return Ok(FactOutcome {
                    turns: uids.len(),
                    drafts: drafts.len(),
                    dropped: 0,
                    uncovered: 0,
                    written: 0,
                    duplicate: 0,
                    rejected: 0,
                    uncertain: drafts.len(),
                    facts: 0,
                    cards: 0,
                    mentions: 0,
                    system_turns_skipped: plan.system_turns_skipped,
                    retry_exhausted: false,
                    state: PassState::Quarantined,
                });
            }
            ReviewState::Stale => {
                mark_every_turn(l, &uids, family, PassState::Rejected, Some("stale"), now_ms)?;
                return Ok(FactOutcome {
                    turns: uids.len(),
                    drafts: drafts.len(),
                    dropped: 0,
                    uncovered: 0,
                    written: 0,
                    duplicate: 0,
                    rejected: drafts.len(),
                    uncertain: 0,
                    facts: 0,
                    cards: 0,
                    mentions: 0,
                    system_turns_skipped: plan.system_turns_skipped,
                    retry_exhausted: false,
                    state: PassState::Rejected,
                });
            }
        }
    }

    // ---- 占位：入队与程序闸都完成后、调引擎之前（H1b）---------------------------
    // `false`（另一写者持有 / 退避未到）不碰引擎：有草稿的轮等审核，读数只报本批已确定
    // 的那四格（`turns`/`drafts`/`dropped`/`uncovered`），写库面全 0，`state=Pending`。
    // 已有 `applied` 的 item 走只读重放（`claimable=false`），既有幂等重放读数不变。
    let mut lease_owner: Option<String> = None;
    if let (Some(id), true) = (item_id, claimable) {
        let owner = format!("distill-{}-{now_ms}", std::process::id());
        if claim(m, id, &owner, review::REVIEW_LEASE_MS, now_ms)? {
            lease_owner = Some(owner);
        } else {
            // 另一写者正在处理这条 item：有草稿的轮等审核，没有草稿的轮沿用抽取终态
            // （不把它们留在 `running`——那会让下一次选批永远跳过这一轮）。
            mark_awaiting_or_extraction_terminal(
                l, &uids, family, &draft_turns, &uncovered, dropped, &dropped_reasons, now_ms,
            )?;
            // I3：程序驳回的轮不因"另一写者持有"回到 pending。
            mark_program_rejected(l, &uids, family, &program_turns, now_ms)?;
            return Ok(FactOutcome {
                turns: uids.len(),
                drafts: drafts.len(),
                dropped,
                uncovered: uncovered.len(),
                written: 0,
                duplicate: 0,
                rejected: 0,
                uncertain: 0,
                facts: 0,
                cards: 0,
                mentions: 0,
                system_turns_skipped: plan.system_turns_skipped,
                retry_exhausted: false,
                state: PassState::Pending,
            });
        }
    }

    // ---- 第二次调用 + 判定 + 写库：整段交给 review::run（S3b-2b）-------------
    // 引擎负责窄审调用、判定记账与写库；runner 只做错误映射与逐轮/批级终态映射。
    // 抢到的租约无论成败都释放（`let _ =`：释放失败不许盖住真正的错误）。
    let run = match item_id {
        Some(id) => {
            let result = run_item(m, t, l, id, backend, now_ms);
            if let Some(owner) = lease_owner.as_deref() {
                let _ = release(m, id, owner);
            }
            match result {
                Ok(run) => {
                    // S4：内联窄审（`distill once` / `cycle once` 的默认路径）与 `hub review run`
                    // 是同一条 apply，所以卡也必须在这里提升——同一份 helper，同一份口径。
                    // **失败不改变这一批的结果**：行已提交、item 已 applied，把错误上抛只会让
                    // 下面把有草稿的轮重铺成 `pending(review_pending)`（那会重跑一遍已写过的
                    // 批），而卡停在 `unreviewed` 仍有 audit 兜底——与 `let _ = release(..)`、
                    // artifact 写这些记账口同一把尺子。
                    let _ = promote_applied_cards(m, &run, now_ms);
                    Some(run)
                }
                Err(RunError::Llm(failure)) => {
                    // 审核都开不了口 ⇒ 一行不写；轮回到 pending，退避后可整批重来。
                    // 座位级（传输 / 额度）：记一次失败后 `Err`，不隔离（W19c2）。
                    if !crate::pass_state::unit_level(failure) {
                        for uid in &uids {
                            pass_state::note_attempt(
                                l, key(uid, family), RETRY_BACKOFF_MS, now_ms)?;
                        }
                        return Err(DistillError::Llm(failure));
                    }
                    // 单元级：试满上限的那一格隔离，整批隔离了就按成功交回；否则按 pending
                    // 交回——退避中的那几轮不会被立刻重选，整族继续（与抽取那一支同一条政策）。
                    let mut all_quarantined = true;
                    for uid in &uids {
                        all_quarantined &= pass_state::note_attempt_or_quarantine(
                            l, key(uid, family), RETRY_BACKOFF_MS, now_ms)?
                            == PassState::Quarantined;
                    }
                    if all_quarantined {
                        return Ok(FactOutcome {
                            turns: uids.len(),
                            drafts: 0,
                            dropped: 0,
                            uncovered: 0,
                            written: 0,
                            duplicate: 0,
                            rejected: 0,
                            uncertain: 0,
                            facts: 0,
                            cards: 0,
                            mentions: 0,
                            system_turns_skipped: plan.system_turns_skipped,
                            retry_exhausted: true,
                            state: PassState::Quarantined,
                        });
                    }
                    return Ok(FactOutcome {
                        turns: uids.len(),
                        drafts: 0,
                        dropped: 0,
                        uncovered: 0,
                        written: 0,
                        duplicate: 0,
                        rejected: 0,
                        uncertain: 0,
                        facts: 0,
                        cards: 0,
                        mentions: 0,
                        system_turns_skipped: plan.system_turns_skipped,
                        retry_exhausted: false,
                        state: PassState::Pending,
                    });
                }
                Err(RunError::Review(e)) => {
                    // 审核给了不是判决的东西：引擎已记 `uncertain(<reason>)` 并落了 artifact，
                    // runner 只把同一个 reason 铺到每一轮（隔离整批，不是 rejected）。
                    let reason = match &e {
                        DistillError::Fact(err) => err.review_reason(),
                        // 引擎的 Review 只装 fact 的审核错误；其它变体是接线错误，按拒收收尾。
                        _ => STORE_FAILURE_REASON.to_string(),
                    };
                    mark_every_turn(
                        l,
                        &uids,
                        family,
                        PassState::Quarantined,
                        Some(&reason),
                        now_ms,
                    )?;
                    return Err(e);
                }
                Err(RunError::Store(e)) => {
                    // 写库失败：引擎不记 apply，item 留在 approved；有草稿的轮收回
                    // `pending(review_pending)`，让下一次补审能接着跑。**不写 rejected(store)**：
                    // 失败没有改变这一批的判定，ledger 不该把"等补审"读成"被驳回"。
                    for uid in &uids {
                        if draft_turns.contains(uid.as_str()) {
                            pass_state::mark_awaiting_review(l, key(uid, family), now_ms)?;
                        }
                    }
                    // I3：程序驳回与"写库失败"无关——坏草稿的轮照旧 rejected(<码>)。
                    mark_program_rejected(l, &uids, family, &program_turns, now_ms)?;
                    return Err(e);
                }
                Err(RunError::Conflict) => {
                    // 另一写者已推进状态（或非法转换），本次判定整体回滚：**不改任何 ledger 行**，
                    // 由那个写者收尾。对 runner 而言这是一次"没做成"，不是失败 ⇒ Ok(Pending)。
                    return Ok(FactOutcome {
                        turns: 0,
                        drafts: 0,
                        dropped: 0,
                        uncovered: 0,
                        written: 0,
                        duplicate: 0,
                        rejected: 0,
                        uncertain: 0,
                        facts: 0,
                        cards: 0,
                        mentions: 0,
                        // 另一写者已推进状态：本进程什么都没做，读数全 0（选批那格也归 0）。
                        system_turns_skipped: 0,
                        retry_exhausted: false,
                        state: PassState::Pending,
                    });
                }
                // 状态不对 / 找不到 item / `ReviewNaming`（naming 的分派）：接线错误，
                // 不猜终态、不动 ledger，只给一句静态话。
                Err(RunError::Unsupported)
                | Err(RunError::MissingItem)
                | Err(RunError::ReviewNaming(_))
                | Err(RunError::ReviewRows(_))
                // 波三刀 A：lesson 的审核 parser 只由 lesson item 产生，fact 走不到 —— 接线错误。
                | Err(RunError::ReviewLesson(_)) => {
                    return Err(DistillError::Memory(rc_memory::MemError::Sql(
                        "review run: unexpected state".to_string(),
                    )));
                }
            }
        }
        // 零草稿不入队（上面已走空批路径）：没有窄审可问，判决为空、读数全 0。
        None => None,
    };
    let (
        item_state,
        verdicts,
        fail_closed_reason,
        duplicate_turns,
        written,
        duplicate,
        facts,
        cards,
        mentions,
        engine_program_turns,
    ) = match run {
        Some(run) => (
            run.item_state,
            run.verdicts,
            run.fail_closed_reason,
            run.duplicate_uids,
            run.written,
            run.duplicate,
            run.facts,
            run.cards,
            run.mentions,
            run.program_rejected_turns,
        ),
        None => (
            ReviewState::Approved,
            Some(Vec::new()),
            None,
            BTreeSet::new(),
            0,
            0,
            0,
            0,
            0,
            Vec::new(),
        ),
    };
    // I3：程序闸拒收的轮以**引擎读数**为准（重跑/approved 路径同样成立）；引擎没跑的
    // 空批路径沿用上面从闸里算出的那一份。
    if !engine_program_turns.is_empty() {
        program_turns = engine_program_turns;
    }

    // ---- 判决不可重建（fail-closed）：不猜判决，尤其不退回全 pass -------------
    let Some(verdicts) = verdicts else {
        // 这不是 Err：Err 会被运维读成"可重试失败"，而这里的重试只会重复同一个结果。
        // applied：行已经写过（事务已提交），不回滚也不冒充"某人被 reject"，逐轮按 Done 收尾。
        if item_state == ReviewState::Applied {
            mark_every_turn(l, &uids, family, PassState::Done, None, now_ms)?;
            return Ok(FactOutcome {
                turns: uids.len(),
                drafts: drafts.len(),
                dropped,
                uncovered: uncovered.len(),
                written: 0,
                duplicate: 0,
                rejected: 0,
                uncertain: 0,
                facts: 0,
                cards: 0,
                mentions: 0,
                system_turns_skipped: plan.system_turns_skipped,
                retry_exhausted: false,
                state: PassState::Done,
            });
        }
        // approved：一行不写，逐轮隔离，等人看台账。
        mark_every_turn(
            l,
            &uids,
            family,
            PassState::Quarantined,
            Some(fail_closed_reason.unwrap_or(review::REVIEW_VERDICTS_MISSING)),
            now_ms,
        )?;
        return Ok(FactOutcome {
            turns: uids.len(),
            drafts: drafts.len(),
            dropped,
            uncovered: uncovered.len(),
            written: 0,
            duplicate: 0,
            rejected: 0,
            uncertain: 0,
            facts: 0,
            cards: 0,
            mentions: 0,
            system_turns_skipped: plan.system_turns_skipped,
            retry_exhausted: false,
            state: PassState::Quarantined,
        });
    };

    // ---- 按判决分流：ref → 草稿（顺序 = drafts）-------------------------------
    let by_ref: HashMap<&str, QueueVerdict> = verdicts
        .iter()
        .map(|(reference, verdict)| (reference.as_str(), *verdict))
        .collect();
    let mut rejected = 0usize;
    let mut uncertain = 0usize;
    let mut draft_turns: HashSet<String> = HashSet::new();
    let mut pass_turns: HashSet<String> = HashSet::new();
    let mut uncertain_turns: HashSet<String> = HashSet::new();
    for draft in &drafts {
        draft_turns.insert(draft.turn_uid.clone());
        // 引擎的 ref 就是草稿自身的 reference（`f1..fN`）；对不上按 Reject——少写，不猜 pass。
        match by_ref.get(draft.reference.as_str()).copied().unwrap_or(QueueVerdict::Reject) {
            QueueVerdict::Pass => {
                // 有 pass 行：这一轮**已处理**（写库/去重都在引擎侧完成）。
                pass_turns.insert(draft.turn_uid.clone());
            }
            QueueVerdict::Reject => rejected += 1,
            QueueVerdict::Uncertain => {
                uncertain += 1;
                uncertain_turns.insert(draft.turn_uid.clone());
            }
        }
    }

    // ---- 每轮终态 -----------------------------------------------------------
    // 顺序即判决（S2e-8b 补充见模块文档）：不确定 > 已写出 > 审核全拒 > 草稿被丢/模型漏轮 > 空。
    // I3：程序闸拒收的轮**已经落过** rejected(<该 draft 的失败码>)，这里不再改写——
    // 尤其不能把"坏草稿的轮"读成 review_reject 或 Done。
    let mut states: Vec<PassState> = Vec::with_capacity(uids.len());
    for uid in &uids {
        if program_turns.iter().any(|(turn, _)| turn == uid) {
            states.push(PassState::Rejected);
            continue;
        }
        let (state, reason) = if uncertain_turns.contains(uid) {
            // 任一不确定草稿 ⇒ 该轮隔离（即使同一轮另有一条 pass 行）。
            (PassState::Quarantined, Some(REVIEW_UNCERTAIN_REASON))
        } else if pass_turns.contains(uid) || duplicate_turns.contains(uid) {
            // 有 pass 行，或该轮的草稿全被去重：这一轮**已处理**（内容早就在库里），不是失败。
            (PassState::Done, None)
        } else if draft_turns.contains(uid) {
            // 有合法草稿但全被审核 reject。
            (PassState::Rejected, Some(REVIEW_REJECT_REASON))
        } else if uncovered.contains(uid) {
            // 既没有合法草稿覆盖、也没被 `empty_turns` 宣告。契约侧的 `covered` 只认**保留**的
            // 草稿，所以"草稿被丢"的轮也出现在这里；§10.63 起写**坏在哪一格**（映射不到才退回
            // `dropped`），一条都没丢时才写 `uncovered`（模型真漏了）。
            let reason = if dropped > 0 {
                dropped_reasons.get(uid).copied().unwrap_or(DROPPED_REASON)
            } else {
                UNCOVERED_REASON
            };
            (PassState::Rejected, Some(reason))
        } else {
            // 被 `empty_turns` 覆盖：合法空产出。
            (PassState::Empty, None)
        };
        pass_state::mark_terminal(l, key(uid, family), state, reason, now_ms)?;
        states.push(state);
    }

    // ---- 批级摘要 -----------------------------------------------------------
    let state = if states.contains(&PassState::Quarantined) {
        PassState::Quarantined
    } else if states.iter().all(|s| *s == PassState::Empty) {
        PassState::Empty
    } else if states.contains(&PassState::Done) {
        PassState::Done
    } else {
        PassState::Rejected
    };

    Ok(FactOutcome {
        turns: uids.len(),
        drafts: drafts.len(),
        dropped,
        uncovered: uncovered.len(),
        written,
        duplicate,
        rejected,
        uncertain,
        facts,
        cards,
        mentions,
        system_turns_skipped: plan.system_turns_skipped,
        retry_exhausted: false,
        state,
    })
}

/// `--family` 的字面值 → 枚举。认不出就 `None`（调用方把它读成用法错误，不回显那个词）。
pub fn family_of_slug(slug: &str) -> Option<FactFamily> {
    FactFamily::ALL.into_iter().find(|family| family.slug() == slug)
}
