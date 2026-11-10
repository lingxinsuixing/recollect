//! 20K 常驻简报（设计 §9.1 分区预算、§9.3 注入协议）。
//!
//! 渲染顺序固定：`L0 → 中文表达优化 → 人格核心 → 主用户画像 → 关系/交互 → 情绪 → 未闭环 → 交接 → 小本本 → pinned → 近期`。
//! 稳定前缀在前、易变状态在后，便于 prompt caching；`scene` 只进头部元信息，不改变顺序。
//!
//! 预算分两层：
//! - 区上限（`CAP_*`）在拼每一行时检查：超了就从尾部丢条目，长行截断并记进 `truncated`；
//! - 总预算在拼完整篇后检查：超了按 §9.3 的顺序淘汰 pinned → 低显著（无截止）loop →
//!   低置信 affect（confidence < 0.5）→ 主用户画像里的偏好；L0 排在最后，只要还有别的可丢就不动它。
//!
//! 淘汰是确定性的：同一份 payload、同一 `now_ms` 与 `budget` 永远得到同一份简报。
//! 唯一的不确定来源是 payload 里没有 salience/频率字段——pinned/loop/偏好都按调用方给的顺序
//! 视为"重要在前"，从尾部淘汰。

use crate::ops::NotebookEntry;
use crate::payload::{Affect, CorePayload, HandoffItem, LoopItem, Subject, HANDOFF_KINDS};
use serde_json::Value;
use std::collections::BTreeMap;

/// 默认常驻内容预算（不含 system prompt/工具 schema）。`0` 表示用这个默认值。
pub const DEFAULT_BUDGET: usize = 20_000;

/// 分区上限（token），逐项照 §9.1 的 1.5K/4K/3.5K/2.5K/2.5K/2.5K/3.5K/1K。
pub const CAP_L0: usize = 1_500;
pub const CAP_CHINESE_EXPRESSION: usize = 600;
pub const CAP_PERSONA: usize = 4_000;
pub const CAP_SUBJECT: usize = 3_500;
pub const CAP_RELATIONSHIP: usize = 2_500;
pub const CAP_AFFECT: usize = 2_500;
pub const CAP_LOOPS: usize = 2_500;
pub const CAP_HANDOFF: usize = 1_000;
/// 「上一棒交接」段（W34 续接包）的区上限。它与 [`CAP_HANDOFF`] 是两把区尺：那一段住
/// `core_brief`，这一段住 hook brief；量行、截断、淘汰走的是同一个 [`Section::push`] 机制。
pub const CAP_HANDOFF_PREV: usize = 600;
/// 小本本索引栏（W36 细案 §4）的区上限。≈20 行索引；量行、截断、淘汰照走
/// [`Section::push`]，不造第二把尺。已拍板：CAP=600。
pub const CAP_NOTEBOOK: usize = 600;
pub const CAP_PINNED: usize = 3_500;
pub const CAP_RECENT: usize = 1_000;

/// `section_tokens`/`truncated` 里的区名。顺序与渲染顺序一致。
pub const SEC_L0: &str = "l0";
pub const SEC_CHINESE_EXPRESSION: &str = "chinese_expression";
pub const SEC_PERSONA: &str = "persona";
pub const SEC_SUBJECT: &str = "subject";
pub const SEC_RELATIONSHIP: &str = "relationship";
pub const SEC_AFFECT: &str = "affect";
pub const SEC_LOOPS: &str = "loops";
pub const SEC_HANDOFF: &str = "handoff";
/// 「上一棒交接」段（W34 续接包）的区名。与 [`SEC_HANDOFF`]/[`SEC_NOTEBOOK`]
/// 是三个不同的段：命名避让是约定，谁先落地谁占名字，后到的改自己的。
pub const SEC_HANDOFF_PREV: &str = "handoff_prev";
/// 小本本索引栏（W36 细案 §4）的区名。
pub const SEC_NOTEBOOK: &str = "notebook";
pub const SEC_PINNED: &str = "pinned";
pub const SEC_RECENT: &str = "recent";

/// `core_brief` 缺省注入的交接条种类：接着做与即将到期。
/// `reminder` 与 `note` 要调用方显式 `include`（见 [`render_brief_with`]）。
pub const DEFAULT_INCLUDE: [&str; 2] = ["continue", "due"];

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;

/// 一次 `core_brief` 的结果。`text` 是给模型看的自然语言；其余三项是 `core_injection_log` 的读数。
#[derive(Debug, Clone, PartialEq)]
pub struct Brief {
    pub text: String,
    pub token_estimate: usize,
    /// 只含实际渲染出来的区；被省略的区不出现。
    pub section_tokens: BTreeMap<&'static str, usize>,
    /// 被截断/淘汰的区名，按首次发生顺序去重。
    pub truncated: Vec<&'static str>,
}

