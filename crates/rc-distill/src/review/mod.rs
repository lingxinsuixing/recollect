//! S3a-2：双审队列的**程序闸**（设计 `2026-10-26-review-queue-s3` §5）。
//!
//! 这一层只做确定性判定：读 `review_item` 的载荷与中心转录/记忆行，返回第一个失败码。
//! 不打印、不写库、不读钟（`now_ms` 由调用方给）；解析失败也是判定结果（`payload_shape`），
//! 不是 `Err`——只有 SQL/句柄错误才向上抛。
//!
//! 闸门的输入是**中心侧的转录与记忆行**，不是模型自报，所以它住在同时握有
//! `TranscriptHandle`/`MemoryHandle` 的 `rc-distill`，不放进 `rc-memory`（句柄互盲守卫）。
//!
//! S3a-3a：这里再加一层**薄接线**——载荷的公共三格（owner/batch/信封）、判定记录与
//! "程序阶段是否已表态"的只读查询。fact/event runner 只负责把各自的草稿填进 `items`
//! （event 多一格 `extras`），字段形状与 `gate::parse_*` 逐字一致。
//!
//! S3c-1：`gate_rows` 是 `/v1/rows` 的 **fact 行**闸（B1 只审 fact、C1 用 `text` 对证据轮）。
//! 它的输入是一行的列值而不是 `review.pass.v1` 信封，所以与 [`gate`] 分开一个模块；两者
//! 共用同一套纪律（纯判定、码序固定、形状错也是判定码）。
//!
//! S3c-3b：rows 接进 [`run`] 引擎——窄审输入走 `pass_prompt::rows_review_*`，指纹的 pipeline
//! 是 [`ROWS_PIPELINE`]、prompt 版本是 [`ROWS_REVIEW_VERSION`]；只有聚合 Pass 才把通过的
//! 行经 `writers_rows::insert_fact_row` 写库。rows 不写 `extract_pass_state`（`PASS_IDS`
//! 没有 rows），所以 `run::mark_turns`/`run::mark_batch` 对它是 no-op。

pub mod gate;
pub mod gate_rows;
pub mod replay;
pub mod rows_payload;
pub mod run;

pub use gate::{gate, gate_report, DraftGate, GateOutcome, GateReport, GATE_RULES_VERSION};
pub use gate_rows::{gate_rows, RowsCandidate, RowsGateOutcome, ROWS_GATE_CODES};
pub use replay::{replay, Replay, ReplayDrafts, ReplayFailure, ReplayGap};
pub use rows_payload::{rows_new_item, rows_payload, RowsRowInput, ROWS_SCHEMA};
/// DR：审核输入的只读重建出口（`hub review show ID --prompt` 用它）。名字与定义都在
/// [`run`] 里——这一行只是把同一份实现摆到 `review::` 的公共面上，不是第二份。
pub use run::{render_prompt_for_item, ReviewPrompt};

/// S4：review apply 之后的卡提升。名字与定义都在 [`run`]（[`run::promote_applied_cards`]）——
/// 五个 apply 入口（`hub review run`、fact 内联窄审、event 内联窄审、lesson 内联窄审、重跑收尾）共用这一份，
/// 不是每个入口各抄一遍循环。这里也只是把同一份实现摆到公共面上。
pub use run::promote_applied_cards;

/// §7.3a：逐条判决与证据（`review_verdict`）的只读出口——§7.3b 的重跑队列与
/// `hub review show` 的溯源视图从这里取，不必各自去认 `rc_memory::verdict` 的模块路径。
///
/// 名字与定义都在 [`rc_memory::verdict`]（写侧也只在 [`run`] 的 fact 分支调它）；这一行与
/// 上面 [`render_prompt_for_item`] 同一条纪律：不是第二份实现。
pub use rc_memory::verdict::{
    item_verdicts, item_verdicts_filtered, EvidenceState, VerdictRow, VerdictSide,
};

