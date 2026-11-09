//! S3c-1：`/v1/rows` 的 **fact 行**程序闸（设计 `2026-10-26-review-queue-s3c.md` §4、§6）。
//!
//! 拍板已定：**B1 只审 `fact` 行**、**C1 用 `text` 对证据轮**。所以闸门的输入是一行 fact 的
//! 列值（hub 已按 `allowed_columns` 校验过的形状），不是载荷信封；其余 23 张表不进这道闸。
//!
//! 与 [`crate::review::gate`] 同一套纪律：
//! - **纯判定**：不写库、不调模型、不读钟；`now_ms` 是形参位（本轮没有时间窗判定）。
//! - 判定顺序固定，返回**第一个**失败码；同一输入两次结果一致。
//! - 形状错不是 `Err`：`payload_shape` 是判定码。只有 SQL/句柄错误才 `Err`。
//! - 码只用静态字符串；返回值里不带载荷正文、锚 id、`turn_uid` 原文（守卫 12）。
//!
//! ## C1 的口径（这一格最容易做错）
//!
//! 数字接地**只看 `evidence_turn_uid` 那一轮**的正文，不看 `turn_uids` 里的其它
//! provenance 轮。理由不是省一次查询：`fact` 表只有一个证据列，窄审（`row-support-review`）
//! 的输入也按设计 §4 只渲染行 `text` + 证据轮；闸门若拿全部 provenance 轮做子串匹配，
//! 一条把数字从邻居轮"借"过来、证据轮里根本没有的行就能过闸，而那正是 `invented_number`
//! 要拦的东西。闸门与窄审必须看同一轮，否则"闸门过了、审不了"的窗口里会混进无据行。
//!
//! ## 形状的两条取舍
//!
//! - `value_json` 落 TEXT 列、内容是 JSON 编码，本闸门不消费它；形状上只拒**数组/对象**
//!   （与 hub 的 `sql_value` 同一条线：它不是标量，进不了列），字符串/数字/布尔/null 都收。
//! - `participants_json` 必须（可缺省/可 null）是**能解析成字符串数组的 JSON 串**：第 7 步
//!   要逐个查锚，读不出来就没法查——形状错比放行诚实。
//!
//! ## S3c-fix：放行即 503 的列全部进闸
//!
//! 复审的 P1-a 是一类**确定性 503**：闸放行、写库必拒的列（DDL 的 `CHECK`/`NOT NULL`/FK
//! 兜底），节点拿着 503 重试同一条永远失败，且水位不打 ⇒ 没有隔离可见性。所以：
//!
//! - `mode` 只收 `stated`（与 fact 写库层 `FACT_MODE_STATED` 同一口径；`inferred`/`reported`
//!   是 fact 列的 DDL 闭集，但不是 `/v1/rows` 的写库口径）；
//! - `status` 可空，非空必须是 `pending|done|cancelled|expired`（DDL 闭集）；
//! - `confidence` 可空，非空必须是 JSON number（`REAL` 列）；
//! - `norm_version` **必填** JSON integer（`INTEGER NOT NULL`）；
//! - 六个毫秒列（`occurred_at_ms`/`occurred_end_ms`/`speech_time_ms`/`valid_from_ms`/
//!   `valid_to_ms`/`due_at_ms`）可空，非空必须是 integer（`INTEGER` 列）；
//! - `speaker_anchor_id` 可空；非空（含空串——那是一个不存在的 id）必须锚存在且未退场
//!   （FK + 与其余锚同一条两遍扫）。
//!
//! ## S3c-fix2：`NOT NULL` 列一个不落
//!
//! 上一轮把 `scope_kind`/`scope_key`/`source_agent`/`device_id`/`created_at_ms` 留给了
//! hub 的列白名单——但白名单只管"列名在不在"，不管"值是不是 TEXT/INTEGER"。节点少给一格
//! 或给个数字，闸放行、写库 `NOT NULL` 必拒，又是同一条 503 死循环。所以这里按 DDL 逐列钉：
//!
//! - `id`：必填、非空 TEXT（写库主键；与 replay 的 `rows_drafts` 同口径）；
//! - `scope_kind`/`scope_key`/`source_agent`/`device_id`：必填、非空 TEXT；
//! - `created_at_ms`：必填 JSON integer；
//! - `world_id`：可空 TEXT（DDL 可空列），非 null 必须是字符串。
//!
//! 一律用 `payload_shape`：它们都是"载荷形状"而不是某一列的语义闭集。

