//! S2b-2a：fact 行的写库层（设计 `2026-10-26-s2-prompt-v0` §2.2 下游、§3 幂等、§4 归属）。
//!
//! 一条 fact = **五处行、一个事务**：`fact` + 逐条 `provenance` + subject/object 的
//! `memory_mention` + 最小 `card` + `resource_owner`（fact 与 card 各一行）。任何一步失败
//! ⇒ 整批回滚，不存在"行在、出处不在"或"行在、归属不在"。
//! `time_text` **像时间表达**时（[`is_time_phrase`]）同一事务里再多一条**未解析**的
//! `time_expr`（S2d-1）：写库层只记短语原文，不做日期算术，`start/end/precision/basis`
//! 留给 time pass 回填。`再`/`先`/`下一格` 这类语气词不是时间短语，不进审计表（S2e-5）。
//!
//! 模型面不许写的东西在这一层**结构上写不出来**：
//! - `id` 由 [`compute_fact_id`] 现算（内容寻址）；
//! - `scope_*` / `source_agent` / `device_id` 只从 [`FactWriteItem::scope`] 与 [`BatchCtx`] 来；
//! - `owner/visibility/retention/created_by_agent` 只从 [`OwnerCtx`] 与 `BatchCtx` 来；
//! - `created_at_ms` / `as_of_ms` 一律取**源轮时刻** `source_ms`（导入钟不参与，I3）；
//! - `speaker_anchor_id` 由程序算（C1）：证据里有人类轮 ⇒ 会话主人锚，否则 ⇒ 会话自锚；
//!   身份未知/锚不在库里 ⇒ NULL。payload 里没有这一格，模型递不进来；
//! - `occurred_at_ms/speech_time/valid_*/due_at/status` 全写 NULL，等 time pass 回填；
//!   `card.time_json` 同理（写卡这一刻没有已解析时间，回填时补，见 [`CARD_INSERT`]）；
//! - `confidence` 是程序常量 0.8，`mode` 只收 `stated`（`reported`/`inferred` 留 S5b）。
//!
//! 幂等 = 内容寻址 id + `INSERT ... ON CONFLICT(id) DO NOTHING` + 其余表 `INSERT OR IGNORE`
//! （见设计 §3）。计数只数"真写进去的行"，所以同一批重放返回全 0。
//!
//! 校验发生在**事务之前**：一条坏 item ⇒ 整批 `Err`，库里零行（与 `put_batch` 同一条纪律）。
use crate::identity::{compute_fact_id, compute_mem_id, compute_time_expr_id};
use crate::keys::{AnchorRole, MemKey, MemTable, Scope};
use crate::owner::{self, OwnerCtx};
use crate::writers_x::BatchCtx;
use crate::MemError;
use rc_contract::normalize_body;
use rc_store::MemoryHandle;

/// `fact.category` 的 DDL 七值闭集（`rc-store/src/ddl.rs` 的 CHECK 逐字对拍）。
/// v0 一次只抽三个 family，但写库层认的是**列**的闭集，不认 family——`event/state_change/
/// observation` 在 S2c 由 event pass 复用这一格，`other` 是残值兜底。
pub const FACT_CATEGORIES: [&str; 7] = [
    "event", "promise", "plan", "decision", "observation", "state_change", "other",
];

/// v0 只收 `stated`。`inferred` 必须有 basis、`reported` 必须有转述者，两者都留 S5b。
pub const FACT_MODE_STATED: &str = "stated";

/// `time_expr.kind` 的取法（S2d-1，设计 §2.4）：只看**类别 + 短语里的截止词**，不看解析结果。
/// - 承诺/计划/决定 + `之前|以前|截止` ⇒ `deadline`；
/// - 事件三类 ⇒ `event_time`；
/// - 其余（含 `other`，以及写库层已经拒过的非法类别）⇒ `validity`。
///
/// `pub` 是为了让时间 pass 用**同一把尺子**判"没解析出的短语该落哪个 kind"——两处各写一份的
/// 后果是同一句短语在 `time_expr` 与 `fact` 两侧落成两种类别。
pub fn time_kind_for(category: &str, time_text: &str) -> &'static str {
    const DEADLINE_MARKERS: [&str; 3] = ["之前", "以前", "截止"];
    match category {
        "promise" | "plan" | "decision" => {
            if DEADLINE_MARKERS.iter().any(|marker| time_text.contains(marker)) {
                "deadline"
            } else {
                "validity"
            }
        }
        "event" | "state_change" | "observation" => "event_time",
        _ => "validity",
    }
}

