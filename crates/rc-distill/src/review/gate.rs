//! 程序闸本体（设计 §5）：载荷形状 → 轮存在 → 会话/设备 → 墓碑 → 压缩块 →
//! 引用锚 → 主语在证据里 → 引文/数字 → 闭集（fact）/ naming / time。
//!
//! 纪律：
//! - **纯函数**：不打印、不写库、不读钟；`now_ms` 由调用方给。
//! - 判定顺序固定，返回**第一个**失败码；同一输入两次结果一致。
//! - 解析失败不是 `Err`：`payload_shape` 是判定码。只有 SQL/句柄错误才 `Err`。
//! - 码只用静态字符串，错误信息与码里绝不带载荷正文、路径、turn_uid 原文（守卫 12）。

use std::collections::{BTreeSet, HashMap};

use rc_memory::review::Item;
use rc_memory::{owner, Scope};
use rc_store::{MemoryHandle, TranscriptHandle};
use serde_json::Value;

use crate::pass_fact::family_of_slug;
use crate::pass_fact_contract::{digit_runs, MAX_QUOTE_CHARS};
use crate::pass_prompt::{is_compaction_body, FactFamily, EVENT_CATEGORIES, EVENT_MODE_STATED};
use crate::prepare::{evidence_view, prepare};
use crate::DistillError;

/// 本闸门的规则版本。写进 `review_decision.rules_version` 时用它。
///
/// I2 起 6c 对 human 轮的行为随会话身份分成两半（有 claim 收紧 + 新增"主语锚 == 主人锚"），
/// 判定函数变了，版本跟着走（v0.1 → v0.2 是加 6c 本身时的先例）。
///
/// v0.3 → v0.4（C1）：两处判定变了——① 6c 第 2 支的名字命中加词边界（短别名/单字名不再
/// 命中任意正文）；② 第 5 支的身份轴从 `owner.owner_agent_id` 统一到 `batch.source_agent`
/// （与提示词、会话自锚同一列）。同一载荷在 v0.3/v0.4 下可能得出不同结论，版本必须走。
///
/// v0.4 → v0.5（C4b）：naming 9c 的判据从"该批任一轮"收到"**该锚自己的**证据轮"
/// （canonical/alias 的逐字证据；失败码仍是 `not_verbatim`，码位与码序不动）。
/// 同一载荷在 v0.4/v0.5 下可能得出不同结论，版本必须走。
///
/// v0.5 → v0.6（A-rebind）：event 的 6c 多一格**放行**——证据轮全是 agent 轮、模型选的
/// actor 不在正文里、且候选池里唯一存在本会话 agent 锚时，actor 由写侧回填成自锚
/// （[`event_actor_rebind`]）。同一载荷在 v0.5/v0.6 下可能得出不同结论，版本必须走。
/// v0.6 → v0.7（P2）：多一条 **pass** 的判定分支（`state`）。既有四族（fact/event/naming/time）
/// 的判据与码序一个字都没动；变的是"state 载荷今天有判定，昨天落在 `payload_shape`"。
///
/// v0.8 → v0.9（P0-2）：naming 尾部多两格 **person 硬约束**（9e 人类证据 / 9f 服务账号形状），
/// 判据在 9a–9d 之后、码序不动。同一载荷在 v0.8/v0.9 下可能得出不同结论（旧版落库、新版拒收），
/// 版本必须走。
/// v0.9 → v0.10（W14d）：9e 那一格从闸里撤掉，改由写库层降级（锚照写、现实性落 unknown、
/// 留审计行）；9f 的判据与码一字未动。同一份"只有 agent 轮作证据的 person"载荷在
/// v0.9 下拒收、v0.10 下落库，版本必须走。
pub const GATE_RULES_VERSION: &str = "review-gate-v0.10";

/// 第 6 步的第三个码：引用锚存在、未退场，但**对该行 owner 不可见**。
///
/// 前两个码只说明"库里有这个锚"；这一码说明"这一行不该看见它"。没有这一格时，
/// 行 owner=qoder 的 fact 可以把另一台设备的私有锚选成主语而闸门照样放行——那是
/// 跨 owner 的实体泄露 + 错误归属。判定复用 [`rc_memory::owner::anchor_visible`]
/// （与候选池同一份谓词），不在这里另写一套 visibility 规则。
pub const ANCHOR_NOT_VISIBLE: &str = "anchor_not_visible";

/// 第 6 步的第四个码：主语锚存在、存活且可见，但它的名字不在这一行的证据里。
///
/// 前三个码只说明"库里有这个锚、这一行看得见它"；这一码说明"这段证据说的是它"。
/// 第 1 天 dogfood 里模型把 human 轮的主语写成编造的名字（"张三"/"qoder"），两审都放过，
/// 就是因为没有这一格。114 条已标注草稿上实测：94 条 `supported` 全部满足本规则
/// （零误杀），判失败的 12 条全部是 human 轮（other 6 / wrong_subject 3 / unsupported 2 /
/// condition_dropped 1）。
///
/// **身份缺口（I1 登记，I2 收紧一半）**：human 轮此前没有 person 锚，
/// `mention.speaker_anchor_id` 的说话人逃逸口对 human 轮不生效，本规则曾对 human 轮
/// 一律 fail-open；I2 起有 claim 的会话按常规判（多"主语锚 == 主人锚"一支），没有 claim
/// （或章是 `unknown`）的会话仍是 fail-open——见 [`KnownPrincipal`]。
pub const SUBJECT_NOT_IN_EVIDENCE: &str = "subject_not_in_evidence";

/// P2 状态 pass 的**冲突**码：同主体同维度已经有一条活着的行，而这一条给的是**另一个值**。
///
/// 为什么不是 `reject`：两条可能都对（状态会变），只是不能由抽取侧决定哪条算数——
/// 覆盖语义（方案 §4）是"不覆盖旧值"，所以 runner 把命中这一码的轮落
/// `quarantined(state_conflict_same_dim)` 而不是 `rejected`：既不写第二行，也不冒充坏了。
///
/// 接在既有码之后、只在 state 分支里判：fact/event/naming/time 的遍历顺序与码序不受影响。
pub const STATE_CONFLICT_CODE: &str = "state_conflict_same_dim";

/// D19 的理由码（读数见台账 §10.133）：human 轮的"我"被挂到了**本会话 agent 的自锚**上。
/// 单独一个码，是为了让它成为分母上的数——今天库里那 7 行是静默写进去的。
pub const SUBJECT_SELF_ON_HUMAN_CODE: &str = "subject_self_on_human_turn";

/// 一次闸门判定的读数。`code` 只在失败时有值，且是下面闭集里的一个静态词。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateOutcome {
    pub passed: bool,
    pub code: Option<&'static str>,
    pub turns_checked: usize,
    pub items_checked: usize,
}

/// 单条 draft 的判定（I3 第一刀：判定单位是 draft，不是整批）。
///
/// `reference` 是**位置**引用（fact `f1..fN`、event `e1..eN`、naming 锚 `a1..aN`、
/// time `t1..tN`），与审核 prompt 的编号、`review_detail` 的 ref 同一套。
/// `turn_uids` 是这条 draft 的证据轮——批级拒绝时只标**这些**轮，不是整批。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftGate {
    pub reference: String,
    /// `None` = 这条 draft 通过；否则是本节闭集里的第一个失败码（同一 draft 多错只记第一个）。
    pub code: Option<&'static str>,
    pub turn_uids: Vec<String>,
}

/// 一次闸门判定的完整读数：**批级前置** + **逐 draft**。
///
/// - `batch_code` 是批级前置（`payload_shape` / `turn_missing` / `turn_session_mismatch` /
///   `turn_tombstoned` / `compaction_evidence`）：命中即整批不可用，`drafts` 为空。
/// - `code` 是**与旧口径逐字兼容**的"第一个失败码"：批级码优先，否则按今天的遍历顺序
///   （阶段为主序、draft 为次序）取第一个失败 draft 的码。`gate()` 仍只暴露这一格。
/// - `drafts` 是逐条判定，码序与旧实现同一条：同一 draft 多个失败只记第一个。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateReport {
    pub batch_code: Option<&'static str>,
    pub code: Option<&'static str>,
    pub turns_checked: usize,
    pub items_checked: usize,
    pub drafts: Vec<DraftGate>,
}

impl GateReport {
    /// 没有任何失败（批级与全部 draft）。
    pub fn passed(&self) -> bool {
        self.code.is_none() && self.batch_code.is_none()
    }

    /// 失败的 draft（保持载荷顺序）。
    pub fn failed(&self) -> impl Iterator<Item = &DraftGate> {
        self.drafts.iter().filter(|draft| draft.code.is_some())
    }

    /// 全部 draft 都失败（批级前置没命中时才有意义）：没有一条好结果可保，整批 rejected。
    pub fn all_failed(&self) -> bool {
        !self.drafts.is_empty() && self.drafts.iter().all(|draft| draft.code.is_some())
    }

