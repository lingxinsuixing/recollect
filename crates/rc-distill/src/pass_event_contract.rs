//! S2c-1：event pass 的严格 parser 与独立审核 parser（设计 `2026-10-26-s2-prompt-v0`
//! §2.3、§4 的程序闸）。
//!
//! 这一层的全部工作是"模型给什么形状、程序怎么拒"，不选批、不调模型、不写库。
//! 严格 parser（[`parse_event_output`]）把任何违规都当**硬拒**：返回错误，不静默丢条、
//! 不猜默认值。错误载荷只带批内短引用与数字串，不回显模型正文（守卫 12）。
//!
//! S2e-8a 多一个**宽松** parser（[`parse_event_output_lenient`]）：信封仍然整批拒，
//! 单条坏只丢一条、其余照常返回，覆盖缺口如实列出而不折整批。两者共用同一份单条校验
//! （[`validate_event`]）——宽松不是第二套规则，只是对同一条规则的另一种处置。
//!
//! 映射规则（§2.3）：
//! - `t1..tN` = `batch.rows` 顺序；`t*` 只在这里存在，程序把它换成真实 `turn_uid`；
//! - `a1..aN` = `candidates` 顺序；`actor`/`object`/`participants`/`place` 都是 `a*`，
//!   程序把它换成 `anchor_id`；
//! - `evidence` 只许目标 `t*`，且必须包含本条自己的 `turn`；
//! - 每个目标轮必须落在某条 event 的 `turn`/`evidence` 或 `empty_turns` 里。
//!
//! C6b：prompt 里的 `CTX [c*]` 邻轮**不是**映射面。`targets` 只由 `batch.rows` 建，
//! 邻轮不在 `batch.rows` 里 ⇒ `c*` 进 `turn`/`evidence` 就是 `unknown_turn`/
//! `evidence_not_target`；邻轮的句子也不是合法 `quote`（逐字检查只对照目标轮正文）。
//! 契约一个字不改：CTX 不进门，是因为它从来没有门。
//!
//! 与 fact parser 的关系：规则同形但**不复用实现**——两个 pass 各自可单独重试、归因、
//! 回滚（设计 §1），一处改动不该同时改变另一方的拒收面。共用的只有设计常量
//! （`EVENT_*` 闭集、`ReviewVerdict` 三分支）与 `prepare` 这一把剪刀。
//!
//! 错误码表是派发档给的 19 格，本仓此后扩到 24 格（逐格见枚举）：§7.5 切片一a 加了
//! `time_not_in_quote`（见 [`EventPassError::TimeNotInQuote`]）：`time_text ⊂ quote` 与 fact 的
//! `value ⊂ quote` 是同一条证据原则，19 格里没有能装下它的格子，就近归并会把"时间从邻句借来"读成
//! 引文不逐字，排障方向就错了。切片一b 又加了 `trigger_not_in_quote`/
//! `state_change_without_object`/`announcement_only`，G1 刀 B 加了 `review_value_axis`。
//! 新增格一律在枚举里逐格登记；表里没有的违规仍按语义就近归并，
//! 且在每一处归并处写明理由（见 `parse_event_review` 的注释）：新增一个错误名会让 S2c-2
//! 的 runner 与观测面多一个未知词，而"这行被拒"的结论不变。
//!
//! S2e-1：每格各有**稳定 slug**（[`EventPassError::slug`]），runner 把它写进
//! `extract_pass_state.reason`。审核侧沿用的仍是这 24 格，所以审核失败的 reason 是
//! `review_` 前缀 + 同一格（`review_json`/`review_uncovered`/`review_unknown_turn`）；
//! 前缀由 [`EventPassError::review_reason`] 补一次，不出现 `review_review_*`。

use crate::batch::{Batch, TurnRow};
use crate::pass_fact_contract::{json_payload, ReviewVerdict};
use crate::pass_prompt::{
    FactCandidate, ValueAxis, EVENT_CATEGORIES, EVENT_MODE_STATED, EVENT_RESULTS,
    EVENT_REVIEW_CODES, EVENT_SCHEMA_VERSION, PASS_EVENT, PASS_EVENT_REVIEW,
};
use crate::prepare::evidence_view;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

/// `quote` 的字符上限。§4 的引文闸对 fact/event 是同一个数；常量各自一份，
/// 因为两个 pass 的拒收面必须能独立演进（改 event 的上限不该悄悄改 fact 的）。
pub const MAX_QUOTE_CHARS: usize = 120;

