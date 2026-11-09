//! S3b-2a：`review run` 的**引擎**——对一条已入队的 item，只凭载荷 + 转录只读查询，
//! 完成窄审调用、判定记账与写库。
//!
//! 设计 `2026-10-26-review-queue-s3` §4/§5/§6/§8 的"审核外置"落地：S3b-1 已让
//! fact/event 的载荷能逐字节重放审核输入（[`crate::review::replay`]），本模块把
//! "重放 → 复核 → 判定 → 写库"连成一次可独立调用的作业，供 `hub review run`（S3b-4）使用。
//!
//! runner 已委托（S3b-2b）：`pass_fact::run_fact` / `pass_event::run_event` 都把窄审整段交给
//! `run_item`，`tests/review_run.rs` 也直接驱动它。
//!
//! S3b-3b：naming 也走这条引擎（`pass_id=naming`、family 恒空串），与 fact/event 共用
//! "重放 → 复核 → 判定 → 写库"的骨架，差别只在**过滤**与**读数口径**：
//!
//! - 审核输入 = `naming_review_system()` + `naming_review_user(&a1, &batch)`，指纹仍走
//!   `extraction_fingerprint(NAMING_PIPELINE, model, system, user)`；refs 是 anchors 的
//!   `a1..aN`（位置编号，与 prompt 里的 `ANCHOR a{n}` 同一套）。
//! - **只有聚合 Pass 才写库**，写之前按"通过的锚"过滤整批（naming 独有）：
//!   1. anchors 只留 verdict=pass 的；
//!   2. mentions 只留 `anchor_ref` 在 pass 集合里的；`speaker_ref` 指向未通过锚时置空
//!      （与 `pass_naming::drop_noise_anchors` 同一条：丢掉的锚不能继续当主语，
//!      否则写库层的 `by_ref[speaker]` 会整批失败）；
//!   3. issues 的 `refs` 先过滤到 pass 集合，过滤后为空 ⇒ 整条丢弃；
//!   4. unresolved 全留（它是轮级的，不指向锚）。
//! - 读数沿 naming 的口径：`written`/`facts` = 落库锚数（`NamingOutcome.anchors`
//!   同格），`cards` 恒 0（naming 不产卡），`mentions` = 新插入的提及行数。
//! - 审核解析失败没有 `DistillError` 契约变体可装（naming 的 parser 是 S3b-3a 的
//!   `NamingReviewError`）：返回 [`RunError::ReviewNaming`]，装**已带前缀**的隔离码
//!   `review_<slug>`（H6：与 fact/event 同前缀，runner/hub 原样沿用、不再自行拼）。
//!
//! 纪律（与 runner 的第二次调用逐条同口径）：
//! - **审核输入逐字节一致**：system/user/`input_sha256`/`prompt_version` 全部走 runner
//!   同一组函数与同一个指纹式（`water::extraction_fingerprint`），不另起一套渲染。
//! - **LLM 故障不记判定、不写库、不落 artifact**：item 留在原状态等下一次。
//! - **审核解析失败先落 artifact 再记 `uncertain(<reason>)`**：调用方据此隔离。
//! - **gap/fail-closed 不是"全 pass"**：载荷不可重放、判定重建不出，都返回 `verdicts=None`
//!   与一个隔离码，调用方按隔离处理。**approved 上的这两种情形（H2）还会记一条
//!   `program/uncertain` 把 item 推到 `quarantined`**——静默留在 approved 会让剩余计数与
//!   退出码漏报，而调用方已经逐轮写了 `quarantined(<码>)`。
//! - 不读钟、不打印；`now_ms` 由调用方给。
//!
//! S3b-4a：`hub review run` 的两件新面都在这份文件里——
//! - [`RetryPolicy`]：`RespectState`（runner 用，`quarantined` 空操作）与 `Quarantined`
//!   （`hub review run` 用，隔离项按 pending 同一路径重跑）。[`run_item`] 等于前者。
//! - [`mark_turns`]：把一次运行的终态铺到批内轮（naming 整批同格；fact/event **先按轮
//!   聚合再每轮写一次**，没有草稿的轮不动）。runner 继续用自己的映射，本轮不改；hub 不
//!   复制这套键规则。
//!
//! H3：`mark_turns` 与 [`mark_batch`] 的逐轮口径统一到"一条轮一个终态"——
//! - 同轮多条草稿按 `uncertain > done/duplicate > rejected` 聚合（与 runner 的逐轮终态
//!   同一条顺序），后写不再覆盖先写；
//! - 作用域只到**载荷里有草稿的轮**（[`draft_turns`]）；载荷不可读才回退整批
//!   `turn_uids_json`（fail-closed）。批内没有草稿的轮（抽取期已 `empty`/`rejected`）
//!   不再被审核失败改写。
//!
//! H4a：入口先做**载荷防篡改**复核（[`payload_verified`]）：`payload_json` 与列
//! `payload_sha256` 不一致的 item 一律不进 replay、不问模型、不写库。pending/approved
//! 记 `program/uncertain(PAYLOAD_GAP, detail=payload_tampered)` 推 `quarantined`；
//! applied 只返回 fail-closed（ledger 已是 Done，不碰）；quarantined 已是隔离态，
//! 不重复记账。
//!
//! H5a：pending 分支在**调模型之前**比一次审核输入尺寸（[`REVIEW_INPUT_MAX_BYTES`]，
//! `system + user` 的 UTF-8 字节）。超限 ⇒ [`oversized_review_input`] 记
//! `program/uncertain(REVIEW_INPUT_TOO_LARGE)` 推 `quarantined`、返回 fail-closed 读数：
//! 不截断、不问模型、不写库。截断再送只会走到 SchemaMismatch 与无限退避。
//!
//! H5b：审核调用按 [`crate::review::REVIEW_MAX_ITEMS`]（10）条一片切开（设计 §6 的
//! "窄审每批 ≤10 条"）。边界与映射：
//! - **fact/event** 按草稿顺序 `chunks(10)`：每片 user 只渲染本片草稿（`ITEM f1..fk`
//!   从片内 1 编号），refs 传片内 `f1..fk`/`e1..ek`；解析结果按
//!   `chunk_offset + local_index` 映射回**全局** ref（载荷里草稿的原始 `f*`/`e*`）。
//! - **naming** 按锚顺序 `chunks(10)`：每个子批只含该片锚与 `anchor_ref` 命中片内的提及
//!   （TURNS 仍是整批的轮）；parser 的 `a1..ak` 映射回该锚的**原始 reference**。
//! - **每片**都先过 H5a 的 [`oversized_review_input`]：任一片超限 ⇒ 整 item
//!   `review_input_too_large` 隔离，**一片都不送**。
//! - 任一调用 LLM 失败 ⇒ 立即 `Err(RunError::Llm)`（item 留 pending，已花费的调用不写判定）。
//! - 任一片解析失败 ⇒ 落该片 raw 的 artifact、记一条 `review/uncertain(<slug>)`、
//!   整 item quarantine（不继续后续分片、不写库）。
//! - 所有成功的片聚合成**一条** review 判定（不是每片一条）：`p/r/u` 与 `review_detail`
//!   用全局 ref；`input_sha256` 仍是载荷指纹；`review_input_sha256` 是**分片合成指纹**
//!   `extraction_fingerprint(pipeline, model, system, users.join("\n\x1e\n"))`
//!   （`users` 为按顺序的各片 user）——多片时它不单指某一次调用，而是这一整次 item 审核
//!   送出去的全部输入的指纹；单片时与旧口径逐字节相同。
//! - `time` 不调模型，不变。
//!
//! H7c2：引擎收尾（[`run_item_with_policy`] 返回前）best-effort 写一次**该 pass** 的
//! 审查轴水位（[`write_watermark`]：`stats` 的三态 + `max_decision_id` → ledger 的
//! `review_watermark`）。写失败不改变引擎结果；纯 LLM 故障与队列竞态不写。
//!
//! S3c-3b：rows 走这条引擎（`pass_id=rows`、family 恒空串）。审核输入 = `rows_review_system()`
//! + `rows_review_user(&chunk)`，指纹走 `extraction_fingerprint(ROWS_PIPELINE, model, system,
//! user)`；refs 是行的**位置编号** `r1..rN`（与 prompt 的 `ROW r{n}` 同一套）。
//! - 逐行渲染项：`text`/`category` 取行值，`subject_canonical` 按 `subject_anchor_id` 现查
//!   `anchor.canonical`（查不到 ⇒ [`REVIEW_REF_MISSING`]，与 parser 失败同一条路径：记一条
//!   `review/uncertain(review_ref_missing)`、不调模型、不写库），`evidence_body` 取重放批里
//!   `evidence_turn_uid` 那一轮的正文，`conflict` 取载荷 `conflict.value_text`（可空）。
//! - **只有聚合 Pass 才写库**：对每条 pass 行把行值转成 SQLite 标量后逐行
//!   `writers_rows::insert_fact_row`（`n==1` 计 inserted、`n==0` 计 duplicate）；数组/对象
//!   不是列值 ⇒ [`REVIEW_PAYLOAD_SHAPE`]。apply 判定 detail 是 `rows=<n>;duplicate=<n>`。
//! - 读数：`written`/`facts` = inserted、`duplicate` = duplicate、`cards`/`mentions` = 0。
//! - rows 不写 `extract_pass_state`：[`mark_turns`]/[`mark_batch`] 对 `pass_id=rows` 直接
//!   `Ok(())`，水位照写（ledger 的 `review_watermark` 没有 pass CHECK）。
//!
//! §7.3b：判 `uncertain` 的 **fact** 草稿走 [`retry_uncertain`]——队列只看
//! `state='quarantined'` + `review_verdict.side='final' AND verdict='uncertain'`；每条草稿
//! 单独重问一次（`pass_prompt::fact_review_retry_*`，自己的 canary），输入比正常审核更全
//! （引文边界 / 整轮 / 前后邻轮 / 同轮子句清单 / 候选锚全表）；item 终态由程序按
//! 「任一条 reject ⇒ rejected；全 pass ⇒ applied；仍有 uncertain ⇒ 保持 quarantined」合成；
//! 四个率由 [`RetryMetrics`] 数出来、落 `review_decision.detail` 与 `--json`。
//! 证据规则与 §7.2 同一份实现（`pass_fact_contract::review_ruling`），不合格逐条降 uncertain。

use rc_ledger::pass_state::{self, PassKey, PassState};
use rc_ledger::review_mark;
use rc_ledger::water;
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::review::{
    append_program_reject, decisions, get, payload_verified, record_decision, Decider, Item,
    ReviewStage, ReviewState, ReviewVerdict,
};
use rc_memory::owner::{visibility_slug, OwnerCtx};
use rc_memory::verdict::{
    item_verdicts, item_verdicts_filtered, replace_item_verdicts, EvidenceState, NewVerdict,
    VerdictSide,
};
use rc_memory::writers_fact::{
    put_fact_rows_with_extras, FactExtras, FactWriteItem,
};
use rc_memory::writers_state::{put_state_rows, StateWriteItem};
use rc_memory::writers_rows::insert_fact_row;
use rc_memory::writers_v4::{write_a1_with_owner, A1Batch};
use rc_memory::{AnchorRole, MemError, MemKey, Scope};
use rc_store::{LedgerHandle, MemoryHandle, StoreError, TranscriptHandle};
use rusqlite::types::Value as SqlValue;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};

use crate::contract::ContractError;
use crate::batch::Batch;
use crate::pass_event::{draft_value, draft_value_json, EVENT_PIPELINE};
use crate::pass_event_contract::{parse_event_review_full, EventPassError};
use crate::pass_prompt::{state_review_system, state_review_user, PASS_STATE};
use crate::pass_state::STATE_PIPELINE;
use crate::pass_state_contract::{parse_state_review, StatePassError};
use crate::pass_fact::{already_written, family_of_slug, FACT_PIPELINE_PREFIX};
use crate::pass_fact_contract::{
    parse_fact_review, parse_fact_review_retry, EvidenceDowngrade, FactDraft, FactPassError,
    FactReviewReport, ReviewTarget, ReviewVerdict as DraftVerdict,
};
use crate::pass_naming::NAMING_PIPELINE;
use crate::pass_naming_review_contract::parse_naming_review;
use crate::pass_lesson::{LESSON_PIPELINE, REVIEW_REJECT_CODE};
use crate::pass_lesson_contract::{parse_lesson_review, LessonPassError, PASS_LESSON};
use crate::pass_prompt::{
    event_review_system, event_review_user, fact_review_challenge_system, fact_review_retry_system,
    fact_review_retry_user, fact_review_system, fact_review_user, lesson_review_system,
    lesson_review_user, naming_review_system, naming_review_user, rows_review_system,
    rows_review_user, FactFamily, ReviewInputContext, RowReviewItem, ValueAxis, PASS_EVENT,
    PASS_FACT,
};
use crate::pass_rows_review_contract::parse_rows_review;
use crate::review::gate::{
    check_subject_in_evidence, event_actor_rebind, known_principal, SubjectCheck, SubjectClaim,
};
use crate::review::PASS_STATE_REVIEW_VERSION;
use crate::review::replay::RowsDraft;
use crate::review::{
    gate_report, review_detail, GateReport, Replay, ReplayDrafts, ReplayFailure,
    PASS_EVENT_REVIEW_VERSION, PASS_FACT_RETRY_VERSION, PASS_FACT_REVIEW_VERSION,
    PASS_LESSON_REVIEW_VERSION, PASS_NAMING_REVIEW_VERSION, REVIEW_MIXED, REVIEW_PASS,
    REVIEW_REJECT, REVIEW_UNCERTAIN, REVIEW_VERDICTS_MISSING, REVIEW_MAX_ITEMS, ROWS_PIPELINE,
    ROWS_REVIEW_VERSION,
};
use crate::DistillError;

/// 判定缺失/载荷不可重放时的隔离码（落 `review_decision.reason_code` 与返回给调用方的
/// `fail_closed_reason`）。
pub const PAYLOAD_GAP: &str = "review_payload_gap";

/// H5a：审核输入的字节上限（`system.len() + user.len()`）。
///
/// 超限的输入既不截断也不送模型：截断只会让模型答非所问、解析失败、无限退避。这里确定性
/// 隔离，人看台账再决定拆批。H5b 起审核调用已按 [`crate::review::REVIEW_MAX_ITEMS`] 条
/// 分片，本上限改按**单片**判（任一片超限即整 item 隔离）。
pub const REVIEW_INPUT_MAX_BYTES: usize = 256 * 1024;

/// H5a：超长审核输入的隔离码（`review_decision.reason_code` 与 `fail_closed_reason` 同码，
/// detail 同词）。
pub const REVIEW_INPUT_TOO_LARGE: &str = "review_input_too_large";

/// H4a：`payload_json` 与列 `payload_sha256` 不一致时的判定 detail（与 `approved_gap`
/// 同一格：程序侧裁定，不带正文）。
const TAMPERED_DETAIL: &str = "payload_tampered";

/// rows 的主语锚查不到时的隔离码（S3c-3b）。
///
/// 与 parser 失败同一条路径：判定记 `review/uncertain(review_ref_missing)`、
/// [`RunError::ReviewRows`] 装**已带 `review_` 前缀**的同一个串、不写库。没有模型原文
/// 可落 artifact——这一格在调模型之前就判掉了。
pub const REVIEW_REF_MISSING: &str = "review_ref_missing";

/// rows 的行值不是标量（数组/对象）时的隔离码（S3c-3b）。
///
/// 行值要逐个绑定进 `fact` 的列，数组/对象不是列值。同一 item 的行值**全部转换完**
/// 才开始写库：形状错一笔都不写，判定也不记 apply（item 留 approved）。
pub const REVIEW_PAYLOAD_SHAPE: &str = "review_payload_shape";

/// rows 在 `extract_pass_state`/artifact 里的 pass 字面量（`PASS_IDS` 没有 rows）。
const ROWS_PASS: &str = "rows";

/// 引擎的失败面。每一支的"写没写"语义见各变体文档。
#[derive(Debug)]
pub enum RunError {
    /// 窄审 LLM 故障：**不记判定、不写库**，item 留在原状态（pending）。
    Llm(LlmFailure),
    /// 审核输出不是判决：判定已记 `uncertain(<reason>)`，artifact 已落；调用方负责逐轮隔离。
    Review(DistillError),
    /// naming 的审核输出不是判决（S3b-3b）：与 [`RunError::Review`] 同一处语义——判定已记
    /// `uncertain(<reason>)`、artifact 已落；装的是**已带 `review_` 前缀**的隔离码（H6），
    /// 调用方逐轮写 `quarantined(<同一个串>)`。naming 没有自己的 `DistillError`
    /// 契约变体，所以不复用 `Review`（不把解析失败伪装成别的 pass 的契约错）。
    ReviewNaming(String),
    /// rows 的审核输出不是判决，或窄审前就判掉的确定性缺口（S3c-3b）：与
    /// [`RunError::ReviewNaming`] 同格——装的是**已带 `review_` 前缀**的隔离码
    /// （parser 的 slug 或 [`REVIEW_REF_MISSING`]）。判定已记 `uncertain(<码>)`；不写库。
    ReviewRows(String),
    /// 未支持/状态不对/找不到 item：不写任何东西。
    Unsupported,
    MissingItem,
    /// H1a：队列状态机拒绝（另一写者已推进状态/非法转换）。**本次什么也没做**：
    /// 判定与状态更新在同一事务里回滚，ledger 也不该被改写。
    Conflict,
    /// 波三刀 A：lesson 的审核输出不是判决。语义与 [`RunError::Review`] 逐条相同
    /// （判定已记 `uncertain(<reason>)`、artifact 已落），但**不新增 `DistillError` 变体**：
    /// 那个枚举的读者里有一支在 `rc-cycle` 的闭集匹配上，加一格就要改那把刀的文件集之外。
    /// 装 parser 的变体，调用方按契约族报。
    ReviewLesson(LessonPassError),
    /// 存储/事务失败：判定可能已记 review，apply 未记；item 留 approved。
    Store(DistillError),
}

