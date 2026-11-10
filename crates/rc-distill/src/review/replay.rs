//! S3b-1：审核输入的**重放**——只凭 `review_item.payload_json` + 转录库只读查询，
//! 逐字节重建当时的审核输入与写库参数。
//!
//! 这是 S3b-2 的**外部审核入口的地基**：S3b-2 要把 fact/event 的第二次调用（窄审）与
//! 写库从 `pass_fact::run_fact` / `pass_event::run_event` 搬到 `hub review run`（另一个
//! 进程）。搬运的前提是"审核输入可以脱离 runner 的内存重建"——本模块只做这件事：
//!
//! - `batch`：按 `payload.batch.turn_uids` 的**顺序**从转录表回查每轮的
//!   `session_key/device_id/role/body/created_at_ms/ingested_at_ms`，拼回 [`Batch`]；
//! - `ctx` / `owner` / `scope`：`payload.batch` 的 `source_agent/device_id` 与
//!   `payload.owner` 六键直取；
//! - `drafts`：`payload.items`（event 还要同下标的 `extras`）逐键还原成
//!   [`FactDraft`](crate::pass_fact_contract::FactDraft) /
//!   [`EventDraft`](crate::pass_event_contract::EventDraft) /
//!   [`A1Batch`](rc_memory::writers_v4::A1Batch) /
//!   [`RowsDraft`]。`reference` 由**位置**决定
//!   （`f1..fN` / `e1..eN` / `a1..aN`），与 `fact_review_user` / `event_review_user` /
//!   `naming_review_user` 的编号同一套纪律。rows 只带回行值/证据轮/冲突值，
//!   列闭集留给 `gate_rows`。
//!
//! S3b-3b：naming 的载荷也是 `items` 四键（`anchors/mentions/issues/unresolved`），
//! 重放要把三条**引用**一次校验干净——任何一条不成立 ⇒ [`ReplayGap::PayloadShape`]
//! （这三类就是"重放必炸"的引用）：
//!
//! - 每个 mention 的 `anchor_ref` 与 `speaker_ref` 必须在 anchors 的 `a1..aN` 里；
//! - 每条锚证据与每条 mention 的 `turn_uid` 必须在 `payload.batch.turn_uids` 里；
//! - 锚的 `reference` 必须是 `a1..aN` 按位置、唯一（位置编号是 prompt 与裁决 ref 的同一套）。
//!
//! 写库层 `write_a1_with_owner` 靠它们查锚/查轮，缺一条到写库时才炸。
//!
//! 纪律：
//! - **纯读**：不写库、不读钟、不调模型；`now_ms` 不在这层的形参里。
//! - **gap 与 Store 分家**：载荷读不出/旧载荷缺新键/轮缺失/归属形状坏/本轮不支持的
//!   pass 组合都是**判定**（[`ReplayGap`]），只有 SQL/句柄错误才是 `Err`
//!   （[`ReplayFailure::Store`]）。与 `gate` 的"解析失败不是 Err"同一条纪律。
//! - **严格按载荷实际写出的形状解析**：`value`/`object_*`/`place_*` 可 `null`，
//!   `participants_canonical` 必须是数组；不猜默认值、不把闭集外的 `visibility`
//!   静默读成私有（[`owner::parse_visibility`] 的兜底在这里不适用，见 [`owner_of`]）。
//! - **H4a 重放一致性**：证据的 `anchor_role` 必须仍等于转录行现在的 `role`，
//!   `batch.source_ms` 必须仍等于重建批内 `created_at_ms` 的最大值；任一条不符 ⇒
//!   [`ReplayGap::TurnMismatch`]。载荷只能证明"当时写了什么"，不能替转录现状作证。
//! - 守卫 12：本模块的失败值只有静态枚举，不带载荷正文、路径或 uid 原文。

use rc_memory::owner::{self, OwnerCtx};
use rc_memory::review::Item;
use rc_memory::writers_v4::{A1Anchor, A1Batch, A1Issue, A1Mention, A1Unresolved};
use rc_memory::{AnchorRole, BatchCtx, MemError, Provenance, Scope, ScopeKind};
use rc_store::TranscriptHandle;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

use crate::batch::{Batch, TurnRow};
use crate::pass_event_contract::EventDraft;
use crate::pass_fact::family_of_slug;
use crate::pass_fact_contract::FactDraft;
use crate::review::rows_payload::ROWS_SCHEMA;
use crate::review::REVIEW_SCHEMA;

