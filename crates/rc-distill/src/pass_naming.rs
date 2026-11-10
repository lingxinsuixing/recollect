//! S2a-2：naming pass 的库面（设计 `2026-10-26-s2-prompt-v0` §2.1、§3、§7）。
//!
//! 一条真实 pass 只做三件事：从转录里**选**一轮还没表态的批次 → 跑现有 A1 双跑 →
//! 带会话归属落 `anchor/alias/mention/provenance` 并写 `extract_pass_state`。
//! 本片不做 CLI、不做 fact/event/time（模型面经 `run_a1_pair` / 窄审经 `run_item`，座位由调用方给）。
//!
//! S3a-3b：双跑合成与噪声闸之后、写库之前插入**待审队列**
//! （`review_item` + 程序闸 + 判定，设计 `2026-10-26-review-queue-s3` §4/§5/§8）。
//!
//! S3b-3c：闸之后的"窄审 + 判定 + 写库"不再由本模块做，整段委托给
//! [`review::run::run_item`]——naming 从"闸过即 approved"（S3a 的 `program_only` 临时口径）
//! 变成"闸过 → 窄审（`pass-naming-review-v0.2`）→ 只写通过审核的锚"。本模块只剩：
//! 选批、双跑 A1、合成/噪声闸、入队、程序闸，以及把引擎读数折成逐轮终态。
//!
//! 四条纪律：
//! 1. **选批只认 `extract_pass_state`**：不看 `extract_state`，也不看 `job_watermark`。
//!    那是 legacy v3 的轮级汇总与游标（设计 §3 的"不能复用 `select_batch`"），两条管线不互相记账。
//! 2. **一次读表态集合**：`(turn, 'naming', '')` 的行按"要不要重跑"分三类——没有行、`stale`、
//!    以及 `pending` 且退避已到期（`next_retry_at_ms IS NULL` 或 `<= now_ms`）都可重选；
//!    `running`/`done`/`empty`/`rejected`/`quarantined` 与**未到期**的 `pending` 不重选。
//!    判据与 `pass_state::should_run` 同向，但选批不比较指纹（prompt 版本判断是 runner 的事）。
//!    P0-2 例外：终态行的 `prompt_version` 非空且与当前 system prompt 版本不同 ⇒ 可重选
//!    （改判据后旧轮自动回头）；NULL 不重选。
//! 3. **A1 复用不改**：`run_a1_pair` + `plan_a1` 一字不重写 prompt；
//!    指纹只由 `pipeline|model|system|user` 算出（`water::extraction_fingerprint`）。
//! 4. **状态先写 running 再调模型**：LLM 故障 ⇒ 每轮 `pending`（60s 退避）；契约/A1 失败 ⇒
//!    每轮 `rejected(schema)`；窄审拒收 ⇒ 每轮 `quarantined(<slug>)`；写库失败 ⇒ 整批轮
//!    `pending(review_pending)`（H1b：不写 `rejected(store)`，便于补审）。单元级（超时 / 答复不合格）
//!    试满上限 ⇒ `quarantined` + `retry_exhausted`（W19c2），未满 ⇒ `pending`（均 `Ok` 交回）；
//!    只有座位级 / 契约 / 写库失败走 `Err`。失败整批回滚由
//!    `write_a1_with_owner` 的单事务保证（写库仍由引擎调用这一个入口）——本模块不写半批。

use crate::a1_run::run_a1_pair;
use crate::batch::{count_system_turns_in_span, Batch, TurnRow};
use crate::contract_v4::plan_a1;
use crate::pass_prompt::{is_compaction_body, is_runtime_notice, COMPACT_SUMMARY_REASON, NOTICE_REASON};
use crate::prepare::evidence_view;
use crate::prompt_v4::{a1_system, a1_user, AnchorCandidate};
use crate::review::{self, ReviewMode};
use crate::DistillError;
use rc_ledger::pass_state::{self, PassKey, PassState};
use rc_ledger::water;
use rc_llm::LlmBackend;
use rc_memory::owner::{self, OwnerCtx};
use rc_memory::review::{
    claim, enqueue, get, release, ReviewState, ReviewVerdict as QueueVerdict,
};
use rc_memory::writers_v4::{A1Anchor, A1Batch, A1Mention};
use rc_memory::{BatchCtx, MemError, Scope};
use rc_store::{LedgerHandle, MemoryHandle, StoreError, TranscriptHandle};
use rusqlite::types::ValueRef;
use rusqlite::{params, Row};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};

/// `extract_pass_state.pass_id` 的这一格。`family` 恒空串（只有 fact 分族）。
pub const NAMING_PASS: &str = "naming";

/// 指纹里的 pipeline 那一段。它是 pass 版本的唯一名字，改 prompt 语义时一起改。
pub const NAMING_PIPELINE: &str = "pass-naming-v0.1";

/// 调用方给 0 时的默认窗宽（设计 §2.1：naming 8–20 轮）。
pub const DEFAULT_LIMIT: usize = 20;

/// LLM 故障的退避（设计 §3 的重试/退避：60s 起）。
pub const RETRY_BACKOFF_MS: i64 = 60_000;

/// 写库失败的 reason slug。`rejected(schema)` 是模型那侧的词，存储那侧必须自己有一个。
const STORE_FAILURE_REASON: &str = "store";

/// `stale` item 的逐轮终态理由（与 fact 同一词：旧指纹作废，载荷换了）。
const STALE_REASON: &str = "stale";

/// 一批评 naming 的计划：批次、P 独占列、归属、行 scope、候选锚。
/// 全部来自程序（转录行 + `sessions` 的归属列 + 记忆库的活锚），模型一个字段都递不进来。
#[derive(Debug, Clone)]
pub struct NamingPlan {
    pub batch: Batch,
    /// 批区间内被跳过的 `role='system'` 轮数（转换器配套）：它们只作上下文，不作 TARGET。
    /// 读数的 `system_turns_skipped` 就是这一格；naming 没有 CTX 段（`a1_user` 只渲染
    /// TURNS），所以这一格在这里是"这批里有多少条工具输出被挡在批外"的可见读数。
    pub system_turns_skipped: usize,
    pub ctx: BatchCtx,
    pub owner: OwnerCtx,
    pub scope: Scope,
    pub candidates: Vec<AnchorCandidate>,
    /// P0-1：这批话的主人，**渲染用的一格**（`a1_user` 的第一行）。来源是
    /// `resolve_principal` 的同一个读数（与 `owner.owner_principal_id` 同一次查询），
    /// 取值顺序是"可读名 → 稳定 id → 无"。它**不进** `owner` 那一列：归属列只要 id。
    pub principal_name: Option<String>,
}