/// 一次引擎运行的读数。
#[derive(Debug)]
pub struct RunOutcome {
    /// 引擎结束时 item 的状态（record_decision 的返回值；无写入时读到的原状态）。
    pub item_state: ReviewState,
    /// 逐草稿判决（顺序 = 载荷 items 顺序，ref = f1..fN / e1..eN / a1..aN）。
    /// `None` = 判决不可重建（fail-closed），调用方按隔离处理，不许当"全 pass"。
    pub verdicts: Option<Vec<(String, ReviewVerdict)>>,
    /// verdicts=None 时的隔离码（`PAYLOAD_GAP` 或 `REVIEW_VERDICTS_MISSING`）。
    pub fail_closed_reason: Option<&'static str>,
    /// 工单 §7.2 证据闸的程序侧降级：`(全局 ref, 原因 slug)`，按片顺序。
    ///
    /// 原因只取 [`EvidenceDowngrade::as_str`]（`evidence_missing` / `evidence_not_in_quote`），
    /// **不进** `REVIEW_CODES`；落库 reason 仍是 `review_uncertain`。这条读数是"哪些条被闸
    /// 降过级、为什么"的唯一程序侧出口（不要求落库）。
    pub evidence_downgrades: Vec<(String, &'static str)>,
    /// 两档合成后的**逐条**判决与 code（全局 ref）：裁决矩阵的程序侧读数。
    ///
    /// `verdicts` 只有判决；这一格多带 code，才能断言"challenger reject 的 code 胜出、
    /// 否则 proposer reject 的 code 胜出"（工单 §2.4）。程序闸拒收的 draft 不进这一格
    /// （它没被送去审核），与 `verdicts` 的差别只在 code。
    pub adjudications: Vec<(String, ReviewVerdict, Option<String>)>,
    /// 程序闸逐 draft 拒收的证据轮 `(turn_uid, 码)`（I3：判定单位是 draft）。
    ///
    /// 已剔除"被通过 draft 覆盖"的轮（工单 §4：同一轮任一条 draft 通过就不算坏草稿的轮）。
    /// 逐轮终态用它把坏 draft 的轮写成 `rejected(<码>)`，而不是把整批写成 `rejected`。
    /// 只有 `rows` 与"闸没跑过"的路径为空。
    pub program_rejected_turns: Vec<(String, &'static str)>,
    /// fact 的跨 family 去重命中轮（event / naming 恒空）。
    pub duplicate_uids: BTreeSet<String>,
    /// 本次"交出去写"的条数：fact/event 是写库项数，naming 是落库锚数。
    pub written: usize,
    pub duplicate: usize,
    /// A-rebind：本次由程序把 actor **回填**成会话自锚的 event 条数（其余 pass 恒 0）。
    ///
    /// 与 §7.3 的指标同一出口风格：程序记、不要求落库正文；`assemble_event` 每替一条
    /// 记一次，[`RunOutcome`] 与 apply 的 `detail`（`;rebound=N`，只在 N>0 时出现）各带一份。
    pub actor_rebound: usize,
    /// fact/event 是插入的事实行数；naming 复用这格放**落库锚数**
    /// （与 `NamingOutcome.anchors` 同格，`written` 同值）。
    pub facts: usize,
    /// fact/event 是插入的卡数；naming **恒 0**（命名不产卡）。
    pub cards: usize,
    /// fact/event 是插入的提及行数；naming 复用这格放**新插入的提及行数**
    /// （与 `NamingOutcome.mentions` 同格）。
    pub mentions: usize,
    /// 本次真的写了库（Applied 重放为 false）。
    pub applied_now: bool,
    /// S4：本次运行**真的插入**的卡句柄（只有 fact/event 产卡；naming/rows/state 恒空）。
    ///
    /// 提升**不由 hub 自己抄一遍**，也不在引擎里做：每个 apply 入口（`hub review run`、内联
    /// 窄审、`hub review retry-uncertain`）在收尾时把这一格交给
    /// [`promote_applied_cards`]，由它逐条调 [`rc_memory::promote_verdict`]。提升的口径是
    /// "这次 apply 写下的卡"，所以这里只装插入成功的那一组：`INSERT OR IGNORE` 命中同址
    /// 既有卡（去重）时它已经不是本次产物，不进这一格；推导卡（`mode='inferred'`）走另一条
    /// 写入口，天然不在这里。**重放**（item 已是 applied）也不产 id ⇒ 空：这一版不回填旧卡。
    ///
    /// 谁消费：`hub/src/review.rs` 的 `run_claimed`、`pass_fact`/`pass_event`/`pass_lesson` 的
    /// runner、`review::run::finish_retry`——**这五处是全部**；新增 apply 入口时必须一并接
    /// [`promote_applied_cards`]，否则那一族卡会停在 `unreviewed` 等 audit 兜底。
    pub card_ids: Vec<MemKey>,
}

/// S4：review **apply 之后**的唯一收尾动作——把这次真写下的卡 `unreviewed → passed`。
///
/// 五个 apply 入口都拿到同一份 [`RunOutcome`]，提升口径也只在这里写一遍：
/// `hub review run`（`run_item_with_policy` 的调用方）、内联窄审（`pass_fact`/`pass_event`/
/// `pass_lesson` 的 runner 调 `run_item` 的那一格）、`hub review retry-uncertain`
/// （`retry_uncertain` 的 `finish_retry`）。**不放进 `write_rows`**：抽取也走同一个写库口，在那里提升等于
/// "提取即通过"，正是 S4 要避免的；所以它必须由调用方在 apply 边界上显式调一次。
///
/// 三条口径：
/// - 只有 `item_state == Applied` 才进循环。非 applied 的 `card_ids` 本来就恒空
///   （`RunOutcome::zero`），这道守卫是纵深防御：读的人不必先证明"空"。
/// - 只提升 `card_ids` 里的卡——那是**本次插入成功**的一组（去重命中同址旧卡返回 0，
///   不进这一格）。这里因此不解析载荷、也不重算内容寻址 id：hub 不懂 payload 的 JSON。
/// - 返回真正被推动的行数；0 行**不是错误**（[`rc_memory::promote_verdict`] 的 CAS 语义：
///   那张卡已不是 `unreviewed`）。错误原样上抛，**由调用方定夺**是否吞掉——理由写在
///   各调用点：pass runner 与重跑只能吞（行已提交、逐轮终态已落，上抛会让同一批被读成
///   "没做成"并重铺 pending），`hub review run` 则按 `mark_*` 那条尺子停本轮。
pub fn promote_applied_cards(
    m: &MemoryHandle,
    outcome: &RunOutcome,
    now_ms: i64,
) -> Result<usize, MemError> {
    if outcome.item_state != ReviewState::Applied {
        return Ok(0);
    }
    let mut promoted = 0usize;
    for key in &outcome.card_ids {
        // `?` 在函数内部照常上抛：第 k 条失败时前 k-1 条已提升（多条 CAS 不在一个事务里，
        // 已裁定的 M3），由调用方决定这次失败是否要中止它自己那一轮。
        promoted += rc_memory::promote_verdict(m, key, now_ms)?;
    }
    Ok(promoted)
}

impl RunOutcome {
    /// 零读数：状态给定，其余全 0/false。
    fn zero(item_state: ReviewState) -> Self {
        RunOutcome {
            item_state,
            verdicts: None,
            fail_closed_reason: None,
            evidence_downgrades: Vec::new(),
            adjudications: Vec::new(),
            program_rejected_turns: Vec::new(),
            duplicate_uids: BTreeSet::new(),
            written: 0,
            duplicate: 0,
            actor_rebound: 0,
            facts: 0,
            cards: 0,
            mentions: 0,
            applied_now: false,
            card_ids: Vec::new(),
        }
    }
}

/// 本轮的 pass 组合：fact（已知 family）、event（family 恒空串）、naming（family 恒空串）
/// 与 rows（family 恒空串；S3c-3b）。
#[derive(Debug, Clone, Copy)]
enum Pass {
    Fact(FactFamily),
    Event,
    Naming,
    Rows,
    /// P2 状态 pass（family 恒空串，与 event/naming/rows 同一条纪律）。
    State,
    /// 波三刀 A：lesson（经验卡）。family 恒空串；终态行由 `pass_lesson::mark_lesson_terminal`
    /// 自己写（`lesson` 自波三修 1 起在 `PASS_IDS` 六项里，引擎这一层不重写），幂等落在
    /// `review_item` 的 batch_key 上。
    Lesson,
}

/// 引擎在 `pending`/`approved` 上重算程序闸时，批级前置命中的判定 detail（不带正文）。
const GATE_BATCH_DETAIL: &str = "program_gate_batch";

/// 一次程序闸的逐 draft 结果（I3）。引擎只消费三格：要剔出审核输入的 draft ref、
/// 要逐轮落 `rejected(<码>)` 的轮，以及**逐条码本身**（C3 的审计行按它一行一格地写）。
/// 批级前置由调用方先判（命中即整条 item 隔离）。
#[derive(Debug, Default)]
struct ProgramGate {
    failed_refs: HashSet<String>,
    /// 逐 draft 的失败码（ref → 码），**载荷顺序**。normal 路径只用 `failed_refs`；
    /// §7.3b 的重跑要按 ref 取码（"code 取那条的"），C3 的审计行按这个顺序逐条写。
    rejects: Vec<(String, &'static str)>,
    rejected_turns: Vec<(String, &'static str)>,
}

impl ProgramGate {
    fn from_report(report: &GateReport) -> Self {
        ProgramGate {
            failed_refs: report.failed().map(|draft| draft.reference.clone()).collect(),
            rejects: report
                .failed()
                .filter_map(|draft| draft.code.map(|code| (draft.reference.clone(), code)))
                .collect(),
            rejected_turns: report.rejected_turns(),
        }
    }

    /// 该 draft 被程序闸拒收时的码（§7.3b 的 item 驳回码就用它）。
    fn code_of(&self, reference: &str) -> Option<&'static str> {
        self.rejects
            .iter()
            .find(|(failed, _)| failed == reference)
            .map(|(_, code)| *code)
    }

    /// 把程序拒收的 draft 强制成 Reject：**闸是硬门**——判决台账里即便有它的 pass
    /// （approved 的 human pass / 重建路径），也不许被组装写库。
    fn enforce(&self, pairs: Vec<(String, DraftVerdict)>) -> Vec<(String, DraftVerdict)> {
        pairs
            .into_iter()
            .map(|(reference, verdict)| {
                if self.failed_refs.contains(&reference) {
                    (reference, DraftVerdict::Reject)
                } else {
                    (reference, verdict)
                }
            })
            .collect()
    }
}

/// 只重算闸、不解释批级结果：调用方自己决定批级命中怎么收尾。
fn gate_of(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    item: &Item,
    now_ms: i64,
) -> Result<GateReport, RunError> {
    gate_report(m, t, item, now_ms).map_err(distill_store)
}

/// C3：程序闸逐 draft 拒收的**审计行**——一条被闸拒的 draft 一行 `review_decision`。
///
/// 补的是"审核拒收逐 draft 可审计"：闸的码原先只被折成批级计数（`review_detail`）与逐轮
/// 终态（`rejected(<码>)`），事后查不到"哪几条草稿、各是什么码"。四格取值与该 item 的
/// 汇总判定同源（同一个 `now_ms`、同一个载荷指纹、同一份 model/prompt 版本）；`detail`
/// 只有 `ref=<f1|e1|s1|a1|…>` 一格，不塞正文也不塞载荷。
///
/// 幂等由 [`append_program_reject`] 执法：同 item + 同码 + 同 ref 只落一行，所以 pending/
/// approved 的复算与重跑不会长出重复行；码变了是另一次裁定，另起一行。
///
/// 只写"闸已经判完"的那几条——批级前置命中时没有可指的 draft，这一层不动。
fn audit_program_rejects(
    m: &MemoryHandle,
    item: &Item,
    gate: &ProgramGate,
    model: &str,
    prompt_version: &str,
    now_ms: i64,
) -> Result<(), RunError> {
    for (reference, code) in &gate.rejects {
        let decision = super::decision(
            ReviewStage::Program,
            ReviewVerdict::Reject,
            code,
            model,
            prompt_version,
            &item.payload_sha256,
            "",
            Decider::Program,
            &format!("ref={reference}"),
        );
        append_program_reject(m, item.meta.id, &decision, now_ms).map_err(mem_store)?;
    }
    Ok(())
}

fn mem_store(e: MemError) -> RunError {
    // H1a：队列状态机拒绝（`review: illegal transition …`）不是存储故障——另一写者已把
    // 状态推进/非法转换，本次判定整体回滚。单列一支：调用方据此不改 ledger、不记退避。
    if e.is_review_conflict() {
        return RunError::Conflict;
    }
    RunError::Store(DistillError::Memory(e))
}

fn distill_store(e: DistillError) -> RunError {
    RunError::Store(e)
}

/// A7：给重放批回填**轮级说话人**。`mention` 是库里唯一写着"这一轮是谁说的"的形状，
/// [`crate::batch::load_turn_speakers`] 是唯一读法（抽取侧 `pass_event`/`pass_state`
/// 同一函数；读不到 = 保持 `None`，渲染逐字回落 role 推导，输出与改动前一致）。
///
/// replay 本身只读转录库（模块文档的"唯一的 I/O"纪律），所以这一步在引擎侧、replay
/// **之后**做。三个送审入口共用本 helper：窄审（`run_pending`）、prompt 重放
/// （`render_prompt_for_item`）、重跑（`retry_item`）——缺一处，三份输入就不一致。
/// 注意 `run_approved`/`run_applied` 不送审，不调这里。
fn fill_turn_speakers(m: &MemoryHandle, replayed: &mut Replay) -> Result<(), RunError> {
    crate::batch::load_turn_speakers(m, &mut replayed.batch.rows)
        .map(|_| ())
        .map_err(DistillError::from)
        .map_err(RunError::Store)
}

fn contract_store(e: ContractError) -> RunError {
    RunError::Store(DistillError::Contract(e))
}

fn supported_pass(item: &Item) -> Result<Pass, RunError> {
    match item.meta.pass_id.as_str() {
        "fact" => family_of_slug(&item.meta.family)
            .map(Pass::Fact)
            .ok_or(RunError::Unsupported),
        "event" if item.meta.family.is_empty() => Ok(Pass::Event),
        "naming" if item.meta.family.is_empty() => Ok(Pass::Naming),
        // S3c-3b：rows 与 event/naming 同一条 family 纪律（中心 pass，family 恒空串）。
        "rows" if item.meta.family.is_empty() => Ok(Pass::Rows),
        "state" if item.meta.family.is_empty() => Ok(Pass::State),
        // 波三刀 A：lesson 与 event/naming/rows/state 同一条 family 纪律（中心 pass，family 恒空串）。
        "lesson" if item.meta.family.is_empty() => Ok(Pass::Lesson),
        _ => Err(RunError::Unsupported),
    }
}

/// `hub review run` 的重试策略（S3b-4a）。runner 与 hub 对 `quarantined` 的态度不同：
/// runner 只负责"这一批的这一次运行"，隔离项留给人；`hub review run` 的职责恰恰是把它
/// 再问一遍（设计 §11 的 S3b 验收：`quarantined` 人工放行后补写）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryPolicy {
    /// 只按 item 自己的状态分派：`quarantined` 空操作。runner 用这一格。
    RespectState,
    /// `quarantined` 按 `pending_review` 同一路径重跑（replay → 窄审 → 判定）。
    Quarantined,
}

/// 对一条已入队的 item 跑一次引擎（runner 用：`quarantined` 空操作）。
pub fn run_item(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    l: &LedgerHandle,
    item_id: i64,
    backend: &dyn LlmBackend,
    now_ms: i64,
) -> Result<RunOutcome, RunError> {
    run_item_with_policy(m, t, l, item_id, backend, now_ms, RetryPolicy::RespectState)
}

/// 对一条已入队的 item 跑一次引擎，`policy` 决定隔离项要不要再问一遍。
///
/// `RetryPolicy::Quarantined` 只在 **item 状态是 `quarantined`** 时改变行为：走
/// [`run_pending`] 的同一条路（重放 → 窄审 → 判定 → 写库）；`Rejected`/`Stale`/`Applied`
/// 仍按各自分派。复审判定的落库由 `record_decision` 决定（S3b-4a 起 `review` 阶段允许
/// 从 `quarantined` 出发），本函数不另写状态。
///
/// H7c2：返回前 best-effort 写一次**该 pass** 的审查轴水位（[`write_watermark`]）——
/// Ok、以及"已记判定"的 Err（`Review`/`ReviewNaming`/`Store`）都写；纯 LLM 故障、
/// 未支持/找不到 item、队列竞态（`Conflict`：本事务整体回滚）不写。写水位失败不改变
/// 引擎结果（`let _ =`）：它是读数，不是这一步的产出。
pub fn run_item_with_policy(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    l: &LedgerHandle,
    item_id: i64,
    backend: &dyn LlmBackend,
    now_ms: i64,
    policy: RetryPolicy,
) -> Result<RunOutcome, RunError> {
    let item = get(m, item_id).map_err(mem_store)?.ok_or(RunError::MissingItem)?;
    let pass = supported_pass(&item)?;
    // H4a：载荷指纹必须与列一致，且要在任何 replay/模型调用之前判。不一致 ⇒ 这一行
    // 已经不是入队时那一份载荷：不重放、不问模型、不写库，只交回 fail-closed 读数。
    let out = if !payload_verified(&item).map_err(mem_store)? {
        payload_tampered(m, &item, now_ms)
    } else {
        match item.meta.state {
            ReviewState::PendingReview => run_pending(m, t, l, &item, pass, backend, now_ms),
            ReviewState::Approved => run_approved(m, t, l, &item, pass, backend, now_ms),
            ReviewState::Applied => run_applied(m, t, &item, now_ms),
            // S3b-4a：隔离项在 `hub review run` 里按 pending 同一路径重跑；runner 那一支仍空操作。
            ReviewState::Quarantined if policy == RetryPolicy::Quarantined => {
                run_pending(m, t, l, &item, pass, backend, now_ms)
            }
            // rejected / quarantined（RespectState）/ stale：不写任何东西，状态推进留给调用方。
            ReviewState::Rejected | ReviewState::Quarantined | ReviewState::Stale => {
                Ok(RunOutcome::zero(item.meta.state))
            }
        }
    };
    let decision_may_be_recorded = match &out {
        Ok(_) => true,
        Err(
            RunError::Review(_)
            | RunError::ReviewNaming(_)
            | RunError::ReviewRows(_)
            | RunError::ReviewLesson(_)
            | RunError::Store(_),
        ) => true,
        Err(
            RunError::Llm(_) | RunError::Unsupported | RunError::MissingItem | RunError::Conflict,
        ) => false,
    };
    if decision_may_be_recorded {
        let _ = write_watermark(m, l, &item.meta.pass_id, now_ms);
    }
    out
}

/// DR：一次审核调用的**原文**——`system` 与按顺序的每一片 `user`。
///
/// 它只是 `ReviewPlan` 的只读投影：片数、片内编号与线上逐字一致，调用方（hub 的
/// `review show ID --prompt`）只负责打印。
#[derive(Debug, Clone)]
pub struct ReviewPrompt {
    pub system: String,
    /// 每一片的 `user`，顺序就是真实运行送出的顺序（单片 ⇒ 只有一格）。
    pub chunks: Vec<String>,
}

/// DR：重建"这条 item 当时会送出的审核输入"，**不调模型、不写任何表**。
///
/// 与 `run_pending` 共用同两行：同一个 `gate_of`（同一份程序闸报告）与同一个
/// `review_plan`（同一份 skip 集合 ⇒ 同一套片内编号）。这一层不复制任何渲染/分片逻辑：
/// 两份输入如果哪里不一样，那就是引擎的 bug，不是这条出口的。
///
/// 返回值：
/// - `Ok(Some(_))`：真实运行会在这一格开口；
/// - `Ok(None)`：真实运行也在开口**之前**收口——载荷与列指纹不一致（H4a）、载荷不可重放
///   （gap）、批级闸前置命中，或 rows 的主语锚查不到。这一格没有"模型看到的原文"可言，
///   打印层据此说没有 prompt，而不是打一份半成品；
/// - `Err(_)`：存储/句柄故障，交给调用方按运维面报。
///
/// `item` 的状态不限（pending/approved/applied/rejected 都只是分派条件，replay 只读载荷
/// 与转录）：`rejected` 的 item 也能看到它当初那份输入，这正是审计要的。
pub fn render_prompt_for_item(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    item: &Item,
    now_ms: i64,
) -> Result<Option<ReviewPrompt>, RunError> {
    let pass = supported_pass(item)?;
    // H4a：载荷与列指纹不一致 ⇒ 真实运行不进 replay、不问模型。
    if !payload_verified(item).map_err(mem_store)? {
        return Ok(None);
    }
    let mut replayed = match super::replay::replay(t, item) {
        Ok(replayed) => replayed,
        Err(ReplayFailure::Gap(_)) => return Ok(None),
        Err(ReplayFailure::Store(e)) => return Err(RunError::Store(DistillError::Memory(e))),
    };
    // A7：show prompt 必须与真实送出的输入逐字节一致——回填与 `run_pending` 共用同一 helper。
    fill_turn_speakers(m, &mut replayed)?;
    // 与 `run_pending` 同一段：rows 不走这道闸；其余 pass 的批级前置命中即整条隔离。
    let gate = if matches!(pass, Pass::Rows | Pass::Lesson) {
        ProgramGate::default()
    } else {
        let report = gate_of(m, t, item, now_ms)?;
        if report.batch_code.is_some() {
            return Ok(None);
        }
        ProgramGate::from_report(&report)
    };
    let plan = match review_plan(m, &item.meta.session_key, pass, &replayed, &gate.failed_refs) {
        Ok(plan) => plan,
        // rows 的主语锚查不到：真实运行记 `review_ref_missing`、不开口。这一格没有送审输入。
        Err(RunError::ReviewRows(_)) => return Ok(None),
        Err(other) => return Err(other),
    };
    Ok(Some(ReviewPrompt {
        system: plan.system,
        chunks: plan.chunks.into_iter().map(|chunk| chunk.user).collect(),
    }))
}

/// H7c2：把一个 pass 的审查轴水位写进 ledger 的 `review_watermark`（**唯一写入口**，
/// 三个写点共用：引擎收尾、time runner 收尾、`hub review decide`）。
///
/// 取数是 [`rc_memory::review::stats`] 的**当前值**（全 pass 扫描，不是增量）+ 本地
/// `review_decision` 的最大 id；三态只取 `pending_review`/`quarantined`/`applied`
/// （`stats.by_pass` 的 6 格下标见 `ReviewStats`）。该 pass 一行队列都没有 ⇒ 三态全 0，
/// 但最大 id 照写（判定可能来自别的 pass，水位仍是水位）。
///
/// 调用方一律 `let _ =`：写失败不改变这一次作业的结果。
pub fn write_watermark(
    m: &MemoryHandle,
    l: &LedgerHandle,
    pass_id: &str,
    now_ms: i64,
) -> Result<(), StoreError> {
    let stats = rc_memory::review::stats(m, now_ms)
        .map_err(|_| StoreError::Sql("review: watermark stats unreadable".to_string()))?;
    let counts = stats
        .by_pass
        .iter()
        .find(|(pass, _)| pass == pass_id)
        .map(|(_, c)| *c)
        .unwrap_or([0; 6]);
    let max_decision_id = rc_memory::review::max_decision_id(m)
        .map_err(|_| StoreError::Sql("review: watermark max decision id unreadable".to_string()))?;
    review_mark::put(
        l,
        &review_mark::ReviewWatermark {
            pass_id: pass_id.to_string(),
            max_decision_id,
            pending_review: counts[0],
            quarantined: counts[4],
            applied: counts[2],
            updated_at_ms: now_ms,
        },
    )
}

/// H4a：`payload_verified == false` 的处置（详情见派发档 §2）。
///
/// - `pending_review` / `approved`：记一条 `program/uncertain + PAYLOAD_GAP`
///   （detail `payload_tampered`）把 item 推到 `quarantined`——静默留在原状态会让
///   剩余计数与退出码看不见它。这条边是状态机已有的（H2 的 approved 隔离同一格）。
/// - `quarantined`：已经在隔离里，**不再写判定**——状态机不允许 `program/uncertain`
///   从 `quarantined` 出发（`program_uncertain_is_illegal_on_applied_stale_and_quarantined`），
///   而且隔离项不该每重试一次就多一条同因判定。返回 `fail_closed_reason=PAYLOAD_GAP`，
///   调用方照旧逐轮写隔离。
/// - `applied`：ledger 已是 Done，**不写判定、不写库、不改状态**，只 fail-closed 返回。
/// - `rejected` / `stale`：终态空操作。
fn payload_tampered(
    m: &MemoryHandle,
    item: &Item,
    now_ms: i64,
) -> Result<RunOutcome, RunError> {
    match item.meta.state {
        ReviewState::PendingReview | ReviewState::Approved => {
            quarantined_by_program(m, item, PAYLOAD_GAP, TAMPERED_DETAIL, now_ms)
        }
        ReviewState::Quarantined | ReviewState::Applied => {
            let mut out = RunOutcome::zero(item.meta.state);
            out.fail_closed_reason = Some(PAYLOAD_GAP);
            Ok(out)
        }
        ReviewState::Rejected | ReviewState::Stale => Ok(RunOutcome::zero(item.meta.state)),
    }
}

// ── 逐轮终态（S3b-4a）────────────────────────────────────────────────────────

/// 批内轮：`item.turn_uids_json`（与重放同一份来源）。读不出是数据不一致，不猜一批空。
fn batch_turns(item: &Item) -> Result<Vec<String>, StoreError> {
    serde_json::from_str::<Vec<String>>(&item.turn_uids_json)
        .map_err(|_| StoreError::Sql("review: turn_uids_json unreadable".to_string()))
}

/// 一轮的 `extract_pass_state` 键：fact 带 family，其余 pass 的 family 恒空串（设计 §3.3）。
fn turn_key<'a>(item: &'a Item, uid: &'a str) -> PassKey<'a> {
    let family = if item.meta.pass_id == "fact" {
        item.meta.family.as_str()
    } else {
        ""
    };
    PassKey { turn_uid: uid, pass_id: item.meta.pass_id.as_str(), family }
}

/// 载荷里有草稿的轮：`payload.items[].turn_uid`，去重保序。
///
/// - 全部可读 ⇒ `Some`（批内真的带草稿的轮）；
/// - 载荷/`items` 读不出、任一 `turn_uid` 缺失或不是字符串 ⇒ `None`
///   （调用方回退整批 `turn_uids_json` 的 fail-closed）。
///
/// naming 的 `items` 是四键对象（`anchors/mentions/issues/unresolved`，不是逐草稿数组），
/// 所以这一格对 naming 恒为 `None`：整批同一个终态与 naming 的写库口径一致。
fn draft_turns(item: &Item) -> Option<Vec<String>> {
    let payload: serde_json::Value = serde_json::from_str(&item.payload_json).ok()?;
    let items = payload.get("items")?.as_array()?;
    let mut turns: Vec<String> = Vec::with_capacity(items.len());
    let mut seen: HashSet<&str> = HashSet::new();
    for entry in items {
        let uid = entry.get("turn_uid")?.as_str()?;
        if seen.insert(uid) {
            turns.push(uid.to_string());
        }
    }
    Some(turns)
}

/// 把批内每一轮落成同一个终态。`hub review run` 的失败面（`Review`/`Store`）用它：
/// 那时没有可用的 `RunOutcome`（没有判决可逐条摊开），只能整批同一个状态。
///
/// H3：作用域是**载荷里有草稿的轮**（[`draft_turns`]），不是整批 `turn_uids_json`——
/// 批内没有草稿的轮（抽取期已 `empty`/`rejected(uncovered)`）不该被审核失败改写。
/// 载荷不可读 ⇒ 回退整批（拿不到草稿范围时的 fail-closed）。
pub fn mark_batch(
    l: &LedgerHandle,
    item: &Item,
    state: PassState,
    reason: Option<&str>,
    now_ms: i64,
) -> Result<(), StoreError> {
    // S3c-3b：rows 不写 `extract_pass_state`（`PASS_IDS` 没有 rows，`mark_terminal` 的
    // pass/family 校验会拒）——逐轮终态这一面对它没有可写的键，直接 no-op。
    // 波三刀 A：lesson 也直接 no-op，但理由不同——它的终态由 `pass_lesson::mark_lesson_terminal`
    // 自己写（`lesson` 自波三修 1 起在 `PASS_IDS` 里），引擎这一层不再重写；它的幂等落在
    // `review_item.batch_key`。
    if item.meta.pass_id == ROWS_PASS || item.meta.pass_id == PASS_LESSON {
        return Ok(());
    }
    let scope = match draft_turns(item) {
        Some(turns) => turns,
        None => batch_turns(item)?,
    };
    for uid in &scope {
        pass_state::mark_terminal(l, turn_key(item, uid), state, reason, now_ms)?;
    }
    Ok(())
}

/// `payload.items[i].turn_uid`：审核 ref 由**位置**定（`f1..fN`/`e1..eN`），与
/// `fact_review_user`/`event_review_user` 的编号同一套纪律。读不出整批 ⇒ Store 错，
/// 不把判决记到猜出来的轮上。
fn payload_turns(item: &Item) -> Result<Vec<String>, StoreError> {
    let payload: serde_json::Value = serde_json::from_str(&item.payload_json)
        .map_err(|_| StoreError::Sql("review: payload_json unreadable".to_string()))?;
    let items = payload
        .get("items")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| StoreError::Sql("review: payload items unreadable".to_string()))?;
    let mut turns = Vec::with_capacity(items.len());
    for entry in items {
        let uid = entry
            .get("turn_uid")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| StoreError::Sql("review: payload item turn_uid unreadable".to_string()))?;
        turns.push(uid.to_string());
    }
    Ok(turns)
}

/// 判决 ref ⇒ 载荷里同位置的轮（`f2` ⇒ `items[1].turn_uid`）。ref 没有数字后缀或越界 ⇒
/// `None`（调用方按 Store 错收尾：ref 与载荷对不上是数据不一致）。
fn turn_of_ref<'a>(turns: &'a [String], reference: &str) -> Option<&'a str> {
    let index = reference
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .parse::<usize>()
        .ok()?
        .checked_sub(1)?;
    turns.get(index).map(String::as_str)
}