/// 重放**做不到**的原因（判定，不是故障）。每一格都是静态词，不带载荷内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayGap {
    /// schema 不是 `review.pass.v1` / 信封与 `item.meta` 不符 / items 形状不对 /
    /// 新键缺失（**含 S3b-1 之前的旧载荷**：没有 `turn_uid` 或 canonical 四格；也含
    /// S3b-3b 之前的 naming 旧载荷：mention 没有 `anchor_ref`、items 没有 `issues`/`unresolved`）。
    PayloadShape,
    /// `batch.turn_uids` 里某轮在转录库查不到。
    TurnMissing,
    /// 转录行存在，但与载荷声明的现状不符：`session_key`/`device_id` 不同；或证据的
    /// `anchor_role` 与转录行的 `role` 不同；或 `batch.source_ms` 不等于重建批内
    /// `created_at_ms` 的最大值（H4a：载荷不许带着"当时"的 role/源时刻重放）。
    TurnMismatch,
    /// `payload.owner` 六键读不出（含 `visibility`/`scope_kind` 的闭集外值）。
    OwnerShape,
    /// `pass_id`/`family` 组合本轮不重放（`time` 或未知组合）。
    Unsupported,
}

/// rows 的一行草稿：fact 行原列 + 证据轮 + 冲突值。
///
/// 重放只保证"能重建"——列闭集与语义由 `gate_rows` 判，本层不重复那张闭集。
#[derive(Debug, Clone, PartialEq)]
pub struct RowsDraft {
    pub values: Map<String, Value>,
    pub evidence_turn_uid: String,
    pub conflict: Option<Value>,
}

/// 重放出来的草稿。六个变体与 `pass_id` 一一对应（fact / event / naming / rows / state / lesson）。
#[derive(Debug, Clone)]
pub enum ReplayDrafts {
    Fact(Vec<FactDraft>),
    Event(Vec<EventDraft>),
    /// naming 的一批 A1 计划（锚/提及/问题/未决表面），已过引用校验。
    Naming(A1Batch),
    /// rows 的一批 fact 行（`review.rows.v1`），行值原样带回。
    Rows(Vec<RowsDraft>),
    /// P2：一批状态行（`StateDraft` 已由载荷重建，`mode` 落在两值闭集里）。
    State(Vec<crate::pass_state_contract::StateDraft>),
    /// 波三刀 A：一批 lesson 草稿（三元组 + 错误轴），`reference` 是 `l1..lN`。
    Lesson(Vec<crate::pass_lesson_contract::LessonDraft>),
}

/// 一次成功重放的全部输入：审核 prompt 与写库参数都能从这里再算一遍。
#[derive(Debug, Clone)]
pub struct Replay {
    pub batch: Batch,
    pub ctx: BatchCtx,
    pub owner: OwnerCtx,
    pub scope: Scope,
    pub drafts: ReplayDrafts,
}

/// 重放失败：**gap（判定）**与**Store（真 Err）**分开，调用方据此决定"退回人工"还是"重试"。
#[derive(Debug)]
pub enum ReplayFailure {
    Gap(ReplayGap),
    /// 转录 SQL/句柄错误。转成 `MemError::Sql` 是这条 API 的既定形状（与 `review` 的其余
    /// 只读 API 同一条错误族）。
    Store(MemError),
}

impl From<MemError> for ReplayFailure {
    fn from(e: MemError) -> Self {
        ReplayFailure::Store(e)
    }
}

fn shape<T>() -> Result<T, ReplayFailure> {
    Err(ReplayFailure::Gap(ReplayGap::PayloadShape))
}

fn gap<T>(gap: ReplayGap) -> Result<T, ReplayFailure> {
    Err(ReplayFailure::Gap(gap))
}

// ── 形状读取（与 `gate::parse_*` 同一把尺子，只多读新键）──────────────────────

fn req_str(map: &Map<String, Value>, key: &str) -> Result<String, ReplayFailure> {
    match map.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        _ => shape(),
    }
}

/// `value` / `object_anchor_id` / `object_canonical` / `place_*` 的形状：缺键与 `null`
/// 都读成 `None`（与 `gate::opt_str` 同一口径——非 string 的非 null 值才是形状错）。
fn opt_str(map: &Map<String, Value>, key: &str) -> Result<Option<String>, ReplayFailure> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        _ => shape(),
    }
}

fn str_array(value: Option<&Value>) -> Result<Vec<String>, ReplayFailure> {
    let array = match value {
        Some(Value::Array(array)) => array,
        _ => return shape(),
    };
    let mut out = Vec::with_capacity(array.len());
    for entry in array {
        match entry {
            Value::String(s) => out.push(s.clone()),
            _ => return shape(),
        }
    }
    Ok(out)
}

/// 证据逐格的 `(turn_uid, anchor_role 文案)`。`anchor_role` 必须在场（与
/// `gate::parse_evidence` 同一形状：payload 写的是"每格都有角色"），且**原样**带回：
/// H4a 要拿这段文案与转录行现在的 `role` 逐条比对，解析成枚举会把脏值读成"另一个合法值"。
fn evidence_turns(value: Option<&Value>) -> Result<Vec<(String, String)>, ReplayFailure> {
    let array = match value {
        Some(Value::Array(array)) => array,
        _ => return shape(),
    };
    let mut out = Vec::with_capacity(array.len());
    for entry in array {
        let obj = match entry.as_object() {
            Some(obj) => obj,
            None => return shape(),
        };
        let role = match obj.get("anchor_role") {
            Some(Value::String(role)) => role.clone(),
            _ => return shape(),
        };
        out.push((req_str(obj, "turn_uid")?, role));
    }
    Ok(out)
}