impl Brief {
    fn empty() -> Self {
        Brief {
            text: String::new(),
            token_estimate: 0,
            section_tokens: BTreeMap::new(),
            truncated: Vec::new(),
        }
    }
}

/// 保守 token 近似。唯一实现住 `rc-contract::text`：检索 brief 与本简报必须用同一把尺子，
/// 否则同一条记忆在两张面上会被算成不同长度。
pub fn estimate_tokens(text: &str) -> usize {
    rc_contract::estimate_tokens(text)
}

/// 相对时间。唯一实现住 `rc-contract::text`（同上：两张面同一句话）。
pub fn render_delta(now_ms: i64, then_ms: i64) -> String {
    rc_contract::render_delta(now_ms, then_ms)
}

/// 截止时间的说法：未到期"还有 N 天"，已到期"已过期 N 天"。`due_ms <= 0` ⇒ 未知。
pub fn render_due(now_ms: i64, due_ms: i64) -> String {
    if due_ms <= 0 {
        return "未知".to_string();
    }
    let delta = due_ms.saturating_sub(now_ms);
    if delta > 0 {
        if delta < HOUR_MS {
            format!("还有 {} 分钟", (delta / MINUTE_MS).max(1))
        } else if delta < DAY_MS {
            format!("还有 {} 小时", delta / HOUR_MS)
        } else {
            format!("还有 {} 天", delta / DAY_MS)
        }
    } else {
        let late = now_ms.saturating_sub(due_ms);
        if late == 0 {
            "今天到期".to_string()
        } else if late < HOUR_MS {
            format!("已过期 {} 分钟", (late / MINUTE_MS).max(1))
        } else if late < DAY_MS {
            format!("已过期 {} 小时", late / HOUR_MS)
        } else {
            format!("已过期 {} 天", late / DAY_MS)
        }
    }
}

/// 时长（不是时间点）：attention_debt 这类"攒了多久"。
fn render_elapsed(ms: i64) -> String {
    if ms <= 0 {
        return "0 分钟".to_string();
    }
    if ms < MINUTE_MS {
        "不足 1 分钟".to_string()
    } else if ms < HOUR_MS {
        format!("{} 分钟", ms / MINUTE_MS)
    } else if ms < DAY_MS {
        format!("{} 小时", ms / HOUR_MS)
    } else {
        format!("{} 天", ms / DAY_MS)
    }
}

/// 行标签决定总预算淘汰时谁先走。
#[derive(Debug, Clone, Copy, PartialEq)]
enum Tag {
    Plain,
    /// pinned 经历指针：低 salience 先出，这里按调用方给的顺序从尾部走。
    Pinned,
    Loop,
    Affect { confidence: f64 },
    /// 主用户偏好：§9.3 淘汰顺序的最后一项。
    Preference,
}

#[derive(Debug, Clone)]
struct Line {
    text: String,
    tag: Tag,
}

/// 一个分区：固定标题 + 若干行。空分区（没有行）不输出。
#[derive(Debug)]
struct Section {
    key: &'static str,
    heading: &'static str,
    cap: usize,
    lines: Vec<Line>,
    truncated: bool,
}

impl Section {
    fn new(key: &'static str, heading: &'static str, cap: usize) -> Self {
        Section { key, heading, cap, lines: Vec::new(), truncated: false }
    }

    /// 追加一行；超过区上限就截断这一行（放不下则丢弃），并记 `truncated`。
    fn push(&mut self, tag: Tag, text: String) {
        if text.trim().is_empty() {
            return;
        }
        let mut candidate = self.lines.clone();
        candidate.push(Line { text, tag });
        if estimate_tokens(&render_lines(self.heading, &candidate)) <= self.cap {
            self.lines = candidate;
            return;
        }
        self.truncated = true;
        let line = candidate.pop().expect("just pushed");
        let used = estimate_tokens(&render_lines(self.heading, &self.lines));
        let mut remaining = self.cap.saturating_sub(used);
        // 截断后还要重新按"标题 + 前缀 + 换行"整体估算，超了就再缩一点；最多几轮就收敛。
        for _ in 0..4 {
            let Some(short) = truncate_to_fit(&line.text, remaining) else {
                break;
            };
            let mut candidate = self.lines.clone();
            candidate.push(Line { text: short, tag });
            let cost = estimate_tokens(&render_lines(self.heading, &candidate));
            if cost <= self.cap {
                self.lines = candidate;
                break;
            }
            remaining = remaining.saturating_sub(cost - self.cap + 1);
        }
    }

    fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    fn render(&self) -> String {
        render_lines(self.heading, &self.lines)
    }

    fn tokens(&self) -> usize {
        estimate_tokens(&self.render())
    }
}

fn render_lines(heading: &str, lines: &[Line]) -> String {
    let mut out = String::from(heading);
    for line in lines {
        out.push('\n');
        out.push_str(&line.text);
    }
    out
}