/// 一次 pass 运行对**窄审**的两条路（S3b-4b）。
///
/// - [`ReviewMode::Inline`]：S2/S3a 的既有口径——程序闸过了就在同一次运行里问审核模型，
///   判定与写库当场做完。所有既有调用（含 `hub distill once` 的默认）都走这一格。
/// - [`ReviewMode::EnqueueOnly`]：只抽取、只入队；程序闸过且 item 是 `pending_review` 的那一批
///   不再问模型，逐轮落 `pending(review_pending)`，产出等 `hub review run` 补审补写。
///   它**只**跳过 "pending 的窄审"：`approved`/`applied` 的重试（判决从台账重建、写库）仍走引擎。
///
/// 两种模式共用同一次抽取、同一个程序闸与同一份载荷：`--no-review` 改变的只是"这一轮问到哪"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewMode {
    Inline,
    EnqueueOnly,
}

use rc_memory::review::{
    decisions, record_decision, Decider, Decision, NewItem, ReviewStage, ReviewState, ReviewVerdict,
};
use rc_memory::MemError;
use rc_store::MemoryHandle;
use serde_json::{json, Value};
use std::collections::HashSet;

use crate::batch::Batch;
use crate::DistillError;

/// 载荷信封的 schema 字面量（与 `gate::parse_payload` 同一条真相）。
pub const REVIEW_SCHEMA: &str = "review.pass.v1";

/// distill 进程内占用一条待审项的租约（LLM 调用可能很慢；hub 侧按自己的 timeout+120s 传）。
pub const REVIEW_LEASE_MS: i64 = 900_000; // 15 分钟

/// fact / event 的窄审 prompt 版本（设计 §6）。判定台账里按这个值回溯 prompt。
///
/// 工单 §7.2：fact 从 `v0.2` → `v0.3`——同一片发 proposer + challenger 两次调用，
/// `verdicts[]` 从三键变四键（新增 `evidence`），判决规则改成"两侧都 pass 才 pass"。
/// 模板与契约都变了，旧判定台账不能再按同一个版本回溯。event 不动。
///
/// G1 刀 B：`v0.4` → `v0.5`——审核 prompt 增加价值轴（闭集问句 `VALUE_AXIS_QUESTION`、
/// `value_axis` 这一格、标准与正反例各多一条），输出契约从"四键 + 逐条四格"变成
/// "逐条五格"。判定台账按版本回溯，所以版本必须跟着变。
pub const PASS_FACT_REVIEW_VERSION: &str = "pass-fact-review-v0.5";
/// §7.3b：uncertain 草稿**单条重跑**的 prompt 版本（`run::retry_uncertain`）。
///
/// 与 [`PASS_FACT_REVIEW_VERSION`] 分开：重跑是同一批 item 的第三次调用，模板（单条契约、
/// 五项上下文）与正常审核都不同，判定台账必须能分开回溯。它也是 apply 判定的
/// `prompt_version`——重跑写库那一次的事实行是重跑这条判决的产物。
pub const PASS_FACT_RETRY_VERSION: &str = "pass-fact-review-retry-v0.1";
/// G1 刀 B：`v0.3` → `v0.4`——event 审核 prompt 同样多了一轴（问句 + 闭集 + 正反例）。
pub const PASS_EVENT_REVIEW_VERSION: &str = "pass-event-review-v0.4";