/// 程序盖章的默认置信度：设计 §2.2 下游那句"程序补 confidence 默认 0.8"。
/// 审核通过后才允许升，本层不冒充。
const FACT_CONFIDENCE: f64 = 0.8;

/// 一条待写的 fact。字段与设计 §2.2 的输出契约一一对应，另外两格（`scope`/`source_ms`）
/// 由调用方从批次与源轮带进来——它们不在模型输出里。
#[derive(Debug, Clone)]
pub struct FactWriteItem {
    pub subject_anchor_id: String,
    pub object_anchor_id: Option<String>,
    pub predicate: String,
    pub value: Option<String>,
    /// 七值闭集，见 [`FACT_CATEGORIES`]。
    pub category: String,
    /// 只收 [`FACT_MODE_STATED`]。
    pub mode: String,
    pub time_text: String,
    pub quote: String,
    /// 多证据全写 `provenance`；`evidence_turn_uid` 取第一条。非空。
    pub evidence: Vec<(String, AnchorRole)>,
    pub text: String,
    pub scope: Scope,
    /// 源轮时刻：`created_at_ms` / `as_of_ms` / mention / 归属的入库时刻都取它。
    pub source_ms: i64,
}

/// 本次调用真写进去的行数。三格计数都是**插入行数**，不是计划条数：重放全 0。
///
/// `card_ids` 是**本次真的插进去的卡**（`INSERT OR IGNORE` 命中同址既有卡时那一行返回 0，
/// 不进这一格）：S4 的 review apply 之后只提升这一组——"这次写下的卡"才有资格被这一次审核
/// 背书；一张在本次之前就存在的卡可能已被 audit 判过，不能被另一条 fact 的通过顺手改写。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FactWriteOutcome {
    pub facts: usize,
    pub cards: usize,
    pub mentions: usize,
    /// 本次插入成功的卡句柄（顺序 = 插入顺序；去重命中不进）。
    pub card_ids: Vec<MemKey>,
}

/// fact 行（含 event 复用）的**附加参数**（S2c-1，设计 §2.3 下游）。fact 的三类 family 不传
/// extras，走 [`FactExtras::default`]，于是 [`put_fact_rows`] 的行为逐字不变。
///
/// 前两格是**列**（`participants_json` / `value_json` / `place_anchor_id`），第四格
/// `value_demote`（G1 刀 B）**不是列**——它只在落卡事务里决定要不要盖 `value_axis` 章。
///
/// 只加三列，不动 id/幂等/归属/card：
/// - `participants`：额外参与者锚 id。subject 与 object 由写库层自动并入并去重，
///   调用方不必重复填；它只进 `participants_json`，**不进** `memory_mention`
///   （mention 仍是 subject/object 各一条，与 fact 入口同一条纪律）。
/// - `value_json`：覆盖默认的「`value` 的 JSON 字符串」。event 的 value 通常是一段
///   动作/结果结构（`{"action":…,"result":…}`），不是一句自然语言，所以需要整列覆盖；
///   `value_text` 不受影响，仍是 item 自己的 `value`。
/// - `place_anchor_id`：落 `place_anchor_id` 列。列上有 `REFERENCES anchor(id)`，
///   调用方必须给已存在的锚；v0 只接受已由 naming 产出的 place 锚（设计 §6）。
#[derive(Debug, Clone, Default)]
pub struct FactExtras {
    /// 额外参与者 anchor id（subject/object 自动并入）。
    pub participants: Vec<String>,
    /// 覆盖默认的 value_json。
    pub value_json: Option<String>,
    /// 地点锚（列上有外键，必须是已存在的锚）。
    pub place_anchor_id: Option<String>,
    /// G1 刀 B（设计 §波次一第 2 层）：审核 LLM 价值轴判 `demote` 的那一行。
    ///
    /// 语义只有一句：**落卡时盖 `card_sink(reason='value_axis')` 章**（与程序预闸同表
    /// 同待遇、同一条读口），原文全留、verdict 不动。它与 correctness 判决**正交**：
    /// 被 reject 的行根本走不到这里（不落库 ⇒ 无卡可盖）。
    ///
    /// 先写者赢由 [`crate::value_gate::mark_archive`] 的 `INSERT OR IGNORE` 保证；
    /// **同一次调用里**两轴都命中时程序章优先（判定顺序就在下面那一句 if/else 里），
    /// 不让价值轴把"程序已经认过的流水"改成另一个 reason。
    pub value_demote: bool,
}

