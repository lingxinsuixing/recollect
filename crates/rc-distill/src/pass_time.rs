//! S2d-1 / S2d-2：时间 pass 的确定性半与 runner（设计 `2026-10-26-s2-prompt-v0` §2.4、§3、§7）。
//!
//! 三块，边界写在这里：
//!
//! 1. [`parse_time_text`]：程序把 v0 闭集里的短语归一成 `start_ms/end_ms/precision/basis/kind`。
//!    时区固定 +08:00（与 `rc_agent::recall::TZ_OFFSET_MIN` **同一处真相**，这里只做 i64 收窄），
//!    存储一律 UTC 毫秒；日期算术自己写（`days_from_civil` / `civil_from_days`），不引入
//!    chrono/time 依赖。闭集外、非法日期、空串 ⇒ `None/None/unknown/none/<默认 kind>`，
//!    **不猜**（设计 §2.4 的反例：`改天/回头/最近` 不许编出具体日期）。
//! 2. [`parse_pending_time_expr`] + [`apply_time_resolutions`]：只读扫描 → 单事务 CAS 更新
//!    `time_expr`（`WHERE id=? AND basis='none'`）→ 只有 UPDATE 真改了 1 行才按 kind 回填
//!    `fact` 的时间列，并把刚回填的那一组时间镜像到卡的 `time_json`（T33：写卡时刻没有
//!    解析结果，卡面时间只能在回填时补；`card.time_json IS NULL` 才写，重跑不覆盖）。
//!    解析不出的行**原地不动**（下一轮仍会扫到）。旧 API
//!    [`resolve_pending_time_expr`] 保留为"parse + apply"的薄封装。
//! 3. [`select_time_turns`] + [`run_time`]（S2d-2 / S3a-3c）：选批 → 给选中轮落 `running` →
//!    解析结果**先入待审队列**（`review_item` + 程序闸 + `program_only` 判定）→ 只有闸 pass
//!    才 apply → 每组按终态收尾。**不调 LLM**：v0 的 time pass 是纯程序。
//!
//! **不做**（S2d-2）：LLM、`extraction_issue`。
//! 这里的 `kind` 只决定回填哪几列，不代表任何"审核通过"；审核通过由 item 状态表示。
//!
//! 精度与区间的约定（毫秒）：
//! - `day` ⇒ 本地日 00:00:00.000 .. 23:59:59.999（周几也按 day）；
//! - `hour` ⇒ **精确时刻**用退化区间 `[t, t]`（带偏移的 ISO、裸 `HH:MM`、`现在`、
//!   `下周X上午十点`：短语自己说的是某一刻，不是"那一小时"）；只有整点桶形态
//!   （`2026-11-05 18:30`、`昨晚`）仍是 `[hh:00.000, hh:59:59.999]`；
//! - `month` ⇒ 该月 1 日 00:00 .. 月末 23:59:59.999；
//! - `?之前|以前|截止` ⇒ `kind=deadline`：日历单元取整段（当日/当月末），带日期的精确
//!   时刻取 `[t, t]`，**裸时刻**只给上界 `[NULL, t]`（不知道"之前"从哪一秒算起，不猜）；
//! - `?之后|以后|起|一过` ⇒ `kind=validity`，左端是单元头/时刻，**右端开口（NULL）**；
//! - `每周|每天|每月` ⇒ `kind=recurrence`，两端 NULL、precision 是周期粒度。
//!
//! **C2 起**（`2026-10-28-duty-split-and-code-fixes` §2 C2）：闭集扩到带偏移/秒的 ISO、
//! 裸 `HH:MM`（+`前/后`）、`现在/刚才`、`下周X上午十点前`、`接下来 N 天`、周期短语；
//! 解析不出的轮**不再落 Done**——它们落 `stale(reason=unresolved)`，解析器版本升级
//! （[`TIME_PIPELINE`] 变）后可被重新选中回填。

use crate::batch::{Batch, TurnRow};
use crate::review;
use crate::DistillError;
use rc_ledger::pass_state::{self, PassKey, PassState};
use rc_ledger::water;
use rc_memory::owner::{self, OwnerCtx};
use rc_memory::review::{enqueue, get, ReviewStage, ReviewState as QueueState, ReviewVerdict as QueueVerdict};
use rc_memory::writers_fact::time_kind_for;
use rc_memory::{BatchCtx, MemError, Scope, ScopeKind};
use rc_store::{LedgerHandle, MemoryHandle, StoreError, TranscriptHandle};
use rusqlite::params;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// +08:00（毫秒）。唯一真相在 `rc_agent::recall::TZ_OFFSET_MIN`：改时区只改那一处。
const TZ_MS: i64 = rc_agent::recall::TZ_OFFSET_MIN as i64 * 60_000;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;
/// 晚间短语（`昨晚|今晚|明晚`）的本地钟点：20:00。
const EVENING_HOUR: i64 = 20;

/// 一个短语的解析结果。五格与 `time_expr` 的对应列一一对齐，`&'static str` 是 DDL 的 CHECK
/// 闭集（`precision ∈ hour|day|week|month|unknown`、`basis ∈ turn_ts|explicit|inferred|none`、
/// `kind ∈ event_time|speech_time|validity|deadline|recurrence`）。
///
/// `start_ms/end_ms` 是 UTC 毫秒；解析不出时两格为 `None`（不是 0、不是"今天随便给一个"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTime {
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub precision: &'static str,
    pub basis: &'static str,
    pub kind: &'static str,
}

/// 一次回填的读数。
/// - `expressions`：扫到的 `basis='none'` 行数（含解析不出的）；
/// - `resolved`：其中真算出 `start_ms` 的行数（这些行的 `time_expr` 被更新）；
/// - `facts`：被回填的 `fact` 行数（三个 `UPDATE` 的 `changes()` 之和）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimeResolveReport {
    pub expressions: usize,
    pub resolved: usize,
    pub facts: usize,
}

/// 一条**能解析**的待回填结果（S3a-3c）。它是载荷里 `items` 的一格，也是 apply 的 CAS 输入。
///
/// `time_expr_id` 是 `time_expr` 的内容寻址主键：真实表结构里是 `TEXT`（64 位十六进制，
/// `compute_time_expr_id`），**不是**自增行号。派发档写的 `i64` 与真实表冲突，以表为准。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeResolution {
    pub time_expr_id: String,
    pub turn_uid: String,
    pub text: String,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub precision: String,
    pub basis: String,
    pub kind: String,
}

/// 把一句时间短语归一成确定的 UTC 毫秒区间 + 精度 + 依据 + 类别。
///
/// `turn_ms` 是这一轮的源轮时刻（UTC 毫秒），相对短语（今天/昨晚/下周X/N天/现在/裸钟点）
/// 都以它的**本地日历**为参照；`category` 是这一行的 fact 类别，用于取默认 `kind`（与写库层
/// `time_kind_for` 同一把尺子）。
///
/// 优先级（闭集，先命中先返回）：
/// 0. 周期：`每周|每天|每天定时|每月` ⇒ `recurrence`（无单次锚点）；
/// 1. 指示语：`现在|此刻|当前|刚才|刚刚` ⇒ 轮时刻本身（`speech_time`）；
/// 2. `接下来N天`（以轮时刻为左端的跨度）；
/// 3. `N天/小时后|前|之后|之前|以前`（偏移短语，`之前/之后` 在这里是偏移的一部分）；
/// 4. 剥尾部近似词 `左右`；
/// 5. 剥尾部方向词 `之前|以前|截止|一过|之后|以后|起|前|后`；
/// 6. 带偏移/秒的 ISO（`2026-10-27T08:04+08:00`）⇒ 精确时刻；`YYYY-MM-DD[ HH:MM|THH:MM]`、
///    `YYYY年M月D日`、`M月D日`（当年）⇒ 日历单元；`今天|明日… 20:00`、`周X 上午十点`、
///    裸 `HH:MM[:SS]` / `H点[半]` ⇒ 精确时刻；
/// 7. `上周X|这周X|本周X|下周X|周X|星期X`；
/// 8. `今天|今日|昨天|昨日|前天|明天|明日|后天`、`昨晚|今晚|明晚`、`本月|这个月|下个月|下月`。
///
/// `basis` 的取法：文本自带日历分量 ⇒ `explicit`；短语就是轮所在的本地日/月/刻
/// （`今天|今日|今晚|现在` 与 `本月|这个月`）⇒ `turn_ts`；需要以轮为锚做平移
/// （`昨天|明天|昨晚|明晚|周X|N天|N小时|下周X|裸 HH:MM|接下来N天`）⇒ `inferred`；
/// 解析不出 ⇒ `none`。
pub fn parse_time_text(text: &str, turn_ms: i64, category: &str) -> ParsedTime {
    let raw = text.trim();
    let unresolvable = ParsedTime {
        start_ms: None,
        end_ms: None,
        precision: "unknown",
        basis: "none",
        kind: time_kind_for(category, raw),
    };
    if raw.is_empty() { return unresolvable; }

    // 解析只在"去空白"的紧凑形上做（`20:00 前`、`接下来 5 天`、`今天 13:04` 都带空格）；
    // 原文 `raw` 仍用于 kind 判定与 span 回溯，一个字都不改。
    let compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();

    // ⓪ 周期短语：没有单次锚点，这不是"解析失败"——`start/end` 留空是对的。
    if let Some(precision) = parse_recurrence(&compact) {
        return ParsedTime {
            start_ms: None,
            end_ms: None,
            precision,
            basis: "explicit",
            kind: "recurrence",
        };
    }

    // ① 指示语：`现在/刚才` 的指称就是这一轮的时刻（唯一稳定的锚是轮 `created_at_ms`）。
    if is_moment(&compact) {
        return ParsedTime {
            start_ms: Some(turn_ms),
            end_ms: Some(turn_ms),
            precision: "hour",
            basis: "turn_ts",
            kind: "speech_time",
        };
    }

    // ② `接下来 N 天`：左端是轮时刻，右端 N×24h（精确端点，不落到日桶）。
    if let Some((start, end)) = parse_next_days(&compact, turn_ms) {
        return ParsedTime {
            start_ms: Some(start),
            end_ms: Some(end),
            precision: "day",
            basis: "inferred",
            kind: time_kind_for(category, raw),
        };
    }

    // ③ 偏移短语先试：`3天之前` 里的「之前」是偏移的一部分，不能被 ⑤ 当成 before 修饰。
    if let Some((start, end, precision)) = parse_offset(&compact, turn_ms) {
        return resolved(start, end, precision, "inferred", category, raw, Modifier::None);
    }

    // ④ 近似词 `左右` 不跨桶：只说明"大约"，不改区间。
    let body = compact.strip_suffix("左右").unwrap_or(&compact);

    // ⑤ 尾部方向词。`20:00 一过` 的「一过」与 `20:00 额度重置后` 的「后」都在这一层剥。
    let (core, modifier) = split_modifier(body);
    let core = core.trim();
    if core.is_empty() { return unresolvable; }
    let turn_day = local_day(turn_ms);

    let hit = if let Some(ms) = parse_iso_instant(core) {
        Some(Hit::Instant { ms, basis: "explicit", dated: true })
    } else if let Some((ms, dated)) = parse_clock_phrase(core, turn_day, modifier != Modifier::None) {
        Some(Hit::Instant { ms, basis: "inferred", dated })
    } else if let Some(unit) = parse_iso(core) {
        Some(Hit::Unit(unit, "explicit"))
    } else if let Some(unit) = parse_cn_date(core, turn_day) {
        Some(Hit::Unit(unit, "explicit"))
    } else if let Some((unit, basis)) = parse_weekday(core, turn_day) {
        Some(Hit::Unit(unit, basis))
    } else if let Some((unit, basis)) = parse_keyword(core, turn_day) {
        Some(Hit::Unit(unit, basis))
    } else {
        None
    };
    match hit {
        Some(Hit::Unit(unit, basis)) => {
            let (start, end) = unit.bounds();
            resolved(start, end, unit.precision(), basis, category, raw, modifier)
        }
        Some(Hit::Instant { ms, basis, dated }) => {
            resolved_instant(ms, basis, dated, category, raw, modifier)
        }
        None => unresolvable,
    }
}