/// 一批的读数。`anchors`/`mentions` 与 `A1Outcome` 同口径：`anchors` 是本批去重锚数
/// （重放同值），`mentions` 是本次真正新插入的行数。`filtered` 是后置闸丢掉的锚数（S2e-5）。
/// `ungrounded_mentions` 是逐轮核对时被丢掉的**提及**数（C4b）：不杀锚，也不参与成败判定。
/// `system_turns_skipped` 是选批层面的一格：批区间内 `role='system'` 的轮数（转换器配套，
/// 它们只作上下文、永不作 TARGET）。它是 additive：既有格一个不改，也不进 `schema_version`。
#[derive(Debug, Clone)]
pub struct NamingOutcome {
    pub turns: usize,
    pub anchors: usize,
    pub mentions: usize,
    /// 噪声闸丢掉的锚数。计数在 `filtered` 与失败无关：过滤是多写一笔读数，不是故障。
    pub filtered: usize,
    /// 非 speaker 提及里 surface 不落在自己那条轮的条数（C4b）。同 `filtered`：只丢行，不算失败。
    pub ungrounded_mentions: usize,
    /// 批区间内被跳过的 `role='system'` 轮数（选批层面的读数，与这批跑成什么无关）。
    pub system_turns_skipped: usize,
    pub state: PassState,
    /// 这一批是不是"单元级失败试满上限、整批落成隔离"（W19c2）。**只在那一支为真**。
    ///
    /// 它与 `state` 不是一回事：`Quarantined` 另有来源（审核不确定、迟到的同维度异值、
    /// 程序判出的推导件）——那些是**质量**隔离，不是这一格卡死。日路径的连续隔离墙只认这一格：
    /// 从 `state` 猜来源会把质量隔离读成座位卡死，从而停掉一族本可以继续的活。
    pub retry_exhausted: bool,
}

/// `select_naming_batch` 读到的会话行。归属列 + scope 两格在这里一次读全：
/// 两个 `Option<String>` 的语义不同（`project_hint` = 归属键提示、`principal_name` =
/// 可读名），元组位置一错就是静默串值，所以用结构体（同 `extract::select_batch`）。
///
/// 身份层 I1 起主人身份**不在这一行里**：它每会话走
/// `rc_ledger::transcript::resolve_principal`（claim 表最新一条，缺省回落 `sessions` 那一列），
/// 因此这里不读 `owner_principal_id`。
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

/// 选批读到的一格表态：`state`、退避到期时刻、理由、产下这一行的 prompt 版本（P0-2）。
type StatedRow = (PassState, Option<i64>, Option<String>, Option<String>);

/// 这一轮今天该不该再跑。`None` = 没有状态行（从没跑过）。
///
/// 与 `pass_state::should_run` 的差别只有一个：这里不看指纹。选批只回答"有没有活可干"，
/// "这份活是不是同一份"是 runner 的指纹与 `stale` 标记管的事（设计 §3 的幂等）。
///
/// H5a：`pending(review_pending)` **不可重选**——那一轮的产出已经在 `review_item` 里等
/// `review run`，再选一次只会重抽一遍、白烧两次调用。其余 `pending`（含 NULL reason 的
/// 退避行）到期后照旧可选；`stale` 与"没有行"照旧可选。
///
/// P0-2：终态行另看 `prompt_version`——它非空且与当前 system prompt 的版本不同 ⇒ 可重选
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