/// 一个值是否是"没给"：`None`、全空白、或归一后为空都算。
fn blank(part: &str) -> bool {
    normalize_body(part).is_empty()
}

fn blank_opt(part: Option<&str>) -> bool {
    part.map(blank).unwrap_or(true)
}

/// 时间词闭集（S2e-5，冒烟发现 5）：短语里出现任一个，就当它"像时间表达"。
/// 这是**词面**判定，不是解析：认出来只说明值得记一行 `time_expr` 让 time pass 去试，
/// 不说明能算出日期（`那阵子` 这种会认、但解析不出，行停在 `basis='none'`）。
const TIME_WORDS: [&str; 30] = [
    "今天", "今日", "昨天", "昨日", "前天", "明天", "明日", "后天", "今晚", "昨晚", "明晚",
    "周", "星期", "月", "年", "小时", "分钟", "天", "之前", "以后", "以前", "之后", "起",
    "最近", "现在", "刚才", "上午", "下午", "晚上", "阵子",
];

/// 短语是不是时间表达（S2e-5；C2 收窄）。
///
/// 三个入口任一即 true：
/// - 含**钟面/ISO**（`10:30`、`2026-10-26`）——"含 ASCII 数字"不再单独算数：
///   `H7c`、`v2`、`8GB` 里也有数字，但它们是编号/版本/容量，不是时间；
/// - 含 [`TIME_WORDS`] 里的任一时间词（`下个月` 命中 `月`、`昨晚` 命中 `昨晚`、`3天后` 命中 `天`）。
///
/// 反例（无钟面且无时间词）：空白、`再`、`先`、`下一格`、`回头`。冒烟里这三类被写进了
/// `time_expr`，把审计表变成了语气词表——所以写库层只在这个谓词为真时才落行。
/// **里程碑/批次编号**（`H5`、`H7b`、`H1c`、以及含它们的整句）一律 false：它们没有日历锚，
/// 不该变成一行"待解析的时间"（`H6` 的 unknown 是解析结论，而不是"建行再猜"）。
pub fn is_time_phrase(text: &str) -> bool {
    let phrase = text.trim();
    if phrase.is_empty() {
        return false;
    }
    // 批次编号没有时间语义，先于一切词面判定排除。
    if has_milestone_token(phrase) {
        return false;
    }
    if has_clock_surface(phrase) || has_iso_date(phrase) || has_point_clock(phrase) {
        return true;
    }
    TIME_WORDS.iter().any(|word| phrase.contains(word))
}

/// `H` + 1..2 位数字 + 可选小写字母，且两侧不是字母数字：`H5`、`H7b`、`H1c`。
/// 只认大写 `H`（`h2o`、`hub_pub` 不是批次编号）。
fn has_milestone_token(phrase: &str) -> bool {
    let chars: Vec<char> = phrase.chars().collect();
    for (index, ch) in chars.iter().enumerate() {
        if *ch != 'H' { continue; }
        let left_ok = index == 0 || !chars[index - 1].is_ascii_alphanumeric();
        if !left_ok { continue; }
        let mut end = index + 1;
        let digits_from = end;
        while end < chars.len() && end - digits_from < 2 && chars[end].is_ascii_digit() {
            end += 1;
        }
        if end == digits_from { continue; }
        if end < chars.len() && chars[end].is_ascii_lowercase() {
            end += 1;
        }
        let right_ok = end == chars.len() || !chars[end].is_ascii_alphanumeric();
        if right_ok { return true; }
    }
    false
}

