//! S2b-1：fact pass 的严格 parser（设计 `2026-10-26-s2-prompt-v0` §2.2、§4 的程序闸）。
//!
//! 这一层的全部工作是"模型给什么形状、程序怎么拒"，不选批、不调模型、不写库。
//! 严格 parser（[`parse_fact_output`]）把任何违规都当**硬拒**：返回错误，不静默丢条、
//! 不猜默认值。错误载荷只带批内短引用与数字串，不回显模型正文（守卫 12）。
//!
//! S2e-8a 多一个**宽松** parser（[`parse_fact_output_lenient`]）：信封仍然整批拒，
//! 单条坏则只丢一条、其余照常返回，覆盖缺口改为如实列出而不折整批。两者共用同一份单条
//! 校验（[`validate_fact`]）——宽松不是第二套规则，只是对同一条规则的另一种处置。
//!
//! 映射规则（设计 §2.2）：
//! - `t1..tN` = `batch.rows` 顺序；`t*` 只在这里存在，程序把它换成真实 `turn_uid`；
//! - `a1..aN` = `candidates` 顺序；`a*` 只在 `subject`/`object` 出现，程序把它换成
//!   `anchor_id`；
//! - `evidence` 只许目标 `t*`，且必须包含本条自己的 `turn`；
//! - 每个目标轮必须落在某条 fact 的 `turn`/`evidence` 或 `empty_turns` 里。
//!
//! C6b：prompt 里的 `CTX [c*]` 邻轮**不是**映射面。`targets` 只由 `batch.rows` 建，
//! 邻轮不在 `batch.rows` 里 ⇒ `c*` 进 `turn`/`evidence` 就是 `unknown_turn`/
//! `evidence_not_target`；邻轮的句子也不是合法 `quote`（逐字检查只对照目标轮正文）。
//! 契约一个字不改：CTX 不进门，是因为它从来没有门。
//!
//! 空必填字符串（`ref`）按**形状**问题归 `Json`：错误码表里没有
//! 更细的一格，而把它塞进 `MissingSubject`/`MissingValue` 会让排障读错方向。
//! `text` 自 §10.47 起**可省**：弱模型的第一次真跑（Qwen3-4B）整批漏写它而被判 `json`，
//! 而 `text` 只是给人看的复述（判同、归属、证据都不用它），由程序拼 `主语：value` 更稳。
//! `time_text` **不是**必填串（S2e-4）：没有时间短语的轮次写 `null` 或干脆省略键，
//! 两种都读成空串，由写库层跳过 `time_expr`。真实的第一次冒烟就是栽在
//! `"time_text": null` 被当成必填字符串、整批 `json` 上。
//! `empty_turns` 的重复项复用 `DuplicateRef`：它同样是"批内短引用重复"。
//!
//! S2e-1：每个变体都有一格**稳定 slug**（[`FactPassError::slug`]），runner 把它写进
//! `extract_pass_state.reason`。slug 只描述"坏在哪一格"，载荷（短引用、`t*` 序号）不进
//! 这一列，模型原文更不进（守卫 12）。审核的六个自有变体自带 `review_` 前缀，与抽取的
//! 五格区分开；`review_reason()` 负责把两次调用共用的信封格也加上前缀。

use crate::batch::{Batch, TurnRow};
use crate::pass_prompt::{FactCandidate, FactFamily, ValueAxis, FACT_SCHEMA_VERSION, PASS_FACT,
                          PASS_FACT_REVIEW, REVIEW_CODES};
use crate::prepare::evidence_view;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

/// `quote` 的字符上限（设计 §2.2 / §4："≤120 字"）。
pub const MAX_QUOTE_CHARS: usize = 120;

/// `mode` 在 v0 只收这一个值（`reported`/`inferred` 留 S5b）。
pub const MODE_STATED: &str = "stated";

/// 模型可写的一条 fact 草稿。短引用已在这里换成真实 uid / anchor_id，
/// `predicate/category/mode` 是 family 的闭集值（不信模型自报）。
#[derive(Debug, Clone, PartialEq)]
pub struct FactDraft {
    pub reference: String,
    pub turn_uid: String,
    pub subject_anchor_id: String,
    pub object_anchor_id: Option<String>,
    /// subject 的 canonical 名字。**只给审核 prompt 渲染用**：审核不许看 anchor_id，
    /// 但"引文是否支持这个主语"必须看到名字。它由 parser 从候选表带过来，不进写库层。
    pub subject_canonical: String,
    /// object 的 canonical；object 缺席时 `None`。
    pub object_canonical: Option<String>,
    pub predicate: String,
    pub value: Option<String>,
    pub category: String,
    pub mode: String,
    pub time_text: String,
    pub quote: String,
    pub evidence_turn_uids: Vec<String>,
    pub text: String,
}

/// 拒收原因。同一格在严格 parser 里是"整批不出"的硬拒；在宽松 parser（S2e-8a）里，
/// 单条内容错只丢那一条并原样记进 [`LenientFacts::dropped`]——错误码不因处置方式变宽或变窄，
/// 也不新增自由文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactPassError {
    /// 不是 JSON、不是对象、缺键、多键、字段类型错、空必填串（`ref`；
    /// `time_text` 自 S2e-4 起可空、`text` 自 §10.47 起可省，都不进这一格）。
    Json,
    /// `schema_version` 不是 0。
    Version,
    /// `pass` 不是 `"fact"`。
    Pass,
    /// `family` 与本次调用要问的不一致。
    Family,
    /// `turn` / `empty_turns` 引用了批外的 `t*`。
    UnknownTurn(String),
    /// `subject` / `object` 引用了候选表外的 `a*`。
    UnknownCandidate(String),
    /// `category` 与 family 的闭集值不符。
    Category,
    /// `predicate` 与 family 的闭集值不符。
    Predicate,
    /// `mode` 不是 `"stated"`。
    Mode,
    /// `subject` 为空。
    MissingSubject,
    /// `object` 与 `value` 都为空。
    MissingValue,
    /// `evidence` 为空。
    EmptyEvidence,
    /// `evidence` 里有非目标引用；或 evidence 不含本条自己的 `turn`。
    EvidenceNotTarget(String),
    /// 批内短引用重复（`facts[].ref` 或 `empty_turns[]`）。
    DuplicateRef(String),
    /// `quote` 不是某个目标轮 `prepare` 正文的逐字子串（空 quote 也归这里）。
    QuoteNotVerbatim(String),
    /// `quote` 超过 120 字符。
    QuoteTooLong,
    /// `quote` 含省略号（`…` 或 `...`）。
    Ellipsis,
    /// `clause` 指向一个本批没有的编号。
    UnknownClause(String),
    /// `clause` 属于另一轮：程序切句是逐轮编号的，跨轮借用就是引错证据。
    ClauseTurnMismatch(String),
    /// `text` 里的数字串没有出现在 `quote` 里。
    NumberNotGrounded(String),
    /// 模型显式给的 `value` 不是 `quote` 的逐字子串。载荷是 `ref`（不回显模型写的值）。
    ValueNotInQuote(String),
    /// 有目标轮既没进 fact 的 `turn`/`evidence`，也没进 `empty_turns`；按 `t` 序号有序。
    Uncovered(Vec<String>),
    /// 审核的 `ref` 不在调用方给的 `refs` 里（多给了一行判决）。
    UnknownReviewRef(String),
    /// 审核的 `ref` 出现两次。
    DuplicateReviewRef(String),
    /// 有 `ref` 一条判决都没拿到；按 `refs` 顺序列出全部缺失项。
    MissingReviewRef(Vec<String>),
    /// `verdict` 不在 `pass|reject|uncertain` 闭集里。载荷是模型写的那个词。
    BadReviewVerdict(String),
    /// `code` 与 verdict 不配：pass 带 code、reject/uncertain 缺 code 或 code 在闭集外。
    /// 载荷是 `ref`（不把模型写的 code 回显进运维输出）。
    BadReviewCode(String),
    /// G1 刀 B：`value_axis` 不在 `keep|demote` 闭集里。载荷是 `ref`，
    /// 不回显模型写的词（与 [`BadReviewCode`](Self::BadReviewCode) 同一条理由）。
    BadValueAxis(String),
}