/// 模型可写的一条 event 草稿。短引用已在这里换成真实 uid / anchor_id。
///
/// `actor_canonical`/`object_canonical`/`participant_canonicals`/`place_canonical` 是
/// **只给审核 prompt 渲染用**的：审核不许看 anchor_id，但"引文是否支持这个参与者"必须
/// 看到名字。它们与 `FactDraft::subject_canonical` 同一条纪律，不进写库层。
#[derive(Debug, Clone, PartialEq)]
pub struct EventDraft {
    pub reference: String,
    pub turn_uid: String,
    /// `event|state_change|observation`（`EVENT_CATEGORIES`）。
    pub category: String,
    pub actor_anchor_id: String,
    pub object_anchor_id: Option<String>,
    /// 不值得建锚的字面量（与 `object` 至多一个非空）。
    pub object_text: Option<String>,
    pub participants_anchor_ids: Vec<String>,
    pub place_anchor_id: Option<String>,
    /// 最短动词短语，逐字取自 `quote`（§7.5 切片一b）；category 是它的类别。
    pub trigger: String,
    pub action: String,
    /// `done|failed|blocked|ongoing|unknown`（`EVENT_RESULTS`）。
    pub result: String,
    /// v0 只收 `stated`。
    pub mode: String,
    pub time_text: String,
    pub quote: String,
    pub evidence_turn_uids: Vec<String>,
    pub text: String,
    /// actor 的 canonical 名字（只给审核渲染）。
    pub actor_canonical: String,
    /// object 的 canonical；object 缺席时 `None`。
    pub object_canonical: Option<String>,
    /// participants 的 canonical 名字，顺序与 `participants_anchor_ids` 一一对应。
    pub participant_canonicals: Vec<String>,
    /// place 的 canonical；place 缺席时 `None`。
    pub place_canonical: Option<String>,
}

/// 拒收原因。同一格在严格 parser 里是"整批不出"的硬拒；在宽松 parser（S2e-8a）里，
/// 单条内容错只丢那一条并原样记进 [`LenientEvents::dropped`]——错误码不因处置方式变宽或变窄。
///
/// 派发档列的是 19 格闭集（本仓此后扩到 24 格，逐格见枚举），这里逐格照办。三个"表里没有对应格"的违规就近归并：
/// - `participants` 为空 ⇒ `MissingActor`（空列表不可能包含 actor，是同一处违约）；
/// - 空的 `text` ⇒ `Json`（形状问题，与 fact parser 同一条裁定）；`time_text` 自 S2e-4 起
///   **可空**（`null`/缺键/空白 ⇒ draft 空串），不再进这一格；
/// - 审核侧的 `verdict`/`code` 闭集错 ⇒ `Json`（见 `parse_event_review`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventPassError {
    /// 不是 JSON、不是对象、缺键、多键、字段类型错、空必填串（`ref`/`actor`/`action`/`text`；
    /// `time_text` 自 S2e-4 起可空，不进这一格）。
    Json,
    /// `schema_version` 不是 0。
    Version,
    /// `pass` 不是 `"event"`（审核侧是 `"event_review"`）。
    Pass,
    /// `turn` / `empty_turns` 引用了批外的 `t*`；审核侧多给了待审集合外的 `ref`。
    UnknownTurn(String),
    /// `actor` / `object` / `participants` / `place` 引用了候选表外的 `a*`。
    UnknownCandidate(String),
    /// `category` 不在 `event|state_change|observation` 里。
    Category,
    /// `mode` 不是 `"stated"`。
    Mode,
    /// `actor` 为空；或 `participants` 为空 / 不含 actor。
    MissingActor,
    /// `action` 为空。
    MissingAction,
    /// `object` 与 `object_text` 同时给出。
    ObjectBoth,
    /// `result` 不在 `done|failed|blocked|ongoing|unknown` 里。
    BadResult,
    /// `evidence` 为空。
    EmptyEvidence,
    /// `evidence` 里有非目标引用；或 evidence 不含本条自己的 `turn`。
    EvidenceNotTarget(String),
    /// 批内短引用重复（`events[].ref`、`empty_turns[]` 或审核的 `ref`）。
    DuplicateRef(String),
    /// `quote` 不是本条目标轮 `prepare` 正文的逐字子串（空 quote 也归这里）。
    QuoteNotVerbatim(String),
    /// `quote` 超过 120 字符。
    QuoteTooLong,
    /// `quote` 含省略号（`…` 或 `...`）。
    Ellipsis,
    /// `text` 里的数字串没有出现在 `quote` 里。
    NumberNotGrounded(String),
    /// 非空的 `time_text` 不是 `quote` 的逐字子串（§7.5 切片一a）。载荷是 `ref`
    /// （不回显模型写的时间短语）。空 `time_text` 不归这一格。
    TimeNotInQuote(String),
    /// `trigger` 不是 `quote` 的逐字子串（§7.5 切片一b）。载荷是 `ref`，不回显模型写的短语。
    /// 空的 `trigger` 是形状错（`Json`，与空的 `ref`/`text` 同一条读法）。
    TriggerNotInQuote(String),
    /// `category == "state_change"` 但 `object` 与 `object_text` 都空（§7.5 切片一b）。
    /// 状态变化必须点名"谁的"状态变了；没有客体的 state_change 是 category 用错了格。
    StateChangeWithoutObject,
    /// 有目标轮既没进 event 的 `turn`/`evidence`，也没进 `empty_turns`；按 `t` 序号有序。
    /// 审核侧：有 `ref` 一条判决都没拿到，按 `refs` 顺序列出全部缺失项。
    Uncovered(Vec<String>),
    /// `category=event` 的 quote 只是"接下来要做"的宣布（现在/先/接着/我再…），
    /// 且没有任何已发生/已开始的标记。载荷是 `ref`，不回显模型写的句子。
    /// 新闸接在既有格之后：既有格的 slug 一个都不动。
    AnnouncementOnly(String),
    /// G1 刀 B：审核的 `value_axis` 不在 `keep|demote` 闭集里。载荷是 `ref`，
    /// 不回显模型写的词（与 `UnknownTurn` 一类同一取向）。
    BadValueAxis(String),
}

