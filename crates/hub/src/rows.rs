//! `POST /v1/rows` 的吸收面：节点把离线自蒸产生的新行按表送上来，中心整批写进记忆库，
//! 再把载荷里 `provenance.turn_uid` 的去重集合记进水位库的 `pre_distilled_turn`。
//!
//! 三条顺序是判定的一部分：
//! ① **外键安全**：按 [`INSERT_ORDER`] 逐表落行，`alias` 因此在 `anchor` 之后；
//! ② **两段式提交**：记忆行（连同资源归属）一个事务先提交，水位标记后写。第二段失败 ⇒ 503，
//!    节点不 pull、带着同一载荷重试；第一段是幂等的（id 内容寻址 + `ON CONFLICT DO NOTHING`），
//!    重试只补标记；
//! ③ **归属只由中心推导**（S1b-C2b）：载荷里的 `resource_owner`/`resource_grant` 一律 400，
//!    owner 从 hub **自己的** `provenance.turn_uid → turns → sessions` 推（[`session_owners`]），
//!    推导值只是"能填什么"，不是权限来源。跨库两次独立查询：先读 transcript，再开 memory 事务，
//!    中间没有 JOIN、没有 ATTACH。
//!
//! 表名与列名都不来自载荷正文：表名过 [`allowed_table`]（`MEMORY_TABLES` 去掉
//! `{attention, resource_owner, resource_grant, review_item, review_decision, card_verdict_event}`），
//! 列名过 [`allowed_columns`]（从 `OWNERSHIP` 现算）。载荷里的字符串永远只作为**值**绑定，
//! 不拼进 SQL。
//!
//! ## S3c-4：`fact` 行不再直写，改走"程序闸 → 入队 → 同步窄审 → apply"
//!
//! 分派点只有一处：解析时 `table == "fact"` 的行**不进** `parsed`（直写集合）也不进归属
//! 目标；其余 25 张表（`INSERT_ORDER` 除 `fact` 外的全部）照旧在第一段事务里直写。fact 行逐行建
//! [`RowsCandidate`](rc_distill::review::RowsCandidate)（owner/scope 沿用本文件的推导），
//! 过 [`gate_rows`](rc_distill::review::gate_rows)：
//!
//! - **闸拒**：该行单独入队一个 item（一行一个），记一条 `program/reject(<code>)`，计
//!   `rows_rejected`；不写库；
//! - **闸过**：按 ≤[`REVIEW_MAX_ITEMS`](rc_distill::review::REVIEW_MAX_ITEMS) 行一批组
//!   `review.rows.v1` item（同一批的会话/设备/归属/scope 必须一致，见 [`GroupKey`]），
//!   首次 `pending_review` 时记一条 `program/pass`，随后逐项顺序调
//!   [`run_item`](rc_distill::review::run::run_item)；
//! - **没有后端而载荷含 fact 行** ⇒ 在任何 fact 入队/写库之前返回 503 `review_unavailable`
//!   （结构行已直写，重试幂等）；
//! - **任何一次入队/窄审/写库失败** ⇒ 503（`store_unavailable`）且**水位一个都不打**；
//!   **行没有结论**的分支也在这条线上（S3c-fix）：`Stale`（本请求的 item 被顶替）、
//!   `Conflict`（另一写者推进）、`PendingReview`/`Approved`（引擎没推进）都不许 200
//!   ——200 + 打水会让节点以为这批已经处理完，那些行永久不入库；
//!   只有全部成功才把载荷里的 `turn_uid` 写进 `pre_distilled_turn`。
//!
//! S3c-fix 的计数口径（复审 P1-d）：每一行恰好落一格，四格之和恰等于 `rows_reviewed`：
//!
//! - `rows_absorbed`：写进 fact（含已 applied 重试的整 item）；
//! - `rows_duplicate`：窄审过了但 id 已存在（没写进去）；
//! - `rows_rejected`：程序闸或窄审拒；
//! - `rows_quarantined`：窄审不确定/隔离。
//!
//! 混合 item 里被拒/隔离的行不再从读数里蒸发。
//!
//! 已 `applied` 的 item（重试命中）只计数、不重写：`run_item` 的 Applied 分支不落库，
//! 本次按 item 的行数计入 `rows_absorbed`。