/// 载荷里逐条证据的角色声明（H4a 的比对输入）。`FactDraft`/`EventDraft` 仍只存 uid；
/// 角色文案走这条旁路，比对在 [`replay`] 里做。
type EvidenceRoles = Vec<(String, String)>;

/// `(turn_uid, role 文案)` 与转录行逐条比对。任一条对不上（含证据 uid 不在本批）⇒
/// `TurnMismatch`：载荷里的角色必须仍是这一行**现在**的角色。
fn verify_evidence_roles(
    evidence: &[(String, String)],
    rows: &[TurnRow],
) -> Result<(), ReplayFailure> {
    let current: HashMap<&str, &str> =
        rows.iter().map(|row| (row.turn_uid.as_str(), row.role.as_str())).collect();
    for (uid, role) in evidence {
        match current.get(uid.as_str()) {
            Some(stored) if *stored == role.as_str() => {}
            _ => return gap(ReplayGap::TurnMismatch),
        }
    }
    Ok(())
}

/// naming 的角色声明：`anchors[].evidence[]` 的 `(turn_uid, anchor_role)`。`naming_batch`
/// 已把角色解析成 [`Provenance`]；这里回成落库文案（`human`/`agent`/`system`）再比对。
fn naming_evidence_roles(a1: &A1Batch) -> EvidenceRoles {
    a1.anchors
        .iter()
        .flat_map(|anchor| {
            anchor
                .evidence
                .iter()
                .map(|provenance| {
                    (provenance.turn_uid.clone(), provenance.anchor_role.as_str().to_string())
                })
        })
        .collect()
}

fn is_turn_uid(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

// ── owner / scope ─────────────────────────────────────────────────────────────

/// `Visibility` 的五个落库字面量（`owner::parse_visibility` 认得的全集中"不是兜底"的那五个）。
/// 重放**不能**用它的兜底：脏值会被静默读成 `PrivateAgent`，重建出的 `OwnerCtx` 就与
/// 入队时不是同一份（闭集外的值必须是 `OwnerShape`，不是"最保守的一档"）。
const VISIBILITIES: [&str; 5] = ["private_agent", "person", "project", "org", "public"];

fn owner_of(map: &Map<String, Value>) -> Result<(OwnerCtx, Scope), ReplayFailure> {
    let owner_principal_id = match map.get("owner_principal_id") {
        None => return gap(ReplayGap::OwnerShape),
        Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return gap(ReplayGap::OwnerShape),
    };
    let owner_agent_id = match map.get("owner_agent_id").and_then(Value::as_str) {
        // 与 `gate::parse_payload` 同一条下界：所有者 agent 不许是空白。
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => return gap(ReplayGap::OwnerShape),
    };
    let visibility = match map.get("visibility").and_then(Value::as_str) {
        Some(s) if VISIBILITIES.contains(&s) => s,
        _ => return gap(ReplayGap::OwnerShape),
    };
    let retention = match map.get("retention").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return gap(ReplayGap::OwnerShape),
    };
    let kind = match map.get("scope_kind").and_then(Value::as_str) {
        Some(s) => match ScopeKind::parse(s) {
            Some(kind) => kind,
            None => return gap(ReplayGap::OwnerShape),
        },
        None => return gap(ReplayGap::OwnerShape),
    };
    let key = match map.get("scope_key").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return gap(ReplayGap::OwnerShape),
    };
    Ok((
        OwnerCtx {
            owner_principal_id,
            owner_agent_id,
            visibility: owner::parse_visibility(visibility),
            retention,
        },
        Scope { kind, key },
    ))
}

// ── 转录回查 ──────────────────────────────────────────────────────────────────

/// 按 `turn_uids` 的**顺序**回查转录行。列清单以 `crates/rc-distill/src/extract.rs`
/// 的 `turns` 读法为准（`session_key/device_id/role/body/created_at_ms/ingested_at_ms`）。
///
/// A7 注记：`turns` 表**没有** speaker 列——轮级说话人住在 memory 库的 `mention`
/// （`discourse_role='speaker' AND resolution='resolved'`）。replay 只读转录库（上面
/// "唯一的 I/O"纪律），所以这里保持 `None`；引擎侧在 replay 之后用
/// `batch::load_turn_speakers` 回填（`review::run::fill_turn_speakers`，三个送审入口共用）。
fn rows_of(
    t: &TranscriptHandle,
    turn_uids: &[String],
    session_key: &str,
    device_id: &str,
) -> Result<Vec<TurnRow>, ReplayFailure> {
    let mut st = t
        .conn()
        .prepare(
            "SELECT session_key, device_id, role, body, created_at_ms, ingested_at_ms
               FROM turns WHERE turn_uid=?1",
        )
        .map_err(MemError::from)?;
    let mut rows = Vec::with_capacity(turn_uids.len());
    for uid in turn_uids {
        let mut query = st.query([uid]).map_err(MemError::from)?;
        let row = match query.next().map_err(MemError::from)? {
            None => return gap(ReplayGap::TurnMissing),
            Some(row) => row,
        };
        let row_session: String = row.get(0).map_err(MemError::from)?;
        let row_device: String = row.get(1).map_err(MemError::from)?;
        // 会话/设备以 payload 为准逐字比对：重放不接受"同一 uid 换了会话"的库。
        if row_session != session_key || row_device != device_id {
            return gap(ReplayGap::TurnMismatch);
        }
        rows.push(TurnRow {
            turn_uid: uid.clone(),
            role: row.get(2).map_err(MemError::from)?,
            body: row.get(3).map_err(MemError::from)?,
            created_at_ms: row.get(4).map_err(MemError::from)?,
            ingested_at_ms: row.get(5).map_err(MemError::from)?,
            speaker: None,
            speaker_anchor_id: None,
        });
    }
    Ok(rows)
}