/// 命中形态：日历单元（有宽度的日/月/整点桶）或精确时刻（`[t, t]` 的退化区间）。
enum Hit {
    Unit(Unit, &'static str),
    /// `dated` = 短语自己报了日历日（ISO/`今天 10:00`/`下周X上午十点`）；裸钟点为 false。
    Instant { ms: i64, basis: &'static str, dated: bool },
}

/// 精确时刻的组装（精度一律 `hour`：闭集里没有 minute，但值保留毫秒）。
fn resolved_instant(
    ms: i64,
    basis: &'static str,
    dated: bool,
    category: &str,
    phrase: &str,
    modifier: Modifier,
) -> ParsedTime {
    match modifier {
        Modifier::None => ParsedTime {
            start_ms: Some(ms),
            end_ms: Some(ms),
            precision: "hour",
            basis,
            kind: time_kind_for(category, phrase),
        },
        // 截止：带日期的时刻是"到期那一刻"⇒ 点；裸时刻只声明上界，左端不猜。
        Modifier::Before => ParsedTime {
            start_ms: if dated { Some(ms) } else { None },
            end_ms: Some(ms),
            precision: "hour",
            basis,
            kind: "deadline",
        },
        // 一过/之后：开口区间，右端 NULL 是"没有终点"，不是"解析失败"。
        Modifier::After => ParsedTime {
            start_ms: Some(ms),
            end_ms: None,
            precision: "hour",
            basis,
            kind: "validity",
        },
    }
}

/// 组装解析成功的结果：没有修饰 ⇒ kind 按类别（[`time_kind_for`]）；`before` ⇒ deadline
/// （右端 = 单元末）；`after` ⇒ validity（右端开口、NULL）。
fn resolved(
    start: i64, end: i64, precision: &'static str, basis: &'static str,
    category: &str, phrase: &str, modifier: Modifier,
) -> ParsedTime {
    match modifier {
        Modifier::None => ParsedTime {
            start_ms: Some(start),
            end_ms: Some(end),
            precision,
            basis,
            kind: time_kind_for(category, phrase),
        },
        // 截止：右端就是单元末（`bounds` 已给），整段区间连同左端一起保留。
        Modifier::Before => ParsedTime {
            start_ms: Some(start),
            end_ms: Some(end),
            precision,
            basis,
            kind: "deadline",
        },
        // 之后/起：开口区间。右端 NULL 是"没有终点"，不是"解析失败"。
        Modifier::After => ParsedTime {
            start_ms: Some(start),
            end_ms: None,
            precision,
            basis,
            kind: "validity",
        },
    }
}

/// 尾部修饰词。`之前|以前|截止|前` ⇒ before；`一过|之后|以后|起|后` ⇒ after。只认**整句尾部**：
/// 句中出现（`下周三之前再说`）不猜，整句落 unknown。
///
/// 裸 `前`/`后` 排在 `之前/之后` 之后（先长后短），`一过` 排第一（它只表示"过点之后"）。
/// `周五下班前` 剥出 `周五下班` 后仍然解析不出（`下班` 依赖个人作息）⇒ 落 unknown，不猜。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Modifier {
    None,
    Before,
    After,
}

fn split_modifier(text: &str) -> (&str, Modifier) {
    for (suffix, modifier) in [
        ("一过", Modifier::After),
        ("之前", Modifier::Before),
        ("以前", Modifier::Before),
        ("截止", Modifier::Before),
        ("之后", Modifier::After),
        ("以后", Modifier::After),
        ("起", Modifier::After),
        ("前", Modifier::Before),
        ("后", Modifier::After),
    ] {
        if let Some(core) = text.strip_suffix(suffix) {
            return (core, modifier);
        }
    }
    (text, Modifier::None)
}

/// 周期短语（`2026-10-28` §2 C2 的 recurrence）。**整句**匹配：`每周一次` 认，`每周三` 不认
/// （那是单次周日，走 [`parse_weekday`]）。没有单次锚点 ⇒ 两端留空。
fn parse_recurrence(text: &str) -> Option<&'static str> {
    match text {
        "每天" | "每日" | "每天定时" | "每日定时" | "每天都" => Some("day"),
        "每周" | "每星期" | "每周一次" | "每周定时" | "每礼拜" => Some("week"),
        "每月" | "每个月" => Some("month"),
        _ => None,
    }
}

/// 指示语（`现在/刚才`）：指称唯一 = 说话时刻 = 源轮 `created_at_ms`。
/// 只认**整句**：`现在起` 这类带方向词的写法在 ⑤ 剥完修饰后落 unknown，不猜区间。
fn is_moment(text: &str) -> bool {
    matches!(text, "现在" | "此刻" | "当前" | "刚才" | "刚刚")
}

/// `接下来 N 天`：左端 = 轮时刻，右端 = 轮时刻 + N×24h（标注口径 §5.6：精确端点）。
fn parse_next_days(text: &str, turn_ms: i64) -> Option<(i64, i64)> {
    let rest = text.strip_prefix("接下来")?;
    let digits = rest.strip_suffix('天')?;
    let count = parse_digits(digits)?;
    if count <= 0 { return None; }
    Some((turn_ms, turn_ms + count * DAY_MS))
}


/// 已定位到本地日历上的一个单元。`bounds()` 给出闭区间的两端（UTC 毫秒）。
#[derive(Debug, Clone, Copy)]
enum Unit {
    Day { days: i64 },
    Hour { days: i64, hh: i64, mm: i64 },
    Month { year: i64, month: u32 },
}

impl Unit {
    fn precision(self) -> &'static str {
        match self {
            Unit::Day { .. } => "day",
            Unit::Hour { .. } => "hour",
            Unit::Month { .. } => "month",
        }
    }

    fn bounds(self) -> (i64, i64) {
        match self {
            Unit::Day { days } => {
                let start = day_start(days);
                (start, start + DAY_MS - 1)
            }
            Unit::Hour { days, hh, mm } => {
                let start = day_start(days) + hh * HOUR_MS + mm * 60_000;
                (start, start + HOUR_MS - 1)
            }
            Unit::Month { year, month } => {
                let start = day_start(days_from_civil(year, month, 1));
                let (next_year, next_month) =
                    if month == 12 { (year + 1, 1) } else { (year, month + 1) };
                (start, day_start(days_from_civil(next_year, next_month, 1)) - 1)
            }
        }
    }
}