/// 把一行截到 `max_tokens` 以内（加省略号）。放不下任何有意义的字符 ⇒ `None`。
fn truncate_to_fit(text: &str, max_tokens: usize) -> Option<String> {
    // 给省略号本身留 1 token，否则截出来的前缀会正好占满、反而没有省略号可加。
    let limit = max_tokens.checked_sub(1)?;
    let mut ascii = 0usize;
    let mut wide = 0usize;
    let mut taken = 0usize;
    for (i, c) in text.char_indices() {
        let (a, w) = if c.is_ascii() { (1, 0) } else { (0, 1) };
        if (ascii + a) / 4 + (wide + w) > limit {
            break;
        }
        ascii += a;
        wide += w;
        taken = i + c.len_utf8();
    }
    let prefix = text[..taken].trim_end();
    if prefix.chars().count() < 4 {
        return None;
    }
    Some(format!("{prefix}…"))
}

/// 单行化：载荷里的自由文本可能带换行，渲染不能因此多出假标题。
fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !in_space && !out.is_empty() {
                out.push(' ');
                in_space = true;
            }
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out.trim().to_string()
}

/// 头部元信息里不放大段自由文本。
fn clip(text: &str, max_chars: usize) -> String {
    let line = one_line(text);
    if line.chars().count() <= max_chars {
        line
    } else {
        let mut out: String = line.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(s) => one_line(s),
        other => other.to_string(),
    }
}

fn mark(list: &mut Vec<&'static str>, key: &'static str) {
    if !list.contains(&key) {
        list.push(key);
    }
}

fn l0_section(payload: &CorePayload) -> Section {
    let mut s = Section::new(SEC_L0, "[L0 安全与身份]", CAP_L0);
    for item in &payload.l0 {
        s.push(Tag::Plain, format!("- {}", one_line(item)));
    }
    s
}

fn chinese_expression_section(payload: &CorePayload) -> Section {
    let mut s = Section::new(
        SEC_CHINESE_EXPRESSION,
        "[中文表达优化]",
        CAP_CHINESE_EXPRESSION,
    );
    if !payload.chinese_expression.enabled {
        return s;
    }
    for rule in crate::payload::CHINESE_EXPRESSION_RULES {
        s.push(Tag::Plain, format!("- {rule}"));
    }
    s
}

fn persona_section(payload: &CorePayload) -> Section {
    let mut s = Section::new(SEC_PERSONA, "[L1 人格核心]", CAP_PERSONA);
    let p = &payload.persona;
    if !p.core_text.trim().is_empty() {
        s.push(Tag::Plain, format!("- 核心：{}", one_line(&p.core_text)));
    }
    for (k, v) in &p.traits {
        s.push(Tag::Plain, format!("- 特质 {}：{}", one_line(k), value_text(v)));
    }
    for (k, v) in &p.style {
        s.push(Tag::Plain, format!("- 风格 {}：{}", one_line(k), value_text(v)));
    }
    for quirk in &p.quirks {
        s.push(Tag::Plain, format!("- 癖好：{}", one_line(quirk)));
    }
    for boundary in &p.boundaries {
        s.push(Tag::Plain, format!("- 禁忌：{}", one_line(boundary)));
    }
    // 语气样例只放前 2 条：它是风味，不是清单。
    for (i, example) in p.voice_examples.iter().take(2).enumerate() {
        s.push(Tag::Plain, format!("- 语气样例{}：{}", i + 1, one_line(example)));
    }
    s
}

fn subject_section(payload: &CorePayload) -> Section {
    let mut s = Section::new(SEC_SUBJECT, "[主用户画像]", CAP_SUBJECT);
    // 主用户优先；其余是核心关系，各占一行摘要。
    let mut ordered: Vec<&Subject> =
        payload.subjects.iter().filter(|x| x.is_primary).collect();
    ordered.extend(payload.subjects.iter().filter(|x| !x.is_primary));
    for subject in ordered {
        if subject.is_primary {
            let who = if subject.subject_id.trim().is_empty() {
                "主用户".to_string()
            } else {
                format!("主用户 {}", one_line(&subject.subject_id))
            };
            s.push(Tag::Plain, format!("- {who}"));
            if !subject.profile_text.trim().is_empty() {
                s.push(Tag::Plain, format!("- 背景：{}", one_line(&subject.profile_text)));
            }
            for (k, v) in &subject.preferences {
                s.push(
                    Tag::Preference,
                    format!("- 偏好 {}：{}", one_line(k), value_text(v)),
                );
            }
            for trigger in &subject.triggers {
                s.push(Tag::Plain, format!("- 触发点：{}", one_line(trigger)));
            }
            for taboo in &subject.taboos {
                s.push(Tag::Plain, format!("- 雷区：{}", one_line(taboo)));
            }
        } else {
            let id = if subject.subject_id.trim().is_empty() {
                "未命名".to_string()
            } else {
                one_line(&subject.subject_id)
            };
            let text = one_line(&subject.profile_text);
            if text.is_empty() {
                s.push(Tag::Plain, format!("- 核心关系 {id}"));
            } else {
                s.push(Tag::Plain, format!("- 核心关系 {id}：{text}"));
            }
            for (k, v) in &subject.preferences {
                s.push(
                    Tag::Preference,
                    format!("- 偏好 {}：{}", one_line(k), value_text(v)),
                );
            }
        }
    }
    s
}

