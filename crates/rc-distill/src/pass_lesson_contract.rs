//! 第 6 pass（lesson）的输出契约：抽取侧的三元组 + 审核侧的逐条判决。
//!
//! 设计真源：`design/2026-11-08-g1-reuse-lesson-design.md` §波次三。这一层只做**形状**：
//! 一次一个语义任务的 `{title, description, content}` 三元组（ReasoningBank 形状）、
//! 一个可选的小闭集 `error_kind`、以及审核侧的四键判决。语义判断（"这一条错在哪、
//! 下次怎么做"）住 prompt，价值判断住审核，这里一格都不越界。
//!
//! 三条硬纪律（与 `pass_event_contract` 同源）：
//! 1. **不接收模型写的 id**：`deny_unknown_fields` 让 `ref`/`turn_uid`/`id`/`anchor`/`*_ms`
//!    这类键在多给一个字节时当场整批拒收；三元组之外的键一个都不读。
//! 2. **闭集硬拒**：`error_kind` 只认 [`LESSON_ERROR_KINDS`]，审核的 `code` 只认
//!    [`LESSON_REVIEW_CODES`]；闭集外的词是形状错，不是"折成默认值"。
//! 3. **长度为上界，不是建议**：标题 60 字符、描述与内容各 400 字符。超长整批拒收——
//!    截断会让"模型说了什么"与"库里写了什么"分叉。
//!
//! 空 `lessons`（`[]`）是**合法产出**：这一段的错误不值得一条规则。它不是解析失败，
//! 也不写成"零卡"以外的任何读数（设计 §波次三判据：无错轮产出 0 卡，空读数合法）。

use crate::batch::Batch;
use crate::pass_prompt::ReviewInputContext;
use serde::Deserialize;

/// 抽取契约的 `pass` 值（与 `pass_prompt::PASS_LESSON` 同一个词）。
pub const PASS_LESSON: &str = "lesson";
/// 审核契约的 `pass` 值：指向**哪一次调用**，所以与 [`PASS_LESSON`] 不同名。
pub const PASS_LESSON_REVIEW: &str = "lesson_review";
/// 契约版本号（与 fact/event 一样**不冒充 v4**）。
pub const LESSON_SCHEMA_VERSION: u32 = 0;

/// `error_kind` 的小闭集：错误的**轴**，不是错误本身。
///
/// 程序侧已经知道错误族的码（`extract_pass_state.reason`），模型只回答"这属于哪一轴"：
/// `grounding`（数字/事实没有出处）、`quoting`（引文与原文不一致）、`shape`（输出形状读不出来）、
/// `judgement`（判断本身错了，例如把宣布读成已完成）、`other`。轴只给审核与读数看，
/// 不进卡的 `kind`（那是另一种东西）。
pub const LESSON_ERROR_KINDS: [&str; 5] = ["grounding", "quoting", "shape", "judgement", "other"];

/// 审核 `code` 的闭集。`pass` 的 code 必须是 null。
pub const LESSON_REVIEW_CODES: [&str; 5] =
    ["ungrounded", "wrong_error", "not_actionable", "restates", "other"];

pub const LESSON_TITLE_MAX_CHARS: usize = 60;
pub const LESSON_DESCRIPTION_MAX_CHARS: usize = 400;
pub const LESSON_CONTENT_MAX_CHARS: usize = 400;
/// 一次调用最多交回几条（卡片数上界）。超过整批拒收：一批里塞三条规则通常意味着模型
/// 在把同一件事拆成三条，而不是真的有三条。
pub const LESSON_MAX_PER_BATCH: usize = 3;

/// 一条经验卡草稿。`turn_uid` 与 `evidence_turn_uids` 由**程序**按批填（模型递不进来）：
/// 代表轮取批内第一轮，证据是整批（同会话同错误族的这一段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LessonDraft {
    /// 位置编号 `l1..lN`（与 `lesson_review_user` 的 `ITEM l{n}` 同一套）。
    pub reference: String,
    pub turn_uid: String,
    pub title: String,
    pub description: String,
    pub content: String,
    /// [`LESSON_ERROR_KINDS`] 之一。
    pub error_kind: String,
    /// 程序给的错误族（`extract_pass_state.reason` 原词，不来自模型）。
    pub error_code: String,
    pub evidence_turn_uids: Vec<String>,
}