impl FactPassError {
    /// 进 `extract_pass_state.reason` 的封闭 slug（S2e-1）。抽取那一支是裸词，
    /// 审核自有的六格自带 `review_` 前缀（其中 `review_value_axis` 是价值轴那一格）——排障第一眼要分得出是第几次开口坏的。
    ///
    /// 这里**故意不写 `_ =>`**（与 `ContractError::slug` 同一条纪律）：新增变体必须在这张表里
    /// 表态。载荷一律忽略：短引用与 `t*` 序号只留在错误值里给测试看，不进 reason 列。
    pub fn slug(&self) -> &'static str {
        match self {
            FactPassError::Json => "json",
            FactPassError::Version => "version",
            FactPassError::Pass => "pass",
            FactPassError::Family => "family",
            FactPassError::UnknownTurn(_) => "unknown_turn",
            FactPassError::UnknownCandidate(_) => "unknown_candidate",
            FactPassError::Category => "category",
            FactPassError::Predicate => "predicate",
            FactPassError::Mode => "mode",
            FactPassError::MissingSubject => "missing_subject",
            FactPassError::MissingValue => "missing_value",
            FactPassError::EmptyEvidence => "empty_evidence",
            FactPassError::EvidenceNotTarget(_) => "evidence_not_target",
            FactPassError::DuplicateRef(_) => "duplicate_ref",
            FactPassError::QuoteNotVerbatim(_) => "quote_not_verbatim",
            FactPassError::UnknownClause(_) => "unknown_clause",
            FactPassError::ClauseTurnMismatch(_) => "clause_turn_mismatch",
            FactPassError::QuoteTooLong => "quote_too_long",
            FactPassError::Ellipsis => "ellipsis",
            FactPassError::NumberNotGrounded(_) => "number_not_grounded",
            FactPassError::ValueNotInQuote(_) => "value_not_in_quote",
            FactPassError::Uncovered(_) => "uncovered",
            FactPassError::UnknownReviewRef(_) => "review_unknown_ref",
            FactPassError::DuplicateReviewRef(_) => "review_duplicate_ref",
            FactPassError::MissingReviewRef(_) => "review_missing_ref",
            FactPassError::BadReviewVerdict(_) => "review_verdict",
            FactPassError::BadReviewCode(_) => "review_code",
            FactPassError::BadValueAxis(_) => "review_value_axis",
        }
    }

    /// 审核那一支失败落库用的 reason。审核自有的六格已带前缀，原样返回；`json`/`version`/
    /// `pass`/`family` 这几格两次调用共用，这里补 `review_`，否则运维面读不出"模型不会写
    /// 抽取 JSON"和"模型不会写判决 JSON"的区别。
    ///
    /// 返回 `String` 是因为前缀拼接要分配；结果只交给 `rc_ledger::pass_state::mark_terminal`
    /// 写 `extract_pass_state.reason`，不进 stdout、不进错误载荷。
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
struct RawFactOutput {
    schema_version: u32,
    pass: String,
    family: String,
    facts: Vec<RawFact>,
    empty_turns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFact {
    #[serde(rename = "ref")]
    reference: String,
    turn: String,
    /// §10.47：有 `clause` 时可以省。程序按该轮说话人补（`speaker=` 那一格）。
    #[serde(default)]
    subject: String,
    #[serde(default)]
    object: Option<String>,
    #[serde(default)]
    value: Option<String>,
    category: String,
    predicate: String,
    mode: String,
    /// S2e-4：时间短语**可空**。`null` 与"键整个省略"都是"这一条没有时间短语"，
    /// 不是形状错；draft 里统一落成空串（写库层对空串不写 `time_expr`）。
    #[serde(default)]
    time_text: Option<String>,
    /// §10.47：程序切好的子句 id（`c1..cN`）。给了它，`quote` 就由程序从该子句抄，
    /// 弱模型不再需要逐字重打引文；两条都给时以 `clause` 为准。
    #[serde(default)]
    clause: Option<String>,
    #[serde(default)]
    quote: String,
    evidence: Vec<String>,
    /// 弱模型兜底（§10.47）：`text` 可以整个省略。它只是一句给人看的复述，
    /// 不含任何判同/归属信息，由程序按 `主语：value` 拼；模型写了就以模型为准。
    #[serde(default)]
    text: String,
}

/// S2e-2：整段一个 fenced 块时的剥壳（```` ```json\n{...}\n``` ````）。
///
/// **只剥整段**：剥的条件是 `trim` 之后以围栏开头、以围栏结尾——围栏外有说明文字、两个围栏块、
/// 标记拼错的，一律原样返回，交给 `serde_json` 在原来的 `Json` 格里拒掉。不做"从散文里抠第一个
/// `{`"：那会把"模型没按契约说话"读成"模型说了"。
///
/// 剥完仍是**严格 JSON**：`deny_unknown_fields` 与所有闭集校验一个字都不动，围栏只是运输层。
///
/// 一份实现、两个 pass 共用（`pass_event_contract` 从这里 import）：围栏怎么剥若两边各写一份，
/// 同一个模型输出会在 fact 收、在 event 拒，而"为什么"只藏在两份差的实现里。fact/event 各自
/// 保留的是**拒收面**（错误变体与校验顺序），不是这把剥壳的剪刀。
pub(crate) fn json_payload(raw: &str) -> &str {
    let t = raw.trim();
    if let Some(rest) = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")) {
        let rest = rest.strip_prefix('\n').unwrap_or(rest);
        if let Some(body) = rest.strip_suffix("```") {
            return body.trim();
        }
    }
    t
}