/// 同一轮收到多条草稿判决时的合并：`uncertain > pass > reject`。
///
/// 与 runner 的逐轮终态同一条顺序（uncertain > done/duplicate > rejected）：`duplicate`
/// 是轮级信息，不在这一层合并，由 [`mark_turns`] 在 `pass` 之后、`reject` 之前判。
fn stronger(a: ReviewVerdict, b: ReviewVerdict) -> ReviewVerdict {
    match (a, b) {
        (ReviewVerdict::Uncertain, _) | (_, ReviewVerdict::Uncertain) => ReviewVerdict::Uncertain,
        (ReviewVerdict::Pass, _) | (_, ReviewVerdict::Pass) => ReviewVerdict::Pass,
        _ => ReviewVerdict::Reject,
    }
}

/// 把一次引擎运行的终态铺到 `extract_pass_state` 的批内轮上（`hub review run` 用；
/// runner 继续走自己的映射，本轮不改）。
///
/// 边界（派发档 §2/§3，H3 起与 runner 的逐轮优先级单源）：
/// - **naming**：整批同一个状态——`Applied ⇒ Done`；`Rejected ⇒ Rejected(program_code 或
///   review_reject)`；`Quarantined ⇒ Quarantined(fail_closed_reason 或 review_uncertain)`；
///   `approved`/`pending` 且带了隔离码时记 `quarantined(<码>)`，没有隔离码就不动。
/// - **fact/event 且 `verdicts=None`**：整批同一状态（`item_state==Applied ⇒ Done`，否则
///   `Quarantined(fail_closed_reason 或 review_uncertain)`），作用域与 [`mark_batch`] 同一条：
///   只写载荷里有草稿的轮，载荷不可读才回退整批。
/// - **fact/event 且 `verdicts=Some`**：按载荷 `items[i].turn_uid` 建 ref→轮，**同一轮多条
///   草稿先聚合、每轮只写一次**——任一 `uncertain ⇒ Quarantined(review_uncertain)`；否则任一
///   `pass`、或该轮在 `duplicate_uids` ⇒ `Done`；否则（全是 `reject`）⇒
///   `Rejected(review_reject)`。这与 runner 的逐轮终态逐条一致。
/// - **批内没有草稿的轮不动**（抽取阶段已经给过它自己的状态）。
pub fn mark_turns(
    l: &LedgerHandle,
    item: &Item,
    outcome: &RunOutcome,
    now_ms: i64,
) -> Result<(), StoreError> {
    // S3c-3b：rows 的终态不落 `extract_pass_state`（见 [`mark_batch`]）——先于任何
    // 载荷读数返回，`outcome` 不参与。波三刀 A：lesson 的终态由 `pass_lesson::mark_lesson_terminal`
    // 自己写，引擎这一层不重写，所以同样先返回。
    if item.meta.pass_id == ROWS_PASS || item.meta.pass_id == PASS_LESSON {
        return Ok(());
    }
    let uids = batch_turns(item)?;
    if uids.is_empty() {
        return Ok(());
    }
    if item.meta.pass_id == "naming" {
        let (state, reason) = match outcome.item_state {
            ReviewState::Applied => (PassState::Done, None),
            ReviewState::Rejected => (
                PassState::Rejected,
                Some(
                    item.meta
                        .program_code
                        .clone()
                        .unwrap_or_else(|| REVIEW_REJECT.to_string()),
                ),
            ),
            ReviewState::Quarantined => (
                PassState::Quarantined,
                Some(
                    outcome
                        .fail_closed_reason
                        .unwrap_or(REVIEW_UNCERTAIN)
                        .to_string(),
                ),
            ),
            // approved/pending：写库没发生也不是驳回；带隔离码（gap）时按隔离记，否则不动。
            _ => match outcome.fail_closed_reason {
                Some(reason) => (PassState::Quarantined, Some(reason.to_string())),
                None => return Ok(()),
            },
        };
        for uid in &uids {
            pass_state::mark_terminal(l, turn_key(item, uid), state, reason.as_deref(), now_ms)?;
        }
        return mark_program_rejected_turns(l, item, outcome, now_ms);
    }
    match &outcome.verdicts {
        None => {
            let (state, reason) = if outcome.item_state == ReviewState::Applied {
                (PassState::Done, None)
            } else {
                (
                    PassState::Quarantined,
                    Some(
                        outcome
                            .fail_closed_reason
                            .unwrap_or(REVIEW_UNCERTAIN)
                            .to_string(),
                    ),
                )
            };
            // §3：作用域只到载荷里有草稿的轮；载荷不可读才回退整批。
            let scope = match draft_turns(item) {
                Some(turns) => turns,
                None => uids,
            };
            for uid in &scope {
                pass_state::mark_terminal(
                    l,
                    turn_key(item, uid),
                    state,
                    reason.as_deref(),
                    now_ms,
                )?;
            }
        }
        Some(verdicts) => {
            let turns = payload_turns(item)?;
            // 同一轮可能收到多条草稿的判决（载荷是"逐草稿"，轮是共享键）：先按轮聚合，
            // 保持载荷里第一次出现的顺序，每轮只 mark 一次。
            let mut order: Vec<&str> = Vec::new();
            let mut merged: HashMap<&str, ReviewVerdict> = HashMap::new();
            for (reference, verdict) in verdicts {
                let uid = turn_of_ref(&turns, reference).ok_or_else(|| {
                    StoreError::Sql("review: verdict ref outside payload items".to_string())
                })?;
                match merged.get_mut(uid) {
                    Some(slot) => *slot = stronger(*slot, *verdict),
                    None => {
                        order.push(uid);
                        merged.insert(uid, *verdict);
                    }
                }
            }
            for uid in order {
                let verdict = merged[uid];
                let (state, reason) = if verdict == ReviewVerdict::Uncertain {
                    (PassState::Quarantined, Some(REVIEW_UNCERTAIN))
                } else if verdict == ReviewVerdict::Pass || outcome.duplicate_uids.contains(uid) {
                    (PassState::Done, None)
                } else {
                    (PassState::Rejected, Some(REVIEW_REJECT))
                };
                pass_state::mark_terminal(l, turn_key(item, uid), state, reason, now_ms)?;
            }
        }
    }
    mark_program_rejected_turns(l, item, outcome, now_ms)
}

/// I3：程序闸拒收的 draft 的证据轮落 `rejected(<码>)`。
///
/// 放在整批映射**之后**：批级映射把批内轮铺成同一个终态（naming 的 Applied ⇒ Done、
/// fact/event 的 reject ⇒ review_reject），而坏 draft 的轮不是那个终态——它是确定性的
/// 程序驳回，码就是该 draft 的失败码。引擎已剔除"被通过 draft 覆盖"的轮，所以这里
/// 不会把好草稿的轮写成 rejected（工单 §4）。
fn mark_program_rejected_turns(
    l: &LedgerHandle,
    item: &Item,
    outcome: &RunOutcome,
    now_ms: i64,
) -> Result<(), StoreError> {
    for (uid, code) in &outcome.program_rejected_turns {
        pass_state::mark_terminal(l, turn_key(item, uid), PassState::Rejected, Some(code), now_ms)?;
    }
    Ok(())
}

// ── PENDING_REVIEW ────────────────────────────────────────────────────────────