// ── items 还原 ────────────────────────────────────────────────────────────────

fn fact_drafts(items: &[Value]) -> Result<(Vec<FactDraft>, EvidenceRoles), ReplayFailure> {
    let mut drafts = Vec::with_capacity(items.len());
    let mut roles: EvidenceRoles = Vec::new();
    for (index, entry) in items.iter().enumerate() {
        let obj = match entry.as_object() {
            Some(obj) => obj,
            None => return shape(),
        };
        let evidence = evidence_turns(obj.get("evidence"))?;
        roles.extend(evidence.iter().cloned());
        drafts.push(FactDraft {
            // `reference` 只由**位置**决定（与 `fact_review_user` 的编号同一套纪律）。
            reference: format!("f{}", index + 1),
            turn_uid: req_str(obj, "turn_uid")?,
            subject_anchor_id: req_str(obj, "subject_anchor_id")?,
            object_anchor_id: opt_str(obj, "object_anchor_id")?,
            subject_canonical: req_str(obj, "subject_canonical")?,
            object_canonical: opt_str(obj, "object_canonical")?,
            predicate: req_str(obj, "predicate")?,
            value: opt_str(obj, "value")?,
            category: req_str(obj, "category")?,
            mode: req_str(obj, "mode")?,
            time_text: req_str(obj, "time_text")?,
            quote: req_str(obj, "quote")?,
            evidence_turn_uids: evidence.into_iter().map(|(uid, _)| uid).collect(),
            text: req_str(obj, "text")?,
        });
    }
    Ok((drafts, roles))
}

fn event_drafts(
    items: &[Value],
    extras: &[Value],
) -> Result<(Vec<EventDraft>, EvidenceRoles), ReplayFailure> {
    if items.len() != extras.len() {
        // `items` 与 `extras` 逐下标对应；长度不等就没有"同下标"可读。
        return shape();
    }
    let mut drafts = Vec::with_capacity(items.len());
    let mut roles: EvidenceRoles = Vec::new();
    for (index, (entry, extra)) in items.iter().zip(extras).enumerate() {
        let obj = match entry.as_object() {
            Some(obj) => obj,
            None => return shape(),
        };
        let extra = match extra.as_object() {
            Some(obj) => obj,
            None => return shape(),
        };
        let value_json = match extra.get("value_json").and_then(Value::as_object) {
            Some(obj) => obj,
            None => return shape(),
        };
        let evidence = evidence_turns(obj.get("evidence"))?;
        roles.extend(evidence.iter().cloned());
        drafts.push(EventDraft {
            reference: format!("e{}", index + 1),
            turn_uid: req_str(obj, "turn_uid")?,
            category: req_str(obj, "category")?,
            // 载荷沿用 `subject_anchor_id` 这一格装 actor（与闸门的列形状同一处真相）。
            actor_anchor_id: req_str(obj, "subject_anchor_id")?,
            object_anchor_id: opt_str(obj, "object_anchor_id")?,
            // `action`/`result`/`object_text`/`trigger` 只在 `extras[i].value_json` 里，不重复读 items。
            action: req_str(value_json, "action")?,
            // §7.5 切片一b 之前落库的载荷没有这一格：重放按空串读，不让旧载荷整批 shape 失败
            // （旧载荷的 category 无法回溯核验，这是重放面的已知代价，记在台账 §10.61）。
            trigger: opt_str(value_json, "trigger")?.unwrap_or_default(),
            result: req_str(value_json, "result")?,
            object_text: opt_str(value_json, "object_text")?,
            participants_anchor_ids: str_array(extra.get("participants"))?,
            place_anchor_id: opt_str(extra, "place_anchor_id")?,
            mode: req_str(obj, "mode")?,
            time_text: req_str(obj, "time_text")?,
            quote: req_str(obj, "quote")?,
            evidence_turn_uids: evidence.into_iter().map(|(uid, _)| uid).collect(),
            text: req_str(obj, "text")?,
            actor_canonical: req_str(obj, "actor_canonical")?,
            object_canonical: opt_str(obj, "object_canonical")?,
            participant_canonicals: str_array(obj.get("participants_canonical"))?,
            place_canonical: opt_str(obj, "place_canonical")?,
        });
    }
    Ok((drafts, roles))
}