fn relationship_section(payload: &CorePayload, now_ms: i64) -> Section {
    let mut s = Section::new(SEC_RELATIONSHIP, "[关系与交互]", CAP_RELATIONSHIP);
    let r = &payload.relationship;
    let i = &payload.interaction;
    let untouched = r.text.trim().is_empty()
        && (r.trust - 0.5).abs() < f64::EPSILON
        && (r.intimacy - 0.5).abs() < f64::EPSILON
        && r.tension == 0.0
        && r.status == "normal"
        && i.last_user_message_at_ms <= 0
        && i.last_agent_initiated_at_ms <= 0
        && i.unanswered_user_messages == 0
        && i.attention_debt_ms <= 0
        && i.current_topic.trim().is_empty();
    if untouched {
        return s;
    }

    let status = if r.status.trim().is_empty() { "未知" } else { r.status.as_str() };
    s.push(
        Tag::Plain,
        format!(
            "- 关系：信任 {:.2} / 亲密 {:.2} / 张力 {:.2} / 状态 {}",
            r.trust, r.intimacy, r.tension, status
        ),
    );
    if !r.text.trim().is_empty() {
        s.push(Tag::Plain, format!("- 关系描述：{}", one_line(&r.text)));
    }
    let last_message = if i.last_user_message_at_ms > 0 {
        render_delta(now_ms, i.last_user_message_at_ms)
    } else {
        "未知".to_string()
    };
    let last_initiated = if i.last_agent_initiated_at_ms > 0 {
        render_delta(now_ms, i.last_agent_initiated_at_ms)
    } else {
        "未知".to_string()
    };
    let topic = if i.current_topic.trim().is_empty() {
        "未知".to_string()
    } else {
        one_line(&i.current_topic)
    };
    s.push(Tag::Plain, format!("- 上次消息：{last_message}"));
    s.push(Tag::Plain, format!("- 上次主动：{last_initiated}"));
    s.push(
        Tag::Plain,
        format!("- 未回消息：{} 条", i.unanswered_user_messages),
    );
    s.push(Tag::Plain, format!("- 当前话题：{topic}"));
    s.push(
        Tag::Plain,
        format!("- 注意欠账：{}", render_elapsed(i.attention_debt_ms)),
    );
    s
}

fn affect_section(payload: &CorePayload, now_ms: i64) -> Section {
    let mut s = Section::new(SEC_AFFECT, "[情绪状态]", CAP_AFFECT);
    let mut affects: Vec<&Affect> = payload.affects.iter().collect();
    // 先 user 后 agent；同类里新的在前。
    affects.sort_by(|a, b| {
        let rank = |kind: &str| usize::from(kind != "user");
        rank(&a.subject_kind)
            .cmp(&rank(&b.subject_kind))
            .then(b.valid_from_ms.cmp(&a.valid_from_ms))
            .then(a.label.cmp(&b.label))
    });
    for affect in affects {
        let who = match affect.subject_kind.as_str() {
            "user" => "用户",
            "agent" => "agent",
            _ => "未知对象",
        };
        let label = if affect.label.trim().is_empty() {
            "未标注".to_string()
        } else {
            one_line(&affect.label)
        };
        let cause = if affect.cause.trim().is_empty() {
            "未知".to_string()
        } else {
            one_line(&affect.cause)
        };
        s.push(
            Tag::Affect { confidence: affect.confidence },
            format!(
                "- {who}：{label}，valence {:.2}，arousal {:.2}，energy {:.2}，置信 {:.2}，起于 {}，原因：{cause}",
                affect.valence,
                affect.arousal,
                affect.energy,
                affect.confidence,
                render_delta(now_ms, affect.valid_from_ms)
            ),
        );
    }
    s
}

fn loops_section(payload: &CorePayload, now_ms: i64) -> Section {
    let mut s = Section::new(SEC_LOOPS, "[未闭环]", CAP_LOOPS);
    let mut open: Vec<&LoopItem> = payload
        .loops
        .iter()
        .filter(|item| item.status == "open")
        .collect();
    // 有截止日的先列，尾部是低显著（无截止）——淘汰也从这个尾部走。
    open.sort_by(|a, b| {
        (a.due_at_ms <= 0)
            .cmp(&(b.due_at_ms <= 0))
            .then(a.due_at_ms.cmp(&b.due_at_ms))
            .then(a.id.cmp(&b.id))
    });
    for item in open {
        let kind = if item.kind.trim().is_empty() { "事项" } else { item.kind.as_str() };
        let text = if item.text.trim().is_empty() {
            "（无描述）".to_string()
        } else {
            one_line(&item.text)
        };
        let mut line = format!("- [{kind}] {text}");
        if item.due_at_ms > 0 {
            line.push_str(&format!("（{}）", render_due(now_ms, item.due_at_ms)));
        }
        s.push(Tag::Loop, line);
    }
    s
}