use rc_memory::owner::OwnerCtx;
use rc_memory::Scope;
use rc_store::{MemoryHandle, TranscriptHandle};
use serde_json::{Map, Value};

use crate::pass_fact_contract::digit_runs;
use crate::pass_prompt::is_compaction_body;
use crate::prepare::prepare;
use crate::DistillError;

// ── 码表：`&'static str` 的封闭表，顺序 = 判定顺序 ────────────────────────────

pub const PAYLOAD_SHAPE: &str = "payload_shape";
pub const TEXT_MISSING: &str = "text_missing";
pub const TURN_MISSING: &str = "turn_missing";
pub const TURN_SESSION_MISMATCH: &str = "turn_session_mismatch";
pub const TURN_TOMBSTONED: &str = "turn_tombstoned";
pub const COMPACTION_EVIDENCE: &str = "compaction_evidence";
pub const REF_MISSING: &str = "ref_missing";
pub const REF_RETIRED: &str = "ref_retired";
/// 引用锚存在、未退场，但对**这一行的 owner** 不可见（跨 owner 的私有锚）。
///
/// 加这一格的根因是一致性缺陷：`/v1/rows` 的 fact 行可以把另一台设备的私有锚写进
/// `subject_anchor_id`，闸门只查"锚在不在"，于是行 owner=qoder、主语锚 owner=别人也能入库。
/// 判定复用 [`rc_memory::owner::anchor_visible`]（与 fact/event 候选池同一份谓词）。
pub const ANCHOR_NOT_VISIBLE: &str = "anchor_not_visible";
pub const CATEGORY: &str = "category";
pub const PREDICATE: &str = "predicate";
pub const MODE: &str = "mode";
pub const STATUS: &str = "status";
pub const NUMBER_NOT_GROUNDED: &str = "number_not_grounded";
pub const OWNER_UNRESOLVED: &str = "owner_unresolved";

/// 全部失败码，**按判定顺序**。测试对着这张表逐码覆盖；新增/改名只改这一处与上面的常量。
pub const ROWS_GATE_CODES: [&str; 15] = [
    PAYLOAD_SHAPE,
    TEXT_MISSING,
    TURN_MISSING,
    TURN_SESSION_MISMATCH,
    TURN_TOMBSTONED,
    COMPACTION_EVIDENCE,
    REF_MISSING,
    REF_RETIRED,
    ANCHOR_NOT_VISIBLE,
    CATEGORY,
    PREDICATE,
    MODE,
    STATUS,
    NUMBER_NOT_GROUNDED,
    OWNER_UNRESOLVED,
];

/// `/v1/rows` 的 `fact.mode` **写库口径**：只有 `stated`。
///
/// 与 fact 写库层（`rc_memory::FACT_MODE_STATED`）同一个值：`fact` 表的 DDL 闭集里还有
/// `inferred`/`reported`，但中心抽取从不产这两个词，`/v1/rows` 也不收——闸放行而写库
/// 另有口径的列就是一条确定性 503（S3c 复审 P1-a）。
const ROW_MODES: [&str; 1] = ["stated"];

/// `fact.status` 的 DDL 闭集（`rc-store/src/ddl.rs` 的 CHECK 逐字对拍）；`NULL` 合法。
const FACT_STATUSES: [&str; 4] = ["pending", "done", "cancelled", "expired"];