/// `N天后|N天前|N天之后|N天之前|N小时后|N小时前`（整句匹配）。
///
/// 日偏移落在**本地日**上（`N天后` = 第 N 个本地日整天）；小时偏移直接落在**轮时刻**上
/// （`turn_ms ± N 小时`）——短语说的是"两小时后"，不是"两小时后那个整点"，所以不取整。
fn parse_offset(text: &str, turn_ms: i64) -> Option<(i64, i64, &'static str)> {
    let mut split = None;
    for (suffix, forward) in [
        ("之后", true),
        ("之前", false),
        ("以后", true),
        ("以前", false),
        ("后", true),
        ("前", false),
    ] {
        if let Some(body) = text.strip_suffix(suffix) {
            split = Some((body, forward));
            break;
        }
    }
    let (body, forward) = split?;
    let (digits, precision) = if let Some(digits) = body.strip_suffix("小时") {
        (digits, "hour")
    } else if let Some(digits) = body.strip_suffix('天') {
        (digits, "day")
    } else {
        let digits = body.strip_suffix('日')?;
        (digits, "day")
    };
    let count = parse_digits(digits)?;
    let signed = if forward { count } else { -count };
    if precision == "day" {
        let start = day_start(local_day(turn_ms) + signed);
        Some((start, start + DAY_MS - 1, "day"))
    } else {
        let start = turn_ms + signed * HOUR_MS;
        Some((start, start + HOUR_MS - 1, "hour"))
    }
}

/// 带偏移/秒的 ISO 时刻：`YYYY-MM-DDTHH:MM[:SS][Z|±HH:MM|±HHMM]`。
///
/// 命中条件是**文本自带时区偏移或秒**——那是"某一刻"而不是"某一小时"，所以结果是精确毫秒
/// （`2026-10-27T08:04+08:00` ⇒ 2026-10-27T00:04Z）。无偏移无秒的 `2026-11-05T18:30`
/// 仍走 [`parse_iso`] 的整点桶（那是人话里的"傍晚六点半"，不是机读时刻）。
///
/// 偏移写在文本里 ⇒ 换算只按文本，不再叠一次本地时区：`naive_local - offset`。
fn parse_iso_instant(text: &str) -> Option<i64> {
    if text.len() < 10 { return None; }
    let year = parse_digits(text.get(0..4)?)?;
    if text.get(4..5)? != "-" || text.get(7..8)? != "-" { return None; }
    let month = u32::try_from(parse_digits(text.get(5..7)?)?).ok()?;
    let day = u32::try_from(parse_digits(text.get(8..10)?)?).ok()?;
    if !valid_date(year, month, day) { return None; }
    let after_date = text.get(10..)?;
    let rest = after_date
        .strip_prefix('T')
        .or_else(|| after_date.strip_prefix('t'))
        .or_else(|| after_date.strip_prefix(' '))?;
    let hh = parse_digits(rest.get(0..2)?)?;
    if rest.get(2..3)? != ":" { return None; }
    let mm = parse_digits(rest.get(3..5)?)?;
    if !(0..24).contains(&hh) || !(0..60).contains(&mm) { return None; }
    let mut tail = rest.get(5..)?;
    let mut seconds = 0i64;
    if let Some(after_colon) = tail.strip_prefix(':') {
        seconds = parse_digits(after_colon.get(0..2)?)?;
        if !(0..60).contains(&seconds) { return None; }
        tail = after_colon.get(2..)?;
    }
    // 无偏移、无秒 ⇒ 不是"时刻字面量"，交回整点桶那条路。
    if tail.is_empty() && seconds == 0 { return None; }
    let offset_min = if tail.is_empty() { TZ_MS / 60_000 } else { parse_offset_suffix(tail)? };
    let days = days_from_civil(year, month, day);
    // `day_start` 已含 +08:00；先还原成"本地墙钟毫秒"，再按文本偏移换算成 UTC 毫秒。
    let naive_local = day_start(days) + TZ_MS + hh * HOUR_MS + mm * 60_000 + seconds * 1000;
    Some(naive_local - offset_min * 60_000)
}

/// 时区后缀 ⇒ 东偏移分钟数。`Z` = 0、`+08:00` = 480、`-0500` = -300。
fn parse_offset_suffix(tail: &str) -> Option<i64> {
    if tail == "Z" || tail == "z" { return Some(0); }
    let (sign, body) = match tail.strip_prefix('+') {
        Some(body) => (1, body),
        None => (-1, tail.strip_prefix('-')?),
    };
    let (hh, mm) = match body.split_once(':') {
        Some((hh, mm)) => (parse_digits(hh)?, parse_digits(mm)?),
        None => {
            if body.len() != 4 { return None; }
            (parse_digits(body.get(0..2)?)?, parse_digits(body.get(2..4)?)?)
        }
    };
    if hh > 14 || mm >= 60 { return None; }
    Some(sign * (hh * 60 + mm))
}

/// 钟点短语（`2026-10-28` §2 C2 的裸 `HH:MM[:SS]` 与 `下周X上午十点`）⇒ `(精确毫秒, 是否带日期)`。
///
/// 三种写法，都是"某一刻"：
/// - `今天|明日… 20:00`：日词给日期；
/// - `下周一|本周五 上午十点`：周词给日期；
/// - `20:00` / `十点`：日期取轮所在本地日（**不猜跨天**：19:36 说的 `20:00` 是当日，
///   16:49 说的 `13:07` 也是当日——那是过去时刻，不是"明天"）。
///
/// `allow_trailing` 只给带方向词的短语开（`20:00 额度重置后`）：钟点仍取开头那个，
/// 后面的动词短语不当时间读，只借它的"之后"。
fn parse_clock_phrase(text: &str, turn_day: i64, allow_trailing: bool) -> Option<(i64, bool)> {
    if let Some((days, rest)) = split_day_word(text, turn_day)
        .or_else(|| split_weekday_prefix(text, turn_day))
    {
        let (hh, mm, ss) = parse_clock_whole(rest)?;
        return Some((day_start(days) + hh * HOUR_MS + mm * 60_000 + ss * 1_000, true));
    }
    let (hh, mm, ss, consumed) = parse_clock_prefix(text)?;
    if consumed != text.len() && !allow_trailing { return None; }
    Some((day_start(turn_day) + hh * HOUR_MS + mm * 60_000 + ss * 1_000, false))
}

/// `上午十点` / `下午 3:30`：时段前缀 + 整句钟面。第三格是秒（`13:55:30`）。
fn parse_clock_whole(text: &str) -> Option<(i64, i64, i64)> {
    let (period, rest) = split_period(text);
    let (hh, mm, ss, consumed) = parse_clock_prefix(rest)?;
    if consumed != rest.len() { return None; }
    Some((apply_period(period, hh), mm, ss))
}

const PERIODS: [&str; 8] = ["上午", "下午", "晚上", "中午", "早上", "凌晨", "傍晚", "夜里"];

fn split_period(text: &str) -> (Option<&'static str>, &str) {
    for period in PERIODS {
        if let Some(rest) = text.strip_prefix(period) {
            return (Some(period), rest);
        }
    }
    (None, text)
}

/// 时段对小时数的修正：`下午/晚上/傍晚/夜里/中午` 的 1..11 点 ⇒ +12。`上午/早上/凌晨` 不动。
fn apply_period(period: Option<&str>, hh: i64) -> i64 {
    match period {
        Some("下午" | "晚上" | "傍晚" | "夜里" | "中午") if hh < 12 => hh + 12,
        _ => hh,
    }
}

/// 钟面前缀：`HH:MM[:SS]` 或 `H点[半|MM分]`。返回 `(时, 分, 秒, 消耗字节数)`。
///
/// 冒号形态收三节：`HH:MM` 的消耗字节数是 5、`HH:MM:SS` 是 8（T33：现网 `08:59:42` 这类
/// 带秒的钟面曾因只认两节而整条解析不出）。秒必须**正好两位**且 `0..60`——缺位（`13:5`）、
/// 越界（`25:00`、`13:55:60`）一律 `None`，不截断、不补零、不猜。
fn parse_clock_prefix(text: &str) -> Option<(i64, i64, i64, usize)> {
    let bytes = text.as_bytes();
    let mut digits = 0usize;
    while digits < bytes.len() && digits < 2 && bytes[digits].is_ascii_digit() {
        digits += 1;
    }
    if digits > 0 && bytes.get(digits) == Some(&b':') {
        let hh = text.get(..digits)?.parse::<i64>().ok()?;
        let minute_start = digits + 1;
        let minute_end = minute_start + 2;
        let mm = text.get(minute_start..minute_end)?.parse::<i64>().ok()?;
        if hh >= 24 || mm >= 60 { return None; }
        let mut consumed = minute_end;
        let mut ss = 0i64;
        if bytes.get(consumed) == Some(&b':') {
            let second_start = consumed + 1;
            let second_end = second_start + 2;
            ss = text.get(second_start..second_end)?.parse::<i64>().ok()?;
            if ss >= 60 { return None; }
            consumed = second_end;
        }
        return Some((hh, mm, ss, consumed));
    }
    let (hh, rest) = if digits > 0 {
        (text.get(..digits)?.parse::<i64>().ok()?, text.get(digits..)?)
    } else {
        parse_cn_number(text)?
    };
    let rest = rest.strip_prefix('点')?;
    if hh >= 24 { return None; }
    if let Some(after) = rest.strip_prefix('半') {
        return Some((hh, 30, 0, text.len() - after.len()));
    }
    let minute_digits = rest.bytes().take(2).take_while(|b| b.is_ascii_digit()).count();
    if minute_digits > 0 {
        let mm = rest.get(..minute_digits)?.parse::<i64>().ok()?;
        if mm >= 60 { return None; }
        let after_digits = rest.get(minute_digits..)?;
        let after = after_digits
            .strip_prefix("分钟")
            .or_else(|| after_digits.strip_prefix('分'))
            .unwrap_or(after_digits);
        return Some((hh, mm, 0, text.len() - after.len()));
    }
    Some((hh, 0, 0, text.len() - rest.len()))
}

