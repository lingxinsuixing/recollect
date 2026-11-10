//! 纯离线 gold 评测（`recollect-eval pass`）：只读 GOLD-SPEC JSONL，不连库、不调模型。
//!
//! 格式的权威是 `GOLD-SPEC.md`；这里只做两件事：**逐行校验**与**分组算指标**。
//! 校验失败一律带 1 起行号与字段名（`GoldDefect`），CLI 映到退出码 2——
//! 「跳过一个坏行继续算」会把标注方的错误读成模型的分数，所以这里不做静默跳过。
//!
//! 指标口径（GOLD-SPEC §5）：
//!
//! - `precision = 判对 / judged`。`judged` 是「确实有一条草稿可判」的行数：
//!   `derive` 的 `missing` 表示草稿根本没产出，不进分母；其余 verdict 全进。
//!   「判对」那一档按 pass 取：`fact`/`event` 是 `supported`，`time`/`derive` 是 `correct`。
//!   后两个 pass 的闭集里**没有** `supported` 这个标签，照字面套 `supported` 会把一轮
//!   全对的 time 读成 0，那是把「尺子不认识这个词」记成模型的错。
//! - `recall = (expect=should_yield 且 actual_yielded) / should_yield`；
//! - `false_yield = (expect=should_not_yield 且 actual_yielded) / should_not_yield`；
//! - 分母为 0 ⇒ `None`（JSON `null`），**不写 0 也不写 1**：没有样本时的 0 与
//!   「全错」的 0 在运维面上长得一样，那是这把尺子最不该犯的混淆。

use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

/// `kind` 的两个取值（GOLD-SPEC §1）。
pub const KIND_DRAFT: &str = "draft";
pub const KIND_TURN: &str = "turn_expectation";
/// `expect` 的两个取值（GOLD-SPEC §3）。
pub const EXPECT_YIELD: &str = "should_yield";
pub const EXPECT_SILENT: &str = "should_not_yield";
/// `derive` 的 `missing`：草稿没产出，没有可判的对象（GOLD-SPEC §2）。
pub const VERDICT_MISSING: &str = "missing";

/// 四个评测类别。顺序即报表顺序（`BTreeMap` 的键序），与 GOLD-SPEC §1 的列举一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Pass {
    Fact,
    Event,
    Time,
    Derive,
}

impl Pass {
    /// 四个 pass 的闭集。`--pass` 的校验与 `--help` 的提示都从这里取，不另写一份。
    pub const ALL: [Pass; 4] = [Pass::Fact, Pass::Event, Pass::Time, Pass::Derive];

    pub fn as_str(self) -> &'static str {
        match self {
            Pass::Fact => "fact",
            Pass::Event => "event",
            Pass::Time => "time",
            Pass::Derive => "derive",
        }
    }

    pub fn parse(raw: &str) -> Option<Pass> {
        Pass::ALL.into_iter().find(|p| p.as_str() == raw)
    }

    /// `verdict` 的闭集（GOLD-SPEC §2），按 pass 各自一套。
    pub fn verdicts(self) -> &'static [&'static str] {
        match self {
            Pass::Fact => &[
                "supported",
                "wrong_subject",
                "condition_dropped",
                "unsupported",
                "hallucinated",
            ],
            Pass::Event => &[
                "supported",
                "wrong_actor",
                "result_unsupported",
                "not_event",
                "hallucinated",
            ],
            Pass::Time => &["correct", "wrong_value", "wrong_precision", "should_be_unknown"],
            Pass::Derive => &["correct", "wrong_relation", "hallucinated", VERDICT_MISSING],
        }
    }

    /// 该 pass 的「判对」那一档：precision 的分子。
    pub fn positive(self) -> &'static str {
        match self {
            Pass::Fact | Pass::Event => "supported",
            Pass::Time | Pass::Derive => "correct",
        }
    }
}

/// `expect` 闭集的类型化形态：解析过了就只剩两个分支，下游不再比字符串。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    ShouldYield,
    ShouldNotYield,
}

/// 一行 gold。只保留算指标要用的东西：行号在解析期用完即弃，
/// `item_id`/`draft_index` 只参与校验（本命令不读库，无法拿它们对齐任何行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoldRow {
    Draft { pass: Pass, verdict: String },
    Turn { pass: Pass, expect: Expect, yielded: bool },
}

impl GoldRow {
    pub fn pass(&self) -> Pass {
        match self {
            GoldRow::Draft { pass, .. } | GoldRow::Turn { pass, .. } => *pass,
        }
    }
}

/// 一条格式违规：1 起行号 + 出错字段 + 一句为什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoldDefect {
    pub line: usize,
    pub field: &'static str,
    pub why: String,
}

impl GoldDefect {
    fn new(line: usize, field: &'static str, why: impl Into<String>) -> Self {
        GoldDefect { line, field, why: why.into() }
    }
}

impl fmt::Display for GoldDefect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "gold_invalid: line {}: field `{}` {}", self.line, self.field, self.why)
    }
}