/// 交接条（2026-10-28）：给下个会话的便签。可见性规则只有这一份：
///
/// - `closed_at_ms.is_some()` 或已过期（`expires_at_ms <= now_ms`）⇒ 永不出现；
/// - `include` 是**种类过滤器**（缺省 [`DEFAULT_INCLUDE`]），不在里面就不出现；
/// - `due`/`reminder` 额外要求到期前 24 小时内（含已到期），`continue`/`note` 不加时间窗。
///
/// 顺序是 `HANDOFF_KINDS` 的下标（接着做 → 到期 → 提醒 → 备注），同 kind 内新的在前。
fn handoff_section(payload: &CorePayload, include: &[&str], now_ms: i64) -> Section {
    let mut s = Section::new(SEC_HANDOFF, "[交接（给下个会话）]", CAP_HANDOFF);
    let mut items: Vec<&HandoffItem> = payload
        .handoffs
        .iter()
        .filter(|h| h.closed_at_ms.is_none())
        .filter(|h| h.expires_at_ms > now_ms)
        .filter(|h| include.contains(&h.kind.as_str()))
        .filter(|h| match h.kind.as_str() {
            // 到期/提醒只在「到期前 24 小时内」出现；过了 `expires_at_ms` 已被上面那条滤掉。
            "due" | "reminder" => h.expires_at_ms.saturating_sub(now_ms) <= DAY_MS,
            _ => true,
        })
        .collect();
    let rank = |kind: &str| {
        HANDOFF_KINDS
            .iter()
            .position(|known| *known == kind)
            .unwrap_or(HANDOFF_KINDS.len())
    };
    items.sort_by(|a, b| {
        rank(&a.kind)
            .cmp(&rank(&b.kind))
            .then(b.created_at_ms.cmp(&a.created_at_ms))
    });
    for item in items {
        let text = one_line(&item.text);
        let line = match item.kind.as_str() {
            "continue" => format!(
                "- 接着做（{}写下，{}）：{text}",
                render_delta(now_ms, item.created_at_ms),
                render_due(now_ms, item.expires_at_ms)
            ),
            "due" => format!("- 到期（{}）：{text}", render_due(now_ms, item.expires_at_ms)),
            "reminder" => {
                format!("- 提醒（{}）：{text}", render_due(now_ms, item.expires_at_ms))
            }
            // `note` 只是备注，没有时间语义；种类闭集保证这里只剩 note。
            _ => format!("- 备注：{text}"),
        };
        s.push(Tag::Plain, line);
    }
    s
}

/// 「上一棒交接」段（W34 续接包）：hook brief 的固定段，内容 = `core_handoff_list`
/// 的未闭条目 + 调用方给的项目状态读数行。**纯读侧**：条目列表原样进来，open/close
/// 语义与生命周期一字不动。
///
/// - `items` 用 [`crate::CoreStore::handoff_list`] 的返回形状：未关闭、**含已过期**
///   （过期与否由 [`render_due`] 在行内标出——"已过期 N 天"就是这一段的 expired 标记），
///   且已按 kind 序排好；这里不再排第二遍。
/// - `readings` 是调用方拼好的读数行（一句一行；**报不报数、报哪个键由调用方定**——
///   本函数不回库补查，rc-core 的依赖面碰不到 memory.db）。空行与整行空白的读数会被
///   [`Section::push`] 丢弃。
/// - 沉默匹配视图内容：无条目且无读数行 ⇒ 返回空串，**段整体缺席**（不是空标题）。
/// - 行形、区上限（[`CAP_HANDOFF_PREV`]）与截断复用 [`Section::push`]，不造第二把尺；
///   截断事实经 [`HandoffPrevSection::truncated`] 交回调用方（hook 面没有注入日志可记，
///   那一侧拿它走 stderr 诊断通道）。
///
/// 围栏不在这里加：本函数与 [`render_brief_with`] 同分工——渲染面只出裸行，逐行前缀
/// 由载体（`rc-agent::surface::brief_block`）统一落。
pub fn render_handoff_prev(
    items: &[HandoffItem],
    readings: &[String],
    now_ms: i64,
) -> HandoffPrevSection {
    let mut s = Section::new(SEC_HANDOFF_PREV, "[上一棒交接]", CAP_HANDOFF_PREV);
    for item in items {
        let text = one_line(&item.text);
        let line = match item.kind.as_str() {
            "continue" => format!(
                "- 接着做（{}写下，{}）：{text}",
                render_delta(now_ms, item.created_at_ms),
                render_due(now_ms, item.expires_at_ms)
            ),
            "due" => format!("- 到期（{}）：{text}", render_due(now_ms, item.expires_at_ms)),
            "reminder" => {
                format!("- 提醒（{}）：{text}", render_due(now_ms, item.expires_at_ms))
            }
            // `note` 只是备注，没有时间语义；kind 闭集（payload 校验）保证这里只剩 note。
            _ => format!("- 备注：{text}"),
        };
        s.push(Tag::Plain, line);
    }
    for reading in readings {
        s.push(Tag::Plain, one_line(reading));
    }
    if s.is_empty() {
        return HandoffPrevSection { text: String::new(), truncated: false };
    }
    HandoffPrevSection { text: s.render(), truncated: s.truncated }
}