/// 六个可空的毫秒列（`INTEGER`）：非 null 必须是 JSON integer。
const MS_COLUMNS: [&str; 6] = [
    "occurred_at_ms",
    "occurred_end_ms",
    "speech_time_ms",
    "valid_from_ms",
    "valid_to_ms",
    "due_at_ms",
];

/// `fact` 的必填 TEXT 列（`TEXT NOT NULL`）：键在、是字符串、长度 > 0。
///
/// `id` 是写库主键；其余四格是作用域与来源。空串在 `NOT NULL` 下能过、语义上是"没给"，
/// 所以这里按非空收——与 replay 的 `rows_drafts`（`id` 非空）同一个口径。
const REQUIRED_TEXT_COLUMNS: [&str; 5] = [
    "id",
    "scope_kind",
    "scope_key",
    "source_agent",
    "device_id",
];

/// `anchor.status` 里"未退场"的两值；其余（`merged`/`retired`）即退场。
const ANCHOR_LIVE: [&str; 2] = ["active", "provisional"];

// ── 输入/输出 ────────────────────────────────────────────────────────────────

/// 一行待审的 fact 候选（`/v1/rows` 载荷里 `table=fact` 的 `items[i]`）。
///
/// `turn_uids` 是该行 provenance 覆盖的全部轮（含证据轮），顺序任意、由调用方去重；
/// `values` 是 fact 行的列值，键集合取自 hub 的 `allowed_columns("fact")`。
pub struct RowsCandidate {
    pub owner: OwnerCtx,
    pub scope: Scope,
    pub session_key: String,
    pub device_id: String,
    pub turn_uids: Vec<String>,
    pub values: Map<String, Value>,
}

/// 一次判定的读数。`code` 只在失败时有值，且是 [`ROWS_GATE_CODES`] 里的一个。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowsGateOutcome {
    pub passed: bool,
    /// 第一个失败码；pass 时 None。
    pub code: Option<&'static str>,
}

fn fail(code: &'static str) -> RowsGateOutcome {
    RowsGateOutcome {
        passed: false,
        code: Some(code),
    }
}

fn pass() -> RowsGateOutcome {
    RowsGateOutcome {
        passed: true,
        code: None,
    }
}

// ── 形状 ──────────────────────────────────────────────────────────────────────

/// 形状过关后的 fact 列（后面的判定只读这几格；`value_json` 等不被消费的列到此为止）。
#[derive(Debug)]
struct FactRow {
    subject_anchor_id: String,
    object_anchor_id: Option<String>,
    place_anchor_id: Option<String>,
    /// `speaker_anchor_id`：可空/空串；非空也要过第 7/8 步的锚存在/退场。
    speaker_anchor_id: Option<String>,
    predicate: String,
    category: String,
    mode: String,
    /// `status`：`NULL` 合法；非 null 在第 9 步过闭集。
    status: Option<String>,
    text: String,
    evidence_turn_uid: String,
    participants: Vec<String>,
}

/// 必填列：键在、是字符串。闭集列的**取值**不在这里判（`category`/`mode` 的第 9 步才是家）。
fn req_str(map: &Map<String, Value>, key: &str) -> Option<String> {
    Some(map.get(key)?.as_str()?.to_string())
}

/// 必填列的**原样非空**串：键在、是字符串、长度 > 0。
///
/// 这里不做 trim——空白串的语义各自有码（`text` → `text_missing`、`predicate` →
/// `predicate`）。把 trim 提前到形状层会让这两个码永远不可达。
fn req_nonempty(map: &Map<String, Value>, key: &str) -> Option<String> {
    let text = map.get(key)?.as_str()?;
    (!text.is_empty()).then(|| text.to_string())
}

/// 可空列：缺省与 `null` 都读成 `None`；字符串读成 `Some`；其余类型是形状错。
fn opt_str(map: &Map<String, Value>, key: &str) -> Option<Option<String>> {
    match map.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(s)) => Some(Some(s.clone())),
        _ => None,
    }
}

