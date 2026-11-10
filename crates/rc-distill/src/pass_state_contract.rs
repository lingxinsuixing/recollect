//! P2 状态 pass 的 parser：抽取侧（严格 + 宽松）与审核侧（方案
//! `design/2026-10-31-state-pass-p2.md` §3–§4）。
//!
//! 这一层只做"模型给什么形状、程序怎么判"，不选批、不调模型、不写库。
//! 与 event 的关系同 fact↔event 那条纪律：**规则同形但不复用实现**——两个 pass 各自可
//! 单独重试、归因、回滚，一处改动不该同时改变另一方的拒收面。共用的只有 `prepare` 那把
//! 剪刀、`json_payload` 那层剥壳、以及 `ReviewVerdict` 的三分支。
//!
//! 映射规则（方案 §3）：
//! - `t1..tN` = `batch.rows` 顺序；`t*` 只在这里存在，程序把它换成真实 `turn_uid`；
//! - `a1..aN` = `candidates` 顺序；`subject` 是 `a*`，程序换成 `anchor_id`；
//! - `evidence` 只许目标 `t*`，且必须包含本条自己的 `turn`；
//! - 每个目标轮必须落在某条状态的 `turn`/`evidence` 或 `empty_turns` 里。
//!
//! **`mode` 由程序判，模型不声明**（方案 §3/§4）：这一片的 `expressed` 有一条可执行的判据
//! ——`value_after` 必须是 `quote` 的逐字子串。直陈出来的值本来就在那句话里；需要换个词
//! 才说得出来的值（"他看起来压力大"→ `紧张`）就是推导，不是直陈。所以：
//! - `value_after ⊂ quote` ⇒ `mode='expressed'`，进审核与落库；
//! - `value_after ⊄ quote` ⇒ `mode='inferred'`，进**隔离**并计数，不落库（v1 边界）。
//!
//! 这条判据同时把"模型写了个库里查不到的值"挡在落库之前，代价是少数同义表述会进隔离——
//! 隔离占比就是方案 §7 要报的那一格读数，用它决定要不要放开，不预先放开。
//!
//! 另一条只有状态 pass 才需要的判据是**陈述性**（`not_assertive`）：问句与宣布不陈述
//! 任何当前状态（「你是不是生气了？」「现在我去跑测试」）。方案 §7 的阴性对照要求
//! "喂纯提问/纯宣布轮，证明闸会拒"，这一格就是它拒的地方。

use crate::batch::{Batch, TurnRow};
use crate::pass_fact_contract::{json_payload, ReviewVerdict};
use crate::pass_prompt::{
    FactCandidate, PASS_STATE, PASS_STATE_REVIEW, STATE_DIMENSIONS, STATE_MODE_EXPRESSED,
    STATE_REVIEW_CODES, STATE_SCHEMA_VERSION,
};
use crate::prepare::evidence_view;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

/// `quote` 的字符上限，与 fact/event 同一个数。常量各留一份：三个 pass 的拒收面必须能
/// 独立演进（改状态的上限不该悄悄改掉 event 的）。
pub const MAX_QUOTE_CHARS: usize = 120;

/// 模型可写的一条状态草稿。短引用已在这里换成真实 uid / anchor_id。
///
/// `subject_canonical` 是**只给审核 prompt 渲染用**的（与 `EventDraft::actor_canonical`
/// 同一条纪律），不进写库层——写库层的 `text` 由程序按同一份 canonical 组装。
/// `mode` 在这一层由程序判出来（见模块文档），模型给不出这一格。
#[derive(Debug, Clone, PartialEq)]
pub struct StateDraft {
    pub reference: String,
    pub turn_uid: String,
    pub subject_anchor_id: String,
    pub subject_canonical: String,
    /// 九值闭集，家在本 crate 的 `pass_prompt::STATE_DIMENSIONS`（与 DDL 的 CHECK 对拍）。
    pub dimension: String,
    /// 逐字取自引文的当前值。
    pub value_after: String,
    /// `expressed`（由程序判，见模块文档）。
    pub mode: &'static str,
    pub quote: String,
    pub evidence_turn_uids: Vec<String>,
}