/// 短 id 映射表：`t1..tN` = `batch.rows` 顺序、`a1..aN` = `candidates` 顺序。
///
/// 严格与宽松两个 parser 共用这一份映射：短 id 的**含义**只有一个实现，谁都不许另写一份
/// 偏移（把 `a1` 读成 candidates[1] 这种错，两份实现里只会错一份，而排障看不出是哪一份）。
///
/// 键用 `String` 而不是 `&str`：表直接拥有键，签名不必为借用的键再写一遍生命周期；
/// 查表仍按 `&str` 走（`String: Borrow<str>`）。
struct FactRefs<'a> {
    /// 目标轮短 id → (批内下标, 行)。子句归属要按下标比，所以下标与行一起存。
    targets: HashMap<String, (usize, &'a TurnRow)>,
    candidates: HashMap<String, &'a FactCandidate>,
    /// 候选的原始顺序（`speaker_ref` 按下标还原 `aN`）。
    candidate_order: &'a [FactCandidate],
    /// 子句 id → (目标轮下标, 子句原文)。与渲染同一个 [`crate::pass_prompt::clause_index`]。
    clauses: HashMap<String, (usize, String)>,
}

impl<'a> FactRefs<'a> {
    fn new(batch: &'a Batch, candidates: &'a [FactCandidate]) -> Self {
        FactRefs {
            targets: batch
                .rows
                .iter()
                .enumerate()
                .map(|(index, row)| (format!("t{}", index + 1), (index, row)))
                .collect(),
            candidates: candidates
                .iter()
                .enumerate()
                .map(|(index, candidate)| (format!("a{}", index + 1), candidate))
                .collect(),
            candidate_order: candidates,
            clauses: crate::pass_prompt::clause_index(batch)
                .into_iter()
                .map(|clause| (clause.id, (clause.row, clause.text)))
                .collect(),
        }
    }
}

/// 信封四格：形状（`Json`）→ 版本 → pass → family。
///
/// 这四格在宽松 parser 里也**整批拒**：它们回答的是"这一批到底是不是这次调用的回复"，
/// 坏在这里就没有"其余草稿"可留——多键的对象可能根本是另一次作业的输出。
fn fact_envelope(raw: &str, family: FactFamily) -> Result<RawFactOutput, FactPassError> {
    let doc: RawFactOutput =
        serde_json::from_str(json_payload(raw)).map_err(|_| FactPassError::Json)?;
    if doc.schema_version != FACT_SCHEMA_VERSION {
        return Err(FactPassError::Version);
    }
    if doc.pass != PASS_FACT {
        return Err(FactPassError::Pass);
    }
    if doc.family != family.slug() {
        return Err(FactPassError::Family);
    }
    Ok(doc)
}

/// **单条** fact 的全部内容校验（S2e-8a 起是唯一实现）。
///
/// 校验顺序固定：turn → subject → object/value → 闭集（category/predicate/mode）→ quote →
/// evidence → text/数字 → value⊂quote。`ref` 的非空与批内唯一由调用方管：严格 parser 把它变成整批错，
/// 宽松 parser 把它变成"丢这一条"，两种处置不该塞进这条唯一实现里。
fn validate_fact(
    fact: &RawFact,
    refs: &FactRefs<'_>,
    family: FactFamily,
) -> Result<FactDraft, FactPassError> {
    let (turn_index, turn_row) = match refs.targets.get(fact.turn.as_str()) {
        Some((index, row)) => (*index, *row),
        None => return Err(FactPassError::UnknownTurn(fact.turn.clone())),
    };
    let turn_uid = turn_row.turn_uid.clone();
    // §10.47：弱模型只回 clause id；主语缺省 = 该轮的说话人（程序解析，`speaker=` 同源）。
    let subject_ref = if fact.subject.trim().is_empty() {
        crate::pass_prompt::speaker_ref(turn_row.role.as_str(), refs.candidate_order)
            .ok_or(FactPassError::MissingSubject)?
    } else {
        fact.subject.clone()
    };
    let subject = match refs.candidates.get(subject_ref.as_str()) {
        Some(candidate) => *candidate,
        None => return Err(FactPassError::UnknownCandidate(subject_ref)),
    };
    let object = match fact.object.as_deref() {
        Some(reference) if !reference.trim().is_empty() => match refs.candidates.get(reference) {
            Some(candidate) => Some(*candidate),
            None => return Err(FactPassError::UnknownCandidate(reference.to_string())),
        },
        _ => None,
    };
    let mut value = fact.value.clone();
    let has_value = value.as_deref().is_some_and(|v| !v.trim().is_empty());
    if object.is_none() && !has_value {
        // §10.47：只给 clause 也算给了内容——value 缺省就是子句原文（quote 同源，不会编）。
        match fact.clause.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
            Some(id) => match refs.clauses.get(id) {
                Some((_, text)) if !text.trim().is_empty() => value = Some(text.clone()),
                _ => return Err(FactPassError::MissingValue),
            },
            None => return Err(FactPassError::MissingValue),
        }
    }
    if fact.category != family.category() {
        return Err(FactPassError::Category);
    }
    if fact.predicate != family.predicate() {
        return Err(FactPassError::Predicate);
    }
    if fact.mode != MODE_STATED {
        return Err(FactPassError::Mode);
    }
    // S2e-4：`time_text` 不再硬拒。`null`/缺键/空白都是"这一条没有时间短语"，
    // 统一成空串；非空短语原样保留（不 trim，写库层的身份与原文都按原样走）。
    let time_text = fact
        .time_text
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_default()
        .to_string();

    // quote：给了 `clause` 就由程序从该子句抄（必须属于本轮）；否则按模型给的逐字核。
    let quote = match fact.clause.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
        Some(id) => match refs.clauses.get(id) {
            Some((row, text)) if *row == turn_index => text.clone(),
            Some(_) => return Err(FactPassError::ClauseTurnMismatch(id.to_string())),
            None => return Err(FactPassError::UnknownClause(id.to_string())),
        },
        None => fact.quote.clone(),
    };
    if quote.trim().is_empty() {
        return Err(FactPassError::QuoteNotVerbatim(fact.reference.clone()));
    }
    // 省略号只在源文本身没有时拒：源文自带 `…` 时照抄是合法逐字引文（§10.70）。
    let verbatim = evidence_view(&turn_row.body).contains(&evidence_view(&quote));
    if !verbatim && (quote.contains('…') || quote.contains("...")) {
        return Err(FactPassError::Ellipsis);
    }
    if quote.chars().count() > MAX_QUOTE_CHARS {
        return Err(FactPassError::QuoteTooLong);
    }
    if !verbatim {
        return Err(FactPassError::QuoteNotVerbatim(fact.reference.clone()));
    }

    // evidence：非空、只含目标 t*、必须包含本条 turn。
    if fact.evidence.is_empty() {
        return Err(FactPassError::EmptyEvidence);
    }
    let mut evidence_turn_uids = Vec::with_capacity(fact.evidence.len());
    for reference in &fact.evidence {
        match refs.targets.get(reference.as_str()) {
            Some((_, row)) => evidence_turn_uids.push(row.turn_uid.clone()),
            None => return Err(FactPassError::EvidenceNotTarget(reference.clone())),
        }
    }
    if !fact.evidence.iter().any(|reference| reference == &fact.turn) {
        // "evidence 不含 turn" 与 "evidence 指向批外轮" 是同一个引用错误：
        // 本条证据集没有覆盖它自己声明的那一轮。
        return Err(FactPassError::EvidenceNotTarget(fact.turn.clone()));
    }

    // text 可省（§10.47）：模型没写就由程序拼一句 `主语：value`，所有字都来自 quote，
    // 因而不可能引入 quote 之外的数字。模型写了的，仍逐个数字串核对落在 quote 里。
    let text = if fact.text.trim().is_empty() {
        let detail = fact
            .value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or_else(|| object.map(|candidate| candidate.canonical.as_str()))
            .unwrap_or_else(|| quote.trim());
        format!("{}：{detail}", subject.canonical.trim())
    } else {
        // 批内引用 `aN` 先还原成正式名（程序只解指针），再做数字接地（§10.70 的 9/11）。
        resolve_text_refs(&fact.text, refs)
    };
    for number in digit_runs(&text) {
        if !quote.contains(&number) {
            return Err(FactPassError::NumberNotGrounded(number));
        }
    }

    // P0-1：模型**显式**给的 value 必须逐字落在 quote 里。`VERBATIM` 标准写了"quote 覆盖
    // value 的每一个字符"，但程序此前只核对 text 里的数字 ⇒ 值可以被改写，而判同键与
    // 溯源证据都建在这个值上。`clause` 那一支的 value 由程序从子句抄（与 quote 同源），
    // 天然满足；空/全空白 value 是上面 `MissingValue` 那一格的事，不归这条。
    // 位置固定在全部既有校验之后：新闸不抢码序，老用例锁着的错误优先级一个字不变。
    if let Some(explicit) = value.as_deref() {
        let explicit = explicit.trim();
        if !explicit.is_empty() && !quote.contains(explicit) {
            return Err(FactPassError::ValueNotInQuote(fact.reference.clone()));
        }
    }

    Ok(FactDraft {
        reference: fact.reference.clone(),
        turn_uid,
        subject_anchor_id: subject.anchor_id.clone(),
        object_anchor_id: object.map(|candidate| candidate.anchor_id.clone()),
        subject_canonical: subject.canonical.clone(),
        object_canonical: object.map(|candidate| candidate.canonical.clone()),
        predicate: family.predicate().to_string(),
        value,
        category: family.category().to_string(),
        mode: MODE_STATED.to_string(),
        time_text,
        quote,
        evidence_turn_uids,
        text,
    })
}