/// 中文数字（1..=24 够用）：`十`、`十一`、`二十`、`两`。返回 `(值, 剩余)`。
fn parse_cn_number(text: &str) -> Option<(i64, &str)> {
    let first = text.chars().next()?;
    if first == '十' {
        let rest = &text[first.len_utf8()..];
        return match take_cn_digit(rest).filter(|(d, _)| (1..=9).contains(d)) {
            Some((d, tail)) => Some((10 + d, tail)),
            None => Some((10, rest)),
        };
    }
    let a = cn_digit(first)?;
    let rest = &text[first.len_utf8()..];
    let Some(after) = rest.strip_prefix('十') else { return Some((a, rest)); };
    match take_cn_digit(after).filter(|(d, _)| (1..=9).contains(d)) {
        Some((b, tail)) => Some((a * 10 + b, tail)),
        None => Some((a * 10, after)),
    }
}

fn take_cn_digit(text: &str) -> Option<(i64, &str)> {
    let c = text.chars().next()?;
    let digit = cn_digit(c)?;
    Some((digit, &text[c.len_utf8()..]))
}

fn cn_digit(c: char) -> Option<i64> {
    Some(match c {
        '零' => 0,
        '一' => 1,
        '二' | '两' => 2,
        '三' => 3,
        '四' => 4,
        '五' => 5,
        '六' => 6,
        '七' => 7,
        '八' => 8,
        '九' => 9,
        _ => return None,
    })
}

/// 日词前缀（`今天/明天/昨天…`）⇒ `(目标本地日, 剩余)`。
fn split_day_word(text: &str, turn_day: i64) -> Option<(i64, &str)> {
    for (word, shift) in [
        ("今天", 0),
        ("今日", 0),
        ("明天", 1),
        ("明日", 1),
        ("昨天", -1),
        ("昨日", -1),
        ("前天", -2),
        ("后天", 2),
    ] {
        if let Some(rest) = text.strip_prefix(word) {
            return Some((turn_day + shift, rest));
        }
    }
    None
}

/// `YYYY-MM-DD`，可选 ` HH:MM` 或 `T18:30`（时分 ⇒ 精度 hour）。
/// 非法日期（`2026-02-30`、`2026-13-01`、`24:00`）一律 `None`，不夹到月末、不借位。
fn parse_iso(text: &str) -> Option<Unit> {
    if text.len() < 10 { return None; }
    let year = parse_digits(text.get(0..4)?)?;
    if text.get(4..5)? != "-" || text.get(7..8)? != "-" { return None; }
    let month = u32::try_from(parse_digits(text.get(5..7)?)?).ok()?;
    let day = u32::try_from(parse_digits(text.get(8..10)?)?).ok()?;
    if !valid_date(year, month, day) { return None; }
    let days = days_from_civil(year, month, day);

    let rest = text.get(10..)?;
    if rest.is_empty() { return Some(Unit::Day { days }); }
    // 分隔符可省：`parse_time_text` 在"去空白"的紧凑形上解析，`2026-11-05 18:30`
    // 到这里已经是 `2026-11-0518:30`；带空格/`T` 的原形也照收。
    let rest = rest
        .strip_prefix(' ')
        .or_else(|| rest.strip_prefix('T'))
        .or_else(|| rest.strip_prefix('t'))
        .unwrap_or(rest);
    if rest.len() != 5 { return None; }
    let hh = parse_digits(rest.get(0..2)?)?;
    if rest.get(2..3)? != ":" { return None; }
    let mm = parse_digits(rest.get(3..5)?)?;
    if !(0..24).contains(&hh) || !(0..60).contains(&mm) { return None; }
    Some(Unit::Hour { days, hh, mm })
}

/// `YYYY年M月D日`（年份取文本）与 `M月D日`（年份取 turn 的**本地年**，不判断"已经过了就明年"：
/// 那是猜测，闭集里没有）。
fn parse_cn_date(text: &str, turn_day: i64) -> Option<Unit> {
    let (year, rest) = match text.split_once('年') {
        Some((year, rest)) => (parse_digits(year)?, rest),
        None => (civil_from_days(turn_day).0, text),
    };
    let (month, rest) = rest.split_once('月')?;
    let day = rest.strip_suffix('日')?;
    let month = u32::try_from(parse_digits(month)?).ok()?;
    let day = u32::try_from(parse_digits(day)?).ok()?;
    if !valid_date(year, month, day) { return None; }
    Some(Unit::Day { days: days_from_civil(year, month, day) })
}

/// 周（周一为一周之首，按本地日历）：`上周X|这周X|本周X|下周X|星期X|周X`。
/// 不带周前缀的 `周X|星期X` 读作**本周**的 X（不猜"下一个周X"）。
fn parse_weekday(text: &str, turn_day: i64) -> Option<(Unit, &'static str)> {
    let (days, rest) = split_weekday_prefix(text, turn_day)?;
    if !rest.is_empty() { return None; }
    Some((Unit::Day { days }, "inferred"))
}

/// 周词前缀 ⇒ `(目标本地日, 剩余)`。剩余留给 `上午十点` 这种"日 + 时刻"的组合。
fn split_weekday_prefix(text: &str, turn_day: i64) -> Option<(i64, &str)> {
    let (weeks, rest) = if let Some(rest) = text.strip_prefix("上星期") {
        (-1, rest)
    } else if let Some(rest) = text.strip_prefix("这星期") {
        (0, rest)
    } else if let Some(rest) = text.strip_prefix("下星期") {
        (1, rest)
    } else if let Some(rest) = text.strip_prefix("星期") {
        (0, rest)
    } else if let Some(rest) = text.strip_prefix("上周") {
        (-1, rest)
    } else if let Some(rest) = text.strip_prefix("这周") {
        (0, rest)
    } else if let Some(rest) = text.strip_prefix("本周") {
        (0, rest)
    } else if let Some(rest) = text.strip_prefix("下周") {
        (1, rest)
    } else {
        let rest = text.strip_prefix('周')?;
        (0, rest)
    };
    let head = rest.chars().next()?;
    let weekday: i64 = match head {
        '一' => 1,
        '二' => 2,
        '三' => 3,
        '四' => 4,
        '五' => 5,
        '六' => 6,
        '日' | '天' => 7,
        _ => return None,
    };
    let this_monday = turn_day - (weekday_of(turn_day) - 1);
    Some((this_monday + weeks * 7 + (weekday - 1), &rest[head.len_utf8()..]))
}

/// `今天|今日|昨天|昨日|前天|明天|明日|后天`、`昨晚|今晚|明晚`（本地 20:00）、
/// `本月|这个月|下个月|下月`。
fn parse_keyword(text: &str, turn_day: i64) -> Option<(Unit, &'static str)> {
    let day_shift = match text {
        "今天" | "今日" => Some(0),
        "昨天" | "昨日" => Some(-1),
        "前天" => Some(-2),
        "明天" | "明日" => Some(1),
        "后天" => Some(2),
        _ => None,
    };
    if let Some(shift) = day_shift {
        let basis = if shift == 0 { "turn_ts" } else { "inferred" };
        return Some((Unit::Day { days: turn_day + shift }, basis));
    }
    let evening_shift = match text {
        "今晚" => Some(0),
        "昨晚" => Some(-1),
        "明晚" => Some(1),
        _ => None,
    };
    if let Some(shift) = evening_shift {
        let basis = if shift == 0 { "turn_ts" } else { "inferred" };
        let unit = Unit::Hour { days: turn_day + shift, hh: EVENING_HOUR, mm: 0 };
        return Some((unit, basis));
    }
    let (year, month, _) = civil_from_days(turn_day);
    match text {
        "本月" | "这个月" => Some((Unit::Month { year, month }, "turn_ts")),
        "下个月" | "下月" => {
            let (year, month) =
                if month == 12 { (year + 1, 1) } else { (year, month + 1) };
            Some((Unit::Month { year, month }, "inferred"))
        }
        _ => None,
    }
}

// ── 日历算术（不引 chrono/time；Howard Hinnant 的 civil 算法）─────────────

/// 纯 ASCII 数字，1..=4 位。全角数字、负号、空白、空串都不收 ⇒ 写法在闭集外就落 unknown。
fn parse_digits(text: &str) -> Option<i64> {
    if text.is_empty() || text.len() > 4 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<i64>().ok()
}

/// 本地日序号（1970-01-01 起的天数）：UTC 毫秒先加时区偏移，再向负无穷取整。
fn local_day(utc_ms: i64) -> i64 {
    (utc_ms + TZ_MS).div_euclid(DAY_MS)
}

/// 某个本地日的 00:00 对应的 UTC 毫秒。
fn day_start(days: i64) -> i64 {
    days * DAY_MS - TZ_MS
}

/// 本地日的周几：周一 = 1 .. 周日 = 7（1970-01-01 是周四 ⇒ 偏移 3）。
fn weekday_of(days: i64) -> i64 {
    (days + 3).rem_euclid(7) + 1
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => if is_leap(year) { 29 } else { 28 },
        _ => 0,
    }
}

fn valid_date(year: i64, month: u32, day: u32) -> bool {
    (1..=12).contains(&month) && day >= 1 && day <= days_in_month(year, month)
}

/// 公历 (y, m, d) ⇒ 1970-01-01 起的天数（m/d 调用方已校验）。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 1970-01-01 起的天数 ⇒ 公历 (y, m, d)。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ── 回填 ─────────────────────────────────────────────────────────────