/// P2 状态窄审的 prompt 版本（与抽取侧同一格位：`review_decision.prompt_version`）。
pub const PASS_STATE_REVIEW_VERSION: &str = "pass-state-review-v0.1";
/// 波三刀 A：lesson 窄审的 prompt 版本。lesson 是本仓第 6 个抽取 pass，审核模板
/// （三元组 + 错误轴 + 这一段的目标轮）与 event 都不同，判定台账必须能分开回溯。
pub const PASS_LESSON_REVIEW_VERSION: &str = "pass-lesson-review-v0.1";
/// naming 的窄审 prompt 版本（S3b-3a 的 `naming_review_*` 渲染，S3b-3b 接进引擎）。
///
/// H6：`v0.1` → `v0.2`——STANDARDS 从四到七、反例从三到六（六码逐码有规则与判决样例、
/// 码间优先级、`reject`/`uncertain` 边界）。模板语义变了，旧判定台账不能再按同一个版本回溯。
///
/// 隔离码：naming 的解析失败与 fact/event **同前缀**——落库与返给调用方的 reason 都是
/// `review_<slug>`（`review_json`、`review_verdict_code`、`review_code_on_pass`…），
/// 由 [`run`] 的 `ReviewNaming` 写入点一次拼好，runner 与 hub 的 `mark_batch` 原样沿用。
pub const PASS_NAMING_REVIEW_VERSION: &str = "pass-naming-review-v0.2";

/// rows 窄审的**流水线名**（`water::extraction_fingerprint` 的第一格）。
///
/// 与 `FACT_PIPELINE_PREFIX`/`EVENT_PIPELINE`/`NAMING_PIPELINE` 并列：同一个模型 + 同一份
/// `system`/`user` 在这个 pipeline 名下的指纹，就是 `review_decision.review_input_sha256`。
/// 名字固定成 `pass-rows-v0.1`，不随 prompt 版本走——那一格是 [`ROWS_REVIEW_VERSION`]。
pub const ROWS_PIPELINE: &str = "pass-rows-v0.1";

/// rows 窄审的 prompt 版本（`review_decision.prompt_version`，也是 `record_apply` 的那一格）。
///
/// 与 `pass_prompt::rows_review_*` 的模板同一条版本线：模板语义变了就升这一格，
/// 旧判定台账不能再按同一个版本回溯（与 fact/event/naming 的纪律相同）。
///
/// S3c-fix：`v0.1` → `v0.2`——**输入与标准都变了**：行渲染多一格 `predicate:`
/// （`contradiction` 的前提"同一主语同一谓词"原先对模型不可见），`REJECT VS UNCERTAIN`
/// 补了"`uncertain` 要填那条轮子无法裁定的规则的码"。旧判定按 `v0.1` 回溯不到新模板。
pub const ROWS_REVIEW_VERSION: &str = "row-support-review-v0.2";

/// 程序闸通过的判定码（设计 §5 的"非失败"格；与失败码同一列）。
pub const PROGRAM_PASS: &str = "program_pass";
/// 程序闸通过但审核混合/全过/驳回/不确定时的判定码（设计 §4/§6）。
pub const REVIEW_PASS: &str = "review_pass";
pub const REVIEW_MIXED: &str = "review_mixed";
pub const REVIEW_REJECT: &str = "review_reject";
pub const REVIEW_UNCERTAIN: &str = "review_uncertain";
/// 写库成功的判定码。
pub const APPLIED: &str = "applied";

/// S3a 的临时口径：程序闸 pass 后**没有**独立窄审，直接记一条 `decider=program` 的
/// `stage=review, verdict=pass`。naming 自 S3b 起已改走 [`record_review`]；这个码如今只留给
/// time（无模型面、无窄审），判定的 `model`/`prompt_version` 在这一支里仍是空串。
pub const PROGRAM_ONLY: &str = "program_only";

/// `program_only` 判定的 `detail` 字面量：一眼看出这条"审核"没有模型参与。
pub const PROGRAM_ONLY_DETAIL: &str = "s3a_program_only";

/// 重试/重放时 review 判定 detail 读不出逐条判决的终态理由（S3a-3a-fix 的 fail-closed）。
pub const REVIEW_VERDICTS_MISSING: &str = "review_verdicts_missing";

/// H5b：一次审核 LLM 调用最多带多少条（设计 §6 的"窄审每批 ≤10 条"）。
///
/// 引擎按它切分片：fact/event 按草稿顺序、naming 按锚顺序；每条 item 仍只有**一条** review
/// 判定，分片只改变"问了几次"，不改变判定的粒度。
pub const REVIEW_MAX_ITEMS: usize = 10;