/// `empty_turns`：只许目标 `t*`、不得重复。
///
/// 它是**批次级声明**而不是一条草稿：重复或指向批外轮，宽松 parser 也一样整批拒
/// （`empty_turns` 说"这些轮没有内容"，批外引用让这句宣告失去指涉对象）。
fn validate_empty_turns(
    empty_turns: &[String],
    refs: &FactRefs<'_>,
) -> Result<Vec<String>, FactPassError> {
    let mut kept: Vec<String> = Vec::with_capacity(empty_turns.len());
    let mut seen_empty: HashSet<&str> = HashSet::new();
    for reference in empty_turns {
        if !seen_empty.insert(reference.as_str()) {
            return Err(FactPassError::DuplicateRef(reference.clone()));
        }
        if !refs.targets.contains_key(reference.as_str()) {
            return Err(FactPassError::UnknownTurn(reference.clone()));
        }
        kept.push(reference.clone());
    }
    Ok(kept)
}

/// 覆盖缺口：`t1..tN` 里既不在 `covered`、也不在 `empty_turns` 里的短 id（有序来自序号）。
fn uncovered_turns(rows: usize, covered: &HashSet<String>) -> Vec<String> {
    (1..=rows)
        .map(|index| format!("t{index}"))
        .filter(|reference| !covered.contains(reference))
        .collect()
}

/// 解析一次 fact pass 的输出。`raw` 必须是**一个** JSON 对象，五键齐全、无多余键。
///
/// 校验顺序是固定的，排障读第一个错误即可：形状（`Json`）→ 版本 → pass → family →
/// 逐条（ref → turn → subject → object/value → 闭集 → quote → evidence → text/数字）→
/// `empty_turns` → 覆盖。任何一步都返回错误，不返回"部分成功"。
///
/// S2e-2：`raw` 是**整段**一个 fenced 块时先剥围栏（[`json_payload`]），再走上面同一条路。
///
/// S2e-8a：单条校验搬进 [`validate_fact`]，与宽松 parser 共用**同一份**实现；本函数的
/// 拒收面（任一条坏 ⇒ 整批 `Err`）一个字不变，[`parse_fact_output_lenient`] 才是"丢该条"。
pub fn parse_fact_output(
    raw: &str,
    batch: &Batch,
    candidates: &[FactCandidate],
    family: FactFamily,
) -> Result<Vec<FactDraft>, FactPassError> {
    let doc = fact_envelope(raw, family)?;
    let refs = FactRefs::new(batch, candidates);

    let mut drafts: Vec<FactDraft> = Vec::with_capacity(doc.facts.len());
    let mut seen_refs: HashSet<&str> = HashSet::new();

    for fact in &doc.facts {
        if fact.reference.trim().is_empty() {
            return Err(FactPassError::Json);
        }
        if !seen_refs.insert(fact.reference.as_str()) {
            return Err(FactPassError::DuplicateRef(fact.reference.clone()));
        }
        drafts.push(validate_fact(fact, &refs, family)?);
    }

    let empty_turns = validate_empty_turns(&doc.empty_turns, &refs)?;

    // 覆盖：每个目标短 id 必须在 facts 的 turn/evidence 或 empty_turns 里。
    // 有序来自 `batch.rows` 顺序，去重来自集合。
    let mut covered: HashSet<String> = empty_turns.into_iter().collect();
    for fact in &doc.facts {
        covered.insert(fact.turn.clone());
        covered.extend(fact.evidence.iter().cloned());
    }
    let missing = uncovered_turns(batch.rows.len(), &covered);
    if !missing.is_empty() {
        return Err(FactPassError::Uncovered(missing));
    }

    Ok(drafts)
}