impl EventPassError {
    /// 进 `extract_pass_state.reason` 的封闭 slug（S2e-1）。24 格里每一格一个词，
    /// 与抽取/审核的分支无关——审核侧共用同一张表（只有 `review_value_axis` 是审核专属格，
    /// 自带的 `review_` 前缀原样返回），前缀由
    /// [`EventPassError::review_reason`] 补。
    ///
    /// 这里**故意不写 `_ =>`**（与 `ContractError::slug` 同一条纪律）：新增变体必须在这张表里
    /// 表态。载荷一律忽略：短引用与 `t*` 序号只留在错误值里给测试看，不进 reason 列。
    pub fn slug(&self) -> &'static str {
        match self {
            EventPassError::Json => "json",
            EventPassError::Version => "version",
            EventPassError::Pass => "pass",
            EventPassError::UnknownTurn(_) => "unknown_turn",
            EventPassError::UnknownCandidate(_) => "unknown_candidate",
            EventPassError::Category => "category",
            EventPassError::Mode => "mode",
            EventPassError::MissingActor => "missing_actor",
            EventPassError::MissingAction => "missing_action",
            EventPassError::ObjectBoth => "object_both",
            EventPassError::BadResult => "bad_result",
            EventPassError::EmptyEvidence => "empty_evidence",
            EventPassError::EvidenceNotTarget(_) => "evidence_not_target",
            EventPassError::DuplicateRef(_) => "duplicate_ref",
            EventPassError::QuoteNotVerbatim(_) => "quote_not_verbatim",
            EventPassError::QuoteTooLong => "quote_too_long",
            EventPassError::Ellipsis => "ellipsis",
            EventPassError::NumberNotGrounded(_) => "number_not_grounded",
            EventPassError::TimeNotInQuote(_) => "time_not_in_quote",
            EventPassError::TriggerNotInQuote(_) => "trigger_not_in_quote",
            EventPassError::StateChangeWithoutObject => "state_change_without_object",
            EventPassError::Uncovered(_) => "uncovered",
            EventPassError::AnnouncementOnly(_) => "announcement_only",
            EventPassError::BadValueAxis(_) => "review_value_axis",
        }
    }

    /// 审核那一支失败落库用的 reason：`review_` + 同一格（`review_json`/`review_uncovered`/
    /// `review_unknown_turn`）。审核与抽取共用 24 格，靠这个前缀在 `extract_pass_state.reason`
    /// 里分出"坏在哪一次开口"。
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
struct RawEventOutput {
    schema_version: u32,
    pass: String,
    events: Vec<RawEvent>,
    empty_turns: Vec<String>,
}

/// 16 个键，逐一对应设计 §2.3 的输出形状。`object`/`object_text`/`place` 可缺省为 null，
/// `time_text` 自 S2e-4 起也可缺省（没有时间短语的轮次写 `null` 或省略键，两种都读成空串），
/// 其余必填（含 `trigger`，§7.5 切片一b 加的；缺键 = 形状错）。`cause`/`relation`/`occurred_at`/`confidence`/`id` 之类
/// 多一个就当场拒（`deny_unknown_fields`）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEvent {
    #[serde(rename = "ref")]
    reference: String,
    turn: String,
    category: String,
    actor: String,
    trigger: String,
    action: String,
    #[serde(default)]
    object: Option<String>,
    #[serde(default)]
    object_text: Option<String>,
    participants: Vec<String>,
    #[serde(default)]
    place: Option<String>,
    result: String,
    mode: String,
    /// S2e-4：时间短语**可空**。`null` 与"键整个省略"都是"这一行没有时间短语"，
    /// 不是形状错；draft 里统一落成空串（写库层对空串不写 `time_expr`）。
    #[serde(default)]
    time_text: Option<String>,
    quote: String,
    evidence: Vec<String>,
    text: String,
}

/// 短 id 映射表：`t1..tN` = `batch.rows` 顺序、`a1..aN` = `candidates` 顺序。
///
/// 严格与宽松两个 parser 共用这一份映射（与 fact 侧 [`crate::pass_fact_contract`] 同一条纪律：
/// 短 id 的含义只有一处实现）。键用 `String`，查表按 `&str` 走。
struct EventRefs<'a> {
    targets: HashMap<String, &'a TurnRow>,
    candidates: HashMap<String, &'a FactCandidate>,
}