/// 载荷/判定里的 owner 六键。`Visibility` 是 serde snake_case 的枚举，这里直接交给
/// `serde_json`（与 `resource_owner` 的字面量同一条纪律：`private_agent` 不是 `PrivateAgent`）。
pub fn owner_value(owner: &rc_memory::owner::OwnerCtx, scope: &rc_memory::Scope) -> Value {
    json!({
        "owner_principal_id": owner.owner_principal_id,
        "owner_agent_id": owner.owner_agent_id,
        "visibility": owner.visibility,
        "retention": owner.retention,
        "scope_kind": scope.kind.as_str(),
        "scope_key": scope.key,
    })
}

/// `owner_json`：`review_item` 的归属列与载荷里的 `owner` 是同一份对象。
pub fn owner_json(owner: &rc_memory::owner::OwnerCtx, scope: &rc_memory::Scope) -> String {
    owner_value(owner, scope).to_string()
}

/// 载荷的 `batch` 五格：轮清单取批内目标轮（与 `turn_uids_json` 同一个数组），
/// `source_ms` 取批内 `created_at_ms` 的最大值（I3：不拿导入钟冒充源时刻）。
pub fn batch_ref(batch: &Batch, ctx: &rc_memory::BatchCtx) -> Value {
    let turn_uids: Vec<&str> = batch.rows.iter().map(|row| row.turn_uid.as_str()).collect();
    let source_ms = batch.rows.iter().map(|row| row.created_at_ms).max().unwrap_or(0);
    json!({
        "turn_uids": turn_uids,
        "source_agent": ctx.source_agent,
        "device_id": ctx.device_id,
        "session_key": batch.session_key,
        "source_ms": source_ms,
    })
}

/// 拼 `review.pass.v1` 信封。`extras` 只有 event 用（与 `items` 等长）。
#[allow(clippy::too_many_arguments)]
pub fn pass_payload(
    pass_id: &str,
    family: &str,
    run_id: &str,
    batch: &Batch,
    ctx: &rc_memory::BatchCtx,
    owner: &rc_memory::owner::OwnerCtx,
    scope: &rc_memory::Scope,
    items: Value,
    extras: Option<Value>,
) -> Value {
    let mut payload = json!({
        "schema": REVIEW_SCHEMA,
        "pass_id": pass_id,
        "family": family,
        "run_id": run_id,
        "batch": batch_ref(batch, ctx),
        "owner": owner_value(owner, scope),
        "items": items,
    });
    if let Some(extras) = extras {
        payload["extras"] = extras;
    }
    payload
}