/// 拒收原因。每一格一个稳定 slug（[`StatePassError::slug`]），runner 把它写进
/// `extract_pass_state.reason`；审核侧共用同一张表，前缀由 [`StatePassError::review_reason`] 补。
///
/// slug 不带 `state_` 前缀：`reason` 与 `pass_id` 同一行，前缀是冗余（与 event/fact 的
/// 既有 slug 同一把尺子）。方案 §4 表里写的是带前缀的名字，差异记在台账 §10.116。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatePassError {
    /// 不是 JSON、不是对象、缺键、多键、字段类型错、空必填串（`ref`/`subject`/`value_after`）。
    Json,
    /// `schema_version` 不是 0。
    Version,
    /// `pass` 不是 `"state"`（审核侧是 `"state_review"`）。
    Pass,
    /// `turn` / `empty_turns` 引用了批外的 `t*`；审核侧多给了待审集合外的 `ref`。
    UnknownTurn(String),
    /// `subject` 引用了候选表外的 `a*`。
    UnknownCandidate(String),
    /// `dimension` 不在九值闭集里。
    Dimension,
    /// `value_after` 为空或纯标点。
    ValueEmpty,
    /// 引文不是陈述句：问句，或"现在/先/接下来…"的纯宣布（方案 §7 的阴性对照走这一格）。
    NotAssertive,
    /// `evidence` 为空。
    EmptyEvidence,
    /// `evidence` 里有非目标引用；或 evidence 不含本条自己的 `turn`。
    EvidenceNotTarget(String),
    /// 批内短引用重复（`states[].ref`、`empty_turns[]` 或审核的 `ref`）。
    DuplicateRef(String),
    /// `quote` 不是本条目标轮 `prepare` 正文的逐字子串（空 quote 也归这里）。
    QuoteNotVerbatim(String),
    /// `quote` 超过 120 字符。
    QuoteTooLong,
    /// `quote` 含省略号（`…` 或 `...`）。
    Ellipsis,
    /// 有目标轮既没进状态的 `turn`/`evidence`，也没进 `empty_turns`。
    Uncovered(Vec<String>),
}

impl StatePassError {
    /// 进 `extract_pass_state.reason` 的封闭 slug。这里**故意不写 `_ =>`**（与
    /// `EventPassError::slug` 同一条纪律）：新增变体必须在这张表里表态。
    pub fn slug(&self) -> &'static str {
        match self {
            StatePassError::Json => "json",
            StatePassError::Version => "version",
            StatePassError::Pass => "pass",
            StatePassError::UnknownTurn(_) => "unknown_turn",
            StatePassError::UnknownCandidate(_) => "unknown_candidate",
            StatePassError::Dimension => "dimension_invalid",
            StatePassError::ValueEmpty => "value_empty",
            StatePassError::NotAssertive => "not_assertive",
            StatePassError::EmptyEvidence => "empty_evidence",
            StatePassError::EvidenceNotTarget(_) => "evidence_not_target",
            StatePassError::DuplicateRef(_) => "duplicate_ref",
            StatePassError::QuoteNotVerbatim(_) => "quote_not_verbatim",
            StatePassError::QuoteTooLong => "quote_too_long",
            StatePassError::Ellipsis => "ellipsis",
            StatePassError::Uncovered(_) => "uncovered",
        }
    }

    /// 审核那一支失败落库用的 reason：`review_` + 同一格。
    pub fn review_reason(&self) -> String {
        let slug = self.slug();
        if slug.starts_with("review_") {
            slug.to_string()
        } else {
            format!("review_{slug}")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStateOutput {
    schema_version: u32,
    pass: String,
    states: Vec<RawState>,
    empty_turns: Vec<String>,
}

/// 七个键，逐一对应方案 §3 的输出形状。除 `ref/turn/subject/dimension/value_after/quote/
/// evidence` 外一律不许出现：`mode`/`confidence`/`valid_from_ms`/`id`/`superseded_by`
/// 多一个就当场拒（`deny_unknown_fields`）——它们全是程序列。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawState {
    #[serde(rename = "ref")]
    reference: String,
    turn: String,
    subject: String,
    dimension: String,
    value_after: String,
    quote: String,
    evidence: Vec<String>,
}

/// 短 id 映射表：`t1..tN` = `batch.rows` 顺序、`a1..aN` = `candidates` 顺序。
struct StateRefs<'a> {
    targets: HashMap<String, &'a TurnRow>,
    candidates: HashMap<String, &'a FactCandidate>,
}