/// [`render_handoff_prev`] 的结果。`text` 为空串 = 段缺席（沉默 ⇔ 空串，与注入面同一等式）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HandoffPrevSection {
    pub text: String,
    /// 区上限截断发生过（[`Section::push`] 的既有判据，原样交回）。
    pub truncated: bool,
}

/// 小本本索引栏（W36 细案 §4）：固定区「[小本本]」，行形
/// `- {id}｜{title}｜{tags 逗号}｜{相对时间}`，新的在前（`entries` 由
/// [`crate::CoreStore::notebook_index`] 给出，已按 `created_at_ms DESC` 排好）。
/// 区上限（[`CAP_NOTEBOOK`]）与截断照走 [`Section::push`]，不造第二把尺。
/// 无条目 ⇒ 区零行 ⇒ [`assemble`] 跳过空区 ⇒ 段整体缺席（沉默匹配视图内容）。
fn notebook_section(entries: &[NotebookEntry], now_ms: i64) -> Section {
    let mut s = Section::new(SEC_NOTEBOOK, "[小本本]", CAP_NOTEBOOK);
    for entry in entries {
        let title = one_line(&entry.title);
        let title = if title.is_empty() { "（无标题）" } else { title.as_str() };
        let tags = entry.tags.join(",");
        let line = format!(
            "- {}｜{}｜{}｜{}",
            one_line(&entry.id),
            title,
            tags,
            render_delta(now_ms, entry.created_at_ms)
        );
        s.push(Tag::Plain, line);
    }
    s
}

fn pinned_section(payload: &CorePayload) -> Section {
    let mut s = Section::new(SEC_PINNED, "[pinned 共同经历]", CAP_PINNED);
    // 只给指针 + 理由；正文要走 core_episode_expand。
    for pinned in &payload.pinned_episodes {
        let id = if pinned.episode_id.trim().is_empty() {
            "未命名".to_string()
        } else {
            one_line(&pinned.episode_id)
        };
        let reason = if pinned.pin_reason.trim().is_empty() {
            "（未写理由）".to_string()
        } else {
            one_line(&pinned.pin_reason)
        };
        s.push(Tag::Pinned, format!("- {id}：{reason}"));
    }
    s
}

fn recent_section(payload: &CorePayload, now_ms: i64) -> Section {
    let mut s = Section::new(SEC_RECENT, "[近期上下文]", CAP_RECENT);
    let i = &payload.interaction;
    if !i.current_topic.trim().is_empty() {
        s.push(Tag::Plain, format!("- 当前话题：{}", one_line(&i.current_topic)));
    }
    let last_contact = i.last_user_message_at_ms.max(i.last_agent_initiated_at_ms);
    if last_contact > 0 {
        s.push(Tag::Plain, format!("- 最近交流：{}", render_delta(now_ms, last_contact)));
    }
    s
}

fn build_sections(
    payload: &CorePayload,
    include: &[&str],
    notebook: &[NotebookEntry],
    now_ms: i64,
) -> Vec<Section> {
    vec![
        l0_section(payload),
        chinese_expression_section(payload),
        persona_section(payload),
        subject_section(payload),
        relationship_section(payload, now_ms),
        affect_section(payload, now_ms),
        loops_section(payload, now_ms),
        handoff_section(payload, include, now_ms),
        notebook_section(notebook, now_ms),
        pinned_section(payload),
        recent_section(payload, now_ms),
    ]
}

/// 头部元信息块。自我提醒（治理小刀②）：`last_write_at_ms` 给了（核心文档存在）就在
/// 头部行下多一行"核心记忆上次更新：X 天前"——让 agent 自己看见自己多久没写，不是替它写。
/// 相对时间走 [`render_delta`]（唯一实现住 rc-contract：两张面同一句话）。`None`
/// （无文档/从未更新）⇒ 一行不加，不造假数据。提醒行计入头部 token（它占的是常驻预算）。
fn header(
    payload: &CorePayload,
    user_id: &str,
    scene: &str,
    last_write_at_ms: Option<i64>,
    now_ms: i64,
) -> String {
    let agent = if payload.agent_id.trim().is_empty() {
        "未指定".to_string()
    } else {
        clip(&payload.agent_id, 64)
    };
    let user = if user_id.trim().is_empty() {
        "未指定".to_string()
    } else {
        clip(user_id, 64)
    };
    let scene = if scene.trim().is_empty() {
        "未指定".to_string()
    } else {
        clip(scene, 32)
    };
    let mut out = format!("[常驻简报] agent={agent} · 用户={user} · 场景={scene}");
    if let Some(at) = last_write_at_ms {
        out.push_str("\n核心记忆上次更新：");
        out.push_str(&render_delta(now_ms, at));
    }
    out
}