use rc_distill::batch::{Batch, TurnRow};
use rc_distill::review::gate_rows::{gate_rows, RowsCandidate};
use rc_distill::review::rows_payload::{rows_new_item, rows_payload, RowsRowInput};
use rc_distill::review::run::{run_item, write_watermark, RunError};
use rc_distill::review::{owner_json, program_decided, record_program, PROGRAM_PASS};
use rc_llm::LlmBackend;
use rc_memory::owner::{self, OwnerCtx, OwnerRow, Visibility};
use rc_memory::review::{ReviewState, ReviewVerdict};
use rc_memory::schema::{MEMORY_TABLES, OWNERSHIP};
use rc_memory::{BatchCtx, Scope};
use rc_store::{MemoryHandle, Root, TranscriptHandle};
use rusqlite::types::Value;
use serde_json::{json, Map, Value as Json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// 一次载荷的行数上限（HTTP 的 64 MiB 之外再加一道有界工作）。超了按 `BadRequest` 拒。
pub const MAX_ROWS_PER_BATCH: usize = 20_000;

/// 一次 transcript 查询的 `turn_uid` 个数上限（SQLite 的变量数有上限；500 是保守的一档）。
const SESSION_CHUNK: usize = 500;

/// 外键安全的吸收顺序。`alias` 引用 `anchor`，所以前者必在后者之后；
/// 其余表之间今天没有 FK，顺序仍固定下来——同一批载荷两次吸收的可观测顺序不许抖。
/// 覆盖 `MEMORY_TABLES` 去掉排除集后的全部 26 张 canonical 表。
/// 排除集（S1b-C2a）：`attention` 是 Center 独占；`resource_owner`/`resource_grant` 是
/// 资源归属——节点永不许上传归属（伪造面），所以它们只能由中心侧推导出来。
/// S3a 起再加 `review_item`/`review_decision`：审查队列与判定台账只在中心写，
/// 节点永不许上传审查表/判定（这同样是一张伪造面）。
/// G1 刀 B：`card_sink` **不在**排除集里——归档章是消费面状态，副本要读出与中心同样的
/// 召回行为（章丢了，副本就会把归档卡当 main 取）。它引用 `card(id)`，所以排在 `card` 之后。
/// 波二刀 A：`card_reuse_event` 同理不在排除集里——复用反馈是消费面状态，副本的召回加成
/// 要读到与中心同一批「worked」读数。它同样引用 `card(id)`，排在 `card` 之后。
const INSERT_ORDER: [&str; 26] = [
    "anchor",
    "alias",
    "attribute",
    "relation",
    "fact",
    "state_assertion",
    "utterance",
    "episode",
    "episode_fact",
    "card",
    "card_sink",
    "card_reuse_event",
    "mention",
    "memory_mention",
    "fact_link",
    "embedding",
    "provenance",
    "attestation",
    "retirement",
    "memory_op",
    "anchor_merge",
    "memory_stats",
    "time_expr",
    "extraction_run",
    "extraction_batch",
    "extraction_issue",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Absorbed {
    pub rows_in: usize,
    pub absorbed: usize,
    pub skipped_duplicate: usize,
    pub turns_marked: usize,
    /// S3c-4：载荷里的 fact 行总数（本次送审的行数）。
    pub rows_reviewed: usize,
    /// 窄审通过并写进 `fact` 的行数（含"已 applied 的重试"那些行）。
    pub rows_absorbed: usize,
    /// 被拒的 fact 行数（程序闸或窄审）。
    pub rows_rejected: usize,
    /// 被隔离的 fact 行数。
    pub rows_quarantined: usize,
    /// 窄审 pass、但 id 已在 `fact` 表里（`ON CONFLICT DO NOTHING`）的 fact 行数。
    ///
    /// S3c-fix：与 `rows_absorbed` 分开记，否则"重复"会被读成"审过又写进去了"。
    /// S3c-fix2 起 HTTP 响应也带这一格；自洽断言用
    /// `rows_absorbed + rows_rejected + rows_quarantined + rows_duplicate == rows_reviewed`。
    pub rows_duplicate: usize,
    /// 非 fact 行（结构行）本次插入的行数。
    pub rows_direct: usize,
}

/// 四种失败对应 HTTP 的几格：`BadRequest` / `BadRows` ⇒ 400，`Store` ⇒ 503，
/// `ReviewUnavailable`（载荷含 fact 行但中心没有在线审核座位）⇒ 503 `review_unavailable`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbsorbError {
    BadRequest,
    BadRows,
    Store,
    ReviewUnavailable,
}

/// 表白名单：`MEMORY_TABLES` 去掉排除集（`attention` 是 Center 独占；两张归属侧表是
/// "节点永不许上传归属"的伪造面，C2a 起一并拒绝；S3a 的两张审查表同理——
/// 节点永不许上传审查表/判定；W15a 的 `card_verdict_event` 是中心侧的背书账，
/// 只由 audit/review 在中心写，节点上传它等于伪造"谁盖的"），且必须已在 [`INSERT_ORDER`] 里排过序。
/// 只认一次来源、不手抄第二份名单。
fn allowed_table(name: &str) -> Option<&'static str> {
    if matches!(
        name,
        "attention" | "resource_owner" | "resource_grant" | "review_item" | "review_decision"
            | "card_verdict_event"
    ) || !MEMORY_TABLES.contains(&name)
    {
        return None;
    }
    INSERT_ORDER.iter().copied().find(|t| *t == name)
}

/// 每张表允许的列集合，从 `OWNERSHIP` **现算**——归属表是列清单的唯一出处。
fn allowed_columns(table: &str) -> Vec<&'static str> {
    OWNERSHIP
        .iter()
        .filter(|o| o.table == table)
        .map(|o| o.column)
        .collect()
}

/// JSON 标量 → SQLite 值。数组 / 对象不是标量；`Number` 只收 i64 或有限 f64。
fn sql_value(v: &Json) -> Option<Value> {
    match v {
        Json::Null => Some(Value::Null),
        Json::Bool(b) => Some(Value::Integer(i64::from(*b))),
        Json::String(s) => Some(Value::Text(s.clone())),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(Value::Integer(i))
            } else {
                let f = n.as_f64()?;
                f.is_finite().then_some(Value::Real(f))
            }
        }
        Json::Array(_) | Json::Object(_) => None,
    }
}

/// 行载荷里**只用于填充**的五个值（S1b-C2b）。它们不是权限来源：
/// `scope_kind` 还要过 `owner::normalize_scope_kind`，`visibility` 只由 session + 行 scope 算。
#[derive(Debug, Clone, Default)]
struct RowMeta {
    scope_kind: Option<String>,
    scope_key: Option<String>,
    source_agent: Option<String>,
    device_id: Option<String>,
    created_at_ms: Option<i64>,
}

fn row_meta_of(values: &[(&'static str, Value)]) -> RowMeta {
    let mut meta = RowMeta::default();
    for (column, value) in values {
        match (*column, value) {
            ("scope_kind", Value::Text(s)) => meta.scope_kind = Some(s.clone()),
            ("scope_key", Value::Text(s)) => meta.scope_key = Some(s.clone()),
            ("source_agent", Value::Text(s)) => meta.source_agent = Some(s.clone()),
            ("device_id", Value::Text(s)) => meta.device_id = Some(s.clone()),
            ("created_at_ms", Value::Integer(i)) => meta.created_at_ms = Some(*i),
            _ => {}
        }
    }
    meta
}

/// 一个 turn 所属 session 的归属读数（`turns JOIN sessions` 的一行，NULL 已 COALESCE）。
/// `Hash`/`Eq` 用来做"整批去重"：整份结构相同才算同一个 session owner——
/// scope/retention/source_agent 任一不同都算分歧，分歧一律退到保守兜底，不随便挑一个。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SessionOwner {
    owner_agent: String,
    owner_principal_id: Option<String>,
    visibility: String,
    retention: String,
    scope_kind: String,
    scope_key: String,
    source_agent: String,
}

/// 在 memory 事务**之外**读 transcript：按 `turn_uid` 分块（≤500/块）查
/// `turns JOIN sessions`。查不到 session 的 uid 不出现在结果里（由调用方走 unresolved）。
fn session_owners(
    t: &TranscriptHandle,
    turns: &BTreeSet<String>,
) -> Result<HashMap<String, SessionOwner>, AbsorbError> {
    let mut out = HashMap::new();
    let uids: Vec<&str> = turns.iter().map(String::as_str).collect();
    for chunk in uids.chunks(SESSION_CHUNK) {
        let slots: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT t.turn_uid,
                    COALESCE(s.owner_agent_id, s.source_agent) AS owner_agent,
                    s.owner_principal_id,
                    COALESCE(s.visibility,'private_agent') AS visibility,
                    COALESCE(s.retention,'default') AS retention,
                    COALESCE(s.scope_kind,'project') AS scope_kind,
                    COALESCE(s.scope_key, s.project_hint, s.session_key) AS scope_key,
                    s.source_agent
             FROM turns t JOIN sessions s
               ON s.session_key=t.session_key AND s.device_id=t.device_id
             WHERE t.turn_uid IN ({})",
            slots.join(",")
        );
        let mut st = t.conn().prepare(&sql).map_err(|_| AbsorbError::Store)?;
        let rows = st
            .query_map(rusqlite::params_from_iter(chunk.iter().copied()), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    SessionOwner {
                        owner_agent: r.get(1)?,
                        owner_principal_id: r.get(2)?,
                        visibility: r.get(3)?,
                        retention: r.get(4)?,
                        scope_kind: r.get(5)?,
                        scope_key: r.get(6)?,
                        source_agent: r.get(7)?,
                    },
                ))
            })
            .map_err(|_| AbsorbError::Store)?;
        for row in rows {
            let (uid, owner) = row.map_err(|_| AbsorbError::Store)?;
            out.insert(uid, owner);
        }
    }
    Ok(out)
}