/// 宽松解析的产物（S2e-8a）。
///
/// 三种结果各占一格，谁都不吞谁：
/// - `drafts`：过了全部单条校验的草稿，**保持原 `facts` 顺序**（被丢的条目原位消失）；
/// - `dropped`：每条被丢草稿的原因，顺序同原 `facts`——第 `i` 个元素就是第 `i` 条被丢的，
///   不是"第 i 条 fact"（合法条目不占位）；
/// - `uncovered`：既没有合法草稿的 `turn`/`evidence` 覆盖、也不在 `empty_turns` 里的目标短 id，
///   按 `t` 序号有序、天然去重。
#[derive(Debug, Clone, PartialEq)]
pub struct LenientFacts {
    pub drafts: Vec<FactDraft>,
    pub dropped: Vec<FactPassError>,
    /// §10.63 同形：被丢草稿的 `(turn t-ref, slug)`。`dropped` 只够数，这一格回答"坏在哪一格"，
    /// run 侧按批内 t-ref 映射回 `turn_uid` 写进 `pass_state.reason`。
    pub dropped_turns: Vec<(String, &'static str)>,
    pub uncovered: Vec<String>,
}

/// §10.47 **弱模型档**的解析：模型只回 `{"keep":["c3","c8",...]}`（也接受裸数组）。
///
/// 程序把这张"留哪些子句"的清单补成完整信封——`turn` 由子句表给出、`subject` 缺省说话人、
/// `value`/`quote` 取子句原文、`evidence` 就是该轮——再交给同一个宽松 parser：
/// 校验、空批、覆盖缺口只有一份实现。Qwen3-4B 在完整契约上只肯给一条 fact，
/// 换成这张清单后能在同一批上稳定枚举（台账 §10.47）。
pub fn parse_fact_compact(
    raw: &str,
    batch: &Batch,
    candidates: &[FactCandidate],
    family: FactFamily,
) -> Result<LenientFacts, FactPassError> {
    let value: serde_json::Value =
        serde_json::from_str(json_payload(raw)).map_err(|_| FactPassError::Json)?;
    // 三种形状都收：裸数组、`{"keep":[id]}`、`{"marks":[{"c":id,"keep":bool}]}`。
    // 第三种是 Qwen3-4B 实测最稳的（逐子句 yes/no，6/7 稳定；keep 清单只到 5/7）。
    let ids: Vec<String> = match &value {
        serde_json::Value::Array(items) => items
            .iter()
            .map(|item| item.as_str().unwrap_or("").to_string())
            .collect(),
        serde_json::Value::Object(map) => {
            if let Some(marks) = map.get("marks").and_then(|marks| marks.as_array()) {
                marks
                    .iter()
                    .filter(|mark| mark.get("keep").and_then(|keep| keep.as_bool()) == Some(true))
                    .map(|mark| {
                        mark.get("c").and_then(|c| c.as_str()).unwrap_or("").to_string()
                    })
                    .collect()
            } else {
                map.get("keep")
                    .and_then(|keep| keep.as_array())
                    .map(|items| {
                        items.iter().map(|item| item.as_str().unwrap_or("").to_string()).collect()
                    })
                    .ok_or(FactPassError::Json)?
            }
        }
        _ => return Err(FactPassError::Json),
    };
    let index = crate::pass_prompt::clause_index(batch);
    let by_id: HashMap<&str, &crate::pass_prompt::ClauseRef> =
        index.iter().map(|clause| (clause.id.as_str(), clause)).collect();
    let mut facts: Vec<serde_json::Value> = Vec::new();
    let mut covered: HashSet<usize> = HashSet::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for (n, raw_id) in ids.iter().enumerate() {
        let id = raw_id.trim();
        if id.is_empty() || !seen.insert(id) {
            continue;
        }
        // 弱模型在短轮里会编出本批没有的编号（把示例里的 c1/c2 照抄）。子句表是程序给的，
        // 不存在的编号就当"没保留"跳过，不拿它整批拒——严格档仍走 `UnknownClause`。
        let Some(clause) = by_id.get(id) else {
            continue;
        };
        // §10.47 弱模型档只从 human 轮抽取：agent 轮的自述步骤在强档里也全部落 empty
        // （play8 16/16），而 4B 分不出"步骤 vs 规则"，会把这些句子记成事实。
        if batch.rows[clause.row].role != "human" {
            continue;
        }
        let turn = format!("t{}", clause.row + 1);
        let subject = crate::pass_prompt::speaker_ref(batch.rows[clause.row].role.as_str(), candidates)
            .ok_or(FactPassError::MissingSubject)?;
        covered.insert(clause.row);
        facts.push(serde_json::json!({
            "ref": format!("f{}", n + 1),
            "turn": turn,
            "subject": subject,
            "category": family.category(),
            "predicate": family.predicate(),
            "mode": MODE_STATED,
            "clause": id,
            "evidence": [turn],
        }));
    }
    let empty_turns: Vec<String> = (0..batch.rows.len())
        .filter(|row| !covered.contains(row))
        .map(|row| format!("t{}", row + 1))
        .collect();
    let doc = serde_json::json!({
        "schema_version": 0,
        "pass": "fact",
        "family": family.slug(),
        "facts": facts,
        "empty_turns": empty_turns,
    });
    parse_fact_output_lenient(&doc.to_string(), batch, candidates, family)
}

/// 宽松解析一次 fact pass 的输出（S2e-8a；设计 `2026-10-26-s2-prompt-v0` §2.2/§3）。
///
/// 与 [`parse_fact_output`] 的**唯一**区别：单条草稿坏在哪一格，就只丢这一条（原因进
/// `dropped`），其余照常返回；覆盖缺口也不再把整批折成错误，而是如实列进 `uncovered`。
/// 真实冒烟正是栽在这条分界上：3 条草稿里 1 条 quote 挪了一个反引号，严格 parser 把整批
/// 判死，而设计要的是"丢该条 + 记住原因"。
///
/// **整批仍拒**的只有信封与批次级声明：形状/多键、`schema_version`、`pass`、`family`、
/// `empty_turns` 的重复与批外引用（见 [`fact_envelope`]、[`validate_empty_turns`]）。
///
/// `ref` 的批内唯一按"**已保留**的草稿"算：先出现的那条合法草稿占住这个 ref，后出现的
/// 同 ref 条目按 `DuplicateRef` 丢；先出现的那条若被丢掉，它不占位，后来的同 ref 仍可保留。
/// 这样返回的 `drafts` 里 ref 仍然唯一（下游按 ref 贴判决），又不会为了一个坏条目连坐好条目。
///
/// S2e-2 的剥围栏与严格侧同一把剪刀（[`json_payload`]）。
pub fn parse_fact_output_lenient(
    raw: &str,
    batch: &Batch,
    candidates: &[FactCandidate],
    family: FactFamily,
) -> Result<LenientFacts, FactPassError> {
    let doc = fact_envelope(raw, family)?;
    let refs = FactRefs::new(batch, candidates);
    // 批次级声明先于逐条：`empty_turns` 坏掉时整批不出，不让"部分草稿"掩盖这条错误。
    let empty_turns = validate_empty_turns(&doc.empty_turns, &refs)?;

    let mut covered: HashSet<String> = empty_turns.into_iter().collect();
    let mut drafts: Vec<FactDraft> = Vec::with_capacity(doc.facts.len());
    let mut dropped: Vec<FactPassError> = Vec::new();
    let mut dropped_turns: Vec<(String, &'static str)> = Vec::new();
    let mut seen_refs: HashSet<String> = HashSet::new();

    for fact in &doc.facts {
        // 空 `ref` 没有身份（与严格侧同一格 `Json`）：它既不能贴判决也不能去重。
        if fact.reference.trim().is_empty() {
            dropped_turns.push((fact.turn.clone(), FactPassError::Json.slug()));
            dropped.push(FactPassError::Json);
            continue;
        }
        if seen_refs.contains(fact.reference.as_str()) {
            dropped_turns.push((
                fact.turn.clone(),
                FactPassError::DuplicateRef(fact.reference.clone()).slug(),
            ));
            dropped.push(FactPassError::DuplicateRef(fact.reference.clone()));
            continue;
        }
        match validate_fact(fact, &refs, family) {
            Ok(draft) => {
                seen_refs.insert(fact.reference.clone());
                covered.insert(fact.turn.clone());
                covered.extend(fact.evidence.iter().cloned());
                drafts.push(draft);
            }
            Err(error) => {
                dropped_turns.push((fact.turn.clone(), error.slug()));
                dropped.push(error);
            }
        }
    }

    let uncovered = uncovered_turns(batch.rows.len(), &covered);
    Ok(LenientFacts { drafts, dropped, dropped_turns, uncovered })
}

/// 连续 ASCII 数字串。`2026-10-26` → `2026`/`10`/`26`；`10月` → `10`。
/// S3a-2 起 `pub(crate)`：程序闸复用同一份数字切分，不写第二份。
/// 把正文里的批内引用 `aN` 换成候选的正式名：程序只解指针，不碰语义。
fn resolve_text_refs(text: &str, refs: &FactRefs<'_>) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == 'a' || chars[i] == 'A' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            let prev_ok = i == 0 || !(chars[i - 1].is_ascii_alphanumeric() || chars[i - 1] == '_');
            let next_ok = j == chars.len() || !(chars[j].is_ascii_alphanumeric() || chars[j] == '_');
            if j > i + 1 && prev_ok && next_ok {
                let token: String = chars[i..j].iter().collect();
                if let Some(candidate) = refs.candidates.get(&token) {
                    out.push_str(&candidate.canonical);
                    i = j;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

pub(crate) fn digit_runs(text: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            current.push(ch);
        } else if !current.is_empty() {
            runs.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs
}

// ── 审核的 parser（S2b-2a）：第二次调用的输出面 ────────────────────────────────
//
// 审核**只**产判决：一行 `(ref, verdict, code)`，不产 id、不产正文、不新增行。
// 所以这一层的错误面比抽取窄：没有 quote/数字/覆盖，只有"引用对不上"与"闭集不对"。

/// 三分支判决。闭集；`parse` 认不出就拒（不接受大小写变体、不接受同义词）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewVerdict { Pass, Reject, Uncertain }

impl ReviewVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewVerdict::Pass => "pass",
            ReviewVerdict::Reject => "reject",
            ReviewVerdict::Uncertain => "uncertain",
        }
    }
    pub fn parse(raw: &str) -> Option<ReviewVerdict> {
        match raw {
            "pass" => Some(ReviewVerdict::Pass),
            "reject" => Some(ReviewVerdict::Reject),
            "uncertain" => Some(ReviewVerdict::Uncertain),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReviewOutput {
    schema_version: u32,
    pass: String,
    family: String,
    verdicts: Vec<RawReviewVerdict>,
}

/// 三键全必填：`code: null` 才是"没有 code"，缺键是形状错（不是"默认没有"）。
/// `reason`/`confidence` 之类多一个键就被 `deny_unknown_fields` 当场拒掉。
///
/// `code` 用 `serde_json::Value` 而不是 `Option<String>`：serde 对 `Option` 有隐式默认
/// （缺键 ⇒ `None`，连新类型包一层也走 `deserialize_option` 的 missing 分支），那会把
/// "模型把 code 键整个漏掉"读成"pass 没有 code"。`Value` 没有这条后门 ⇒ 缺键归 `Json`；
/// 类型错的 code（数字/对象）在下面显式拒成 `Json`。
///
/// S7.2：`evidence` 必须**可缺**——缺键是"这条没给证据"，按工单 §2.3 逐条降 `uncertain`
/// （`evidence_missing`），不是整批拒。所以这里 `#[serde(default)]` 把缺键读成 null，
/// 与显式 `"evidence": null` 同一格；两种都交给证据闸处置。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReviewVerdict {
    #[serde(rename = "ref")]
    reference: String,
    verdict: String,
    code: serde_json::Value,
    #[serde(default)]
    evidence: serde_json::Value,
    /// G1 刀 B：价值轴那一格。用 `Value` 而不是 `Option<String>`（与 `code` 同一条理由：
    /// `Option` 的隐式默认会把"漏写"读成"写了 null"）。缺键/`null` ⇒ `keep`：
    /// 只有**提出**价值轴的那次调用（proposer）会写这一格；challenger 的 prompt 不写，
    /// 于是它的回复里没有这一格，而它的回答本来也不该覆盖 proposer 的价值轴。
    #[serde(default)]
    value_axis: serde_json::Value,
}

/// 一条待审引用的**证据边界**：片内 ref + 该条草稿的 `quote`（工单 §7.2 证据闸）。
///
/// 审核 parser 此前只看 ref；没有 quote 就无法判断 `evidence` 是否真的落在引文里。
/// 由调用方按 prompt 里 `ITEM f1..fN` 的顺序给出，`quote` 就是渲染时用的那一段草稿引文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewTarget {
    pub reference: String,
    pub quote: String,
}

impl ReviewTarget {
    pub fn new(reference: impl Into<String>, quote: impl Into<String>) -> Self {
        ReviewTarget { reference: reference.into(), quote: quote.into() }
    }
}

/// 证据闸的**程序侧**降级原因（工单 §2.3）。它们不是模型 code 闭集的一部分，
/// 绝不进 [`crate::pass_prompt::REVIEW_CODES`]；落库 reason 仍是 `review_uncertain`，
/// 细节进 artifact / 计数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceDowngrade {
    /// `evidence` 缺失、null 或空串。
    Missing,
    /// `evidence` 不是该条 `quote` 的子串。
    NotInQuote,
}

impl EvidenceDowngrade {
    /// 计数/artifact 用的稳定 slug（与 `FactPassError::slug` 同一条纪律：纯小写 ASCII + 下划线）。
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceDowngrade::Missing => "evidence_missing",
            EvidenceDowngrade::NotInQuote => "evidence_not_in_quote",
        }
    }
}