/// gold 文件的两种失败：读不到（路径/权限）与格式不对。两者在 CLI 上都是退出码 2，
/// 但分开报，运维面才知道该去修路径还是修标注。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoldError {
    Unreadable,
    Invalid(GoldDefect),
}

impl fmt::Display for GoldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GoldError::Unreadable => f.write_str("gold_unreadable: the gold file cannot be read"),
            GoldError::Invalid(defect) => write!(f, "{defect}"),
        }
    }
}

impl std::error::Error for GoldError {}

pub fn read_gold(path: &Path) -> Result<Vec<GoldRow>, GoldError> {
    let text = std::fs::read_to_string(path).map_err(|_| GoldError::Unreadable)?;
    parse_gold(&text).map_err(GoldError::Invalid)
}

/// 逐行解析。空行（含文件末尾的换行）跳过；其余每一行要么给出 `GoldRow`，要么给出
/// `GoldDefect`——没有第三种结果。
pub fn parse_gold(text: &str) -> Result<Vec<GoldRow>, GoldDefect> {
    let mut rows = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        if raw.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(raw)
            .map_err(|_| GoldDefect::new(line, "json", "is not a valid JSON value"))?;
        rows.push(parse_row(&value, line)?);
    }
    Ok(rows)
}

fn parse_row(value: &Value, line: usize) -> Result<GoldRow, GoldDefect> {
    let object = value
        .as_object()
        .ok_or_else(|| GoldDefect::new(line, "json", "is not a JSON object"))?;
    let kind = required_str(object, line, "kind")?;
    let pass = match Pass::parse(required_str(object, line, "pass")?) {
        Some(pass) => pass,
        None => {
            return Err(GoldDefect::new(
                line,
                "pass",
                "is not one of fact|event|time|derive",
            ))
        }
    };
    // 公共字段（GOLD-SPEC §1）：标注者与日期也必填——一条没有出处的判定无法追溯。
    required_str(object, line, "annotator")?;
    required_str(object, line, "annotated_at")?;
    required_str(object, line, "evidence")?;
    match kind {
        KIND_DRAFT => {
            let verdict = required_str(object, line, "verdict")?;
            if !pass.verdicts().contains(&verdict) {
                return Err(GoldDefect::new(
                    line,
                    "verdict",
                    format!("`{verdict}` is not in the {} closed set", pass.as_str()),
                ));
            }
            // GOLD-SPEC §4：fact/event 的草稿来自 `review_item.payload` ⇒ `item_id` + `draft_index`；
            // time/derive 不来自 review_item，用 `source_id`（time 可写 `time_expr_id`）替代。
            let has_item = object.get("item_id").is_some_and(|value| !value.is_null());
            if has_item {
                required_i64(object, line, "item_id")?;
                required_i64(object, line, "draft_index")?;
            } else if optional_str(object, "source_id")
                .or_else(|| optional_str(object, "time_expr_id"))
                .is_none()
            {
                return Err(GoldDefect::new(
                    line,
                    "item_id",
                    "is required (time/derive may use `source_id` or `time_expr_id` instead)",
                ));
            }
            Ok(GoldRow::Draft { pass, verdict: verdict.to_string() })
        }
        KIND_TURN => {
            let expect = match required_str(object, line, "expect")? {
                EXPECT_YIELD => Expect::ShouldYield,
                EXPECT_SILENT => Expect::ShouldNotYield,
                other => {
                    return Err(GoldDefect::new(
                        line,
                        "expect",
                        format!("`{other}` is not should_yield or should_not_yield"),
                    ))
                }
            };
            // `actual_yielded` 是抽样时从库里核对过的既成事实，harness 不联网重算（GOLD-SPEC §3）。
            let yielded = required_bool(object, line, "actual_yielded")?;
            Ok(GoldRow::Turn { pass, expect, yielded })
        }
        other => Err(GoldDefect::new(
            line,
            "kind",
            format!("`{other}` is not draft or turn_expectation"),
        )),
    }
}

fn required_str<'a>(
    object: &'a Map<String, Value>,
    line: usize,
    field: &'static str,
) -> Result<&'a str, GoldDefect> {
    match object.get(field) {
        None | Some(Value::Null) => Err(GoldDefect::new(line, field, "is required")),
        Some(Value::String(raw)) if raw.trim().is_empty() => {
            Err(GoldDefect::new(line, field, "must not be empty"))
        }
        Some(Value::String(raw)) => Ok(raw.as_str()),
        Some(_) => Err(GoldDefect::new(line, field, "must be a string")),
    }
}

fn required_i64(
    object: &Map<String, Value>,
    line: usize,
    field: &'static str,
) -> Result<i64, GoldDefect> {
    match object.get(field) {
        None | Some(Value::Null) => Err(GoldDefect::new(line, field, "is required")),
        Some(value) => value
            .as_i64()
            .ok_or_else(|| GoldDefect::new(line, field, "must be an integer")),
    }
}

fn required_bool(
    object: &Map<String, Value>,
    line: usize,
    field: &'static str,
) -> Result<bool, GoldDefect> {
    match object.get(field) {
        None | Some(Value::Null) => Err(GoldDefect::new(line, field, "is required")),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| GoldDefect::new(line, field, "must be a boolean")),
    }
}