/// 选一批该跑 naming 的轮次。可重选判据（设计 §3 的幂等与重试/退避）：
/// - `extract_pass_state` 没有 `(turn, 'naming', '')` 行 ⇒ 可（从没跑过）；
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
/// `rejected(compact_summary)` 并跳过——摘要是索引，不是命名来源；落状态是为了不再每批重扫。
pub fn select_naming_batch(
    t: &TranscriptHandle,
    l: &LedgerHandle,
    m: &MemoryHandle,
    limit: usize,
    now_ms: i64,
) -> Result<Option<NamingPlan>, DistillError> {
    let limit = if limit == 0 { DEFAULT_LIMIT } else { limit };
    // P0-2：当前 system prompt 的版本号——**选批这一侧**也要算，才能和状态行里的那一格比。
    // 只喂 system（`a1_system`），不喂批；批组成变了不会造成"版本不同"。
    let current_version = pass_state::prompt_version(&a1_system());
    // ① 每轮的表态（ledger.db）——**一次**读，不许在会话循环里读（那是 N 次全表扫）。
    //    值带上 `next_retry_at_ms`：判定"pending 到期"与"pending 未到期"是选批这一层的活，
    //    runner 只负责在失败后写下那个时刻（设计 §3 的 60s 退避）。
    let stated: HashMap<String, StatedRow> = {
        let mut st = l.conn().prepare(
            "SELECT turn_uid, state, next_retry_at_ms, reason, prompt_version
             FROM extract_pass_state
             WHERE pass_id=?1 AND family=''",
        )?;
        let got = st.query_map(params![NAMING_PASS], |r| {
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

    // ② 会话清单（transcript.db）。顺序确定 ⇒ 同一库同一状态每次都选同一个会话。
    //    S1b-C1/C3 的归属列在这里读出默认值：旧会话这些列是 NULL ⇒ owner=source_agent、
    //    private_agent、default、project（"未知 ⇒ 最保守"）；scope_key 的口径是
    //    `COALESCE(scope_key, project_hint, session_key)`，与 `Batch::scope_key()` 同向。
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
            // project_hint 是 NULL 或 TEXT：用 ValueRef 判 NULL，`get::<_,Option<String>>` 会
            // 把空串与 NULL 混成一类，而"空串 project_hint"在契约侧是拒绝项。
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
        // 一次解析两处用：`owner.owner_principal_id` 要 id（原样搬运，**不过滤 `unknown`**——
        // 那一格落到归属列是既有行为，本格不改它），`a1_user` 的第一行要可读名。
        let principal = rc_ledger::transcript::resolve_principal(t, &session_key, &device_id)?;
        let owner_principal_id = principal.as_ref().map(|resolved| resolved.principal_id.clone());
        // 渲染那一格只认**已知**身份：`unknown` 与"没有 claim"在 prompt 里是同一句 `NONE`
        // （"'we don't know' 就是不知道"——两种来路，模型的处置一样）。
        // 有 id 没名字时退回 id：身份已知但叫不出名字，与身份未知不是一件事。
        let principal_name = principal.as_ref().and_then(|resolved| {
            let id = resolved.principal_id.trim();
            if !rc_memory::writers_v4::principal_is_known(id) {
                return None;
            }
            Some(resolved.principal_name.clone().unwrap_or_else(|| id.to_string()))
        });
        // ③ 会话内按源会话顺序（`created_at_ms`，同刻 `turn_uid`）扫可选中的轮。
        //    读取是有界的：凑够 limit 立刻 break，不把整会话拉进内存。
        //    转换器配套：`role='system'`（工具调用/工具结果）**只作上下文，永不作 TARGET**，
        //    在 SQL 这一层就挡掉；它们若紧邻目标轮，会由邻轮读取进 CTX（fact/event 有 CTX 段）。
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
                // 压缩摘要块是索引，不是命名来源（S2e-6）：选批当场落
                // `rejected(compact_summary)` 并跳过。`a1_user` 早就把它挡在渲染之外，
                // 但没有状态行 ⇒ 每一批都要再读它一次；这里把"它已处理"钉进 ledger。
                if is_compaction_body(&row.body) {
                    pass_state::mark_terminal(
                        l,
                        key(&row.turn_uid),
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
                        key(&row.turn_uid),
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
        // 转换器配套：批区间内被跳过的那几条 system 轮——读数 `system_turns_skipped`。
        let system_turns_skipped =
            count_system_turns_in_span(t, &session_key, &device_id, &rows)?;
        // scope 归一：契约五值里只有 person/entity 与记忆行同名，其余一律 project
        // （`owner::normalize_scope_kind` 是唯一实现点）。
        let scope = Scope { kind: owner::normalize_scope_kind(&scope_kind), key: scope_key };
        // C4a：候选池与 fact/event 共用同一个可见性谓词（`crate::candidates::load_candidates`）。
        // 这里必须**在移动 `owner_agent` 之前**取，池的 viewer 就是这一批的 owner。
        let candidates = naming_candidates(m, &owner_agent, &scope)?;
        return Ok(Some(NamingPlan {
            batch: Batch {
                session_key,
                device_id: device_id.clone(),
                source_agent: source_agent.clone(),
                project_hint,
                rows,
            },
            system_turns_skipped,
            ctx: BatchCtx { source_agent, device_id },
            // 归属：会话行的四格原文（visibility/retention 已在 SQL 侧 COALESCE 默认，
            // 这里只做字面量 → 枚举/归一，两个函数都是 `rc_memory::owner` 的同一个实现点）。
            owner: OwnerCtx {
                owner_principal_id,
                owner_agent_id: owner_agent,
                visibility: owner::parse_visibility(&visibility),
                retention: owner::normalize_retention(&retention),
            },
            // scope 归一：契约五值里只有 person/entity 与记忆行同名，其余一律 project
            // （`owner::normalize_scope_kind` 是唯一实现点）。
            scope,
            candidates,
            principal_name,
        }));
    }
    Ok(None)
}

/// 已有活锚候选（只作"别改名"的提示，不含 id）。**与 fact/event 同一个池**：
/// 归属/scope 过滤、`created_at DESC, id` 的顺序、上限 30 全部来自
/// [`crate::candidates::load_candidates`]；这里只把 `alias` 补上——命名要判
/// "这些表面是不是同一个物"，别名就是它的主料。
fn naming_candidates(
    m: &MemoryHandle,
    owner_agent_id: &str,
    scope: &Scope,
) -> Result<Vec<AnchorCandidate>, DistillError> {
    let pool = crate::candidates::load_candidates(m, owner_agent_id, scope)?;
    let mut out = Vec::with_capacity(pool.len());
    for candidate in pool {
        let aliases = crate::candidates::aliases_of(m, &candidate.anchor_id)?;
        out.push(AnchorCandidate { kind: candidate.kind, canonical: candidate.canonical, aliases });
    }
    Ok(out)
}

fn key(turn_uid: &str) -> PassKey<'_> {
    PassKey { turn_uid, pass_id: NAMING_PASS, family: "" }
}

/// I3：程序闸拒收的 draft（锚）的**证据轮**落 `rejected(<码>)`（只写本批选中的轮）。
/// 与 `pass_fact`/`pass_event` 的同名 helper 同一条口径：同批通过的锚照常走窄审/写库。
fn mark_program_rejected(
    l: &LedgerHandle,
    uids: &[String],
    turns: &[(String, &'static str)],
    now_ms: i64,
) -> Result<(), DistillError> {
    for (uid, code) in turns {
        if uids.iter().any(|batch_uid| batch_uid == uid) {
            pass_state::mark_terminal(l, key(uid), PassState::Rejected, Some(code), now_ms)?;
        }
    }
    Ok(())
}

/// 一个名字是不是**非实体噪声**（S2e-5，冒烟发现 2）。命中任一即 true：
/// - trim 后为空；
/// - 整段被反引号包住（首尾都是 `` ` ``）：代码令牌不是名字；
/// - 去掉尾部 `…` / `...` 后是 ≥7 位 ASCII hex（大小写不敏感）：commit 哈希片段；
/// - 大小写不敏感地等于 `HEAD` / `ORIGIN`：版本指针；
/// - 含 `://`、以 `/` 或 `\` 开头、含 `:\` 或 `:/`：路径与 URL；
/// - 所有字符都是标点/空白/数字（没有字母或汉字）。
///
/// 判据只用这些**通用形状**：不认任何私有语料里的名字，也不靠"长得不像中文"这类
/// 语料直觉——`批次 M`、`阶段 4`、`某打印机` 都必须留下。
pub fn is_noise_name(name: &str) -> bool {
    let text = name.trim();
    if text.is_empty() {
        return true;
    }
    if text.len() >= 2 && text.starts_with('`') && text.ends_with('`') {
        return true;
    }
    // 模型会把哈希截断成 `99c05800…` 或 `99c05800...`；两种省略号都先剥掉再判 hex。
    let body = text
        .strip_suffix('…')
        .or_else(|| text.strip_suffix("..."))
        .unwrap_or(text)
        .trim_end();
    if body.len() >= 7 && body.chars().all(|c| c.is_ascii_hexdigit()) {
        return true;
    }
    if text.eq_ignore_ascii_case("HEAD") || text.eq_ignore_ascii_case("ORIGIN") {
        return true;
    }
    if text.contains("://")
        || text.starts_with('/')
        || text.starts_with('\\')
        || text.contains(":\\")
        || text.contains(":/")
    {
        return true;
    }
    !text.chars().any(char::is_alphabetic)
}

/// 后置闸：把 [`is_noise_name`] 命中的锚从一批 A1 计划里剔掉，返回被丢的锚数（S2e-5）。
///
/// 模型输出照收（它已经过了契约），这里只做程序侧的最后一道筛：
/// - canonical 噪声 ⇒ 丢该锚，`filtered += 1`；
/// - canonical 保留 ⇒ 别名里的噪声逐条剔除（别名没有独立生命周期，不为它丢锚）；
/// - 引用被丢 ref 的 mention 一起丢（它的 `anchor_ref` 已经没有落点）；
/// - 被丢 ref 当 `speaker_ref` 的 mention 留下，但转述者置空（丢掉的锚不能继续当主语）；
/// - issue 的 refs 过滤后为空（且原本引用过被丢 ref）⇒ 丢这条 issue，否则留剩下的 refs；
/// - `unresolved` 没有 anchor_ref，不在这个闸的处理面上。
///
/// 丢到零锚是合法的：`run_naming` 会把它记成 `empty`，不是失败。
pub fn drop_noise_anchors(batch: &mut A1Batch) -> usize {
    let mut dropped: BTreeSet<String> = BTreeSet::new();
    let mut kept: Vec<A1Anchor> = Vec::with_capacity(batch.anchors.len());
    let mut filtered = 0;
    for mut anchor in std::mem::take(&mut batch.anchors) {
        if is_noise_name(&anchor.canonical) {
            dropped.insert(anchor.reference.clone());
            filtered += 1;
            continue;
        }
        anchor.aliases.retain(|alias| !is_noise_name(alias));
        kept.push(anchor);
    }
    batch.anchors = kept;
    if filtered == 0 {
        return 0;
    }

    let mut mentions = Vec::with_capacity(batch.mentions.len());
    for mut m in std::mem::take(&mut batch.mentions) {
        if dropped.contains(&m.anchor_ref) {
            continue;
        }
        if m.speaker_ref.as_deref().is_some_and(|s| dropped.contains(s)) {
            m.speaker_ref = None;
        }
        mentions.push(m);
    }
    batch.mentions = mentions;

    let mut issues = Vec::with_capacity(batch.issues.len());
    for mut issue in std::mem::take(&mut batch.issues) {
        let referenced_dropped = issue.refs.iter().any(|r| dropped.contains(r));
        issue.refs.retain(|r| !dropped.contains(r));
        if referenced_dropped && issue.refs.is_empty() {
            continue;
        }
        issues.push(issue);
    }
    batch.issues = issues;
    filtered
}

/// 后继过滤（C4b）：非 speaker 的提及必须落在**它自己那条轮**的正文里，返回被丢的提及数。
///
/// 判据（中心实测：353 条提及上照字面做会杀 36 条，其中 24 条是 `speaker` 自称）：
/// - `discourse_role == "speaker"` ⇒ **一律放过**：自称不是文本 span（`agent-smoke` 这类名字
///   在全批任何一轮都没有字面），拿逐字判它只会把真名字误杀；
/// - 其余提及 ⇒ `surface` 必须是对应 `turn_uid` 正文的子串（正文与 surface 都先过
///   [`prepare`]，与提示词/闸同一个输入侧口径）；找不到那一轮（不在批内）同判不接地；
/// - 不成立 ⇒ **只丢这一条提及**，锚与其它提及不动（下游按锚审，丢一条提及不影响锚的判定）。
///
/// `bodies` 是 `turn_uid → 正文`（正文已过 [`evidence_view`]：`prepare` 再抹行内标记）。
/// 这条过滤器**不参与成败判定**：
/// 丢完一条不剩也不是失败，与 [`drop_noise_anchors`] 同一条纪律。
pub fn drop_ungrounded_mentions(batch: &mut A1Batch, bodies: &HashMap<String, String>) -> usize {
    let mut dropped = 0;
    let mut kept: Vec<A1Mention> = Vec::with_capacity(batch.mentions.len());
    for mention in std::mem::take(&mut batch.mentions) {
        if mention.discourse_role == "speaker" {
            kept.push(mention);
            continue;
        }
        let grounded = bodies
            .get(&mention.turn_uid)
            .is_some_and(|body| body.contains(&evidence_view(&mention.surface)));
        if grounded {
            kept.push(mention);
        } else {
            dropped += 1;
        }
    }
    batch.mentions = kept;
    dropped
}

/// 过滤后把引用重编成位置号 `a1..aN`（S3b-3c）。
///
/// `plan_a1` 是**先编号后过滤**：噪声闸丢掉一个锚会留下空号（例如只剩 `a2`）。而引擎的
/// 重放（`review::replay::naming_batch`）与审核 prompt 的 `ANCHOR a{n}` 都按位置读 ref——
/// 空号会被判成载荷不可重放（`PAYLOAD_GAP`），把一条本可写库的批降成隔离。重编号只换
/// 引用标签：anchors 的顺序、mentions 的指向、issues 的 refs 与 unresolved 都不动。
fn renumber_refs(a1: &mut A1Batch) {
    let mut map: HashMap<String, String> = HashMap::with_capacity(a1.anchors.len());
    for (index, anchor) in a1.anchors.iter_mut().enumerate() {
        let next = format!("a{}", index + 1);
        if anchor.reference != next {
            map.insert(anchor.reference.clone(), next.clone());
        }
        anchor.reference = next;
    }
    if map.is_empty() {
        return;
    }
    for mention in &mut a1.mentions {
        if let Some(next) = map.get(&mention.anchor_ref) {
            mention.anchor_ref = next.clone();
        }
        let speaker = mention.speaker_ref.clone();
        if let Some(next) = speaker.as_ref().and_then(|s| map.get(s)) {
            mention.speaker_ref = Some(next.clone());
        }
    }
    for issue in &mut a1.issues {
        for reference in &mut issue.refs {
            if let Some(next) = map.get(reference) {
                *reference = next.clone();
            }
        }
    }
}

/// 把一批 A1 计划折成 `review.pass.v1` 的 `items`：字段与 `gate::parse_naming` 逐字一致。
///
/// `A1Mention` 没有 span（契约只给 surface/turn_uid/speaker_ref/discourse_role/resolution/
/// confidence），载荷里也就不写 `span_start`/`span_end`——不编一个模型没给过的东西。
///
/// S3b-3b：`items` 是**四键**`{anchors, mentions, issues, unresolved}`。闸只读前两键，
/// 后两键是写给重放与引擎的：`mentions[].anchor_ref` 是写库层查锚的唯一键（缺它，
/// `write_a1_with_owner` 必炸），`issues`/`unresolved` 按 `A1Batch` 的顺序直录，
/// 让"过滤到 pass 集合"发生在重放之后、而不是在载荷里预先丢信息。
fn review_items(a1: &A1Batch) -> Value {
    let anchors: Vec<Value> = a1
        .anchors
        .iter()
        .map(|anchor| {
            let evidence: Vec<Value> = anchor
                .evidence
                .iter()
                .map(|e| json!({"turn_uid": e.turn_uid, "anchor_role": e.anchor_role.as_str()}))
                .collect();
            json!({
                "reference": anchor.reference,
                "kind": anchor.kind,
                "canonical": anchor.canonical,
                "aliases": anchor.aliases,
                "reality_status": anchor.reality_status,
                "evidence": evidence,
                "stable": anchor.stable,
            })
        })
        .collect();
    let mentions: Vec<Value> = a1
        .mentions
        .iter()
        .map(|m| {
            json!({
                "anchor_ref": m.anchor_ref,
                "surface": m.surface,
                "turn_uid": m.turn_uid,
                "speaker_ref": m.speaker_ref,
                "discourse_role": m.discourse_role,
                "resolution": m.resolution,
                "confidence": m.confidence,
            })
        })
        .collect();
    let issues: Vec<Value> = a1
        .issues
        .iter()
        .map(|issue| json!({"kind": issue.kind, "refs": issue.refs, "reason": issue.reason}))
        .collect();
    let unresolved: Vec<Value> = a1
        .unresolved
        .iter()
        .map(|u| json!({"surface": u.surface, "turn_uid": u.turn_uid, "reason": u.reason}))
        .collect();
    json!({
        "anchors": anchors,
        "mentions": mentions,
        "issues": issues,
        "unresolved": unresolved,
    })
}

/// 跑一批 naming：先给批内每轮落 `running`（同指纹、同 run_id），再双跑 A1，
/// 合成计划 → 噪声闸 → **待审队列（入队 + 程序闸）** → 委托 [`review::run::run_item`]
/// 完成窄审、判定与写库，最后把引擎读数折成每轮同一个终态。
///
/// S3a-3b：A1 双跑与过滤的代码和 prompt 一字不动，只在"合成计划之后、写库之前"插入队列：
/// - 零产出（`anchors + mentions == 0`）不入队，直接落 `empty`（合法空批）；
/// - 闸 fail ⇒ 记 `program reject`（reason=码），**不写 A1**，逐轮 `rejected(<code>)`；
/// - 闸 pass ⇒ 只记 `program pass`，窄审由引擎开口（S3b-3c：naming 的独立窄审
///   `pass-naming-review-v0.2`）；引擎也只写通过审核的锚。
///
/// S3b-3c 的失败面（引擎错误 → 逐轮状态，一张表）：
/// - `Llm(Timeout|SchemaMismatch)` ⇒ 每轮记一次失败；试满上限的轮隔离，整批隔离 ⇒
///   `Ok(Quarantined)`，否则 `Ok(Pending)`（W19c2：单元级失败不算这一族的失败）；
/// - `Llm(BackendError|QuotaExhausted)` ⇒ 每轮 `pending`（60s 退避）、返回 `DistillError::Llm`
///   （座位级硬失败，交给日路径的连失败闸，不隔离）；
/// - `ReviewNaming(slug)` ⇒ 每轮 `quarantined(<slug>)`、返回 `DistillError::NamingReview`；
/// - `Store(e)` ⇒ 整批轮 `pending(review_pending)`（H1b：不写 `rejected(store)`）、返回原错误；
/// - `Conflict` ⇒ 不改任何 ledger 行、`Ok(Pending)` 且读数全 0（另一写者收尾）；
/// - `Unsupported`/`MissingItem`（以及 naming 走不到的 `Review`）⇒ 不改 ledger、返回一句静态
///   `Memory::Sql`——状态机不归调用方猜。
///
/// 成功面：引擎说 Applied ⇒ 每轮 `Done`（读数取引擎的 `written`/`mentions`；Applied 重放的
/// 读数与旧早退一致，都是 0）；Rejected ⇒ `rejected(review_reject)`；Quarantined ⇒
/// `quarantined(review_uncertain)`；判决不可重建（fail-closed）⇒ `quarantined(<隔离码>)`
/// 并全 0 读数——绝不当"全 pass"。
///
/// 双跑合成之后、落库之前过一道 **[`drop_noise_anchors`]**（S2e-5）：噪声锚被剔掉，
/// 计数进 [`NamingOutcome::filtered`]。这道闸不参与成败判定——过滤之后的零锚仍是 `empty`，
/// 且不入待审队列。
///
/// C4b：紧跟噪声闸再过一道 **[`drop_ungrounded_mentions`]**——非 speaker 提及的 surface 必须
/// 落在它自己那条轮里；不接地的只丢那条提及，锚照旧，计数进
/// [`NamingOutcome::ungrounded_mentions`]。同一条纪律：丢提及不参与成败判定（speaker 自称豁免）。
///
/// S3b-4b：这条签名 = [`run_naming_with`] + [`ReviewMode::Inline`]，既有调用与测试一个字都不用改。
pub fn run_naming(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    l: &LedgerHandle,
    plan: &NamingPlan,
    backend: &dyn LlmBackend,
    now_ms: i64,
) -> Result<NamingOutcome, DistillError> {
    run_naming_with(m, t, l, plan, backend, now_ms, ReviewMode::Inline)
}

/// 带审核模式的同一条 naming pass（S3b-4b）。
///
/// `mode=EnqueueOnly` 只改**一支**：A1 非空、程序闸 pass 且 item 是 `pending_review` 的那一批
/// 不再问审核模型、不写锚——批内每一轮落 [`pass_state::mark_awaiting_review`]
/// （`pending(review_pending)`），读数 `anchors=0`/`mentions=0`（`filtered` 照实），
/// `state=Pending`。合法空批（`anchors+mentions` 为空，含全被噪声闸剔掉）仍走既有的
/// `empty` 路径：不入队、不标 pending，与审核无关。
/// `Approved`/`Applied` 仍交给引擎（那两支不问模型），`Rejected`/`Quarantined`/`Stale`
/// 的早退不变。
#[allow(clippy::too_many_arguments)]
pub fn run_naming_with(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    l: &LedgerHandle,
    plan: &NamingPlan,
    backend: &dyn LlmBackend,
    now_ms: i64,
    mode: ReviewMode,
) -> Result<NamingOutcome, DistillError> {
    let uids: Vec<String> = plan.batch.uids().into_iter().map(str::to_string).collect();
    // 空批不该从 `select_naming_batch` 出来（它按"有缺口"才返回）；直接调用方给空批也
    // 不该落一条没有轮次的 running。零行、零轮、`empty`——不调模型。
    if uids.is_empty() {
        return Ok(NamingOutcome {
            turns: 0,
            anchors: 0,
            mentions: 0,
            filtered: 0,
            ungrounded_mentions: 0,
            // 空批不会从选批出来；直接调用方给的空批没有"批内 system 轮"可言。
            system_turns_skipped: plan.system_turns_skipped,
            retry_exhausted: false,
            state: PassState::Empty,
        });
    }

    // 指纹只由程序算：pipeline 版本 + 当前座位 slug + A1 的 system/user 渲染。
    // 会话正文进了 user，所以"同一批文本换模型/换 prompt"必然换指纹。
    let system = a1_system();
    let fingerprint = water::extraction_fingerprint(
        NAMING_PIPELINE,
        backend.slug().unwrap_or("unknown"),
        &system,
        &a1_user(&plan.batch, &plan.candidates, plan.principal_name.as_deref()),
    );
    // P0-2：版本号只喂 system——改判据（system prompt）之后旧轮才回头，批组成变化不算。
    let prompt_version = pass_state::prompt_version(&system);
    let run_id = format!("naming-{now_ms}");
    for uid in &uids {
        pass_state::mark_running_with_prompt_version(
            l, key(uid), &fingerprint, Some(&prompt_version), &run_id, now_ms)?;
    }

    let pair = match run_a1_pair(backend, &plan.batch, &plan.candidates, plan.principal_name.as_deref())
    {
        Ok(pair) => pair,
        Err(DistillError::Llm(failure)) => {
            // 故障矩阵的"运维失败"那一支：轮是 pending，不是 rejected。退避之后可再选。
            // 座位级（传输 / 额度）坏的是座位，不是这一批数据 ⇒ 沿旧口径记一次失败后 `Err`，
            // 让日路径的连失败闸收手；**不隔离**（W19c2）。
            if !crate::pass_state::unit_level(failure) {
                for uid in &uids {
                    pass_state::note_attempt(l, key(uid), RETRY_BACKOFF_MS, now_ms)?;
                }
                return Err(DistillError::Llm(failure));
            }
            // 单元级（超时 / 答复不合格）：逐格记账，试满 `MAX_PASS_ATTEMPTS` 次的那一格不再重试。
            // 整批都隔离就把这一批按**成功**交回（`state=quarantined`、落库计数全 0）——
            // 卡死的单元因此不会让日路径整族停摆；只有一部分到上限时交回 `state=pending`：
            // 到上限的已在退避里、下一次选批会跳过它，整族继续跑（W19c2）。
            let mut all_quarantined = true;
            for uid in &uids {
                all_quarantined &= pass_state::note_attempt_or_quarantine(
                    l, key(uid), RETRY_BACKOFF_MS, now_ms)? == PassState::Quarantined;
            }
            if all_quarantined {
                return Ok(NamingOutcome {
                    turns: uids.len(),
                    anchors: 0,
                    mentions: 0,
                    filtered: 0,
                    ungrounded_mentions: 0,
                    system_turns_skipped: plan.system_turns_skipped,
                    // 整批都到了上限 ⇒ 这是"重试耗尽"那一支：只有这一格能让日路径的
                    // 连续隔离墙推进（W19c2）。
                    retry_exhausted: true,
                    state: PassState::Quarantined,
                });
            }
            return Ok(NamingOutcome {
                turns: uids.len(),
                anchors: 0,
                mentions: 0,
                filtered: 0,
                ungrounded_mentions: 0,
                system_turns_skipped: plan.system_turns_skipped,
                retry_exhausted: false,
                state: PassState::Pending,
            });
        }
        Err(e) => {
            // 契约 / A1：模型给了不是 A1 的东西。`reason_slug` 只对契约族给 `schema`，
            // 其余（Memory/Store/Judge/Audit）今天在这里走不到；兜底仍写 `schema`
            // ——`mark_terminal` 不收空 reason，而"`rejected` 没理由"是读数上的谎。
            let reason = e.reason_slug().unwrap_or("schema");
            for uid in &uids {
                pass_state::mark_terminal(l, key(uid), PassState::Rejected, Some(reason), now_ms)?;
            }
            return Err(e);
        }
    };

    // 后置闸：模型照收，噪声锚（哈希/HEAD/反引号令牌/路径/URL/纯符号）在这里被剔掉。
    // 过滤不让整批失败：它只少落几行锚，读数里记一笔 `filtered`。
    let mut a1 = plan_a1(&pair.0, &pair.1, &plan.batch);
    let filtered = drop_noise_anchors(&mut a1);
    // C4b：非 speaker 的提及必须逐字落在**它自己那条轮**里；不接地的只丢那条提及。
    // 正文与提示词/闸同一条输入侧口径（`prepare`），批内轮就是模型可见的那几轮。
    // 比对面再抹行内标记（`evidence_view`）：模型抄 surface 时同样会剥 `**`/反引号。
    let bodies: HashMap<String, String> = plan
        .batch
        .rows
        .iter()
        .map(|row| (row.turn_uid.clone(), evidence_view(&row.body)))
        .collect();
    let ungrounded_mentions = drop_ungrounded_mentions(&mut a1, &bodies);
    // 过滤会留下空号；引擎按位置读 ref，重编号把载荷恢复成可重放的 `a1..aN`。
    renumber_refs(&mut a1);

    // ---- 待审队列：入队 + 程序闸（零产出不入队）---------------------------------
    // 锚与提及都为空 = 合法空批（含"全被噪声闸剔掉"）：不入队、不调闸，直接落 `empty`。
    // 闸 pass 之后的一切（窄审、判定、写库）由引擎一次做完（S3b-3c）。
    let mut item_id: Option<i64> = None;
    // H1b：这一格是否要占位。`pending_review`/`approved` 会写库，必须先抢到租约；
    // `applied` 的引擎路径只重读台账（`run_applied` 不写库），状态闭集也不收它，不占位。
    let mut claimable = false;
    // I3：程序闸拒收的锚（draft）的证据轮 `(turn_uid, 码)`——逐轮 rejected(<码>)。
    let mut program_turns: Vec<(String, &'static str)> = Vec::new();
    if !(a1.anchors.is_empty() && a1.mentions.is_empty()) {
        // 载荷必须可重放：`run_id` 用本批指纹（与 fact 先例同律），不拿墙上钟——否则同一批
        // 每次都换 sha，`review_item_payload` 的唯一键永远命不中，第二个 item 就长出来了。
        let payload = review::pass_payload(
            NAMING_PASS,
            "",
            &fingerprint,
            &plan.batch,
            &plan.ctx,
            &plan.owner,
            &plan.scope,
            review_items(&a1),
            None,
        );
        let owner_json = review::owner_json(&plan.owner, &plan.scope);
        let queued_item = review::new_item(NAMING_PASS, "", &plan.batch, owner_json, &payload);
        let queued = enqueue(m, &queued_item, now_ms)?;
        let item = get(m, queued.id)?.ok_or_else(|| {
            rc_memory::MemError::Sql("review: enqueued item vanished".to_string())
        })?;
        match queued.state {
            ReviewState::PendingReview => {
                // I3：先算一次逐 draft（锚）的闸。`program_decided` 只挡重复记账，不挡读闸。
                let report = review::gate_report(m, t, &item, now_ms)?;
                program_turns = report.rejected_turns();
                if !review::program_decided(m, queued.id)? {
                    if report.batch_code.is_some() || report.all_failed() {
                        // 批级前置坏 / **全部**锚都坏：没有好锚可保，与今天一致整批 rejected。
                        // 返回 Ok 而不是 Err：闸是确定性的，重试只会得到同一个码。
                        let code = report.code.unwrap_or("payload_shape");
                        review::record_program(
                            m,
                            queued.id,
                            QueueVerdict::Reject,
                            code,
                            &item.payload_sha256,
                            now_ms,
                        )?;
                        for uid in &uids {
                            pass_state::mark_terminal(
                                l,
                                key(uid),
                                PassState::Rejected,
                                Some(code),
                                now_ms,
                            )?;
                        }
                        return Ok(NamingOutcome {
                            turns: uids.len(),
                            anchors: 0,
                            mentions: 0,
                            filtered,
                            ungrounded_mentions,
                            system_turns_skipped: plan.system_turns_skipped,
                            retry_exhausted: false,
                            state: PassState::Rejected,
                        });
                    }
                    // 混合批：item 记 program pass（好锚还要进窄审/写库）。
                    review::record_program(
                        m,
                        queued.id,
                        QueueVerdict::Pass,
                        review::PROGRAM_PASS,
                        &item.payload_sha256,
                        now_ms,
                    )?;
                }
                // 坏锚的证据轮：程序驳回是确定性终态，当场落；后续逐轮映射不覆盖它们。
                mark_program_rejected(l, &uids, &program_turns, now_ms)?;
                // S3b-4b：`--no-review` 只跳过"pending 的窄审"。A1 非空、闸已 pass、item 仍是
                // `pending_review` ⇒ 整批轮等审核（naming 的窄审按锚裁，锚的证据可能落在批内
                // 任何一轮，所以不做 fact/event 那样的"有草稿/没草稿"分流）。
                if mode == ReviewMode::EnqueueOnly {
                    for uid in &uids {
                        pass_state::mark_awaiting_review(l, key(uid), now_ms)?;
                    }
                    // 等审核的映射会把坏锚的轮写成 pending；程序驳回不因"等补审"复活。
                    mark_program_rejected(l, &uids, &program_turns, now_ms)?;
                    return Ok(NamingOutcome {
                        turns: uids.len(),
                        anchors: 0,
                        mentions: 0,
                        filtered,
                        ungrounded_mentions,
                        system_turns_skipped: plan.system_turns_skipped,
                        retry_exhausted: false,
                        state: PassState::Pending,
                    });
                }
                item_id = Some(queued.id);
                claimable = true;
            }
            // 上次写库失败 / 已写库：都交给引擎按 item 状态决定"重试写"或"只读重放"。
            // 已写库那格在引擎里是零读数的 Applied 早退（与旧本模块的早退同格）。
            ReviewState::Approved | ReviewState::Applied => {
                item_id = Some(queued.id);
                // H1b：只有 approved 会写库；applied 是只读重放，不占位（见 `claimable`）。
                claimable = queued.state == ReviewState::Approved;
            }
            ReviewState::Rejected => {
                // 终态：不写库，逐轮按既有映射收尾（程序码优先，其次审核驳回码）。
                let code = item
                    .meta
                    .program_code
                    .clone()
                    .unwrap_or_else(|| review::REVIEW_REJECT.to_string());
                for uid in &uids {
                    pass_state::mark_terminal(
                        l,
                        key(uid),
                        PassState::Rejected,
                        Some(&code),
                        now_ms,
                    )?;
                }
                return Ok(NamingOutcome {
                    turns: uids.len(),
                    anchors: 0,
                    mentions: 0,
                    filtered,
                    ungrounded_mentions,
                    system_turns_skipped: plan.system_turns_skipped,
                    retry_exhausted: false,
                    state: PassState::Rejected,
                });
            }
            ReviewState::Quarantined => {
                for uid in &uids {
                    pass_state::mark_terminal(
                        l,
                        key(uid),
                        PassState::Quarantined,
                        Some(review::REVIEW_UNCERTAIN),
                        now_ms,
                    )?;
                }
                return Ok(NamingOutcome {
                    turns: uids.len(),
                    anchors: 0,
                    mentions: 0,
                    filtered,
                    ungrounded_mentions,
                    system_turns_skipped: plan.system_turns_skipped,
                    retry_exhausted: false,
                    state: PassState::Quarantined,
                });
            }
            ReviewState::Stale => {
                for uid in &uids {
                    pass_state::mark_terminal(
                        l,
                        key(uid),
                        PassState::Rejected,
                        Some(STALE_REASON),
                        now_ms,
                    )?;
                }
                return Ok(NamingOutcome {
                    turns: uids.len(),
                    anchors: 0,
                    mentions: 0,
                    filtered,
                    ungrounded_mentions,
                    system_turns_skipped: plan.system_turns_skipped,
                    retry_exhausted: false,
                    state: PassState::Rejected,
                });
            }
        }
    }

    // ---- 引擎：窄审 + 判定 + 只写 pass 锚（S3b-3c）------------------------------
    let Some(item_id) = item_id else {
        // 零产出（含全被噪声闸剔掉）：没有 item 可跑，也没有行可写。合法空批 ⇒ 逐轮 `empty`。
        for uid in &uids {
            pass_state::mark_terminal(l, key(uid), PassState::Empty, None, now_ms)?;
        }
        return Ok(NamingOutcome {
            turns: uids.len(),
            anchors: 0,
            mentions: 0,
            filtered,
            ungrounded_mentions,
            system_turns_skipped: plan.system_turns_skipped,
            retry_exhausted: false,
            state: PassState::Empty,
        });
    };

    // ---- 占位：入队与程序闸都完成后、调引擎之前（H1b）---------------------------
    // `false`（另一写者持有 / 退避未到）不碰引擎：naming 的窄审按锚裁、锚的证据可能落在
    // 批内任何一轮，所以**整批**都收回 `pending(review_pending)`；0 锚、`filtered` 照实，
    // `state=Pending`。已有 `applied` 的 item 走只读重放（`claimable=false`）。
    let mut lease_owner: Option<String> = None;
    if claimable {
        let owner = format!("distill-{}-{now_ms}", std::process::id());
        if claim(m, item_id, &owner, review::REVIEW_LEASE_MS, now_ms)? {
            lease_owner = Some(owner);
        } else {
            for uid in &uids {
                pass_state::mark_awaiting_review(l, key(uid), now_ms)?;
            }
            // I3：程序驳回的轮不因"另一写者持有"回到 pending。
            mark_program_rejected(l, &uids, &program_turns, now_ms)?;
            return Ok(NamingOutcome {
                turns: uids.len(),
                anchors: 0,
                mentions: 0,
                filtered,
                ungrounded_mentions,
                system_turns_skipped: plan.system_turns_skipped,
                retry_exhausted: false,
                state: PassState::Pending,
            });
        }
    }

    // 抢到的租约无论成败都释放（`let _ =`：释放失败不许盖住真正的错误）。
    let result = review::run::run_item(m, t, l, item_id, backend, now_ms);
    if let Some(owner) = lease_owner.as_deref() {
        let _ = release(m, item_id, owner);
    }
    let run = match result {
        Ok(run) => run,
        Err(review::run::RunError::Llm(failure)) => {
            // 审核开不了口：不记判定、不写库，轮回到 pending 等退避（与 A1 那一支同一条政策）。
            // 座位级（传输 / 额度）照旧 `note_attempt` + `Err`，不隔离。
            if !crate::pass_state::unit_level(failure) {
                for uid in &uids {
                    pass_state::note_attempt(l, key(uid), RETRY_BACKOFF_MS, now_ms)?;
                }
                return Err(DistillError::Llm(failure));
            }
            // 单元级：试满上限的那一格隔离、整批都隔离了按成功交回；否则按 pending 交回，
            // 整族继续（W19c2）。
            let mut all_quarantined = true;
            for uid in &uids {
                all_quarantined &= pass_state::note_attempt_or_quarantine(
                    l, key(uid), RETRY_BACKOFF_MS, now_ms)? == PassState::Quarantined;
            }
            if all_quarantined {
                return Ok(NamingOutcome {
                    turns: uids.len(),
                    anchors: 0,
                    mentions: 0,
                    filtered: 0,
                    ungrounded_mentions: 0,
                    system_turns_skipped: plan.system_turns_skipped,
                    // 整批都到了上限 ⇒ 这是"重试耗尽"那一支：只有这一格能让日路径的
                    // 连续隔离墙推进（W19c2）。
                    retry_exhausted: true,
                    state: PassState::Quarantined,
                });
            }
            return Ok(NamingOutcome {
                turns: uids.len(),
                anchors: 0,
                mentions: 0,
                filtered: 0,
                ungrounded_mentions: 0,
                system_turns_skipped: plan.system_turns_skipped,
                retry_exhausted: false,
                state: PassState::Pending,
            });
        }
        Err(review::run::RunError::ReviewNaming(slug)) => {
            // 审核给了不是判决的东西：引擎已记 `uncertain(<slug>)` 并落 artifact。
            for uid in &uids {
                pass_state::mark_terminal(
                    l,
                    key(uid),
                    PassState::Quarantined,
                    Some(&slug),
                    now_ms,
                )?;
            }
            return Err(DistillError::NamingReview(slug));
        }
        Err(review::run::RunError::Store(e)) => {
            // 存储/事务失败：item 留在原状态（引擎不记 apply）。整批轮收回
            // `pending(review_pending)`，让下一次补审能接着跑；**不写 rejected(store)**。
            for uid in &uids {
                pass_state::mark_awaiting_review(l, key(uid), now_ms)?;
            }
            // I3：程序驳回与"写库失败"无关——坏锚的轮照旧 rejected(<码>)。
            mark_program_rejected(l, &uids, &program_turns, now_ms)?;
            return Err(e);
        }
        Err(review::run::RunError::Conflict) => {
            // 另一写者已推进状态（或非法转换），本次什么都没做：**不改任何 ledger 行**。
            // 对 runner 而言这不是失败 ⇒ Ok(Pending) 且读数全 0。
            return Ok(NamingOutcome {
                turns: 0,
                anchors: 0,
                mentions: 0,
                filtered: 0,
                ungrounded_mentions: 0,
                // 另一写者已推进状态：本进程什么都没做，读数全 0（选批层面那格也归 0）。
                system_turns_skipped: 0,
                retry_exhausted: false,
                state: PassState::Pending,
            });
        }
        // `Unsupported`/`MissingItem` 是"状态不对"（同一 pass 的分派与重放读的是同一个
        // `pass_id`，正常走不到）；`Review` 只由 fact/event 的 parser 产生，naming item 走不到。
        // 两种都是接线错误：不猜终态、不动 ledger，只给一句静态话。
        Err(
            review::run::RunError::Unsupported
            | review::run::RunError::MissingItem
            | review::run::RunError::Review(_)
            | review::run::RunError::ReviewRows(_)
            // 波三刀 A：lesson 的审核 parser 只由 lesson item 产生，naming 走不到 —— 接线错误。
            | review::run::RunError::ReviewLesson(_),
        ) => {
            return Err(DistillError::Memory(MemError::Sql(
                "review run: unexpected state".into(),
            )));
        }
    };

    // 引擎读数 → 逐轮终态。`verdicts=None` 是 fail-closed：判决重建不出，绝不按"全 pass"。
    let (anchors, mentions, state, reason) = match run.verdicts {
        None if run.item_state == ReviewState::Applied => {
            // Applied 早退：读数与旧 `run_naming` 逐格一致（写库那次已经报过数）。
            (0, 0, PassState::Done, None)
        }
        None => {
            let reason = run.fail_closed_reason.unwrap_or(review::REVIEW_VERDICTS_MISSING);
            (0, 0, PassState::Quarantined, Some(reason.to_string()))
        }
        Some(_) => match run.item_state {
            // 即使 anchors+mentions 为 0 也是 Done：与旧 Applied 早退一致。
            ReviewState::Applied => (run.written, run.mentions, PassState::Done, None),
            ReviewState::Rejected => (
                0,
                0,
                PassState::Rejected,
                Some(review::REVIEW_REJECT.to_string()),
            ),
            ReviewState::Quarantined => (
                0,
                0,
                PassState::Quarantined,
                Some(review::REVIEW_UNCERTAIN.to_string()),
            ),
            // 其余状态不该出现在引擎的成功返回里；按存储面隔离，不猜 Done。
            _ => (
                0,
                0,
                PassState::Quarantined,
                Some(STORE_FAILURE_REASON.to_string()),
            ),
        },
    };

    // I3：程序闸拒收的锚的证据轮落 `rejected(<码>)`，不被"整批同格"覆盖回 Done。
    let program_turns = if run.program_rejected_turns.is_empty() {
        program_turns
    } else {
        run.program_rejected_turns.clone()
    };
    for uid in &uids {
        if program_turns.iter().any(|(turn, _)| turn == uid) {
            continue;
        }
        pass_state::mark_terminal(l, key(uid), state, reason.as_deref(), now_ms)?;
    }
    mark_program_rejected(l, &uids, &program_turns, now_ms)?;
    Ok(NamingOutcome {
        turns: uids.len(),
        anchors,
        mentions,
        filtered,
        ungrounded_mentions,
        system_turns_skipped: plan.system_turns_skipped,
        retry_exhausted: false,
        state,
    })
}