/// 一次审核解析的读数：逐条判决 + 被证据闸降级的条目。
///
/// `downgrades` 按 `targets` 顺序（与 `verdicts` 同一顺序），只列"原本不是 uncertain、
/// 因证据不合格被降级"或"uncertain 但携带的 evidence 越界"的条目；每格是
/// `(ref, 原因)`。调用方据此报出"降级数"，不必再看模型原文。
#[derive(Debug, Clone, PartialEq)]
pub struct FactReviewReport {
    pub verdicts: Vec<(String, ReviewVerdict, Option<String>)>,
    pub downgrades: Vec<(String, EvidenceDowngrade)>,
    /// G1 刀 B：逐条价值轴答案，顺序与 `verdicts` 一致。`keep|demote` 闭集，
    /// 闭集外的值在 parser 里硬拒（[`FactPassError::BadValueAxis`]）。
    pub value_axis: Vec<(String, ValueAxis)>,
}

/// 一条判决归一后的四格：判决、code、证据原文、证据闸的降级原因。
///
/// 整批审核的两侧与单条重跑都要这一组；起个名字是为了让 [`review_ruling`] 的签名
/// 读得出来，而不是一长串 `Option`。
type Ruling = (ReviewVerdict, Option<String>, Option<String>, Option<EvidenceDowngrade>);