/// `time_expr.kind` → 解析用的「事实类别」。它只喂 [`time_kind_for`] 的默认分支，所以映射只要
/// 落在同一个分支里：`deadline` 读作承诺类（否则再解析会丢掉 deadline 的意图），`event_time`
/// 读作事件类，其余读作残值（默认 validity）。
fn category_from_kind(kind: &str) -> &'static str {
    match kind {
        "deadline" => "promise",
        "event_time" => "event",
        _ => "other",
    }
}

/// 回填 `fact` 的三条列组（设计 §2.4 下游）。只改**还空着**的列 ⇒ 重跑与"先写者赢"都成立。
const DEADLINE_BACKFILL: &str = "UPDATE fact SET due_at_ms=?2
     WHERE evidence_turn_uid=?1 AND due_at_ms IS NULL
       AND category IN ('promise','plan','decision')";
const EVENT_TIME_BACKFILL: &str = "UPDATE fact SET occurred_at_ms=?2, occurred_end_ms=?3
     WHERE evidence_turn_uid=?1 AND occurred_at_ms IS NULL
       AND category IN ('event','state_change','observation')";
const VALIDITY_BACKFILL: &str = "UPDATE fact SET valid_from_ms=?2, valid_to_ms=?3
     WHERE evidence_turn_uid=?1 AND valid_from_ms IS NULL AND valid_to_ms IS NULL";

/// 卡面时间的三条回填（T33）。`card.time_json` 在写卡那一刻是 NULL（那时还没有解析结果，
/// 见 `rc_memory::writers_fact::CARD_INSERT`），这里按**刚刚回填的那一组 fact 列**把它补上：
/// - 选卡：最小消费卡的 `pointers_json` 唯一形状就是 `["fact:<id>"]`（内容寻址、无第二形态）；
/// - 选行：`fact.evidence_turn_uid=?1` 且**该列已等于本次写入的值**（`?3`/`?4`）——与上面三条
///   `UPDATE fact` 的 WHERE 同口径，所以"补了哪几行 fact"就是"给哪几张卡写时间"；
/// - 只写 `time_json IS NULL`：先到先得，重跑不覆盖（幂等），也不动别条 time 表达写过的卡。
const CARD_EVENT_TIME_BACKFILL: &str = "UPDATE card SET time_json=?2
     WHERE time_json IS NULL
       AND pointers_json IN (
         SELECT '[\"fact:' || id || '\"]' FROM fact
          WHERE evidence_turn_uid=?1 AND occurred_at_ms=?3
            AND category IN ('event','state_change','observation'))";
const CARD_DEADLINE_BACKFILL: &str = "UPDATE card SET time_json=?2
     WHERE time_json IS NULL
       AND pointers_json IN (
         SELECT '[\"fact:' || id || '\"]' FROM fact
          WHERE evidence_turn_uid=?1 AND due_at_ms=?3
            AND category IN ('promise','plan','decision'))";
const CARD_VALIDITY_BACKFILL: &str = "UPDATE card SET time_json=?2
     WHERE time_json IS NULL
       AND pointers_json IN (
         SELECT '[\"fact:' || id || '\"]' FROM fact
          WHERE evidence_turn_uid=?1 AND valid_from_ms=?3 AND valid_to_ms IS ?4)";

/// 卡面时间的 JSON 形状（`card.time_json` 的**唯一**生产者）。
///
/// 五格与 `time_expr` 的解析结果逐格对齐（`start_ms/end_ms/precision/basis/kind`），另加一格
/// `field` 说明它镜像的是 `fact` 的哪一组列：`occurred`（`occurred_at_ms/occurred_end_ms`）、
/// `due`（`due_at_ms`）、`valid`（`valid_from_ms/valid_to_ms`）。两端都可能单独缺（`20:00 前`
/// 只有右端、`20:00 一过` 只有左端、周期两端都空），所以 `start_ms/end_ms` 允许 null。
fn card_time_json(field: &str, r: &TimeResolution) -> String {
    json!({
        "kind": r.kind,
        "field": field,
        "start_ms": r.start_ms,
        "end_ms": r.end_ms,
        "precision": r.precision,
        "basis": r.basis,
    })
    .to_string()
}

/// 扫所有未解析的时间行（`basis='none'`，按 `id` 升序），把**能解析**的收进列表；解析不出的
/// 行跳过（下一轮仍会扫到，不写"假装解析"）。
///
/// **只读**：不改任何一行。`now_ms` 是形参位（与其余 pass 入口同形），本层**不使用**：每一行的
/// 参照钟是它自己的 `created_at_ms`（源轮时刻），不是"这次运行的钟"——用运行钟会让同一句
/// "下周三"在每次跑批时落到不同的周。把时钟位留着不用，是为了让"本层没有第二个时钟来源"
/// 在读代码时一眼可见。
pub fn parse_pending_time_expr(
    m: &MemoryHandle,
    _now_ms: i64,
) -> Result<Vec<TimeResolution>, MemError> {
    // 先读后写（与旧实现同一个理由）：一次把候选行取到内存，不在游标上开写。
    let pending: Vec<(String, String, String, String, i64)> = {
        let mut st = m.conn().prepare(
            "SELECT id, turn_uid, text, kind, created_at_ms FROM time_expr
             WHERE basis='none' ORDER BY id",
        )?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
                r.get::<_, String>(3)?, r.get::<_, i64>(4)?))
        })?;
        rows.collect::<Result<_, _>>()?
    };

    let mut out = Vec::with_capacity(pending.len());
    for (time_expr_id, turn_uid, text, kind, created_at_ms) in pending {
        let parsed = parse_time_text(&text, created_at_ms, category_from_kind(&kind));
        // `basis='none'` 才是"没解析出"。周期（`每周`）与开口区间（`20:00 前` 只有右端）
        // 都算解析成功：它们的 `start/end` 本来就可以空着。
        if parsed.basis == "none" {
            continue;
        }
        out.push(TimeResolution {
            time_expr_id,
            turn_uid,
            text,
            start_ms: parsed.start_ms,
            end_ms: parsed.end_ms,
            precision: parsed.precision.to_string(),
            basis: parsed.basis.to_string(),
            kind: parsed.kind.to_string(),
        });
    }
    Ok(out)
}

/// 应用一批解析结果：单事务；每行 CAS 更新（`WHERE id=? AND basis='none'`，别人已写过就不覆盖），
/// **只有该 UPDATE 真改了 1 行**才按 kind 回填 `fact`（复用三条 SQL 常量与
/// [`time_kind_for`] 的语义），同一事务里再按同一组列把 `card.time_json` 补上（T33）。
///
/// 读数：`expressions` = 传入条数，`resolved` = CAS 成功条数，`facts` = 回填影响行数之和
/// （**不含**卡行：卡是派生面，它的写入不进"回填了几条事实"这个读数）。
pub fn apply_time_resolutions(
    m: &MemoryHandle,
    resolutions: &[TimeResolution],
    _now_ms: i64,
) -> Result<TimeResolveReport, MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }

    let mut report = TimeResolveReport {
        expressions: resolutions.len(),
        resolved: 0,
        facts: 0,
    };
    let tx = m.conn().unchecked_transaction()?;
    for r in resolutions {
        // 两端都空只对周期成立（`每周`/`每天`）：那也要落库，否则它永远停在 `basis='none'`。
        if r.start_ms.is_none() && r.end_ms.is_none() && r.kind != "recurrence" {
            // `parse_pending_time_expr` 不会产出这种行；防御性跳过，不把 None 当 0。
            continue;
        }
        let changed = tx.execute(
            "UPDATE time_expr SET start_ms=?2, end_ms=?3, precision=?4, basis=?5, kind=?6
             WHERE id=?1 AND basis='none'",
            params![r.time_expr_id, r.start_ms, r.end_ms, r.precision, r.basis, r.kind],
        )?;
        // CAS：别人已经写过（basis 不再是 none）⇒ 这一行不算本层 resolved，也不回填。
        if changed != 1 {
            continue;
        }
        report.resolved += 1;
        report.facts += match r.kind.as_str() {
            // 截止：优先右端（`20:00 前` 只有右端；`下周三之前` 两端都有，右端是当日末）。
            "deadline" => match (r.start_ms, r.end_ms) {
                (_, Some(end)) => {
                    let facts = tx.execute(DEADLINE_BACKFILL, params![r.turn_uid, end])?;
                    tx.execute(
                        CARD_DEADLINE_BACKFILL,
                        params![r.turn_uid, card_time_json("due", r), end],
                    )?;
                    facts
                }
                (Some(start), None) => {
                    let facts = tx.execute(DEADLINE_BACKFILL, params![r.turn_uid, start])?;
                    tx.execute(
                        CARD_DEADLINE_BACKFILL,
                        params![r.turn_uid, card_time_json("due", r), start],
                    )?;
                    facts
                }
                (None, None) => 0,
            },
            "event_time" => match r.start_ms {
                Some(start) => {
                    let facts =
                        tx.execute(EVENT_TIME_BACKFILL, params![r.turn_uid, start, r.end_ms])?;
                    tx.execute(
                        CARD_EVENT_TIME_BACKFILL,
                        params![r.turn_uid, card_time_json("occurred", r), start],
                    )?;
                    facts
                }
                None => 0,
            },
            "validity" => match r.start_ms {
                Some(start) => {
                    let facts =
                        tx.execute(VALIDITY_BACKFILL, params![r.turn_uid, start, r.end_ms])?;
                    tx.execute(
                        CARD_VALIDITY_BACKFILL,
                        params![r.turn_uid, card_time_json("valid", r), start, r.end_ms],
                    )?;
                    facts
                }
                None => 0,
            },
            // `speech_time`/`recurrence` 只更新时间行本身，不回填任何 fact 列——不知道语义
            // 就不动别人的列（`现在` 已由 `fact.created_at_ms` 表达，写一遍是重复）。
            _ => 0,
        };
    }
    tx.commit()?;
    Ok(report)
}