/// 拒收原因。每一格一个封闭 slug（进 `extract_pass_state.reason` / 判定 `reason_code`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LessonPassError {
    /// 不是 JSON、不是对象、缺键、多键（含模型自己写的 `ref`/`id`）、字段类型错。
    Json,
    /// `schema_version` 不是 0。
    Version,
    /// `pass` 不是 `"lesson"`（审核侧是 `"lesson_review"`）。
    Pass,
    /// 一次交回超过 [`LESSON_MAX_PER_BATCH`] 条。
    TooManyLessons,
    /// 三元组某格为空（空白也算空）。
    EmptyField(&'static str),
    /// 三元组某格超过上界。
    TooLong(&'static str),
    /// `error_kind` 不在 [`LESSON_ERROR_KINDS`] 里。
    BadErrorKind,
    /// 审核侧：同一个 ref 出现两次。
    DuplicateRef(String),
    /// 审核侧：有 ref 一条判决都没拿到（按 refs 顺序列出）。
    Uncovered(Vec<String>),
}

impl LessonPassError {
    /// 进库的封闭 slug。**故意不写 `_ =>`**（与 fact/event 的 slug 表同一条纪律）。
    pub fn slug(&self) -> &'static str {
        match self {
            LessonPassError::Json => "json",
            LessonPassError::Version => "version",
            LessonPassError::Pass => "pass",
            LessonPassError::TooManyLessons => "too_many_lessons",
            LessonPassError::EmptyField(_) => "empty_field",
            LessonPassError::TooLong(_) => "too_long",
            LessonPassError::BadErrorKind => "error_kind",
            LessonPassError::DuplicateRef(_) => "duplicate_ref",
            LessonPassError::Uncovered(_) => "uncovered",
        }
    }

    /// 审核侧失败落库用的 reason：`review_` + 同一格（与 event 的 `review_reason` 同前缀）。
    pub fn review_reason(&self) -> String {
        let slug = self.slug();
        if slug.starts_with("review_") {
            slug.to_string()
        } else {
            format!("review_{slug}")
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLessonOutput {
    schema_version: u32,
    pass: String,
    lessons: Vec<RawLesson>,
}

/// 四键，逐一对应 prompt 的输出契约。多一个键（`ref`/`id`/`turn`/`confidence`…）就整批拒收。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLesson {
    title: String,
    description: String,
    content: String,
    error_kind: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLessonReviewOutput {
    schema_version: u32,
    pass: String,
    verdicts: Vec<RawLessonVerdict>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLessonVerdict {
    #[serde(rename = "ref")]
    reference: String,
    verdict: String,
    #[serde(default)]
    code: serde_json::Value,
}

/// 一次审核的读数：逐条判决（顺序跟 `refs` 走，不跟回复顺序走）。
pub type LessonReviewReport = Vec<(String, crate::pass_fact_contract::ReviewVerdict, Option<String>)>;

/// 宽松的 JSON 壳：模型偶尔包一层 ```json 围栏或前后说一句话。与 event 同一把剪刀。
fn json_payload(raw: &str) -> &str {
    let trimmed = raw.trim();
    let without_fence = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|rest| rest.trim_start())
        .and_then(|rest| rest.strip_suffix("```").map(str::trim_end))
        .unwrap_or(trimmed);
    // 前后仍有散文时取第一个 `{` 到最后一个 `}`。
    match (without_fence.find('{'), without_fence.rfind('}')) {
        (Some(start), Some(end)) if start < end => &without_fence[start..=end],
        _ => without_fence,
    }
}

/// 解析一次 lesson 抽取的输出。
///
/// `batch` 与 `error_code` 由程序给：代表轮取 `batch.rows` 第一轮的 `turn_uid`，证据是整批
/// 的轮（顺序即批序）。**模型一个 id 都写不进来**——回复里出现 `turn`/`ref`/`id` 这类键，
/// `deny_unknown_fields` 当场按 [`LessonPassError::Json`] 拒。
///
/// 空数组 `lessons=[]` 是合法产出（`Ok(vec![])`），不是失败。
pub fn parse_lesson_output(
    raw: &str,
    batch: &Batch,
    error_code: &str,
) -> Result<Vec<LessonDraft>, LessonPassError> {
    let doc: RawLessonOutput =
        serde_json::from_str(json_payload(raw)).map_err(|_| LessonPassError::Json)?;
    if doc.schema_version != LESSON_SCHEMA_VERSION {
        return Err(LessonPassError::Version);
    }
    if doc.pass != PASS_LESSON {
        return Err(LessonPassError::Pass);
    }
    if doc.lessons.len() > LESSON_MAX_PER_BATCH {
        return Err(LessonPassError::TooManyLessons);
    }
    let Some(first) = batch.rows.first() else {
        // 空批不该走到契约层（选批就不会交出来）；真到了，没有轮可挂就是形状错。
        return Err(LessonPassError::Json);
    };
    let evidence: Vec<String> = batch.rows.iter().map(|row| row.turn_uid.clone()).collect();
    let mut out = Vec::with_capacity(doc.lessons.len());
    for (index, lesson) in doc.lessons.into_iter().enumerate() {
        let title = checked_field(lesson.title, "title", LESSON_TITLE_MAX_CHARS)?;
        let description =
            checked_field(lesson.description, "description", LESSON_DESCRIPTION_MAX_CHARS)?;
        let content = checked_field(lesson.content, "content", LESSON_CONTENT_MAX_CHARS)?;
        let error_kind = lesson.error_kind.trim().to_string();
        if !LESSON_ERROR_KINDS.contains(&error_kind.as_str()) {
            // 不回显模型写的词（守卫 12 的同一条取向：错误载荷不带自由文本）。
            return Err(LessonPassError::BadErrorKind);
        }
        out.push(LessonDraft {
            reference: format!("l{}", index + 1),
            turn_uid: first.turn_uid.clone(),
            title,
            description,
            content,
            error_kind,
            error_code: error_code.to_string(),
            evidence_turn_uids: evidence.clone(),
        });
    }
    Ok(out)
}

/// 三元组的一格：去首尾空白后非空、且在字符上界内。
fn checked_field(
    value: String,
    field: &'static str,
    max_chars: usize,
) -> Result<String, LessonPassError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(LessonPassError::EmptyField(field));
    }
    if trimmed.chars().count() > max_chars {
        return Err(LessonPassError::TooLong(field));
    }
    Ok(trimmed.to_string())
}

/// 解析一次 lesson 审核的输出。校验顺序固定：形状 → 版本 → pass → 逐条（ref 非空 →
/// 在 `refs` 内 → 不重复 → verdict 闭集 → code 与 verdict 相配）→ 缺失。
pub fn parse_lesson_review(
    raw: &str,
    refs: &[String],
) -> Result<LessonReviewReport, LessonPassError> {
    let doc: RawLessonReviewOutput =
        serde_json::from_str(json_payload(raw)).map_err(|_| LessonPassError::Json)?;
    if doc.schema_version != LESSON_SCHEMA_VERSION {
        return Err(LessonPassError::Version);
    }
    if doc.pass != PASS_LESSON_REVIEW {
        return Err(LessonPassError::Pass);
    }
    let mut seen: std::collections::HashMap<&str, (crate::pass_fact_contract::ReviewVerdict, Option<String>)> =
        std::collections::HashMap::with_capacity(doc.verdicts.len());
    for item in &doc.verdicts {
        if item.reference.trim().is_empty() {
            return Err(LessonPassError::Json);
        }
        if !refs.iter().any(|reference| reference == &item.reference) {
            // 待审集合之外的 ref 是形状错（不回显模型写的短 id）。
            return Err(LessonPassError::Json);
        }
        if seen.contains_key(item.reference.as_str()) {
            return Err(LessonPassError::DuplicateRef(item.reference.clone()));
        }
        let verdict = crate::pass_fact_contract::ReviewVerdict::parse(&item.verdict)
            .ok_or(LessonPassError::Json)?;
        let raw_code = match &item.code {
            serde_json::Value::Null => None,
            serde_json::Value::String(code) => Some(code.as_str()),
            _ => return Err(LessonPassError::Json),
        };
        let code = match (verdict, raw_code) {
            (crate::pass_fact_contract::ReviewVerdict::Pass, None) => None,
            // `uncertain` 允许不带码：判不了本身就是一个答案。
            (crate::pass_fact_contract::ReviewVerdict::Uncertain, None) => None,
            (_, Some(code))
                if LESSON_REVIEW_CODES.contains(&code)
                    && verdict != crate::pass_fact_contract::ReviewVerdict::Pass =>
            {
                Some(code.to_string())
            }
            _ => return Err(LessonPassError::Json),
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
        return Err(LessonPassError::Uncovered(missing));
    }
    Ok(out)
}

/// lesson 审核的归属批头（与 event 共用同一份 `ReviewInputContext`，只是不渲染候选）。
pub fn lesson_review_header(ctx: &ReviewInputContext) -> String {
    let mut s = format!(
        "SESSION: {} | owner: {} | visibility: {} | scope: {}\n",
        ctx.session_key, ctx.owner_agent_id, ctx.visibility, ctx.scope
    );
    s.push_str(&format!("SPEAKERS: {}\n", ctx.speakers.join(", ")));
    s
}