/// `value_json`：落 TEXT 列的 JSON 编码，本闸门不消费内容。只拒数组/对象（非标量）。
fn check_value_json(map: &Map<String, Value>) -> Option<()> {
    match map.get("value_json") {
        None | Some(Value::Null | Value::String(_) | Value::Number(_) | Value::Bool(_)) => Some(()),
        Some(Value::Array(_) | Value::Object(_)) => None,
    }
}

/// 必填的 JSON integer（`norm_version` 落 `INTEGER NOT NULL`）。
fn req_int(map: &Map<String, Value>, key: &str) -> Option<i64> {
    map.get(key)?.as_i64()
}

/// 可空的 JSON integer（六个毫秒列）：缺省/null ⇒ 空；整数 ⇒ 有值；其余（含浮点）形状错。
fn opt_int(map: &Map<String, Value>, key: &str) -> Option<Option<i64>> {
    match map.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(Value::Number(number)) => number.as_i64().map(Some),
        _ => None,
    }
}

/// `confidence`（`REAL` 列）：缺省/null ⇒ 空；JSON number ⇒ 有值；字符串/布尔等形状错。
fn check_confidence(map: &Map<String, Value>) -> Option<()> {
    match map.get("confidence") {
        None | Some(Value::Null | Value::Number(_)) => Some(()),
        _ => None,
    }
}

/// 六个毫秒列逐个查形状（可空 integer）。
fn check_ms_columns(map: &Map<String, Value>) -> Option<()> {
    for column in MS_COLUMNS {
        opt_int(map, column)?;
    }
    Some(())
}

/// `participants_json`：缺省/null ⇒ 空；字符串 ⇒ 必须解析成字符串数组。
fn participants_of(map: &Map<String, Value>) -> Option<Vec<String>> {
    match map.get("participants_json") {
        None | Some(Value::Null) => Some(Vec::new()),
        Some(Value::String(raw)) => {
            let parsed: Value = serde_json::from_str(raw).ok()?;
            parsed
                .as_array()?
                .iter()
                .map(|entry| Some(entry.as_str()?.to_string()))
                .collect()
        }
        _ => None,
    }
}

fn parse_row(values: &Map<String, Value>) -> Option<FactRow> {
    check_value_json(values)?;
    check_confidence(values)?;
    check_ms_columns(values)?;
    let subject_anchor_id = req_nonempty(values, "subject_anchor_id")?;
    let predicate = req_nonempty(values, "predicate")?;
    // `category`/`mode`/`status` 只要求"是字符串"或 null：空串与闭集外的值一样走第 9 步的闭集码。
    let category = req_str(values, "category")?;
    let mode = req_str(values, "mode")?;
    let status = opt_str(values, "status")?;
    let text = req_nonempty(values, "text")?;
    let evidence_turn_uid = req_nonempty(values, "evidence_turn_uid")?;
    let object_anchor_id = opt_str(values, "object_anchor_id")?;
    let place_anchor_id = opt_str(values, "place_anchor_id")?;
    // `speaker_anchor_id` 与 object/place 同一形状与同一条锚判定：`null`/缺省 = 没给；
    // **空串不是"没给"**——它会被写库的 FK 拒（没有 id 为空的锚），所以它走第 7 步的
    // `ref_missing`，而不是让闸放行、写库 503。
    let speaker_anchor_id = opt_str(values, "speaker_anchor_id")?;
    // `value_text` 只校验形状（可空串或 null），本闸门不消费它。
    opt_str(values, "value_text")?;
    // `norm_version` 是 `INTEGER NOT NULL`：必填整数（缺/浮点/字符串都走 `payload_shape`）。
    req_int(values, "norm_version")?;
    // S3c-fix2：`TEXT NOT NULL` 的五格（主键 + 作用域 + 来源）：缺键/非字符串/空串都是
    // "闸过、写库必拒"——一律 `payload_shape`。
    for column in REQUIRED_TEXT_COLUMNS {
        req_nonempty(values, column)?;
    }
    // `created_at_ms` 是 `INTEGER NOT NULL`：与 `norm_version` 同一条判定。
    req_int(values, "created_at_ms")?;
    // `world_id` 是 DDL 里的可空 TEXT：缺省/null = 没给；非 null 必须是字符串。
    opt_str(values, "world_id")?;
    Some(FactRow {
        subject_anchor_id,
        object_anchor_id,
        place_anchor_id,
        speaker_anchor_id,
        predicate,
        category,
        mode,
        status,
        text,
        evidence_turn_uid,
        participants: participants_of(values)?,
    })
}