/// 旧 API（S2d-1）：`parse + apply` 的薄封装。读数口径与旧实现一致——`expressions` 仍是
/// **扫到的** `basis='none'` 行数（含解析不出的），所以现有调用方与测试不用改。
pub fn resolve_pending_time_expr(
    m: &MemoryHandle,
    now_ms: i64,
) -> Result<TimeResolveReport, MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    let scanned: i64 = m.conn().query_row(
        "SELECT count(*) FROM time_expr WHERE basis='none'",
        [],
        |r| r.get(0),
    )?;
    let resolutions = parse_pending_time_expr(m, now_ms)?;
    let mut report = apply_time_resolutions(m, &resolutions, now_ms)?;
    report.expressions = usize::try_from(scanned).unwrap_or(usize::MAX);
    Ok(report)
}

// ── 选批与 runner（S2d-2）─────────────────────────────────────────────

/// 指纹里的 pipeline 段（设计 §3 的 `prompt=<pass_id>@v0.1`）。time pass 没有 prompt：
/// model 段写死 `"program"`，两个 prompt 段是空串——指纹只随这两个常量变。
///
/// **C2 起 `v0.2`**：闭集扩容是解析器版本变化，指纹必须跟着变——`stale(unresolved)` 的轮
/// 只有指纹与当前版本不同才会被重新选中（见 [`reselectable`]），这是"解析器升级后回填"的唯一开关。
///
/// **T33 起 `v0.3`**：`parse_clock_prefix` 收 `HH:MM:SS`（秒级钟面）。不 bump 的话，现网两条
/// `stale(unresolved)` 的 `08:59:42` 永远不会被重选——解析器改了、结论会变，但选批看不见。
/// 指纹 `= sha256("rc.extract_fingerprint.v1" ⊕ pipeline ⊕ model="program" ⊕ 两个空 prompt 段)`
/// （`rc_ledger::water::extraction_fingerprint`），所以这里改一个字符就换掉整条重选键。
pub const TIME_PIPELINE: &str = "pass-time-v0.3";

/// 状态键的 `pass_id`。time 是单一 pass，`family` 恒空串。
const TIME_PASS: &str = "time";

/// 失败理由（与 naming/fact/event 的写库侧同词）。
const STORE_FAILURE_REASON: &str = "store";

/// `basis='none'` 且本版解析器也解析不出的轮：单独终态（`stale` + 这个词），**不是** Done。
/// 它把"已处理但结论未知"与"已处理且解析成功"分开：前者在解析器升版后可重跑。
pub const UNRESOLVED_REASON: &str = "unresolved";

/// `stale` item 的逐轮终态理由（与 naming 同一词：旧指纹作废，载荷换了）。
const STALE_REASON: &str = "stale";

/// 调用方给 0 时的默认窗宽（与其余三条 pass 同一个数；应用层的缺省在 hub）。
const DEFAULT_LIMIT: usize = 20;

/// 一组（同一 `(session_key, device_id)`）的解析结果。`origin_ms` 与 `resolutions` 等长：
/// 每一项是这一轮在转录里的 `created_at_ms`（查不到 origin 的轮是 `None`）。
#[derive(Debug, Default)]
struct TimeGroup {
    resolutions: Vec<TimeResolution>,
    origin_ms: Vec<Option<i64>>,
}

/// 一组的接线读数。`failed` = 该组选中轮已被落成 rejected/quarantined，run_time 最后
/// **不得**再用 Done 覆盖它们。
#[derive(Debug, Clone, Default)]
struct GroupOutcome {
    applied: bool,
    failed: bool,
    quarantined: bool,
    resolved: usize,
    facts: usize,
    /// I3：程序闸拒收、但本组仍 apply 了其余结论的轮——它们已是 `rejected(<码>)`，
    /// run_time 收尾的 Done 不许覆盖。
    rejected_turns: Vec<String>,
}

/// 会话归属七列的读法（与 naming 的 `select_naming_batch` 同一条 COALESCE 口径）。
struct SessionOwnership {
    source_agent: String,
    owner: OwnerCtx,
    scope: Scope,
}

/// 读一个 `(session_key, device_id)` 的 `source_agent` 与归属。没有会话行时落"推不出"的
/// 保守默认：`owner_agent_id='unknown'`（与 `hub::ownership::unresolved_row` 同一哨兵）、
/// `private_agent`/`default`/`project/<session_key>`。
///
/// 为什么必须给一个非空 owner：查不到 origin 的轮会单独归到 `("","")` 组，载荷若因
/// `owner_agent_id` 为空而被闸门判成 `payload_shape`，就轮不到 `turn_missing` 说明真相。
fn session_ownership(
    t: &TranscriptHandle,
    session_key: &str,
    device_id: &str,
) -> Result<SessionOwnership, DistillError> {
    let row = {
        let mut st = t.conn().prepare(
            "SELECT source_agent, owner_principal_id,
                    COALESCE(owner_agent_id, source_agent) AS owner_agent,
                    COALESCE(visibility, 'private_agent') AS visibility,
                    COALESCE(retention, 'default') AS retention,
                    COALESCE(scope_kind, 'project') AS scope_kind,
                    COALESCE(scope_key, project_hint, session_key) AS scope_key
             FROM sessions WHERE session_key=?1 AND device_id=?2",
        )?;
        let mut rows = st.query(params![session_key, device_id])?;
        match rows.next()? {
            None => None,
            Some(r) => Some((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
            )),
        }
    };
    Ok(match row {
        Some((source_agent, principal, agent, visibility, retention, scope_kind, scope_key)) => {
            SessionOwnership {
                source_agent,
                owner: OwnerCtx {
                    owner_principal_id: principal,
                    owner_agent_id: agent,
                    visibility: owner::parse_visibility(&visibility),
                    retention: owner::normalize_retention(&retention),
                },
                scope: Scope {
                    kind: owner::normalize_scope_kind(&scope_kind),
                    key: scope_key,
                },
            }
        }
        None => SessionOwnership {
            source_agent: String::new(),
            owner: OwnerCtx {
                owner_principal_id: None,
                owner_agent_id: "unknown".to_string(),
                visibility: owner::Visibility::PrivateAgent,
                retention: "default".to_string(),
            },
            scope: Scope { kind: ScopeKind::Project, key: session_key.to_string() },
        },
    })
}

/// 把一组解析结果落成 ledger 的逐轮终态。
fn mark_turns(
    l: &LedgerHandle,
    uids: &[String],
    state: PassState,
    reason: Option<&str>,
    now_ms: i64,
) -> Result<(), DistillError> {
    for uid in uids {
        pass_state::mark_terminal(l, key(uid), state, reason, now_ms)?;
    }
    Ok(())
}