/// 可选的非空字符串：time/derive 的溯源键（`source_id` / `time_expr_id`）。
fn optional_str<'a>(object: &'a Map<String, Value>, field: &'static str) -> Option<&'a str> {
    match object.get(field) {
        Some(Value::String(raw)) if !raw.trim().is_empty() => Some(raw.as_str()),
        _ => None,
    }
}

/// 一个 pass 的 draft 读数。`total` 是行数，`judged` 是其中「有草稿可判」的行数；
/// 两者只在 `derive` 的 `missing` 上分岔。
#[derive(Debug, Clone, Serialize)]
pub struct DraftMetrics {
    pub total: usize,
    pub judged: usize,
    /// 落在该 pass「判对」那一档的行数（`supported` | `correct`）。
    pub positive: usize,
    pub verdicts: BTreeMap<String, usize>,
    pub precision: Option<f64>,
}

/// 一个 pass 的 turn_expectation 读数。`yielded` / `false_yielded` 是两个分子，
/// 和分母一起报出来，读者不必反推比率是怎么来的。
#[derive(Debug, Clone, Serialize)]
pub struct TurnMetrics {
    pub total: usize,
    pub should_yield: usize,
    pub should_not_yield: usize,
    pub yielded: usize,
    pub false_yielded: usize,
    pub recall: Option<f64>,
    pub false_yield: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PassReport {
    pub pass: Pass,
    /// 落进这一组的 gold 行数（draft + turn_expectation）。
    pub rows: usize,
    pub drafts: DraftMetrics,
    pub turn_expectations: TurnMetrics,
}

#[derive(Debug, Clone, Serialize)]
pub struct GoldReport {
    pub passes: Vec<PassReport>,
}

#[derive(Default)]
struct Acc {
    rows: usize,
    drafts_total: usize,
    judged: usize,
    positive: usize,
    verdicts: BTreeMap<String, usize>,
    turn_total: usize,
    should_yield: usize,
    should_not_yield: usize,
    yielded: usize,
    false_yielded: usize,
}

impl Acc {
    fn draft(&mut self, pass: Pass, verdict: &str) {
        self.rows += 1;
        self.drafts_total += 1;
        *self.verdicts.entry(verdict.to_string()).or_insert(0) += 1;
        if verdict == VERDICT_MISSING {
            return;
        }
        self.judged += 1;
        if verdict == pass.positive() {
            self.positive += 1;
        }
    }

    fn turn(&mut self, expect: Expect, yielded: bool) {
        self.rows += 1;
        self.turn_total += 1;
        match expect {
            Expect::ShouldYield => {
                self.should_yield += 1;
                if yielded {
                    self.yielded += 1;
                }
            }
            Expect::ShouldNotYield => {
                self.should_not_yield += 1;
                if yielded {
                    self.false_yielded += 1;
                }
            }
        }
    }

    fn report(self, pass: Pass) -> PassReport {
        PassReport {
            pass,
            rows: self.rows,
            drafts: DraftMetrics {
                total: self.drafts_total,
                judged: self.judged,
                positive: self.positive,
                verdicts: self.verdicts,
                precision: rate(self.positive, self.judged),
            },
            turn_expectations: TurnMetrics {
                total: self.turn_total,
                should_yield: self.should_yield,
                should_not_yield: self.should_not_yield,
                yielded: self.yielded,
                false_yielded: self.false_yielded,
                recall: rate(self.yielded, self.should_yield),
                false_yield: rate(self.false_yielded, self.should_not_yield),
            },
        }
    }
}

/// 分母 0 ⇒ `None`。这是全文件唯一的一处除法，别处不许再写 `x / y`。
fn rate(hit: usize, total: usize) -> Option<f64> {
    if total == 0 {
        None
    } else {
        Some(hit as f64 / total as f64)
    }
}

/// 按 pass 分组出报表。`only = None` 时只出文件里出现过的 pass（各自一份）；
/// `only = Some(p)` 时固定出这一份——**即使文件里没有这个 pass 的行**，
/// 也是一份 total 0、指标全 `null` 的报表，而不是什么都不打（无声的空输出最容易被读成"全绿"）。
pub fn evaluate(rows: &[GoldRow], only: Option<Pass>) -> GoldReport {
    let mut acc: BTreeMap<Pass, Acc> = BTreeMap::new();
    if let Some(pass) = only {
        acc.insert(pass, Acc::default());
    }
    for row in rows {
        if only.is_some_and(|pass| pass != row.pass()) {
            continue;
        }
        match row {
            GoldRow::Draft { pass, verdict } => acc.entry(*pass).or_default().draft(*pass, verdict),
            GoldRow::Turn { pass, expect, yielded } => {
                acc.entry(*pass).or_default().turn(*expect, *yielded)
            }
        }
    }
    GoldReport { passes: acc.into_iter().map(|(pass, acc)| acc.report(pass)).collect() }
}