// ── 读数 ──────────────────────────────────────────────────────────────────────

fn turn_body(t: &TranscriptHandle, turn_uid: &str) -> Result<Option<String>, DistillError> {
    let mut st = t.conn().prepare("SELECT body FROM turns WHERE turn_uid=?1")?;
    let mut rows = st.query([turn_uid])?;
    match rows.next()? {
        None => Ok(None),
        Some(row) => Ok(Some(row.get(0)?)),
    }
}

/// 锚的 `status`；`None` = `anchor` 表里没有这一行。
fn anchor_status(m: &MemoryHandle, anchor_id: &str) -> Result<Option<String>, DistillError> {
    let mut st = m.conn().prepare("SELECT status FROM anchor WHERE id=?1")?;
    let mut rows = st.query([anchor_id])?;
    match rows.next()? {
        None => Ok(None),
        Some(row) => Ok(Some(row.get(0)?)),
    }
}

/// 一行 fact 引用到的全部锚：subject / object / place / speaker / participants 逐个。
fn anchors_of(row: &FactRow) -> Vec<&str> {
    let mut out = Vec::with_capacity(4 + row.participants.len());
    out.push(row.subject_anchor_id.as_str());
    out.extend(row.object_anchor_id.as_deref());
    out.extend(row.place_anchor_id.as_deref());
    out.extend(row.speaker_anchor_id.as_deref());
    out.extend(row.participants.iter().map(String::as_str));
    out
}

// ── 判定 ──────────────────────────────────────────────────────────────────────