/// 一组的完整接线：构造载荷 → 入队 → 取 item → 闸 → `program_only` 判定 → apply → 判定。
///
/// - 闸 fail ⇒ 记 `program reject`，该组选中轮 `rejected(<code>)`，不 apply（`failed=true`）；
/// - 闸 pass ⇒ `program_only` review 判定 → approved → apply → 成功记 apply
///   （detail 只放 `resolved=..;facts=..`）；失败：选中轮 `rejected(store)`、item 留 approved、
///   返回 Err；
/// - `fresh=false` 分支：`applied` 不重复 apply（轮由 run_time 收尾 Done）、`approved` 只 apply、
///   `rejected`/`quarantined`/`stale` 不写、轮按对应终态。
#[allow(clippy::too_many_arguments)]
fn wire_group(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    l: &LedgerHandle,
    group_key: &(String, String),
    group: &TimeGroup,
    selected: &BTreeSet<String>,
    fingerprint: &str,
    now_ms: i64,
) -> Result<GroupOutcome, DistillError> {
    let turn_uids: BTreeSet<&String> = group.resolutions.iter().map(|r| &r.turn_uid).collect();
    let group_selected: Vec<String> = turn_uids
        .iter()
        .filter(|uid| selected.contains(**uid))
        .map(|uid| (*uid).clone())
        .collect();
    let ownership = session_ownership(t, &group_key.0, &group_key.1)?;
    let rows: Vec<TurnRow> = turn_uids
        .iter()
        .map(|uid| {
            let created = group
                .resolutions
                .iter()
                .zip(group.origin_ms.iter())
                .filter(|(r, _)| &r.turn_uid == *uid)
                .filter_map(|(_, created)| *created)
                .max()
                .unwrap_or(0);
            // time 不消费正文：载荷只用 uid 与源时刻，role/body 是空占位（不编造转录内容）。
            TurnRow {
                turn_uid: (*uid).clone(),
                role: String::new(),
                body: String::new(),
                created_at_ms: created,
                ingested_at_ms: created,
                speaker: None,
                speaker_anchor_id: None,
            }
        })
        .collect();
    let batch = Batch {
        session_key: group_key.0.clone(),
        device_id: group_key.1.clone(),
        source_agent: ownership.source_agent.clone(),
        project_hint: None,
        rows,
    };
    let ctx = BatchCtx {
        source_agent: ownership.source_agent.clone(),
        device_id: group_key.1.clone(),
    };
    let items: Vec<Value> = group
        .resolutions
        .iter()
        .map(|r| {
            json!({
                "time_expr_id": r.time_expr_id,
                "turn_uid": r.turn_uid,
                "text": r.text,
                "start_ms": r.start_ms,
                "end_ms": r.end_ms,
                "precision": r.precision,
                "basis": r.basis,
                "kind": r.kind,
            })
        })
        .collect();
    // `run_id` 用本批指纹：同一载荷重跑命中同一 sha，不产生第二个 item。
    let payload = review::pass_payload(
        TIME_PASS,
        "",
        fingerprint,
        &batch,
        &ctx,
        &ownership.owner,
        &ownership.scope,
        Value::Array(items),
        None,
    );
    let owner_json = review::owner_json(&ownership.owner, &ownership.scope);
    let queued_item = review::new_item(TIME_PASS, "", &batch, owner_json, &payload);
    let queued = enqueue(m, &queued_item, now_ms)?;
    let item = get(m, queued.id)?
        .ok_or_else(|| MemError::Sql("review: enqueued item vanished".to_string()))?;

    // I3：程序闸拒收的结论（draft）——只丢它们，其余照常 apply。
    let mut kept_resolutions: Option<Vec<TimeResolution>> = None;
    let mut rejected_turns: Vec<(String, &'static str)> = Vec::new();

    match queued.state {
        QueueState::PendingReview => {
            // I3：判定单位是 draft（这里的一条 draft = 一条时间结论）。先算逐条闸。
            let report = review::gate_report(m, t, &item, now_ms)?;
            if !review::program_decided(m, queued.id)? {
                if report.batch_code.is_some() || report.all_failed() {
                    // 批级前置坏 / 全部结论坏：没有好结论可保，与今天一致整组 rejected。
                    // 返回 Ok：闸是确定性的，重试只会得到同一个码。
                    let code = report.code.unwrap_or("payload_shape");
                    review::record_program(
                        m,
                        queued.id,
                        QueueVerdict::Reject,
                        code,
                        &item.payload_sha256,
                        now_ms,
                    )?;
                    mark_turns(l, &group_selected, PassState::Rejected, Some(code), now_ms)?;
                    return Ok(GroupOutcome { failed: true, ..GroupOutcome::default() });
                }
                // 混合组：item 记 program pass（好结论还要 apply）。
                review::record_program(
                    m,
                    queued.id,
                    QueueVerdict::Pass,
                    review::PROGRAM_PASS,
                    &item.payload_sha256,
                    now_ms,
                )?;
            }
            // 坏结论只丢它自己：验收只把**通过**的那几条交给 apply，坏结论的轮落
            // rejected(<码>)，交给 run_time 收尾时不再写 Done。
            if report.code.is_some() {
                let failed: BTreeSet<usize> = report
                    .failed()
                    .filter_map(|draft| {
                        draft
                            .reference
                            .strip_prefix('t')
                            .and_then(|digits| digits.parse::<usize>().ok())
                            .and_then(|index| index.checked_sub(1))
                    })
                    .collect();
                rejected_turns = report.rejected_turns();
                kept_resolutions = Some(
                    group
                        .resolutions
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| !failed.contains(index))
                        .map(|(_, resolution)| resolution.clone())
                        .collect(),
                );
                for (uid, code) in &rejected_turns {
                    if group_selected.contains(uid) {
                        pass_state::mark_terminal(
                            l,
                            key(uid),
                            PassState::Rejected,
                            Some(code),
                            now_ms,
                        )?;
                    }
                }
            }
            // S3a 口径：time 没有模型面，程序闸通过即判定通过。补一条 `program_only` 的
            // review 判定把 item 推进 approved（model/prompt 都是空串）。
            if !rc_memory::review::has_stage(m, queued.id, ReviewStage::Review)? {
                review::record_program_only_review(m, queued.id, &item.payload_sha256, now_ms)?;
            }
        }
        QueueState::Approved => {}
        QueueState::Applied => {
            // 已写库：不重复 apply；选中轮由 run_time 最后的 Done 收尾。
            return Ok(GroupOutcome::default());
        }
        QueueState::Rejected => {
            // 终态：不写库，轮按既有映射收尾（程序码优先，其次审核驳回码）。
            let code = item
                .meta
                .program_code
                .clone()
                .unwrap_or_else(|| review::REVIEW_REJECT.to_string());
            mark_turns(l, &group_selected, PassState::Rejected, Some(&code), now_ms)?;
            return Ok(GroupOutcome { failed: true, ..GroupOutcome::default() });
        }
        QueueState::Quarantined => {
            mark_turns(
                l,
                &group_selected,
                PassState::Quarantined,
                Some(review::REVIEW_UNCERTAIN),
                now_ms,
            )?;
            return Ok(GroupOutcome { failed: true, quarantined: true, ..GroupOutcome::default() });
        }
        QueueState::Stale => {
            mark_turns(l, &group_selected, PassState::Rejected, Some(STALE_REASON), now_ms)?;
            return Ok(GroupOutcome { failed: true, ..GroupOutcome::default() });
        }
    }

    // 只有 approved（或续跑到 approved）才 apply。I3：只落**通过**的结论。
    let resolutions = kept_resolutions.as_deref().unwrap_or(&group.resolutions);
    let report = match apply_time_resolutions(m, resolutions, now_ms) {
        Ok(report) => report,
        Err(e) => {
            // 写库失败：item 留在 approved（不记 apply），选中轮落 store。
            mark_turns(l, &group_selected, PassState::Rejected, Some(STORE_FAILURE_REASON), now_ms)?;
            return Err(DistillError::Memory(e));
        }
    };
    // 写成功才记 apply 判定（item approved → applied）；detail 只放计数。
    let detail = format!("resolved={};facts={}", report.resolved, report.facts);
    review::record_apply(m, queued.id, &detail, "", "", &item.payload_sha256, now_ms)?;
    Ok(GroupOutcome {
        applied: true,
        failed: false,
        quarantined: false,
        resolved: report.resolved,
        facts: report.facts,
        rejected_turns: rejected_turns.into_iter().map(|(uid, _)| uid).collect(),
    })
}

/// 跑一批 time：选批 → 选中轮落 `running` → 解析 → **按会话分组入待审队列** → 闸 pass 才
/// apply → 每组按终态收尾。
///
/// 三步的边界：
/// 1. 空批 ⇒ `turns=0, state=Empty`（不写状态行、不扫描）；
/// 2. 选中轮先 `mark_running`（同一指纹、同一 run_id），再**只读**解析全库未解析行：
///    - 解析列表为空 ⇒ 没有可入队的结论（不建 item）；
///    - 非空 ⇒ 按 `(session_key, device_id)` 分组（origin 查不到归 `("","")`，让闸门的
///      `turn_missing` 拒绝，**不静默丢**），每组一个 item：入队 → 程序闸 → `program_only`
///      判定 → approved → apply；
/// 3. 全部组处理完：未被失败组覆盖的选中轮按"这一轮还有没有 `basis='none'`"收尾——
///    没有 ⇒ `done`；还有 ⇒ `stale(reason=unresolved)`。被失败组覆盖的轮保持 rejected/
///    quarantined，Done 不覆盖它们。**解析不出不等于处理完了**：它只是"本版解析器试过"。
///
/// 读数与旧口径对齐：`expressions` 是这一批开始时扫到的全部 `basis='none'` 行（含解析不出的），
/// `resolved`/`facts` 只累加各 apply 成功组（CAS 真改的行）。
///
/// H7c2：收尾（成功或失败）best-effort 写一次 `pass_id='time'` 的审查轴水位——time 不调
/// 模型，但它的整批是一个 pass，接管后同样要回答"审查差多少条"。写失败不改变本次读数。
pub fn run_time(
    m: &MemoryHandle,
    l: &LedgerHandle,
    t: &TranscriptHandle,
    limit: usize,
    now_ms: i64,
) -> Result<TimeOutcome, DistillError> {
    let out = run_time_inner(m, l, t, limit, now_ms);
    let _ = crate::review::run::write_watermark(m, l, TIME_PASS, now_ms);
    out
}