/// P2 状态的 `items` 还原。每行固定七格：`subject_anchor_id` / `subject_canonical` /
/// `dimension` / `value_after` / `mode` / `quote` / `evidence`（`text` 由程序重算，不进载荷的
/// 消费面；`predicate` 只给闸看，重放不读）。
///
/// `mode` 只认两值闭集（`expressed` / `inferred`，家是 `pass_prompt::STATE_MODES`）：
/// 模型面写不出 `inferred`，但**程序降级**（C1 的主语轴）会把它写进载荷，所以重放不能
/// 只收一个词——闭集外的词仍按形状失败处理，不猜。
fn state_drafts(
    items: &[Value],
) -> Result<(Vec<crate::pass_state_contract::StateDraft>, EvidenceRoles), ReplayFailure> {
    let mut drafts = Vec::with_capacity(items.len());
    let mut roles: EvidenceRoles = Vec::new();
    for (index, entry) in items.iter().enumerate() {
        let obj = match entry.as_object() {
            Some(obj) => obj,
            None => return shape(),
        };
        let evidence = evidence_turns(obj.get("evidence"))?;
        roles.extend(evidence.iter().cloned());
        let raw_mode = req_str(obj, "mode")?;
        let mode = if raw_mode == crate::pass_prompt::STATE_MODE_EXPRESSED {
            crate::pass_prompt::STATE_MODE_EXPRESSED
        } else if raw_mode == crate::pass_prompt::STATE_MODE_INFERRED {
            crate::pass_prompt::STATE_MODE_INFERRED
        } else {
            return shape();
        };
        drafts.push(crate::pass_state_contract::StateDraft {
            reference: format!("s{}", index + 1),
            turn_uid: req_str(obj, "turn_uid")?,
            subject_anchor_id: req_str(obj, "subject_anchor_id")?,
            subject_canonical: req_str(obj, "subject_canonical")?,
            dimension: req_str(obj, "dimension")?,
            value_after: req_str(obj, "value_after")?,
            mode,
            quote: req_str(obj, "quote")?,
            evidence_turn_uids: evidence.into_iter().map(|(uid, _)| uid).collect(),
        });
    }
    Ok((drafts, roles))
}

/// lesson 的 `items` 还原（波三刀 A）。每项固定六键：
/// `{"title","description","content","error_kind","error_code","turn_uid"}`。
///
/// - 四格正文必须是非空字符串（空白也算空），`error_kind` 必须在
///   [`crate::pass_lesson_contract::LESSON_ERROR_KINDS`] 闭集里；
/// - `turn_uid` 必须非空且在 `payload.batch.turn_uids` 里（代表轮）；
/// - 证据是整批（这一段就是那一次判错的上下文），所以 `evidence_turn_uids` 直接取批序。
///
/// 这里不重抄长度上界：长度是**抽取契约**的门（`parse_lesson_output`），载荷是我们自己写的；
/// 重放要保证的是"能重建一张卡"，不是"再判一次模型"。
fn lesson_drafts(
    items: &[Value],
    turn_uids: &[String],
) -> Result<Vec<crate::pass_lesson_contract::LessonDraft>, ReplayFailure> {
    let turns: HashSet<&str> = turn_uids.iter().map(String::as_str).collect();
    let mut drafts = Vec::with_capacity(items.len());
    for (index, entry) in items.iter().enumerate() {
        let obj = match entry.as_object() {
            Some(obj) => obj,
            None => return shape(),
        };
        let title = req_str(obj, "title")?;
        let description = req_str(obj, "description")?;
        let content = req_str(obj, "content")?;
        if title.trim().is_empty() || description.trim().is_empty() || content.trim().is_empty() {
            return shape();
        }
        let error_kind = req_str(obj, "error_kind")?;
        if !crate::pass_lesson_contract::LESSON_ERROR_KINDS.contains(&error_kind.as_str()) {
            return shape();
        }
        let turn_uid = req_str(obj, "turn_uid")?;
        if !turns.contains(turn_uid.as_str()) {
            return shape();
        }
        drafts.push(crate::pass_lesson_contract::LessonDraft {
            reference: format!("l{}", index + 1),
            turn_uid,
            title: title.trim().to_string(),
            description: description.trim().to_string(),
            content: content.trim().to_string(),
            error_kind,
            error_code: req_str(obj, "error_code")?,
            evidence_turn_uids: turn_uids.to_vec(),
        });
    }
    Ok(drafts)
}