/// 构造一条待入队的载荷（`source=distill`，session/device 取批）。
///
/// H1c：`batch_key` 按设计 §1 现算（`pass_id`/`family`/`session_key`/`device_id`/排序去重后的
/// 轮集合）。它不看 `payload`——同一批换模型、换 prompt、输出走样，仍是同一批，
/// `enqueue` 因此用顶替而不是并存。
pub fn new_item(
    pass_id: &str,
    family: &str,
    batch: &Batch,
    owner_json: String,
    payload: &Value,
) -> NewItem {
    let turn_uids: Vec<&str> = batch.rows.iter().map(|row| row.turn_uid.as_str()).collect();
    NewItem {
        source: "distill".to_string(),
        pass_id: pass_id.to_string(),
        family: family.to_string(),
        session_key: batch.session_key.clone(),
        device_id: batch.device_id.clone(),
        turn_uids_json: json!(turn_uids).to_string(),
        owner_json,
        payload_json: payload.to_string(),
        batch_key: rc_memory::review::batch_key(
            pass_id,
            family,
            &batch.session_key,
            &batch.device_id,
            &turn_uids,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn decision(
    stage: ReviewStage,
    verdict: ReviewVerdict,
    reason_code: &str,
    model: &str,
    prompt_version: &str,
    input_sha256: &str,
    review_input_sha256: &str,
    decider: Decider,
    detail: &str,
) -> Decision {
    Decision {
        stage,
        verdict,
        reason_code: reason_code.to_string(),
        rules_version: GATE_RULES_VERSION.to_string(),
        model: model.to_string(),
        prompt_version: prompt_version.to_string(),
        input_sha256: input_sha256.to_string(),
        review_input_sha256: review_input_sha256.to_string(),
        decider,
        detail: detail.to_string(),
    }
}

/// 程序闸的判定：`decider=program`、`model/prompt_version` 为空（设计 §6 的 time 行同形）。
/// H4b：`input_sha256` 是载荷指纹；程序裁定没有送审输入 ⇒ `review_input_sha256` 恒为空串。
pub fn record_program(
    m: &MemoryHandle,
    id: i64,
    verdict: ReviewVerdict,
    reason_code: &str,
    input_sha256: &str,
    now_ms: i64,
) -> Result<ReviewState, MemError> {
    record_decision(
        m,
        id,
        &decision(
            ReviewStage::Program,
            verdict,
            reason_code,
            "",
            "",
            input_sha256,
            "",
            Decider::Program,
            "",
        ),
        now_ms,
    )
}

/// LLM 窄审的判定：`decider=center`，模型与 prompt 版本都落库。
///
/// H4b：两个指纹分列——`payload_sha256` 进 `input_sha256`（载荷指纹，与 program/apply 同格），
/// `review_input_sha256` 是这次送审的 `extraction_fingerprint(pipeline, model, system, user)`。
/// 两列各管一份输入：改了载荷，第一列变；换了模型/prompt，第二列变。
#[allow(clippy::too_many_arguments)]
pub fn record_review(
    m: &MemoryHandle,
    id: i64,
    verdict: ReviewVerdict,
    reason_code: &str,
    detail: &str,
    model: &str,
    prompt_version: &str,
    payload_sha256: &str,
    review_input_sha256: &str,
    now_ms: i64,
) -> Result<ReviewState, MemError> {
    record_decision(
        m,
        id,
        &decision(
            ReviewStage::Review,
            verdict,
            reason_code,
            model,
            prompt_version,
            payload_sha256,
            review_input_sha256,
            Decider::Center,
            detail,
        ),
        now_ms,
    )
}

/// **S3a 的临时口径**：程序闸 pass 之后没有独立窄审，直接记一条
/// `stage=review, verdict=pass, decider=program, reason_code=program_only`，把 item 推进
/// `approved`。`model`/`prompt_version` 都是空串——这条判定里没有模型参与。
///
/// H4b：`input_sha256` 仍是载荷指纹；没有送审输入 ⇒ `review_input_sha256` 为空串。
///
/// 这是 S3a 的临时口径：S3b 已给 naming 补了"canonical/alias 是否逐字且同指"的独立窄审
/// （设计 §6 的 naming 窄审 prompt，当前版本见 [`PASS_NAMING_REVIEW_VERSION`]），
/// naming 的调用点已改用 [`record_review`]。本封装只留给 time（无模型面、无窄审）；
/// 新增带窄审的 pass 不得再用它。
pub fn record_program_only_review(
    m: &MemoryHandle,
    id: i64,
    input_sha256: &str,
    now_ms: i64,
) -> Result<ReviewState, MemError> {
    record_decision(
        m,
        id,
        &decision(
            ReviewStage::Review,
            ReviewVerdict::Pass,
            PROGRAM_ONLY,
            "",
            "",
            input_sha256,
            "",
            Decider::Program,
            PROGRAM_ONLY_DETAIL,
        ),
        now_ms,
    )
}

/// 写库成功的判定：`stage=apply, verdict=pass`，`detail` 只放计数（≤512）。
/// H4b：`input_sha256` 是载荷指纹；写库不是送审 ⇒ `review_input_sha256` 恒为空串。
pub fn record_apply(
    m: &MemoryHandle,
    id: i64,
    detail: &str,
    model: &str,
    prompt_version: &str,
    input_sha256: &str,
    now_ms: i64,
) -> Result<ReviewState, MemError> {
    record_decision(
        m,
        id,
        &decision(
            ReviewStage::Apply,
            ReviewVerdict::Pass,
            APPLIED,
            model,
            prompt_version,
            input_sha256,
            "",
            Decider::Center,
            detail,
        ),
        now_ms,
    )
}

/// 这个 item 是否已经记过**item 级**的程序判定（只读）。重跑同一载荷时用它避免重复记账。
///
/// C3 起 `review_decision` 里还有"逐 draft 拒收"的审计行（`stage=program / verdict=reject`，
/// `detail` 是 `ref=<f1|e1|s1|a1|…>`）。它们不是这条 item 的闸结论，因此**不算**
/// "程序阶段已表态"——否则一次 LLM 故障之后（闸判完、审计行已落、item 仍 pending），
/// 重跑的 runner 会把批级或全拒的早退整段跳过去。
pub fn program_decided(m: &MemoryHandle, id: i64) -> Result<bool, DistillError> {
    Ok(decisions(m, id)?
        .iter()
        .any(|row| row.stage == ReviewStage::Program && !row.is_gate_reject_audit()))
}

/// 从 review 判定 detail 重建的逐条判决：只带草稿 ref，不带正文。
#[derive(Debug, Clone, Default)]
pub struct RebuiltVerdicts {
    pub pass: HashSet<String>,
    pub uncertain: HashSet<String>,
    /// G1 刀 B：审核价值轴判 `demote` 的草稿 ref。旧 detail 没有这一格 ⇒ 空集
    /// （= 全部 keep，不盖章），不是"重建失败"：价值轴是后加的，旧判定本来就没问过。
    pub demote: HashSet<String>,
}

/// 拼 review 判定的 detail：`p=..;r=..;u=..;pass=<refs|->;uncertain=<refs|->[;demote=<refs>]`。
///
/// `pass`/`uncertain` 是草稿 ref（`f{n}`/`e{n}`），升序、逗号分隔、空集合写 `-`；
/// 只放码/计数/ref，不带正文。超过 `MAX_DETAIL_CHARS` 时**不截断**——截断会把
/// "哪些草稿 pass"读成另一个集合——改写成 `!overflow`，重建路径据此 fail-closed。
///
/// G1 刀 B：价值轴的 `demote` 段**只在非空时追加**——既有 detail 的逐字形状（一串用例
/// 钉着 `p=..;r=..;u=..;pass=..;uncertain=..`）在没有 demote 时一个字节都不动。
pub fn review_detail(
    p: usize,
    r: usize,
    u: usize,
    pass_refs: &[String],
    uncertain_refs: &[String],
    demote_refs: &[String],
) -> String {
    let mut detail = format!(
        "p={p};r={r};u={u};pass={};uncertain={}",
        ref_list(pass_refs),
        ref_list(uncertain_refs)
    );
    if !demote_refs.is_empty() {
        detail.push_str(&format!(";demote={}", ref_list(demote_refs)));
    }
    if detail.chars().count() > rc_memory::review::MAX_DETAIL_CHARS {
        return format!("p={p};r={r};u={u};pass=!overflow;uncertain=!overflow");
    }
    detail
}

/// ref 列表：升序（数字后缀按数值比较，`f2` 在 `f10` 之前）、逗号分隔；空集合 `-`。
fn ref_list(refs: &[String]) -> String {
    if refs.is_empty() {
        return "-".to_string();
    }
    let mut sorted: Vec<&String> = refs.iter().collect();
    sorted.sort_by_key(|reference| ref_key(reference));
    sorted
        .iter()
        .map(|reference| reference.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// 排序键：`(前缀, 数字后缀)`。没有数字后缀时数值位取 `u64::MAX`（排在同前缀的数字之后）。
fn ref_key(reference: &str) -> (String, u64) {
    let split = reference
        .char_indices()
        .rev()
        .take_while(|(_, ch)| ch.is_ascii_digit())
        .last()
        .map(|(index, _)| index)
        .unwrap_or(reference.len());
    let (prefix, digits) = reference.split_at(split);
    (prefix.to_string(), digits.parse().unwrap_or(u64::MAX))
}

/// 解析 review detail 的 `pass=`/`uncertain=`。读不出（旧格式缺键、`!overflow`、空值、
/// 集合内/跨集合重复）一律 `None`：调用方据此走 fail-closed，不猜判决。
pub fn parse_review_detail(detail: &str) -> Option<RebuiltVerdicts> {
    let mut pass: Option<HashSet<String>> = None;
    let mut uncertain: Option<HashSet<String>> = None;
    // G1 刀 B：`demote=` 可缺（旧 detail 没问过价值轴）。缺 ⇒ 空集。
    let mut demote: Option<HashSet<String>> = None;
    for part in detail.split(';') {
        let (key, value) = part.split_once('=')?;
        match key {
            "pass" => {
                if pass.is_some() {
                    return None;
                }
                pass = Some(ref_set(value)?);
            }
            "uncertain" => {
                if uncertain.is_some() {
                    return None;
                }
                uncertain = Some(ref_set(value)?);
            }
            "demote" => {
                if demote.is_some() {
                    return None;
                }
                demote = Some(ref_set(value)?);
            }
            _ => {}
        }
    }
    let pass = pass?;
    let uncertain = uncertain?;
    if pass.iter().any(|reference| uncertain.contains(reference)) {
        return None;
    }
    Some(RebuiltVerdicts { pass, uncertain, demote: demote.unwrap_or_default() })
}

fn ref_set(value: &str) -> Option<HashSet<String>> {
    if value == "!overflow" {
        return None;
    }
    if value == "-" {
        return Some(HashSet::new());
    }
    if value.is_empty() {
        return None;
    }
    let mut set = HashSet::new();
    for reference in value.split(',') {
        if reference.is_empty() || !set.insert(reference.to_string()) {
            return None;
        }
    }
    Some(set)
}

/// G1 刀 B：最近一条**能解析**的 review 判定里记的价值轴 demote 集。
///
/// §7.3b 的单条重跑不重问价值轴（它只问对错），但它写库时那条轴的答案仍是前一次判定
/// 给出的——重跑不该悄悄把一条 demote 行升回 keep。`retry` 自己的 detail 是另一种形状
/// （`RetryMetrics::detail`），`parse_review_detail` 读不出就跳过，继续往前找。
/// 没有任何可解析的判定 ⇒ 空集（旧数据 = 全部 keep，不盖章）。
pub fn latest_demote(m: &MemoryHandle, id: i64) -> Result<HashSet<String>, MemError> {
    for row in decisions(m, id)?.iter().rev() {
        if row.stage != ReviewStage::Review {
            continue;
        }
        if let Some(rebuilt) = parse_review_detail(&row.detail) {
            return Ok(rebuilt.demote);
        }
    }
    Ok(HashSet::new())
}

/// 取**最后一条** `stage=review && verdict=pass` 的判定并解析逐条判决；ref 必须全部
/// 对应这批草稿，否则 `None`（缺失/不可读/对不上都走同一条 fail-closed）。
pub fn persisted_verdicts(
    m: &MemoryHandle,
    id: i64,
    draft_refs: &[&str],
) -> Result<Option<RebuiltVerdicts>, MemError> {
    let rows = decisions(m, id)?;
    let Some(row) = rows
        .iter()
        .rev()
        .find(|row| row.stage == ReviewStage::Review && row.verdict == ReviewVerdict::Pass)
    else {
        return Ok(None);
    };
    let Some(rebuilt) = parse_review_detail(&row.detail) else {
        return Ok(None);
    };
    let known: HashSet<&str> = draft_refs.iter().copied().collect();
    if rebuilt
        .pass
        .iter()
        .chain(rebuilt.uncertain.iter())
        .chain(rebuilt.demote.iter())
        .any(|reference| !known.contains(reference.as_str()))
    {
        return Ok(None);
    }
    Ok(Some(rebuilt))
}
