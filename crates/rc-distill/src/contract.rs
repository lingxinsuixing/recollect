//! 抽取输出的契约层。这一层的唯一判据是 §7 那句"缺任何一个 ⇒ 整批丢弃 ⇒ 不推水位"，
//! 所以它的返回类型只有一个 `Result`：**没有"部分接受"这个形状**。
//! 把"这一条不合法所以跳过"实现出来 = provenance 覆盖率 100% 那条判据当场变成口号。

use crate::batch::Batch;
use rc_contract::normalize_body;
use rc_memory::{MemTable, Provenance, Scope, ScopeKind};

pub const KEYS: [&str; 5] = ["entities", "cards", "events", "assertions", "worthless_turn_uids"];
pub const PROVENANCE_KEY: &str = "provenance";

/// 输出契约文本。**整词扫描守卫读的就是这一段**（tests/p_column_scan.rs），
/// 所以它必须与 `parse_output` 实际读的字段名同源——改一处不改另一处就是守卫骗人。
/// 还有一条隐蔽依赖要说在前面：文本里的 `turn_uid` / `ids` / `Ids,` 全靠**相邻的词字符**躲过
/// `id` 这个 P 独占列名的整词命中。改写措辞时（任务 5 的 prompt 也拼这一段）若写成 "each id"，
/// 守卫会真的判红——那不是误伤，那正是在要求 X 命名 P 的列。
/// 标题行 `OUTPUT CONTRACT:` 也在这一格里，理由与任务 7 的 `prompt.rs::TAIL_EXTRACT` 有关：
/// `concat!` 只接受**字面量**，不接受 `&'static str` 常量的路径，所以 prompt 层拼不出
/// "标题行 + 本常量"。按任务 7 那条"改契约这一侧、不许 prompt 层用 `format!` 绕"的处置，
/// 标题行搬到这里来——它是这份契约文本的**一部分**，不是第二个家。
///
/// W23a：元素占位从 `[turn_uid]` 改成 `[t1]`，与 prompt 渲染的批内位置别名同一门语法
/// （`batch::ref_index`）。契约文本要说什么，模型就抄什么——它不再被要求抄 64 位身份值。
pub const OUTPUT_CONTRACT_SCHEMA: &str = concat!(
    "OUTPUT CONTRACT:\n",
    "Output is ONE JSON object and nothing else. No markdown fence, no prose, no trailing commas.\n",
    "Top-level keys, all five always present (empty array is allowed, a missing key is not):\n",
    "  \"entities\": [{\"kind\": \"person|project|org|tool|place|concept\", \"title\": string, \"aliases\": [string], \"provenance\": [t1]}]\n",
    "  \"cards\":    [{\"kind\": string, \"title\": string, \"body\": string, \"valid_to_ms\": number|null, \"provenance\": [t1]}]\n",
    "  \"events\":   [{\"kind\": string, \"title\": string, \"body\": string, \"valid_to_ms\": number|null, \"provenance\": [t1]}]\n",
    "  \"assertions\": same shape as events\n",
    "  \"worthless_turn_uids\": [t1]\n",
    "\"provenance\" is a non-empty array of batch refs (t1 .. tN) copied verbatim from the brackets in the input; the program turns each ref back into the turn it labels. ",
    "Ids, timestamps, device names and ownership are assigned by the program and must never be emitted here.",
);

#[derive(Debug, Clone)]
pub struct Draft {
    pub kind: String,
    pub title: String,
    pub body: String,
    pub valid_to_ms: Option<i64>,
    pub aliases: Vec<String>,
    pub uids: Vec<String>,
}