/// rows 的 `items` 还原（S3c-3a）。每项固定三键：
/// `{"row": {...}, "evidence_turn_uid": "...", "conflict": null|{...}}`。
///
/// - `row` 必须是非空对象，且 `id`/`text`/`evidence_turn_uid` 是**非空字符串**（行的最小身份）；
/// - 顶层 `evidence_turn_uid` 必须非空且在 `payload.batch.turn_uids` 里；
/// - `conflict` 必须在场，是 `null` 或对象。
///
/// **不**校验 fact 列闭集：闭集是 `gate_rows` 的事，重放只保证"能重建"一份行。
fn rows_drafts(items: &[Value], turn_uids: &[String]) -> Result<Vec<RowsDraft>, ReplayFailure> {
    let turns: HashSet<&str> = turn_uids.iter().map(String::as_str).collect();
    let mut drafts = Vec::with_capacity(items.len());
    for entry in items {
        let item = match entry.as_object() {
            Some(item) => item,
            None => return shape(),
        };
        let values = match item.get("row").and_then(Value::as_object) {
            Some(row) if !row.is_empty() => row.clone(),
            _ => return shape(),
        };
        for key in ["id", "text", "evidence_turn_uid"] {
            match values.get(key).and_then(Value::as_str) {
                Some(text) if !text.is_empty() => {}
                _ => return shape(),
            }
        }
        let evidence_turn_uid = req_str(item, "evidence_turn_uid")?;
        if evidence_turn_uid.is_empty() || !turns.contains(evidence_turn_uid.as_str()) {
            return shape();
        }
        // 缺键不是"没有冲突"：载荷固定写这一格，重放不替它补默认值。
        let conflict = match item.get("conflict") {
            Some(Value::Null) => None,
            Some(value @ Value::Object(_)) => Some(value.clone()),
            _ => return shape(),
        };
        drafts.push(RowsDraft { values, evidence_turn_uid, conflict });
    }
    Ok(drafts)
}

/// naming 的 `items` 四键还原（S3b-3b）。形状与 `pass_naming::review_items` 逐键对齐：
/// `anchors[]`（`reference/kind/canonical/aliases/reality_status/evidence/stable`）、
/// `mentions[]`（`anchor_ref/surface/turn_uid/speaker_ref/discourse_role/resolution/confidence`）、
/// `issues[]`（`kind/refs/reason`）、`unresolved[]`（`surface/turn_uid/reason`）。
///
/// 三类引用**在这里**校验（写库层不再兜底：`write_a1_with_owner` 的 `by_ref[..]`
/// 缺键会直接炸）：
/// 1. `reference` 必须是 `a{位置+1}`（唯一由位置保证）；
/// 2. `anchor_ref`/`speaker_ref` 必须命中 anchors 的 refs；
/// 3. 锚证据与 mention 的 `turn_uid` 必须在 `payload.batch.turn_uids` 里。
fn naming_batch(
    value: &Value,
    turn_uids: &[String],
) -> Result<A1Batch, ReplayFailure> {
    let obj = match value.as_object() {
        Some(obj) => obj,
        None => return shape(),
    };
    let turns: HashSet<&str> = turn_uids.iter().map(String::as_str).collect();

    let anchor_values = match obj.get("anchors").and_then(Value::as_array) {
        Some(values) => values,
        None => return shape(),
    };
    let mut anchors = Vec::with_capacity(anchor_values.len());
    let mut refs: HashSet<String> = HashSet::with_capacity(anchor_values.len());
    for (index, entry) in anchor_values.iter().enumerate() {
        let anchor = match entry.as_object() {
            Some(anchor) => anchor,
            None => return shape(),
        };
        let reference = req_str(anchor, "reference")?;
        // 位置编号：`a1..aN`，不许跳号、不许重号（重复由"逐位置相等"直接挡住）。
        if reference != format!("a{}", index + 1) {
            return shape();
        }
        let evidence_values = match anchor.get("evidence").and_then(Value::as_array) {
            Some(values) => values,
            None => return shape(),
        };
        let mut evidence = Vec::with_capacity(evidence_values.len());
        for entry in evidence_values {
            let pair = match entry.as_object() {
                Some(pair) => pair,
                None => return shape(),
            };
            let turn_uid = req_str(pair, "turn_uid")?;
            if !turns.contains(turn_uid.as_str()) {
                return shape();
            }
            let role = req_str(pair, "anchor_role")?;
            let anchor_role = match AnchorRole::parse(&role) {
                Some(role) => role,
                None => return shape(),
            };
            evidence.push(Provenance { turn_uid, anchor_role });
        }
        let stable = match anchor.get("stable").and_then(Value::as_bool) {
            Some(stable) => stable,
            None => return shape(),
        };
        refs.insert(reference.clone());
        anchors.push(A1Anchor {
            reference,
            kind: req_str(anchor, "kind")?,
            canonical: req_str(anchor, "canonical")?,
            aliases: str_array(anchor.get("aliases"))?,
            reality_status: req_str(anchor, "reality_status")?,
            evidence,
            stable,
        });
    }

    let mention_values = match obj.get("mentions").and_then(Value::as_array) {
        Some(values) => values,
        None => return shape(),
    };
    let mut mentions = Vec::with_capacity(mention_values.len());
    for entry in mention_values {
        let mention = match entry.as_object() {
            Some(mention) => mention,
            None => return shape(),
        };
        let anchor_ref = req_str(mention, "anchor_ref")?;
        if !refs.contains(&anchor_ref) {
            return shape();
        }
        let speaker_ref = opt_str(mention, "speaker_ref")?;
        if speaker_ref.as_deref().is_some_and(|speaker| !refs.contains(speaker)) {
            return shape();
        }
        let turn_uid = req_str(mention, "turn_uid")?;
        if !turns.contains(turn_uid.as_str()) {
            return shape();
        }
        let confidence = match mention.get("confidence").and_then(Value::as_f64) {
            Some(confidence) => confidence,
            None => return shape(),
        };
        mentions.push(A1Mention {
            anchor_ref,
            surface: req_str(mention, "surface")?,
            turn_uid,
            speaker_ref,
            discourse_role: req_str(mention, "discourse_role")?,
            resolution: req_str(mention, "resolution")?,
            confidence,
        });
    }

    let issue_values = match obj.get("issues").and_then(Value::as_array) {
        Some(values) => values,
        None => return shape(),
    };
    let mut issues = Vec::with_capacity(issue_values.len());
    for entry in issue_values {
        let issue = match entry.as_object() {
            Some(issue) => issue,
            None => return shape(),
        };
        issues.push(A1Issue {
            kind: req_str(issue, "kind")?,
            refs: str_array(issue.get("refs"))?,
            reason: req_str(issue, "reason")?,
        });
    }

    let unresolved_values = match obj.get("unresolved").and_then(Value::as_array) {
        Some(values) => values,
        None => return shape(),
    };
    let mut unresolved = Vec::with_capacity(unresolved_values.len());
    for entry in unresolved_values {
        let u = match entry.as_object() {
            Some(u) => u,
            None => return shape(),
        };
        unresolved.push(A1Unresolved {
            surface: req_str(u, "surface")?,
            turn_uid: req_str(u, "turn_uid")?,
            reason: req_str(u, "reason")?,
        });
    }

    Ok(A1Batch { anchors, mentions, issues, unresolved })
}