fn assemble(header: &str, sections: &[Section]) -> String {
    let mut out = String::from(header);
    for section in sections.iter().filter(|s| !s.is_empty()) {
        out.push('\n');
        out.push('\n');
        out.push_str(&section.render());
    }
    out
}

fn evict_pinned(sections: &mut [Section]) -> Option<&'static str> {
    for section in sections.iter_mut() {
        if let Some(pos) = section.lines.iter().rposition(|l| l.tag == Tag::Pinned) {
            section.lines.remove(pos);
            return Some(SEC_PINNED);
        }
    }
    None
}

fn evict_loop(sections: &mut [Section]) -> Option<&'static str> {
    for section in sections.iter_mut() {
        if let Some(pos) = section.lines.iter().rposition(|l| l.tag == Tag::Loop) {
            section.lines.remove(pos);
            return Some(SEC_LOOPS);
        }
    }
    None
}

fn evict_low_confidence_affect(sections: &mut [Section]) -> Option<&'static str> {
    let mut target: Option<(usize, usize, f64)> = None;
    for (si, section) in sections.iter().enumerate() {
        for (li, line) in section.lines.iter().enumerate() {
            if let Tag::Affect { confidence } = line.tag {
                if confidence < 0.5 && target.is_none_or(|(_, _, c)| confidence < c) {
                    target = Some((si, li, confidence));
                }
            }
        }
    }
    let (si, li, _) = target?;
    sections[si].lines.remove(li);
    Some(SEC_AFFECT)
}

fn evict_preference(sections: &mut [Section]) -> Option<&'static str> {
    for section in sections.iter_mut() {
        if let Some(pos) = section.lines.iter().rposition(|l| l.tag == Tag::Preference) {
            section.lines.remove(pos);
            return Some(SEC_SUBJECT);
        }
    }
    None
}

/// 兜底：按渲染顺序的反向丢最后一行（近期 → pinned → … → persona）。
/// **L0 永不淘汰**：它是安全/身份底线，预算再小也整段保留；生效预算因此有一个
/// `header + L0` 的下限（见 [`render_brief`]）。
fn hard_trim(sections: &mut [Section]) -> Option<&'static str> {
    for section in sections.iter_mut().rev() {
        if section.key == SEC_L0 {
            continue;
        }
        if section.lines.pop().is_some() {
            section.truncated = true;
            return Some(section.key);
        }
    }
    None
}

fn evict_once(sections: &mut [Section]) -> Option<&'static str> {
    if let Some(key) = evict_pinned(sections) {
        return Some(key);
    }
    if let Some(key) = evict_loop(sections) {
        return Some(key);
    }
    if let Some(key) = evict_low_confidence_affect(sections) {
        return Some(key);
    }
    if let Some(key) = evict_preference(sections) {
        return Some(key);
    }
    hard_trim(sections)
}

/// 渲染常驻简报。`budget == 0` 用 [`DEFAULT_BUDGET`]，超过也按它封顶。
/// 交接段用 [`DEFAULT_INCLUDE`]（接着做 + 到期）；要别的种类走 [`render_brief_with`]。
///
/// **L0 永不淘汰**：如果调用方给的预算连头部 + L0 都放不下，生效预算会抬到
/// `header + L0`（另加分隔符余量），因此 `token_estimate` 的唯一例外是这一格。
pub fn render_brief(
    payload: &CorePayload,
    user_id: &str,
    scene: &str,
    budget: usize,
    now_ms: i64,
) -> Brief {
    render_brief_with(payload, user_id, scene, budget, &DEFAULT_INCLUDE, now_ms)
}

/// 同 [`render_brief`]，但 `include` 决定交接段列哪几种 kind。
/// 它是**种类过滤器**，不是"全都要"：时间窗口规则对 `due`/`reminder` 仍然生效。
/// 小本本索引栏（W36）以空 vec 委托 [`render_brief_with_notebook`]——本函数的输出
/// 与 W36 之前逐字节相同（既有渲染用例不破，「测试名可能被文档引用」纪律）。
pub fn render_brief_with(
    payload: &CorePayload,
    user_id: &str,
    scene: &str,
    budget: usize,
    include: &[&str],
    now_ms: i64,
) -> Brief {
    render_brief_with_notebook(payload, user_id, scene, budget, include, &[], None, now_ms)
}