    /// 失败 draft 的证据轮 `(turn_uid, 码)`：调用方按它逐轮落 `rejected(<码>)`。
    ///
    /// 两条口径：
    /// - **被通过 draft 覆盖的轮不列入**——同一轮被多条 draft 覆盖时，只要有任一条通过，
    ///   该轮就不是"坏草稿的轮"；
    /// - 同一轮只留第一个码（draft 顺序），与"同一 draft 多个失败只记第一个"同一条纪律。
    pub fn rejected_turns(&self) -> Vec<(String, &'static str)> {
        let covered: BTreeSet<&str> = self
            .drafts
            .iter()
            .filter(|draft| draft.code.is_none())
            .flat_map(|draft| draft.turn_uids.iter().map(String::as_str))
            .collect();
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut out: Vec<(String, &'static str)> = Vec::new();
        for draft in self.drafts.iter().filter(|draft| draft.code.is_some()) {
            let Some(code) = draft.code else {
                continue;
            };
            for uid in &draft.turn_uids {
                if covered.contains(uid.as_str()) || !seen.insert(uid.as_str()) {
                    continue;
                }
                out.push((uid.clone(), code));
            }
        }
        out
    }
}

/// `anchor.kind` 的 DDL 九值闭集（`rc-store/src/ddl.rs` 的 CHECK 逐字对拍）。
const ANCHOR_KINDS: [&str; 9] = [
    "person", "agent", "org", "place", "object", "animal", "work", "event", "concept",
];

/// `time_expr.kind` 的 DDL 五值闭集。
const TIME_KINDS: [&str; 5] = [
    "event_time", "speech_time", "validity", "deadline", "recurrence",
];

/// 人的锚就是 `person`（与 `writers_v4` 的名字锚/主人锚同一格）。9f 只对这一个 kind 生效。
const PERSON_KIND: &str = "person";

// 9e 已移交写库层降级（W14d）：agent 轮转述的人名照写，只把现实性落成未知。
// 闸里不再判"有没有人自己开口作证据"这一格——拒收会切断整条转述路径。

/// 9f：canonical 在它自己的证据轮里以**服务账号的形状**出现（反引号令牌 / 与数字标识
/// 同行相邻 / `key: value` 的值）。
///
/// 独立于 9e：有人的轮做证据也不足以让它成为人——"我的 B 站叫某某（UID 123…）"说的是
/// **账号**，不是人。账号昵称、电脑用户名/主机名、第三方服务用户名都归环境/设备，
/// person 只有一个，所以这一支自己判、照旧拒收。
pub const SERVICE_ACCOUNT_SHAPE: &str = "service_account_shape";

/// event 的谓词由程序盖（与 `pass_event_contract` 的 `predicate=action` 同一处真相）。
const EVENT_PREDICATE: &str = "action";

/// P2 状态的谓词格。`state_assertion` 表没有 predicate 列，这一格只活在**载荷**里，
/// 作用是让闸知道"这条行说的是状态"而不是借来的 fact 形状（与 `EVENT_PREDICATE` 同档）。
const STATE_PREDICATE: &str = "state";

/// 时间窗：2000-01-01 .. 2100-01-01（UTC 毫秒，左闭右开）。
const TIME_MIN_MS: i64 = 946_684_800_000;
const TIME_MAX_MS: i64 = 4_102_444_800_000;

fn fail(code: &'static str, turns: usize, items: usize) -> GateOutcome {
    GateOutcome {
        passed: false,
        code: Some(code),
        turns_checked: turns,
        items_checked: items,
    }
}

fn pass(turns: usize, items: usize) -> GateOutcome {
    GateOutcome {
        passed: true,
        code: None,
        turns_checked: turns,
        items_checked: items,
    }
}

// ── 载荷形状（解析成功才进入判定；失败一律 `payload_shape`）────────────────────

#[derive(Debug)]
struct Evidence {
    turn_uid: String,
}

#[derive(Debug)]
struct FactItem {
    subject_anchor_id: String,
    object_anchor_id: Option<String>,
    predicate: String,
    category: String,
    mode: String,
    quote: String,
    evidence: Vec<Evidence>,
    text: String,
    /// C2：时间短语原文。这里只用来做「接地」判定，抽取/写入仍走契约那一路。
    time_text: String,
}

#[derive(Debug)]
struct EventExtras {
    participants: Vec<String>,
    place_anchor_id: Option<String>,
}

#[derive(Debug)]
struct NamingAnchor {
    /// 同批锚的短引用（`reference` 或 `ref`，两者都收）。mention 的 `speaker_ref`
    /// 指向同批引用时视为通过，不去 `anchor` 表查。
    reference: Option<String>,
    kind: String,
    canonical: String,
    aliases: Vec<String>,
    evidence: Vec<Evidence>,
}

#[derive(Debug)]
struct NamingMention {
    surface: String,
    turn_uid: String,
    /// 该提及指向的同批锚（`a1..aN`）：mention 级的失败（说话人可解析、span 越界）归到
    /// **这一条锚**上——判定单位是锚，不是整批。
    anchor_ref: Option<String>,
    speaker_ref: Option<String>,
    span_start: Option<i64>,
    span_end: Option<i64>,
}

#[derive(Debug)]
struct NamingItems {
    anchors: Vec<NamingAnchor>,
    mentions: Vec<NamingMention>,
}

#[derive(Debug)]
struct TimeItem {
    turn_uid: String,
    start_ms: Option<i64>,
    end_ms: Option<i64>,
    kind: String,
}

#[derive(Debug)]
enum Items {
    Fact {
        family: FactFamily,
        items: Vec<FactItem>,
    },
    Event {
        items: Vec<FactItem>,
        extras: Vec<EventExtras>,
    },
    Naming(NamingItems),
    Time(Vec<TimeItem>),
    /// P2 状态行。`values` 与 `items` 同下标：`FactItem` 没有"值"这一格（fact 的值在
    /// `value_json` 里，闸不读），而状态的冲突判据必须读到 `value_after`。
    State {
        items: Vec<FactItem>,
        values: Vec<String>,
    },
}

#[derive(Debug)]
struct Payload {
    batch_turns: Vec<String>,
    /// 行 owner 的 agent 与 scope：第 6 步判"引用的锚对这一行可不可见"的读者身份。
    /// 与 `parse_payload` 里 `owner` 块逐字同源，不另读一次。
    owner_agent_id: String,
    /// 会话的 `source_agent`（`sessions.source_agent`，经 `payload.batch` 递进来）。
    ///
    /// **身份轴（C1）**：这一列是"本 agent 是谁"的权威值——提示词里的说话人、会话自锚的
    /// canonical（`ensure_agent_anchor`）都用它。`owner_agent_id` 是**归属/可见性**轴
    /// （`resource_owner.owner_agent_id`），两者可以不同（会话声明的 owner 是另一个 agent）。
    /// 6c 第 5 支判"主语 == 本 agent 锚"读这一列，不读 `owner_agent_id`。
    source_agent: String,
    owner_scope: Scope,
    items: Items,
}

impl Payload {
    fn item_count(&self) -> usize {
        match &self.items {
            Items::Fact { items, .. } | Items::Event { items, .. } => items.len(),
            Items::Naming(naming) => naming.anchors.len() + naming.mentions.len(),
            Items::Time(items) => items.len(),
            Items::State { items, .. } => items.len(),
        }
    }

    /// 载荷里被引用/引文的轮：fact/event 的 evidence、naming 的锚证据与 mention、
    /// time 的每条 `turn_uid`。顺序固定（条目顺序，去重由集合负责）。
    fn referenced_turns(&self) -> Vec<&str> {
        let mut out = Vec::new();
        match &self.items {
            Items::Fact { items, .. } | Items::Event { items, .. } => {
                for item in items {
                    out.extend(item.evidence.iter().map(|e| e.turn_uid.as_str()));
                }
            }
            Items::Naming(naming) => {
                for anchor in &naming.anchors {
                    out.extend(anchor.evidence.iter().map(|e| e.turn_uid.as_str()));
                }
                out.extend(naming.mentions.iter().map(|m| m.turn_uid.as_str()));
            }
            Items::Time(items) => {
                out.extend(items.iter().map(|item| item.turn_uid.as_str()));
            }
            Items::State { items, .. } => {
                for item in items {
                    out.extend(item.evidence.iter().map(|e| e.turn_uid.as_str()));
                }
            }
        }
        out
    }
}

fn req_str(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    Some(map.get(key)?.as_str()?.to_string())
}

fn opt_str(map: &serde_json::Map<String, Value>, key: &str) -> Option<Option<String>> {
    match map.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(s)) => Some(Some(s.clone())),
        _ => None,
    }
}

fn is_turn_uid(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn parse_evidence(value: &Value) -> Option<Vec<Evidence>> {
    value
        .as_array()?
        .iter()
        .map(|entry| {
            let obj = entry.as_object()?;
            obj.get("anchor_role")?.as_str()?;
            Some(Evidence {
                turn_uid: obj.get("turn_uid")?.as_str()?.to_string(),
            })
        })
        .collect()
}

fn parse_fact_items(value: &Value) -> Option<Vec<FactItem>> {
    value
        .as_array()?
        .iter()
        .map(|entry| {
            let obj = entry.as_object()?;
            // `value` 只校验形状（可空）；`time_text` 还要留下原文，供第 9 步做接地判定。
            opt_str(obj, "value")?;
            Some(FactItem {
                subject_anchor_id: req_str(obj, "subject_anchor_id")?,
                object_anchor_id: opt_str(obj, "object_anchor_id")?,
                predicate: req_str(obj, "predicate")?,
                category: req_str(obj, "category")?,
                mode: req_str(obj, "mode")?,
                quote: req_str(obj, "quote")?,
                evidence: parse_evidence(obj.get("evidence")?)?,
                text: req_str(obj, "text")?,
                time_text: opt_str(obj, "time_text")?.unwrap_or_default(),
            })
        })
        .collect()
}

fn parse_extras(value: &Value, expected: usize) -> Option<Vec<EventExtras>> {
    let array = value.as_array()?;
    if array.len() != expected {
        return None;
    }
    array
        .iter()
        .map(|entry| {
            let obj = entry.as_object()?;
            // `value_json` 是任意 JSON（含 null），形状上不设闭集；本闸门不消费它。
            obj.get("value_json")?;
            let participants = obj
                .get("participants")?
                .as_array()?
                .iter()
                .map(|p| Some(p.as_str()?.to_string()))
                .collect::<Option<Vec<_>>>()?;
            Some(EventExtras {
                participants,
                place_anchor_id: opt_str(obj, "place_anchor_id")?,
            })
        })
        .collect()
}

fn parse_naming(value: &Value) -> Option<NamingItems> {
    let obj = value.as_object()?;
    let anchors = obj
        .get("anchors")?
        .as_array()?
        .iter()
        .map(|entry| {
            let a = entry.as_object()?;
            let reference = match a.get("reference").or_else(|| a.get("ref")) {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.clone()),
                _ => return None,
            };
            // `reality_status` / `stable` 只校验形状；本闸门不消费它们。
            a.get("reality_status")?.as_str()?;
            a.get("stable")?.as_bool()?;
            let aliases = a
                .get("aliases")?
                .as_array()?
                .iter()
                .map(|alias| Some(alias.as_str()?.to_string()))
                .collect::<Option<Vec<_>>>()?;
            Some(NamingAnchor {
                reference,
                kind: req_str(a, "kind")?,
                canonical: req_str(a, "canonical")?,
                aliases,
                evidence: parse_evidence(a.get("evidence")?)?,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let mentions = obj
        .get("mentions")?
        .as_array()?
        .iter()
        .map(|entry| {
            let m = entry.as_object()?;
            // `discourse_role` / `resolution` / `confidence` 只校验形状。
            m.get("discourse_role")?.as_str()?;
            m.get("resolution")?.as_str()?;
            m.get("confidence")?.as_f64()?;
            let span_start = match m.get("span_start") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_i64()?),
            };
            let span_end = match m.get("span_end") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_i64()?),
            };
            // 两个 span 要么都在、要么都不在；只有一个键是形状错。
            if span_start.is_some() != span_end.is_some() {
                return None;
            }
            Some(NamingMention {
                surface: req_str(m, "surface")?,
                turn_uid: req_str(m, "turn_uid")?,
                anchor_ref: opt_str(m, "anchor_ref")?,
                speaker_ref: opt_str(m, "speaker_ref")?,
                span_start,
                span_end,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(NamingItems { anchors, mentions })
}

fn parse_time_items(value: &Value) -> Option<Vec<TimeItem>> {
    value
        .as_array()?
        .iter()
        .map(|entry| {
            let obj = entry.as_object()?;
            // `text` / `precision` / `basis` 只校验形状；本闸门不消费它们。
            obj.get("text")?.as_str()?;
            obj.get("precision")?.as_str()?;
            obj.get("basis")?.as_str()?;
            let start_ms = match obj.get("start_ms") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_i64()?),
            };
            let end_ms = match obj.get("end_ms") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_i64()?),
            };
            Some(TimeItem {
                turn_uid: req_str(obj, "turn_uid")?,
                start_ms,
                end_ms,
                kind: req_str(obj, "kind")?,
            })
        })
        .collect()
}