/// 一条判决的 code/evidence 归一与证据闸（§7.2 与 §7.3b **共用同一份**）。
///
/// 三件事，顺序固定（与 `parse_fact_review` 的既有校验顺序逐条相同）：
/// 1. `code` 类型归一（null ⇒ 没有码，字符串 ⇒ 有码，其它 ⇒ 形状错）与"码与判决相配"；
/// 2. `evidence` 类型归一（null ⇒ 没有证据，字符串 ⇒ 有证据，其它 ⇒ 形状错）；
/// 3. 证据闸：`pass`/`reject` 缺证据或越界 ⇒ 降 `uncertain`（code 清空）；`uncertain` 带的
///    证据越界同样降级并记原因。
///
/// 两个 parser 各写一份的话，"差一个字符就算越界"这条规则迟早会在其中一份里漂。
fn review_ruling(
    reference: &str,
    verdict_raw: &str,
    code_raw: &serde_json::Value,
    evidence_raw: &serde_json::Value,
    quote: &str,
) -> Result<Ruling, FactPassError> {
    let verdict = ReviewVerdict::parse(verdict_raw)
        .ok_or_else(|| FactPassError::BadReviewVerdict(verdict_raw.to_string()))?;
    let raw_code = match code_raw {
        serde_json::Value::Null => None,
        serde_json::Value::String(code) => Some(code.as_str()),
        _ => return Err(FactPassError::Json),
    };
    let code = match (verdict, raw_code) {
        (ReviewVerdict::Pass, None) => None,
        // `uncertain` 允许不带码：判不了本身就是一个答案（台账 §10.26 缺陷 4）。
        (ReviewVerdict::Uncertain, None) => None,
        // pass 带 code、reject 缺 code、code 在闭集外，都是同一格错误：判决与 code 不配。
        // 载荷是 ref，不把模型写的词回显出去。
        (_, Some(code)) if REVIEW_CODES.contains(&code) && verdict != ReviewVerdict::Pass => {
            Some(code.to_string())
        }
        _ => return Err(FactPassError::BadReviewCode(reference.to_string())),
    };
    let evidence = match evidence_raw {
        serde_json::Value::Null => None,
        serde_json::Value::String(evidence) => Some(evidence.to_string()),
        _ => return Err(FactPassError::Json),
    };
    // 证据闸：pass/reject 缺证据或越界 ⇒ 降 uncertain；uncertain 越界同样降级并记原因。
    // 空串按缺失算（"非空"是契约的一部分）；降级后 code 清空——那个 code 已经没有
    // 证据支撑，继续留着就是把模型的自报当第二证人。
    let downgrade = match evidence.as_deref() {
        None => match verdict {
            ReviewVerdict::Uncertain => None,
            _ => Some(EvidenceDowngrade::Missing),
        },
        Some(evidence) if evidence.trim().is_empty() => Some(EvidenceDowngrade::Missing),
        Some(evidence) if !quote.contains(evidence) => Some(EvidenceDowngrade::NotInQuote),
        Some(_) => None,
    };
    let (verdict, code) = match downgrade {
        Some(_) => (ReviewVerdict::Uncertain, None),
        None => (verdict, code),
    };
    Ok((verdict, code, evidence, downgrade))
}