/// 能推导的行：owner 取 session；`created_by_agent` 取行载荷的 `source_agent`（缺则 session 的），
/// `visibility` 取 `min(session, ceiling(行 scope))`；`scope_kind` 走归一，`scope_key` 行优先。
fn derived_row(
    mem_table: &str,
    mem_id: &str,
    session: &SessionOwner,
    meta: &RowMeta,
    now_ms: i64,
) -> OwnerRow {
    // 行 scope 是记忆行自己的 scope（载荷列的副本，只用于填充）；缺列时按 project 的下界算。
    let kind = owner::normalize_scope_kind(meta.scope_kind.as_deref().unwrap_or(""));
    OwnerRow {
        mem_table: mem_table.to_string(),
        mem_id: mem_id.to_string(),
        owner_principal_id: session.owner_principal_id.clone(),
        owner_agent_id: session.owner_agent.clone(),
        created_by_agent: meta
            .source_agent
            .clone()
            .unwrap_or_else(|| session.source_agent.clone()),
        visibility: owner::effective(owner::parse_visibility(&session.visibility), kind),
        retention: session.retention.clone(),
        scope: Scope {
            kind,
            key: meta
                .scope_key
                .clone()
                .unwrap_or_else(|| session.scope_key.clone()),
        },
        source_agent: meta.source_agent.clone(),
        device_id: meta.device_id.clone(),
        created_at_ms: meta.created_at_ms.unwrap_or(now_ms),
    }
}

/// 推不出的行（无出处 / 跨 session / 整批无唯一 owner）：设计 §6 的 unresolved 标法——
/// owner 与 produced-by 都取行 `source_agent`（anchor 没有该列 ⇒ `unknown`），
/// **private_agent + default**，scope 取行。不写 NULL owner、不建待审表。
fn unresolved_row(mem_table: &str, mem_id: &str, meta: &RowMeta, now_ms: i64) -> OwnerRow {
    let agent = meta
        .source_agent
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    OwnerRow {
        mem_table: mem_table.to_string(),
        mem_id: mem_id.to_string(),
        owner_principal_id: None,
        owner_agent_id: agent.clone(),
        created_by_agent: agent,
        visibility: Visibility::PrivateAgent,
        retention: "default".to_string(),
        scope: Scope {
            kind: owner::normalize_scope_kind(meta.scope_kind.as_deref().unwrap_or("")),
            key: meta.scope_key.clone().unwrap_or_default(),
        },
        source_agent: meta.source_agent.clone(),
        device_id: meta.device_id.clone(),
        created_at_ms: meta.created_at_ms.unwrap_or(now_ms),
    }
}

// ── S3c-4：fact 行的逐行构图（候选 / 入队 / 同步审）────────────────────────────

/// **同一批 fact 行**的身份：会话、设备、源 agent 与整份归属信封逐字相同。
///
/// 为什么分组键里要有归属与 scope：`review.rows.v1` 的信封只有**一份** `owner`，重放与
/// apply 都按它写整批行；把 scope 不同的行塞进同一 item，后写的那几行会顶着前一批的
/// scope 落库。分组因此是"同一份信封"的等价类，`≤10 行/批` 只在类内切。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GroupKey {
    session_key: String,
    device_id: String,
    source_agent: String,
    /// `owner_value(owner, scope)` 的序列化：六键逐字相同才算同一份信封。
    owner_json: String,
}

/// 一行 fact 的候选与它入队所需的全部输入（逐行构图的产物）。
struct FactSeed {
    /// 行原列（键集合已过 `allowed_columns("fact")`）。
    values: Map<String, Json>,
    /// 该行 provenance 覆盖的轮（去重、升序）；证据轮不在其中 ⇒ 闸判 `payload_shape`。
    turn_uids: Vec<String>,
    evidence_turn_uid: String,
    /// 既有事实的异值（`{"value_text":旧值,"turn_uid":旧证据轮}`）；无冲突为 `None`。
    conflict: Option<Json>,
    session_key: String,
    device_id: String,
    source_agent: String,
    owner: OwnerCtx,
    scope: Scope,
}

impl FactSeed {
    fn group_key(&self) -> GroupKey {
        GroupKey {
            session_key: self.session_key.clone(),
            device_id: self.device_id.clone(),
            source_agent: self.source_agent.clone(),
            owner_json: owner_json(&self.owner, &self.scope),
        }
    }
}

/// 行载荷的五个填充值（JSON 版）：与 [`row_meta_of`] 同一条读法，只是值来自载荷正文
/// 而不是已转好的 SQLite 值。
fn row_meta_of_json(values: &Map<String, Json>) -> RowMeta {
    let mut meta = RowMeta::default();
    for (column, value) in values {
        match (column.as_str(), value) {
            ("scope_kind", Json::String(s)) => meta.scope_kind = Some(s.clone()),
            ("scope_key", Json::String(s)) => meta.scope_key = Some(s.clone()),
            ("source_agent", Json::String(s)) => meta.source_agent = Some(s.clone()),
            ("device_id", Json::String(s)) => meta.device_id = Some(s.clone()),
            ("created_at_ms", Json::Number(n)) => meta.created_at_ms = n.as_i64(),
            _ => {}
        }
    }
    meta
}

