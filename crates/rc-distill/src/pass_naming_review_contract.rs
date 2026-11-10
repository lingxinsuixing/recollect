//! S3b-3a：naming 窄审的严格 parser（设计 `2026-10-26-review-queue-s3` §6 的 naming 行）。
//!
//! 这一层只回答"模型给什么形状、程序怎么拒"：不选批、不调模型、不写库、不入队。
//! 它与 `pass_fact_contract::parse_fact_review` 同强度：任何违规都是**硬拒**，没有
//! "丢一条、留其余"的中间态；错误面同样只带 slug 与批内短引用，不回显模型正文。
//!
//! 与 fact 审核的差别只在**问什么**（这里问锚的名字，不问引文），不在"怎么拒"：
//! - 信封同一条路：`json_payload` 剥整段围栏 → 顶层恰好三键 → `schema_version == 0` →
//!   `pass == PASS_NAMING_REVIEW`；
//! - 每个 `verdicts[]` 条目只许 `ref/verdict/code` 三键，`ref` 必须命中调用方给的 `refs`
//!   恰一次，返回顺序**跟 `refs` 走**（回复里的顺序在这里就被消掉了）；
//! - `verdict` 闭集 `pass|reject|uncertain`；`pass` 的 code 必须是 null，其余必须是
//!   六码闭集里的非空串。
//!
//! 错误码是封闭词：`slug()` **故意不写 `_ =>`**（与 `ContractError::slug`、
//! `FactPassError::slug` 同一条纪律），新增变体必须在这张表里表态。`CodeMissing` 与
//! `CodeUnknown` 共用 `verdict_code` 一词：运维面读的是"code 与 verdict 不配"，
//! 细分留在这个枚举里给测试与排障看。
//!
//! `code` 的**缺键**按"没有写出 null"处理：`pass` 缺 code ⇒ [`NamingReviewError::CodeOnPass`]
//! （契约要求显式 null），非 `pass` 缺 code ⇒ [`NamingReviewError::CodeMissing`]。两条都是
//! 更严的那一支——不把"漏了一个必填键"读成"默认没有 code"。

use crate::pass_fact_contract::{json_payload, ReviewVerdict};
use crate::pass_prompt::{NAMING_REVIEW_CODES, PASS_NAMING_REVIEW};
use serde_json::Value;
use std::collections::HashMap;

/// 本契约的版本号：与 fact/event 一样**不冒充 v4**（设计 §2 "新契约用 `schema_version=0`"）。
const NAMING_REVIEW_SCHEMA_VERSION: u64 = 0;

/// 顶层三键（顺序即契约文档里的顺序，逐字给出）。
const TOP_KEYS: [&str; 3] = ["schema_version", "pass", "verdicts"];

/// 判决条目的三键。
const VERDICT_KEYS: [&str; 3] = ["ref", "verdict", "code"];

/// 一次 naming 窄审的拒收原因。载荷只放批内短引用（`ref`），不放模型写的 verdict/code，
/// 更不能放模型正文（守卫 12）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamingReviewError {
    /// 不是 JSON、不是对象、缺必填键、字段类型错。
    Json,
    /// `schema_version` 不是 0。
    SchemaVersion,
    /// `pass` 不是 `"naming_review"`。
    Pass,
    /// `verdicts` 不是数组。
    VerdictsShape,
    /// 判决的 `ref` 不在调用方给的 `refs` 里。
    RefUnknown,
    /// 同一个 `ref` 出现两次。
    RefDuplicate,
    /// 有 `ref` 一条判决都没拿到。
    RefMissing,
    /// `verdict` 不在 `pass|reject|uncertain` 闭集里。
    VerdictUnknown,
    /// 非 `pass` 的判决缺 code（缺键 / null / 空串）。
    CodeMissing,
    /// code 不在 [`NAMING_REVIEW_CODES`] 里。
    CodeUnknown,
    /// `pass` 带了 code（缺键或非 null）。
    CodeOnPass,
    /// 顶层或判决条目多出一个键。
    ExtraKey,
}

impl NamingReviewError {
    /// 封闭 slug。只给"坏在哪一格"，不给模型写的词，也不给正文。
    pub fn slug(&self) -> &'static str {
        match self {
            NamingReviewError::Json => "json",
            NamingReviewError::SchemaVersion => "schema_version",
            NamingReviewError::Pass => "pass",
            NamingReviewError::VerdictsShape => "verdicts",
            NamingReviewError::RefUnknown => "ref_unknown",
            NamingReviewError::RefDuplicate => "ref_duplicate",
            NamingReviewError::RefMissing => "ref_missing",
            NamingReviewError::VerdictUnknown => "verdict",
            NamingReviewError::CodeMissing => "verdict_code",
            NamingReviewError::CodeUnknown => "verdict_code",
            NamingReviewError::CodeOnPass => "code_on_pass",
            NamingReviewError::ExtraKey => "extra_key",
        }
    }
}