impl<'a> StateRefs<'a> {
    fn new(batch: &'a Batch, candidates: &'a [FactCandidate]) -> Self {
        StateRefs {
            targets: batch
                .rows
                .iter()
                .enumerate()
                .map(|(index, row)| (format!("t{}", index + 1), row))
                .collect(),
            candidates: candidates
                .iter()
                .enumerate()
                .map(|(index, candidate)| (format!("a{}", index + 1), candidate))
                .collect(),
        }
    }
}

/// 信封三格：形状（`Json`）→ 版本 → pass。
fn state_envelope(raw: &str) -> Result<RawStateOutput, StatePassError> {
    let doc: RawStateOutput =
        serde_json::from_str(json_payload(raw)).map_err(|_| StatePassError::Json)?;
    if doc.schema_version != STATE_SCHEMA_VERSION {
        return Err(StatePassError::Version);
    }
    if doc.pass != PASS_STATE {
        return Err(StatePassError::Pass);
    }
    Ok(doc)
}

/// `value_after` 是否逐字出现在 `quote` 里（两侧都过 `evidence_view`，与引文闸同一把剪刀：
/// 模型系统性剥 `**`/反引号，那是表征差异）。为真 ⇒ `expressed`。
fn value_in_quote(quote: &str, value: &str) -> bool {
    if value.trim().is_empty() {
        return false;
    }
    evidence_view(quote).contains(&evidence_view(value))
}

/// 陈述性判定（`not_assertive`）。三条确定性规则，只看引文字面：
/// 1. 问号（全角/半角）在句尾或句中出现 ⇒ 疑问，不陈述任何状态；
/// 2. 句中有"是不是 / 有没有 / 会不会 / 难道 / 吗"这类疑问标记 ⇒ 同上；
/// 3. 句首是计划语气词（现在/先/接下来/接着/然后/我再/我来/我去/准备…）且整句没有任何
///    状态直陈标记 ⇒ 纯宣布。
///
/// 与 event 的 `announcement_only` 同源同档，但**判据不同**：event 问"动作发生没有"，
/// 这里问"这句话陈述了一个当前状态没有"。所以状态侧的"直陈标记"是系词/程度/变化词，
/// 而不是 event 的完成标记。
///
/// 这是**减误收**的闸：它放过的不一定对，只是把最不像陈述的那批挡掉。
fn is_assertive(quote: &str) -> bool {
    let text = quote.trim();
    if text.is_empty() {
        return false;
    }
    if text.contains('？') || text.contains('?') {
        return false;
    }
    const QUESTION_MARKERS: [&str; 6] = ["是不是", "有没有", "会不会", "难道", "吗", "为何"];
    if QUESTION_MARKERS.iter().any(|marker| text.contains(marker)) {
        return false;
    }
    const ANNOUNCEMENT_HEADS: [&str; 12] = [
        "现在", "先", "接下来", "接着", "然后", "最后", "准备", "将要", "即将", "下一步",
        "我来", "我去",
    ];
    // "在" 单独不算标记：它出现在"现在/接着"这类计划语气里，会把宣布放过去。
    const ASSERTION_MARKERS: [&str; 12] = [
        "感到", "觉得", "感觉", "是", "有", "很", "有点", "有些", "不太", "越来越", "还是",
        "正在",
    ];
    let head = text.trim_start_matches(|c: char| {
        c.is_whitespace() || "，。；;：:,、！!".contains(c)
    });
    let announced = ANNOUNCEMENT_HEADS.iter().any(|marker| head.starts_with(marker));
    !(announced && !ASSERTION_MARKERS.iter().any(|marker| text.contains(marker)))
}