/// 钟面：`H:MM`/`HH:MM`（`:` 两侧都是数字，小时 ≤2 位）。
fn has_clock_surface(phrase: &str) -> bool {
    let chars: Vec<char> = phrase.chars().collect();
    for index in 0..chars.len() {
        if !chars[index].is_ascii_digit() { continue; }
        let mut end = index;
        while end < chars.len() && chars[end].is_ascii_digit() { end += 1; }
        if end - index <= 2 && chars.get(end) == Some(&':') {
            return true;
        }
    }
    false
}

/// ISO 日期面：`YYYY-MM-DD`。
fn has_iso_date(phrase: &str) -> bool {
    let chars: Vec<char> = phrase.chars().collect();
    chars.windows(10).any(|w| {
        w[0..4].iter().all(char::is_ascii_digit) && w[4] == '-'
            && w[5..7].iter().all(char::is_ascii_digit) && w[7] == '-'
            && w[8..10].iter().all(char::is_ascii_digit)
    })
}

/// 中文/数字钟点：`十点`、`三点半`、`9点`。只认"数字紧跟点"这一种，`两点建议` 会误收成
/// 一行待解析时间（停在 `basis='none'`），代价可控——它不会给出错的值。
fn has_point_clock(phrase: &str) -> bool {
    let chars: Vec<char> = phrase.chars().collect();
    chars.iter().enumerate().any(|(index, ch)| {
        if *ch != '点' || index == 0 { return false; }
        let prev = chars[index - 1];
        prev.is_ascii_digit()
            || matches!(prev, '一' | '二' | '两' | '三' | '四' | '五' | '六' | '七' | '八' | '九' | '十')
    })
}

/// 写侧校验（在任何一个事务开始**之前**）。任一条坏 ⇒ 整批 `Err`，不落半批。
/// 错误载荷只带列名（`&'static str`），不带正文（守卫 12）。
///
/// **为什么复用 `MemError::BadA1Field` 而不新开 `BadFactField`**（这一格是片内的一处妥协，
/// 已登记在交付报告）：`MemError` 是 `crates/rc-agent/src/error.rs:195-243` 那张**逐变体点名**
/// 的分型表的输入（裁定 R40 明确拒绝通配兜底），新增一支会逼那个文件补一个 match 臂，而它不在
/// 本片的允许清单里。代价：运维输出里那句 Display 仍写 "a1 input has an invalid …"——它对 fact
/// 行不精确；换来的是文件集不越界、且列名照旧可读（`bad_a1_input` + "重抽这一批"）。
fn validate(items: &[FactWriteItem]) -> Result<(), MemError> {
    for it in items {
        if blank(&it.subject_anchor_id) { return Err(MemError::BadA1Field("subject")); }
        if it.object_anchor_id.as_deref().is_some_and(blank) {
            return Err(MemError::BadA1Field("object"));
        }
        if blank(&it.predicate) { return Err(MemError::BadA1Field("predicate")); }
        if blank(&it.text) { return Err(MemError::BadA1Field("text")); }
        if !FACT_CATEGORIES.contains(&it.category.as_str()) {
            return Err(MemError::BadA1Field("category"));
        }
        if it.mode != FACT_MODE_STATED { return Err(MemError::BadA1Field("mode")); }
        // object 与 value 至少一个非空（设计 §2.2 的 `MissingValue` 在写侧的第二道闸）。
        if blank_opt(it.object_anchor_id.as_deref()) && blank_opt(it.value.as_deref()) {
            return Err(MemError::BadA1Field("value"));
        }
        if it.evidence.is_empty() { return Err(MemError::BadA1Field("evidence")); }
        if it.evidence.iter().any(|(turn_uid, _)| turn_uid.trim().is_empty()) {
            return Err(MemError::BadA1Field("evidence"));
        }
    }
    Ok(())
}