/// `code` 这一格的三种形状。缺键与显式 null 分开：前者是"没写"，后者是"写了没有"，
/// 两者对 `pass` 的判定不同（见模块文档）。
enum RawCode<'a> {
    Absent,
    Null,
    Text(&'a str),
}

/// 解析一次 naming 窄审的输出。`refs` 是调用方按 prompt 里 `ANCHOR a1..aN` 的顺序给出的待审引用。
///
/// 校验顺序固定：形状（多键先于缺键）→ 版本 → pass → `verdicts` 形状 → 逐条
/// （键集 → ref 非空/在 `refs` 内/不重复 → verdict 闭集 → code 与 verdict 相配）→ 缺失。
/// 返回顺序**跟 `refs` 走**，不跟模型回复顺序走。
///
/// `verdicts.len() != refs.len()` 不另设一格：条目多了必有一个 `ref` 未知或重复，条目少了
/// 必有一个 `ref` 缺席，三种情况在逐条与缺失两步里各自落 `RefUnknown`/`RefDuplicate`/`RefMissing`。
pub fn parse_naming_review(
    raw: &str,
    refs: &[String],
) -> Result<Vec<(String, ReviewVerdict, Option<String>)>, NamingReviewError> {
    let doc: Value =
        serde_json::from_str(json_payload(raw)).map_err(|_| NamingReviewError::Json)?;
    let top = doc.as_object().ok_or(NamingReviewError::Json)?;
    // 多键先判：多出来的键可能根本是另一次作业的输出，它比"缺了哪个键"更该先说。
    if top.keys().any(|key| !TOP_KEYS.contains(&key.as_str())) {
        return Err(NamingReviewError::ExtraKey);
    }
    if top.keys().len() != TOP_KEYS.len() {
        return Err(NamingReviewError::Json);
    }

    let schema_version = top["schema_version"].as_u64().ok_or(NamingReviewError::Json)?;
    if schema_version != NAMING_REVIEW_SCHEMA_VERSION {
        return Err(NamingReviewError::SchemaVersion);
    }
    let pass = top["pass"].as_str().ok_or(NamingReviewError::Json)?;
    if pass != PASS_NAMING_REVIEW {
        return Err(NamingReviewError::Pass);
    }
    let verdicts = top["verdicts"].as_array().ok_or(NamingReviewError::VerdictsShape)?;

    // `ref → (verdict, code)`。值的拥有者在这里，不借模型的 `Value` 树。
    let mut seen: HashMap<String, (ReviewVerdict, Option<String>)> =
        HashMap::with_capacity(verdicts.len());
    for item in verdicts {
        let entry = item.as_object().ok_or(NamingReviewError::Json)?;
        if entry.keys().any(|key| !VERDICT_KEYS.contains(&key.as_str())) {
            return Err(NamingReviewError::ExtraKey);
        }
        let reference = entry
            .get("ref")
            .and_then(Value::as_str)
            .ok_or(NamingReviewError::Json)?;
        let verdict_raw = entry
            .get("verdict")
            .and_then(Value::as_str)
            .ok_or(NamingReviewError::Json)?;
        if reference.trim().is_empty() {
            // 空 ref 没有身份：它既不能命中 `refs`，也不能去重（与 fact 审核同一格）。
            return Err(NamingReviewError::Json);
        }
        if !refs.iter().any(|known| known == reference) {
            return Err(NamingReviewError::RefUnknown);
        }
        if seen.contains_key(reference) {
            return Err(NamingReviewError::RefDuplicate);
        }
        let verdict = ReviewVerdict::parse(verdict_raw).ok_or(NamingReviewError::VerdictUnknown)?;
        let raw_code = match entry.get("code") {
            None => RawCode::Absent,
            Some(Value::Null) => RawCode::Null,
            Some(Value::String(code)) => RawCode::Text(code),
            // 数字/布尔/对象/数组都不是 code：类型错归形状。
            Some(_) => return Err(NamingReviewError::Json),
        };
        let code = match (verdict, raw_code) {
            // pass 必须写出 null：缺键与任何非 null 值都是"给判决配了码"。
            (ReviewVerdict::Pass, RawCode::Null) => None,
            (ReviewVerdict::Pass, _) => return Err(NamingReviewError::CodeOnPass),
            (_, RawCode::Absent) | (_, RawCode::Null) => {
                return Err(NamingReviewError::CodeMissing)
            }
            (_, RawCode::Text(code)) if code.trim().is_empty() => {
                return Err(NamingReviewError::CodeMissing)
            }
            (_, RawCode::Text(code)) if NAMING_REVIEW_CODES.contains(&code) => {
                Some(code.to_string())
            }
            (_, RawCode::Text(_)) => return Err(NamingReviewError::CodeUnknown),
        };
        seen.insert(reference.to_string(), (verdict, code));
    }

    let mut out = Vec::with_capacity(refs.len());
    let mut missing: Vec<String> = Vec::new();
    for reference in refs {
        match seen.get(reference) {
            Some((verdict, code)) => out.push((reference.clone(), *verdict, code.clone())),
            None => missing.push(reference.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(NamingReviewError::RefMissing);
    }
    Ok(out)
}