/// 能推导的行：会话 owner + 行 scope。与 [`derived_row`] 同一套列（visibility 取
/// `min(session, ceiling(行 scope))`、`scope_key` 行优先），只是这里要的是
/// `OwnerCtx` + `Scope` 这一对（入队信封与 apply 的写库形参）。
fn derived_owner(session: &SessionOwner, meta: &RowMeta) -> (OwnerCtx, Scope) {
    let kind = owner::normalize_scope_kind(meta.scope_kind.as_deref().unwrap_or(""));
    (
        OwnerCtx {
            owner_principal_id: session.owner_principal_id.clone(),
            owner_agent_id: session.owner_agent.clone(),
            visibility: owner::effective(owner::parse_visibility(&session.visibility), kind),
            retention: session.retention.clone(),
        },
        Scope {
            kind,
            key: meta
                .scope_key
                .clone()
                .unwrap_or_else(|| session.scope_key.clone()),
        },
    )
}

/// 推不出的行：`owner_agent_id=""`——这是闸门 [`OWNER_UNRESOLVED`](rc_distill::review::gate_rows::OWNER_UNRESOLVED)
/// 的判据（`RowsCandidate.owner.owner_agent_id` 空白 ⇒ 闸拒），所以它永远不会被送审。
fn unresolved_owner(meta: &RowMeta) -> (OwnerCtx, Scope) {
    let kind = owner::normalize_scope_kind(meta.scope_kind.as_deref().unwrap_or(""));
    (
        OwnerCtx {
            owner_principal_id: None,
            owner_agent_id: String::new(),
            visibility: Visibility::PrivateAgent,
            retention: "default".to_string(),
        },
        Scope {
            kind,
            key: meta.scope_key.clone().unwrap_or_default(),
        },
    )
}

/// 批内 fact 轮的转录读数：只拿会话/设备/源时刻（正文与角色由重放自己去读）。
#[derive(Debug, Clone)]
struct TurnFacts {
    session_key: String,
    device_id: String,
    created_at_ms: i64,
}

/// 按 `turn_uid` 分块（≤[`SESSION_CHUNK`]）读 `turns`。查不到的 uid 不出现（由闸判
/// `turn_missing`）。与 [`session_owners`] 是同一次读法的两个投影，互不替代。
fn turn_facts(
    t: &TranscriptHandle,
    turns: &BTreeSet<String>,
) -> Result<HashMap<String, TurnFacts>, AbsorbError> {
    let mut out = HashMap::new();
    let uids: Vec<&str> = turns.iter().map(String::as_str).collect();
    for chunk in uids.chunks(SESSION_CHUNK) {
        let slots: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT turn_uid, session_key, device_id, created_at_ms FROM turns
             WHERE turn_uid IN ({})",
            slots.join(",")
        );
        let mut st = t.conn().prepare(&sql).map_err(|_| AbsorbError::Store)?;
        let rows = st
            .query_map(rusqlite::params_from_iter(chunk.iter().copied()), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    TurnFacts {
                        session_key: r.get(1)?,
                        device_id: r.get(2)?,
                        created_at_ms: r.get(3)?,
                    },
                ))
            })
            .map_err(|_| AbsorbError::Store)?;
        for row in rows {
            let (uid, facts) = row.map_err(|_| AbsorbError::Store)?;
            out.insert(uid, facts);
        }
    }
    Ok(out)
}

/// 既有事实的异值（S3c-4 §3.2.2）：同 `subject_anchor_id + predicate`、`value_text` 不同
/// 的一行（按 `created_at_ms` 最新）。命中 ⇒ `{"value_text":旧值,"turn_uid":旧证据轮}`，
/// 只作为审核输入（`rows_review_user` 的 `conflict:` 行），不是写库参数。
///
/// S3c-fix（复审 P1-c）：候选查询**必须带 scope**（`scope_kind=? AND scope_key=?`）。
/// 原来只有 subject+predicate，同一个锚在别的项目/别的 agent 下的旧值会进第三方模型
/// 的提示（正文外泄面），也会把"别的项目的旧值"当成本行的矛盾源。
///
/// scope 取**本行将要落成的记忆行 scope**（`derived_owner`/`unresolved_owner` 的那一对：
/// `normalize_scope_kind` + 行优先的 `scope_key`），不是行载荷的原始字符串——中心抽取写
/// 出来的 `fact` 行用的就是这一份归一值，两边同一个词表才比得上。
fn conflict_candidate(
    m: &MemoryHandle,
    values: &Map<String, Json>,
    scope: &Scope,
) -> Result<Option<Json>, AbsorbError> {
    let (Some(subject), Some(predicate)) = (
        values.get("subject_anchor_id").and_then(Json::as_str),
        values.get("predicate").and_then(Json::as_str),
    ) else {
        return Ok(None);
    };
    let id = values.get("id").and_then(Json::as_str).unwrap_or_default();
    let value_text = values.get("value_text").cloned().unwrap_or(Json::Null);
    let bound = sql_value(&value_text).ok_or(AbsorbError::BadRequest)?;
    let mut st = m
        .conn()
        .prepare(
            "SELECT value_text, evidence_turn_uid FROM fact
              WHERE subject_anchor_id=?1 AND predicate=?2 AND id<>?3 AND value_text IS NOT ?4
                AND scope_kind=?5 AND scope_key=?6
              ORDER BY created_at_ms DESC, id LIMIT 1",
        )
        .map_err(|_| AbsorbError::Store)?;
    let mut rows = st
        .query(rusqlite::params![
            subject,
            predicate,
            id,
            bound,
            scope.kind.as_str(),
            scope.key
        ])
        .map_err(|_| AbsorbError::Store)?;
    match rows.next().map_err(|_| AbsorbError::Store)? {
        None => Ok(None),
        Some(row) => {
            let old: Option<String> = row.get(0).map_err(|_| AbsorbError::Store)?;
            let old_uid: String = row.get(1).map_err(|_| AbsorbError::Store)?;
            Ok(Some(json!({ "value_text": old, "turn_uid": old_uid })))
        }
    }
}