fn run_pending(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    l: &LedgerHandle,
    item: &Item,
    pass: Pass,
    backend: &dyn LlmBackend,
    now_ms: i64,
) -> Result<RunOutcome, RunError> {
    let mut replayed = match super::replay::replay(t, item) {
        Ok(replayed) => replayed,
        Err(ReplayFailure::Gap(_)) => {
            // 确定性结果，不是 Err：载荷不可重放 ⇒ 隔离整条 item，绝不当"全 pass"。
            let decision = super::decision(
                ReviewStage::Review,
                ReviewVerdict::Uncertain,
                PAYLOAD_GAP,
                "",
                "",
                &item.payload_sha256,
                "",
                Decider::Program,
                "",
            );
            let state = record_decision(m, item.meta.id, &decision, now_ms).map_err(mem_store)?;
            let mut out = RunOutcome::zero(state);
            out.fail_closed_reason = Some(PAYLOAD_GAP);
            return Ok(out);
        }
        Err(ReplayFailure::Store(e)) => return Err(RunError::Store(DistillError::Memory(e))),
    };
    // A7：送审前回填轮级说话人（mention 表）——审核模型判"谁说的"的轮级一轴。
    fill_turn_speakers(m, &mut replayed)?;

    // I3 第一刀：**判定单位是 draft**。批级前置（payload_shape / turn_missing /
    // turn_session_mismatch / turn_tombstoned / compaction_evidence）命中 ⇒ 这一批没有可用
    // 上下文，整条 item 隔离（不猜、不问模型、不写库）；逐 draft 失败 ⇒ 只把坏草稿剔出
    // 审核输入，并在 tally 里计 reject（复用既有 `review_mixed` 读数）。
    // rows 不是本闸门的输入（`parse_payload` 不认 `rows`），保持原路径。
    let gate = if matches!(pass, Pass::Rows | Pass::Lesson) {
        ProgramGate::default()
    } else {
        let report = gate_of(m, t, item, now_ms)?;
        if let Some(code) = report.batch_code {
            return quarantined_by_program(m, item, code, GATE_BATCH_DETAIL, now_ms);
        }
        let gate = ProgramGate::from_report(&report);
        // C3：闸已经判完，先给每一条被拒的 draft 留一行审计；本轮的处置（剔出审核输入、
        // tally 计 reject、逐轮终态）一个字不改。
        audit_program_rejects(
            m,
            item,
            &gate,
            backend.slug().unwrap_or("unknown"),
            prompt_version_of(pass),
            now_ms,
        )?;
        gate
    };

    let plan = match review_plan(m, &item.meta.session_key, pass, &replayed, &gate.failed_refs) {
        Ok(plan) => plan,
        // S3c-3b：rows 的渲染项要按行查主语锚（`anchor.canonical`）。查不到是**确定性缺口**，
        // 与 parser 失败同一条路径——记一条 `review/uncertain(review_ref_missing)`、不调模型、
        // 不写库；没有模型原文可落 artifact（缺口在开口之前就判掉了）。
        Err(RunError::ReviewRows(slug)) => {
            super::record_review(
                m,
                item.meta.id,
                ReviewVerdict::Uncertain,
                &slug,
                "",
                "",
                "",
                &item.payload_sha256,
                "",
                now_ms,
            )
            .map_err(mem_store)?;
            return Err(RunError::ReviewRows(slug));
        }
        Err(other) => return Err(other),
    };
    // H5a/H5b：**每一片**都先比尺寸；任一片超限 ⇒ 整 item 隔离，一片都不送。
    if let Some(out) = oversized_review_input(m, item, &plan, now_ms)? {
        return Ok(out);
    }

    let model = backend.slug().unwrap_or("unknown");
    let first_uid = replayed
        .batch
        .rows
        .first()
        .map(|row| row.turn_uid.clone())
        .unwrap_or_default();
    // 已送出的各片 user（按顺序）：分片合成指纹的输入，也是解析失败时那次判定的送审指纹。
    let mut users: Vec<String> = Vec::with_capacity(plan.chunks.len());
    // 全局 ref 的逐条判决：片内判决按 `chunk_offset + local_index` 映射回来后按片顺序拼。
    let mut parsed: Vec<(String, DraftVerdict, Option<String>)> = Vec::new();
    // G1 刀 B：全局 ref 的价值轴答案（只有**提出**这一轴的调用会写；fact 是 proposer，
    // event 是它那唯一一次）。没有答案的 ref 按 keep 处理（parser 的同一读法）。
    let mut value_axis: Vec<(String, ValueAxis)> = Vec::new();
    // 证据闸的程序侧降级（全局 ref）：只做读数，不落库（工单 §2.3）。
    let mut downgrades: Vec<(String, &'static str)> = Vec::new();
    // §7.3a：两侧原判（全局 ref + 原文证据），落库前只在这里攒；非 fact 恒空。
    let mut side_rows: Vec<SideVerdict> = Vec::new();
    for chunk in &plan.chunks {
        users.push(chunk.user.clone());
        // 工单 §7.2：fact 每片发**两次**调用——先 proposer、后 challenger，同一片、同一套 ref。
        // 其余 pass 一个字不动（单次调用）。
        if let (Pass::Fact(_), Some(challenger_system)) =
            (pass, plan.challenger_system.as_deref())
        {
            let proposer_raw = ask_review(backend, &plan.system, &chunk.user)?;
            let proposer = match parse_chunk(pass, &proposer_raw, chunk) {
                Ok(got) => Side::Parsed(got),
                Err(failure) => {
                    record_review_artifact(l, item, pass, &first_uid, &failure, &proposer_raw, now_ms);
                    Side::Failed(failure)
                }
            };
            // proposer 解析失败也照样发 challenger：反判若找到反证，这条就该 reject——
            // "不漏"优先于省一次调用（工单 §2.1）。
            let challenger_raw = ask_review(backend, challenger_system, &chunk.user)?;
            let challenger = match parse_chunk(pass, &challenger_raw, chunk) {
                Ok(got) => Side::Parsed(got),
                Err(failure) => {
                    record_review_artifact(l, item, pass, &first_uid, &failure, &challenger_raw, now_ms);
                    Side::Failed(failure)
                }
            };
            match (proposer, challenger) {
                (Side::Parsed(proposer), Side::Parsed(challenger)) => {
                    parsed.extend(combine_sides(chunk, &proposer.verdicts, &challenger.verdicts));
                    // 价值轴只认 proposer：challenger 的 prompt 不写这一格，它的回答
                    // 也不该覆盖价值判断（它只找"这条不成立"的原文）。
                    value_axis.extend(globalize_axis(proposer.value_axis.clone(), &chunk.globals));
                    collect_downgrades(chunk, &proposer.downgrades, &mut downgrades);
                    collect_downgrades(chunk, &challenger.downgrades, &mut downgrades);
                    // §7.3a：两侧的逐条判决各攒一份（含被证据闸降级的条目）。
                    side_verdicts(chunk, VerdictSide::Proposer, &proposer, &mut side_rows);
                    side_verdicts(chunk, VerdictSide::Challenger, &challenger, &mut side_rows);
                }
                // 两次都失败 ⇒ 全部 uncertain（与今天同一条路：artifact 已落、记一条
                // review/uncertain(reason)、整 item 交由调用方隔离）。
                (Side::Failed(proposer), Side::Failed(_)) => {
                    let review_input_sha256 = plan.fingerprint(model, &users);
                    let reason = proposer.reason();
                    super::record_review(
                        m,
                        item.meta.id,
                        ReviewVerdict::Uncertain,
                        &reason,
                        "",
                        model,
                        plan.prompt_version,
                        &item.payload_sha256,
                        &review_input_sha256,
                        now_ms,
                    )
                    .map_err(mem_store)?;
                    return Err(proposer.into_run_error());
                }
                // 只有一侧坏：
                // - proposer 坏 ⇒ 该侧没有答案；challenger 找到反证仍 **reject**（不漏，
                //   工单 §2.1/§4.4），challenger 没找到反证则不能补成 pass ⇒ uncertain；
                // - challenger 坏 ⇒ 该侧全部 uncertain ⇒ 按 §2.4 整条 uncertain。
                (proposer, challenger) => {
                    match (&proposer, &challenger) {
                        (Side::Failed(_), Side::Parsed(challenger)) => {
                            parsed.extend(challenger_only(chunk, &challenger.verdicts));
                        }
                        (Side::Parsed(_), Side::Failed(_)) => {
                            parsed.extend(all_uncertain_global(chunk));
                        }
                        _ => unreachable!("(Parsed, Parsed) 与 (Failed, Failed) 上面已收口"),
                    }
                    if let Side::Parsed(proposer) = &proposer {
                        // 价值轴只认 proposer（见上一条分支的同一句）。
                        value_axis
                            .extend(globalize_axis(proposer.value_axis.clone(), &chunk.globals));
                        collect_downgrades(chunk, &proposer.downgrades, &mut downgrades);
                        side_verdicts(chunk, VerdictSide::Proposer, proposer, &mut side_rows);
                    }
                    if let Side::Parsed(challenger) = &challenger {
                        collect_downgrades(chunk, &challenger.downgrades, &mut downgrades);
                        side_verdicts(chunk, VerdictSide::Challenger, challenger, &mut side_rows);
                    }
                }
            }
            continue;
        }

        let raw = ask_review(backend, &plan.system, &chunk.user)?;
        let got = match parse_chunk(pass, &raw, chunk) {
            Ok(got) => got,
            Err(failure) => {
                // 与 H5a 之前的单次口径逐条相同：先落**这一片**的原文（诊断补件，写失败
                // 不掩盖解析错误），再记 `uncertain(<reason>)`；整 item 交给调用方隔离。
                record_review_artifact(l, item, pass, &first_uid, &failure, &raw, now_ms);
                let review_input_sha256 = plan.fingerprint(model, &users);
                let reason = failure.reason();
                super::record_review(
                    m,
                    item.meta.id,
                    ReviewVerdict::Uncertain,
                    &reason,
                    "",
                    model,
                    plan.prompt_version,
                    &item.payload_sha256,
                    &review_input_sha256,
                    now_ms,
                )
                .map_err(mem_store)?;
                return Err(failure.into_run_error());
            }
        };
        parsed.extend(globalize(got.verdicts, &chunk.globals));
        // G1 刀 B：event 的价值轴（naming/rows/state 的 parser 不给这一格，`value_axis`
        // 恒空——空表不参与映射，也不假装"答了 keep"）。
        if !got.value_axis.is_empty() {
            value_axis.extend(globalize_axis(got.value_axis, &chunk.globals));
        }
    }

    // 逐条判决按**全部** draft 组装：程序闸拒收的 draft 已被剔出审核输入（`parsed` 里没有），
    // 在这里补成 Reject——顺序 = 载荷草稿顺序（写库层按这个顺序 zip 草稿）。
    let parsed_verdicts: HashMap<&str, DraftVerdict> = parsed
        .iter()
        .map(|(reference, verdict, _)| (reference.as_str(), *verdict))
        .collect();
    let pairs: Vec<(String, DraftVerdict)> = draft_refs(&replayed)
        .into_iter()
        .map(|reference| {
            let verdict = parsed_verdicts
                .get(reference.as_str())
                .copied()
                .unwrap_or(DraftVerdict::Reject);
            (reference, verdict)
        })
        .collect();
    // tally 含程序拒收的那几条（工单 §3：被程序拒的 draft 进入 reject 计数，复用
    // `review_mixed` 读数路径——有 pass 也有 reject 时聚合码就是 `review_mixed`）。
    let (p, r, u) = tally_pairs(&pairs);
    let (verdict, code) = aggregate(p, r, u);
    let (pass_refs, uncertain_refs) = refs_by_verdict(&plan.global_refs, &parsed);
    // G1 刀 B：价值轴判 demote 的全局 ref。**与 correctness 正交**：这里只回答"这条价值不高"，
    // 它是不是 pass 由上面那一支决定；被 reject 的行不落库，章也就无处可盖。
    let demote_refs: Vec<String> = value_axis
        .iter()
        .filter(|(_, axis)| *axis == ValueAxis::Demote)
        .map(|(reference, _)| reference.clone())
        .collect();
    let demote_set: HashSet<String> = demote_refs.iter().cloned().collect();
    let detail = review_detail(p, r, u, &pass_refs, &uncertain_refs, &demote_refs);
    let review_input_sha256 = plan.fingerprint(model, &users);
    let state = super::record_review(
        m,
        item.meta.id,
        queue_verdict(verdict),
        code,
        &detail,
        model,
        plan.prompt_version,
        &item.payload_sha256,
        &review_input_sha256,
        now_ms,
    )
    .map_err(mem_store)?;
    // §7.3a：逐条判决与证据落库（**只 fact**）。两侧原判各一行、裁决后 `final` 每草稿一行；
    // 写失败走 `RunError::Store`（`mem_store`），不吞。证据降级（缺证据/证据越界）那一行
    // 落 `uncertain` + `evidence_state=missing|not_in_quote`，`evidence` 是模型给的原文。
    if matches!(pass, Pass::Fact(_)) {
        write_item_verdicts(
            m,
            item,
            &replayed,
            &side_rows,
            &pairs,
            &parsed,
            model,
            plan.prompt_version,
            now_ms,
        )?;
    }
    let verdicts_out = pairs
        .iter()
        .map(|(reference, verdict)| (reference.clone(), queue_verdict(*verdict)))
        .collect::<Vec<_>>();
    // 两档合成的逐条 code（裁决矩阵读数）：全局 ref + 判决 + 胜出侧的 code。
    let adjudications: Vec<(String, ReviewVerdict, Option<String>)> = parsed
        .iter()
        .map(|(reference, verdict, code)| {
            (reference.clone(), queue_verdict(*verdict), code.clone())
        })
        .collect();

    if verdict != DraftVerdict::Pass {
        let mut out = RunOutcome::zero(state);
        out.verdicts = Some(verdicts_out);
        out.evidence_downgrades = downgrades;
        out.adjudications = adjudications;
        out.program_rejected_turns = gate.rejected_turns.clone();
        return Ok(out);
    }
    let mut out = write_rows(
        m,
        t,
        l,
        item,
        pass,
        &replayed,
        &pairs,
        backend,
        plan.prompt_version,
        // apply 判定记的是**载荷**的 sha（与 runner 的 `item_sha` 同格），不是送审输入哈希。
        &item.payload_sha256,
        now_ms,
        verdicts_out,
        &demote_set,
    )?;
    out.program_rejected_turns = gate.rejected_turns.clone();
    out.evidence_downgrades = downgrades;
    out.adjudications = adjudications;
    Ok(out)
}

// ── APPROVED ──────────────────────────────────────────────────────────────────

/// H2（P0-2）：程序侧的确定性缺口——记一条 `stage=program, verdict=uncertain`
/// 把 item 推到 `quarantined`，返回同形读数（`verdicts=None` + 隔离码）。
/// approved 上用于载荷不可重放/判决重建不出；H5a 起 pending 上的超长审核输入
/// （[`oversized_review_input`]）也走这一格——两处都是"不猜、不问模型、隔离可见"。
///
/// 为什么不是"什么也没发生"：`approved` + 载荷不可重放/判决重建不出时，调用方会逐轮写
/// `quarantined(<码>)`，而队列若留在 `approved`，remaining 与退出码都看不见这条 item
/// （审计 P0-2 的静默成功）。这条边由状态机放行（`program/uncertain` 从
/// `pending_review`/`approved` ⇒ `quarantined`）。
///
/// 判定的形状与程序闸其它裁定同一格：`decider=program`、`model`/`prompt_version` 空串、
/// `input_sha256` = 载荷指纹、`review_input_sha256` = 空串（没有送审输入）、
/// `rules_version` = gate 版本（[`super::decision`] 固定）。
fn quarantined_by_program(
    m: &MemoryHandle,
    item: &Item,
    reason: &'static str,
    detail: &'static str,
    now_ms: i64,
) -> Result<RunOutcome, RunError> {
    let decision = super::decision(
        ReviewStage::Program,
        ReviewVerdict::Uncertain,
        reason,
        "",
        "",
        &item.payload_sha256,
        "",
        Decider::Program,
        detail,
    );
    let state = record_decision(m, item.meta.id, &decision, now_ms).map_err(mem_store)?;
    let mut out = RunOutcome::zero(state);
    out.fail_closed_reason = Some(reason);
    Ok(out)
}

/// H5a：审核输入超过 [`REVIEW_INPUT_MAX_BYTES`] 的确定性处置（三个 pass 共用）。
///
/// H5b 起按**片**判：任一片的 `system.len() + user.len()`（UTF-8 字节）超限，就整 item
/// 隔离——不截断、一片都不问模型、不写库。返回 `Some(隔离读数)` 让调用方直接收尾。判定由
/// [`quarantined_by_program`] 记 `program/uncertain(REVIEW_INPUT_TOO_LARGE)`（detail 同词），
/// 把 item 推到 `quarantined`。每片都没超限 ⇒ `None`（调用方继续逐片送模型）。
fn oversized_review_input(
    m: &MemoryHandle,
    item: &Item,
    plan: &ReviewPlan,
    now_ms: i64,
) -> Result<Option<RunOutcome>, RunError> {
    let too_large = plan
        .chunks
        .iter()
        .any(|chunk| plan.system.len() + chunk.user.len() > REVIEW_INPUT_MAX_BYTES);
    if !too_large {
        return Ok(None);
    }
    quarantined_by_program(
        m,
        item,
        REVIEW_INPUT_TOO_LARGE,
        REVIEW_INPUT_TOO_LARGE,
        now_ms,
    )
    .map(Some)
}

fn run_approved(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    l: &LedgerHandle,
    item: &Item,
    pass: Pass,
    backend: &dyn LlmBackend,
    now_ms: i64,
) -> Result<RunOutcome, RunError> {
    let replayed = match super::replay::replay(t, item) {
        Ok(replayed) => replayed,
        // H2：组装不了写库参数 ⇒ 隔离整条 item（记 program/uncertain），不再返回
        // `Ok(Approved)`——那会让 hub 不计数、退 0，而 ledger 却已 quarantined。
        Err(ReplayFailure::Gap(_)) => {
            return quarantined_by_program(m, item, PAYLOAD_GAP, "approved_gap", now_ms)
        }
        Err(ReplayFailure::Store(e)) => return Err(RunError::Store(DistillError::Memory(e))),
    };
    // I3：判决虽是从台账重建的，**闸仍是硬门**——逐 draft 重算一次，程序拒收的那几条强制
    // Reject（human pass 也不放行）；批级前置命中 ⇒ 整条隔离，不写库。
    let gate = if matches!(pass, Pass::Rows | Pass::Lesson) {
        ProgramGate::default()
    } else {
        let report = gate_of(m, t, item, now_ms)?;
        if let Some(code) = report.batch_code {
            return quarantined_by_program(m, item, code, GATE_BATCH_DETAIL, now_ms);
        }
        ProgramGate::from_report(&report)
    };
    let draft_refs = draft_refs(&replayed);
    // 判决来源：最后一条 human pass ⇒ 全部 pass；否则从最后一条 review pass 判定的 detail 重建。
    let pairs = if last_decision_is_human_pass(m, item.meta.id)? {
        draft_refs
            .iter()
            .map(|reference| (reference.clone(), DraftVerdict::Pass))
            .collect::<Vec<_>>()
    } else {
        let seen = draft_refs.iter().map(String::as_str).collect::<Vec<_>>();
        match super::persisted_verdicts(m, item.meta.id, &seen).map_err(mem_store)? {
            Some(rebuilt) => draft_refs
                .iter()
                .map(|reference| {
                    let verdict = if rebuilt.pass.contains(reference) {
                        DraftVerdict::Pass
                    } else if rebuilt.uncertain.contains(reference) {
                        DraftVerdict::Uncertain
                    } else {
                        DraftVerdict::Reject
                    };
                    (reference.clone(), verdict)
                })
                .collect(),
            None => {
                // H2：判决重建不出（REVIEW_VERDICTS_MISSING）与载荷缺口同一条去向——
                // 记 program/uncertain 推 `quarantined`，不再返回 `Ok(Approved)`。
                return quarantined_by_program(
                    m,
                    item,
                    REVIEW_VERDICTS_MISSING,
                    "approved_verdicts_missing",
                    now_ms,
                );
            }
        }
    };
    // G1 刀 B：价值轴与对错轴**各判各的**——human 放行的是对不对，不是价值高不高，
    // 所以 value_demote 从**最近一条能解析的 review 判定**里取（[`super::latest_demote`]），
    // 不随 human pass 清零。没有任何可解析判定 ⇒ 空集（不盖章）。
    let value_demote = super::latest_demote(m, item.meta.id).map_err(mem_store)?;
    let verdicts_out = pairs
        .iter()
        .map(|(reference, draft_verdict)| (reference.clone(), queue_verdict(*draft_verdict)))
        .collect::<Vec<_>>();
    let prompt_version = prompt_version_of(pass);
    // C3：闸已经判完、判决也重建完了，再给每一条被拒的 draft 留一行审计——approved 是闸
    // **真的挡住写库**的那一格（human pass 也不放行），所以它的拒收同样要可查。
    // 放在重建**之后**：这一层的读者（human pass / 落库判决）不该看见本次刚写下的审计行。
    audit_program_rejects(
        m,
        item,
        &gate,
        backend.slug().unwrap_or("unknown"),
        prompt_version,
        now_ms,
    )?;
    // 闸是硬门：程序拒收的 draft 在这里也强制 Reject，只有通过的那几条会进组装。
    let pairs = gate.enforce(pairs);
    let mut out = write_rows(
        m,
        t,
        l,
        item,
        pass,
        &replayed,
        &pairs,
        backend,
        prompt_version,
        &item.payload_sha256,
        now_ms,
        verdicts_out,
        &value_demote,
    )?;
    out.program_rejected_turns = gate.rejected_turns.clone();
    Ok(out)
}

// ── APPLIED ───────────────────────────────────────────────────────────────────

/// APPLIED：判决逐条重建（重建不出就走 fail-closed），写库参数**照旧组装但不落库**。
///
/// 计数与旧 `run_fact`/`run_event` 的 `write_rows=false` 路径逐项同口径：
/// fact 走同一条跨 family + 批内去重循环（命中计 `duplicate`），event 把 pass 草稿计入
/// `written`；插入读数（facts/cards/mentions）恒 0，`applied_now` 恒 false。
/// naming 与旧 `run_naming` 的 Applied 早退同格：四格读数全 0（它连"组装"都不做，
/// 过滤器与写库参数只在真的写库那次有读数意义）。
fn run_applied(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    item: &Item,
    now_ms: i64,
) -> Result<RunOutcome, RunError> {
    let replayed = match super::replay::replay(t, item) {
        Ok(replayed) => replayed,
        // 已写库：判决重建不出（含载荷不可重放）走同一条 fail-closed，不猜。
        Err(ReplayFailure::Gap(_)) => {
            let mut out = RunOutcome::zero(ReviewState::Applied);
            out.fail_closed_reason = Some(REVIEW_VERDICTS_MISSING);
            return Ok(out);
        }
        Err(ReplayFailure::Store(e)) => return Err(RunError::Store(DistillError::Memory(e))),
    };
    // I3：只读重放也要知道"哪几条 draft 是程序拒收的"——调用方据此把它们的轮继续写成
    // `rejected(<码>)`（而不是被重放改回 Done/review_reject）。批级命中不改写已写库的 item。
    let pass = supported_pass(item)?;
    let gate = if matches!(pass, Pass::Rows | Pass::Lesson) {
        ProgramGate::default()
    } else {
        ProgramGate::from_report(&gate_of(m, t, item, now_ms)?)
    };
    let draft_refs = draft_refs(&replayed);
    let seen = draft_refs.iter().map(String::as_str).collect::<Vec<_>>();
    let rebuilt = super::persisted_verdicts(m, item.meta.id, &seen).map_err(mem_store)?;
    let mut out = RunOutcome::zero(ReviewState::Applied);
    let Some(rebuilt) = rebuilt else {
        out.fail_closed_reason = Some(REVIEW_VERDICTS_MISSING);
        return Ok(out);
    };
    let pairs: Vec<(String, DraftVerdict)> = draft_refs
        .iter()
        .map(|reference| {
            let verdict = if rebuilt.pass.contains(reference) {
                DraftVerdict::Pass
            } else if rebuilt.uncertain.contains(reference) {
                DraftVerdict::Uncertain
            } else {
                DraftVerdict::Reject
            };
            (reference.clone(), verdict)
        })
        .collect();
    // G1 刀 B：只组装不落库，但价值轴与写库那次必须同源（同一份 detail 重建）。
    let value_demote = rebuilt.demote.clone();
    let pairs = gate.enforce(pairs);
    out.program_rejected_turns = gate.rejected_turns.clone();
    out.verdicts = Some(
        pairs
            .iter()
            .map(|(reference, verdict)| (reference.clone(), queue_verdict(*verdict)))
            .collect(),
    );
    // 只组装、不写库：与 runner 的 `write_rows=false` 同一条循环（去重查询键不变）。
    match &replayed.drafts {
        ReplayDrafts::Fact(_) => {
            let fact = assemble_fact(m, &replayed, &pairs, &value_demote)?;
            out.written = fact.items.len();
            out.duplicate = fact.duplicate;
            out.duplicate_uids = fact.duplicate_uids;
        }
        ReplayDrafts::State(_) => {
            // 只组装、不写库：状态没有跨批去重那一格（幂等在写库层的第二道判）。
            let state = assemble_state(m, t, &replayed, &pairs)?;
            out.written = state.len();
        }
        ReplayDrafts::Event(_) => {
            let event = assemble_event(m, &replayed, &pairs, &value_demote)?;
            // 与 fact 同口径：重放时也把去重命中记进读数（C5b）。
            out.written = event.items.len();
            out.duplicate = event.duplicate;
            out.duplicate_uids = event.duplicate_uids;
            // A-rebind：重放的组装与写库那次同一个判据，所以这一格也读得到。
            out.actor_rebound = event.rebound;
        }
        // naming 的 Applied 早退读数与旧 `run_naming` 逐格一致：锚/提及都记 0
        // （写库那次已经报过数），`written`/`facts`/`cards`/`mentions` 全 0。
        ReplayDrafts::Naming(_) => {}
        // S3c-3b：rows 的 Applied 同样只重建判决（上面已填 `verdicts`），不写库、读数全 0
        // ——行值到列值的转换只在真的写库那次有读数意义。
        ReplayDrafts::Rows(_) => {}
        // 波三刀 A：lesson 的 Applied 与 rows 同格——判决已重建，写库那次的读数不再重现。
        ReplayDrafts::Lesson(_) => {}
    }
    Ok(out)
}

// ── 审核输入（H5b：≤REVIEW_MAX_ITEMS 条一片）─────────────────────────────────

/// 一片审核输入：user 只渲染本片条目（`ITEM f1..fk` 从片内 1 编号）。
struct ReviewChunk {
    user: String,
    /// 交给 parser 的**片内**短引用（`f1..fk` / `e1..ek` / `a1..ak`）。
    refs: Vec<String>,
    /// 与 `refs` 逐位对应的**全局** ref（载荷里草稿/锚的原始 reference）：
    /// 解析结果按位映射回来，`chunk_offset + local_index` 就是全局序号。
    globals: Vec<String>,
    /// fact 的证据边界（片内 ref + 该条草稿 quote）；其余 pass 为空。
    /// 工单 §7.2：没有 quote，parser 无法判断 `evidence` 是否落在引文里。
    targets: Vec<ReviewTarget>,
}

/// 一次 item 审核的分片计划：同一份 system/prompt 版本/pipeline，user 按片切开。
struct ReviewPlan {
    system: String,
    /// 反判（challenger）的 system；只有 fact 有（工单 §7.2）。两档用**同一片 user**：
    /// 两次调用看的是同一份 chunk、同一套 ref 编号。
    challenger_system: Option<String>,
    /// 抽取指纹的 pipeline 名（与 runner 同一枚：fact 带 family，event/naming 常量）。
    pipeline: String,
    prompt_version: &'static str,
    chunks: Vec<ReviewChunk>,
    /// 全部草稿/锚的全局 ref（按载荷顺序）：聚合计数、`review_detail` 与逐条判决用它。
    global_refs: Vec<String>,
}

impl ReviewPlan {
    /// 分片合成指纹。单片时与旧口径逐字节相同；多片时它是这一整次 item 审核
    /// 按顺序送出的全部 user 的指纹，**不单指某一次调用**。
    ///
    /// fact 自 §7.2 起送两次调用（proposer + challenger）：proposer system 仍是
    /// `extraction_fingerprint` 的第二格，challenger system 与各片 user 依次进第四格；
    /// 两档共用同一片 user，所以 user 只进一次。其余 pass 的算式一字不变。
    fn fingerprint(&self, model: &str, users: &[String]) -> String {
        let joined = users.join("\n\u{1e}\n");
        let input = match &self.challenger_system {
            Some(challenger) => format!("{challenger}\n\u{1e}\n{joined}"),
            None => joined,
        };
        water::extraction_fingerprint(&self.pipeline, model, &self.system, &input)
    }
}

/// 审核输入的批级信息（C6a）：会话/归属四格 + 与 `batch.rows` 逐位对应的说话人名。
///
/// 四格归属全部来自 review item（`session_key` 由调用方从 `item.meta` 取，
/// `owner_agent_id`/`visibility`/`scope` 取自重放的 `payload.owner`）——**不新增查询**。
///
/// 说话人名**复用 C1 的解析**，不另写一份：
/// - **轮级说话人优先**（A7 接线）：`row.speaker` 由引擎在 replay 后用
///   [`crate::batch::load_turn_speakers`] 从 `mention` 表回填（同一函数，抽取侧
///   `pass_event`/`pass_state` 同源）。它是库里唯一写着"这一轮是谁说的"的程序判定——
///   子 agent 会话里 role 推导给不出这一轴。空串/纯空白 = 没给（写入面
///   `writers_v4::record_turn_speakers` 同一口径），按没有这一格走 role 推导，
///   不出空前缀。名字仍只来自锚的 `canonical`，不拿 id 冒充名字。
/// - 其余轮：human ⇒ 主人锚（[`rc_memory::writers_v4::session_principal_anchor_id`]，即
///   `speaker_anchor_id` 对"证据里有 human"的那一支）的 `canonical`。没有主人身份、
///   锚查不到、或 canonical 只是 id（没有可读名）⇒ `unknown`（与派发档"claim 的
///   `principal_name`，无则 unknown"同口径）。
/// - agent/system 轮 ⇒ `source_agent`。它就是 C1 里会话自锚的 canonical
///   （`session_agent_anchor_id` 用同一个值算 id），所以两者天然同一个名字。
///
/// 渲染层只拿结果、不查库；这一层是唯一的"名字来源"。
pub fn review_context(
    m: &MemoryHandle,
    session_key: &str,
    batch: &Batch,
    owner: &OwnerCtx,
    scope: &Scope,
) -> Result<ReviewInputContext, RunError> {
    let principal = principal_speaker_name(m, owner, scope)?;
    let agent = batch.source_agent.trim();
    let speakers = batch
        .rows
        .iter()
        .map(|row| {
            // A7：轮级说话人优先；空白按没有这一格处理（不出垃圾前缀），回落 role 推导。
            if let Some(speaker) =
                row.speaker.as_deref().map(str::trim).filter(|speaker| !speaker.is_empty())
            {
                return speaker.to_string();
            }
            match AnchorRole::parse(&row.role) {
                Some(AnchorRole::Human) => principal.clone(),
                _ => {
                    if agent.is_empty() {
                        ReviewInputContext::UNKNOWN_SPEAKER.to_string()
                    } else {
                        batch.source_agent.clone()
                    }
                }
            }
        })
        .collect();
    Ok(ReviewInputContext {
        session_key: session_key.to_string(),
        owner_agent_id: owner.owner_agent_id.clone(),
        visibility: visibility_slug(owner.visibility).to_string(),
        scope: format!("{}:{}", scope.kind.as_str(), scope.key),
        speakers,
    })
}

/// human 轮的可读说话人名：主人锚的 `canonical`（C1 的身份轴）。
///
/// §10.50：没有 claim（或锚/可读名缺）时**回退到与抽取侧同一个判据**——scope 内唯一的
/// `kind='person'` 锚就是说话人。此前这里一律 `unknown`，审核模型因此把"示例主人"判成
/// `missing_subject`（抽查里 7 条好草稿 6 条被拒），而抽取侧明明解析出 `speaker=a5 示例主人`。
/// 名字仍只来自锚的 `canonical`：不拿 id 冒充名字，也不拿 agent 名顶替。
fn principal_speaker_name(
    m: &MemoryHandle,
    owner: &OwnerCtx,
    scope: &Scope,
) -> Result<String, RunError> {
    if let Some(name) = claimed_principal_name(m, owner, scope)? {
        return Ok(name);
    }
    let candidates = crate::candidates::load_candidates(m, &owner.owner_agent_id, scope)
        .map_err(RunError::Store)?;
    let persons: Vec<&crate::pass_prompt::FactCandidate> =
        candidates.iter().filter(|candidate| candidate.kind == "person").collect();
    if persons.len() == 1 {
        // 回退到"scope 内唯一 person 锚"时同样给**显示名**：主人锚的正名是账号主键，
        // 直接把 canonical 递给审核输入就会露出 id。
        return rc_memory::reader::display_name(m, &persons[0].anchor_id)
            .map_err(|e| RunError::Store(DistillError::Memory(e)));
    }
    Ok(ReviewInputContext::UNKNOWN_SPEAKER.to_string())
}

/// 有 claim 时的主人名：**显示名**（`reader::display_name` = 最新有效别名，取不到才回落 canonical），
/// 且只有当它不等于 `principal_id` 本身时才算"有可读名"。
///
/// 旧规则比的是 `anchor.canonical`——那时 canonical 就是章上的可读名。"改名不换锚"之后
/// canonical 恒等于账号主键、可读名住在 `alias`，所以这一格必须换成显示名，
/// 否则审核输入里的说话人会退回一串 id。
fn claimed_principal_name(
    m: &MemoryHandle,
    owner: &OwnerCtx,
    scope: &Scope,
) -> Result<Option<String>, RunError> {
    let Some(principal_id) = owner.owner_principal_id.as_deref() else {
        return Ok(None);
    };
    let anchor_id = rc_memory::writers_v4::session_principal_anchor_id(m, principal_id, scope)
        .map_err(mem_store)?;
    let Some(anchor_id) = anchor_id else {
        return Ok(None);
    };
    let shown = rc_memory::reader::display_name(m, &anchor_id).map_err(mem_store)?;
    if shown.trim().is_empty() || shown.trim() == principal_id.trim() {
        return Ok(None);
    }
    Ok(Some(shown))
}

/// 与 runner 第二次调用逐字节一致：同一组 `pass_prompt::*_review_*` 与同一个指纹式。
/// H5b：按 [`REVIEW_MAX_ITEMS`] 条切段，每段一片。
///
/// I3：`skip` 是**程序闸已拒收**的 draft ref（fact `f*` / event `e*` / naming 锚 `a*`）——
/// 它们不进审核输入（不问模型、不可能被模型放行），判决由调用方补成 Reject，
/// 且计入 tally 的 reject 计数。`rows` 不走这道闸（`skip` 对它为空）。
///
/// `m` 只有 rows 用：逐行按 `subject_anchor_id` 查 `anchor.canonical`（重放只校验行能
/// 重建，不查锚）；其余 pass 的渲染不需要库。
fn review_plan(
    m: &MemoryHandle,
    session_key: &str,
    pass: Pass,
    replayed: &Replay,
    skip: &HashSet<String>,
) -> Result<ReviewPlan, RunError> {
    match (pass, &replayed.drafts) {
        (Pass::Fact(family), ReplayDrafts::Fact(drafts)) => {
            let system = fact_review_system(family);
            let ctx = review_context(m, session_key, &replayed.batch, &replayed.owner, &replayed.scope)?;
            // 程序闸拒收的草稿先剔掉；编号由**剩下的**位置决定（片内 `f1..fk`，parser 的
            // refs 与 ITEM 同一套），`globals` 把它们映射回载荷里的原始 ref。
            let kept: Vec<&FactDraft> = drafts
                .iter()
                .filter(|draft| !skip.contains(&draft.reference))
                .collect();
            let chunks = kept
                .chunks(REVIEW_MAX_ITEMS)
                .map(|chunk| {
                    let local: Vec<FactDraft> =
                        chunk.iter().map(|draft| (*draft).clone()).collect();
                    let refs: Vec<String> =
                        (1..=chunk.len()).map(|index| format!("f{index}")).collect();
                    let targets = refs
                        .iter()
                        .zip(local.iter())
                        .map(|(reference, draft)| {
                            ReviewTarget::new(reference.clone(), draft.quote.clone())
                        })
                        .collect();
                    ReviewChunk {
                        user: fact_review_user(family, &local, &replayed.batch, &ctx),
                        refs,
                        globals: chunk.iter().map(|draft| draft.reference.clone()).collect(),
                        targets,
                    }
                })
                .collect();
            Ok(ReviewPlan {
                system,
                challenger_system: Some(fact_review_challenge_system(family)),
                pipeline: format!("{FACT_PIPELINE_PREFIX}/{}", family.slug()),
                prompt_version: PASS_FACT_REVIEW_VERSION,
                chunks,
                global_refs: kept.iter().map(|draft| draft.reference.clone()).collect(),
            })
        }
        (Pass::Event, ReplayDrafts::Event(drafts)) => {
            let system = event_review_system();
            let ctx = review_context(m, session_key, &replayed.batch, &replayed.owner, &replayed.scope)?;
            let kept: Vec<_> = drafts
                .iter()
                .filter(|draft| !skip.contains(&draft.reference))
                .collect();
            let chunks = kept
                .chunks(REVIEW_MAX_ITEMS)
                .map(|chunk| {
                    let local: Vec<_> = chunk.iter().map(|draft| (*draft).clone()).collect();
                    ReviewChunk {
                        user: event_review_user(&local, &replayed.batch, &ctx),
                        refs: (1..=chunk.len()).map(|index| format!("e{index}")).collect(),
                        globals: chunk.iter().map(|draft| draft.reference.clone()).collect(),
                        targets: Vec::new(),
                    }
                })
                .collect();
            Ok(ReviewPlan {
                system,
                challenger_system: None,
                pipeline: EVENT_PIPELINE.to_string(),
                prompt_version: PASS_EVENT_REVIEW_VERSION,
                chunks,
                global_refs: kept.iter().map(|draft| draft.reference.clone()).collect(),
            })
        }
        (Pass::State, ReplayDrafts::State(drafts)) => {
            let system = state_review_system();
            let ctx = review_context(m, session_key, &replayed.batch, &replayed.owner, &replayed.scope)?;
            let kept: Vec<_> = drafts
                .iter()
                .filter(|draft| !skip.contains(&draft.reference))
                .collect();
            let chunks = kept
                .chunks(REVIEW_MAX_ITEMS)
                .map(|chunk| {
                    let local: Vec<_> = chunk.iter().map(|draft| (*draft).clone()).collect();
                    ReviewChunk {
                        user: state_review_user(&local, &replayed.batch, &ctx),
                        refs: (1..=chunk.len()).map(|index| format!("s{index}")).collect(),
                        globals: chunk.iter().map(|draft| draft.reference.clone()).collect(),
                        targets: Vec::new(),
                    }
                })
                .collect();
            Ok(ReviewPlan {
                system,
                challenger_system: None,
                pipeline: STATE_PIPELINE.to_string(),
                prompt_version: PASS_STATE_REVIEW_VERSION,
                chunks,
                global_refs: kept.iter().map(|draft| draft.reference.clone()).collect(),
            })
        }
        (Pass::Naming, ReplayDrafts::Naming(a1)) => {
            let system = naming_review_system();
            // 子批只含该片锚与其提及（`anchor_ref` 命中片内）；TURNS 仍是整批的轮，
            // issues/unresolved 不参与 `naming_review_user` 的渲染，也不属于某一片。
            // I3：程序闸拒收的锚同样剔出（其 mention 也不进渲染——`anchor_ref` 命中不了）。
            let kept: Vec<_> = a1
                .anchors
                .iter()
                .filter(|anchor| !skip.contains(&anchor.reference))
                .collect();
            let chunks = kept
                .chunks(REVIEW_MAX_ITEMS)
                .map(|chunk| {
                    let local = A1Batch {
                        anchors: chunk.iter().map(|anchor| (*anchor).clone()).collect(),
                        mentions: a1
                            .mentions
                            .iter()
                            .filter(|mention| {
                                chunk.iter().any(|anchor| anchor.reference == mention.anchor_ref)
                            })
                            .cloned()
                            .collect(),
                        issues: Vec::new(),
                        unresolved: Vec::new(),
                    };
                    ReviewChunk {
                        user: naming_review_user(&local, &replayed.batch),
                        refs: (1..=chunk.len()).map(|index| format!("a{index}")).collect(),
                        globals: chunk.iter().map(|anchor| anchor.reference.clone()).collect(),
                        targets: Vec::new(),
                    }
                })
                .collect();
            Ok(ReviewPlan {
                system,
                challenger_system: None,
                pipeline: NAMING_PIPELINE.to_string(),
                prompt_version: PASS_NAMING_REVIEW_VERSION,
                chunks,
                // naming 的审核只裁锚：全局 ref 是 `a1..aN`（位置），与 ANCHOR 编号同源。
                global_refs: kept.iter().map(|anchor| anchor.reference.clone()).collect(),
            })
        }
        (Pass::Rows, ReplayDrafts::Rows(drafts)) => {
            let system = rows_review_system();
            // 渲染项按行位置编号（`r1..rN`）；主语 canonical 现查锚（查不到 ⇒ 隔离码）。
            let items = rows_review_items(m, replayed, drafts)?;
            let chunks = items
                .chunks(REVIEW_MAX_ITEMS)
                .enumerate()
                .map(|(chunk_index, chunk)| {
                    // 片内重新编号 `r1..rk`（`rows_review_user` 只看位置），解析结果按
                    // `chunk_index * REVIEW_MAX_ITEMS + local_index` 映射回全局 `r`。
                    let offset = chunk_index * REVIEW_MAX_ITEMS;
                    let local: Vec<RowReviewItem> = chunk
                        .iter()
                        .enumerate()
                        .map(|(index, item)| RowReviewItem {
                            reference: format!("r{}", index + 1),
                            ..item.clone()
                        })
                        .collect();
                    ReviewChunk {
                        user: rows_review_user(&local),
                        refs: (1..=chunk.len()).map(|index| format!("r{index}")).collect(),
                        globals: (0..chunk.len())
                            .map(|index| format!("r{}", offset + index + 1))
                            .collect(),
                        targets: Vec::new(),
                    }
                })
                .collect();
            Ok(ReviewPlan {
                system,
                challenger_system: None,
                pipeline: ROWS_PIPELINE.to_string(),
                prompt_version: ROWS_REVIEW_VERSION,
                chunks,
                // 全局 ref 是 `r1..rN`（位置），与 `draft_refs` 的 rows 分支同一套。
                global_refs: (1..=items.len()).map(|index| format!("r{index}")).collect(),
            })
        }
        (Pass::Lesson, ReplayDrafts::Lesson(drafts)) => {
            let system = lesson_review_system();
            let ctx = review_context(m, session_key, &replayed.batch, &replayed.owner, &replayed.scope)?;
            // 程序闸不跑（见 `run_pending` 的 `Pass::Lesson` 一支），`skip` 恒空；
            // 仍按同一形状过滤一次，免得将来有人给 lesson 接了闸而这里悄悄漏剔。
            let kept: Vec<_> = drafts
                .iter()
                .filter(|draft| !skip.contains(&draft.reference))
                .collect();
            // 一整批只有一族错因（选批按 (会话, 错误族) 分组），取第一条即可。
            let error_code = kept
                .first()
                .map(|draft| draft.error_code.clone())
                .unwrap_or_else(|| REVIEW_REJECT_CODE.to_string());
            let chunks = kept
                .chunks(REVIEW_MAX_ITEMS)
                .map(|chunk| {
                    let local: Vec<_> = chunk.iter().map(|draft| (*draft).clone()).collect();
                    ReviewChunk {
                        user: lesson_review_user(&local, &replayed.batch, &ctx, &error_code),
                        refs: (1..=chunk.len()).map(|index| format!("l{index}")).collect(),
                        globals: chunk.iter().map(|draft| draft.reference.clone()).collect(),
                        targets: Vec::new(),
                    }
                })
                .collect();
            Ok(ReviewPlan {
                system,
                challenger_system: None,
                pipeline: LESSON_PIPELINE.to_string(),
                prompt_version: PASS_LESSON_REVIEW_VERSION,
                chunks,
                global_refs: kept.iter().map(|draft| draft.reference.clone()).collect(),
            })
        }
        // 分派与重放读的是同一个 `pass_id`，这一格不可达。
        _ => Err(RunError::Unsupported),
    }
}

/// rows 的渲染项（S3c-3b）：一行 fact 行 → 一条 [`RowReviewItem`]。
///
/// - `text`/`category`/`predicate` 取行值（`text` 由重放保证非空；`category`/`predicate` 缺失时
///   按空串渲染，闭集/非空由 `gate_rows` 判，本层不重判）；
/// - `subject_canonical` 按 `subject_anchor_id` 现查 `anchor.canonical`：行值缺键或锚查不到
///   ⇒ [`REVIEW_REF_MISSING`]（调用方记 `uncertain` 并隔离，与 parser 失败同一条路径）；
/// - `evidence_body` 取重放批里 `evidence_turn_uid` 那一轮的正文（重放已保证它在批内；
///   读不到是数据不一致，按 `UnlistedUid` 收，不猜一个空证据）；
/// - `conflict` 取载荷 `conflict.value_text`（无冲突或缺这一格 ⇒ 整行 `conflict:` 不渲染）。
fn rows_review_items(
    m: &MemoryHandle,
    replayed: &Replay,
    drafts: &[RowsDraft],
) -> Result<Vec<RowReviewItem>, RunError> {
    let mut items = Vec::with_capacity(drafts.len());
    for draft in drafts {
        let subject_anchor_id = draft
            .values
            .get("subject_anchor_id")
            .and_then(Value::as_str)
            .ok_or_else(ref_missing)?;
        let subject_canonical = anchor_display(m, subject_anchor_id)?.ok_or_else(ref_missing)?;
        let evidence_body = replayed
            .batch
            .row(&draft.evidence_turn_uid)
            .map(|row| row.body.clone())
            .ok_or_else(|| {
                contract_store(ContractError::UnlistedUid {
                    uid: draft.evidence_turn_uid.clone(),
                })
            })?;
        items.push(RowReviewItem {
            // 编号由位置决定，与 `rows_review_user` 的 `ROW r{n}` 同一套纪律。
            reference: format!("r{}", items.len() + 1),
            text: draft
                .values
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            subject_canonical: Some(subject_canonical),
            category: draft
                .values
                .get("category")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            // S3c-fix：谓词是"同一主语同一谓词"判据的另一半，必须进 prompt（复审 P1-b）。
            // 缺键按空串渲染（闸已保证非空；本层不重判）。
            predicate: draft
                .values
                .get("predicate")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            evidence_body,
            conflict: draft
                .conflict
                .as_ref()
                .and_then(|conflict| conflict.get("value_text"))
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    Ok(items)
}

/// rows 的主语锚查不到时的错误面（判定已在调用方记好）。
fn ref_missing() -> RunError {
    RunError::ReviewRows(REVIEW_REF_MISSING.to_string())
}

/// 按 id 读锚的**显示名**（[`rc_memory::reader::display_name`]：最新有效别名，取不到才回落 canonical）；
/// `None` = 锚表里没有这一行。SQL 错误才是 [`RunError::Store`]。
///
/// 为什么审核输入取显示名而不是 `canonical`：主人锚的 canonical 现在是账号主键
/// （"改名不换锚"），把 id 露进审核文本等于让审核模型拿一串哈希判"主语在不在证据里"。
fn anchor_display(m: &MemoryHandle, anchor_id: &str) -> Result<Option<String>, RunError> {
    let exists: i64 = m.conn().query_row(
        "SELECT count(*) FROM anchor WHERE id=?1",
        [anchor_id],
        |r| r.get(0),
    ).map_err(|e| RunError::Store(DistillError::Memory(MemError::from(e))))?;
    if exists == 0 {
        return Ok(None);
    }
    rc_memory::reader::display_name(m, anchor_id)
        .map(Some)
        .map_err(|e| RunError::Store(DistillError::Memory(e)))
}

/// 一片的解析失败：解析器给出的原因（slug/中文原因码）+ 返回给调用方的错误面。
///
/// 三个 pass 的 parser 各有一套错误类型（naming/rows 只有 slug），统一到这一格只是为了让
/// "落这片的 artifact → 记一条 `uncertain(<reason>)` → 整 item 隔离"在循环里只写一遍。
enum ChunkFailure {
    Fact(FactPassError),
    Event(EventPassError),
    Naming(String),
    Rows(String),
    State(StatePassError),
    Lesson(LessonPassError),
}

impl ChunkFailure {
    /// 落 `extract_pass_artifact` 的 `pass_id`。
    fn pass_id(&self) -> &'static str {
        match self {
            ChunkFailure::Fact(_) => PASS_FACT,
            ChunkFailure::Event(_) => PASS_EVENT,
            ChunkFailure::State(_) => PASS_STATE,
            ChunkFailure::Naming(_) => crate::pass_naming::NAMING_PASS,
            // rows 没有 `extract_pass_state` 的 pass，但 artifact 只是一份诊断补件：
            // 用载荷自己的 pass 字面量，读它的运维面不必猜。
            ChunkFailure::Rows(_) => ROWS_PASS,
            // 波三刀 A：lesson 的终态行由 `pass_lesson::mark_lesson_terminal` 自己写；artifact 用载荷的 pass 字面量。
            ChunkFailure::Lesson(_) => PASS_LESSON,
        }
    }

    /// artifact 的 family：只有 fact 有；event/naming 恒空串（与 runner 同格）。
    fn family(&self, pass: Pass) -> &str {
        match (self, pass) {
            (ChunkFailure::Fact(_), Pass::Fact(family)) => family.slug(),
            _ => "",
        }
    }

    /// 判定 `reason_code` 与 artifact 诊断用同一个原因码。
    ///
    /// fact/event 的 `review_reason()` 已自带 `review_` 前缀；naming/rows 的 parser 只有裸
    /// slug，前缀在这里补（[`review_slug_reason`]）——**这是唯一一处**拼前缀：判定的 reason
    /// 与交给调用方的 `ReviewNaming`/`ReviewRows` 装的是同一个串，runner/hub 原样沿用，
    /// 不再各自拼。
    fn reason(&self) -> String {
        match self {
            ChunkFailure::Fact(e) => e.review_reason(),
            ChunkFailure::Event(e) => e.review_reason(),
            ChunkFailure::State(e) => e.review_reason(),
            ChunkFailure::Lesson(e) => e.review_reason(),
            ChunkFailure::Naming(slug) | ChunkFailure::Rows(slug) => review_slug_reason(slug),
        }
    }

    /// 调用方拿到的错误面（与 H5b 之前的单次口径逐条相同）。
    fn into_run_error(self) -> RunError {
        match self {
            ChunkFailure::Fact(e) => RunError::Review(DistillError::Fact(e)),
            ChunkFailure::Event(e) => RunError::Review(DistillError::Event(e)),
            ChunkFailure::State(e) => RunError::Review(DistillError::State(e)),
            ChunkFailure::Lesson(e) => RunError::ReviewLesson(e),
            // naming/rows 没有自己的 `DistillError` 契约变体：只装 slug（已带 `review_` 前缀）。
            ChunkFailure::Naming(slug) => RunError::ReviewNaming(review_slug_reason(&slug)),
            ChunkFailure::Rows(slug) => RunError::ReviewRows(review_slug_reason(&slug)),
        }
    }
}

/// naming/rows 窄审解析失败的隔离码：`review_` + parser 的裸 slug（H6，与 fact/event 同前缀）。
///
/// 例：`json` → `review_json`、`verdict_code` → `review_verdict_code`、
/// `code_on_pass` → `review_code_on_pass`。判定的 `reason_code` 与返回给调用方的
/// [`RunError::ReviewNaming`]/[`RunError::ReviewRows`] 都从这里取，两侧不会分叉。
fn review_slug_reason(slug: &str) -> String {
    if slug.starts_with("review_") {
        slug.to_string()
    } else {
        format!("review_{slug}")
    }
}

/// 按片内短引用调对应 parser。parser 的 `refs` 就是这一片渲染时用的编号。
///
/// fact 自 §7.2 起还带**证据闸**：parser 拿到的是片内 ref + 该条 quote（[`ReviewChunk::targets`]），
/// 所以返回里多了被降级的条目；其余 pass 的 `downgrades` 恒空。
struct ChunkParsed {
    verdicts: Vec<(String, DraftVerdict, Option<String>)>,
    downgrades: Vec<(String, EvidenceDowngrade)>,
    /// §7.3a：`ref`（片内）→ 模型给的 `evidence` 原文。只有 fact 非空——其余 pass 的审核
    /// 回复没有这个键。
    evidence: HashMap<String, Option<String>>,
    /// G1 刀 B：逐条价值轴（片内 ref + 答案）。fact 与 event 有这一格；naming/rows/state
    /// 的 prompt 不问价值轴，恒空。缺键 ⇒ `keep`（parser 的读法），不是"没答"。
    value_axis: Vec<(String, ValueAxis)>,
}

impl ChunkParsed {
    fn plain(verdicts: Vec<(String, DraftVerdict, Option<String>)>) -> Self {
        ChunkParsed {
            verdicts,
            downgrades: Vec::new(),
            evidence: HashMap::new(),
            value_axis: Vec::new(),
        }
    }
}

fn parse_chunk(
    pass: Pass,
    raw: &str,
    chunk: &ReviewChunk,
) -> Result<ChunkParsed, ChunkFailure> {
    let refs = &chunk.refs;
    match pass {
        Pass::Fact(family) => {
            let report: FactReviewReport = parse_fact_review(raw, &chunk.targets, family)
                .map_err(ChunkFailure::Fact)?;
            Ok(ChunkParsed {
                verdicts: report.verdicts,
                downgrades: report.downgrades,
                evidence: review_evidence(raw),
                value_axis: report.value_axis,
            })
        }
        // G1 刀 B：event 也问价值轴，这一支收完整读数（判决 + 轴）。
        Pass::Event => parse_event_review_full(raw, refs)
            .map(|report| ChunkParsed {
                verdicts: report.verdicts,
                downgrades: Vec::new(),
                evidence: HashMap::new(),
                value_axis: report.value_axis,
            })
            .map_err(ChunkFailure::Event),
        Pass::Naming => parse_naming_review(raw, refs)
            .map(ChunkParsed::plain)
            .map_err(|e| ChunkFailure::Naming(e.slug().to_string())),
        // S3c-3b：rows 的 parser 与 naming 同形——只有封闭 slug，没有正文回显。
        Pass::Rows => parse_rows_review(raw, refs)
            .map(ChunkParsed::plain)
            .map_err(|e| ChunkFailure::Rows(e.slug().to_string())),
        Pass::State => parse_state_review(raw, refs)
            .map(ChunkParsed::plain)
            .map_err(ChunkFailure::State),
        // 波三刀 A：lesson 的 parser 与 naming/rows 同形——逐条判决 + 闭集码，没有正文回显。
        Pass::Lesson => parse_lesson_review(raw, refs)
            .map(ChunkParsed::plain)
            .map_err(ChunkFailure::Lesson),
    }
}

/// 片内判决映射回全局 ref：parser 按 `refs` 顺序返回，与 `globals` 逐位对齐
/// （`chunk_offset + local_index` 的同一件事）。
fn globalize(
    got: Vec<(String, DraftVerdict, Option<String>)>,
    globals: &[String],
) -> Vec<(String, DraftVerdict, Option<String>)> {
    debug_assert_eq!(got.len(), globals.len(), "parser 必须逐条回答 refs");
    got.into_iter()
        .zip(globals.iter())
        .map(|((_, verdict, code), reference)| (reference.clone(), verdict, code))
        .collect()
}

/// G1 刀 B：价值轴同样按位置映射回全局 ref（parser 的轴读数与 `refs` 逐位对齐）。
fn globalize_axis(got: Vec<(String, ValueAxis)>, globals: &[String]) -> Vec<(String, ValueAxis)> {
    debug_assert_eq!(got.len(), globals.len(), "parser 必须逐条回答 refs 的价值轴");
    got.into_iter()
        .zip(globals.iter())
        .map(|((_, axis), reference)| (reference.clone(), axis))
        .collect()
}

/// 一次审核开口。LLM 故障原样上抛：item 留 pending，不记判定、不写库、不落 artifact
/// （已花掉的前几片调用也不写判定——判定是整 item 的一条，不许半份落库）。
fn ask_review(
    backend: &dyn LlmBackend,
    system: &str,
    user: &str,
) -> Result<String, RunError> {
    backend
        .complete(&Prompt {
            system: system.to_string(),
            user: user.to_string(),
            derive: false,
        })
        .map_err(RunError::Llm)
}

/// 解析失败的那一片原文落 artifact（诊断补件，写失败不掩盖解析错误）。
fn record_review_artifact(
    l: &LedgerHandle,
    item: &Item,
    pass: Pass,
    first_uid: &str,
    failure: &ChunkFailure,
    raw: &str,
    now_ms: i64,
) {
    let _ = pass_state::record_artifact(
        l,
        failure.pass_id(),
        failure.family(pass),
        "review",
        &artifact_run_id(l, item, first_uid),
        first_uid,
        raw,
        now_ms,
    );
}

/// 一片里的一侧：解析成功（逐条判决）或解析失败。
enum Side {
    Parsed(ChunkParsed),
    Failed(ChunkFailure),
}

/// 一侧坏掉时它对**全部** ref 的答案：`uncertain`（沿用 `review_uncertain` 语义）。
fn all_uncertain_global(chunk: &ReviewChunk) -> Vec<(String, DraftVerdict, Option<String>)> {
    chunk
        .globals
        .iter()
        .map(|global| (global.clone(), DraftVerdict::Uncertain, None))
        .collect()
}

/// proposer 解析失败、challenger 有答复：只有 challenger 找到反证才 reject（不漏），
/// 其余一律 uncertain——一侧没有答案，绝不许因此变成 pass（工单 §2.1/§4.4）。
fn challenger_only(
    chunk: &ReviewChunk,
    challenger: &[(String, DraftVerdict, Option<String>)],
) -> Vec<(String, DraftVerdict, Option<String>)> {
    debug_assert_eq!(challenger.len(), chunk.globals.len(), "challenger 必须逐条回答 refs");
    chunk
        .globals
        .iter()
        .enumerate()
        .map(|(index, global)| {
            let (_, verdict, code) = &challenger[index];
            if *verdict == DraftVerdict::Reject {
                (global.clone(), DraftVerdict::Reject, code.clone())
            } else {
                (global.clone(), DraftVerdict::Uncertain, None)
            }
        })
        .collect()
}

/// 两档的逐条裁决（工单 §2.4，不许改成"少数服从多数"或"任一侧 pass 就算 pass"）：
///
/// - 任一侧 `uncertain`（含被证据闸降级的）⇒ `uncertain`；
/// - 否则 challenger `reject` ⇒ `reject`（取 challenger 的 code）；
/// - 否则 proposer `reject` ⇒ `reject`（取 proposer 的 code）；
/// - 否则两侧都 `pass` ⇒ `pass`（code null）。
fn decide_pair(
    proposer: DraftVerdict,
    proposer_code: &Option<String>,
    challenger: DraftVerdict,
    challenger_code: &Option<String>,
) -> (DraftVerdict, Option<String>) {
    if proposer == DraftVerdict::Uncertain || challenger == DraftVerdict::Uncertain {
        (DraftVerdict::Uncertain, None)
    } else if challenger == DraftVerdict::Reject {
        (DraftVerdict::Reject, challenger_code.clone())
    } else if proposer == DraftVerdict::Reject {
        (DraftVerdict::Reject, proposer_code.clone())
    } else {
        (DraftVerdict::Pass, None)
    }
}

/// 两档的逐条判决合成全局 ref：两侧按 refs 顺序返回，与 `globals` 逐位对齐。
fn combine_sides(
    chunk: &ReviewChunk,
    proposer: &[(String, DraftVerdict, Option<String>)],
    challenger: &[(String, DraftVerdict, Option<String>)],
) -> Vec<(String, DraftVerdict, Option<String>)> {
    debug_assert_eq!(proposer.len(), chunk.globals.len(), "proposer 必须逐条回答 refs");
    debug_assert_eq!(challenger.len(), chunk.globals.len(), "challenger 必须逐条回答 refs");
    chunk
        .globals
        .iter()
        .enumerate()
        .map(|(index, global)| {
            let (_, proposer_verdict, proposer_code) = &proposer[index];
            let (_, challenger_verdict, challenger_code) = &challenger[index];
            let (verdict, code) = decide_pair(
                *proposer_verdict,
                proposer_code,
                *challenger_verdict,
                challenger_code,
            );
            (global.clone(), verdict, code)
        })
        .collect()
}

/// 证据降级按片内 ref 映射回全局 ref（读数用；不落库）。
fn collect_downgrades(
    chunk: &ReviewChunk,
    side: &[(String, EvidenceDowngrade)],
    out: &mut Vec<(String, &'static str)>,
) {
    for (reference, reason) in side {
        if let Some(index) = chunk.refs.iter().position(|local| local == reference) {
            out.push((chunk.globals[index].clone(), reason.as_str()));
        }
    }
}

// ── §7.3a 逐条判决与证据落库（只 fact）─────────────────────────────────────────

/// 从**已过 parser** 的审核原文里取出每条判决的 `evidence` 原文（片内 ref 键）。
///
/// parser（[`parse_fact_review`]）只回判决与降级原因，不回 `evidence` 字面——而 §7.3a 要把
/// 模型给的原文**原样**落库（不 trim、不改写）。这里用同一把剥壳剪刀
/// （[`crate::pass_fact_contract::json_payload`]）再读一遍那份刚被 parser 收下的文档，
/// 只取值、不做任何判定：形状/闭集/去重/证据闸全由 parser 执法。
///
/// 读不出（同一份字符串两次解析分叉）⇒ 空表：判决照落，那一行的 `evidence` 为空、
/// `evidence_state` 落到 `none`——落库后的两列因此永远自洽，不会出现"有状态没原文"。
fn review_evidence(raw: &str) -> HashMap<String, Option<String>> {
    let mut out = HashMap::new();
    let Ok(doc) = serde_json::from_str::<Value>(crate::pass_fact_contract::json_payload(raw)) else {
        return out;
    };
    let Some(items) = doc.get("verdicts").and_then(Value::as_array) else {
        return out;
    };
    for item in items {
        let Some(reference) = item.get("ref").and_then(Value::as_str) else {
            continue;
        };
        let evidence = match item.get("evidence") {
            Some(Value::String(text)) => Some(text.clone()),
            _ => None,
        };
        out.insert(reference.to_string(), evidence);
    }
    out
}

/// 一条草稿在**某一侧**的原判：§7.3a 落库前的形状（还差草稿坐标，写库时补）。
struct SideVerdict {
    /// 全局 ref（载荷里的 `f1`…）。
    reference: String,
    side: VerdictSide,
    verdict: DraftVerdict,
    code: Option<String>,
    /// 模型给的原文；`None` 与 `Some("")` 是两件事，原样带下去。
    evidence: Option<String>,
    evidence_state: EvidenceState,
}

/// 把一侧的逐条判决按片内 ref 映射回全局 ref，并贴上证据闸的裁定。
///
/// `evidence_state` 只由两样东西决定，不重算证据：
/// - parser 已降级 ⇒ `missing`/`not_in_quote`（[`EvidenceDowngrade`] 的同一格）；
/// - 没降级且带着原文 ⇒ `ok`；没降级也没原文 ⇒ `none`（`uncertain` 允许不带证据）。
fn side_verdicts(
    chunk: &ReviewChunk,
    side: VerdictSide,
    got: &ChunkParsed,
    out: &mut Vec<SideVerdict>,
) {
    debug_assert_eq!(got.verdicts.len(), chunk.globals.len(), "一侧必须逐条回答 refs");
    for (index, (local_ref, verdict, code)) in got.verdicts.iter().enumerate() {
        let evidence = got.evidence.get(local_ref).cloned().flatten();
        let downgrade = got
            .downgrades
            .iter()
            .find(|(reference, _)| reference == local_ref)
            .map(|(_, reason)| *reason);
        let evidence_state = match (downgrade, evidence.as_deref()) {
            (Some(EvidenceDowngrade::Missing), _) => EvidenceState::Missing,
            (Some(EvidenceDowngrade::NotInQuote), _) => EvidenceState::NotInQuote,
            (None, Some(_)) => EvidenceState::Ok,
            (None, None) => EvidenceState::Absent,
        };
        out.push(SideVerdict {
            reference: chunk.globals[index].clone(),
            side,
            verdict: *verdict,
            code: code.clone(),
            evidence,
            evidence_state,
        });
    }
}

/// 草稿坐标：全局 ref → (载荷 `items[]` 下标, 该草稿所属轮)。只有 fact 有草稿坐标。
fn draft_slots(replayed: &Replay) -> HashMap<String, (i64, String)> {
    match &replayed.drafts {
        ReplayDrafts::Fact(drafts) => drafts
            .iter()
            .enumerate()
            .map(|(index, draft)| (draft.reference.clone(), (index as i64, draft.turn_uid.clone())))
            .collect(),
        _ => HashMap::new(),
    }
}

/// §7.3a：把这次 fact 审核的逐条判决落进 `review_verdict`——两侧原判各一行、`final` 一行。
///
/// 调用点在 `record_review` **之后**：判定台账是系统的账，这一张是它的证据附件，先记判定
/// 再记证据；写失败原样折成 [`RunError::Store`]，不吞。`final` 行的 `code` 取裁决出来的
/// code（[`decide_pair`] 的胜出侧），`evidence` 留空。
///
/// 只给 fact 调（`pass` 是 fact 才进得来）：event/naming/rows 一个字不写。
#[allow(clippy::too_many_arguments)]
fn write_item_verdicts(
    m: &MemoryHandle,
    item: &Item,
    replayed: &Replay,
    sides: &[SideVerdict],
    pairs: &[(String, DraftVerdict)],
    adjudicated: &[(String, DraftVerdict, Option<String>)],
    model: &str,
    prompt_version: &str,
    now_ms: i64,
) -> Result<(), RunError> {
    let slots = draft_slots(replayed);
    let codes: HashMap<&str, &Option<String>> = adjudicated
        .iter()
        .map(|(reference, _, code)| (reference.as_str(), code))
        .collect();
    let mut rows: Vec<NewVerdict> = Vec::with_capacity(sides.len() + pairs.len());
    for side in sides {
        // 坐标查不到只可能是"审核输入与载荷对不上"——那一条在 `review_plan` 就被挡掉了，
        // 这里兜底跳过（不落一条没有轮/下标的行）。
        let Some((draft_index, turn_uid)) = slots.get(&side.reference) else {
            debug_assert!(false, "{} 不在载荷草稿里", side.reference);
            continue;
        };
        rows.push(NewVerdict {
            pass_id: item.meta.pass_id.clone(),
            turn_uid: turn_uid.clone(),
            reference: side.reference.clone(),
            draft_index: *draft_index,
            side: side.side,
            verdict: queue_verdict(side.verdict),
            code: side.code.clone(),
            evidence: side.evidence.clone(),
            evidence_state: side.evidence_state,
            prompt_version: prompt_version.to_string(),
            model: model.to_string(),
        });
    }
    for (reference, verdict) in pairs {
        let Some((draft_index, turn_uid)) = slots.get(reference) else {
            debug_assert!(false, "{reference} 不在载荷草稿里");
            continue;
        };
        rows.push(NewVerdict {
            pass_id: item.meta.pass_id.clone(),
            turn_uid: turn_uid.clone(),
            reference: reference.clone(),
            draft_index: *draft_index,
            side: VerdictSide::Final,
            verdict: queue_verdict(*verdict),
            code: codes.get(reference.as_str()).and_then(|code| (*code).clone()),
            evidence: None,
            evidence_state: EvidenceState::Absent,
            prompt_version: prompt_version.to_string(),
            model: model.to_string(),
        });
    }
    replace_item_verdicts(m, item.meta.id, &rows, now_ms).map_err(mem_store)?;
    Ok(())
}


// ── §7.3b：uncertain 草稿的**单条重跑** ────────────────────────────────────────
//
// 首审（proposer）与反判（challenger）都判 uncertain 的草稿，**单独出列、单独重问一次**，
// 用比正常审核更足的上下文（`pass_prompt::fact_review_retry_user` 的五个信息段）。程序
// 记指标，不接受模型自报。三条边界：
//
// 1. **队列只认一件事**：`state='quarantined'` 且 `pass_id='fact'`，且 `review_verdict`
//    里有 `side='final' AND verdict='uncertain'` 的行（§7.3a 落的那张表）。缺逐条判决行的
//    隔离项（例如审核解析失败的那一批）**不进队列**——它没有"要重跑的那一条"。
// 2. **item 终态由程序合成**（不靠模型）：把该 item **全部**草稿的 final 判决取齐（重跑的
//    用新结果，其余用 §7.3a 的旧 final；旧 final 缺行按 uncertain，fail-closed），然后
//    任一条 reject ⇒ rejected（code 取那条的）；全部 pass ⇒ applied（走既有 apply 路径写
//    事实行）；仍有 uncertain ⇒ 保持 quarantined（reason 仍是 `review_uncertain`）。
// 3. **指标只由程序数**：见 [`RetryMetrics`]。落 `review_decision` 的 `detail`（一行计数）
//    与 `--json` 是同一组数。
//
// 与 normal 审核共用的硬门一个字不改：载荷指纹复核（H4a）、重放、程序闸（批级命中不重跑、
// 逐 draft 命中强制 reject）、证据闸（`parse_fact_review_retry` 复用同一份 `review_ruling`）、
// 审核输入字节上限（H5a）。LLM 故障不记判定、不写库（item 留在 quarantined 等下一次）。

/// §7.3b 的一次重跑读数：**程序数出来的**，不接受模型自报。
///
/// 四个率的分母写清（派发档 §2.4）：
/// - `uncertain_rate` = [`RetryMetrics::prescan_uncertain`] / [`RetryMetrics::drafts_total`]，
///   即"重跑前 final=uncertain 的草稿数 / 本次队列域内 item 的载荷草稿总数"；
/// - `retry_pass_rate` / `retry_reject_rate` / `retry_still_uncertain_rate` 的分母都是
///   [`RetryMetrics::retried`]（真的重问过的草稿数），分子分别是重跑判 pass / reject /
///   uncertain 的条数。解析失败按"重跑仍判不出来"计入 `retry_uncertain`（另计 `parse_failures`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetryMetrics {
    /// 真的重跑过（并落了合成判定）的 item 数。
    pub items: usize,
    /// 本次队列域内 item 的载荷草稿总数（`uncertain_rate` 的分母）。
    pub drafts_total: usize,
    /// 重跑前 `final=uncertain` 的草稿数（`uncertain_rate` 的分子）。
    pub prescan_uncertain: usize,
    /// 真的重问过的草稿数（三个 `retry_*_rate` 的分母）。
    pub retried: usize,
    pub retry_pass: usize,
    pub retry_reject: usize,
    /// 重跑后仍 uncertain 的条数（含解析失败那几条）。
    pub retry_uncertain: usize,
    /// 重跑里证据缺失（`evidence_missing`）被降级的条数。
    pub evidence_missing: usize,
    /// 重跑里证据越界（`evidence_not_in_quote`）被降级的条数。
    pub evidence_not_in_quote: usize,
    /// 重跑回复解析失败的条数（计入 `retry_uncertain`，另计这一格）。
    pub parse_failures: usize,
    /// 合成终态落在 applied / rejected / quarantined 的 item 数。
    pub applied: usize,
    pub rejected: usize,
    pub quarantined: usize,
    /// 没跑成的条数（LLM 故障、载荷缺口、闸批级命中、输入超限…）：这些 item 零写入。
    pub errors: usize,
    /// 队列竞态（另一写者已推进状态）的条数。
    pub conflicts: usize,
    /// provider 额度耗尽 ⇒ 本轮提前停下。
    pub exhausted: bool,
    /// 本轮结束后仍有 `final=uncertain` 行的隔离 item 数（`--limit` 截断或重跑仍判不出来）。
    pub remaining_retryable: i64,
}

impl RetryMetrics {
    /// 四个率按**四个小数位**取整（整数运算：`(num*10000 + den/2) / den`；分母 0 ⇒ 0）。
    pub fn rate_bp(num: usize, den: usize) -> u32 {
        if den == 0 {
            return 0;
        }
        ((num.saturating_mul(10_000) + den / 2) / den) as u32
    }

    /// 与 [`Self::rate_bp`] 同一格的小数形（`0.3333`）。
    pub fn rate_str(num: usize, den: usize) -> String {
        let bp = Self::rate_bp(num, den);
        format!("{}.{:04}", bp / 10_000, bp % 10_000)
    }

    pub fn uncertain_rate(&self) -> f64 {
        f64::from(Self::rate_bp(self.prescan_uncertain, self.drafts_total)) / 10_000.0
    }

    pub fn retry_pass_rate(&self) -> f64 {
        f64::from(Self::rate_bp(self.retry_pass, self.retried)) / 10_000.0
    }

    pub fn retry_reject_rate(&self) -> f64 {
        f64::from(Self::rate_bp(self.retry_reject, self.retried)) / 10_000.0
    }

    pub fn retry_still_uncertain_rate(&self) -> f64 {
        f64::from(Self::rate_bp(self.retry_uncertain, self.retried)) / 10_000.0
    }

    /// 一次重跑（一个 item）的计数累加进总量。
    fn merge(&mut self, one: RetryMetrics) {
        self.items += one.items;
        self.drafts_total += one.drafts_total;
        self.prescan_uncertain += one.prescan_uncertain;
        self.retried += one.retried;
        self.retry_pass += one.retry_pass;
        self.retry_reject += one.retry_reject;
        self.retry_uncertain += one.retry_uncertain;
        self.evidence_missing += one.evidence_missing;
        self.evidence_not_in_quote += one.evidence_not_in_quote;
        self.parse_failures += one.parse_failures;
        self.applied += one.applied;
        self.rejected += one.rejected;
        self.quarantined += one.quarantined;
    }

    /// 单条重跑的判决计数（`retried` 在调用前已 +1）。
    fn tally(&mut self, verdict: DraftVerdict) {
        match verdict {
            DraftVerdict::Pass => self.retry_pass += 1,
            DraftVerdict::Reject => self.retry_reject += 1,
            DraftVerdict::Uncertain => self.retry_uncertain += 1,
        }
    }

    /// 落 `review_decision.detail` 的一行：只放码与计数（≤512 字符，守卫 12 的零正文）。
    ///
    /// 前缀 `retry=1` 是这一格的标识：`stage` 的闭集（`program|review|apply|human`）与
    /// `ReviewStage` 枚举都在 `rc-memory`/`rc-store`（本派发档的白名单之外），`stage='retry'`
    /// 在结构上表达不出来；重跑的判定因此落 `stage='review'` + `prompt_version=
    /// PASS_FACT_RETRY_VERSION` + 本函数的 `retry=1;` 前缀（交付报告里登记了这一格）。
    pub fn detail(&self) -> String {
        format!(
            "retry=1;pre_u={};pre_n={};u_rate={};retried={};p={};r={};u={};p_rate={};r_rate={};\
             su_rate={};ev_missing={};ev_not_in_quote={};parse_fail={}",
            self.prescan_uncertain,
            self.drafts_total,
            Self::rate_str(self.prescan_uncertain, self.drafts_total),
            self.retried,
            self.retry_pass,
            self.retry_reject,
            self.retry_uncertain,
            Self::rate_str(self.retry_pass, self.retried),
            Self::rate_str(self.retry_reject, self.retried),
            Self::rate_str(self.retry_uncertain, self.retried),
            self.evidence_missing,
            self.evidence_not_in_quote,
            self.parse_failures,
        )
    }
}

/// §7.3b 的重跑队列：`state='quarantined'`、`pass_id='fact'`，且 `review_verdict` 里
/// 有 `side='final' AND verdict='uncertain'` 的行。
///
/// `limit` 按 **item** 计数（与 `review run` 同口径；0 = 不设上限）。缺逐条判决行的隔离项
/// 不进队列——没有"要重跑的那一条"，重跑就没有输入。
pub fn retry_queue(m: &MemoryHandle, limit: usize) -> Result<Vec<i64>, RunError> {
    let items = rc_memory::review::list(m, Some(ReviewState::Quarantined), Some("fact"), 0)
        .map_err(mem_store)?;
    let mut out: Vec<i64> = Vec::new();
    for meta in items {
        if limit != 0 && out.len() >= limit {
            break;
        }
        let uncertain = item_verdicts_filtered(
            m,
            meta.id,
            Some(VerdictSide::Final),
            Some(ReviewVerdict::Uncertain),
        )
        .map_err(mem_store)?;
        if !uncertain.is_empty() {
            out.push(meta.id);
        }
    }
    Ok(out)
}

/// §7.3b：把队列里每一条 item 的 uncertain 草稿各重问一次，并按程序规则合成 item 终态。
///
/// 出口语义：
/// - `Ok(metrics)`：`errors`/`conflicts` 只是计数，调用方按它们决定退出码；
/// - `Err(RunError::MissingItem)`：队列读到、取行时没了（并发删），本次已写的照旧落库；
/// - 其余 Err 不出现：单条 item 的 LLM 故障/缺口/超限都折成 `errors` 继续下一条，
///   存储失败折成 `errors` 并**立即停**（不再级联写别的 item）。
pub fn retry_uncertain(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    l: &LedgerHandle,
    backend: &dyn LlmBackend,
    now_ms: i64,
    limit: usize,
) -> Result<RetryMetrics, RunError> {
    let mut total = RetryMetrics::default();
    for item_id in retry_queue(m, limit)? {
        let item = match get(m, item_id).map_err(mem_store)? {
            Some(item) => item,
            None => {
                total.errors += 1;
                continue;
            }
        };
        match retry_item(m, t, l, &item, backend, now_ms) {
            Ok(one) => total.merge(one),
            // LLM 故障：不记判定、不写库（item 留在 quarantined 等下一次）。额度耗尽即停。
            Err(RunError::Llm(failure)) => {
                total.errors += 1;
                if failure == LlmFailure::QuotaExhausted {
                    total.exhausted = true;
                    break;
                }
            }
            // 另一写者已推进状态/非法转换：本次零写入，不改 ledger。
            Err(RunError::Conflict) => total.conflicts += 1,
            // 存储/事务失败：不再碰下一条（避免级联），计 error 让退出码看得见。
            Err(RunError::Store(_)) => {
                total.errors += 1;
                break;
            }
            // 这条 item 现在不能重跑（载荷缺口、闸批级命中、输入超限、没有可重跑的草稿）：
            // 零写入，计 error 继续下一条——静默跳过会让隔离项永远没人看。
            Err(_) => total.errors += 1,
        }
    }
    total.remaining_retryable = retry_queue(m, 0)?.len() as i64;
    Ok(total)
}

/// 一条 item 的重跑：逐条重问 → 合成 → 落判定 → （全 pass 时）走既有 apply 路径。
fn retry_item(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    l: &LedgerHandle,
    item: &Item,
    backend: &dyn LlmBackend,
    now_ms: i64,
) -> Result<RetryMetrics, RunError> {
    // H4a：载荷指纹必须先自洽；不一致 = 这一行已不是入队时那份载荷 ⇒ 零写入。
    if !payload_verified(item).map_err(mem_store)? {
        return Err(RunError::Unsupported);
    }
    let mut replayed = match super::replay::replay(t, item) {
        Ok(replayed) => replayed,
        // 重放不出来就没有"这一条草稿的上下文"，重跑没有输入面可言。
        Err(ReplayFailure::Gap(_)) => return Err(RunError::Unsupported),
        Err(ReplayFailure::Store(e)) => return Err(RunError::Store(DistillError::Memory(e))),
    };
    // A7：重跑的送审输入与窄审同一份信息段——说话人同样要回填。
    fill_turn_speakers(m, &mut replayed)?;
    let drafts: &[FactDraft] = match &replayed.drafts {
        ReplayDrafts::Fact(drafts) => drafts.as_slice(),
        _ => return Err(RunError::Unsupported),
    };
    let family = family_of_slug(&item.meta.family).ok_or(RunError::Unsupported)?;
    // 程序闸仍是硬门（与 normal 同一条）：批级命中 ⇒ 这一批没有可用上下文，不重跑；
    // 逐 draft 命中 ⇒ 那条草稿强制 reject、码取闸的码。
    let report = gate_of(m, t, item, now_ms)?;
    if report.batch_code.is_some() {
        return Err(RunError::Unsupported);
    }
    let gate = ProgramGate::from_report(&report);
    // §7.3a 的旧 final 判决：重跑的草稿换新结果，其余照旧。
    let old = final_verdicts(m, item.meta.id)?;
    let targets: Vec<&FactDraft> = drafts
        .iter()
        .filter(|draft| {
            old.get(&draft.reference)
                .is_some_and(|(verdict, _)| *verdict == ReviewVerdict::Uncertain)
        })
        .collect();
    // 队列已按"有 final=uncertain 行"选过；空 ⇒ 两次读之间被改写，零写入。
    if targets.is_empty() {
        return Err(RunError::Unsupported);
    }
    // C3：§7.3b 的重跑也是一次程序闸判定（被闸拒的草稿一律 reject，码取闸的）——同一条
    // 审计口径；重跑不改写闸的结论，所以这一步与 normal 路径同一份 helper。
    audit_program_rejects(
        m,
        item,
        &gate,
        backend.slug().unwrap_or("unknown"),
        PASS_FACT_RETRY_VERSION,
        now_ms,
    )?;
    let mut metrics = RetryMetrics {
        items: 1,
        drafts_total: drafts.len(),
        prescan_uncertain: targets.len(),
        ..RetryMetrics::default()
    };
    let ctx = review_context(
        m,
        &item.meta.session_key,
        &replayed.batch,
        &replayed.owner,
        &replayed.scope,
    )?;
    // ⑤ 候选锚全表：与抽取同一个池（可见性过滤同一份），并按"名字出现在本轮正文"重排
    // （只重排、不删）。这一步只读库，不改任何格。
    let mut candidates =
        crate::candidates::load_candidates(m, &replayed.owner.owner_agent_id, &replayed.scope)
            .map_err(RunError::Store)?;
    crate::candidates::order_by_evidence(m, &mut candidates, &replayed.batch.rows)
        .map_err(RunError::Store)?;

    let system = fact_review_retry_system();
    let model = backend.slug().unwrap_or("unknown");
    let mut users: Vec<String> = Vec::with_capacity(targets.len());
    let mut fresh: HashMap<String, (DraftVerdict, Option<String>)> = HashMap::new();
    for draft in &targets {
        let user = fact_review_retry_user(family, draft, &replayed.batch, &ctx, &candidates);
        // H5a 的同一条字节闸：超限不截断、不问模型（这条 item 零写入，等人工拆批）。
        if system.len() + user.len() > REVIEW_INPUT_MAX_BYTES {
            return Err(RunError::Unsupported);
        }
        let raw = ask_review(backend, &system, &user)?;
        metrics.retried += 1;
        match parse_fact_review_retry(&raw, &draft.reference, &draft.quote) {
            Ok(ruling) => {
                // `note`（缺哪类信息）的唯一落点：这一次调用的模型原文。`review_verdict`
                // 没有这一列，§7.3a 的表结构本片不动；程序只存不判分。
                record_retry_artifact(l, item, family, &draft.turn_uid, &raw, now_ms);
                match ruling.downgrade {
                    Some(EvidenceDowngrade::Missing) => metrics.evidence_missing += 1,
                    Some(EvidenceDowngrade::NotInQuote) => metrics.evidence_not_in_quote += 1,
                    None => {}
                }
                metrics.tally(ruling.verdict);
                fresh.insert(draft.reference.clone(), (ruling.verdict, ruling.code));
            }
            // 解析失败：这条草稿的重跑**没有判决** ⇒ 记 uncertain（不把整条 item 判死，
            // 也不许当 pass）。原文落 artifact（诊断补件）。
            Err(_failure) => {
                record_retry_artifact(l, item, family, &draft.turn_uid, &raw, now_ms);
                metrics.parse_failures += 1;
                metrics.retry_uncertain += 1;
                fresh.insert(draft.reference.clone(), (DraftVerdict::Uncertain, None));
            }
        }
        users.push(user);
    }

    // 合成：**全部**草稿的 final 判决取齐。重跑的用新结果，其余用旧 final；旧 final 缺行
    // （数据不一致）按 uncertain（fail-closed，不许当 pass）。程序闸拒收的草稿一律 reject。
    let mut merged: Vec<(String, DraftVerdict, Option<String>)> = Vec::with_capacity(drafts.len());
    for draft in drafts {
        let (verdict, code) = match fresh.get(&draft.reference) {
            Some((verdict, code)) => (*verdict, code.clone()),
            None => match old.get(&draft.reference) {
                Some((verdict, code)) => (draft_verdict(*verdict), code.clone()),
                None => (DraftVerdict::Uncertain, None),
            },
        };
        match gate.code_of(&draft.reference) {
            Some(code) => merged.push((
                draft.reference.clone(),
                DraftVerdict::Reject,
                Some(code.to_string()),
            )),
            None => merged.push((draft.reference.clone(), verdict, code)),
        }
    }
    let any_reject = merged
        .iter()
        .any(|(_, verdict, _)| *verdict == DraftVerdict::Reject);
    let all_pass = merged
        .iter()
        .all(|(_, verdict, _)| *verdict == DraftVerdict::Pass);
    let detail = metrics.detail();
    // 判定的 `review_input_sha256`：与 normal 同一条指纹式，pipeline 多一格 `/retry`
    // （这次调用问的不是整批，是"这些草稿的第二次单独审"）。
    let review_input_sha256 = water::extraction_fingerprint(
        &format!("{FACT_PIPELINE_PREFIX}/{}/retry", family.slug()),
        model,
        &system,
        &users.join("\n\u{1e}\n"),
    );
    if any_reject {
        // 任一条 reject ⇒ item rejected；code 取**第一条** reject 的（载荷顺序）。
        let code = merged
            .iter()
            .find(|(_, verdict, _)| *verdict == DraftVerdict::Reject)
            .and_then(|(_, _, code)| code.clone())
            .unwrap_or_else(|| REVIEW_REJECT.to_string());
        let state = super::record_review(
            m,
            item.meta.id,
            ReviewVerdict::Reject,
            &code,
            &detail,
            model,
            PASS_FACT_RETRY_VERSION,
            &item.payload_sha256,
            &review_input_sha256,
            now_ms,
        )
        .map_err(mem_store)?;
        let mut outcome = RunOutcome::zero(state);
        outcome.verdicts = Some(queue_verdicts(&merged));
        outcome.program_rejected_turns = gate.rejected_turns.clone();
        finish_retry(
            m,
            l,
            item,
            &fresh,
            drafts,
            model,
            &outcome,
            &mut metrics,
            now_ms,
        )?;
        return Ok(metrics);
    }
    if !all_pass {
        // 仍有 uncertain（重跑后）⇒ item 保持 quarantined，reason 仍是 `review_uncertain`。
        let state = super::record_review(
            m,
            item.meta.id,
            ReviewVerdict::Uncertain,
            REVIEW_UNCERTAIN,
            &detail,
            model,
            PASS_FACT_RETRY_VERSION,
            &item.payload_sha256,
            &review_input_sha256,
            now_ms,
        )
        .map_err(mem_store)?;
        let mut outcome = RunOutcome::zero(state);
        outcome.verdicts = Some(queue_verdicts(&merged));
        outcome.program_rejected_turns = gate.rejected_turns.clone();
        finish_retry(
            m,
            l,
            item,
            &fresh,
            drafts,
            model,
            &outcome,
            &mut metrics,
            now_ms,
        )?;
        return Ok(metrics);
    }
    // 全部 pass ⇒ 先记一条 review/pass 把 item 推到 approved（状态机的既有边），再走
    // 既有的 apply 路径写事实行（`write_rows`：去重、卡、提及、provenance 一个字不改）。
    super::record_review(
        m,
        item.meta.id,
        ReviewVerdict::Pass,
        REVIEW_PASS,
        &detail,
        model,
        PASS_FACT_RETRY_VERSION,
        &item.payload_sha256,
        &review_input_sha256,
        now_ms,
    )
    .map_err(mem_store)?;
    let pairs: Vec<(String, DraftVerdict)> = merged
        .iter()
        .map(|(reference, verdict, _)| (reference.clone(), *verdict))
        .collect();
    // G1 刀 B：重跑只问对错，价值轴沿用前一次判定记下的答案（见 `latest_demote`）。
    let value_demote = super::latest_demote(m, item.meta.id).map_err(mem_store)?;
    let mut outcome = write_rows(
        m,
        t,
        l,
        item,
        Pass::Fact(family),
        &replayed,
        &pairs,
        backend,
        PASS_FACT_RETRY_VERSION,
        &item.payload_sha256,
        now_ms,
        queue_verdicts(&merged),
        &value_demote,
    )?;
    outcome.program_rejected_turns = gate.rejected_turns.clone();
    finish_retry(
        m,
        l,
        item,
        &fresh,
        drafts,
        model,
        &outcome,
        &mut metrics,
        now_ms,
    )?;
    Ok(metrics)
}

/// 重跑的收尾（三条终态共用）：逐轮终态 + §7.3a 表的 `final` 行改写 + 读数落格。
#[allow(clippy::too_many_arguments)]
fn finish_retry(
    m: &MemoryHandle,
    l: &LedgerHandle,
    item: &Item,
    fresh: &HashMap<String, (DraftVerdict, Option<String>)>,
    drafts: &[FactDraft],
    model: &str,
    outcome: &RunOutcome,
    metrics: &mut RetryMetrics,
    now_ms: i64,
) -> Result<(), RunError> {
    match outcome.item_state {
        ReviewState::Applied => metrics.applied += 1,
        ReviewState::Rejected => metrics.rejected += 1,
        _ => metrics.quarantined += 1,
    }
    // 逐轮终态与 `hub review run` 同一条映射（同一份 `mark_turns`）。
    mark_turns(l, item, outcome, now_ms).map_err(|_| {
        RunError::Store(DistillError::Memory(MemError::Sql(
            "review: retry mark_turns failed".to_string(),
        )))
    })?;
    // S4：重跑到 applied 也是一次 apply（`write_rows` 刚写过行与卡），所以卡的提升与
    // `hub review run`、内联窄审同一条口径——同一份 [`promote_applied_cards`]。
    // **失败不改变重跑结果**：行与逐轮终态都已落，上抛只会让 `hub review retry` 把一次
    // 已成立的 apply 报成失败；卡停在 `unreviewed` 仍由 audit 兜底（与上面 `mark_turns`
    // 之后的记账口同一把尺子：账没记上不等于账错了）。
    let _ = promote_applied_cards(m, outcome, now_ms);
    rewrite_final_verdicts(m, item, fresh, drafts, model, now_ms)
}

/// §7.3a 的表：把重跑那几条的 `final` 行换成新结果，其余行**原样保留**。
///
/// `replace_item_verdicts` 是整条 item 覆盖，所以先读全表、改那几行、再写回——两侧原判
/// （proposer/challenger 的证据）不会因重跑而丢失。`final` 行仍不带 `evidence`（与 §7.3a
/// 同一条：证据留在两侧原判与重跑的 artifact 里）。旧表缺该草稿的 final 行时补一行
/// （坐标按载荷草稿的顺序取）。
fn rewrite_final_verdicts(
    m: &MemoryHandle,
    item: &Item,
    fresh: &HashMap<String, (DraftVerdict, Option<String>)>,
    drafts: &[FactDraft],
    model: &str,
    now_ms: i64,
) -> Result<(), RunError> {
    let slots: HashMap<&str, (i64, &str)> = drafts
        .iter()
        .enumerate()
        .map(|(index, draft)| {
            (
                draft.reference.as_str(),
                (index as i64, draft.turn_uid.as_str()),
            )
        })
        .collect();
    let rows = item_verdicts(m, item.meta.id).map_err(mem_store)?;
    let mut out: Vec<NewVerdict> = Vec::with_capacity(rows.len() + fresh.len());
    let mut written: HashSet<String> = HashSet::new();
    for row in rows {
        let replaced = if row.side == VerdictSide::Final {
            fresh.get(&row.reference)
        } else {
            None
        };
        let new = match replaced {
            Some((verdict, code)) => {
                written.insert(row.reference.clone());
                NewVerdict {
                    pass_id: row.pass_id.clone(),
                    turn_uid: row.turn_uid.clone(),
                    reference: row.reference.clone(),
                    draft_index: row.draft_index,
                    side: VerdictSide::Final,
                    verdict: queue_verdict(*verdict),
                    code: code.clone(),
                    evidence: None,
                    evidence_state: EvidenceState::Absent,
                    prompt_version: PASS_FACT_RETRY_VERSION.to_string(),
                    model: model.to_string(),
                }
            }
            None => NewVerdict {
                pass_id: row.pass_id,
                turn_uid: row.turn_uid,
                reference: row.reference,
                draft_index: row.draft_index,
                side: row.side,
                verdict: row.verdict,
                code: row.code,
                evidence: row.evidence,
                evidence_state: row.evidence_state,
                prompt_version: row.prompt_version,
                model: row.model,
            },
        };
        out.push(new);
    }
    // 旧表里没有 final 行的那几条：补行（坐标取载荷草稿的顺序）。
    for (reference, (verdict, code)) in fresh {
        if written.contains(reference) {
            continue;
        }
        let Some((draft_index, turn_uid)) = slots.get(reference.as_str()) else {
            debug_assert!(false, "{reference} 不在载荷草稿里");
            continue;
        };
        out.push(NewVerdict {
            pass_id: item.meta.pass_id.clone(),
            turn_uid: (*turn_uid).to_string(),
            reference: reference.clone(),
            draft_index: *draft_index,
            side: VerdictSide::Final,
            verdict: queue_verdict(*verdict),
            code: code.clone(),
            evidence: None,
            evidence_state: EvidenceState::Absent,
            prompt_version: PASS_FACT_RETRY_VERSION.to_string(),
            model: model.to_string(),
        });
    }
    replace_item_verdicts(m, item.meta.id, &out, now_ms).map_err(mem_store)?;
    Ok(())
}

/// 合成后的逐条判决 → 队列判决对（`mark_turns` 与 `write_rows` 用）。
fn queue_verdicts(
    merged: &[(String, DraftVerdict, Option<String>)],
) -> Vec<(String, ReviewVerdict)> {
    merged
        .iter()
        .map(|(reference, verdict, _)| (reference.clone(), queue_verdict(*verdict)))
        .collect()
}

/// 解析层的三分支判决 ↔ 队列的三分支判决（两个枚举同值不同家）。
fn draft_verdict(verdict: ReviewVerdict) -> DraftVerdict {
    match verdict {
        ReviewVerdict::Pass => DraftVerdict::Pass,
        ReviewVerdict::Reject => DraftVerdict::Reject,
        ReviewVerdict::Uncertain => DraftVerdict::Uncertain,
    }
}

/// 该 item 的旧 `final` 判决（ref → 判决 + code）。表还没建 ⇒ 空表（与
/// `item_verdicts_filtered` 同一条：没有逐条判决就是"这个库里一条都没有"）。
fn final_verdicts(
    m: &MemoryHandle,
    item_id: i64,
) -> Result<HashMap<String, (ReviewVerdict, Option<String>)>, RunError> {
    let rows =
        item_verdicts_filtered(m, item_id, Some(VerdictSide::Final), None).map_err(mem_store)?;
    Ok(rows
        .into_iter()
        .map(|row| (row.reference, (row.verdict, row.code)))
        .collect())
}

/// 重跑一次调用的原文落 artifact（`note` 的唯一落点）。
///
/// artifact 的 `stage` 闭集是 `extract|review`（ledger 的 DDL）：重跑是一次**审核**调用，
/// 落在 `review` 这一格；`run_id` 与既有审核 artifact 同一把取法。
/// 写失败不掩盖判决（`let _`：artifact 是诊断补件，调用方仍拿到判决）。
fn record_retry_artifact(
    l: &LedgerHandle,
    item: &Item,
    family: FactFamily,
    turn_uid: &str,
    raw: &str,
    now_ms: i64,
) {
    let run_id = artifact_run_id(l, item, turn_uid);
    let _ = pass_state::record_artifact(
        l,
        PASS_FACT,
        family.slug(),
        "review",
        &run_id,
        turn_uid,
        raw,
        now_ms,
    );
}

fn payload_run_id(item: &Item) -> String {
    serde_json::from_str::<serde_json::Value>(&item.payload_json)
        .ok()
        .and_then(|root| {
            root.get("run_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// 落 artifact 用的 `run_id`：旧 runner 写的是 `extract_pass_state.run_id`
/// （`fact-promise-<ms>` / `event-<ms>`），**不是**载荷里那枚抽取指纹。
///
/// 读不到那一行 / 那一行没有 `run_id` / 查询本身失败 ⇒ 退回 [`payload_run_id`]：
/// artifact 是诊断补件，不许因它盖住真正的审核解析错误（调用方仍拿到 `Err(Review)`）。
fn artifact_run_id(l: &LedgerHandle, item: &Item, fallback_uid: &str) -> String {
    let first_uid = first_batch_turn(item).unwrap_or_else(|| fallback_uid.to_string());
    if first_uid.is_empty() {
        return payload_run_id(item);
    }
    let key = pass_state::PassKey {
        turn_uid: &first_uid,
        pass_id: &item.meta.pass_id,
        family: &item.meta.family,
    };
    match pass_state::get(l, key) {
        Ok(Some(record)) => match record.run_id {
            Some(run_id) if !run_id.is_empty() => run_id,
            _ => payload_run_id(item),
        },
        _ => payload_run_id(item),
    }
}

/// 批里的第一轮：`item.turn_uids_json`（与重放同一份来源）；读不出就退回 payload
/// `batch.turn_uids[0]`；两者都没有 ⇒ `None`（调用方退回 payload 的 `run_id`）。
fn first_batch_turn(item: &Item) -> Option<String> {
    if let Ok(turns) = serde_json::from_str::<Vec<String>>(&item.turn_uids_json) {
        if let Some(first) = turns.into_iter().next() {
            return Some(first);
        }
    }
    serde_json::from_str::<serde_json::Value>(&item.payload_json)
        .ok()
        .and_then(|root| {
            root.get("batch")
                .and_then(|batch| batch.get("turn_uids"))
                .and_then(serde_json::Value::as_array)
                .and_then(|uids| uids.first())
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
}

fn prompt_version_of(pass: Pass) -> &'static str {
    match pass {
        Pass::Fact(_) => PASS_FACT_REVIEW_VERSION,
        Pass::Event => PASS_EVENT_REVIEW_VERSION,
        Pass::Naming => PASS_NAMING_REVIEW_VERSION,
        Pass::Rows => ROWS_REVIEW_VERSION,
        Pass::State => PASS_STATE_REVIEW_VERSION,
        Pass::Lesson => PASS_LESSON_REVIEW_VERSION,
    }
}

// ── 判决聚合 ──────────────────────────────────────────────────────────────────

fn tally_pairs(pairs: &[(String, DraftVerdict)]) -> (usize, usize, usize) {
    let (mut p, mut r, mut u) = (0usize, 0usize, 0usize);
    for (_, verdict) in pairs {
        match verdict {
            DraftVerdict::Pass => p += 1,
            DraftVerdict::Reject => r += 1,
            DraftVerdict::Uncertain => u += 1,
        }
    }
    (p, r, u)
}

fn aggregate(p: usize, r: usize, u: usize) -> (DraftVerdict, &'static str) {
    if p > 0 {
        (
            DraftVerdict::Pass,
            if r > 0 || u > 0 { REVIEW_MIXED } else { REVIEW_PASS },
        )
    } else if u > 0 {
        (DraftVerdict::Uncertain, REVIEW_UNCERTAIN)
    } else {
        (DraftVerdict::Reject, REVIEW_REJECT)
    }
}

fn refs_by_verdict(
    refs: &[String],
    parsed: &[(String, DraftVerdict, Option<String>)],
) -> (Vec<String>, Vec<String>) {
    let mut pass_refs = Vec::new();
    let mut uncertain_refs = Vec::new();
    for (reference, (_, verdict, _)) in refs.iter().zip(parsed.iter()) {
        match verdict {
            DraftVerdict::Pass => pass_refs.push(reference.clone()),
            DraftVerdict::Uncertain => uncertain_refs.push(reference.clone()),
            DraftVerdict::Reject => {}
        }
    }
    (pass_refs, uncertain_refs)
}

fn queue_verdict(verdict: DraftVerdict) -> ReviewVerdict {
    match verdict {
        DraftVerdict::Pass => ReviewVerdict::Pass,
        DraftVerdict::Reject => ReviewVerdict::Reject,
        DraftVerdict::Uncertain => ReviewVerdict::Uncertain,
    }
}

fn draft_refs(replayed: &Replay) -> Vec<String> {
    match &replayed.drafts {
        ReplayDrafts::Fact(drafts) => drafts.iter().map(|d| d.reference.clone()).collect(),
        ReplayDrafts::Event(drafts) => drafts.iter().map(|d| d.reference.clone()).collect(),
        // naming 的"草稿"就是锚：ref 是 `a1..aN`（位置），判决台账里记的也是它。
        ReplayDrafts::Naming(a1) => a1.anchors.iter().map(|a| a.reference.clone()).collect(),
        // S3c-3b：rows 的"草稿"是行，ref 是 `r1..rN`（位置，与 `rows_review_user` 同一套）。
        // approved/applied 的判决重建按它对齐台账 detail 里的 ref。
        ReplayDrafts::Rows(drafts) => {
            (1..=drafts.len()).map(|index| format!("r{index}")).collect()
        }
        // P2：状态的 ref 是 `s1..sN`（位置），与 `state_review_user` 的 `ITEM s{}` 同一套。
        ReplayDrafts::State(drafts) => {
            (1..=drafts.len()).map(|index| format!("s{index}")).collect()
        }
        // 波三刀 A：lesson 的 ref 是 `l1..lN`（位置），与 `lesson_review_user` 的 `ITEM l{}` 同一套。
        ReplayDrafts::Lesson(drafts) => {
            drafts.iter().map(|draft| draft.reference.clone()).collect()
        }
    }
}

fn last_decision_is_human_pass(m: &MemoryHandle, item_id: i64) -> Result<bool, RunError> {
    let rows = decisions(m, item_id).map_err(mem_store)?;
    // C3 的逐 draft 审计行跳过：它们是闸对单条草稿的裁定，不是这条 item 的表态——否则
    // 一次 human pass 之后的复算会被上一次写下的审计行顶掉。
    Ok(rows
        .iter()
        .rev()
        .find(|row| !row.is_gate_reject_audit())
        .is_some_and(|row| row.stage == ReviewStage::Human && row.verdict == ReviewVerdict::Pass))
}

// ── 写库 ──────────────────────────────────────────────────────────────────────

/// 组装并写库。只有聚合判决 `Pass`（有 pass 草稿）才走到这里。
#[allow(clippy::too_many_arguments)]
fn write_rows(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    l: &LedgerHandle,
    item: &Item,
    pass: Pass,
    replayed: &Replay,
    verdicts: &[(String, DraftVerdict)],
    backend: &dyn LlmBackend,
    prompt_version: &str,
    input_sha256: &str,
    now_ms: i64,
    verdicts_out: Vec<(String, ReviewVerdict)>,
    value_demote: &HashSet<String>,
) -> Result<RunOutcome, RunError> {
    if !verdicts.iter().any(|(_, verdict)| *verdict == DraftVerdict::Pass) {
        // 没有 pass 草稿就没有行可写（approved 重试的极端格）：item 不前进。
        let mut out = RunOutcome::zero(ReviewState::Approved);
        out.verdicts = Some(verdicts_out);
        return Ok(out);
    }
    if let Pass::Naming = pass {
        // naming 不是"逐条写事实行"，而是"把 A1 计划过滤后整批写"：走自己的写入口。
        return write_naming(
            m,
            l,
            item,
            replayed,
            verdicts,
            backend,
            input_sha256,
            now_ms,
            verdicts_out,
        );
    }
    if let Pass::State = pass {
        // P2：状态行写 `state_assertion`（不是 fact/card 那一套），走自己的写入口。
        return write_state(
            m,
            t,
            item,
            replayed,
            verdicts,
            backend,
            prompt_version,
            input_sha256,
            now_ms,
            verdicts_out,
        );
    }
    if let Pass::Rows = pass {
        // S3c-3b：rows 也是"逐行写"，但写的是 fact 表 + 归属行（`insert_fact_row`），
        // 不是 fact/event 的批量写入口（那里会补卡/提及/provenance）。
        return write_rows_apply(
            m,
            item,
            replayed,
            verdicts,
            backend,
            input_sha256,
            now_ms,
            verdicts_out,
        );
    }
    if let Pass::Lesson = pass {
        // 波三刀 A：lesson 写的是**卡**（`kind='lesson'`），没有 fact 行、没有镜像卡、
        // 没有提及/锚——它的三元组本身就是一张经验卡的标题与正文。自己的写入口。
        return write_lesson(
            m,
            item,
            replayed,
            verdicts,
            backend,
            prompt_version,
            input_sha256,
            now_ms,
            verdicts_out,
        );
    }
    let (items, extras, duplicate, duplicate_uids, actor_rebound) = match pass {
        Pass::Fact(_) => {
            let fact = assemble_fact(m, replayed, verdicts, value_demote)?;
            (fact.items, fact.extras, fact.duplicate, fact.duplicate_uids, 0)
        }
        Pass::Event => {
            let event = assemble_event(m, replayed, verdicts, value_demote)?;
            (
                event.items,
                event.extras,
                event.duplicate,
                event.duplicate_uids,
                event.rebound,
            )
        }
        // 上面已经分流；这三格不可达。
        Pass::Naming | Pass::Rows | Pass::State | Pass::Lesson => return Err(RunError::Unsupported),
    };
    let written = items.len();
    // G1 刀 B：fact 与 event 走同一处写口（`put_fact_rows_with_extras` 是程序预闸与价值轴
    // **两族章的唯一落点**）；extras 只多一格 `value_demote`，其余逐字沿旧形状。
    let outcome = put_fact_rows_with_extras(
        m,
        &items,
        &extras,
        &replayed.ctx,
        &replayed.owner,
        now_ms,
    )
    .map_err(|e| RunError::Store(DistillError::from(e)))?;
    let detail = if duplicate > 0 {
        format!(
            "facts={};cards={};mentions={};dedup={duplicate}",
            outcome.facts, outcome.cards, outcome.mentions
        )
    } else {
        format!(
            "facts={};cards={};mentions={}",
            outcome.facts, outcome.cards, outcome.mentions
        )
    };
    // A-rebind 的计数只在真发生时追加：既有 detail 的逐字形状（三条 apply 用例钉着）
    // 一个字节都不动。
    let detail = if actor_rebound > 0 {
        format!("{detail};rebound={actor_rebound}")
    } else {
        detail
    };
    let state = super::record_apply(
        m,
        item.meta.id,
        &detail,
        backend.slug().unwrap_or("unknown"),
        prompt_version,
        input_sha256,
        now_ms,
    )
    .map_err(mem_store)?;
    let mut out = RunOutcome::zero(state);
    out.verdicts = Some(verdicts_out);
    out.duplicate_uids = duplicate_uids;
    out.written = written;
    out.duplicate = duplicate;
    out.actor_rebound = actor_rebound;
    out.facts = outcome.facts;
    out.cards = outcome.cards;
    out.mentions = outcome.mentions;
    // S4：把写库层带出来的卡句柄原样交出去（只有插入成功的那些）；hub 据此逐条提升。
    out.card_ids = outcome.card_ids;
    out.applied_now = true;
    Ok(out)
}

/// naming 的写库（S3b-3b）：把重放出来的一批 A1 计划**过滤到 pass 集合**后整批写。
///
/// 过滤四条（naming 独有；其他 pass 是"逐条草稿"，没有这批语义）：
/// 1. anchors 只留 verdict=pass 的；
/// 2. mentions 只留 `anchor_ref` 在 pass 集合里的；`speaker_ref` 指向未通过锚时置空
///    （丢掉的锚不能继续当主语，也避免写库层 `by_ref[speaker]` 整批失败）；
/// 3. issues 的 `refs` 先过滤到 pass 集合，过滤后为空 ⇒ 整条丢弃；
/// 4. unresolved 全留（轮级，不指向锚）。
///
/// `run_id` 用 [`artifact_run_id`]（状态行有就用，否则载荷 `run_id`）：与 runner 写库那次
/// 同一个记账键。写失败 ⇒ [`RunError::Store`]，不记 apply（item 留 approved）。
#[allow(clippy::too_many_arguments)]
fn write_naming(
    m: &MemoryHandle,
    l: &LedgerHandle,
    item: &Item,
    replayed: &Replay,
    verdicts: &[(String, DraftVerdict)],
    backend: &dyn LlmBackend,
    input_sha256: &str,
    now_ms: i64,
    verdicts_out: Vec<(String, ReviewVerdict)>,
) -> Result<RunOutcome, RunError> {
    let a1 = match &replayed.drafts {
        ReplayDrafts::Naming(a1) => a1,
        _ => return Err(RunError::Unsupported),
    };
    let pass_refs: BTreeSet<String> = verdicts
        .iter()
        .filter(|(_, verdict)| *verdict == DraftVerdict::Pass)
        .map(|(reference, _)| reference.clone())
        .collect();
    let filtered = filter_naming(a1, &pass_refs);
    let first_uid = replayed
        .batch
        .rows
        .first()
        .map(|row| row.turn_uid.clone())
        .unwrap_or_default();
    let run_id = artifact_run_id(l, item, &first_uid);
    let outcome = write_a1_with_owner(
        m,
        &filtered,
        &replayed.ctx,
        &replayed.owner,
        &replayed.scope,
        &run_id,
        now_ms,
    )
    .map_err(|e| RunError::Store(DistillError::Memory(e)))?;
    // detail 只放计数（与 runner 的 `anchors=..;mentions=..;filtered=..` 同一族；
    // 引擎不重算噪声闸，`filtered` 那格不属于审核后的这次写入）。
    let detail = format!("anchors={};mentions={}", outcome.anchors, outcome.mentions);
    let state = super::record_apply(
        m,
        item.meta.id,
        &detail,
        backend.slug().unwrap_or("unknown"),
        PASS_NAMING_REVIEW_VERSION,
        input_sha256,
        now_ms,
    )
    .map_err(mem_store)?;
    let mut out = RunOutcome::zero(state);
    out.verdicts = Some(verdicts_out);
    // 读数口径沿用 naming：`written`/`facts` 都是落库锚数（`NamingOutcome.anchors` 同格），
    // `cards` 恒 0（naming 不产卡），`mentions` 是新插入的提及行数。
    out.written = outcome.anchors;
    out.facts = outcome.anchors;
    out.cards = 0;
    out.mentions = outcome.mentions;
    out.applied_now = true;
    Ok(out)
}

/// 把 A1 计划过滤到通过的锚（四条规则见 [`write_naming`]）。纯函数，不改入参。
fn filter_naming(a1: &A1Batch, pass_refs: &BTreeSet<String>) -> A1Batch {
    let anchors: Vec<_> = a1
        .anchors
        .iter()
        .filter(|anchor| pass_refs.contains(&anchor.reference))
        .cloned()
        .collect();
    let mentions: Vec<_> = a1
        .mentions
        .iter()
        .filter(|mention| pass_refs.contains(&mention.anchor_ref))
        .map(|mention| {
            let mut kept = mention.clone();
            if kept
                .speaker_ref
                .as_deref()
                .is_some_and(|speaker| !pass_refs.contains(speaker))
            {
                kept.speaker_ref = None;
            }
            kept
        })
        .collect();
    let issues: Vec<_> = a1
        .issues
        .iter()
        .filter_map(|issue| {
            let refs: Vec<String> = issue
                .refs
                .iter()
                .filter(|r| pass_refs.contains(*r))
                .cloned()
                .collect();
            if refs.is_empty() {
                return None;
            }
            let mut kept = issue.clone();
            kept.refs = refs;
            Some(kept)
        })
        .collect();
    A1Batch {
        anchors,
        mentions,
        issues,
        // unresolved 是轮级的：审核驳回某个锚不影响"这一轮有个认不出的表面"。
        unresolved: a1.unresolved.clone(),
    }
}

/// 波三刀 A：lesson 的写库——**只写卡**（`kind='lesson'`），不写 fact / 镜像卡 / 提及。
///
/// 落点与 [`rc_memory::writers_x::put_card`] 逐表相同（`card` + `provenance` +
/// `resource_owner`），差别只有归属：走 [`rc_memory::writers_x::put_card_with_owner`]，
/// 把会话的 owner/visibility/retention/scope 原样带下去，而不是合成默认
/// （合成默认会把经验写成 `rc-distill` 的私有行，读闸对真读者一律拒）。
///
/// **EDV default-reject 在这一格兑现**：只有 `verdict=Pass` 的草稿写卡。`Reject`/`Uncertain`
/// 直接丢（判决台账留着），一条都不写。这里不另判语义——语义判在审核 prompt 里，
/// 这一层只执行判决。
///
/// 读数：`written` = pass 草稿数；`cards` = 真插入的卡数；`duplicate` = 内容寻址命中的
/// 同址卡数（同一 title+正文+scope 已经在库里）。apply 判定 detail 只放计数。
#[allow(clippy::too_many_arguments)]
fn write_lesson(
    m: &MemoryHandle,
    item: &Item,
    replayed: &Replay,
    verdicts: &[(String, DraftVerdict)],
    backend: &dyn LlmBackend,
    prompt_version: &str,
    input_sha256: &str,
    now_ms: i64,
    verdicts_out: Vec<(String, ReviewVerdict)>,
) -> Result<RunOutcome, RunError> {
    let drafts = match &replayed.drafts {
        ReplayDrafts::Lesson(drafts) => drafts,
        _ => return Err(RunError::Unsupported),
    };
    let pass_refs: HashSet<&str> = verdicts
        .iter()
        .filter(|(_, verdict)| *verdict == DraftVerdict::Pass)
        .map(|(reference, _)| reference.as_str())
        .collect();
    let mut card_ids: Vec<MemKey> = Vec::new();
    let mut inserted = 0usize;
    let mut duplicate = 0usize;
    let mut written = 0usize;
    for draft in drafts {
        if !pass_refs.contains(draft.reference.as_str()) {
            // 未过审：直接丢（不落卡）。这是本函数唯一的价值判断位，且它只读判决。
            continue;
        }
        written += 1;
        // 出处：整批的轮（与抽取/重放同一条证据集合），role 从重放批读；
        // 读不到是数据不一致（replay 已挡过一次），不猜一个 role。
        let mut evidence: Vec<rc_memory::Provenance> =
            Vec::with_capacity(draft.evidence_turn_uids.len());
        for uid in &draft.evidence_turn_uids {
            let Some(row) = replayed.batch.row(uid) else {
                return Err(contract_store(ContractError::UnlistedUid { uid: uid.clone() }));
            };
            let Some(role) = AnchorRole::parse(&row.role) else {
                return Err(contract_store(ContractError::RoleUnreadable { uid: uid.clone() }));
            };
            evidence.push(rc_memory::Provenance { turn_uid: uid.clone(), anchor_role: role });
        }
        if evidence.is_empty() {
            return Err(contract_store(ContractError::ProvenanceEmpty {
                table: "card",
                index: card_ids.len(),
            }));
        }
        // 卡面 = 标题 + 描述/内容两段。描述是"什么时候适用"，内容是"具体怎么做"，
        // 两段都进正文，读卡的人不用回查判定台账。
        let content = rc_memory::writers_x::FactContent {
            kind: "lesson".to_string(),
            title: draft.title.clone(),
            body: format!("{}\n\n{}", draft.description, draft.content),
            scope: replayed.scope.clone(),
            valid_to_ms: None,
        };
        let (key, n) = rc_memory::writers_x::put_card_with_owner(
            m,
            &content,
            &evidence,
            &replayed.ctx,
            &replayed.owner,
            now_ms,
        )
        .map_err(|e| RunError::Store(DistillError::Memory(e)))?;
        if n == 1 {
            inserted += 1;
            card_ids.push(key);
        } else {
            duplicate += 1;
        }
    }
    let detail = format!("cards={inserted};duplicate={duplicate}");
    let state = super::record_apply(
        m,
        item.meta.id,
        &detail,
        backend.slug().unwrap_or("unknown"),
        prompt_version,
        input_sha256,
        now_ms,
    )
    .map_err(mem_store)?;
    let mut out = RunOutcome::zero(state);
    out.verdicts = Some(verdicts_out);
    out.written = written;
    out.duplicate = duplicate;
    out.cards = inserted;
    out.card_ids = card_ids;
    out.applied_now = true;
    Ok(out)
}

/// rows 的写库（S3c-3b）：把**判 pass 的行**逐行写进 `fact` 表（`insert_fact_row`）。
///
/// 边界：
/// - 行值全部**先在内存里转成 SQLite 标量**再开写：任一行有数组/对象 ⇒
///   [`REVIEW_PAYLOAD_SHAPE`]，一笔都不写、不记 apply（item 留 approved，等人工/重跑）。
/// - ref 由位置定（`r1..rN`，与 `rows_review_user` 的 `ROW r{n}` 同一套）；非 pass 行跳过。
/// - `insert_fact_row` 自带事务与幂等：`n==1` 计 inserted、`n==0` 是 id 已存在的重复。
/// - apply 判定 detail 只放计数 `rows=<inserted>;duplicate=<duplicate>`，model 取 backend
///   slug、prompt 版本取 [`ROWS_REVIEW_VERSION`]、`input_sha256` 仍是载荷指纹。
/// - 读数：`written`/`facts` = inserted、`duplicate` = duplicate、`cards`/`mentions` = 0；
///   `applied_now` = true（真的写了这一批）。
#[allow(clippy::too_many_arguments)]
fn write_rows_apply(
    m: &MemoryHandle,
    item: &Item,
    replayed: &Replay,
    verdicts: &[(String, DraftVerdict)],
    backend: &dyn LlmBackend,
    input_sha256: &str,
    now_ms: i64,
    verdicts_out: Vec<(String, ReviewVerdict)>,
) -> Result<RunOutcome, RunError> {
    let drafts = match &replayed.drafts {
        ReplayDrafts::Rows(drafts) => drafts,
        _ => return Err(RunError::Unsupported),
    };
    let pass_refs: HashSet<&str> = verdicts
        .iter()
        .filter(|(_, verdict)| *verdict == DraftVerdict::Pass)
        .map(|(reference, _)| reference.as_str())
        .collect();
    let rows: Vec<Vec<(String, SqlValue)>> = drafts
        .iter()
        .enumerate()
        .filter(|(index, _)| pass_refs.contains(format!("r{}", index + 1).as_str()))
        .map(|(_, draft)| sql_row(&draft.values))
        .collect::<Result<_, _>>()?;
    let (mut inserted, mut duplicate) = (0usize, 0usize);
    for values in &rows {
        let n = insert_fact_row(
            m,
            values,
            &replayed.owner,
            &replayed.scope,
            &replayed.ctx,
            now_ms,
        )
        .map_err(|e| RunError::Store(DistillError::Memory(e)))?;
        // 幂等命中（同一 id 已在库里）不是错误：它是这一行的"已写过"。
        if n == 1 {
            inserted += 1;
        } else {
            duplicate += 1;
        }
    }
    let detail = format!("rows={inserted};duplicate={duplicate}");
    let state = super::record_apply(
        m,
        item.meta.id,
        &detail,
        backend.slug().unwrap_or("unknown"),
        ROWS_REVIEW_VERSION,
        input_sha256,
        now_ms,
    )
    .map_err(mem_store)?;
    let mut out = RunOutcome::zero(state);
    out.verdicts = Some(verdicts_out);
    out.written = inserted;
    out.facts = inserted;
    out.duplicate = duplicate;
    out.applied_now = true;
    Ok(out)
}

/// 一行的列值 → 绑定值。只收标量：`string`/`number`/`bool`/`null`；
/// 数组与对象不是列值 ⇒ [`REVIEW_PAYLOAD_SHAPE`]。
///
/// 整数落 `INTEGER`；超出 `i64` 的数字落 `REAL`（SQLite 的数字亲和）；两者都不成立的值
/// 也按形状错收（`serde_json::Number` 里不会有，但不 `unwrap`）。
fn sql_row(values: &serde_json::Map<String, Value>) -> Result<Vec<(String, SqlValue)>, RunError> {
    values
        .iter()
        .map(|(column, value)| Ok((column.clone(), sql_value(value)?)))
        .collect()
}

fn sql_value(value: &Value) -> Result<SqlValue, RunError> {
    let shape = || RunError::ReviewRows(REVIEW_PAYLOAD_SHAPE.to_string());
    match value {
        Value::Null => Ok(SqlValue::Null),
        Value::Bool(flag) => Ok(SqlValue::Integer(i64::from(*flag))),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => Ok(SqlValue::Integer(integer)),
            None => number.as_f64().map(SqlValue::Real).ok_or_else(shape),
        },
        Value::String(text) => Ok(SqlValue::Text(text.clone())),
        Value::Array(_) | Value::Object(_) => Err(shape()),
    }
}

struct FactAssembly {
    items: Vec<FactWriteItem>,
    /// G1 刀 B：与 `items` 逐位对齐的 extras（fact 只可能带 `value_demote` 一格，
    /// 其余三格恒默认——事件那一支才用得到它们）。
    extras: Vec<FactExtras>,
    duplicate: usize,
    duplicate_uids: BTreeSet<String>,
}

/// fact 的写库项组装（与 runner 931–1030 同口径）：证据 role 从批内轮读、`source_ms`
/// 取证据轮 `created_at_ms` 最大值、scope 取载荷重放值；pass 之前做跨 family 去重。
///
/// `value_demote` 是审核价值轴判 demote 的全局 ref 集：命中的草稿在 extras 里带一格
/// `value_demote=true`，由写库层在落卡事务里盖 `value_axis` 章（G1 刀 B）。
fn assemble_fact(
    m: &MemoryHandle,
    replayed: &Replay,
    verdicts: &[(String, DraftVerdict)],
    value_demote: &HashSet<String>,
) -> Result<FactAssembly, RunError> {
    let drafts: &[FactDraft] = match &replayed.drafts {
        ReplayDrafts::Fact(drafts) => drafts,
        _ => return Err(RunError::Unsupported),
    };
    let mut items: Vec<FactWriteItem> = Vec::with_capacity(drafts.len());
    let mut extras: Vec<FactExtras> = Vec::with_capacity(drafts.len());
    let mut duplicate = 0usize;
    let mut duplicate_uids = BTreeSet::new();
    // 批内已见的重复：与 `already_written` 同一条分岔（S2e-6 / C5 / C5-fix）。
    // - 有 value ⇒ 键是 `(证据轮, value)`（跨族去重：同一句话只留一行）；
    // - 无 value ⇒ 键是 `(证据轮, 主语, 类别, 谓词, 宾语身份)`（空值是常态，谓词/类别是族常量，
    //   只比槽位会把同轮同主语的两条不同承诺吞掉——宾语不同就是两条事）。
    // value 一律 `trim()` 后进键：与 `already_written`（库内同样先 `TRIM`）同一把尺子。
    let mut seen_in_batch: HashSet<Vec<String>> = HashSet::new();
    for (draft, (reference, verdict)) in drafts.iter().zip(verdicts.iter()) {
        if *verdict != DraftVerdict::Pass {
            continue;
        }
        let evidence_uid = match draft.evidence_turn_uids.first() {
            Some(uid) => uid.clone(),
            None => {
                return Err(contract_store(ContractError::ProvenanceEmpty {
                    table: "fact",
                    index: items.len(),
                }))
            }
        };
        let value_key = draft.value.as_deref().unwrap_or_default().trim().to_string();
        let object_id = draft.object_anchor_id.clone().unwrap_or_default();
        let slot: Vec<String> = if value_key.is_empty() {
            vec![
                evidence_uid.clone(),
                draft.subject_anchor_id.clone(),
                draft.category.clone(),
                draft.predicate.clone(),
                // fact 草稿没有字面宾语（字面量在 value 里），空值支的宾语身份只剩锚。
                crate::pass_fact::object_key(&object_id, ""),
            ]
        } else {
            vec![evidence_uid.clone(), value_key.clone()]
        };
        if !seen_in_batch.insert(slot)
            || already_written(
                m,
                &evidence_uid,
                &value_key,
                &draft.subject_anchor_id,
                &draft.category,
                &draft.predicate,
                &object_id,
            )
            .map_err(distill_store)?
        {
            duplicate += 1;
            duplicate_uids.insert(draft.turn_uid.clone());
            continue;
        }
        let (evidence, source_ms) =
            evidence_of(replayed, &draft.evidence_turn_uids, items.len())?;
        items.push(FactWriteItem {
            subject_anchor_id: draft.subject_anchor_id.clone(),
            object_anchor_id: draft.object_anchor_id.clone(),
            predicate: draft.predicate.clone(),
            value: draft.value.clone(),
            category: draft.category.clone(),
            mode: draft.mode.clone(),
            time_text: draft.time_text.clone(),
            quote: draft.quote.clone(),
            evidence,
            text: draft.text.clone(),
            scope: replayed.scope.clone(),
            source_ms,
        });
        extras.push(FactExtras {
            value_demote: value_demote.contains(reference),
            ..Default::default()
        });
    }
    Ok(FactAssembly { items, extras, duplicate, duplicate_uids })
}

struct EventAssembly {
    items: Vec<FactWriteItem>,
    extras: Vec<FactExtras>,
    duplicate: usize,
    duplicate_uids: BTreeSet<String>,
    /// A-rebind：本次组装里由程序**回填**成会话自锚的 actor 条数（进 `RunOutcome`）。
    /// 与 `duplicate` 同一出口风格：程序自己数、不靠模型自报。
    rebound: usize,
}

/// event 的写库项组装：与 fact 同口径，另加 extras 三格。C5b 起与 fact 一样做去重；
/// C5-fix 起键是 `(证据轮, actor, action, 宾语身份)`。
/// 键里**没有** `text`：真实数据里重复的正是"同一动作换了种说法"
/// （`write_fact` 一行写 `fact`、一行写 `1 条 fact`）。键里**有** action 与宾语身份：
/// 同一轮同一个 actor 的 `complete_tls_verification`/`create_build_dir`/`create_data_dir`
/// 是三件不同的事，不能并（`rerun-e2` 里恰好有这一组，3 行同 actor、3 个不同 action）；
/// 同一轮同一动作、`object_text` 不同的（"把配置改回去" / "把部署脚本改回旧版"）也是两件事
/// ——旧的只认锚的键会把第二条静默记成 duplicate 丢掉。宾语身份与库内查询共用
/// [`crate::pass_fact::object_key`]。
fn assemble_event(
    m: &MemoryHandle,
    replayed: &Replay,
    verdicts: &[(String, DraftVerdict)],
    value_demote: &HashSet<String>,
) -> Result<EventAssembly, RunError> {
    let drafts = match &replayed.drafts {
        ReplayDrafts::Event(drafts) => drafts,
        _ => return Err(RunError::Unsupported),
    };
    let mut items: Vec<FactWriteItem> = Vec::with_capacity(drafts.len());
    let mut extras: Vec<FactExtras> = Vec::with_capacity(drafts.len());
    let mut duplicate = 0usize;
    let mut duplicate_uids = BTreeSet::new();
    let mut rebound = 0usize;
    let mut seen_in_batch: HashSet<Vec<String>> = HashSet::new();
    // A-rebind 的判据要证据轮的正文与角色：从重放批取一份（同一 item 的轮都在里面）。
    let bodies: HashMap<String, String> = replayed
        .batch
        .rows
        .iter()
        .map(|row| (row.turn_uid.clone(), row.body.clone()))
        .collect();
    let roles: HashMap<String, String> = replayed
        .batch
        .rows
        .iter()
        .map(|row| (row.turn_uid.clone(), row.role.clone()))
        .collect();
    for (draft, (reference, verdict)) in drafts.iter().zip(verdicts.iter()) {
        if *verdict != DraftVerdict::Pass {
            continue;
        }
        let evidence_uid = match draft.evidence_turn_uids.first() {
            Some(uid) => uid.clone(),
            None => {
                return Err(contract_store(ContractError::ProvenanceEmpty {
                    table: "event",
                    index: items.len(),
                }))
            }
        };
        // A-rebind：闸放行了"模型选的 actor 不在 agent 轮正文里、候选池里唯一存在本会话
        // agent 锚"的那几条；这一侧用**同一个**判据把它落实成自锚（闸与写库不各写一份）。
        let mut actor = draft.actor_anchor_id.clone();
        let mut participants = draft.participants_anchor_ids.clone();
        if let Some(rebound_actor) = event_actor_rebind(
            m,
            &actor,
            &draft.evidence_turn_uids,
            &bodies,
            &roles,
            &replayed.batch.source_agent,
            &replayed.owner.owner_agent_id,
            &replayed.scope,
        )
        .map_err(distill_store)?
        {
            // participants 里也要含回填后的 actor：旧 actor 换成新 actor，再确保在场
            // （写库层会把 subject 自动并进 participants，这里不靠那一层兜底）。
            for participant in participants.iter_mut() {
                if *participant == actor {
                    *participant = rebound_actor.clone();
                }
            }
            if !participants.contains(&rebound_actor) {
                participants.push(rebound_actor.clone());
            }
            actor = rebound_actor;
            rebound += 1;
        }
        let object_id = draft.object_anchor_id.clone().unwrap_or_default();
        let object_text = draft.object_text.clone().unwrap_or_default();
        let slot = vec![
            evidence_uid.clone(),
            actor.clone(),
            draft.action.clone(),
            crate::pass_fact::object_key(&object_id, &object_text),
        ];
        if !seen_in_batch.insert(slot)
            || crate::pass_event::already_written_event(
                m,
                &evidence_uid,
                &actor,
                &draft.action,
                &object_id,
                &object_text,
            )
            .map_err(distill_store)?
        {
            duplicate += 1;
            duplicate_uids.insert(draft.turn_uid.clone());
            continue;
        }
        let (evidence, source_ms) =
            evidence_of(replayed, &draft.evidence_turn_uids, items.len())?;
        items.push(FactWriteItem {
            subject_anchor_id: actor,
            object_anchor_id: draft.object_anchor_id.clone(),
            predicate: draft.action.clone(),
            value: draft_value(draft),
            category: draft.category.clone(),
            mode: draft.mode.clone(),
            time_text: draft.time_text.clone(),
            quote: draft.quote.clone(),
            evidence,
            text: draft.text.clone(),
            scope: replayed.scope.clone(),
            source_ms,
        });
        extras.push(FactExtras {
            participants,
            value_json: Some(draft_value_json(draft).to_string()),
            place_anchor_id: draft.place_anchor_id.clone(),
            // G1 刀 B：价值轴判 demote 的事件行在同一落卡事务里盖 `value_axis` 章。
            value_demote: value_demote.contains(reference),
        });
    }
    Ok(EventAssembly { items, extras, duplicate, duplicate_uids, rebound })
}

/// 证据：逐 uid 从重放批取 role（`AnchorRole::parse`），`source_ms` 取最大值。
/// 读不到/role 读不出是数据不一致（parser 已挡过一次），不猜一个 role。
fn evidence_of(
    replayed: &Replay,
    uids: &[String],
    index: usize,
) -> Result<(Vec<(String, AnchorRole)>, i64), RunError> {
    if uids.is_empty() {
        return Err(contract_store(ContractError::ProvenanceEmpty { table: "fact", index }));
    }
    let mut evidence: Vec<(String, AnchorRole)> = Vec::with_capacity(uids.len());
    let mut source_ms = i64::MIN;
    for uid in uids {
        let Some(row) = replayed.batch.row(uid) else {
            return Err(contract_store(ContractError::UnlistedUid { uid: uid.clone() }));
        };
        let Some(role) = AnchorRole::parse(&row.role) else {
            return Err(contract_store(ContractError::RoleUnreadable { uid: uid.clone() }));
        };
        source_ms = source_ms.max(row.created_at_ms);
        evidence.push((uid.clone(), role));
    }
    Ok((evidence, source_ms))
}

/// P2：状态的写库项组装（与 [`assemble_fact`] 同口径）：证据 role 从批内轮读、`source_ms`
/// 取证据轮 `created_at_ms` 最大值、scope 取载荷重放值。
///
/// 这里**不做**跨批去重：同 `(主体, 维度, 值)` 的重复由写库层的第二道幂等判
/// （`writers_state::find_live_state`，它看得见本事务刚插入的行），两侧不必各算一份键。
/// 冲突（同维度异值）也不在这里：那是程序闸 [`crate::review::gate::STATE_CONFLICT_CODE`]
/// 的活，坏 draft 根本进不到审核这一层。
fn assemble_state(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    replayed: &Replay,
    verdicts: &[(String, DraftVerdict)],
) -> Result<Vec<StateWriteItem>, RunError> {
    let drafts = match &replayed.drafts {
        ReplayDrafts::State(drafts) => drafts,
        _ => return Err(RunError::Unsupported),
    };
    let pass_refs: HashSet<&str> = verdicts
        .iter()
        .filter(|(_, verdict)| *verdict == DraftVerdict::Pass)
        .map(|(reference, _)| reference.as_str())
        .collect();
    // C1：`mode` 由 6c 那一把尺子盖章（`check_subject_in_evidence` 的放行支身份），
    // 不是载荷说什么就是什么——载荷只能声明"原文直陈"，"主语与值同句"要在这里按库再判一次。
    // 两侧输入同源：正文/角色取重放批（与闸读的转录行逐字相同，H4a 已核）、主人取同一个
    // `known_principal`、scope/source_agent 取载荷重放值。
    let bodies: HashMap<String, String> = replayed
        .batch
        .rows
        .iter()
        .map(|row| (row.turn_uid.clone(), row.body.clone()))
        .collect();
    let roles: HashMap<String, String> = replayed
        .batch
        .rows
        .iter()
        .map(|row| (row.turn_uid.clone(), row.role.clone()))
        .collect();
    let principal = known_principal(
        t,
        &replayed.batch.session_key,
        &replayed.batch.device_id,
    )
    .map_err(distill_store)?;
    let mut items: Vec<StateWriteItem> = Vec::new();
    for (index, draft) in drafts.iter().enumerate() {
        let reference = format!("s{}", index + 1);
        if !pass_refs.contains(reference.as_str()) {
            continue;
        }
        let (evidence, source_ms) =
            evidence_of(replayed, &draft.evidence_turn_uids, items.len())?;
        let claim = SubjectClaim {
            subject_anchor_id: &draft.subject_anchor_id,
            evidence_uids: draft.evidence_turn_uids.iter().map(String::as_str).collect(),
            value: Some(draft.value_after.as_str()),
        };
        let allowance = check_subject_in_evidence(
            m,
            &claim,
            &bodies,
            &roles,
            principal.as_ref(),
            &replayed.scope,
            &replayed.ctx.source_agent,
        )
        .map_err(distill_store)?;
        // 放行支身份 ⇒ 两值：值句共现（或"主语不是锚"/身份未知这两支）才是直陈；
        // 跨句借名与"只靠说话人豁免放行"一律降级。拒收的行本来就写不进来，这里按降级兜底。
        let grounded = match allowance {
            SubjectCheck::Allowed(allow) => allow.grounded(),
            SubjectCheck::Rejected(_) => false,
        };
        let mode = if grounded {
            crate::pass_prompt::STATE_MODE_EXPRESSED
        } else {
            crate::pass_prompt::STATE_MODE_INFERRED
        };
        items.push(StateWriteItem {
            subject_anchor_id: draft.subject_anchor_id.clone(),
            dimension: draft.dimension.clone(),
            value_after: draft.value_after.clone(),
            mode: mode.to_string(),
            text: crate::pass_state::compose_text(
                &draft.subject_canonical,
                &draft.dimension,
                &draft.value_after,
            ),
            evidence,
            scope: replayed.scope.clone(),
            source_ms,
        });
    }
    Ok(items)
}

/// P2 状态行的写库收口（与 fact/event 的 `write_rows` 同一条尾巴：写 → `record_apply` →
/// 读数），差别只在 detail 那一格写 `states=`/`mentions=`——状态不产卡，也不写 fact 表。
#[allow(clippy::too_many_arguments)]
fn write_state(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    item: &Item,
    replayed: &Replay,
    verdicts: &[(String, DraftVerdict)],
    backend: &dyn LlmBackend,
    prompt_version: &str,
    input_sha256: &str,
    now_ms: i64,
    verdicts_out: Vec<(String, ReviewVerdict)>,
) -> Result<RunOutcome, RunError> {
    let items = assemble_state(m, t, replayed, verdicts)?;
    let written = items.len();
    let outcome = put_state_rows(m, &items, &replayed.ctx, &replayed.owner, now_ms)
        .map_err(|e| RunError::Store(DistillError::from(e)))?;
    // `detail` 是这条 item 的落库账（进 `review_verdict`，可按 id 回查）。零格不写：
    // 一片没有冲突的语料不该读出 `superseded=0` 这种噪声。
    let mut detail = format!("states={};mentions={}", outcome.states, outcome.mentions);
    for (key, value) in [
        ("dedup", outcome.duplicate),
        ("superseded", outcome.superseded),
        ("backfilled", outcome.backfilled),
        ("superseded_skipped", outcome.superseded_skipped),
    ] {
        if value > 0 {
            detail.push_str(&format!(";{key}={value}"));
        }
    }
    let state = super::record_apply(
        m,
        item.meta.id,
        &detail,
        backend.slug().unwrap_or("unknown"),
        prompt_version,
        input_sha256,
        now_ms,
    )
    .map_err(mem_store)?;
    let mut out = RunOutcome::zero(state);
    out.verdicts = Some(verdicts_out);
    out.written = written;
    out.duplicate = outcome.duplicate;
    // `facts` 这一格对 state 装的是**插入的状态行数**（naming 复用同一格放锚数，同一条先例）。
    out.facts = outcome.states;
    out.mentions = outcome.mentions;
    out.applied_now = true;
    Ok(out)
}