/// 同 [`render_brief_with`]，另收小本本索引行（[`crate::CoreStore::notebook_index`] 的
/// 返回形状，新的在前）。`payload_is_blank` 的语义在此扩一格：**有 notebook 行也算非空**——
/// 否则只写了小本本的新 agent 永远只出头部（同 handoff 当年那格）。其余逐字相同：
/// 区上限/淘汰/注入读数都不因这个参数而变。
///
/// `last_write_at_ms`（治理小刀②）：核心文档的"上次更新"时刻，`Some` ⇒ 头部多一行
/// "核心记忆上次更新：X 天前"；`None` ⇒ 一行不加。取数家在 [`crate::CoreStore::brief_with`]。
#[allow(clippy::too_many_arguments)]
pub fn render_brief_with_notebook(
    payload: &CorePayload,
    user_id: &str,
    scene: &str,
    budget: usize,
    include: &[&str],
    notebook: &[NotebookEntry],
    last_write_at_ms: Option<i64>,
    now_ms: i64,
) -> Brief {
    let budget = match budget {
        0 => DEFAULT_BUDGET,
        b => b.min(DEFAULT_BUDGET),
    };
    if payload_is_blank(payload) && notebook.is_empty() {
        let mut brief = Brief::empty();
        brief.text = header(payload, user_id, scene, last_write_at_ms, now_ms);
        brief.token_estimate = estimate_tokens(&brief.text);
        if brief.token_estimate > budget {
            brief.text = truncate_to_fit(&brief.text, budget).unwrap_or_default();
            brief.token_estimate = estimate_tokens(&brief.text);
        }
        return brief;
    }

    let head = header(payload, user_id, scene, last_write_at_ms, now_ms);
    let mut sections = build_sections(payload, include, notebook, now_ms);
    // L0 是底线：预算再小也要整段留下。加一点分隔符余量，避免估算边界把最后一行挤掉。
    let l0_tokens = sections
        .iter()
        .find(|s| s.key == SEC_L0)
        .map(|s| estimate_tokens(&s.render()))
        .unwrap_or(0);
    let budget = budget.max(estimate_tokens(&head) + l0_tokens + 8);
    let mut truncated: Vec<&'static str> = Vec::new();
    for section in &sections {
        if section.truncated {
            mark(&mut truncated, section.key);
        }
    }

    let mut text = assemble(&head, &sections);
    // 每次淘汰都永久少一行，因此循环必然终止。
    while estimate_tokens(&text) > budget {
        let Some(key) = evict_once(&mut sections) else {
            break;
        };
        mark(&mut truncated, key);
        text = assemble(&head, &sections);
    }
    if estimate_tokens(&text) > budget {
        // 走到这里说明所有分区都已空，只剩头部；预算小到连头部都放不下。
        text = truncate_to_fit(&text, budget).unwrap_or_default();
    }

    let mut section_tokens = BTreeMap::new();
    for section in sections.iter().filter(|s| !s.is_empty()) {
        section_tokens.insert(section.key, section.tokens());
    }
    Brief {
        token_estimate: estimate_tokens(&text),
        text,
        section_tokens,
        truncated,
    }
}

/// 全默认载荷：没有任何区值得渲染，只留头部。
///
/// 未关闭的交接条也算「有内容」——否则新 agent 只挂了一条便签时，简报只剩头部，
/// 交接段永远没机会出现。
fn payload_is_blank(payload: &CorePayload) -> bool {
    let p = &payload.persona;
    let no_subject = payload.subjects.iter().all(|s| {
        s.profile_text.trim().is_empty()
            && s.preferences.is_empty()
            && s.triggers.is_empty()
            && s.taboos.is_empty()
            && s.subject_id.trim().is_empty()
    });
    let i = &payload.interaction;
    let r = &payload.relationship;
    payload.l0.iter().all(|s| s.trim().is_empty())
        && p.core_text.trim().is_empty()
        && p.traits.is_empty()
        && p.style.is_empty()
        && p.quirks.is_empty()
        && p.boundaries.is_empty()
        && p.voice_examples.iter().all(|s| s.trim().is_empty())
        && no_subject
        && r.text.trim().is_empty()
        && (r.trust - 0.5).abs() < f64::EPSILON
        && (r.intimacy - 0.5).abs() < f64::EPSILON
        && r.tension == 0.0
        && r.status == "normal"
        && i.last_user_message_at_ms <= 0
        && i.last_agent_initiated_at_ms <= 0
        && i.unanswered_user_messages == 0
        && i.attention_debt_ms <= 0
        && i.current_topic.trim().is_empty()
        && payload.affects.is_empty()
        && payload.loops.iter().all(|l| l.status != "open" || l.text.trim().is_empty())
        && payload.pinned_episodes.iter().all(|p| p.episode_id.trim().is_empty())
        && payload.handoffs.iter().all(|h| h.closed_at_ms.is_some())
        && !payload.chinese_expression.enabled
}