/// 一行 fact → [`FactSeed`]：provenance 轮、首轮会话/设备、owner/scope 推导、冲突候选。
///
/// owner/scope 的三支与结构行同一条（[`derived_row`]/[`unresolved_row`]）：恰好一个
/// session owner ⇒ 用它；无出处 ⇒ 整批唯一 owner；跨 session/无 owner ⇒ unresolved
/// （`owner_agent_id=""`，让闸拒）。
fn fact_seed(
    row: &Map<String, Json>,
    provenance_by_row: &BTreeMap<(String, String), BTreeSet<String>>,
    sessions: &HashMap<String, SessionOwner>,
    payload_owner: Option<&SessionOwner>,
    turn_facts: &HashMap<String, TurnFacts>,
) -> FactSeed {
    let id = row.get("id").and_then(Json::as_str).unwrap_or_default();
    let turn_uids: Vec<String> = provenance_by_row
        .get(&("fact".to_string(), id.to_string()))
        .map(|uids| uids.iter().cloned().collect())
        .unwrap_or_default();
    let evidence_turn_uid = row
        .get("evidence_turn_uid")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();
    let first = turn_uids.first().and_then(|uid| turn_facts.get(uid));
    let session = turn_uids.first().and_then(|uid| sessions.get(uid));
    let meta = row_meta_of_json(row);
    let owners: HashSet<&SessionOwner> = turn_uids
        .iter()
        .filter_map(|uid| sessions.get(uid))
        .collect();
    let (owner, scope) = if owners.len() == 1 {
        derived_owner(owners.into_iter().next().unwrap(), &meta)
    } else if owners.is_empty() {
        match payload_owner {
            Some(session) => derived_owner(session, &meta),
            None => unresolved_owner(&meta),
        }
    } else {
        unresolved_owner(&meta)
    };
    FactSeed {
        values: row.clone(),
        turn_uids,
        evidence_turn_uid,
        conflict: None,
        session_key: first.map(|f| f.session_key.clone()).unwrap_or_default(),
        device_id: first.map(|f| f.device_id.clone()).unwrap_or_default(),
        source_agent: session.map(|s| s.source_agent.clone()).unwrap_or_default(),
        owner,
        scope,
    }
}

/// 一批 fact 行的 `Batch`/`BatchCtx`：轮取并集（升序），`created_at_ms` 从转录逐轮读。
/// 组的成员同会话（[`GroupKey`]），所以重放的 `rows_of` 逐轮比对不会在这里先炸。
fn batch_of(seeds: &[FactSeed], turn_facts: &HashMap<String, TurnFacts>) -> (Batch, BatchCtx) {
    let first = &seeds[0];
    let mut uids: BTreeSet<&str> = BTreeSet::new();
    for seed in seeds {
        uids.extend(seed.turn_uids.iter().map(String::as_str));
    }
    let rows = uids
        .into_iter()
        .map(|uid| {
            let at = turn_facts.get(uid).map(|f| f.created_at_ms).unwrap_or(0);
            TurnRow {
                turn_uid: uid.to_string(),
                // 角色/正文不参与 `batch_ref`（只取 uid 与 created_at_ms）；重放自己读转录。
                role: "human".to_string(),
                body: String::new(),
                created_at_ms: at,
                ingested_at_ms: at,
                speaker: None,
                speaker_anchor_id: None,
            }
        })
        .collect();
    (
        Batch {
            session_key: first.session_key.clone(),
            device_id: first.device_id.clone(),
            source_agent: first.source_agent.clone(),
            project_hint: None,
            rows,
        },
        BatchCtx {
            source_agent: first.source_agent.clone(),
            device_id: first.device_id.clone(),
        },
    )
}

/// `review.rows.v1` 的 `run_id`。**常量**：载荷指纹是入队去重的键，时钟进载荷会让同一批
/// 重试变成另一条 item。
const ROWS_UPLOAD_RUN_ID: &str = "rows-upload";

/// 一行候选 → 载荷 item（一行一个；闸拒的行只入队不入审）。
fn row_item(seed: &FactSeed, batch: &Batch, ctx: &BatchCtx) -> rc_memory::review::NewItem {
    let payload = rows_payload(
        ROWS_UPLOAD_RUN_ID,
        batch,
        ctx,
        &seed.owner,
        &seed.scope,
        &[RowsRowInput {
            values: seed.values.clone(),
            evidence_turn_uid: seed.evidence_turn_uid.clone(),
            conflict: seed.conflict.clone(),
        }],
    );
    rows_new_item(batch, owner_json(&seed.owner, &seed.scope), &payload)
}

/// 记一条程序判定；**队列竞态不算存储故障**（S3c-fix，复审 P1-3）。
///
/// `program_decided` 与 `record_program` 之间没有锁：两个写者同时看到"程序阶段还没表态"
/// 时，输的一方拿到 `review: illegal transition …`（`MemError::is_review_conflict`）。
/// 语义是"另一写者已记"——忽略它并继续（引擎自己按状态分派），不再折成 503
/// `store_unavailable`（那是存储故障的码，节点据此重试一个永远同样结果的请求）。
fn record_program_once(
    m: &MemoryHandle,
    id: i64,
    verdict: ReviewVerdict,
    code: &str,
    payload_sha256: &str,
    now_ms: i64,
) -> Result<(), AbsorbError> {
    match record_program(m, id, verdict, code, payload_sha256, now_ms) {
        Ok(_) => Ok(()),
        Err(error) if error.is_review_conflict() => Ok(()),
        Err(_) => Err(AbsorbError::Store),
    }
}