/// [`run_time`] 的主体（三步边界见上）。
fn run_time_inner(
    m: &MemoryHandle,
    l: &LedgerHandle,
    t: &TranscriptHandle,
    limit: usize,
    now_ms: i64,
) -> Result<TimeOutcome, DistillError> {
    let uids = select_time_turns(m, l, limit, now_ms)?;
    if uids.is_empty() {
        return Ok(TimeOutcome {
            turns: 0,
            expressions: 0,
            resolved: 0,
            facts: 0,
            state: PassState::Empty,
        });
    }

    // 指纹只由程序算：pipeline 版本 + `"program"` + 两个空 prompt 段（time 没有模型面）。
    let fingerprint = water::extraction_fingerprint(TIME_PIPELINE, "program", "", "");
    let run_id = format!("time-{now_ms}");
    for uid in &uids {
        pass_state::mark_running(l, key(uid), &fingerprint, &run_id, now_ms)?;
    }

    // ① 只读解析（全库，不按 limit 切——旧实现同一条扫描面）。
    let resolutions = match parse_pending_time_expr(m, now_ms) {
        Ok(resolutions) => resolutions,
        Err(e) => {
            mark_turns(l, &uids, PassState::Rejected, Some(STORE_FAILURE_REASON), now_ms)?;
            return Err(DistillError::Memory(e));
        }
    };
    if resolutions.is_empty() {
        // 没有任何可解析的结论 ⇒ 不建 item（没有结论可审）。这些轮**不是** Done：
        // 收尾那一步会把仍带 `basis='none'` 的轮落成 `stale(unresolved)`。
        let report = match resolve_pending_time_expr(m, now_ms) {
            Ok(report) => report,
            Err(e) => {
                mark_turns(l, &uids, PassState::Rejected, Some(STORE_FAILURE_REASON), now_ms)?;
                return Err(DistillError::Memory(e));
            }
        };
        let unresolved = unresolved_turns(m)?;
        let mut any_unresolved = false;
        for uid in &uids {
            if unresolved.contains(uid) {
                pass_state::mark_terminal(
                    l,
                    key(uid),
                    PassState::Stale,
                    Some(UNRESOLVED_REASON),
                    now_ms,
                )?;
                any_unresolved = true;
            } else {
                pass_state::mark_terminal(l, key(uid), PassState::Done, None, now_ms)?;
            }
        }
        return Ok(TimeOutcome {
            turns: uids.len(),
            expressions: report.expressions,
            resolved: 0,
            facts: 0,
            state: if any_unresolved { PassState::Stale } else { PassState::Done },
        });
    }
    let scanned: i64 = m.conn().query_row(
        "SELECT count(*) FROM time_expr WHERE basis='none'",
        [],
        |r| r.get(0),
    )?;

    // ② 按 (session_key, device_id) 分组；查不到 origin 的轮归 ("","")，交给闸门 turn_missing。
    let mut groups: BTreeMap<(String, String), TimeGroup> = BTreeMap::new();
    for resolution in resolutions {
        let origin = rc_ledger::transcript::turn_origin(t, &resolution.turn_uid)?;
        let group_key = match &origin {
            Some(o) => (o.session_key.clone(), o.device_id.clone()),
            None => (String::new(), String::new()),
        };
        let entry = groups.entry(group_key).or_default();
        entry.origin_ms.push(origin.map(|o| o.created_at_ms));
        entry.resolutions.push(resolution);
    }

    let selected: BTreeSet<String> = uids.iter().cloned().collect();
    let mut resolved = 0usize;
    let mut facts = 0usize;
    let mut failed_turns: BTreeSet<String> = BTreeSet::new();
    let mut any_applied = false;
    let mut any_rejected = false;
    let mut any_quarantined = false;

    for (group_key, group) in &groups {
        let group_turns: Vec<String> =
            group.resolutions.iter().map(|r| r.turn_uid.clone()).collect();
        let out = wire_group(m, t, l, group_key, group, &selected, &fingerprint, now_ms)?;
        resolved += out.resolved;
        facts += out.facts;
        any_applied |= out.applied;
        if out.failed {
            any_rejected |= !out.quarantined;
            any_quarantined |= out.quarantined;
            failed_turns.extend(group_turns.into_iter().filter(|uid| selected.contains(uid)));
        }
        // I3：程序闸拒收的结论的轮已是 rejected(<码>)，收尾的 Done 不许覆盖。
        failed_turns.extend(out.rejected_turns);
    }

    // ③ 未被失败组覆盖的选中轮：**解析出结论的**落 Done；仍带 `basis='none'` 的落
    //    `stale(unresolved)`——"已处理、结论未知"不是 Done，否则解析器升级后没人记得回填它。
    //    被失败组覆盖的轮保持 rejected/quarantined，Done 不覆盖它们。
    let unresolved = unresolved_turns(m)?;
    let mut any_unresolved = false;
    for uid in &uids {
        if failed_turns.contains(uid) {
            continue;
        }
        if unresolved.contains(uid) {
            pass_state::mark_terminal(
                l,
                key(uid),
                PassState::Stale,
                Some(UNRESOLVED_REASON),
                now_ms,
            )?;
            any_unresolved = true;
        } else {
            pass_state::mark_terminal(l, key(uid), PassState::Done, None, now_ms)?;
        }
    }

    let state = if any_applied {
        PassState::Done
    } else if any_rejected {
        PassState::Rejected
    } else if any_quarantined {
        PassState::Quarantined
    } else if any_unresolved {
        PassState::Stale
    } else {
        PassState::Done
    };
    Ok(TimeOutcome {
        turns: uids.len(),
        expressions: usize::try_from(scanned).unwrap_or(usize::MAX),
        resolved,
        facts,
        state,
    })
}

/// 一批 time pass 的读数。
///
/// `turns` 是选中的轮数；`expressions`/`resolved`/`facts` 是**这一次回填**的读数，注意
/// 回填扫的是**全库**未解析行（见 [`run_time`]）：`limit` 只截选批，不截回填。
/// S3a-3c 起 `resolved`/`facts` 只累加各 apply 成功组（CAS 真改的行）；`expressions` 仍是
/// 这一批开始时扫到的全部 `basis='none'` 行（与旧口径一致，含解析不出的）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeOutcome {
    pub turns: usize,
    pub expressions: usize,
    pub resolved: usize,
    pub facts: usize,
    pub state: PassState,
}

/// 一轮的 time 表态（`extract_pass_state` 里 `pass_id='time'` 那一行）。
struct Stated {
    state: PassState,
    next_retry_at_ms: Option<i64>,
    fingerprint: Option<String>,
}

/// 这一轮今天该不该再跑。与 `pass_event::reselectable` / `pass_fact` 同向：
/// - 无状态行 ⇒ 可；
/// - `stale` ⇒ **仅当指纹与当前解析器版本不同**（或没有指纹）可。`stale(unresolved)` 是本版
///   解析器试过但解析不出的轮：同版重跑只会得到同一个结论，不选；解析器升版（[`TIME_PIPELINE`]
///   变、指纹随之变）才重选——这就是"升级后回填旧 `none`"的路径；
/// - `pending` 且（无到期时间或已到期 `<= now_ms`）⇒ 可；
/// - 其余（`running`/`done`/`empty`/`rejected`/`quarantined`、未到期的 `pending`）⇒ 不可。
fn reselectable(rec: Option<&Stated>, now_ms: i64, parser_fingerprint: &str) -> bool {
    match rec {
        None => true,
        Some(rec) => match rec.state {
            PassState::Stale => rec.fingerprint.as_deref() != Some(parser_fingerprint),
            PassState::Pending => rec.next_retry_at_ms.is_none_or(|due| due <= now_ms),
            _ => false,
        },
    }
}

/// 全库仍带 `basis='none'` 的轮（去重）。收尾用它判"这一轮到底解析出来了没有"。
fn unresolved_turns(m: &MemoryHandle) -> Result<BTreeSet<String>, MemError> {
    let mut st = m.conn().prepare("SELECT DISTINCT turn_uid FROM time_expr WHERE basis='none'")?;
    let rows = st.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<Result<BTreeSet<_>, _>>()?)
}

/// 扫有未解析 `time_expr`（`basis='none'`）且没有 time 终态的轮，返回去重后的 `turn_uid`。
///
/// **为什么两个句柄**：`time_expr` 住 memory 库、`extract_pass_state` 住 ledger 库，是两条
/// 独立的 SQLite 连接，"左连接"只能在这里做而不是一条 SQL 里。派发档写的是
/// `select_time_turns(l, limit, now_ms)`——那一句与当前树冲突（没有记忆句柄就找不到
/// `time_expr`），以树为准，签名补上 `m`。
///
/// `basis='none'` 是**唯一**工作判据：状态行只决定"这一轮还许不许跑"，不决定"有没有活干"。
/// 顺序按 `turn_uid` 字典序（去重后天然稳定，重跑同一批的选批结果不抖）。
pub fn select_time_turns(
    m: &MemoryHandle,
    l: &LedgerHandle,
    limit: usize,
    now_ms: i64,
) -> Result<Vec<String>, DistillError> {
    let limit = if limit == 0 { DEFAULT_LIMIT } else { limit };
    let parser_fingerprint = water::extraction_fingerprint(TIME_PIPELINE, "program", "", "");
    // ① time 这一 pass 的表态（ledger.db）——一次读全，不在候选循环里再查。
    let stated: HashMap<String, Stated> = {
        let mut st = l.conn().prepare(
            "SELECT turn_uid, state, next_retry_at_ms, fingerprint FROM extract_pass_state
             WHERE pass_id=?1 AND family=''",
        )?;
        let got = st.query_map(params![TIME_PASS], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        let mut map = HashMap::new();
        for row in got {
            let (uid, raw, next_retry, fingerprint) = row?;
            // 坏 slug 与 `pass_state::get` 同一条纪律：读不出的状态不是"还没跑"，是数据不一致。
            let state = PassState::parse(&raw)
                .ok_or_else(|| StoreError::Sql(format!("pass_state: unreadable state {raw}")))?;
            map.insert(uid, Stated { state, next_retry_at_ms: next_retry, fingerprint });
        }
        map
    };
    // ② 未解析的时间行（memory.db），去重成轮。
    let unresolved: Vec<String> = {
        let mut st = m.conn().prepare(
            "SELECT DISTINCT turn_uid FROM time_expr WHERE basis='none' ORDER BY turn_uid",
        )?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<_, _>>()?
    };
    Ok(unresolved
        .into_iter()
        .filter(|uid| reselectable(stated.get(uid), now_ms, &parser_fingerprint))
        .take(limit)
        .collect())
}

/// time 的键：`family` 恒空串（它是单一 pass，没有调用变体）。
fn key(turn_uid: &str) -> PassKey<'_> {
    PassKey { turn_uid, pass_id: TIME_PASS, family: "" }
}