/// P2 状态载荷的逐条解析：`(闸用的行, value_after)` 两列同下标返回。
///
/// 键的必需性与其余 pass 同一条纪律：读不出形状就是 `payload_shape`（批级），不猜默认值。
fn parse_state_items(value: &Value) -> Option<(Vec<FactItem>, Vec<String>)> {
    let mut items: Vec<FactItem> = Vec::new();
    let mut values: Vec<String> = Vec::new();
    for entry in value.as_array()?.iter() {
        let obj = entry.as_object()?;
        let evidence = parse_evidence(obj.get("evidence")?)?;
        // `subject_canonical` 只校验形状（渲染归 replay 那一侧），闸不消费它。
        obj.get("subject_canonical")?.as_str()?;
        let dimension = req_str(obj, "dimension")?;
        let value_after = req_str(obj, "value_after")?;
        items.push(FactItem {
            subject_anchor_id: req_str(obj, "subject_anchor_id")?,
            object_anchor_id: None,
            predicate: req_str(obj, "predicate")?,
            category: dimension,
            mode: req_str(obj, "mode")?,
            quote: req_str(obj, "quote")?,
            evidence,
            text: req_str(obj, "text")?,
            // 状态的日期归 time pass（方案 §2"不做时间推断"），载荷里没有短语这一格。
            time_text: String::new(),
        });
        values.push(value_after);
    }
    Some((items, values))
}

fn parse_payload(item: &Item) -> Option<Payload> {
    let root: Value = serde_json::from_str(&item.payload_json).ok()?;
    let obj = root.as_object()?;
    if obj.get("schema")?.as_str()? != "review.pass.v1" {
        return None;
    }
    let pass_id = obj.get("pass_id")?.as_str()?;
    if pass_id != item.meta.pass_id {
        return None;
    }
    let family = obj.get("family")?.as_str()?;
    if family != item.meta.family {
        return None;
    }
    // family 只对 fact 取值；其余中心 pass 必须空串（与 `extract_pass_state` 同口径）。
    if pass_id != "fact" && !family.is_empty() {
        return None;
    }
    obj.get("run_id")?.as_str()?;

    let batch = obj.get("batch")?.as_object()?;
    let turn_values = batch.get("turn_uids")?.as_array()?;
    let mut batch_turns = Vec::with_capacity(turn_values.len());
    for value in turn_values {
        let uid = value.as_str()?;
        if !is_turn_uid(uid) {
            return None;
        }
        batch_turns.push(uid.to_string());
    }
    let source_agent = batch.get("source_agent")?.as_str()?.to_string();
    batch.get("device_id")?.as_str()?;
    batch.get("session_key")?.as_str()?;
    batch.get("source_ms")?.as_i64()?;

    let owner = obj.get("owner")?.as_object()?;
    if !matches!(
        owner.get("owner_principal_id"),
        Some(Value::Null) | Some(Value::String(_))
    ) {
        return None;
    }
    let owner_agent_id = owner.get("owner_agent_id")?.as_str()?;
    if owner_agent_id.trim().is_empty() {
        return None;
    }
    owner.get("visibility")?.as_str()?;
    owner.get("retention")?.as_str()?;
    let owner_scope = Scope {
        kind: owner::normalize_scope_kind(owner.get("scope_kind")?.as_str()?),
        key: owner.get("scope_key")?.as_str()?.to_string(),
    };

    let items_value = obj.get("items")?;
    let items = match pass_id {
        "fact" => Items::Fact {
            family: family_of_slug(family)?,
            items: parse_fact_items(items_value)?,
        },
        "event" => {
            let items = parse_fact_items(items_value)?;
            let extras = parse_extras(obj.get("extras")?, items.len())?;
            Items::Event { items, extras }
        }
        "naming" => Items::Naming(parse_naming(items_value)?),
        "time" => Items::Time(parse_time_items(items_value)?),
        "state" => {
            let (items, values) = parse_state_items(items_value)?;
            Items::State { items, values }
        }
        // 五个中心 pass 之外的 pass（例如 `rows`）不是本闸门的输入。
        _ => return None,
    };

    // `item.turn_uids_json` 与 `batch.turn_uids` 排序后逐项相等（含重复项）。
    let item_turns: Vec<String> = serde_json::from_str(&item.turn_uids_json).ok()?;
    let mut left = batch_turns.clone();
    left.sort();
    let mut right = item_turns;
    right.sort();
    if left != right {
        return None;
    }

    Some(Payload {
        batch_turns,
        owner_agent_id: owner_agent_id.to_string(),
        source_agent,
        owner_scope,
        items,
    })
}

// ── 判定 ──────────────────────────────────────────────────────────────────────

fn turn_body_and_role(t: &TranscriptHandle, turn_uid: &str)
    -> Result<Option<(String, String, i64)>, DistillError> {
    let mut st = t.conn().prepare("SELECT body, role, created_at_ms FROM turns WHERE turn_uid=?1")?;
    let mut rows = st.query([turn_uid])?;
    match rows.next()? {
        None => Ok(None),
        Some(row) => Ok(Some((row.get(0)?, row.get(1)?, row.get(2)?))),
    }
}

/// 锚存在且未退场。`None` = 通过；否则是 `ref_missing` / `ref_retired`。
fn check_anchor(m: &MemoryHandle, anchor_id: &str) -> Result<Option<&'static str>, DistillError> {
    let mut st = m.conn().prepare("SELECT status FROM anchor WHERE id=?1")?;
    let mut rows = st.query([anchor_id])?;
    match rows.next()? {
        None => Ok(Some("ref_missing")),
        Some(row) => {
            let status: String = row.get(0)?;
            if matches!(status.as_str(), "active" | "provisional") {
                Ok(None)
            } else {
                // DDL 闭集只有 active|provisional|merged|retired；非前两者即退场。
                Ok(Some("ref_retired"))
            }
        }
    }
}

/// 锚对行 owner 可见。与候选池 [`crate::candidates`] 共用
/// [`rc_memory::owner::anchor_visible`] 这一份谓词：候选表里出得来的锚，闸门一定放行；
/// 候选表里没有的私有锚，即使模型伪造 id 也过不去。
fn check_anchor_visible(
    m: &MemoryHandle,
    anchor_id: &str,
    owner_agent_id: &str,
    owner_scope: &Scope,
) -> Result<Option<&'static str>, DistillError> {
    if rc_memory::owner::anchor_visible(m, anchor_id, owner_agent_id, owner_scope)? {
        Ok(None)
    } else {
        Ok(Some(ANCHOR_NOT_VISIBLE))
    }
}

fn is_pronoun(name: &str) -> bool {
    let trimmed = name.trim();
    rc_memory::PRONOUNS.contains(&trimmed)
        || rc_memory::PRONOUNS_ASCII.contains(&trimmed.to_lowercase().as_str())
}

/// 名字两侧的"账号标记"：它们说明正文在描述一个**服务端标识**，不是在称呼一个人。
///
/// 只认这些通用词与 UID/ID 两个 ASCII 词（后者按词边界匹配，不撞 `valid` 里的 `id`）。
const ACCOUNT_MARKERS: [&str; 5] = ["uid", "id", "账号", "帐号", "用户名"];

/// 数字标识的判定窗口（字符数）：名字两侧各看这么多字符，仍限在**同一行**内。
const SHAPE_WINDOW: usize = 24;

/// 服务账号形状（9f 的判据）。三种字面形状，任一命中即 true：
///
/// 1. 名字落在**同一行的反引号里**（配置/代码令牌）；
/// 2. 名字与**数字标识同行相邻**：窗口内有一串 ≥5 位的数字，或窗口内有账号标记时
///    有一串 ≥2 位的数字（`UID 42` 这类短标识）；
/// 3. 名字是 **`key: value`** 里的值（`key: "value"` 也收），且键本身是 ASCII 标识符
///    ——中文引导语（`他说：张三`）不是键，所以不会误伤。
///
/// 判据只看"它在正文里以什么身份出现"，不认任何具体名字。名字一次都没在正文里出现 ⇒
/// 没有形状可言（那一支归 9c，不在这里重复判）。
fn service_account_shape(name: &str, body: &str) -> bool {
    let name = name.trim();
    if name.is_empty() {
        return false;
    }
    for line in body.lines() {
        let mut from = 0usize;
        while let Some(offset) = line[from..].find(name) {
            let start = from + offset;
            let end = start + name.len();
            if inside_backticks(line, start, end)
                || has_identifier_neighbour(line, start, end)
                || is_key_value(line, start)
            {
                return true;
            }
            from = end;
        }
    }
    false
}

/// 出现处被同一行的一对反引号包着（左开右合，缺一边不算）。
fn inside_backticks(line: &str, start: usize, end: usize) -> bool {
    line[..start].matches('`').count() % 2 == 1 && line[end..].contains('`')
}