/// 一行 fact 行的程序闸。见模块文档的判定顺序；返回第一个失败码。
pub fn gate_rows(
    m: &MemoryHandle,
    t: &TranscriptHandle,
    cand: &RowsCandidate,
    now_ms: i64,
) -> Result<RowsGateOutcome, DistillError> {
    // 形参位：本轮没有时间窗判定（与 `gate` 同形，接线那一格由调用方传自己的钟）。
    let _ = now_ms;

    // 1. 载荷形状：列形状 + "证据轮在轮清单里"。
    let Some(row) = parse_row(&cand.values) else {
        return Ok(fail(PAYLOAD_SHAPE));
    };
    if cand.turn_uids.is_empty()
        || !cand
            .turn_uids
            .iter()
            .any(|uid| uid == &row.evidence_turn_uid)
    {
        return Ok(fail(PAYLOAD_SHAPE));
    }

    // 2. 正文去空白后为空。
    if row.text.trim().is_empty() {
        return Ok(fail(TEXT_MISSING));
    }

    // 3. 每一轮都在转录里（先全查存在，再比会话/设备）。
    for uid in &cand.turn_uids {
        if rc_ledger::transcript::turn_origin(t, uid)?.is_none() {
            return Ok(fail(TURN_MISSING));
        }
    }

    // 4. 会话/设备与候选逐字相等。
    for uid in &cand.turn_uids {
        match rc_ledger::transcript::turn_origin(t, uid)? {
            Some(origin)
                if origin.session_key == cand.session_key
                    && origin.device_id == cand.device_id => {}
            Some(_) => return Ok(fail(TURN_SESSION_MISMATCH)),
            // 第 3 步已查过；并发删除也按"查无此轮"收，不猜。
            None => return Ok(fail(TURN_MISSING)),
        }
    }

    // 5. 撤销：会话/设备命中墓碑。
    if rc_ledger::transcript::tombstone_blocks(t, &cand.session_key, &cand.device_id, None)? {
        return Ok(fail(TURN_TOMBSTONED));
    }

    // 证据轮正文：第 3 步已保证存在；这里缺行只能是并发删除，仍按 `turn_missing` 收。
    let Some(body) = turn_body(t, &row.evidence_turn_uid)? else {
        return Ok(fail(TURN_MISSING));
    };

    // 6. 证据轮是压缩摘要块（摘要不是"某人说过的话"，不能当证据）。
    if is_compaction_body(&body) {
        return Ok(fail(COMPACTION_EVIDENCE));
    }

    // 7/8. 引用锚：先**全查存在**，再**全查退场**。
    //
    // 两遍而不是一遍里"查一个判一个"：否则"subject 已退场 + object 不存在"会先撞上
    // 第 8 步的码，报出与 §3 顺序相反的答案。码序是契约，不能取决于哪个锚排在前面。
    let refs = anchors_of(&row);
    let mut statuses = Vec::with_capacity(refs.len());
    for anchor in &refs {
        statuses.push(anchor_status(m, anchor)?);
    }
    if statuses.iter().any(Option::is_none) {
        return Ok(fail(REF_MISSING));
    }
    if statuses
        .iter()
        .flatten()
        .any(|status| !ANCHOR_LIVE.contains(&status.as_str()))
    {
        // DDL 闭集只有 active|provisional|merged|retired；非前两者即退场。
        return Ok(fail(REF_RETIRED));
    }

    // 8b. 引用锚对**这一行的 owner** 可见（与 fact/event 候选池同一份谓词）。
    //
    // 放在"存在/退场"两遍之后：库外伪造的锚先按 `ref_missing`、退场的先按 `ref_retired` 报，
    // 可见性只回答"锚在、活着，但这一行不该看见它"这一格。每个锚逐个判，与第 7/8 步同序。
    //
    // owner 未解决（空 agent）的行在最后一步按 `owner_unresolved` 拒——这里跳过可见性，
    // 否则第 11 步的码永远不可达（空 owner 本来就没有"看得见谁"这回事，也不写库）。
    if !cand.owner.owner_agent_id.trim().is_empty() {
        for anchor in &refs {
            if !rc_memory::owner::anchor_visible(
                m,
                anchor,
                &cand.owner.owner_agent_id,
                &cand.scope,
            )? {
                return Ok(fail(ANCHOR_NOT_VISIBLE));
            }
        }
    }

    // 9. 闭集：category → predicate → mode → status。
    if !rc_memory::FACT_CATEGORIES.contains(&row.category.as_str()) {
        return Ok(fail(CATEGORY));
    }
    // `predicate` 没有闭集（与 fact 写库层同口径：只要求非空）。
    if row.predicate.trim().is_empty() {
        return Ok(fail(PREDICATE));
    }
    // S3c-fix：`mode` 只收写库口径的 `stated`（见 [`ROW_MODES`]）。
    if !ROW_MODES.contains(&row.mode.as_str()) {
        return Ok(fail(MODE));
    }
    // S3c-fix：`status` 可空；非空过 DDL 四值闭集。
    if let Some(status) = &row.status {
        if !FACT_STATUSES.contains(&status.as_str()) {
            return Ok(fail(STATUS));
        }
    }

    // 10. 数字接地：`text` 的每一段 ASCII 数字都必须是**证据轮正文**的子串（C1）。
    //
    // 正文读数与 `gate::check_quote` 同一把尺子（`prepare`：去围栏块与零宽、规范化），
    // 数字落在围栏块里不算接地。
    let evidence = prepare(&body);
    for run in digit_runs(&row.text) {
        if !evidence.contains(&run) {
            return Ok(fail(NUMBER_NOT_GROUNDED));
        }
    }

    // 11. 归属可解析：跨 session / 无出处的行由 hub 判成 unresolved，用空 agent 表示。
    if cand.owner.owner_agent_id.trim().is_empty() {
        return Ok(fail(OWNER_UNRESOLVED));
    }

    Ok(pass())
}