/// extras 的写侧校验，与 [`validate`] 同一时点（事务之前）。
///
/// 三格都拒空串：`participants_json` 里一个空锚 id、`value_json` 是一段空白、
/// `place_anchor_id` 是空串，都会在库里变成"看起来有值、其实什么都没有"的列。
/// 静默丢弃（过滤掉空项）比报错更糟——调用方会以为那一项被记下了。
fn validate_extras(extras: &[FactExtras]) -> Result<(), MemError> {
    for extra in extras {
        if extra.participants.iter().any(|anchor_id| blank(anchor_id)) {
            return Err(MemError::BadA1Field("participants"));
        }
        if extra.value_json.as_deref().is_some_and(blank) {
            return Err(MemError::BadA1Field("value_json"));
        }
        if extra.place_anchor_id.as_deref().is_some_and(blank) {
            return Err(MemError::BadA1Field("place"));
        }
    }
    Ok(())
}

/// fact 行的唯一插入字面量。列序与 `rc-store` 的 DDL 逐位对应，**每一列都显式出现**：
/// 时间/状态那七列写 NULL 是设计 §2.2 的"先空，等 time pass 回填"，交给 DEFAULT 会变成
/// "没人说过"和"程序说过是空"分不清。
///
/// `place_anchor_id` 是 S2c-1 起唯一从 extras 来的列（`?9`）；`speaker_anchor_id` 是 C1 起
/// **程序算出来的列**（`?10`，算法见 [`speaker_anchor_id`]）；其余列与 S2b-2a 逐位相同。
const FACT_INSERT: &str = "INSERT INTO fact(
    id, subject_anchor_id, predicate, object_anchor_id, value_json, value_text, category,
    participants_json, place_anchor_id, speaker_anchor_id, occurred_at_ms, occurred_end_ms,
    speech_time_ms, valid_from_ms, valid_to_ms, due_at_ms, status, mode, confidence, text,
    evidence_turn_uid, superseded_by, world_id, scope_kind, scope_key, source_agent, device_id,
    created_at_ms, norm_version)
 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,NULL,NULL,NULL,NULL,NULL,NULL,NULL,?11,?12,?13,
        ?14,NULL,NULL,?15,?16,?17,?18,?19,?20)
 ON CONFLICT(id) DO NOTHING";

/// 最小消费卡的唯一插入字面量。`kind=category`、`verdict='unreviewed'`：
/// 审核尚未外置（S3），所以卡不冒充 `passed`；`mode/confidence` 走列的 DEFAULT。
///
/// `time_json` **显式写 NULL**（T33）：写卡这一刻还没有任何已解析的时间——解析在
/// `rc_distill::pass_time`（rc-memory 不反向依赖解析器），而 `fact` 的七列时间也是同一时刻
/// 留空的。卡面时间因此由 time 回填在**同一事务**里补（`card.time_json IS NULL` 才写），
/// 形状见 `pass_time` 的 `card_time_json`。交给 DEFAULT 同样会写成 NULL，但"没人说过"与
/// "程序说过是空"就分不清了（与 [`FACT_INSERT`] 对时间列的处理同一条纪律）。
const CARD_INSERT: &str = "INSERT OR IGNORE INTO card(
    id, kind, title, text, time_json, pointers_json, scope_kind, scope_key, as_of_ms, verdict,
    source_agent, device_id, created_at_ms, norm_version)
 VALUES(?1,?2,?3,?4,NULL,?5,?6,?7,?8,'unreviewed',?9,?10,?11,?12)";

/// 未解析时间行的唯一插入字面量（S2d-1）。`start/end=NULL` + `precision='unknown'` +
/// `basis='none'` 是**"还没解析"的诚实形状**：写库层不做日期算术（那是 time pass 的事），
/// 也不许用 0 或列的 DEFAULT 冒充一个时间。`id` 是内容寻址（[`compute_time_expr_id`]）⇒
/// 同一 `(turn, text, kind)` 只有一行，重放不增行。
const TIME_EXPR_INSERT: &str = "INSERT OR IGNORE INTO time_expr(
    id, turn_uid, text, start_ms, end_ms, precision, basis, kind, created_at_ms)
 VALUES(?1,?2,?3,NULL,NULL,'unknown','none',?4,?5)";