/// 名字两侧 [`SHAPE_WINDOW`] 字符内有没有数字标识。
fn has_identifier_neighbour(line: &str, start: usize, end: usize) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let start = line[..start].chars().count();
    let end = line[..end].chars().count();
    let window: String = chars[start.saturating_sub(SHAPE_WINDOW)..(end + SHAPE_WINDOW).min(chars.len())]
        .iter()
        .collect();
    identifier_shape(&window)
}

/// 一串文本里有没有"账号数字标识"的形状：≥5 位数字串，或（有账号标记时的）≥2 位数字串。
fn identifier_shape(text: &str) -> bool {
    let runs = digit_runs(text);
    if runs.iter().any(|run| run.len() >= 5) {
        return true;
    }
    has_account_marker(text) && runs.iter().any(|run| run.len() >= 2)
}

/// ASCII 标记按词边界匹配（`valid` 里的 `id` 不算），CJK 标记按子串匹配。
fn has_account_marker(text: &str) -> bool {
    let lowered = text.to_lowercase();
    ACCOUNT_MARKERS.iter().any(|marker| {
        if marker.is_ascii() {
            let mut from = 0usize;
            while let Some(offset) = lowered[from..].find(marker) {
                let start = from + offset;
                let before_ok = lowered[..start]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !c.is_ascii_alphanumeric());
                let after = lowered[start + marker.len()..].chars().next();
                let after_ok = after.is_none_or(|c| !c.is_ascii_alphanumeric());
                if before_ok && after_ok {
                    return true;
                }
                from = start + marker.len();
            }
            false
        } else {
            lowered.contains(marker)
        }
    })
}

/// 名字紧跟在 `key:` / `key：` / `key: "` 之后，且键是 ASCII 标识符（行内、允许空白）。
fn is_key_value(line: &str, start: usize) -> bool {
    let before = line[..start].trim_end();
    let before = before.strip_suffix('"').unwrap_or(before).trim_end();
    let Some(before) = before.strip_suffix(':').or_else(|| before.strip_suffix('：')) else {
        return false;
    };
    let key = before.trim_end();
    let key = key.strip_suffix('"').unwrap_or(key);
    let chars: Vec<char> = key.chars().collect();
    let ident = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
    let mut len = 0usize;
    for &c in chars.iter().rev() {
        if !ident(c) {
            break;
        }
        len += 1;
    }
    len > 0 && (len == chars.len() || !ident(chars[chars.len() - len - 1]))
}

fn check_quote(item: &FactItem, bodies: &HashMap<String, String>) -> Option<&'static str> {
    if item.quote.trim().is_empty() {
        return Some("quote_not_verbatim");
    }
    if item.quote.chars().count() > MAX_QUOTE_CHARS {
        return Some("quote_too_long");
    }
    // 逐字子串：evidence 里任一轮的正文包含引文即可。两侧都走 `evidence_view`
    // （`prepare` + 抹行内标记）：模型系统性剥 `**`/反引号，那是表征差异不是编造。
    let verbatim = item.evidence.iter().any(|evidence| {
        bodies
            .get(&evidence.turn_uid)
            .is_some_and(|body| evidence_view(body).contains(&evidence_view(&item.quote)))
    });
    // 省略号只在源文本身没有时拒（§10.70 的 e98cf58f：源文自带 `…`）。
    if !verbatim && (item.quote.contains('…') || item.quote.contains("...")) {
        return Some("ellipsis");
    }
    if !verbatim {
        return Some("quote_not_verbatim");
    }
    for number in digit_runs(&item.text) {
        if !item.quote.contains(&number) {
            return Some("number_not_grounded");
        }
    }
    None
}

/// 会话的主人——**只有身份已知且盖过章时**才有值（I2）。它决定 6c 对 human 轮收不收紧：
///
/// - `Some` ⇒ human 轮的说话人可判定（就是这位主人），不再自动放行；
/// - `None` ⇒ 身份未知，6c 维持 I1 的 fail-open（对 human 轮硬拒等于把"用户自己的决定"
///   整类砍掉，而那是这个记忆服务的主要输入）。
///
/// "有 claim"的口径按工单 I2 规则 4 的原话：该会话在 `session_principal_claim` 里有章。
/// `resolve_principal` 还会回落到 `sessions.owner_principal_id`（旧行/运维手填），回落值
/// 能读身份、却不是 I1 的盖章机制——没盖章的会话必须保持 fail-open，所以这里要求
/// `claim.is_some()`。章盖了 `unknown` 同样算"不知道"（设计 §2.1 第 3 级）：
/// [`rc_memory::writers_v4::principal_anchor_canonical`] 对空/`unknown` 返回 `None`。
/// `canonical` 与主人锚同一套算法，闸门不另写一份。
pub(crate) struct KnownPrincipal {
    canonical: String,
}

/// 读这个会话的主人。没 claim、或章是"不知道" ⇒ `None`（不是错误：不知道就是不知道）。
pub(crate) fn known_principal(
    t: &TranscriptHandle,
    session_key: &str,
    device_id: &str,
) -> Result<Option<KnownPrincipal>, DistillError> {
    let Some(resolved) = rc_ledger::transcript::resolve_principal(t, session_key, device_id)?
    else {
        return Ok(None);
    };
    if resolved.claim.is_none() {
        return Ok(None);
    }
    let Some(canonical) = rc_memory::writers_v4::principal_anchor_canonical(
        &resolved.principal_id,
        resolved.principal_name.as_deref(),
    ) else {
        return Ok(None);
    };
    Ok(Some(KnownPrincipal { canonical: canonical.to_string() }))
}