/// **单条**状态的全部内容校验。
///
/// 顺序固定：turn → subject → dimension → value → quote → 陈述性 → evidence。
/// `ref` 的非空与批内唯一由调用方管。
/// 返回 `Err(inferred)` 的判据不在错误里：`expressed/inferred` 是**处置**而不是拒收，
/// 所以它由 [`parse_state_output_lenient`] 用 [`is_expressed`] 单独分岔，见那里的说明。
fn validate_state(
    state: &RawState,
    refs: &StateRefs<'_>,
) -> Result<StateDraft, StatePassError> {
    let turn_row = match refs.targets.get(state.turn.as_str()) {
        Some(row) => *row,
        None => return Err(StatePassError::UnknownTurn(state.turn.clone())),
    };
    if !STATE_DIMENSIONS.contains(&state.dimension.as_str()) {
        return Err(StatePassError::Dimension);
    }
    if state.subject.trim().is_empty() {
        return Err(StatePassError::Json);
    }
    let candidate = match refs.candidates.get(state.subject.as_str()) {
        Some(candidate) => *candidate,
        None => return Err(StatePassError::UnknownCandidate(state.subject.clone())),
    };
    let value_after = state.value_after.trim();
    if value_after.is_empty() {
        return Err(StatePassError::ValueEmpty);
    }
    let quote = state.quote.trim();
    if quote.is_empty() {
        return Err(StatePassError::QuoteNotVerbatim(state.reference.clone()));
    }
    if quote.chars().count() > MAX_QUOTE_CHARS {
        return Err(StatePassError::QuoteTooLong);
    }
    // 引文逐字命中**目标轮**正文（与 event 同一把剪刀：两侧都过 evidence_view）。
    if !evidence_view(&turn_row.body).contains(&evidence_view(quote)) {
        if quote.contains('…') || quote.contains("...") {
            return Err(StatePassError::Ellipsis);
        }
        return Err(StatePassError::QuoteNotVerbatim(state.reference.clone()));
    }
    if !is_assertive(quote) {
        return Err(StatePassError::NotAssertive);
    }
    if state.evidence.is_empty() {
        return Err(StatePassError::EmptyEvidence);
    }
    let mut evidence_uids: Vec<String> = Vec::with_capacity(state.evidence.len());
    for reference in &state.evidence {
        match refs.targets.get(reference.as_str()) {
            Some(evidence_row) => evidence_uids.push(evidence_row.turn_uid.clone()),
            None => return Err(StatePassError::UnknownTurn(reference.clone())),
        }
    }
    // evidence 必须包含本条自己的 turn（与 event 同一条纪律）。
    if !evidence_uids.iter().any(|uid| uid == &turn_row.turn_uid) {
        return Err(StatePassError::EvidenceNotTarget(state.turn.clone()));
    }
    Ok(StateDraft {
        reference: state.reference.clone(),
        turn_uid: turn_row.turn_uid.clone(),
        subject_anchor_id: candidate.anchor_id.clone(),
        subject_canonical: candidate.canonical.clone(),
        dimension: state.dimension.clone(),
        value_after: value_after.to_string(),
        mode: STATE_MODE_EXPRESSED,
        quote: quote.to_string(),
        evidence_turn_uids: evidence_uids,
    })
}

/// `expressed` 的程序判据（模块文档那条）：值逐字在引文里。
pub fn is_expressed(draft: &StateDraft) -> bool {
    value_in_quote(&draft.quote, &draft.value_after)
}

fn validate_empty_turns(
    empty_turns: &[String],
    refs: &StateRefs<'_>,
) -> Result<Vec<String>, StatePassError> {
    let mut kept: Vec<String> = Vec::with_capacity(empty_turns.len());
    let mut seen_empty: HashSet<&str> = HashSet::new();
    for reference in empty_turns {
        if !seen_empty.insert(reference.as_str()) {
            return Err(StatePassError::DuplicateRef(reference.clone()));
        }
        if !refs.targets.contains_key(reference.as_str()) {
            return Err(StatePassError::UnknownTurn(reference.clone()));
        }
        kept.push(reference.clone());
    }
    Ok(kept)
}

/// 覆盖缺口：`t1..tN` 里既不在 `covered`、也不在 `empty_turns` 里的短 id。
fn uncovered_turns(rows: usize, covered: &HashSet<String>) -> Vec<String> {
    (1..=rows)
        .map(|index| format!("t{index}"))
        .filter(|reference| !covered.contains(reference))
        .collect()
}