// ── 入口 ──────────────────────────────────────────────────────────────────────
/// 只凭载荷 + 转录库重放一次审核输入。见模块文档的形状与纪律。
///
/// 判定顺序：信封（schema/pass_id/family/run_id）→ 本轮的 pass 组合 → `batch` 形状 →
/// 转录回查 → `owner` 六键 → `items`（event 还有 `extras`）。先判"能不能重放"，
/// 再花转录查询的钱。
pub fn replay(t: &TranscriptHandle, item: &Item) -> Result<Replay, ReplayFailure> {
    let root: Value = match serde_json::from_str(&item.payload_json) {
        Ok(root) => root,
        Err(_) => return shape(),
    };
    let obj = match root.as_object() {
        Some(obj) => obj,
        None => return shape(),
    };

    // —— 信封：与 `gate::parse_payload` 同一组守卫（不放宽）；rows 有自己的 schema ——
    //
    // 先读 `pass_id` 再选 schema：`review.rows.v1` 只属于 rows，其余 pass 仍只认
    // `review.pass.v1`。两边都不许拿对方的信封蒙混。
    let pass_id = req_str(obj, "pass_id")?;
    if pass_id != item.meta.pass_id {
        return shape();
    }
    let schema = if pass_id == "rows" { ROWS_SCHEMA } else { REVIEW_SCHEMA };
    if obj.get("schema").and_then(Value::as_str) != Some(schema) {
        return shape();
    }
    let family = req_str(obj, "family")?;
    if family != item.meta.family {
        return shape();
    }
    // family 只对 fact 取值；其余中心 pass 必须空串（与 `extract_pass_state` 同口径）。
    if pass_id != "fact" && !family.is_empty() {
        return shape();
    }
    req_str(obj, "run_id")?;

    // —— 本层能重放的 pass 组合：fact（带已知 family）、event、naming、state、rows 与 lesson ——
    let drafts_kind = match pass_id.as_str() {
        "fact" => match family_of_slug(&family) {
            // 未知 family 的 fact 不是"形状坏"，是"这一格本轮不重放"。
            Some(_) => DraftsKind::Fact,
            None => return gap(ReplayGap::Unsupported),
        },
        "event" => DraftsKind::Event,
        "naming" => DraftsKind::Naming,
        "state" => DraftsKind::State,
        "rows" => DraftsKind::Rows,
        "lesson" => DraftsKind::Lesson,
        // time 与任何未知 pass 都没有重放实现。
        _ => return gap(ReplayGap::Unsupported),
    };

    // —— batch 形状（与 `gate::parse_payload` 逐键同形）——
    let batch = match obj.get("batch").and_then(Value::as_object) {
        Some(batch) => batch,
        None => return shape(),
    };
    let turn_values = match batch.get("turn_uids").and_then(Value::as_array) {
        Some(values) => values,
        None => return shape(),
    };
    let mut turn_uids = Vec::with_capacity(turn_values.len());
    for value in turn_values {
        match value.as_str() {
            Some(uid) if is_turn_uid(uid) => turn_uids.push(uid.to_string()),
            _ => return shape(),
        }
    }
    let source_agent = req_str(batch, "source_agent")?;
    let device_id = req_str(batch, "device_id")?;
    let session_key = req_str(batch, "session_key")?;
    let source_ms = match batch.get("source_ms").and_then(Value::as_i64) {
        Some(source_ms) => source_ms,
        None => return shape(),
    };

    // `item.turn_uids_json` 与 `batch.turn_uids` 排序后逐项相等（含重复项）——与闸同一条。
    let item_turns: Vec<String> = match serde_json::from_str(&item.turn_uids_json) {
        Ok(turns) => turns,
        Err(_) => return shape(),
    };
    let mut left = turn_uids.clone();
    left.sort();
    let mut right = item_turns;
    right.sort();
    if left != right {
        return shape();
    }

    // —— 转录回查（唯一的 I/O）——
    let rows = rows_of(t, &turn_uids, &session_key, &device_id)?;

    // H4a：`batch.source_ms` 是写入时的批源时刻（`batch_ref` 取批内 `created_at_ms` 最大值）。
    // 转录行被改写/替换后，这一格必须跟着失败，而不是继续拿载荷里的旧值去写库。
    let current_source_ms = rows.iter().map(|row| row.created_at_ms).max().unwrap_or(0);
    if source_ms != current_source_ms {
        return gap(ReplayGap::TurnMismatch);
    }

    // —— owner / scope ——
    let owner_map = match obj.get("owner").and_then(Value::as_object) {
        Some(owner) => owner,
        None => return gap(ReplayGap::OwnerShape),
    };
    let (owner, scope) = owner_of(owner_map)?;

    // —— drafts ——
    // `items` 的形状随 pass 而变：fact/event 是数组，naming 是四键对象（同一段读一次值，
    // 不复制第二份 batch/owner/scope 重建）。
    let items = match obj.get("items") {
        Some(items) => items,
        None => return shape(),
    };
    let (drafts, evidence_roles) = match drafts_kind {
        DraftsKind::Fact => {
            let items = match items.as_array() {
                Some(items) => items,
                None => return shape(),
            };
            let (drafts, roles) = fact_drafts(items)?;
            (ReplayDrafts::Fact(drafts), roles)
        }
        DraftsKind::Event => {
            let items = match items.as_array() {
                Some(items) => items,
                None => return shape(),
            };
            let extras = match obj.get("extras").and_then(Value::as_array) {
                Some(extras) => extras,
                None => return shape(),
            };
            let (drafts, roles) = event_drafts(items, extras)?;
            (ReplayDrafts::Event(drafts), roles)
        }
        DraftsKind::Naming => {
            let a1 = naming_batch(items, &turn_uids)?;
            let roles = naming_evidence_roles(&a1);
            (ReplayDrafts::Naming(a1), roles)
        }
        DraftsKind::State => {
            let items = match items.as_array() {
                Some(items) => items,
                None => return shape(),
            };
            let (drafts, roles) = state_drafts(items)?;
            (ReplayDrafts::State(drafts), roles)
        }
        DraftsKind::Rows => {
            let items = match items.as_array() {
                Some(items) => items,
                None => return shape(),
            };
            // rows 的 item 没有 evidence 数组（证据轮由顶层 `evidence_turn_uid` 单点给出），
            // 所以没有角色声明要回查；行值原样带回。
            (ReplayDrafts::Rows(rows_drafts(items, &turn_uids)?), EvidenceRoles::new())
        }
        DraftsKind::Lesson => {
            let items = match items.as_array() {
                Some(items) => items,
                None => return shape(),
            };
            // lesson 的证据是**整批**（这一段就是那一次判错的上下文），没有逐条 evidence 数组：
            // 与 rows 同一条，没有角色声明要回查。
            (ReplayDrafts::Lesson(lesson_drafts(items, &turn_uids)?), EvidenceRoles::new())
        }
    };

    // H4a：证据角色必须仍是转录行**现在**的角色（fact/event 的 `items[].evidence[]`、
    // naming 的 `anchors[].evidence[]` 同一条闸）。
    verify_evidence_roles(&evidence_roles, &rows)?;

    Ok(Replay {
        // `project_hint` 不参与审核/写库：scope 从 `payload.owner` 来（设计 §4）。
        batch: Batch {
            session_key,
            device_id: device_id.clone(),
            source_agent: source_agent.clone(),
            project_hint: None,
            rows,
        },
        ctx: BatchCtx {
            source_agent,
            device_id,
        },
        owner,
        scope,
        drafts,
    })
}

/// `pass_id` → 该解哪一套 items（只在本模块用；`ReplayDrafts` 的变体已经表达了结果）。
enum DraftsKind {
    Fact,
    Event,
    Naming,
    Rows,
    State,
    Lesson,
}