impl Draft {
    /// 锚角色从批次行查，不从模型读（归属表里 `provenance.anchor_role` 是 `Actor::Program`）。
    pub fn to_provenance(&self, batch: &Batch) -> Result<Vec<Provenance>, ContractError> {
        self.uids
            .iter()
            .map(|uid| {
                batch
                    .role_of(uid)
                    .map(|role| Provenance { turn_uid: uid.clone(), anchor_role: role })
                    .ok_or_else(|| ContractError::RoleUnreadable { uid: uid.clone() })
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExtractOutput {
    pub entities: Vec<Draft>,
    pub cards: Vec<Draft>,
    pub events: Vec<Draft>,
    pub assertions: Vec<Draft>,
    pub worthless: Vec<String>,
    /// 内容条目总数（`entities` 是**过滤后**的清单）。**为 0 且 worthless 也为 0 时调用方必须判失败**
    /// （`has_content`）。被单条丢弃的坏 kind 实体不是内容。
    pub content_items: usize,
    /// A45：模型这一批给了几条实体（**分母**，在 kind 闸门之前计数）。
    pub entities_seen: usize,
    /// A39：kind 归一后不在六词内、被单条丢弃的实体数。计数点在 `filter_entity_kinds`。
    pub kinds_rejected: usize,
}

/// kind 归一：裁空白 + 小写。全 workspace 只有这一份尺子（`extract.rs` 落库前也用它）。
pub(crate) fn normalize_kind(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

/// 坏 kind 必须**在出处校验之前**单条丢弃（A39）：否则 `{"kind":"human", ..., "provenance":[]}`
/// 会以 `ProvenanceEmpty` 把整批拖死——一条本可以单独拒收的草稿炸掉同批所有好行。
/// 缺 `kind` / `kind` 不是字符串的那两格**不在这里吞**：它们交给 `parse_items` 报 `ContentEmpty`
/// （"先归一再判定"那条通则不变，错误不许消失）。
fn filter_entity_kinds(items: Vec<serde_json::Value>) -> (Vec<serde_json::Value>, usize, usize) {
    let seen = items.len();
    let mut kept = Vec::with_capacity(seen);
    let mut rejected = 0usize;
    for item in items {
        match item.get("kind").and_then(serde_json::Value::as_str) {
            Some(k) if rc_contract::is_valid_entity_kind(&normalize_kind(k)) => kept.push(item),
            Some(_) => rejected += 1,
            None => kept.push(item),
        }
    }
    (kept, seen, rejected)
}

impl ExtractOutput {
    pub fn has_content(&self) -> bool {
        self.content_items > 0 || !self.worthless.is_empty()
    }
}

/// 契约错误。载荷只许是**表名、下标与 uid**：它是运维输出的上游，正文与路径一个字都不许进（守卫 12）。
/// `uid` 那一格有个前提：它装的必须是**过了形状关的** uid（见 `uid_echo`，坑 P-72）。
/// `UnlistedUid` / `WorthlessUnlisted` 恰恰是"这段文本不在我们批次里"的那两条，也就是模型的原文本身，
/// 所以不过形状关它们就是正文走私进运维输出的通道——任务 7 的 `JudgeError` 零载荷是同一通则的另一侧。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContractError {
    #[error("output is not one json object")]
    NotAnObject,
    /// 五键齐全但一条内容都没产出、也没有一条被判无价值（坑 P-04 的正身变体）。
    /// 它**不与 `NotAnObject` 合并**：那一格是"根本不是对象"，这一格是"对象合法但零产出"，
    /// 混成一格 P2 的读者就分不清"模型没答"与"模型答了个空"。
    #[error("output produced nothing")]
    ZeroOutput,
    #[error("missing key {0}")]
    MissingKey(&'static str),
    #[error("key {0} is not an array")]
    KeyNotArray(&'static str),
    #[error("item {index} in {table} is not an object")]
    ItemNotObject { table: &'static str, index: usize },
    #[error("item {index} in {table} has empty content")]
    ContentEmpty { table: &'static str, index: usize },
    #[error("item {index} in {table} has no provenance key")]
    ProvenanceMissing { table: &'static str, index: usize },
    #[error("item {index} in {table} has an empty provenance list")]
    ProvenanceEmpty { table: &'static str, index: usize },
    /// 字段**在场而读不出来**：`valid_to_ms` 不是整数、`aliases` 不是字符串数组、
    /// `provenance` 里混进非字符串。它与 `ContentEmpty` / `ProvenanceEmpty` 分开——那两格说的是
    /// "给了，但是空的"，这一格说的是"给了，但类型对不上"（坑 P-71 的模型输入侧）。
    /// `field` 只许是程序里的静态字段名，永远不许是模型给的文本（守卫 12、坑 P-72）。
    #[error("item {index} in {table} has an unreadable {field}")]
    FieldUnreadable { table: &'static str, index: usize, field: &'static str },
    #[error("provenance uid {uid} is not a turn of this batch")]
    UnlistedUid { uid: String },
    #[error("worthless uid {uid} is not a turn of this batch")]
    WorthlessUnlisted { uid: String },
    #[error("role of turn {uid} is not readable")]
    RoleUnreadable { uid: String },
}

impl ContractError {
    /// 进 `extract_state.reason` 的封闭短语。契约族**全部**落 `schema`：
    /// P2 的读者要区分的是"为什么没蒸出东西"这一层，不是"哪个键错了"那一层（那是日志的事，而本阶段没有日志面）。
    /// 这里**故意不写 `_ =>`**：穷尽列举意味着新增变体时必须在这里表一次态，
    /// 而不是被通配臂静默归进 `schema`。（若 clippy 对单臂穷尽 match 有意见，允许改写成
    /// `const SCHEMA: &str = "schema";` 后逐臂返回同一个常量——**不许**用通配臂兜住未来变体。）
    pub fn slug(&self) -> &'static str {
        match self {
            ContractError::NotAnObject
            | ContractError::ZeroOutput
            | ContractError::MissingKey(_)
            | ContractError::KeyNotArray(_)
            | ContractError::ItemNotObject { .. }
            | ContractError::ContentEmpty { .. }
            | ContractError::ProvenanceMissing { .. }
            | ContractError::ProvenanceEmpty { .. }
            | ContractError::FieldUnreadable { .. }
            | ContractError::UnlistedUid { .. }
            | ContractError::WorthlessUnlisted { .. }
            | ContractError::RoleUnreadable { .. } => "schema",
        }
    }
}

/// 归属由程序算（守卫 11：`scope_kind` / `scope_key` 是 P 独占列，模型一个字节都递不进来）。
pub fn scope_for(table: MemTable, kind: &str, title: &str, batch: &Batch) -> Scope {
    match table {
        MemTable::Anchor => Scope {
            kind: if kind == "person" { ScopeKind::Person } else { ScopeKind::Entity },
            key: normalize_body(title),
        },
        // 这里同样**故意不写 `_ =>`**（与上面 `slug()` 一条纪律）：`MemTable` 今天是两格，
        // 而 §1 的表清单 `MEMORY_TABLES` 比它宽得多（阶段 3 起含 relationship/attention）——将来新增一张候选表时 `prefix()`/`parse()`
        // 会编译不过，但那一格若落到通配臂就是**静默的 `ScopeKind::Project`**。归属不许静默。
        MemTable::Card => Scope {
            kind: ScopeKind::Project,
            key: batch.scope_key(),
        },
    }
}

/// 缺席 / 非字符串 ⇒ 空串。这一处折成默认值是**安全**的：紧跟着的 `ContentEmpty` 会把这一条
/// 判成错误——P-71 禁的是"错误消失"，不是"先归一再判定"。
fn str_field(item: &serde_json::Map<String, serde_json::Value>, key: &str) -> String {
    item.get(key).and_then(serde_json::Value::as_str).unwrap_or_default().to_string()
}

/// `turn_uid` 的形状 = 它唯一出处 `rc_contract::compute_turn_uid` 的 `hex()`（`format!("{b:02x}")`）
/// ⇒ **64 位小写十六进制**。这是"模型递进来的这段文本能不能当身份值回显"的判据（坑 P-72）。
fn is_turn_uid(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 进错误载荷的 uid：形状对 ⇒ 原样回显（运维要靠它定位到具体那一轮）；形状不对 ⇒ 只回显长度，
/// 内容一个字都不出。守卫 12 的"零正文出口"落到这一层的形状就是这一句。
///
/// 形状对有两支：64 位十六进制（legacy 支），以及批内位置别名 `t1..tN`——后者的语法由
/// [`crate::batch::ref_index`] 定，字符集只有 `t` 与数字、长度也钉在批上限内（这里再收一道
/// ≤5 字符），所以回显它不构成正文走私，而"引错了哪一格"正是运维要读的那一格。
fn uid_echo(uid: &str) -> String {
    if is_turn_uid(uid) || (uid.len() <= 5 && crate::batch::ref_index(uid).is_some()) {
        uid.to_string()
    } else {
        format!("<not-a-uid {} chars>", uid.chars().count())
    }
}

/// 模型回的一个引用 ⇒ 本批真实 uid。`None` = 这个引用不在本批。
///
/// 两条支路，不是两套编号：
/// 1. 批内位置别名 `t1..tN`——新 prompt **唯一**会产生的形状，编号规则住在
///    [`crate::batch::turn_ref`]／[`Batch::uid_of_ref`]；
/// 2. **legacy**：整串 64 位 uid（老回放夹具、老节点缓存里的回复都是这个形状）。
///    兼容期收着它，好让夹具清理（W23b）单独做；新 prompt 不再产生它。
///
/// 解析顺序无所谓：64 位 hex 不可能以 `t` 开头，两支不会撞。
fn resolve_ref<'a>(batch: &'a Batch, r: &str) -> Option<&'a str> {
    batch.uid_of_ref(r).or_else(|| batch.row(r).map(|row| row.turn_uid.as_str()))
}

fn parse_items(arr: &[serde_json::Value], table: &'static str, batch: &Batch, need_body: bool)
    -> Result<Vec<Draft>, ContractError> {
    let mut out = Vec::new();
    for (index, v) in arr.iter().enumerate() {
        let Some(item) = v.as_object() else {
            return Err(ContractError::ItemNotObject { table, index });
        };
        let kind = str_field(item, "kind");
        let title = str_field(item, "title");
        let body = if need_body { str_field(item, "body") } else { String::new() };
        // `kind` 与 `title`/`body` 同级：它是分类子，而 `kind TEXT NOT NULL` 没有 CHECK——
        // 空串会写成一行 `kind=''`，P3 的目录（任务 11）按 kind 分桶，那一格就成了垃圾桶。
        if normalize_body(&kind).is_empty()
            || normalize_body(&title).is_empty()
            || (need_body && normalize_body(&body).is_empty())
        {
            return Err(ContractError::ContentEmpty { table, index });
        }
        let prov = item.get(PROVENANCE_KEY).ok_or(ContractError::ProvenanceMissing { table, index })?;
        // 出处清单：**在场而读不出来 = 错误**，不许折成"少一条"（坑 P-71 的模型输入侧）。
        // 同一个 uid 写两遍是同一个锚：`provenance` 的主键是 (mem_table, mem_id, turn_uid)，
        // 去重住在这一层，留给任务 6 的写侧一个规范形状。顺序 = 首次出现序。
        //
        // 去重的判据是**映回之后**的真实 uid，不是模型写的那串引用：同一个锚用两种拼法
        // （`t1` 与那一轮的完整 uid）报两遍，语义上仍是一格，不去重就会撞主键。
        let bad_prov = ContractError::FieldUnreadable { table, index, field: "provenance" };
        let mut uids: Vec<String> = Vec::new();
        for x in prov.as_array().ok_or_else(|| bad_prov.clone())? {
            let cited = x.as_str().ok_or_else(|| bad_prov.clone())?;
            // 未知引用走既有的 batch 校验错误，**不静默**（不折成空串、也不落某个默认行）。
            let uid = resolve_ref(batch, cited)
                .ok_or_else(|| ContractError::UnlistedUid { uid: uid_echo(cited) })?;
            if uids.iter().any(|seen| seen.as_str() == uid) {
                continue;                       // 已收过：同一个锚，不再收一次
            }
            uids.push(uid.to_string());
        }
        if uids.is_empty() {
            return Err(ContractError::ProvenanceEmpty { table, index });
        }
        // 可选字段：**缺席与 `null` 是合法的"没有"**，在场而读不出来才是错误（同上一条通则）。
        let valid_to_ms = match item.get("valid_to_ms") {
            None => None,
            Some(v) if v.is_null() => None,
            Some(v) => Some(v.as_i64().ok_or(ContractError::FieldUnreadable {
                table, index, field: "valid_to_ms",
            })?),
        };
        let aliases = match item.get("aliases") {
            None => Vec::new(),
            Some(v) if v.is_null() => Vec::new(),
            Some(v) => {
                let bad = ContractError::FieldUnreadable { table, index, field: "aliases" };
                let mut acc = Vec::new();
                for a in v.as_array().ok_or_else(|| bad.clone())? {
                    acc.push(a.as_str().ok_or_else(|| bad.clone())?.to_string());
                }
                acc
            }
        };
        out.push(Draft { kind, title, body, valid_to_ms, aliases, uids });
    }
    Ok(out)
}

pub fn parse_output(json: &str, batch: &Batch) -> Result<ExtractOutput, ContractError> {
    let doc: serde_json::Value = serde_json::from_str(json).map_err(|_| ContractError::NotAnObject)?;
    let obj = doc.as_object().ok_or(ContractError::NotAnObject)?;   // 绝不"解析失败就给空对象"
    for key in KEYS {
        if !obj.contains_key(key) {
            return Err(ContractError::MissingKey(key));
        }
    }
    let pick = |key: &'static str| -> Result<Vec<serde_json::Value>, ContractError> {
        obj[key].as_array().cloned().ok_or(ContractError::KeyNotArray(key))
    };
    let (entity_items, entities_seen, kinds_rejected) = filter_entity_kinds(pick("entities")?);
    let entities = parse_items(&entity_items, "entity", batch, false)?;
    let cards = parse_items(&pick("cards")?, "card", batch, true)?;
    let events = parse_items(&pick("events")?, "event", batch, true)?;
    let assertions = parse_items(&pick("assertions")?, "assertion", batch, true)?;
    let worthless = pick("worthless_turn_uids")?
        .iter()
        .enumerate()
        .map(|(index, v)| -> Result<String, ContractError> {
            // 非字符串那一格与 `provenance` 同判（坑 P-71 的模型输入侧）：不许折成空串、
            // 再把它报成"这一条不在批次里"——同一个坏法在两支清单上必须同一个形状。
            let cited = v.as_str().ok_or(ContractError::FieldUnreadable {
                table: "worthless_turn_uids", index, field: "uid",
            })?;
            // 与 `provenance` 同一条映射：位置别名与 legacy 完整 uid 都收，未知引用报错。
            let uid = resolve_ref(batch, cited)
                .ok_or_else(|| ContractError::WorthlessUnlisted { uid: uid_echo(cited) })?;
            Ok(uid.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let content_items = entities.len() + cards.len() + events.len() + assertions.len();
    let out = ExtractOutput { entities, cards, events, assertions, worthless, content_items,
                              entities_seen, kinds_rejected };
    // 五键齐全却零产出 = "写零行 = 错误"的第一个执法点。
    // 它不许被读成"这批 turn 都没信息量"——没信息量必须由 worthless 数组逐条表态。
    if !out.has_content() {
        return Err(ContractError::ZeroOutput);
    }
    Ok(out)
}