/// JSON 字符串字面量（带引号）。rc-memory 的依赖集里**没有** `serde_json`（`writers_x.rs`
/// 文件头那条守卫 10 的红线），而 `fact.value_json` 是 TEXT 列、读侧按 JSON 解 ⇒ 这里只做
/// 最小转义：`"` `\` 与 C0 控制字符，其余原样。它不是通用 JSON 编码器，也不假装是。
///
/// `pub(crate)`（W14c）：`attribute.value_json` 是同一形状的 TEXT 列，那一层也走这一把尺子
/// ——两个转义器就是"同一个值在两张表里编码成两串字节"的起点。
pub(crate) fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 字符串数组的 JSON 文本。participants 与 card pointers 都只有内容哈希/锚 id，
/// 但走同一把转义尺子，免得日后有人把自由文本塞进来。
fn json_array(values: &[String]) -> String {
    let mut out = String::from("[");
    for (index, value) in values.iter().enumerate() {
        if index > 0 { out.push(','); }
        out.push_str(&json_string(value));
    }
    out.push(']');
    out
}

/// 写一批 fact 行（fact pass 的旧入口）。**整批一个事务**：任何一条失败 ⇒ 一条都不写。
///
/// 行为 = [`put_fact_rows_with_extras`] + 全默认 extras：participants 只有 subject/object、
/// `value_json` 是 `value` 的 JSON 字符串、`place_anchor_id` 为 NULL。
/// 这一格保持独立存在，是为了 fact 的三族调用方一个字节都不用改（S2c-1 的硬约束）。
pub fn put_fact_rows(
    m: &MemoryHandle, items: &[FactWriteItem], ctx: &BatchCtx,
    owner: &OwnerCtx, now_ms: i64,
) -> Result<FactWriteOutcome, MemError> {
    let extras = vec![FactExtras::default(); items.len()];
    put_fact_rows_with_extras(m, items, &extras, ctx, owner, now_ms)
}