pub fn absorb(
    root: &mut Root,
    body: &[u8],
    now_ms: i64,
    backend: Option<&dyn LlmBackend>,
) -> Result<Absorbed, AbsorbError> {
    debug_assert_eq!(
        INSERT_ORDER.iter().copied().collect::<BTreeSet<_>>(),
        MEMORY_TABLES
            .iter()
            .copied()
            .filter(|t| !matches!(
                *t,
                "attention" | "resource_owner" | "resource_grant" | "review_item" | "review_decision"
                    | "card_verdict_event"
            ))
            .collect::<BTreeSet<_>>(),
        "INSERT_ORDER 与 MEMORY_TABLES − {{attention,resource_owner,resource_grant,\
         review_item,review_decision,card_verdict_event}} 漂移"
    );

    let doc: Json = serde_json::from_slice(body).map_err(|_| AbsorbError::BadRequest)?;
    let obj = doc.as_object().ok_or(AbsorbError::BadRequest)?;
    if obj.get("api_version").and_then(Json::as_i64) != Some(1) {
        return Err(AbsorbError::BadRequest);
    }
    let tables = obj
        .get("tables")
        .and_then(Json::as_object)
        .ok_or(AbsorbError::BadRequest)?;

    let mut parsed: BTreeMap<&'static str, Vec<Vec<(&'static str, Value)>>> = BTreeMap::new();
    // S3c-4：fact 行不进 `parsed`（直写集合）——它们逐行走闸/入队/窄审，写库在 apply。
    let mut fact_rows: Vec<Map<String, Json>> = Vec::new();
    let mut rows_in = 0usize;
    let mut turns: BTreeSet<String> = BTreeSet::new();
    // 归属推导的两张索引（S1b-C2b）：
    // ① provenance 行给出的 `(mem_table, mem_id) → {turn_uid}`；
    // ② 载荷里"自己就是一行记忆"的行（有 TEXT 列 `id`）的 `(表名, id)` 及其填充元数据。
    // 两者都只用表名/列名当键，不信任值——值只作为绑定参数与填充值。
    let mut provenance_by_row: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut row_meta: BTreeMap<(String, String), RowMeta> = BTreeMap::new();
    for (name, rows_json) in tables {
        let table = allowed_table(name).ok_or(AbsorbError::BadRequest)?;
        let rows = rows_json.as_array().ok_or(AbsorbError::BadRequest)?;
        let columns = allowed_columns(table);
        for row_json in rows {
            let row = row_json.as_object().ok_or(AbsorbError::BadRequest)?;
            if row.is_empty() {
                return Err(AbsorbError::BadRequest);
            }
            let mut values = Vec::with_capacity(row.len());
            for (column, raw) in row {
                let &column = columns
                    .iter()
                    .find(|c| *c == column)
                    .ok_or(AbsorbError::BadRequest)?;
                let value = sql_value(raw).ok_or(AbsorbError::BadRequest)?;
                values.push((column, value));
            }
            if table == "provenance" {
                let mut mem_table: Option<String> = None;
                let mut mem_id: Option<String> = None;
                let mut uid: Option<String> = None;
                for (column, value) in &values {
                    let Value::Text(text) = value else { continue };
                    match *column {
                        // 水位标记只认 uid：缺 mem_table/mem_id 的 provenance 行也照样标。
                        "turn_uid" => {
                            turns.insert(text.clone());
                            uid = Some(text.clone());
                        }
                        "mem_table" => mem_table = Some(text.clone()),
                        "mem_id" => mem_id = Some(text.clone()),
                        _ => {}
                    }
                }
                if let (Some(t), Some(id), Some(uid)) = (mem_table, mem_id, uid) {
                    provenance_by_row.entry((t, id)).or_default().insert(uid);
                }
            } else if table == "fact" {
                // S3c-4 的分派点：fact 行只留原列（闸/载荷/apply 都读 JSON 形状），
                // 不进直写集合、也不进归属目标——归属由 `insert_fact_row` 在 apply 写。
                fact_rows.push(row.clone());
            } else if let Some(Value::Text(id)) = values.iter().find(|(c, _)| *c == "id").map(|(_, v)| v)
            {
                // 表名就是 `mem_table`、`id` 就是 `mem_id`：不手抄第二份"哪些表是记忆行"。
                row_meta
                    .entry((table.to_string(), id.clone()))
                    .or_insert_with(|| row_meta_of(&values));
            }
            rows_in += 1;
            if rows_in > MAX_ROWS_PER_BATCH {
                return Err(AbsorbError::BadRequest);
            }
            if table != "fact" {
                parsed.entry(table).or_default().push(values);
            }
        }
    }

    // 归属推导的第一跳：transcript（**事务之外**，与 memory.db 是两次独立查询、不 JOIN）。
    let sessions = session_owners(root.transcript(), &turns)?;
    // 整批唯一的 session owner：0 个或 ≥2 个都不成立（后者不许挑一个算数）。
    let mut distinct: HashSet<SessionOwner> = sessions.values().cloned().collect();
    let payload_owner = if distinct.len() == 1 {
        distinct.drain().next()
    } else {
        None
    };

    // 第一段：记忆行，一个事务。非冲突错误（NOT NULL / CHECK / FK）⇒ 整批回滚 + BadRows。
    let tx = root
        .memory()
        .conn()
        .unchecked_transaction()
        .map_err(|_| AbsorbError::Store)?;
    let mut absorbed = 0usize;
    for table in INSERT_ORDER {
        let Some(rows) = parsed.remove(table) else { continue };
        for row in rows {
            let names: Vec<&str> = row.iter().map(|(column, _)| *column).collect();
            let slots: Vec<String> = (1..=row.len()).map(|i| format!("?{i}")).collect();
            let sql = format!(
                "INSERT INTO {table}({}) VALUES ({}) ON CONFLICT DO NOTHING",
                names.join(","),
                slots.join(",")
            );
            let values: Vec<Value> = row.into_iter().map(|(_, value)| value).collect();
            match tx.execute(&sql, rusqlite::params_from_iter(values.iter())) {
                Ok(n) => absorbed += n,
                // 冲突已被 ON CONFLICT DO NOTHING 吞掉，这里剩下的都是约束错误。
                Err(_) => return Err(AbsorbError::BadRows),
            }
        }
    }

    // 同一事务内补归属：provenance 指到的行 + 载荷里出现的每一行。
    // 注意 **absorbed=0 也照样写**：旧行可能缺 owner（C1 之前的吸收、旧备份恢复）。
    // `provenance.mem_table` 是载荷里的自由文本，只有落在 canonical 表名上的才写归属行：
    // 归属表是权限面，键空间不许被节点用任意字符串撑开。
    let mut targets: BTreeSet<(String, String)> = provenance_by_row
        .keys()
        // S3c-4：fact 的归属由 apply 的 `insert_fact_row` 写（它只在真插入时写），
        // 直写阶段替它写会让"闸拒/待审"的行提前拿到归属。
        .filter(|(t, _)| t != "fact" && allowed_table(t).is_some())
        .cloned()
        .collect();
    targets.extend(row_meta.keys().cloned());
    let mut conflicts = 0usize;
    for key in targets {
        let (mem_table, mem_id) = (key.0.as_str(), key.1.as_str());
        let meta = row_meta.get(&key).cloned().unwrap_or_default();
        let owners: HashSet<&SessionOwner> = provenance_by_row
            .get(&key)
            .map(|uids| uids.iter().filter_map(|uid| sessions.get(uid)).collect())
            .unwrap_or_default();
        let row = if owners.len() == 1 {
            // 恰好一个 session owner ⇒ 用它。
            derived_row(mem_table, mem_id, owners.into_iter().next().unwrap(), &meta, now_ms)
        } else if owners.is_empty() {
            // 没有出处 ⇒ 退到整批唯一 owner；整批也没有就 unresolved。
            match payload_owner.as_ref() {
                Some(session) => derived_row(mem_table, mem_id, session, &meta, now_ms),
                None => unresolved_row(mem_table, mem_id, &meta, now_ms),
            }
        } else {
            // 跨 session（≥2 个不同 session owner）⇒ unresolved，不挑、不静默合并。
            unresolved_row(mem_table, mem_id, &meta, now_ms)
        };

        let fresh = owner::insert(&tx, &row).map_err(|_| AbsorbError::Store)?;
        if fresh {
            continue;
        }
        // 已有归属行：同 owner ⇒ 幂等重放；不同 owner ⇒ **冲突**，逐行抑制并记账。
        let existing = owner::existing_owner(&tx, mem_table, mem_id)
            .map_err(|_| AbsorbError::Store)?;
        let differs = match &existing {
            Some((principal, agent)) => {
                principal.as_deref() != row.owner_principal_id.as_deref()
                    || agent != &row.owner_agent_id
            }
            None => false,
        };
        if differs {
            tx.execute(
                "INSERT INTO memory_op(op, mem_table, mem_id, before_json, after_json, reason,
                                      actor, created_at_ms)
                 VALUES('NOOP', ?1, ?2, NULL, NULL, 'owner_conflict', 'program', ?3)",
                rusqlite::params![mem_table, mem_id, now_ms],
            )
            .map_err(|_| AbsorbError::Store)?;
            conflicts += 1;
        }
    }
    tx.commit().map_err(|_| AbsorbError::Store)?;

    // 冲突计数一次写 ledger（只在非零时写，不留 count=0 的空行）。
    // 失败 ⇒ 503：记忆行已提交，重试幂等（冲突那一格会再数一次，这是"可数不静默"的代价）。
    if conflicts > 0 {
        rc_ledger::counters::bump(
            root.ledger_mut(),
            "maintenance",
            "ownership",
            "ownership_conflicts",
            conflicts as i64,
            now_ms,
        )
        .map_err(|_| AbsorbError::Store)?;
    }

    // ── S3c-4：fact 行（结构行已提交；从这里开始任何失败都 503，水位一个都不打）──
    let rows_direct = absorbed;
    let rows_reviewed = fact_rows.len();
    let mut rows_absorbed = 0usize;
    let mut rows_rejected = 0usize;
    let mut rows_quarantined = 0usize;
    let mut rows_duplicate = 0usize;
    let mut fact_inserted = 0usize;
    if rows_reviewed > 0 {
        // 后端缺失的判据在**任何 fact 入队/写库之前**：结构行已经落了（幂等重放），
        // 这一支只把"本批审不了"这件事如实说出来，节点带同一载荷重试即可。
        let Some(backend) = backend else {
            return Err(AbsorbError::ReviewUnavailable);
        };
        // 本轮 fact 行引用到的转录读数（闸与批信封都要它；查不到由闸判 `turn_missing`）。
        let mut fact_turns: BTreeSet<String> = BTreeSet::new();
        for row in &fact_rows {
            let id = row.get("id").and_then(Json::as_str).unwrap_or_default();
            if let Some(uids) = provenance_by_row.get(&("fact".to_string(), id.to_string())) {
                fact_turns.extend(uids.iter().cloned());
            }
        }
        let turns_of_rows = turn_facts(root.transcript(), &fact_turns)?;

        // ① 逐行构图 → 冲突候选 → 闸。闸拒的行在这里就入队并记 `program/reject(<code>)`。
        let mut passing: Vec<FactSeed> = Vec::new();
        for row in &fact_rows {
            let mut seed = fact_seed(
                row,
                &provenance_by_row,
                &sessions,
                payload_owner.as_ref(),
                &turns_of_rows,
            );
            seed.conflict = conflict_candidate(root.memory(), &seed.values, &seed.scope)?;
            let candidate = RowsCandidate {
                owner: seed.owner.clone(),
                scope: seed.scope.clone(),
                session_key: seed.session_key.clone(),
                device_id: seed.device_id.clone(),
                turn_uids: seed.turn_uids.clone(),
                values: seed.values.clone(),
            };
            let gate = gate_rows(root.memory(), root.transcript(), &candidate, now_ms)
                .map_err(|_| AbsorbError::Store)?;
            if gate.passed {
                passing.push(seed);
                continue;
            }
            // 闸拒：一行一个 item，一行一条 `program/reject(<gate code>)`。不写库。
            let (batch, ctx) = batch_of(std::slice::from_ref(&seed), &turns_of_rows);
            let item = row_item(&seed, &batch, &ctx);
            let queued = rc_memory::review::enqueue(root.memory(), &item, now_ms)
                .map_err(|_| AbsorbError::Store)?;
            // A2（W32 止血）：入队成功且是新行就补写该 pass 的审查轴水位。`hub pending` 的
            // review 轴只读水位缓存不读实表，enqueue 不写水位 ⇒ 入队一条之后 `review.pending`
            // 仍是旧值，读数失真（review_item 211 实证）。best-effort：镜像 `review.rs` 人工
            // 判定那一支的 `let _ =` 同律——水位是读数，写不进去不改变这次入队的结果，
            // 之后的窄审收尾会再写一遍。
            // 写点在程序闸判定**之后**（评审 11-07 指出）：闸拒支下面会把这条新行 Reject 掉，
            // 若先写水位，全闸拒批会把 `rows` 的 pending_review 虚挂到下一次入队/收尾，
            // 正是 review_watermark_mismatch 反复误报的源头。既有先例（review.rs:313、
            // run.rs:568）全是判定后写。
            if queued.state == ReviewState::PendingReview
                && !program_decided(root.memory(), queued.id)
                    .map_err(|_| AbsorbError::Store)?
            {
                let code = gate.code.unwrap_or("payload_shape");
                record_program_once(
                    root.memory(),
                    queued.id,
                    ReviewVerdict::Reject,
                    code,
                    &queued.payload_sha256,
                    now_ms,
                )?;
            }
            if queued.fresh {
                let _ = write_watermark(root.memory(), root.ledger(), &item.pass_id, now_ms);
            }
            rows_rejected += 1;
        }

        // ② 闸过的行按"同一份信封"分组、组内 ≤10 行/批。同一批共用一份 owner/scope，
        //    所以分组键里必须有它们（见 [`GroupKey`]）。
        let mut groups: BTreeMap<GroupKey, Vec<FactSeed>> = BTreeMap::new();
        for seed in passing {
            groups.entry(seed.group_key()).or_default().push(seed);
        }
        // 已入队的待审项：(item id, 该 item 的行数)。同步审在**全部入队之后**按序跑。
        let mut queued_items: Vec<(i64, usize)> = Vec::new();
        for seeds in groups.values() {
            for chunk in seeds.chunks(rc_distill::review::REVIEW_MAX_ITEMS) {
                let (batch, ctx) = batch_of(chunk, &turns_of_rows);
                let owner = &chunk[0].owner;
                let scope = &chunk[0].scope;
                let rows_input: Vec<RowsRowInput> = chunk
                    .iter()
                    .map(|seed| RowsRowInput {
                        values: seed.values.clone(),
                        evidence_turn_uid: seed.evidence_turn_uid.clone(),
                        conflict: seed.conflict.clone(),
                    })
                    .collect();
                let payload = rows_payload(ROWS_UPLOAD_RUN_ID, &batch, &ctx, owner, scope, &rows_input);
                let item = rows_new_item(&batch, owner_json(owner, scope), &payload);
                let queued = rc_memory::review::enqueue(root.memory(), &item, now_ms)
                    .map_err(|_| AbsorbError::Store)?;
                // A2：与上面闸拒那一支同一格写点（fresh 才写，`let _ =` 同律）。同样在程序
                // 判定之后写——Pass 会把新行挪出 pending_review，先写就留一个失配窗口。
                if queued.state == ReviewState::PendingReview
                    && !program_decided(root.memory(), queued.id)
                        .map_err(|_| AbsorbError::Store)?
                {
                    record_program_once(
                        root.memory(),
                        queued.id,
                        ReviewVerdict::Pass,
                        PROGRAM_PASS,
                        &queued.payload_sha256,
                        now_ms,
                    )?;
                }
                if queued.fresh {
                    let _ = write_watermark(root.memory(), root.ledger(), &item.pass_id, now_ms);
                }
                queued_items.push((queued.id, chunk.len()));
            }
        }

        // ③ 同步窄审：逐项、顺序。**只有"这一批有结论"才算成功**（S3c-fix，复审 P1-2/P1-d）：
        //    - `Stale`（本请求的 item 被顶替）/ `Conflict`（另一写者推进）/ 引擎没推进
        //      （`PendingReview`/`Approved`）⇒ 这些行**没有结论**：503，且水位一个都不打
        //      （200 会让节点以为成功、永不重发；S3c 复审 P0 就是这样静默丢行的）；
        //    - 其余 `Err` 一律 503（不继续）。
        for (item_id, rows) in queued_items {
            let outcome = match run_item(
                root.memory(),
                root.transcript(),
                root.ledger(),
                item_id,
                backend,
                now_ms,
            ) {
                Ok(outcome) => outcome,
                // 另一写者把这条 item 推进了：本请求的行不在它手上，没有结论。
                Err(RunError::Conflict) => return Err(AbsorbError::Store),
                Err(_) => return Err(AbsorbError::Store),
            };
            match outcome.item_state {
                ReviewState::Applied => {
                    if outcome.applied_now {
                        // 真写了库：插入数进 `rows_absorbed`，撞 id 的进 `rows_duplicate`，
                        // 被窄审拒/隔离的行按逐条判决进各自那格（复审 P1-d 的"混合 item 蒸发"）。
                        rows_absorbed += outcome.facts;
                        rows_duplicate += outcome.duplicate;
                        if let Some(verdicts) = outcome.verdicts.as_deref() {
                            rows_rejected += verdicts
                                .iter()
                                .filter(|(_, verdict)| *verdict == ReviewVerdict::Reject)
                                .count();
                            rows_quarantined += verdicts
                                .iter()
                                .filter(|(_, verdict)| *verdict == ReviewVerdict::Uncertain)
                                .count();
                        }
                        fact_inserted += outcome.facts;
                    } else {
                        // 已 applied 的重试：只计行数、不重写、不加 `rows_duplicate`。
                        rows_absorbed += rows;
                    }
                }
                ReviewState::Rejected => rows_rejected += rows,
                ReviewState::Quarantined => rows_quarantined += rows,
                // 顶替：本请求的 item 已被别人换掉，行没有结论（不许 200 + 打水）。
                ReviewState::Stale => return Err(AbsorbError::Store),
                // 窄审没推进状态（引擎没有可写的终态）：不猜，503 让节点重试。
                ReviewState::PendingReview | ReviewState::Approved => {
                    return Err(AbsorbError::Store)
                }
            }
        }
        // 自洽：每一行恰好落在一格（写进 / 撞 id / 被拒 / 被隔离）。只在 debug 构建里断，
        // 生产不因读数口径不同而拒绝一个已经把行写完的请求。
        debug_assert_eq!(
            rows_absorbed + rows_rejected + rows_quarantined + rows_duplicate,
            rows_reviewed,
            "absorbed({rows_absorbed}) + rejected({rows_rejected}) + quarantined({rows_quarantined}) \
             + duplicate({rows_duplicate}) 必须等于 reviewed({rows_reviewed})"
        );
    }

    // 第二段：水位标记。失败 ⇒ 503（节点不 pull，会带同一载荷重试；第一段吸收是幂等的）。
    // S3c-4：只有本次请求没有返回 503 才走到这里——fact 行的失败都在上面早退。
    let turns_marked = turns.len();
    for uid in &turns {
        rc_ledger::water::mark_pre_distilled(root.ledger(), uid, now_ms)
            .map_err(|_| AbsorbError::Store)?;
    }

    let absorbed = absorbed + fact_inserted;
    Ok(Absorbed {
        rows_in,
        absorbed,
        skipped_duplicate: rows_in.saturating_sub(absorbed),
        turns_marked,
        rows_reviewed,
        rows_absorbed,
        rows_rejected,
        rows_quarantined,
        rows_duplicate,
        rows_direct,
    })
}