/// 解析一次 fact 审核的输出。`targets` 是调用方按 prompt 里 `ITEM f1..fN` 的顺序给出的
/// 待审引用（ref + 该条 quote）。
///
/// 校验顺序固定：形状 → 版本 → pass → family → 逐条（ref 非空 → 在 targets 内 → 不重复 →
/// verdict 闭集 → code 与 verdict 相配 → 证据闸）→ 缺失。返回顺序**跟 targets 走**，
/// 不跟模型回复顺序走：调用方按位置把判决贴回草稿，回复里的顺序在这里就被消掉了。
///
/// 证据闸（工单 §2.3）是**逐条降级**而不是整批拒：`pass`/`reject` 缺证据或证据不在 quote 内
/// ⇒ 该条降 `uncertain`（code 清空，原因进 `downgrades`）；`uncertain` 带的证据越界同样降级
/// 并记原因。其余条目照常裁决，降级不把整片判成解析失败。
///
/// S2e-2：与抽取同一把剥壳剪刀（[`json_payload`]）——审核也常把整段判决包进围栏。
pub fn parse_fact_review(
    raw: &str,
    targets: &[ReviewTarget],
    family: FactFamily,
) -> Result<FactReviewReport, FactPassError> {
    let doc: RawReviewOutput =
        serde_json::from_str(json_payload(raw)).map_err(|_| FactPassError::Json)?;
    if doc.schema_version != FACT_SCHEMA_VERSION {
        return Err(FactPassError::Version);
    }
    if doc.pass != PASS_FACT_REVIEW {
        return Err(FactPassError::Pass);
    }
    if doc.family != family.slug() {
        return Err(FactPassError::Family);
    }

    // quote 由 ref 定位；同一个 ref 在 targets 里重复是调用方的错，但 parser 不新增错误格：
    // 先出现的那条占位（与 `seen` 的批内唯一同一取向）。
    let mut quotes: HashMap<&str, &str> = HashMap::with_capacity(targets.len());
    for target in targets {
        quotes.entry(target.reference.as_str()).or_insert(target.quote.as_str());
    }

    let mut seen: HashMap<&str, (ReviewVerdict, Option<String>, Option<EvidenceDowngrade>)> =
        HashMap::with_capacity(doc.verdicts.len());
    // G1 刀 B：价值轴按 ref 攒一份；闭集外的值整批硬拒（与 verdict/code 同一条纪律）。
    let mut axes: HashMap<&str, ValueAxis> = HashMap::with_capacity(doc.verdicts.len());
    let mut downgrades: Vec<(String, EvidenceDowngrade)> = Vec::new();
    for item in &doc.verdicts {
        if item.reference.trim().is_empty() {
            return Err(FactPassError::Json);
        }
        if !targets.iter().any(|target| target.reference == item.reference) {
            return Err(FactPassError::UnknownReviewRef(item.reference.clone()));
        }
        if seen.contains_key(item.reference.as_str()) {
            return Err(FactPassError::DuplicateReviewRef(item.reference.clone()));
        }
        // 价值轴：缺键/`null` ⇒ keep（challenger 不写这一格）；字符串必须在闭集里；
        // 其它类型（数字/布尔/对象/数组）是形状错。
        let axis = match &item.value_axis {
            serde_json::Value::Null => ValueAxis::Keep,
            serde_json::Value::String(raw) => ValueAxis::parse(raw)
                .ok_or_else(|| FactPassError::BadValueAxis(item.reference.clone()))?,
            _ => return Err(FactPassError::Json),
        };
        let quote = quotes
            .get(item.reference.as_str())
            .copied()
            .ok_or_else(|| FactPassError::UnknownReviewRef(item.reference.clone()))?;
        // code/evidence 的归一与证据闸只有一份实现（[`review_ruling`]，§7.3b 的重跑共用它）。
        let (verdict, code, _, downgrade) =
            review_ruling(&item.reference, &item.verdict, &item.code, &item.evidence, quote)?;
        if let Some(reason) = downgrade {
            downgrades.push((item.reference.clone(), reason));
        }
        seen.insert(item.reference.as_str(), (verdict, code, downgrade));
        axes.insert(item.reference.as_str(), axis);
    }

    let mut out = Vec::with_capacity(targets.len());
    let mut out_axes = Vec::with_capacity(targets.len());
    let mut missing: Vec<String> = Vec::new();
    for target in targets {
        match seen.get(target.reference.as_str()) {
            Some((verdict, code, _)) => {
                out.push((target.reference.clone(), *verdict, code.clone()));
                // `seen` 与 `axes` 同一轮插入，键集相同；查不到只可能是调用方改了代码。
                out_axes.push((
                    target.reference.clone(),
                    axes.get(target.reference.as_str()).copied().unwrap_or(ValueAxis::Keep),
                ));
            }
            None => missing.push(target.reference.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(FactPassError::MissingReviewRef(missing));
    }
    Ok(FactReviewReport { verdicts: out, downgrades, value_axis: out_axes })
}

// ── 单条重跑（§7.3b）：一次调用只判一条 ──────────────────────────────────────
//
// 重跑的契约比整批审核窄一格：没有 `schema_version`/`pass`/`family`/`verdicts[]`——这一次
// 调用没有"批"可标识，只有一条草稿与一条判决。`verdict`/`code` 必填（`code` 可为 null），
// `evidence`/`note` 可缺（见 [`RawRetryRuling`]：缺证据走证据闸降级，缺 note 只丢那句话）。
//
// 证据规则与 §7.2 **逐条相同**（[`review_ruling`]）：`pass`/`reject` 必须带一段落在该条
// `quote` 内的连续原文；缺失/空串 ⇒ `evidence_missing`，越界 ⇒ `evidence_not_in_quote`；
// 两种都逐条降 `uncertain`（code 清空），不是整条拒。
//
// `note` 是"缺哪类信息"的一句话：**程序只存不判分**。它进 [`RetryRuling::note`]，由调用方
// 原样落 artifact（`review_verdict` 没有这一列，§7.3a 的表结构本片不动）。

/// 单条重跑的一次判决。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryRuling {
    /// 判决：证据闸降级后的结果（`pass`/`reject` 证据不合格 ⇒ `uncertain`）。
    pub verdict: ReviewVerdict,
    /// 模型的 code（降级后清空；`pass` 恒空）。
    pub code: Option<String>,
    /// 模型给的原文（原样带回，不 trim、不改写；没有证据时为 `None`）。
    pub evidence: Option<String>,
    /// "缺哪类信息"的一句话（原样带回；程序只存不判分）。
    pub note: Option<String>,
    /// 证据闸的程序侧裁定；没有降级时为 `None`。
    pub downgrade: Option<EvidenceDowngrade>,
}

/// 单条重跑的输出面。`code` 必填（`null` 是"没有码"，缺键是形状错）；`evidence`/`note`
/// 可缺——缺证据是"这条没给证据"，按 §7.2 的证据闸降 `uncertain`（`evidence_missing`），
/// 缺 `note` 只是"没说缺什么"，不值得把一条真判决整条扔掉。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRetryRuling {
    verdict: String,
    code: serde_json::Value,
    #[serde(default)]
    evidence: serde_json::Value,
    #[serde(default)]
    note: serde_json::Value,
}

/// 解析一次**单条重跑**的输出。
///
/// `reference` 是该草稿在载荷里的 ref（只用于错误载荷，不回显模型正文）；`quote` 是它的
/// 引文——证据闸只对照这一段，与整批审核同一条判据。
///
/// 校验顺序固定：形状（含围栏剥壳）→ verdict 闭集 → code/evidence 归一与相配 → 证据闸 →
/// note 类型。任何一步不合格都返回错误（整条不发判决）；证据不合格**不是**错误，是降级。
pub fn parse_fact_review_retry(
    raw: &str,
    reference: &str,
    quote: &str,
) -> Result<RetryRuling, FactPassError> {
    let doc: RawRetryRuling =
        serde_json::from_str(json_payload(raw)).map_err(|_| FactPassError::Json)?;
    let (verdict, code, evidence, downgrade) =
        review_ruling(reference, &doc.verdict, &doc.code, &doc.evidence, quote)?;
    let note = match &doc.note {
        serde_json::Value::Null => None,
        serde_json::Value::String(note) => Some(note.clone()),
        _ => return Err(FactPassError::Json),
    };
    Ok(RetryRuling {
        verdict,
        code,
        evidence,
        note,
        downgrade,
    })
}