/// 写一批 fact/event 行，并接受三格附加字段（S2c-1）。
///
/// `extras.len()` 必须等于 `items.len()`：对不上是**调用方的错**，返回 `MemError::Sql`
/// 而不是把长的那一侧截断（静默跳过会让"事件少了一个参与者"永远查不出来）。
///
/// `now_ms` 是形参位（与其余写入口同形），本层**不使用**：所有时刻取源轮 `source_ms`。
/// 这不是省事——I3 的教训正是"拿导入钟冒充源时刻"会让行的时间偏几天；把时钟位留着不用，
/// 是为了让"本层没有第二个时钟来源"在读代码时一眼可见。
pub fn put_fact_rows_with_extras(
    m: &MemoryHandle, items: &[FactWriteItem], extras: &[FactExtras],
    ctx: &BatchCtx, owner: &OwnerCtx, _now_ms: i64,
) -> Result<FactWriteOutcome, MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    if extras.len() != items.len() {
        // 载荷是一句静态话：不把两侧长度或任何行内容带进运维输出。
        return Err(MemError::Sql("fact extras length does not match items length".to_string()));
    }
    validate(items)?;
    validate_extras(extras)?;
    let mut out = FactWriteOutcome::default();
    if items.is_empty() { return Ok(out); }
    // 说话人（C1）：**程序**按"证据轮的转录角色 + 会话身份"算，模型一个字段都递不进来
    // （payload 里没有这一格）。算法只有一处实现（[`crate::writers_v4::speaker_anchor_id`]），
    // 闸门与读侧不再各拼一份。查不到锚 ⇒ NULL：本层不造锚，也不写悬空 id
    // （`speaker_anchor_id` 有 `REFERENCES anchor(id)`）。
    let speakers: Vec<Option<String>> = items
        .iter()
        .map(|it| {
            crate::writers_v4::speaker_anchor_id(
                m,
                &ctx.source_agent,
                owner.owner_principal_id.as_deref(),
                &it.evidence,
                &it.scope,
            )
        })
        .collect::<Result<_, _>>()?;
    let norm_version = rc_store::capability::NORM_VERSION as i64;
    let tx = m.conn().unchecked_transaction()?;
    for (index, it) in items.iter().enumerate() {
        let extra = &extras[index];
        let id = compute_fact_id(
            &it.subject_anchor_id, &it.predicate, it.object_anchor_id.as_deref(),
            it.value.as_deref(), &it.category, &it.time_text, &it.text, &it.scope,
        );
        // participants：subject 在前、object 次之、extras 的参与者随后，重复只留一次
        // （subject==object 的写法合法；extras 里重复写同一个锚也不造第二格）。
        let mut participants: Vec<String> = Vec::with_capacity(2 + extra.participants.len());
        let heads = [Some(&it.subject_anchor_id), it.object_anchor_id.as_ref()];
        for anchor_id in heads.into_iter().flatten().chain(extra.participants.iter()) {
            if !participants.iter().any(|seen| seen == anchor_id) {
                participants.push(anchor_id.clone());
            }
        }
        // value_json：extras 给了就整列覆盖（event 的动作/结果是结构，不是一句话）；
        // 否则仍是 `value` 的 JSON 字符串（fact 的旧行为）。
        let value_json = extra.value_json.clone().or_else(|| it.value.as_deref().map(json_string));
        let evidence_turn_uid = &it.evidence[0].0;   // validate 已保证非空
        let inserted: usize = tx.execute(
            FACT_INSERT,
            rusqlite::params![
                id, it.subject_anchor_id, it.predicate, it.object_anchor_id,
                value_json, it.value, it.category, json_array(&participants),
                extra.place_anchor_id, speakers[index], FACT_MODE_STATED, FACT_CONFIDENCE,
                it.text, evidence_turn_uid, it.scope.kind.as_str(), it.scope.key,
                ctx.source_agent, ctx.device_id, it.source_ms, norm_version,
            ],
        )?;
        out.facts += inserted;
        // 时间短语：记下原文，**不解析**（S2d-1）。`start/end/precision/basis` 留"没解析"，
        // 由 time pass 的确定性解析（或 S2d-2 的模型）回填。`created_at_ms` 取源轮时刻
        // ——它既是这一行的入库钟，也是日后解析"今天/下周"的参照钟（I3：不用导入钟）。
        // S2e-5：只有**像时间表达**的短语才落行（`再`/`先`/`下一格` 不是时间，见 [`is_time_phrase`]）。
        if is_time_phrase(&it.time_text) {
            let kind = time_kind_for(&it.category, &it.time_text);
            let time_id = compute_time_expr_id(evidence_turn_uid, &it.time_text, kind);
            tx.execute(
                TIME_EXPR_INSERT,
                rusqlite::params![time_id, evidence_turn_uid, it.time_text, kind, it.source_ms],
            )?;
        }
        // 出处：多证据全写，role 取证据自己的（`evidence_turn_uid` 只是第一条的副本）。
        for (turn_uid, role) in &it.evidence {
            tx.execute(
                "INSERT OR IGNORE INTO provenance(mem_table, mem_id, turn_uid, anchor_role)
                 VALUES('fact',?1,?2,?3)",
                rusqlite::params![id, turn_uid, role.as_str()],
            )?;
        }
        // 提及：subject/object 各一条（同一个锚只一条），P 独占列只从 ctx 取。
        // extras 的参与者**不**进 mention：mention 是"这条行提到的主客体"，与 fact 入口
        // 逐字同形；把事件参与者塞进来会让同一条行的两套计数口径分叉。
        let mut mentioned: Vec<&String> = vec![&it.subject_anchor_id];
        if let Some(object) = &it.object_anchor_id {
            if object != &it.subject_anchor_id { mentioned.push(object); }
        }
        for anchor_id in mentioned {
            out.mentions += tx.execute(
                "INSERT OR IGNORE INTO memory_mention(
                     mem_table, mem_id, anchor_id, source_agent, device_id, created_at_ms,
                     norm_version)
                 VALUES('fact',?1,?2,?3,?4,?5,?6)",
                rusqlite::params![id, anchor_id, ctx.source_agent, ctx.device_id,
                                  it.source_ms, norm_version],
            )?;
        }
        // 最小消费卡：title 截 30 字、指针指回 fact、verdict 不冒充已审。
        let title: String = it.text.chars().take(30).collect();
        let card_id = compute_mem_id(MemTable::Card, &it.category, &title, &it.text, &it.scope);
        let inserted = tx.execute(
            CARD_INSERT,
            rusqlite::params![
                card_id, it.category, title, it.text, json_array(&[format!("fact:{id}")]),
                it.scope.kind.as_str(), it.scope.key, it.source_ms,
                ctx.source_agent, ctx.device_id, it.source_ms, norm_version,
            ],
        )?;
        out.cards += inserted;
        // S4：只记**真的插进去**的卡。`INSERT OR IGNORE` 命中同址既有卡时返回 0，那张卡
        // 不是这次写的（可能早已被 audit 判过）⇒ 不交给 review apply 去提升。
        if inserted == 1 {
            out.card_ids.push(MemKey { table: MemTable::Card, id: card_id.clone() });
        }
        // 镜像卡自己的出处：推导池的口径是"行自身要有 turn 出处"
        // （`reader::turn_basis_pred`），只写 `mem_table='fact'` 时推导看不见任何事实/事件行
        // （样本库 150 张卡只有 8 张进池）。证据轮与角色与 fact 逐条同形；
        // 检索侧的 `pointers_json` 兜底因此不再是唯一路径，两条读到的是同一组轮。
        for (turn_uid, role) in &it.evidence {
            tx.execute(
                "INSERT OR IGNORE INTO provenance(mem_table, mem_id, turn_uid, anchor_role)
                 VALUES('card',?1,?2,?3)",
                rusqlite::params![card_id, turn_uid, role.as_str()],
            )?;
        }
        // G1 刀 A：程序资格预闸（与回填同一判定函数同一把尺子）。三者合取、豁免优先；
        // 命中 ⇒ 镜像卡盖 sink=archive 章：原文全留、照常进库、verdict 不动，
        // 只是召回主循环缺省不取（消费在 rc_search::rank::rank_counted 单一读口）。
        // 章钟取源轮时刻（I3：与卡同一口钟，不用导入钟）。重放命中原章 ⇒ 0 行（幂等）。
        let self_anchor = crate::writers_v4::session_agent_anchor_id(
            m, &ctx.source_agent, &it.scope)?;
        let human = crate::value_gate::human_provenance_exists(&tx, "card", &card_id)?;
        let gate_hit = crate::value_gate::program_gate_hit(&crate::value_gate::GateInput {
            subject_anchor_id: &it.subject_anchor_id,
            speaker_anchor_id: speakers[index].as_deref(),
            self_anchor_id: self_anchor.as_deref(),
            human_in_provenance: human,
            category: &it.category,
            predicate: &it.predicate,
        });
        if gate_hit {
            crate::value_gate::mark_archive(
                &tx, &card_id, crate::value_gate::PROGRAM_GATE_REASON,
                it.source_ms, norm_version)?;
        } else if extra.value_demote {
            // G1 刀 B：审核 LLM 价值轴判 demote。两轴命中同一次调用时程序章优先（上面那支）；
            // 已被**先前**一次写入盖过程序章的卡，这里 `INSERT OR IGNORE` 返回 0（先写者赢）。
            crate::value_gate::mark_archive(
                &tx, &card_id, crate::value_gate::VALUE_AXIS_REASON,
                it.source_ms, norm_version)?;
        }
        // 归属：fact 与 card 各一行。visibility 再过一次上界（行 scope 是天花板）。
        for (table, mem_id) in [(MemTable::Card.prefix(), &card_id), ("fact", &id)] {            owner::insert(&tx, &owner::OwnerRow {
                mem_table: table.to_string(),
                mem_id: mem_id.clone(),
                owner_principal_id: owner.owner_principal_id.clone(),
                owner_agent_id: owner.owner_agent_id.clone(),
                created_by_agent: ctx.source_agent.clone(),
                visibility: owner::effective(owner.visibility, it.scope.kind),
                retention: owner::normalize_retention(&owner.retention),
                scope: it.scope.clone(),
                source_agent: Some(ctx.source_agent.clone()),
                device_id: Some(ctx.device_id.clone()),
                created_at_ms: it.source_ms,
            })?;
        }
    }
    tx.commit()?;
    Ok(out)
}