/// 宽松解析的产物。`drafts` 是**可落库**的（`expressed`）；`inferred` 是形状与引文都对、
/// 但值不在引文里的那些——它们不是拒收，是隔离（方案 §4 的 `inferred_deferred`）。
#[derive(Debug, Clone, PartialEq)]
pub struct LenientStates {
    pub drafts: Vec<StateDraft>,
    /// 隔离的推导草稿（已映射回真实 uid/anchor_id，只用于计数与逐轮 `quarantined`）。
    pub inferred: Vec<StateDraft>,
    pub dropped: Vec<StatePassError>,
    /// 被丢草稿的 `(turn t-ref, slug)`：run 侧按批内 t-ref 映射回 `turn_uid` 写进
    /// `pass_state.reason`。
    pub dropped_turns: Vec<(String, &'static str)>,
    /// 被隔离草稿的 `(turn t-ref)`：run 侧映射回真实轮并落 `quarantined(inferred_deferred)`。
    pub inferred_turns: Vec<String>,
    pub uncovered: Vec<String>,
}

/// 宽松解析一次状态 pass 的输出。
///
/// **整批仍拒**的只有信封与批次级声明：形状/多键、`schema_version`、`pass`、`empty_turns`
/// 的重复与批外引用。单条坏在哪一格只丢这一条。
/// 单条**合法但是推导**（值不在引文里）的不丢也不留：它进 `inferred`，占住这一轮的覆盖，
/// 但不进审核、不落库。
pub fn parse_state_output_lenient(
    raw: &str,
    batch: &Batch,
    candidates: &[FactCandidate],
) -> Result<LenientStates, StatePassError> {
    let doc = state_envelope(raw)?;
    let refs = StateRefs::new(batch, candidates);
    let empty_turns = validate_empty_turns(&doc.empty_turns, &refs)?;

    let mut covered: HashSet<String> = empty_turns.into_iter().collect();
    let mut drafts: Vec<StateDraft> = Vec::with_capacity(doc.states.len());
    let mut inferred: Vec<StateDraft> = Vec::new();
    let mut dropped: Vec<StatePassError> = Vec::new();
    let mut dropped_turns: Vec<(String, &'static str)> = Vec::new();
    let mut inferred_turns: Vec<String> = Vec::new();
    let mut seen_refs: HashSet<String> = HashSet::new();

    for state in &doc.states {
        if state.reference.trim().is_empty() {
            dropped_turns.push((state.turn.clone(), StatePassError::Json.slug()));
            dropped.push(StatePassError::Json);
            continue;
        }
        if seen_refs.contains(state.reference.as_str()) {
            dropped_turns.push((
                state.turn.clone(),
                StatePassError::DuplicateRef(state.reference.clone()).slug(),
            ));
            dropped.push(StatePassError::DuplicateRef(state.reference.clone()));
            continue;
        }
        match validate_state(state, &refs) {
            Ok(draft) => {
                seen_refs.insert(state.reference.clone());
                covered.insert(state.turn.clone());
                covered.extend(state.evidence.iter().cloned());
                if is_expressed(&draft) {
                    drafts.push(draft);
                } else {
                    inferred_turns.push(state.turn.clone());
                    inferred.push(draft);
                }
            }
            Err(error) => {
                dropped_turns.push((state.turn.clone(), error.slug()));
                dropped.push(error);
            }
        }
    }

    let uncovered = uncovered_turns(batch.rows.len(), &covered);
    Ok(LenientStates {
        drafts,
        inferred,
        dropped,
        dropped_turns,
        inferred_turns,
        uncovered,
    })
}

/// 严格解析一次状态 pass 的输出（任何违规整批 `Err`）。
///
/// 与 event 的同名函数一样，它只服务测试与"要么全成要么全不"的调用方；runner 走
/// [`parse_state_output_lenient`]。
///
/// 严格侧**不分岔 inferred**：v1 只让 `expressed` 落库，而"整批要么全合法"的语义里没有
/// 任何一格能表达"这条要隔离"。折成任何一个拒收码都会谎报坏在哪一格，所以裁定是：
/// 出现推导草稿就整批拒，slug 用最贴近的 `quote_not_verbatim`（引文没把那个值撑住），
/// 载荷给那条的 ref。宽松侧才是隔离的住处。
pub fn parse_state_output(
    raw: &str,
    batch: &Batch,
    candidates: &[FactCandidate],
) -> Result<Vec<StateDraft>, StatePassError> {
    let doc = state_envelope(raw)?;
    let refs = StateRefs::new(batch, candidates);
    let mut drafts: Vec<StateDraft> = Vec::with_capacity(doc.states.len());
    let mut seen_refs: HashSet<&str> = HashSet::new();
    for state in &doc.states {
        if state.reference.trim().is_empty() {
            return Err(StatePassError::Json);
        }
        if !seen_refs.insert(state.reference.as_str()) {
            return Err(StatePassError::DuplicateRef(state.reference.clone()));
        }
        let draft = validate_state(state, &refs)?;
        // 严格侧的裁定：**推导不是这一批的合法输出**。v1 只让 `expressed` 落库，
        // 而"整批要么全合法"的语义里没有任何一格能表达"这条要隔离"——把它折成任何
        // 拒收码都会谎报坏在哪一格。所以严格侧对推导整批拒，slug 用最贴近的
        // `quote_not_verbatim`（值不逐字在引文里 = 引文没把它撑住），载荷给那条的 ref。
        if !is_expressed(&draft) {
            return Err(StatePassError::QuoteNotVerbatim(state.reference.clone()));
        }
        drafts.push(draft);
    }
    let empty_turns = validate_empty_turns(&doc.empty_turns, &refs)?;
    let mut covered: HashSet<String> = empty_turns.into_iter().collect();
    for state in &doc.states {
        covered.insert(state.turn.clone());
        covered.extend(state.evidence.iter().cloned());
    }
    let missing = uncovered_turns(batch.rows.len(), &covered);
    if !missing.is_empty() {
        return Err(StatePassError::Uncovered(missing));
    }
    Ok(drafts)
}

// ── 状态窄审的 parser ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStateReviewOutput {
    schema_version: u32,
    pass: String,
    verdicts: Vec<RawReviewVerdict>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReviewVerdict {
    #[serde(rename = "ref")]
    reference: String,
    verdict: String,
    /// `null` 才是"没有 code"，缺键是形状错（与 event 审核同一条理由）。
    code: serde_json::Value,
}

/// 解析一次状态审核的输出。`refs` 是调用方按 prompt 里 `ITEM s1..sN` 的顺序给出的待审引用。
/// 返回顺序**跟 `refs` 走**，不跟模型回复顺序走。
pub fn parse_state_review(
    raw: &str,
    refs: &[String],
) -> Result<Vec<(String, ReviewVerdict, Option<String>)>, StatePassError> {
    let doc: RawStateReviewOutput =
        serde_json::from_str(json_payload(raw)).map_err(|_| StatePassError::Json)?;
    if doc.schema_version != STATE_SCHEMA_VERSION {
        return Err(StatePassError::Version);
    }
    if doc.pass != PASS_STATE_REVIEW {
        return Err(StatePassError::Pass);
    }
    let mut seen: HashMap<&str, (ReviewVerdict, Option<String>)> =
        HashMap::with_capacity(doc.verdicts.len());
    for item in &doc.verdicts {
        if item.reference.trim().is_empty() {
            return Err(StatePassError::Json);
        }
        if !refs.iter().any(|reference| reference == &item.reference) {
            return Err(StatePassError::UnknownTurn(item.reference.clone()));
        }
        if seen.contains_key(item.reference.as_str()) {
            return Err(StatePassError::DuplicateRef(item.reference.clone()));
        }
        let verdict = ReviewVerdict::parse(&item.verdict).ok_or(StatePassError::Json)?;
        let raw_code = match &item.code {
            serde_json::Value::Null => None,
            serde_json::Value::String(code) => Some(code.as_str()),
            _ => return Err(StatePassError::Json),
        };
        let code = match (verdict, raw_code) {
            (ReviewVerdict::Pass, None) => None,
            (ReviewVerdict::Uncertain, None) => None,
            (_, Some(code))
                if STATE_REVIEW_CODES.contains(&code) && verdict != ReviewVerdict::Pass =>
            {
                Some(code.to_string())
            }
            _ => return Err(StatePassError::Json),
        };
        seen.insert(item.reference.as_str(), (verdict, code));
    }
    let mut out = Vec::with_capacity(refs.len());
    let mut missing: Vec<String> = Vec::new();
    for reference in refs {
        match seen.get(reference.as_str()) {
            Some((verdict, code)) => out.push((reference.clone(), *verdict, code.clone())),
            None => missing.push(reference.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(StatePassError::Uncovered(missing));
    }
    Ok(out)
}