impl<'a> EventRefs<'a> {
    fn new(batch: &'a Batch, candidates: &'a [FactCandidate]) -> Self {
        EventRefs {
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
///
/// 这四键信封在宽松 parser 里也**整批拒**：它回答"这一批是不是这次调用的回复"，
/// 坏在这里就没有"其余事件"可留。event 信封没有 `family` 键（§2.3 的四键），
/// 所以这一侧本来就没有 family 整批拒这一格。
fn event_envelope(raw: &str) -> Result<RawEventOutput, EventPassError> {
    let doc: RawEventOutput =
        serde_json::from_str(json_payload(raw)).map_err(|_| EventPassError::Json)?;
    if doc.schema_version != EVENT_SCHEMA_VERSION {
        return Err(EventPassError::Version);
    }
    if doc.pass != PASS_EVENT {
        return Err(EventPassError::Pass);
    }
    Ok(doc)
}

/// **单条** event 的全部内容校验（S2e-8a 起是唯一实现）。
///
/// 校验顺序固定：turn → category → actor → action → trigger → object/object_text → participants →
/// place → result → mode → quote → evidence → text/数字 → time_text⊂quote → trigger⊂quote →
/// state_change 有客体。`ref` 的非空与
/// 批内唯一由调用方管：严格 parser 变成整批错，宽松 parser 变成"丢这一条"。
fn validate_event(
    event: &RawEvent,
    refs: &EventRefs<'_>,
) -> Result<EventDraft, EventPassError> {
    let (turn_uid, turn_row) = match refs.targets.get(event.turn.as_str()) {
        Some(row) => (row.turn_uid.clone(), *row),
        None => return Err(EventPassError::UnknownTurn(event.turn.clone())),
    };
    if !EVENT_CATEGORIES.contains(&event.category.as_str()) {
        return Err(EventPassError::Category);
    }
    if event.actor.trim().is_empty() {
        return Err(EventPassError::MissingActor);
    }
    let actor = match refs.candidates.get(event.actor.as_str()) {
        Some(candidate) => *candidate,
        None => return Err(EventPassError::UnknownCandidate(event.actor.clone())),
    };
    if event.action.trim().is_empty() {
        return Err(EventPassError::MissingAction);
    }
    // `trigger` 必填且非空：空的必填串按形状错（与空 `ref`/`text` 同一条读法）。
    // 去首尾空白只做一次：闸与 draft 读同一个值。
    let trigger = event.trigger.trim();
    if trigger.is_empty() {
        return Err(EventPassError::Json);
    }

    // object / object_text：空串等于没给（与 fact 的 object 同一条读法），
    // 两者都非空是 `ObjectBoth`——一列锚 + 一列字面量会让下游不知道哪个才是宾语。
    let object = match event.object.as_deref() {
        Some(reference) if !reference.trim().is_empty() => match refs.candidates.get(reference) {
            Some(candidate) => Some(*candidate),
            None => return Err(EventPassError::UnknownCandidate(reference.to_string())),
        },
        _ => None,
    };
    let object_text = event
        .object_text
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string);
    if object.is_some() && object_text.is_some() {
        return Err(EventPassError::ObjectBoth);
    }

    // participants：非空、只含候选、必须包含 actor。空列表与缺 actor 归同一格
    // （`MissingActor`）：两种写法都让"谁做的"没有着落。
    if event.participants.is_empty() {
        return Err(EventPassError::MissingActor);
    }
    let mut participants: Vec<&FactCandidate> = Vec::with_capacity(event.participants.len());
    for reference in &event.participants {
        match refs.candidates.get(reference.as_str()) {
            Some(candidate) => participants.push(*candidate),
            None => return Err(EventPassError::UnknownCandidate(reference.clone())),
        }
    }
    if !event.participants.iter().any(|reference| reference == &event.actor) {
        return Err(EventPassError::MissingActor);
    }

    let place = match event.place.as_deref() {
        Some(reference) if !reference.trim().is_empty() => match refs.candidates.get(reference) {
            Some(candidate) => Some(*candidate),
            None => return Err(EventPassError::UnknownCandidate(reference.to_string())),
        },
        _ => None,
    };

    if !EVENT_RESULTS.contains(&event.result.as_str()) {
        return Err(EventPassError::BadResult);
    }
    if event.mode != EVENT_MODE_STATED {
        return Err(EventPassError::Mode);
    }
    // S2e-4：`time_text` 不再硬拒。`null`/缺键/空白都读成"这一行没有时间短语"，
    // 统一成空串；非空短语去首尾空白后保留（§7.5 切片一a：闸与落库读同一个值）。
    let time_text = event
        .time_text
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or_default()
        .to_string();

    // quote：逐字子串、≤120 字。空 quote 先于逐字检查拒掉。
    // 省略号只在**源文本身没有**时拒：源文自带 `199aa91d…d6ce` 时照抄是合法的逐字引文
    // （§10.70 的 e98cf58f；旧规则无条件拒，等于把这一类轮判死）。
    if event.quote.trim().is_empty() {
        return Err(EventPassError::QuoteNotVerbatim(event.reference.clone()));
    }
    let verbatim = evidence_view(&turn_row.body).contains(&evidence_view(&event.quote));
    if !verbatim && (event.quote.contains('…') || event.quote.contains("...")) {
        return Err(EventPassError::Ellipsis);
    }
    if event.quote.chars().count() > MAX_QUOTE_CHARS {
        return Err(EventPassError::QuoteTooLong);
    }
    if !verbatim {
        return Err(EventPassError::QuoteNotVerbatim(event.reference.clone()));
    }
    // 宣布闸（台账 §10.111）：`category=event` 的 quote 如果只是"接下来要做"的宣布，
    // 且没有任何已发生/已开始的标记，就拒掉。只拦 event：observation/state_change 的
    // "现在服务在跑"是读数，不是宣布。判据是启发式（见 [`is_announcement_only`]），
    // 所以它只做**减误收**，不做"这句话一定是事件"的正面证明。
    if event.category == "event" && is_announcement_only(&event.quote) {
        return Err(EventPassError::AnnouncementOnly(event.reference.clone()));
    }

    // evidence：非空、只含目标 t*、必须包含本条 turn。
    if event.evidence.is_empty() {
        return Err(EventPassError::EmptyEvidence);
    }
    let mut evidence_turn_uids = Vec::with_capacity(event.evidence.len());
    for reference in &event.evidence {
        match refs.targets.get(reference.as_str()) {
            Some(row) => evidence_turn_uids.push(row.turn_uid.clone()),
            None => return Err(EventPassError::EvidenceNotTarget(reference.clone())),
        }
    }
    if !event.evidence.iter().any(|reference| reference == &event.turn) {
        // "evidence 不含 turn" 与 "evidence 指向批外轮" 是同一个引用错误。
        return Err(EventPassError::EvidenceNotTarget(event.turn.clone()));
    }

    // text 非空；批内引用 `aN` 先按候选表还原成正式名（程序只管指针，语义仍由模型给），
    // 再做数字接地——否则 `"a1 抽查确认…"` 里的 `1` 会被读成凭空数字（§10.70 的 9/11）。
    if event.text.trim().is_empty() {
        return Err(EventPassError::Json);
    }
    let text = resolve_text_refs(&event.text, refs);
    for number in digit_runs(&text) {
        if !event.quote.contains(&number) {
            return Err(EventPassError::NumberNotGrounded(number));
        }
    }

    // §7.5 切片一a：非空的时间短语必须是 quote 的逐字子串。判据措辞与正反例见
    // `.local/research-2026-10-29-event/report.md` §3.4；与 fact 的 `value ⊂ quote`
    // （`b97bbc3`）同一条原则——时间是从邻句/邻轮补出来的，就是没有出处。
    // 位置固定在全部既有校验之后：新闸不抢码序，老用例锁着的错误优先级一个字不变。
    if !time_text.is_empty() && !event.quote.contains(&time_text) {
        return Err(EventPassError::TimeNotInQuote(event.reference.clone()));
    }

    // §7.5 切片一b：`trigger` 必须是 `quote` 的逐字子串。category 是它的类别，
    // 所以这个短语一旦可以被改写，"category 从哪来"就没有可核的出处了。
    if !event.quote.contains(trigger) {
        return Err(EventPassError::TriggerNotInQuote(event.reference.clone()));
    }

    // §7.5 切片一b：state_change 必须点名客体。引文断言"某物的某属性前态≠后态"时，
    // 客体不可省略——没有客体就不是状态变化，是 category 用错了格。
    if event.category == "state_change" && object.is_none() && object_text.is_none() {
        return Err(EventPassError::StateChangeWithoutObject);
    }

    Ok(EventDraft {
        reference: event.reference.clone(),
        turn_uid,
        category: event.category.clone(),
        actor_anchor_id: actor.anchor_id.clone(),
        object_anchor_id: object.map(|candidate| candidate.anchor_id.clone()),
        object_text,
        participants_anchor_ids: participants
            .iter()
            .map(|candidate| candidate.anchor_id.clone())
            .collect(),
        place_anchor_id: place.map(|candidate| candidate.anchor_id.clone()),
        trigger: trigger.to_string(),
        action: event.action.clone(),
        result: event.result.clone(),
        mode: EVENT_MODE_STATED.to_string(),
        time_text,
        quote: event.quote.clone(),
        evidence_turn_uids,
        text,
        actor_canonical: actor.canonical.clone(),
        object_canonical: object.map(|candidate| candidate.canonical.clone()),
        participant_canonicals: participants
            .iter()
            .map(|candidate| candidate.canonical.clone())
            .collect(),
        place_canonical: place.map(|candidate| candidate.canonical.clone()),
    })
}

/// `empty_turns`：只许目标 `t*`、不得重复。批次级声明——宽松 parser 也一样整批拒。
fn validate_empty_turns(
    empty_turns: &[String],
    refs: &EventRefs<'_>,
) -> Result<Vec<String>, EventPassError> {
    let mut kept: Vec<String> = Vec::with_capacity(empty_turns.len());
    let mut seen_empty: HashSet<&str> = HashSet::new();
    for reference in empty_turns {
        if !seen_empty.insert(reference.as_str()) {
            return Err(EventPassError::DuplicateRef(reference.clone()));
        }
        if !refs.targets.contains_key(reference.as_str()) {
            return Err(EventPassError::UnknownTurn(reference.clone()));
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

/// 解析一次 event pass 的输出。`raw` 必须是**一个** JSON 对象，四键齐全、无多余键。
///
/// 校验顺序是固定的，排障读第一个错误即可：形状（`Json`）→ 版本 → pass → 逐条
/// （ref → turn → category → actor → action → trigger → object/object_text → participants → place
/// → result → mode → quote → evidence → text/数字 → time_text⊂quote → trigger⊂quote →
/// state_change 有客体）→ `empty_turns` → 覆盖。
/// `time_text` 可空（`null`/缺键/空白都读成空串），非空时要逐字落在 `quote` 里（§7.5 切片一a）。
/// 任何一步都返回错误，不返回"部分成功"。
///
/// S2e-2：`raw` 是**整段**一个 fenced 块时先剥围栏（[`json_payload`]，与 fact 共用那一份实现），
/// 再走上面同一条路。围栏内的严格性一个字都不动。
///
/// S2e-8a：单条校验搬进 [`validate_event`]，与宽松 parser 共用**同一份**实现；本函数的
/// 拒收面（任一条坏 ⇒ 整批 `Err`）一个字不变，[`parse_event_output_lenient`] 才是"丢该条"。
pub fn parse_event_output(
    raw: &str,
    batch: &Batch,
    candidates: &[FactCandidate],
) -> Result<Vec<EventDraft>, EventPassError> {
    let doc = event_envelope(raw)?;
    let refs = EventRefs::new(batch, candidates);

    let mut drafts: Vec<EventDraft> = Vec::with_capacity(doc.events.len());
    let mut seen_refs: HashSet<&str> = HashSet::new();

    for event in &doc.events {
        if event.reference.trim().is_empty() {
            return Err(EventPassError::Json);
        }
        if !seen_refs.insert(event.reference.as_str()) {
            return Err(EventPassError::DuplicateRef(event.reference.clone()));
        }
        drafts.push(validate_event(event, &refs)?);
    }

    let empty_turns = validate_empty_turns(&doc.empty_turns, &refs)?;

    // 覆盖：每个目标短 id 必须在 events 的 turn/evidence 或 empty_turns 里。
    // 有序来自 `batch.rows` 顺序，去重来自集合。
    let mut covered: HashSet<String> = empty_turns.into_iter().collect();
    for event in &doc.events {
        covered.insert(event.turn.clone());
        covered.extend(event.evidence.iter().cloned());
    }
    let missing = uncovered_turns(batch.rows.len(), &covered);
    if !missing.is_empty() {
        return Err(EventPassError::Uncovered(missing));
    }

    Ok(drafts)
}

/// 宽松解析的产物（S2e-8a）。字段含义与 [`crate::pass_fact_contract::LenientFacts`] 逐格对应：
/// `drafts` 保持原 `events` 顺序，`dropped` 顺序同原 `events`，`uncovered` 按 `t` 序号有序。
#[derive(Debug, Clone, PartialEq)]
pub struct LenientEvents {
    pub drafts: Vec<EventDraft>,
    pub dropped: Vec<EventPassError>,
    /// §10.62：被丢草稿的 `(turn t-ref, slug)`。`dropped` 只够数，这一格才回答"坏在哪一格"；
    /// run 侧按批内 t-ref 映射回 `turn_uid` 写进 `pass_state.reason`（不再只写 `dropped`）。
    pub dropped_turns: Vec<(String, &'static str)>,
    pub uncovered: Vec<String>,
}

/// 宽松解析一次 event pass 的输出（S2e-8a；设计 `2026-10-26-s2-prompt-v0` §2.3/§3）。
///
/// 与 [`parse_event_output`] 的**唯一**区别：单条 event 坏在哪一格就只丢这一条（原因进
/// `dropped`），其余照常返回；覆盖缺口如实列进 `uncovered`，不再折成整批错误。
///
/// **整批仍拒**的只有信封与批次级声明：形状/多键、`schema_version`、`pass`、`empty_turns`
/// 的重复与批外引用。event 的信封**没有** `family` 键（§2.3 的四键信封），所以这一侧
/// 不存在 family 整批拒——这是两个 pass 契约的真实差别，不是宽松 parser 漏掉的一格。
///
/// `ref` 的批内唯一按"**已保留**的草稿"算，与 fact 宽松侧同一条纪律：先保留的条目占住 ref，
/// 被丢掉的不占位。
///
/// S2e-2 的剥围栏与严格侧同一把剪刀（[`json_payload`]）。
pub fn parse_event_output_lenient(
    raw: &str,
    batch: &Batch,
    candidates: &[FactCandidate],
) -> Result<LenientEvents, EventPassError> {
    let doc = event_envelope(raw)?;
    let refs = EventRefs::new(batch, candidates);
    // 批次级声明先于逐条：`empty_turns` 坏掉时整批不出。
    let empty_turns = validate_empty_turns(&doc.empty_turns, &refs)?;

    let mut covered: HashSet<String> = empty_turns.into_iter().collect();
    let mut drafts: Vec<EventDraft> = Vec::with_capacity(doc.events.len());
    let mut dropped: Vec<EventPassError> = Vec::new();
    let mut dropped_turns: Vec<(String, &'static str)> = Vec::new();
    let mut seen_refs: HashSet<String> = HashSet::new();

    for event in &doc.events {
        if event.reference.trim().is_empty() {
            dropped_turns.push((event.turn.clone(), EventPassError::Json.slug()));
            dropped.push(EventPassError::Json);
            continue;
        }
        if seen_refs.contains(event.reference.as_str()) {
            dropped_turns.push((
                event.turn.clone(),
                EventPassError::DuplicateRef(event.reference.clone()).slug(),
            ));
            dropped.push(EventPassError::DuplicateRef(event.reference.clone()));
            continue;
        }
        match validate_event(event, &refs) {
            Ok(draft) => {
                seen_refs.insert(event.reference.clone());
                covered.insert(event.turn.clone());
                covered.extend(event.evidence.iter().cloned());
                drafts.push(draft);
            }
            Err(error) => {
                dropped_turns.push((event.turn.clone(), error.slug()));
                dropped.push(error);
            }
        }
    }

    let uncovered = uncovered_turns(batch.rows.len(), &covered);
    Ok(LenientEvents { drafts, dropped, dropped_turns, uncovered })
}

/// 完成/已发生/已开始的标记。命中任意一个就不算"纯宣布"。
const COMPLETION_MARKERS: &[&str] = &[
    "已", "完成", "通过", "失败", "成功", "跑通", "提交", "落地", "结案", "修复", "还原",
    "删除", "创建", "更新", "发出", "收到", "获得", "确定", "决定", "采用", "裁",
    "红", "绿", "passed", "failed", "MUTATED", "开始", "正在", "进行中",
];

/// 句首就是"下一步"的语气词。只认句首（去掉前导标点/空白后），避免"先前/再读"误伤。
const ANNOUNCEMENT_HEADS: &[&str] = &[
    "现在", "先", "接下来", "接着", "然后", "最后", "准备", "将要", "即将",
    "下一步", "随后", "之后", "我来", "我去", "我再", "再",
];

/// 句中出现的"计划动作"短语。这些短语本身就说明"还没做"。
const ANNOUNCEMENT_ANY: &[&str] = &[
    "然后", "接着", "下一步", "我再", "我来", "我去", "先派", "先做", "先写", "先改",
    "先核", "先跑", "先确认", "先看", "核一下", "看一下", "确认一下", "查一下", "再核",
    "再确认", "再查", "并核",
];

/// `category=event` 的引文是不是"纯宣布"。
///
/// 判据分两步：先看有没有完成/已发生/已开始的标记（有 ⇒ 不是宣布）；
/// 再看句首或句中是否带计划语气（是 ⇒ 宣布）。只用于 event：
/// `observation`/`state_change` 的"现在服务在跑"是读数，不该被这条拦。
///
/// 这是**减误收**的闸，不是"事件识别器"：它放过的不一定对，只是把最像宣布的那批挡掉。
fn is_announcement_only(quote: &str) -> bool {
    if COMPLETION_MARKERS.iter().any(|marker| quote.contains(marker)) {
        return false;
    }
    let head = quote.trim_start_matches(|c: char| {
        c.is_whitespace() || "，。；;：:,、！!？?".contains(c)
    });
    ANNOUNCEMENT_HEADS.iter().any(|marker| head.starts_with(marker))
        || ANNOUNCEMENT_ANY.iter().any(|marker| quote.contains(marker))
}

/// 连续 ASCII 数字串。`2026-10-26` → `2026`/`10`/`26`；`12GB` → `12`。
/// 与 fact parser 同一把尺子，但各留一份：两个 pass 的 grounding 口径可以独立演进。
/// 把正文里的批内引用 `aN` 换成候选的正式名：**程序只解指针，不碰语义**。
/// 只认独立令牌（前后不是字母/数字/下划线），候选表里没有的保持原样（后面的数字接地会拒）。
fn resolve_text_refs(text: &str, refs: &EventRefs<'_>) -> String {
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

fn digit_runs(text: &str) -> Vec<String> {
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

// ── 审核的 parser：第三次调用的输出面 ────────────────────────────────────────
//
// 审核**只**产判决：一行 `(ref, verdict, code)`，不产 id、不产正文、不新增行。
//
// 19 格错误表没有给审核侧留"ref 不在待审集合/重复/缺失/verdict 闭集"这些格。
// 归并规则（每一处都写明"为什么是这一格"）：
// - 空的 `ref`、坏 `verdict`、`code` 与 verdict 不配、`code` 不在闭集 ⇒ `Json`
//   （这条回复不合契约，不是这一行的内容问题；fact parser 对空必填串也是这条裁定）；
// - 多给的 `ref` ⇒ `UnknownTurn`（它不在**本次调用**的任何一行上；与"引用批外轮"同形）；
// - 重复的 `ref` ⇒ `DuplicateRef`（与抽取侧"批内短引用重复"同一格）；
// - 缺失的 `ref` ⇒ `Uncovered`（"每个 ref 恰好一次"的另一半，按 `refs` 顺序全列）。
//
// 这样 S2c-2 的 runner 只需要认识 `slug()` 的全集（当前 24 个词，见该函数）；"哪一行判决缺失/多出"仍能从载荷读出。

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEventReviewOutput {
    schema_version: u32,
    pass: String,
    verdicts: Vec<RawReviewVerdict>,
}

/// 三键全必填：`code: null` 才是"没有 code"，缺键是形状错（不是"默认没有"）。
/// `code` 用 `serde_json::Value` 而不是 `Option<String>`：serde 对 `Option` 有隐式默认，
/// 那会把"模型把 code 键整个漏掉"读成"pass 没有 code"（与 fact 审核同一条理由）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReviewVerdict {
    #[serde(rename = "ref")]
    reference: String,
    verdict: String,
    code: serde_json::Value,
    /// G1 刀 B：价值轴那一格（缺键/`null` ⇒ `keep`）。event 审核只有**一次**调用，
    /// 所以没有"proposer 写、challenger 不写"的分叉；这里缺键就按 keep 读，
    /// 模型漏写不会变成"被迫 conservative demote"。
    #[serde(default)]
    value_axis: serde_json::Value,
}

/// 一次 event 审核的读数：逐条判决 + 逐条价值轴（顺序都跟 `refs` 走）。
#[derive(Debug, Clone, PartialEq)]
pub struct EventReviewReport {
    pub verdicts: Vec<(String, ReviewVerdict, Option<String>)>,
    /// G1 刀 B：`keep|demote` 闭集；闭集外的值在 parser 里硬拒。
    pub value_axis: Vec<(String, ValueAxis)>,
}

/// 解析一次 event 审核的输出。`refs` 是调用方按 prompt 里 `ITEM e1..eN` 的顺序给出的待审引用。
///
/// 校验顺序固定：形状 → 版本 → pass → 逐条（ref 非空 → 在 `refs` 内 → 不重复 →
/// verdict 闭集 → code 与 verdict 相配）→ 缺失。返回顺序**跟 `refs` 走**，不跟模型回复
/// 顺序走：调用方按位置把判决贴回草稿，回复里的顺序在这里就被消掉了。
///
/// S2e-2：与抽取同一把剥壳剪刀（[`json_payload`]）。
///
/// G1 刀 B：判决之外多收一格价值轴。旧入口 [`parse_event_review`] 保留原签名（只回判决），
/// 需要价值轴的调用方走 [`parse_event_review_full`]——两个入口共用同一份校验。
pub fn parse_event_review(
    raw: &str,
    refs: &[String],
) -> Result<Vec<(String, ReviewVerdict, Option<String>)>, EventPassError> {
    parse_event_review_full(raw, refs).map(|report| report.verdicts)
}

/// 见 [`parse_event_review`]；这一支把价值轴一起交出来。
pub fn parse_event_review_full(
    raw: &str,
    refs: &[String],
) -> Result<EventReviewReport, EventPassError> {
    let doc: RawEventReviewOutput =
        serde_json::from_str(json_payload(raw)).map_err(|_| EventPassError::Json)?;
    if doc.schema_version != EVENT_SCHEMA_VERSION {
        return Err(EventPassError::Version);
    }
    if doc.pass != PASS_EVENT_REVIEW {
        return Err(EventPassError::Pass);
    }

    let mut seen: HashMap<&str, (ReviewVerdict, Option<String>)> =
        HashMap::with_capacity(doc.verdicts.len());
    // G1 刀 B：价值轴按 ref 攒；闭集外的词整批硬拒，其它类型是形状错。
    let mut axes: HashMap<&str, ValueAxis> = HashMap::with_capacity(doc.verdicts.len());
    for item in &doc.verdicts {
        if item.reference.trim().is_empty() {
            return Err(EventPassError::Json);
        }
        if !refs.iter().any(|reference| reference == &item.reference) {
            return Err(EventPassError::UnknownTurn(item.reference.clone()));
        }
        if seen.contains_key(item.reference.as_str()) {
            return Err(EventPassError::DuplicateRef(item.reference.clone()));
        }
        let axis = match &item.value_axis {
            serde_json::Value::Null => ValueAxis::Keep,
            serde_json::Value::String(raw) => ValueAxis::parse(raw)
                .ok_or_else(|| EventPassError::BadValueAxis(item.reference.clone()))?,
            _ => return Err(EventPassError::Json),
        };
        let verdict = ReviewVerdict::parse(&item.verdict).ok_or(EventPassError::Json)?;
        // code 的类型先归一：null ⇒ 没有 code，字符串 ⇒ 有 code，其它类型是形状错。
        let raw_code = match &item.code {
            serde_json::Value::Null => None,
            serde_json::Value::String(code) => Some(code.as_str()),
            _ => return Err(EventPassError::Json),
        };
        let code = match (verdict, raw_code) {
            (ReviewVerdict::Pass, None) => None,
            // `uncertain` 允许不带码（台账 §10.26 缺陷 4）：判不了本身就是一个答案。
            (ReviewVerdict::Uncertain, None) => None,
            // pass 带 code、reject 缺 code、code 在闭集外，都是同一格：判决与
            // code 不配。载荷不把模型写的词回显出去。
            (_, Some(code)) if EVENT_REVIEW_CODES.contains(&code)
                && verdict != ReviewVerdict::Pass => Some(code.to_string()),
            _ => return Err(EventPassError::Json),
        };
        seen.insert(item.reference.as_str(), (verdict, code));
        axes.insert(item.reference.as_str(), axis);
    }

    let mut out = Vec::with_capacity(refs.len());
    let mut out_axes = Vec::with_capacity(refs.len());
    let mut missing: Vec<String> = Vec::new();
    for reference in refs {
        match seen.get(reference.as_str()) {
            Some((verdict, code)) => {
                out.push((reference.clone(), *verdict, code.clone()));
                out_axes.push((
                    reference.clone(),
                    axes.get(reference.as_str()).copied().unwrap_or(ValueAxis::Keep),
                ));
            }
            None => missing.push(reference.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(EventPassError::Uncovered(missing));
    }
    Ok(EventReviewReport { verdicts: out, value_axis: out_axes })
}