/// 主语锚就是**本 agent 的锚**：agent 轮的说话人是本 agent，所以"主语 = 本 agent"与
/// human 轮的"主语 = 主人"是同一件事的两面。
///
/// 判定按库读：`kind='agent'` 且 `canonical` 与会话的 **`source_agent`** 逐字相等。
/// **身份轴只有一条**（C1）：`source_agent` 是"本 agent 是谁"的权威列——提示词里的说话人、
/// 会话自锚的 canonical 都用它；`owner_agent_id` 是归属/可见性轴（`resource_owner`），
/// 拿它判身份会让"会话 owner 是另一个 agent"的会话把本 agent 的锚判成别人的锚
/// （提示词说主语是 qoder，闸门却按 owner 名比）。
/// 与 [`is_principal_anchor`] 同一条取向：只读、不建锚。
fn is_session_agent_anchor(
    m: &MemoryHandle,
    anchor_id: &str,
    source_agent: &str,
) -> Result<bool, DistillError> {
    let name = source_agent.trim();
    if name.is_empty() {
        return Ok(false);
    }
    match m.conn().query_row(
        "SELECT kind, canonical FROM anchor WHERE id=?1",
        rusqlite::params![anchor_id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    ) {
        Ok((kind, canonical)) => Ok(kind == "agent" && canonical.trim() == name),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// 主语锚就是主人锚：主人锚是内容寻址的（id = `compute_mem_id(Anchor, "person",
/// canonical, "", scope)`），所以"该 scope 下 canonical 等于主人 canonical 的 person 锚"
/// 只有一格 id。**只读**判定：这里不建锚——判闸不该产生数据。
fn is_principal_anchor(
    m: &MemoryHandle,
    anchor_id: &str,
    principal: &KnownPrincipal,
    scope: &Scope,
) -> Result<bool, DistillError> {
    let expected = rc_memory::compute_mem_id(
        rc_memory::MemTable::Anchor,
        "person",
        &principal.canonical,
        "",
        scope,
    );
    if expected != anchor_id {
        return Ok(false);
    }
    // 同一格 id 已蕴含 canonical 与 scope；再核一遍 kind，防手写/夹具行把别的锚塞进这一格。
    let kind: String = match m.conn().query_row(
        "SELECT kind FROM anchor WHERE id=?1",
        rusqlite::params![anchor_id],
        |r| r.get(0),
    ) {
        Ok(kind) => kind,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    Ok(kind == "person")
}

/// 6c 的取值输入：判的是**哪一格主语**、比**哪几条证据轮**，以及（state 才有）**承载这条
/// 断言的值**。fact/event 没有"值"这一格（fact 的值在 `value_json` 里，闸不读）⇒ `None`。
pub(crate) struct SubjectClaim<'a> {
    pub subject_anchor_id: &'a str,
    pub evidence_uids: Vec<&'a str>,
    pub value: Option<&'a str>,
}

impl<'a> SubjectClaim<'a> {
    /// 闸门里的 `FactItem` 那一支：state 的 `values` 与 `items` 同下标，值由调用方另给。
    fn of_item(item: &'a FactItem, value: Option<&'a str>) -> Self {
        Self {
            subject_anchor_id: &item.subject_anchor_id,
            evidence_uids: item.evidence.iter().map(|evidence| evidence.turn_uid.as_str()).collect(),
            value,
        }
    }
}

/// 6c 的放行支身份（C1）。fact/event 只用"放没放行"；state 侧靠它盖 `mode`：
/// [`SubjectAllow::grounded`] 为真的支才算有源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubjectAllow {
    /// 第 1 支：主语不是锚（自由文本/代词），本闸不判主语面。
    NonAnchorSubject,
    /// 第 1 支的 fail-open：human 轮 + 会话身份未知（I1）。
    UnknownIdentity,
    /// 第 2 支：主语名与承载值的那一句同句。
    NamedInValueSentence,
    /// 第 2 支的旧口径：主语名只在同轮的别的句子里出现（跨句借名）。
    NamedInTurnOnly,
    /// 第 3 支：证据轮说话人就是该锚；`grounded` = 值句里有第一人称标记。
    AnchorSpeaks { grounded: bool },
    /// 第 4 支：human 轮 + 主语锚是主人锚；`grounded` 同上。
    PrincipalSpeaks { grounded: bool },
    /// 第 5 支：agent 轮 + 主语锚是本会话自锚；`grounded` 同上。
    SessionAgentSpeaks { grounded: bool },
}

impl SubjectAllow {
    /// 这一支是否把主语落在了"承载值的那一句"里——state 的 `expressed` 判据。
    ///
    /// 前两支不降级，理由与"有没有源"无关：主语是代词时源就是那个代词（本闸不判自由
    /// 文本主语）；身份未知时 human 轮的 fail-open 是 I1 的既有裁定（硬拒会砍掉"用户
    /// 自己的决定"整类能力）。收紧它们不是 C1 的范围。
    pub(crate) fn grounded(&self) -> bool {
        match self {
            SubjectAllow::NonAnchorSubject
            | SubjectAllow::UnknownIdentity
            | SubjectAllow::NamedInValueSentence => true,
            SubjectAllow::NamedInTurnOnly => false,
            SubjectAllow::AnchorSpeaks { grounded }
            | SubjectAllow::PrincipalSpeaks { grounded }
            | SubjectAllow::SessionAgentSpeaks { grounded } => *grounded,
        }
    }
}

/// 6c 的判定：放行（带支身份）或拒收（码）。拒收码集合与 C1 之前逐字相同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubjectCheck {
    Allowed(SubjectAllow),
    Rejected(&'static str),
}

/// 主语必须在证据里（第 6c 步）。放行条件任一成立：
///
/// 1. **主语不是锚**（自由文本 / 代词）：本闸不判自由文本主语。代词这一支读得到两处——
///    主语 id 本身与锚的 `canonical`；自由文本主语在第 6 步已被 `ref_missing` 拒收
///    （`subject_anchor_id` 指向不存在的行就过不了），走不到这里。
/// 2. 该锚的 canonical 或**任一别名**经 `prepare` 归一后出现在任一条 evidence 轮正文里
///    （与 [`check_quote`] 同一把尺子），且**按词出现**（[`rc_memory::reader::name_occurs`]：
///    ASCII 名按词边界、汉字名至少两字——短别名与单字名不再命中任意正文）。
///    词表取自库（`anchor` + `alias`），这里不抄第二份。
///    **C1 把这一支分成两档**：名字与**承载值的那一句**同句 ⇒
///    [`SubjectAllow::NamedInValueSentence`]；名字只出现在同轮的别的句子里（跨句借名）⇒
///    [`SubjectAllow::NamedInTurnOnly`]。分句规则的家是
///    [`rc_memory::reader::name_in_value_sentence`]，本文件不另写一份。
/// 3. 该行的某条 evidence 轮**说话人就是该锚**（`mention.speaker_anchor_id`）。
///    这一列 C1 起是**程序列**（写库层按"转录角色 + 会话身份"盖章：human ⇒ 主人锚、
///    agent ⇒ 自锚），模型在 payload 里写的 `speaker_ref` 只在程序给不出值时保留。
///    读法在 [`rc_memory::reader::turn_spoken_by`]，不在本 crate 里拼第二份 SQL。
/// 4. 证据轮里**有** human 轮且这一行的主语锚**就是主人锚**（I2）：human 轮的说话人是会话主人，
///    "我…"的主语于是有可判的一格。只在身份已知（[`KnownPrincipal`]）时生效。
/// 5. **agent 轮的说话人是本 agent**，主语锚落在本会话自锚那一格同理（[`is_session_agent_anchor`]）。
///
/// human 轮的 fail-open（I1）仍在，但**只对身份未知的会话**：有身份时 human 轮与 agent 轮
/// 同一把尺子。理由见 [`KnownPrincipal`]——两个方向都有代价，收紧的那一半必须有身份才做。
/// 五支都不成立 ⇒ [`SUBJECT_NOT_IN_EVIDENCE`]。空名字/空白名字不参与第 2 条：
/// 空串是任何正文的子串，放它进来等于把这一条变成恒真。
///
/// 第 3/4/5 支的 `grounded` 读的是**值句里有没有第一人称标记**（
/// [`rc_memory::reader::first_person_in_value_sentence`]）：说话人是主语这件事只说清了
/// "谁在说"，没说清"哪一句承载值"；两者同时成立才算主语与值同句。
pub(crate) fn check_subject_in_evidence(
    m: &MemoryHandle,
    claim: &SubjectClaim<'_>,
    bodies: &HashMap<String, String>,
    roles: &HashMap<String, String>,
    principal: Option<&KnownPrincipal>,
    owner_scope: &Scope,
    source_agent: &str,
) -> Result<SubjectCheck, DistillError> {
    let Some(names) = rc_memory::reader::anchor_names(m, claim.subject_anchor_id)? else {
        // 第 6 步刚放行过，这里查不到只可能是两步之间行被删；按引用缺失处理，不吞。
        return Ok(SubjectCheck::Rejected("ref_missing"));
    };
    if is_pronoun(claim.subject_anchor_id) || is_pronoun(&names.canonical) {
        return Ok(SubjectCheck::Allowed(SubjectAllow::NonAnchorSubject));
    }
    // human 轮的主语是"说话人自己"：正文写"我把 X 改了"，名字本来就不该出现在正文里。
    // I1 时这一格对 human 轮一律 fail-open（那时没有可判定的说话人锚）；I2 起分成两半：
    // 会话身份**已知** ⇒ 按常规几支判（第 4 支"主语锚 == 主人锚"是 human 轮的正解）；
    // 身份**未知** ⇒ 仍是 fail-open（设计 §3.3：对未知身份的 human 轮硬拒会砍掉
    // "用户自己的决定"整类能力，已登记 ledger §7）。
    //
    // 判据是**任一**证据轮是 human，不是"全是 human"：一条 human 陈述常带 agent 的回执作
    // 第二条证据（"我把 X 改了" + "收到"），要求全 human 会把这类正确草稿判成
    // "主语不在证据里"（2026-10-28 冒烟实测：smoke-2026-10-28 的第一条 fact 就是这样被误拒）。
    let human_evidence = claim
        .evidence_uids
        .iter()
        .any(|uid| roles.get(*uid).is_some_and(|r| r == "human"));
    // agent 轮同理：说话人是本 agent，主语锚落在本 agent 那一格也算"在证据里"
    // （2026-10-28 冒烟：agent 回执"明白…"带出的草稿主语是 qoder，按名字判定会被误拒）。
    let agent_evidence = claim
        .evidence_uids
        .iter()
        .any(|uid| roles.get(*uid).is_some_and(|r| r == "agent"));
    if human_evidence && principal.is_none() {
        return Ok(SubjectCheck::Allowed(SubjectAllow::UnknownIdentity));
    }
    let evidence_bodies: Vec<String> = claim
        .evidence_uids
        .iter()
        .filter_map(|uid| bodies.get(*uid))
        .map(|body| prepare(body))
        .collect();
    let name_list: Vec<&str> = std::iter::once(names.canonical.as_str())
        .chain(names.aliases.iter().map(String::as_str))
        .filter(|name| !name.trim().is_empty())
        .collect();
    let named = name_list
        .iter()
        .any(|name| evidence_bodies.iter().any(|body| rc_memory::reader::name_occurs(body, name)));
    // 值句共现（C1）：名字与"承载值的那一句"同句。没有值这一格（fact/event）⇒ 不成立，
    // 于是第 2 支的身份永远落在旧口径上——fact/event 的身份没人读，行为逐字不变。
    let named_in_value = match claim.value {
        Some(value) if named => {
            let value = prepare(value);
            name_list.iter().any(|name| {
                evidence_bodies.iter().any(|body| {
                    rc_memory::reader::name_in_value_sentence(body, &value, name)
                })
            })
        }
        _ => false,
    };
    if named {
        return Ok(SubjectCheck::Allowed(if named_in_value {
            SubjectAllow::NamedInValueSentence
        } else {
            SubjectAllow::NamedInTurnOnly
        }));
    }
    // 三支说话人豁免共用这一格：值句里有没有第一人称标记。
    let speaker_grounded = match claim.value {
        Some(value) => {
            let value = prepare(value);
            evidence_bodies
                .iter()
                .any(|body| rc_memory::reader::first_person_in_value_sentence(body, &value))
        }
        None => false,
    };
    // 第 3 支：证据轮的说话人就是该锚。读的是 `mention.speaker_anchor_id`——C1 起这一列由
    // 写库层按程序规则盖章（human ⇒ 主人锚、agent ⇒ 自锚），闸**不读**模型在 payload 里
    // 写的 `speaker_ref`（那一格只作命名 pass 的早期值，写库层会覆盖）。
    for uid in &claim.evidence_uids {
        if rc_memory::reader::turn_spoken_by(m, uid, claim.subject_anchor_id)? {
            return Ok(SubjectCheck::Allowed(SubjectAllow::AnchorSpeaks {
                grounded: speaker_grounded,
            }));
        }
    }
    // 第 4 支（I2）：human 轮的说话人就是会话主人，主语锚落在主人锚那一格也算"在证据里"。
    if human_evidence {
        if let Some(principal) = principal {
            if is_principal_anchor(m, claim.subject_anchor_id, principal, owner_scope)? {
                return Ok(SubjectCheck::Allowed(SubjectAllow::PrincipalSpeaks {
                    grounded: speaker_grounded,
                }));
            }
        }
    }
    // 第 5 支：agent 轮的说话人是本 agent，主语锚落在本 agent 那一格同理。
    // 身份轴 = `batch.source_agent`（不是 owner_agent_id）：见 [`is_session_agent_anchor`]。
    if agent_evidence
        && is_session_agent_anchor(m, claim.subject_anchor_id, source_agent)?
    {
        return Ok(SubjectCheck::Allowed(SubjectAllow::SessionAgentSpeaks {
            grounded: speaker_grounded,
        }));
    }
    Ok(SubjectCheck::Rejected(SUBJECT_NOT_IN_EVIDENCE))
}

// ── A-rebind：agent 轮的 event actor 程序回填 ─────────────────────────────────
//
// 台账 §10.14 / item 141：证据轮全是 agent 轮、正文只有"我/实施者"时，模型会把 actor 填成
// 正文里出现的某个工具/包名（`rc-distill`），6c 于是整条拒——14 条真事件就这么丢了。
// 裁定（2026-10-29，交接档 §3.5）**收紧后采纳**：agent 轮的行动者是该 agent 是确定性事实，
// 但只有下面三条**同时**成立时程序才把 actor 回填成**会话自锚**，否则保持原来的整条拒。

/// 三条判据（缺一不可）：
///
/// 1. 该条 event 的证据轮**全是** agent 轮（有任何 human/system 轮就不猜：那些轮的"我"
///    另有身份轴）；
/// 2. 模型选的 actor 在那些轮正文里**不逐字出现**——用闸门自己的名字尺子
///    （[`rc_memory::reader::name_occurs`]，与 6c 第 2 支同一份），不另写一条"包含"；
/// 3. 候选池里**唯一**存在本会话 agent 锚：`kind='agent'`、`canonical` 等于会话的
///    `source_agent`（与 [`crate::pass_prompt::speaker_ref`] 的 `[self]` 同一套判据），
///    且对该行 owner 可见。0 个或多个 ⇒ 不猜，仍然拒。
///
/// 返回 `Some(自锚 id)` = 这条可以回填；`None` = 保持 6c 的整条拒。判定是纯读：
/// 不建锚、不改载荷、不写库。闸与写库两侧都调它（[`crate::review::run::assemble_event`]），
/// 同一份输入得出同一个结果，不各写一份判据。
// 八个入参是这条判定的全部输入（memory 句柄、actor、证据轮、正文、角色、会话 agent、
// owner agent、owner scope）；打包成结构体只会把同一份数据挪个地方再解一次。
// clippy 的 7 个上限是风格阈值，不是这条读判定的约束。
#[allow(clippy::too_many_arguments)]
pub(crate) fn event_actor_rebind(
    m: &MemoryHandle,
    actor_anchor_id: &str,
    evidence: &[String],
    bodies: &HashMap<String, String>,
    roles: &HashMap<String, String>,
    source_agent: &str,
    owner_agent_id: &str,
    owner_scope: &Scope,
) -> Result<Option<String>, DistillError> {
    // 1. 证据轮全是 agent 轮（空证据走不到这里：6c 之前已按 `turn_uids` 建过骨架）。
    if evidence.is_empty()
        || !evidence
            .iter()
            .all(|uid| roles.get(uid).is_some_and(|role| role == "agent"))
    {
        return Ok(None);
    }
    // 2. 模型选的 actor 不在正文里（名字面与 6c 同一把尺子：canonical + 任一别名）。
    let Some(names) = rc_memory::reader::anchor_names(m, actor_anchor_id)? else {
        return Ok(None);
    };
    let prepared: Vec<String> = evidence
        .iter()
        .filter_map(|uid| bodies.get(uid))
        .map(|body| prepare(body))
        .collect();
    let named = std::iter::once(names.canonical.as_str())
        .chain(names.aliases.iter().map(String::as_str))
        .filter(|name| !name.trim().is_empty())
        .any(|name| prepared.iter().any(|body| rc_memory::reader::name_occurs(body, name)));
    if named {
        return Ok(None);
    }
    // 3. 候选池里唯一的本会话 agent 锚。
    session_agent_anchor(m, source_agent, owner_agent_id, owner_scope)
}

/// 本会话的 agent 自锚：`kind='agent'`、`canonical` 等于会话 `source_agent`、仍在场、且对
/// 该行 owner 可见的那**一格**（与 `append_agent_anchor` 盖章的 `[self]` 同一口径）。
///
/// 唯一性是判据的一部分：0 格或多格都返回 `None`（"多个则不猜"）。id 本身是内容寻址的
/// （`compute_mem_id(Anchor, "agent", canonical, "", scope)`），所以多格只可能来自脏数据/
/// 手写行——那正是不能再猜的那一档。
fn session_agent_anchor(
    m: &MemoryHandle,
    source_agent: &str,
    owner_agent_id: &str,
    owner_scope: &Scope,
) -> Result<Option<String>, DistillError> {
    let name = source_agent.trim();
    if name.is_empty() {
        return Ok(None);
    }
    let mut st = m.conn().prepare(
        "SELECT id, canonical FROM anchor
          WHERE kind='agent' AND status='active' AND scope_kind=?1 AND scope_key=?2",
    )?;
    let rows = st.query_map(rusqlite::params![owner_scope.kind.as_str(), owner_scope.key], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut hits: Vec<String> = Vec::new();
    for row in rows {
        let (id, canonical) = row?;
        if canonical.trim() == name {
            hits.push(id);
        }
    }
    match hits.as_slice() {
        [only] => {
            // 候选池的口径：不可见的锚不进池，也就不参与回填。
            if rc_memory::owner::anchor_visible(m, only, owner_agent_id, owner_scope)? {
                Ok(Some(only.clone()))
            } else {
                Ok(None)
            }
        }
        _ => Ok(None),
    }
}

/// 逐 draft 的失败记账：只记**第一个**失败码（同一 draft 多错以先命中者为准，码序与旧实现
/// 的遍历顺序一致）。
#[derive(Debug, Default)]
struct Failures {
    /// 按发现顺序的第一个失败 draft（下标）。它决定 [`GateReport::code`]——批级没命中时，
    /// 这一格就是今天"整批的第一个失败码"。
    first: Option<usize>,
}

impl Failures {
    fn set(&mut self, drafts: &mut [DraftGate], index: usize, code: &'static str) {
        if drafts[index].code.is_none() {
            drafts[index].code = Some(code);
            if self.first.is_none() {
                self.first = Some(index);
            }
        }
    }
}

/// 批级前置失败：整批不可用，没有可判 draft（`drafts` 留空，调用方按整批处理）。
fn batch_fail(code: &'static str, turns: usize, items: usize) -> GateReport {
    GateReport {
        batch_code: Some(code),
        code: Some(code),
        turns_checked: turns,
        items_checked: items,
        drafts: Vec::new(),
    }
}

/// 每条 draft 的骨架：位置引用 + 证据轮。判定只往 `code` 里写，不改这两格。
fn draft_skeletons(items: &Items) -> Vec<DraftGate> {
    let evidence_turns = |evidence: &[Evidence]| -> Vec<String> {
        evidence.iter().map(|e| e.turn_uid.clone()).collect()
    };
    match items {
        Items::Fact { items, .. } => items
            .iter()
            .enumerate()
            .map(|(index, item)| DraftGate {
                reference: format!("f{}", index + 1),
                code: None,
                turn_uids: evidence_turns(&item.evidence),
            })
            .collect(),
        Items::Event { items, .. } => items
            .iter()
            .enumerate()
            .map(|(index, item)| DraftGate {
                reference: format!("e{}", index + 1),
                code: None,
                turn_uids: evidence_turns(&item.evidence),
            })
            .collect(),
        // naming 的判定单位是**锚**（`a1..aN`）；mention 级的失败归到它指向的锚上。
        Items::Naming(naming) => naming
            .anchors
            .iter()
            .enumerate()
            .map(|(index, anchor)| DraftGate {
                reference: format!("a{}", index + 1),
                code: None,
                turn_uids: evidence_turns(&anchor.evidence),
            })
            .collect(),
        // state 的位置引用是 `s1..sN`，与 `state_review_user` 的 `ITEM s{}` 同一套。
        Items::State { items, .. } => items
            .iter()
            .enumerate()
            .map(|(index, item)| DraftGate {
                reference: format!("s{}", index + 1),
                code: None,
                turn_uids: evidence_turns(&item.evidence),
            })
            .collect(),
        // time 的每条结论只落一轮：turn 就是它的证据轮（位置引用不进任何 prompt）。
        Items::Time(items) => items
            .iter()
            .enumerate()
            .map(|(index, item)| DraftGate {
                reference: format!("t{}", index + 1),
                code: None,
                turn_uids: vec![item.turn_uid.clone()],
            })
            .collect(),
    }
}

/// mention 归到哪条锚：`anchor_ref` 对不上任何同批锚 ⇒ `None`（调用方按批级 fail-closed）。
fn mention_draft(anchors: &[NamingAnchor], mention: &NamingMention) -> Option<usize> {
    let reference = mention.anchor_ref.as_deref()?;
    anchors
        .iter()
        .position(|anchor| anchor.reference.as_deref() == Some(reference))
}

/// 程序闸（逐 draft，I3 第一刀）。批级前置见模块文档；通过前置后逐条判定，
/// 一条失败不阻断其他条。返回 [`GateReport`]：批级码 + 逐 draft 码 + 与旧口径兼容的首个码。
pub fn gate_report(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    item: &Item,
    now_ms: i64,
) -> Result<GateReport, DistillError> {
    let Some(payload) = parse_payload(item) else {
        return Ok(batch_fail("payload_shape", 0, 0));
    };
    let items_checked = payload.item_count();
    let mut drafts = draft_skeletons(&payload.items);
    let mut failures = Failures::default();
    let mut turns: BTreeSet<String> = payload.batch_turns.iter().cloned().collect();
    turns.extend(payload.referenced_turns().into_iter().map(str::to_string));
    let turns_checked = turns.len();

    // 2. 轮存在（先全部查完，再进入会话/设备比对）。
    for uid in &turns {
        if rc_ledger::transcript::turn_origin(t, uid)?.is_none() {
            return Ok(batch_fail("turn_missing", turns_checked, items_checked));
        }
    }

    // 3. 会话/设备与 item 逐字相等。
    for uid in &turns {
        match rc_ledger::transcript::turn_origin(t, uid)? {
            Some(origin)
                if origin.session_key == item.meta.session_key
                    && origin.device_id == item.meta.device_id => {}
            Some(_) => {
                return Ok(batch_fail("turn_session_mismatch", turns_checked, items_checked));
            }
            None => return Ok(batch_fail("turn_missing", turns_checked, items_checked)),
        }
    }

    // 4. 撤销：会话/设备命中 tombstone。
    if rc_ledger::transcript::tombstone_blocks(
        t,
        &item.meta.session_key,
        &item.meta.device_id,
        None,
    )? {
        return Ok(batch_fail("turn_tombstoned", turns_checked, items_checked));
    }

    // 一次读齐所有相关轮正文；上面的存在性检查保证这里不会有缺行。
    let mut bodies: HashMap<String, String> = HashMap::with_capacity(turns.len());
    // 角色与正文同一次读：6c 的主语规则对身份未知的 human 轮 fail-open（见
    // check_subject_in_evidence）。
    let mut roles: HashMap<String, String> = HashMap::with_capacity(turns.len());
    // 轮时刻同一次读：D10 的冲突分流要比"新断言的证据轮"与"库里那条活行的起点"谁更晚，
    // 而这一格与写侧的 `source_ms`（证据轮 `created_at_ms` 最大值）必须是同一把尺子。
    let mut turn_ms: HashMap<String, i64> = HashMap::with_capacity(turns.len());
    for uid in &turns {
        let (body, role, created_at_ms) = turn_body_and_role(t, uid)?.unwrap_or_default();
        bodies.insert(uid.clone(), body);
        roles.insert(uid.clone(), role);
        turn_ms.insert(uid.clone(), created_at_ms);
    }

    // 6c 收不收紧只看这个会话的身份知不知道；读法与 pass_* 选批同一个 `resolve_principal`。
    let principal = known_principal(t, &item.meta.session_key, &item.meta.device_id)?;

    // 5. 压缩摘要块：被引用/引文的轮不得是摘要。
    let referenced: BTreeSet<&str> = payload.referenced_turns().into_iter().collect();
    for uid in referenced {
        if bodies.get(uid).is_some_and(|body| is_compaction_body(body)) {
            return Ok(batch_fail("compaction_evidence", turns_checked, items_checked));
        }
    }

    match &payload.items {
        Items::Fact { family, items } => {
            // 6. 引用锚存在且未退场。
            for (index, it) in items.iter().enumerate() {
                if drafts[index].code.is_some() {
                    continue;
                }
                if let Some(code) = check_anchor(m, &it.subject_anchor_id)? {
                    failures.set(&mut drafts, index, code);
                    continue;
                }
                if let Some(object) = &it.object_anchor_id {
                    if let Some(code) = check_anchor(m, object)? {
                        failures.set(&mut drafts, index, code);
                    }
                }
            }
            // 6b. 引用锚对该行 owner 可见（跨 owner 的私有锚不得被引用）。
            // 码序是契约：每条 draft 只看自己的引用，不因别条先失败而漏判。
            for (index, it) in items.iter().enumerate() {
                if drafts[index].code.is_some() {
                    continue;
                }
                let refs = std::iter::once(&it.subject_anchor_id)
                    .chain(it.object_anchor_id.iter());
                for anchor in refs {
                    if let Some(code) = check_anchor_visible(
                        m,
                        anchor,
                        &payload.owner_agent_id,
                        &payload.owner_scope,
                    )? {
                        failures.set(&mut drafts, index, code);
                        break;
                    }
                }
            }
            // 6c. 主语必须在证据里（引用锚存在/可见之后的第三条）。
            for (index, it) in items.iter().enumerate() {
                if drafts[index].code.is_some() {
                    continue;
                }
                match check_subject_in_evidence(
                    m,
                    &SubjectClaim::of_item(it, None),
                    &bodies,
                    &roles,
                    principal.as_ref(),
                    &payload.owner_scope,
                    &payload.source_agent,
                )? {
                    // fact 不读放行支身份：`stated` 不是 state 的 mode 轴，降级在这里没有位置。
                    SubjectCheck::Allowed(_) => {}
                    SubjectCheck::Rejected(code) => failures.set(&mut drafts, index, code),
                }
            }
            // 7. 引文/数字。
            for (index, it) in items.iter().enumerate() {
                if drafts[index].code.is_some() {
                    continue;
                }
                if let Some(code) = check_quote(it, &bodies) {
                    failures.set(&mut drafts, index, code);
                }
            }
            // 8. 闭集：category → predicate → mode。
            for (index, it) in items.iter().enumerate() {
                if drafts[index].code.is_some() {
                    continue;
                }
                if !rc_memory::FACT_CATEGORIES.contains(&it.category.as_str()) {
                    failures.set(&mut drafts, index, "category");
                } else if it.predicate != family.predicate() {
                    failures.set(&mut drafts, index, "predicate");
                } else if it.mode != rc_memory::FACT_MODE_STATED {
                    failures.set(&mut drafts, index, "mode");
                }
            }
            // 9. 时间短语必须接地（C2）：非空时是 quote 的子串，或逐字出现在某条证据轮正文。
            // 放在既有码序**之后**：坏在一处的草稿仍报原来的码（如 subject_not_in_evidence），
            // 不给 `time_text` 抢前位——码序是契约（per_draft_gate 的两条用例钉住了这一点）。
            for (index, it) in items.iter().enumerate() {
                if drafts[index].code.is_some() {
                    continue;
                }
                let phrase = it.time_text.trim();
                if !phrase.is_empty()
                    && !it.quote.contains(phrase)
                    && !bodies.values().any(|body| body.contains(phrase))
                {
                    failures.set(&mut drafts, index, "time_text_ungrounded");
                }
            }
        }
        Items::Event { items, extras } => {
            // 6. 引用锚：subject/object/place/participants。
            for (index, (it, extra)) in items.iter().zip(extras).enumerate() {
                if drafts[index].code.is_some() {
                    continue;
                }
                if let Some(code) = check_anchor(m, &it.subject_anchor_id)? {
                    failures.set(&mut drafts, index, code);
                    continue;
                }
                if let Some(object) = &it.object_anchor_id {
                    if let Some(code) = check_anchor(m, object)? {
                        failures.set(&mut drafts, index, code);
                        continue;
                    }
                }
                if let Some(place) = &extra.place_anchor_id {
                    if let Some(code) = check_anchor(m, place)? {
                        failures.set(&mut drafts, index, code);
                        continue;
                    }
                }
                for participant in &extra.participants {
                    if let Some(code) = check_anchor(m, participant)? {
                        failures.set(&mut drafts, index, code);
                        break;
                    }
                }
            }
            // 6b. 同一批锚再看可见性（subject/object/place/participants 一个不落）。
            // 事件路径与 fact 同一条规则：候选表里没有的私有锚不能被 event 借成参与者。
            for (index, (it, extra)) in items.iter().zip(extras).enumerate() {
                if drafts[index].code.is_some() {
                    continue;
                }
                let refs = std::iter::once(&it.subject_anchor_id)
                    .chain(it.object_anchor_id.iter())
                    .chain(extra.place_anchor_id.iter())
                    .chain(extra.participants.iter());
                for anchor in refs {
                    if let Some(code) = check_anchor_visible(
                        m,
                        anchor,
                        &payload.owner_agent_id,
                        &payload.owner_scope,
                    )? {
                        failures.set(&mut drafts, index, code);
                        break;
                    }
                }
            }
            // 6c. 主语必须在证据里（与 fact 同一个码，同一条规则）。
            for (index, it) in items.iter().enumerate() {
                if drafts[index].code.is_some() {
                    continue;
                }
                let code = match check_subject_in_evidence(
                    m,
                    &SubjectClaim::of_item(it, None),
                    &bodies,
                    &roles,
                    principal.as_ref(),
                    &payload.owner_scope,
                    &payload.source_agent,
                )? {
                    // event 也不读放行支身份（A-rebind 另走一条程序回填，见下面）。
                    SubjectCheck::Allowed(_) => continue,
                    SubjectCheck::Rejected(code) => code,
                };
                // A-rebind（只对 event）：主语没在证据里、而证据轮全是 agent 轮、模型选的
                // actor 不在正文、且候选池里唯一存在本会话 agent 锚 ⇒ 由写侧回填成自锚，
                // 闸**放行**。别的失败码（`ref_missing` 等）不受影响，原样拒。
                if code == SUBJECT_NOT_IN_EVIDENCE
                    && event_actor_rebind(
                        m,
                        &it.subject_anchor_id,
                        &it.evidence.iter().map(|e| e.turn_uid.clone()).collect::<Vec<_>>(),
                        &bodies,
                        &roles,
                        &payload.source_agent,
                        &payload.owner_agent_id,
                        &payload.owner_scope,
                    )?
                    .is_some()
                {
                    continue;
                }
                failures.set(&mut drafts, index, code);
            }
            // 7. 引文/数字。
            for (index, it) in items.iter().enumerate() {
                if drafts[index].code.is_some() {
                    continue;
                }
                if let Some(code) = check_quote(it, &bodies) {
                    failures.set(&mut drafts, index, code);
                }
            }
            // 8. 闭集：category → predicate → mode（event 的谓词由程序盖成 `action`）。
            for (index, it) in items.iter().enumerate() {
                if drafts[index].code.is_some() {
                    continue;
                }
                if !EVENT_CATEGORIES.contains(&it.category.as_str()) {
                    failures.set(&mut drafts, index, "category");
                } else if it.predicate != EVENT_PREDICATE {
                    failures.set(&mut drafts, index, "predicate");
                } else if it.mode != EVENT_MODE_STATED {
                    failures.set(&mut drafts, index, "mode");
                }
            }
            // 9. 时间短语接地（C2），与 fact 分支同一条规则、同一个码。
            for (index, it) in items.iter().enumerate() {
                if drafts[index].code.is_some() {
                    continue;
                }
                let phrase = it.time_text.trim();
                if !phrase.is_empty()
                    && !it.quote.contains(phrase)
                    && !bodies.values().any(|body| body.contains(phrase))
                {
                    failures.set(&mut drafts, index, "time_text_ungrounded");
                }
            }
        }
        Items::State { items, values } => {
            // 6. 主体锚存在且未退场（v1 只认 anchor 主体，没有 object/place/participants）。
            for (index, it) in items.iter().enumerate() {
                if drafts[index].code.is_some() {
                    continue;
                }
                if let Some(code) = check_anchor(m, &it.subject_anchor_id)? {
                    failures.set(&mut drafts, index, code);
                }
            }
            // 6b. 主体锚对这一行的 owner 可见（与 fact/event 同一份谓词）。
            for (index, it) in items.iter().enumerate() {
                if drafts[index].code.is_some() {
                    continue;
                }
                if let Some(code) = check_anchor_visible(
                    m,
                    &it.subject_anchor_id,
                    &payload.owner_agent_id,
                    &payload.owner_scope,
                )? {
                    failures.set(&mut drafts, index, code);
                }
            }
            // 6c. 主语必须在证据里（与 fact/event 同一个码、同一条规则）。状态的主体常常是
            // "我"（human 轮 ⇒ 主人锚那一支放行）或引文里点名的第三方，两种都在这一格里判。
            // 值（`values[index]`）也进这一格：**它与主语同句**才是 `expressed` 的源
            // （盖戳在写库组装那一处，判据是同一个函数，见 `review::run::assemble_state`）。
            for (index, (it, value)) in items.iter().zip(values.iter()).enumerate() {
                if drafts[index].code.is_some() {
                    continue;
                }
                match check_subject_in_evidence(
                    m,
                    &SubjectClaim::of_item(it, Some(value)),
                    &bodies,
                    &roles,
                    principal.as_ref(),
                    &payload.owner_scope,
                    &payload.source_agent,
                )? {
                    // state 的 mode 不在闸门盖：盖戳发生在写库组装，闸只判"放不放行"。
                    SubjectCheck::Allowed(_) => {}
                    SubjectCheck::Rejected(code) => failures.set(&mut drafts, index, code),
                }
            }
            // 6d. D19：human 轮的主语不许是本会话 agent 的自锚（读数 §10.133：三路 21 行里 7 行
            //     是这个形状，而锚表里 human 锚 0 条）。6c 在"会话身份未知"时 fail-open 是 I2 的
            //     既有设计，不为它改；这一格只拒**那一种**挂法，并给一个能统计的理由码。
            //     agent 自己说"我"仍挂自锚（那 8 行"合规"就是它）⇒ 这不是收紧主语判定。
            for (index, it) in items.iter().enumerate() {
                if drafts[index].code.is_some() {
                    continue;
                }
                let human_evidence = it.evidence.iter().any(|evidence| {
                    roles.get(&evidence.turn_uid).is_some_and(|role| role == "human")
                });
                if human_evidence
                    && is_session_agent_anchor(m, &it.subject_anchor_id, &payload.source_agent)?
                {
                    failures.set(&mut drafts, index, SUBJECT_SELF_ON_HUMAN_CODE);
                }
            }
            // 7. 引文逐字与数字接地（同一份 check_quote）。
            for (index, it) in items.iter().enumerate() {
                if drafts[index].code.is_some() {
                    continue;
                }
                if let Some(code) = check_quote(it, &bodies) {
                    failures.set(&mut drafts, index, code);
                }
            }
            // 8. 闭集：dimension → predicate → mode。三格都是 DDL/契约的字面量，家在外面。
            for (index, it) in items.iter().enumerate() {
                if drafts[index].code.is_some() {
                    continue;
                }
                if !crate::pass_prompt::STATE_DIMENSIONS.contains(&it.category.as_str()) {
                    failures.set(&mut drafts, index, "dimension_invalid");
                } else if it.predicate != STATE_PREDICATE {
                    failures.set(&mut drafts, index, "predicate");
                } else if !crate::pass_prompt::STATE_MODES.contains(&it.mode.as_str()) {
                    failures.set(&mut drafts, index, "mode");
                }
            }
            // 9. 同维度异值自 D10 起**分流**，不再"异值即隔离"（方案
            //    `design/2026-11-01-state-conflict-adjudication.md` §3/§4）：
            //    · 新断言的证据轮**不早于**库里那条活行的起点 ⇒ 它就是要顶掉旧值的那一条，放行给
            //      审核；替代动作在写库里同一事务完成（`writers_state::put_state_rows`），闸既不
            //      判"该不该换"，也不替它记账。
            //    · 更早（迟到的旧话）⇒ 它不改写现在，是"补一段历史"，仍进隔离等人看，理由码不变。
            //    只拦不裁会让"他状态变了"这一类话把当前值**冻结在第一条**上，那比覆盖更坏。
            //    放在最后一条：先确认这一条本身站得住，再判它与库里那条的关系。
            for (index, (it, value)) in items.iter().zip(values.iter()).enumerate() {
                if drafts[index].code.is_some() {
                    continue;
                }
                let Some(live) = rc_memory::writers_state::live_conflict(
                    m,
                    &it.subject_anchor_id,
                    &it.category,
                    value,
                    &payload.owner_scope,
                )?
                else {
                    continue;
                };
                // 尺子与写侧同一格：证据轮 `created_at_ms` 的最大值。读不到时刻就按"更早"处理
                // ——没有尺子的行不许上位（宁可隔离，也不拿导入钟冒充源时刻，同 I3）。
                let item_ms = it
                    .evidence
                    .iter()
                    .filter_map(|evidence| turn_ms.get(&evidence.turn_uid))
                    .max()
                    .copied()
                    .unwrap_or(i64::MIN);
                if item_ms < live.valid_from_ms {
                    failures.set(&mut drafts, index, STATE_CONFLICT_CODE);
                }
            }
        }
        Items::Naming(naming) => {
            // 6. `speaker_ref`：同批锚的 reference 视为通过；否则必须在 `anchor` 表存在。
            // 失败归到该 mention 指向的锚；锚对不上是载荷不可归因 ⇒ 批级 fail-closed。
            let batch_refs: BTreeSet<&str> = naming
                .anchors
                .iter()
                .filter_map(|anchor| anchor.reference.as_deref())
                .collect();
            for mention in &naming.mentions {
                let Some(speaker) = mention.speaker_ref.as_deref() else {
                    continue;
                };
                if batch_refs.contains(speaker) {
                    continue;
                }
                if let Some(code) = check_anchor(m, speaker)? {
                    match mention_draft(&naming.anchors, mention) {
                        Some(index) => failures.set(&mut drafts, index, code),
                        None => return Ok(batch_fail(code, turns_checked, items_checked)),
                    }
                }
            }
            // 9a. kind 闭集。
            for (index, anchor) in naming.anchors.iter().enumerate() {
                if !ANCHOR_KINDS.contains(&anchor.kind.as_str()) {
                    failures.set(&mut drafts, index, "kind_mismatch");
                }
            }
            // 9b. canonical / alias 不得是代词（大小写不敏感）。
            for (index, anchor) in naming.anchors.iter().enumerate() {
                if is_pronoun(&anchor.canonical)
                    || anchor.aliases.iter().any(|alias| is_pronoun(alias))
                {
                    failures.set(&mut drafts, index, "pronoun");
                }
            }
            // 9c. canonical 与每个 alias 逐字出现在**该锚自己的**证据轮正文，或等于该锚
            // 自己在这些证据轮上的 mention 的 surface（C4b：判据从"该批任一轮"收到证据轮。
            // 中心实测 34 条草稿 / 199 锚 / 353 提及上只多杀 1 个锚（0.55%）——批内别处的
            // 同名不是这条锚的证据，"名字逐字出现"必须落在这条锚自己引用过的那几轮里）。
            for (index, anchor) in naming.anchors.iter().enumerate() {
                let evidence_turns: BTreeSet<&str> =
                    anchor.evidence.iter().map(|e| e.turn_uid.as_str()).collect();
                let own_bodies: Vec<&String> = evidence_turns
                    .iter()
                    .filter_map(|uid| bodies.get(*uid))
                    .collect();
                // "该锚自己的" mention：指向这条锚、且落在它自己的证据轮上。指向别处的
                // mention 即便 surface 同名，也不是这条锚在别处被叫过的证据。
                let own_surfaces: BTreeSet<&str> = naming
                    .mentions
                    .iter()
                    .filter(|mention| {
                        evidence_turns.contains(mention.turn_uid.as_str())
                            && mention_draft(&naming.anchors, mention) == Some(index)
                    })
                    .map(|mention| mention.surface.as_str())
                    .collect();
                let verbatim = |name: &str| {
                    own_surfaces.contains(name)
                        || own_bodies.iter().any(|body| body.contains(name))
                };
                if !verbatim(&anchor.canonical)
                    || anchor.aliases.iter().any(|alias| !verbatim(alias))
                {
                    failures.set(&mut drafts, index, "not_verbatim");
                }
            }
            // 9d. mention span 落在该轮字符数内，且 start < end。
            for mention in &naming.mentions {
                if let (Some(start), Some(end)) = (mention.span_start, mention.span_end) {
                    let chars = bodies
                        .get(&mention.turn_uid)
                        .map(|body| body.chars().count())
                        .unwrap_or(0);
                    if start < 0 || end < 0 || start >= end || end as usize > chars {
                        match mention_draft(&naming.anchors, mention) {
                            Some(index) => failures.set(&mut drafts, index, "span_out_of_range"),
                            None => {
                                return Ok(batch_fail(
                                    "span_out_of_range",
                                    turns_checked,
                                    items_checked,
                                ))
                            }
                        }
                    }
                }
            }
            // 9e 不在这里判（W14d）：只有 agent 轮作证据的 person 锚照写，现实性由写库层
            // 降成未知并留审计行；闸门拒收会切断"有来源、有去处"的转述。
            // 9f. 服务账号形状不许升 person（P0-2）：判据见 [`service_account_shape`]。
            for (index, anchor) in naming.anchors.iter().enumerate() {
                if anchor.kind != PERSON_KIND {
                    continue;
                }
                let shaped = anchor.evidence.iter().any(|evidence| {
                    bodies
                        .get(&evidence.turn_uid)
                        .is_some_and(|body| service_account_shape(&anchor.canonical, body))
                });
                if shaped {
                    failures.set(&mut drafts, index, SERVICE_ACCOUNT_SHAPE);
                }
            }
        }
        Items::Time(items) => {
            for (index, it) in items.iter().enumerate() {
                if drafts[index].code.is_some() {
                    continue;
                }
                // 10a. 2000–2100。
                if it.start_ms.is_some_and(|ms| !(TIME_MIN_MS..TIME_MAX_MS).contains(&ms))
                    || it.end_ms.is_some_and(|ms| !(TIME_MIN_MS..TIME_MAX_MS).contains(&ms))
                {
                    failures.set(&mut drafts, index, "time_range");
                    continue;
                }
                // 10b. start <= end。
                if let (Some(start), Some(end)) = (it.start_ms, it.end_ms) {
                    if start > end {
                        failures.set(&mut drafts, index, "valid_range");
                        continue;
                    }
                }
                // 10c. kind 闭集。
                if !TIME_KINDS.contains(&it.kind.as_str()) {
                    failures.set(&mut drafts, index, "kind_mismatch");
                    continue;
                }
                // 10d. 事件/说话时间不在未来（promise/plan/decision 的 due 例外不在 time pass）。
                if matches!(it.kind.as_str(), "event_time" | "speech_time")
                    && it.start_ms.is_some_and(|ms| ms > now_ms)
                {
                    failures.set(&mut drafts, index, "time_future");
                }
            }
        }
    }

    // 与旧口径逐字兼容的"第一个失败码"：批级没命中时，按发现顺序取第一个失败 draft 的码
    // （发现顺序 = 今天的"阶段为主序、draft 为次序"）。
    let code = failures
        .first
        .and_then(|index| drafts.get(index))
        .and_then(|draft| draft.code);
    Ok(GateReport { batch_code: None, code, turns_checked, items_checked, drafts })
}

/// 程序闸（整批口径的旧入口，保留给既有调用与表驱动用例）。
///
/// `code` 是今天的"第一个失败码"；逐 draft 判定用 [`gate_report`]。
pub fn gate(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    item: &Item,
    now_ms: i64,
) -> Result<GateOutcome, DistillError> {
    let report = gate_report(m, t, item, now_ms)?;
    Ok(match report.code {
        Some(code) => fail(code, report.turns_checked, report.items_checked),
        None => pass(report.turns_checked, report.items_checked),
    })
}
