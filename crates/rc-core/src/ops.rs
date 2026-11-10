//! core 工具背后的操作层（设计 §9.4/§10.3，工具语义见工具契约 §6）。
//!
//! 每个写方法都走同一条流水线：[`CoreStore::update_doc`] 的**原子读改写**——写锁
//! （`BEGIN IMMEDIATE`）内读当前载荷 → 闭包在内存里改 → 同一事务落一个新版本。
//! 因此写路径永远不直接 UPDATE 物理表，也永远不发出一条只改一半的记录：校验失败就整份
//! 不存，版本号只在保存成功时前进；而且读发生在写锁之后，两个写者不可能各持 vN 载荷
//! 互盖（丢更新）——闭包看到的一定是最新已提交版本。
//!
//! L0 不可自改在两道闸上落地：`PersonaPatch` 没有 `l0` 字段且 `deny_unknown_fields`，
//! 带 `l0` 的 patch 在反序列化阶段就失败；`apply_patch` 只覆盖 L1 的六个字段，从不触碰
//! `payload.l0`。

use crate::crypto::{aad, aad_notebook, load_key, open, seal, sha256_hex_of, NONCE_LEN};
use crate::payload::{
    Affect, CorePayload, HandoffItem, JsonMap, LoopItem, Persona, PinnedEpisode, Relationship,
    Subject, HANDOFF_CLOSE_REASONS, HANDOFF_KINDS, HANDOFF_OPEN_LIMIT, HANDOFF_TEXT_MAX,
    HANDOFF_TTL_MS, HANDOFF_TOTAL_LIMIT,
};
use crate::store::{checked_agent_id, CoreStore, SECTION_DOC};
use crate::CoreError;
use rusqlite::{params, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 提案状态机：`proposed` 只能走向 `applied` 或 `rejected`，两步都是终态。
pub const PROPOSAL_PROPOSED: &str = "proposed";
pub const PROPOSAL_APPLIED: &str = "applied";
pub const PROPOSAL_REJECTED: &str = "rejected";

/// loop 的合法状态（§9.2 的 `open|done|dropped`）。
pub const LOOP_OPEN: &str = "open";
pub const LOOP_DONE: &str = "done";
pub const LOOP_DROPPED: &str = "dropped";

/// 情绪只有 user/agent 两条（§9.2）。
pub const AFFECT_KINDS: [&str; 2] = ["user", "agent"];

/// 主用户 1 个 + 最多 3 个核心关系（§9.6）。
pub const SUBJECT_LIMIT: usize = 4;

/// 未关闭且已过期的交接条在**写时清理**里的宽限期：7 天。
///
/// 一条"到点但没人关"的提醒，如果 24 小时窗内没人读就被真删，它就再也不会出现。
/// 所以已关闭且过期的立即删；未关闭且过期的先留满这段宽限，过期超过宽限期才真删。
/// 宽限期内的过期条由 [`crate::CoreStore::handoff_list`] 照常返回，过期与否交给工具面标。
pub const HANDOFF_EXPIRED_GRACE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// `core_handoff_write` 的回执：新条 id、被取代的旧 `continue` id 列表、落盘版本。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffWrite {
    pub id: String,
    pub superseded: Vec<String>,
    pub version: u32,
}

/// `core_handoff_close` 的回执。已关闭的再关一次是幂等的：`already_closed=true`，
/// 且**不新增版本**（`version` 是那一刻文档的真实版本，不是"又存了一版"）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffClose {
    pub id: String,
    pub closed_at_ms: i64,
    pub reason: String,
    pub already_closed: bool,
    pub version: u32,
}

/// 一次人格自编辑提案（§9.4）：证据、理由、状态与落款时间都留下。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonaProposal {
    pub id: String,
    pub patch: PersonaPatch,
    pub reason: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    /// `proposed|applied|rejected`。
    pub status: String,
    pub created_at_ms: i64,
    #[serde(default)]
    pub applied_at_ms: Option<i64>,
}

/// L1 人格补丁。**没有 `l0`**，且 `deny_unknown_fields`：任何带 `l0` 的 patch
/// 在反序列化时就失败（L0 只能由用户在文档层改）。
/// 所有字段都是 `Option`：`None` 表示"这一项不动"。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonaPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traits: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quirks: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundaries: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_examples: Option<Vec<String>>,
}

/// `core_interaction_touch` 的入参：只给 `Some` 的项动值，`None` 一律不碰。
///
/// 形状与载荷里的 `Interaction`（§9.2）逐字段对应：`last_user_message_at_ms`、
/// `last_agent_initiated_at_ms`、`unanswered_user_messages`、`attention_debt_ms`、
/// `current_topic`。工具契约 §6 要求 touch 只收时间戳/计数，所以这里没有自由正文；
/// 唯一的文本是话题名，给空串表示清空。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionTouch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_user_message_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_agent_initiated_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unanswered_user_messages: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention_debt_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_topic: Option<String>,
}

/// 提案 id：`sha256("persona-proposal|agent_id|now|reason|index")` 的 64 位 hex。
/// 纯函数，同一输入永远同一 id；`index` 是提案当前序号，让同一毫秒的重复提案不撞。
pub fn proposal_id(agent_id: &str, now_ms: i64, reason: &str, index: usize) -> String {
    sha256_hex_of(
        format!("persona-proposal|{agent_id}|{now_ms}|{reason}|{index}").as_bytes(),
    )
}

/// 把 patch 叠到人格上。纯函数：不读库、不写库，也不碰任何调用方的 `l0`。
/// `traits`/`style` 必须是 JSON 对象；其它形状 ⇒ `BadPayload`（不静默丢内容）。
pub fn apply_patch(persona: &Persona, patch: &PersonaPatch) -> Result<Persona, CoreError> {
    let mut next = persona.clone();
    if let Some(core_text) = &patch.core_text {
        next.core_text = core_text.clone();
    }
    if let Some(traits) = &patch.traits {
        next.traits = object_map(traits, "traits")?;
    }
    if let Some(style) = &patch.style {
        next.style = object_map(style, "style")?;
    }
    if let Some(quirks) = &patch.quirks {
        next.quirks = quirks.clone();
    }
    if let Some(boundaries) = &patch.boundaries {
        next.boundaries = boundaries.clone();
    }
    if let Some(voice_examples) = &patch.voice_examples {
        next.voice_examples = voice_examples.clone();
    }
    Ok(next)
}

fn object_map(value: &Value, field: &str) -> Result<JsonMap, CoreError> {
    match value {
        Value::Object(map) => Ok(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        _ => Err(CoreError::BadPayload(format!(
            "persona patch {field} must be a JSON object"
        ))),
    }
}

/// 在保存前就把 patch 的形状核一遍：形状错的提案不该进文档（否则 apply 时才发现）。
fn checked_patch(patch: &PersonaPatch) -> Result<(), CoreError> {
    if let Some(traits) = &patch.traits {
        object_map(traits, "traits")?;
    }
    if let Some(style) = &patch.style {
        object_map(style, "style")?;
    }
    Ok(())
}

impl CoreStore {
    /// 追加一条提案（`status=proposed`），**不改** persona；保存后返回 `(版本, 提案 id)`。
    ///
    /// `reason` 空白 ⇒ `BadPayload`；`evidence` 可以为空（只记"没有证据"这一事实）。
    /// 提案 id 的 `index` 在**写锁内**取当前待审数——计数与插入同一 `BEGIN IMMEDIATE`，
    /// 并发的第二个提案看到的是 +1 后的行数（小本本 append 的同形先例），同毫秒的两个
    /// 并发提案不会撞 id。
    pub fn persona_propose(
        &self,
        agent_id: &str,
        patch: PersonaPatch,
        reason: &str,
        evidence: Vec<String>,
        device_id: &str,
        now_ms: i64,
    ) -> Result<(u32, String), CoreError> {
        if reason.trim().is_empty() {
            return Err(CoreError::BadPayload(
                "persona proposal reason is empty".to_string(),
            ));
        }
        let agent_id = checked_agent_id(agent_id)?;
        checked_patch(&patch)?;
        let mut id = String::new();
        let version = self.mutate_doc(agent_id, device_id, now_ms, |payload| {
            let index = payload.persona_proposals.len();
            id = proposal_id(agent_id, now_ms, reason, index);
            payload.persona_proposals.push(PersonaProposal {
                id: id.clone(),
                patch,
                reason: reason.to_string(),
                evidence,
                status: PROPOSAL_PROPOSED.to_string(),
                created_at_ms: now_ms,
                applied_at_ms: None,
            });
            Ok(())
        })?;
        Ok((version, id))
    }

    /// 应用提案：只接受 `proposed`；应用后提案**出队**（不再留在载荷里），persona 落 patch。
    /// 未找到 ⇒ `NotFound`（已出队的终态提案再 apply 也是 `NotFound`）；旧版载荷里还挂着
    /// `applied|rejected` 的历史提案 ⇒ `BadPayload`。
    pub fn persona_apply(
        &self,
        agent_id: &str,
        proposal_id: &str,
        device_id: &str,
        now_ms: i64,
    ) -> Result<u32, CoreError> {
        self.mutate_doc(agent_id, device_id, now_ms, |payload| {
            let index = proposal_index(payload, proposal_id)?;
            if payload.persona_proposals[index].status != PROPOSAL_PROPOSED {
                return Err(CoreError::BadPayload(format!(
                    "persona proposal {proposal_id} is already {}",
                    payload.persona_proposals[index].status
                )));
            }
            // 出队与应用同一版完成：终态提案不再留在 `payload.persona_proposals`
            // （"只增不清、数组无界增长"的修复）。历史不丢：这一版之前的载荷
            // （提案还在、还是 `proposed`、带着全部证据）就在 rev 表里。
            let proposal = payload.persona_proposals.remove(index);
            let next = apply_patch(&payload.persona, &proposal.patch)?;
            payload.persona = next;
            Ok(())
        })
    }

    /// 拒绝提案：只接受 `proposed`；persona 不变，提案**出队**（不再留在载荷里）。
    /// 拒绝的账在版本历史里：被拒提案完整地留在 rev 表的前一版载荷中。
    pub fn persona_reject(
        &self,
        agent_id: &str,
        proposal_id: &str,
        device_id: &str,
        now_ms: i64,
    ) -> Result<u32, CoreError> {
        self.mutate_doc(agent_id, device_id, now_ms, |payload| {
            let index = proposal_index(payload, proposal_id)?;
            if payload.persona_proposals[index].status != PROPOSAL_PROPOSED {
                return Err(CoreError::BadPayload(format!(
                    "persona proposal {proposal_id} is already {}",
                    payload.persona_proposals[index].status
                )));
            }
            payload.persona_proposals.remove(index);
            Ok(())
        })
    }

    /// 回滚人格到某一版的 `persona`：生成**新版本**，历史 revision 一行不删。
    /// 版本不存在 ⇒ `NotFound`。旧版 persona 是 rev 表里 append-only 的历史行，锁外读
    /// 不构成丢更新窗口；真正的读改写走 [`CoreStore::update_doc`]，在写锁内落到最新载荷上。
    pub fn persona_revert(
        &self,
        agent_id: &str,
        version: u32,
        device_id: &str,
        now_ms: i64,
    ) -> Result<u32, CoreError> {
        let agent_id = checked_agent_id(agent_id)?;
        let old = self
            .try_load_revision(agent_id, version)?
            .ok_or(CoreError::NotFound)?;
        self.mutate_doc(agent_id, device_id, now_ms, move |payload| {
            payload.persona = old.persona;
            Ok(())
        })
    }

    /// 按 `subject_id` upsert；`is_primary=true` 会把其它 subject 降为 false。
    /// 超过 4 个（1 primary + 3 核心关系）⇒ `BadPayload`，且整份不存。
    pub fn subject_update(
        &self,
        agent_id: &str,
        subject: Subject,
        device_id: &str,
        now_ms: i64,
    ) -> Result<u32, CoreError> {
        if subject.subject_id.trim().is_empty() {
            return Err(CoreError::BadPayload("subject_id is empty".to_string()));
        }
        let subject_id = subject.subject_id.clone();
        let is_primary = subject.is_primary;
        self.mutate_doc(agent_id, device_id, now_ms, move |payload| {
            if let Some(index) = payload
                .subjects
                .iter()
                .position(|s| s.subject_id == subject_id)
            {
                payload.subjects[index] = subject;
            } else {
                payload.subjects.push(subject);
            }
            if is_primary {
                for other in payload.subjects.iter_mut() {
                    if other.subject_id != subject_id {
                        other.is_primary = false;
                    }
                }
            }
            if payload.subjects.len() > SUBJECT_LIMIT {
                return Err(CoreError::BadPayload(format!(
                    "at most {SUBJECT_LIMIT} subjects are allowed (1 primary + 3)"
                )));
            }
            Ok(())
        })
    }

    /// 只更新给了值的交互字段；`unanswered_user_messages` 给了就覆盖；
    /// `current_topic` 给空串（或纯空白）表示清空。
    pub fn interaction_touch(
        &self,
        agent_id: &str,
        touch: InteractionTouch,
        device_id: &str,
        now_ms: i64,
    ) -> Result<u32, CoreError> {
        self.mutate_doc(agent_id, device_id, now_ms, move |payload| {
            let interaction = &mut payload.interaction;
            if let Some(value) = touch.last_user_message_at_ms {
                interaction.last_user_message_at_ms = value;
            }
            if let Some(value) = touch.last_agent_initiated_at_ms {
                interaction.last_agent_initiated_at_ms = value;
            }
            if let Some(value) = touch.unanswered_user_messages {
                interaction.unanswered_user_messages = value;
            }
            if let Some(value) = touch.attention_debt_ms {
                interaction.attention_debt_ms = value;
            }
            if let Some(topic) = touch.current_topic {
                if topic.trim().is_empty() {
                    interaction.current_topic.clear();
                } else {
                    interaction.current_topic = topic;
                }
            }
            Ok(())
        })
    }

    /// 按 `subject_kind` 替换（user 一条、agent 一条）；`confidence` 必须在 0..=1。
    pub fn affect_update(
        &self,
        agent_id: &str,
        affect: Affect,
        device_id: &str,
        now_ms: i64,
    ) -> Result<u32, CoreError> {
        if !AFFECT_KINDS.contains(&affect.subject_kind.as_str()) {
            return Err(CoreError::BadPayload(format!(
                "affect subject_kind must be user or agent, got {:?}",
                affect.subject_kind
            )));
        }
        if !(0.0..=1.0).contains(&affect.confidence) {
            return Err(CoreError::BadPayload(
                "affect confidence must be within 0..=1".to_string(),
            ));
        }
        let kind = affect.subject_kind.clone();
        self.mutate_doc(agent_id, device_id, now_ms, move |payload| {
            if let Some(index) = payload
                .affects
                .iter()
                .position(|a| a.subject_kind == kind)
            {
                payload.affects[index] = affect;
            } else {
                payload.affects.push(affect);
            }
            Ok(())
        })
    }

    /// 替换关系段（§9.2 的 `Relationship` 单段，不是列表——简报的「关系与交互」区与
    /// `core_interaction_get` 回执读的就是这一段）。core.db 治理小刀③（2026-11-07
    /// 分诊）：payload 11 段里它此前唯一零写者，本方法是与 affect/subject 同形的
    /// **agent 主动写口**——不接任何蒸馏/程序派生生产者。
    ///
    /// 校验照 [`Self::affect_update`] 的那把尺：四个比值字段 `trust`/`intimacy`/
    /// `tension`/`confidence` 各自必须在 0..=1（NaN 不在区间内，同样被拒），越界整份
    /// 不存、版本不前进；`status`/`text` 是自由文本，不设词表（渲染照抄，不猜业务语义）。
    /// 走 [`CoreStore::update_doc`] 的原子读改写：读发生在写锁之后，两个写者不互盖。
    pub fn relationship_update(
        &self,
        agent_id: &str,
        relationship: Relationship,
        device_id: &str,
        now_ms: i64,
    ) -> Result<u32, CoreError> {
        for (name, value) in [
            ("trust", relationship.trust),
            ("intimacy", relationship.intimacy),
            ("tension", relationship.tension),
            ("confidence", relationship.confidence),
        ] {
            if !(0.0..=1.0).contains(&value) {
                return Err(CoreError::BadPayload(format!(
                    "relationship {name} must be within 0..=1"
                )));
            }
        }
        self.mutate_doc(agent_id, device_id, now_ms, move |payload| {
            payload.relationship = relationship;
            Ok(())
        })
    }

    /// 按 `id` upsert 未闭环事项；`status` 必须是 `open|done|dropped`。
    pub fn loop_upsert(
        &self,
        agent_id: &str,
        item: LoopItem,
        device_id: &str,
        now_ms: i64,
    ) -> Result<u32, CoreError> {
        if item.id.trim().is_empty() {
            return Err(CoreError::BadPayload("loop id is empty".to_string()));
        }
        if !matches!(
            item.status.as_str(),
            LOOP_OPEN | LOOP_DONE | LOOP_DROPPED
        ) {
            return Err(CoreError::BadPayload(format!(
                "loop status must be open|done|dropped, got {:?}",
                item.status
            )));
        }
        let id = item.id.clone();
        self.mutate_doc(agent_id, device_id, now_ms, move |payload| {
            if let Some(index) = payload.loops.iter().position(|l| l.id == id) {
                payload.loops[index] = item;
            } else {
                payload.loops.push(item);
            }
            Ok(())
        })
    }

    /// 关闭事项：只接受 `done|dropped`；id 不存在 ⇒ `NotFound`。
    pub fn loop_close(
        &self,
        agent_id: &str,
        id: &str,
        status: &str,
        device_id: &str,
        now_ms: i64,
    ) -> Result<u32, CoreError> {
        if !matches!(status, LOOP_DONE | LOOP_DROPPED) {
            return Err(CoreError::BadPayload(format!(
                "loop close status must be done|dropped, got {status:?}"
            )));
        }
        self.mutate_doc(agent_id, device_id, now_ms, |payload| {
            let item = payload
                .loops
                .iter_mut()
                .find(|l| l.id == id)
                .ok_or(CoreError::NotFound)?;
            item.status = status.to_string();
            Ok(())
        })
    }

    /// 写一张交接条。校验：`kind` 闭集、正文按字符 ≤200、有效期晚于 `now`。
    /// 落盘前的顺序固定为 **清理 → continue 取代 → 上限 → 生成 id → push → 总量裁剪**：
    /// 清理先腾出位置；取代在判上限之前（新 continue 本会取代旧条，未关闭总数不增）；
    /// 总量裁剪放在 push 之后，落盘后不会出现 201。
    pub fn handoff_write(
        &self,
        agent_id: &str,
        kind: &str,
        text: &str,
        expires_at_ms: Option<i64>,
        device_id: &str,
        now_ms: i64,
    ) -> Result<HandoffWrite, CoreError> {
        if !HANDOFF_KINDS.contains(&kind) {
            return Err(CoreError::BadPayload(format!(
                "handoff kind must be continue|due|reminder|note, got {kind:?}"
            )));
        }
        if text.trim().is_empty() {
            return Err(CoreError::BadPayload("handoff text is empty".to_string()));
        }
        if text.chars().count() > HANDOFF_TEXT_MAX {
            return Err(CoreError::BadPayload(format!(
                "handoff text is longer than {HANDOFF_TEXT_MAX} chars"
            )));
        }
        let expires_at_ms = expires_at_ms.unwrap_or(now_ms + HANDOFF_TTL_MS);
        if expires_at_ms <= now_ms {
            return Err(CoreError::BadPayload(
                "handoff expires_at_ms must be later than now".to_string(),
            ));
        }
        let mut id = String::new();
        let mut superseded: Vec<String> = Vec::new();
        let version = self.mutate_doc(agent_id, device_id, now_ms, |payload| {
            // 1. 清理：已关闭且过期的立即删；未关闭且过期的留满宽限期再删。
            payload.handoffs.retain(|h| survives_cleanup(h, now_ms));
            // 2. continue 取代：同 agent 未关闭的旧 continue 一律 `superseded`。
            //    必须在判上限之前——被取代的旧条不再占名额。
            if kind == "continue" {
                for handoff in payload.handoffs.iter_mut() {
                    if handoff.kind == "continue" && handoff.closed_at_ms.is_none() {
                        handoff.closed_at_ms = Some(now_ms);
                        handoff.closed_reason = Some("superseded".to_string());
                        superseded.push(handoff.id.clone());
                    }
                }
            }
            // 3. 上限：按**取代之后**的未关闭条数判；满了就整份不存（错误只带条数，不带正文）。
            let open = payload
                .handoffs
                .iter()
                .filter(|h| h.closed_at_ms.is_none())
                .count();
            if open >= HANDOFF_OPEN_LIMIT {
                return Err(CoreError::HandoffFull(open));
            }
            // 4. 生成 id：`index` 是当前行数，同毫秒同文本的第二条不撞。
            let index = payload.handoffs.len();
            let new_id = format!(
                "h-{}",
                &sha256_hex_of(format!("handoff|{agent_id}|{now_ms}|{index}|{text}").as_bytes())
                    [..12]
            );
            // 5. push：新条永远是未关闭、未被看过。
            payload.handoffs.push(HandoffItem {
                id: new_id.clone(),
                kind: kind.to_string(),
                text: text.to_string(),
                created_at_ms: now_ms,
                expires_at_ms,
                seen_count: 0,
                last_seen_at_ms: None,
                closed_at_ms: None,
                closed_reason: None,
            });
            // 6. 总量裁剪：**push 之后**做，落盘后不会出现 201。
            //    先按同一条清理判据掉可删行，再按 `closed_at_ms` 从旧到新删到 ≤ 上限。
            if payload.handoffs.len() > HANDOFF_TOTAL_LIMIT {
                payload.handoffs.retain(|h| survives_cleanup(h, now_ms));
                while payload.handoffs.len() > HANDOFF_TOTAL_LIMIT {
                    let oldest_closed = payload
                        .handoffs
                        .iter()
                        .enumerate()
                        .filter_map(|(index, h)| h.closed_at_ms.map(|at| (index, at)))
                        .min_by_key(|(_, at)| *at)
                        .map(|(index, _)| index);
                    // 没有可删的已关闭行（未关闭条数自己就被上限 50 限死）⇒ 停手，不删未关闭的。
                    let Some(index) = oldest_closed else { break };
                    payload.handoffs.remove(index);
                }
            }
            id = new_id;
            Ok(())
        })?;
        Ok(HandoffWrite { id, superseded, version })
    }

    /// 列出**未关闭**的交接条，**含已过期的**（过期与否交给工具面标 `expired`）。
    /// **纯读**：不落版本、不删行、不动 `seen_count`。
    /// 排序：`kind` 按 [`HANDOFF_KINDS`] 的下标升序，同 kind 内新的在前。
    pub fn handoff_list(
        &self,
        agent_id: &str,
        _now_ms: i64,
    ) -> Result<Vec<HandoffItem>, CoreError> {
        let agent_id = checked_agent_id(agent_id)?;
        let payload = self.load_or_empty(agent_id)?;
        let mut open: Vec<HandoffItem> = payload
            .handoffs
            .into_iter()
            .filter(|h| h.closed_at_ms.is_none())
            .collect();
        let rank = |kind: &str| {
            HANDOFF_KINDS
                .iter()
                .position(|k| *k == kind)
                .unwrap_or(usize::MAX)
        };
        open.sort_by(|a, b| {
            rank(&a.kind)
                .cmp(&rank(&b.kind))
                .then_with(|| b.created_at_ms.cmp(&a.created_at_ms))
        });
        Ok(open)
    }

    /// 关闭一张交接条。`reason` 缺省 `manual`；已关闭的再关一次是**幂等**返回：
    /// `already_closed=true`、时间与理由取行里的值、`version` 取当前文档版本（不 save）。
    pub fn handoff_close(
        &self,
        agent_id: &str,
        id: &str,
        reason: Option<&str>,
        device_id: &str,
        now_ms: i64,
    ) -> Result<HandoffClose, CoreError> {
        let reason = reason.unwrap_or("manual");
        if !HANDOFF_CLOSE_REASONS.contains(&reason) {
            return Err(CoreError::BadPayload(format!(
                "handoff close reason must be done|superseded|manual, got {reason:?}"
            )));
        }
        let agent_id = checked_agent_id(agent_id)?;
        let payload = self.load_or_empty(agent_id)?;
        let Some(existing) = payload.handoffs.iter().find(|h| h.id == id) else {
            return Err(CoreError::NotFound);
        };
        if let Some(closed_at_ms) = existing.closed_at_ms {
            let reason = existing.closed_reason.clone().unwrap_or_default();
            let version = self.doc_version(agent_id)?;
            return Ok(HandoffClose {
                id: id.to_string(),
                closed_at_ms,
                reason,
                already_closed: true,
                version,
            });
        }
        let version = self.mutate_doc(agent_id, device_id, now_ms, |payload| {
            let item = payload
                .handoffs
                .iter_mut()
                .find(|h| h.id == id)
                .ok_or(CoreError::NotFound)?;
            item.closed_at_ms = Some(now_ms);
            item.closed_reason = Some(reason.to_string());
            Ok(())
        })?;
        Ok(HandoffClose {
            id: id.to_string(),
            closed_at_ms: now_ms,
            reason: reason.to_string(),
            already_closed: false,
            version,
        })
    }

    /// 物理删除已过期的交接条，返回删掉的行数（H3 的运维出口；agent 侧只在**写**时做同样的事）。
    ///
    /// 写路径对"未关闭且过期"的条留 [`HANDOFF_EXPIRED_GRACE_MS`] 宽限；这里是运维口，
    /// 判据仍是"过了 `now_ms` 就删"——手工清理是人的决定，不需要宽限。
    /// 不做有界批：载荷总量上限 [`HANDOFF_TOTAL_LIMIT`] 已经把单 agent 的过期行限死在这个数以内。
    /// **没删到东西就不落版本**——一次"本来就没有过期行"的巡检不该在人格历史里留一条。
    pub fn handoff_prune(
        &self,
        agent_id: &str,
        now_ms: i64,
        device_id: &str,
    ) -> Result<usize, CoreError> {
        let agent_id = checked_agent_id(agent_id)?;
        let mut removed = 0usize;
        self.update_doc(agent_id, device_id, now_ms, |payload| {
            let before = payload.handoffs.len();
            payload.handoffs.retain(|h| h.expires_at_ms > now_ms);
            removed = before - payload.handoffs.len();
            // 没删到东西就不落版本；注册了但还没写过文档 ⇒ 闭包看到的是空载荷，
            // 同样走这条"不落版"的出路，回执是 0 而不是"未注册"。
            Ok(removed > 0)
        })?;
        Ok(removed)
    }

    /// pin 一段共同经历：按 `episode_id` upsert，`reason` 不能为空。
    pub fn episode_pin(
        &self,
        agent_id: &str,
        episode_id: &str,
        reason: &str,
        device_id: &str,
        now_ms: i64,
    ) -> Result<u32, CoreError> {
        if episode_id.trim().is_empty() {
            return Err(CoreError::BadPayload("episode_id is empty".to_string()));
        }
        if reason.trim().is_empty() {
            return Err(CoreError::BadPayload(
                "episode pin reason is empty".to_string(),
            ));
        }
        self.mutate_doc(agent_id, device_id, now_ms, |payload| {
            if let Some(pinned) = payload
                .pinned_episodes
                .iter_mut()
                .find(|p| p.episode_id == episode_id)
            {
                pinned.pin_reason = reason.to_string();
            } else {
                payload
                    .pinned_episodes
                    .push(PinnedEpisode {
                        episode_id: episode_id.to_string(),
                        pin_reason: reason.to_string(),
                    });
            }
            Ok(())
        })
    }

    /// 取消 pin：**幂等**——不在列表里也返回 `Ok`（只落一个新版本，不报错）。
    pub fn episode_unpin(
        &self,
        agent_id: &str,
        episode_id: &str,
        device_id: &str,
        now_ms: i64,
    ) -> Result<u32, CoreError> {
        if episode_id.trim().is_empty() {
            return Err(CoreError::BadPayload("episode_id is empty".to_string()));
        }
        self.mutate_doc(agent_id, device_id, now_ms, |payload| {
            payload
                .pinned_episodes
                .retain(|p| p.episode_id != episode_id);
            Ok(())
        })
    }

    /// 读某一版整份载荷（AAD 用该版 `version` 与 `"doc"` 段）。
    /// 版本不存在（或 agent 未注册）⇒ `None`；解密/结构错误同样折成 `None`。
    /// 需要区分"不存在"与"解不开"的调用方用 [`Self::try_load_revision`]。
    pub fn load_revision(&self, agent_id: &str, version: u32) -> Option<CorePayload> {
        self.try_load_revision(agent_id, version).ok().flatten()
    }

    /// [`Self::load_revision`] 的可诊断版本：AEAD/Crypto 失败会作为错误返回。
    pub fn try_load_revision(
        &self,
        agent_id: &str,
        version: u32,
    ) -> Result<Option<CorePayload>, CoreError> {
        let agent_id = checked_agent_id(agent_id)?;
        let Some((nonce, ciphertext)) = self.revision_row(agent_id, version)? else {
            return Ok(None);
        };
        let nonce: [u8; NONCE_LEN] = nonce
            .as_slice()
            .try_into()
            .map_err(|_| CoreError::Crypto)?;
        let key = load_key(self.data_dir(), agent_id)?;
        let plain = open(&key, &aad(agent_id, version, SECTION_DOC), &nonce, &ciphertext)?;
        let payload: CorePayload =
            serde_json::from_slice(&plain).map_err(|e| CoreError::BadPayload(e.to_string()))?;
        if payload.agent_id != agent_id {
            return Err(CoreError::BadPayload(
                "payload agent_id does not match the caller".to_string(),
            ));
        }
        payload.validate()?;
        Ok(Some(payload))
    }

    /// 统一的读 → 改 → 存，走 [`CoreStore::update_doc`] 的原子读改写：读发生在写锁之后，
    /// 闭包看到的一定是最新已提交版本，两个写者不可能互盖（丢更新）。
    /// `mutate` 返回错误时整份不存，版本不前进。
    /// 打开/关闭「中文表达优化」。规则固定，写入的只有开关、版本与时间戳。
    pub fn chinese_expression_set(
        &self,
        agent_id: &str,
        enabled: bool,
        device_id: &str,
        now_ms: i64,
    ) -> Result<u32, CoreError> {
        self.mutate_doc(agent_id, device_id, now_ms, |payload| {
            payload.chinese_expression.enabled = enabled;
            payload.chinese_expression.version = payload.chinese_expression.version.saturating_add(1);
            payload.chinese_expression.updated_at_ms = now_ms;
            Ok(())
        })
    }

    fn mutate_doc<F>(
        &self,
        agent_id: &str,
        device_id: &str,
        now_ms: i64,
        mutate: F,
    ) -> Result<u32, CoreError>
    where
        F: FnOnce(&mut CorePayload) -> Result<(), CoreError>,
    {
        self.update_doc(agent_id, device_id, now_ms, move |payload| {
            mutate(payload)?;
            Ok(true)
        })
    }

    /// 已注册但还没写过文档 ⇒ 从空载荷起（第一次写就是 v1）；未注册 ⇒ `Unregistered`。
    fn load_or_empty(&self, agent_id: &str) -> Result<CorePayload, CoreError> {
        Ok(self
            .load_doc(agent_id)?
            .unwrap_or_else(|| CorePayload::empty(agent_id)))
    }
}

/// 写时清理的判据（`handoff_write` 的前置清理与 push 后的总量裁剪共用同一条）：
///
/// - 没过期 ⇒ 留；
/// - 已关闭且过期 ⇒ 立即删（没人会再读它）；
/// - 未关闭且过期 ⇒ 留满 [`HANDOFF_EXPIRED_GRACE_MS`] 宽限，过期超过宽限期才真删
///   （"到点但没人关"的提醒不该在下次写时无声消失）。
fn survives_cleanup(handoff: &HandoffItem, now_ms: i64) -> bool {
    if handoff.expires_at_ms > now_ms {
        return true;
    }
    if handoff.closed_at_ms.is_some() {
        return false;
    }
    handoff
        .expires_at_ms
        .saturating_add(HANDOFF_EXPIRED_GRACE_MS)
        >= now_ms
}

fn proposal_index(payload: &CorePayload, id: &str) -> Result<usize, CoreError> {
    payload
        .persona_proposals
        .iter()
        .position(|p| p.id == id)
        .ok_or(CoreError::NotFound)
}

// ── 小本本（W36 细案 §2/§3/§5）───────────────────────────────────────────────
//
// 与上面所有操作的两点不同：①它**不走** `load_doc`/`save_doc`——小本本是 core.db 的
// 第三张 per-agent 动态表 `core_note_<h>`，append-only、行级 AEAD；走 payload 单行表
// 会让每次 append 变成整份重加密 + rev 整行追加（O(n²) 膨胀）。②写口无闸：无审核、
// 无 LLM、无队列、无频闸，唯一写校验是形状（下面的常量那几条），形状错的整条不落
// （照 [`CoreStore::handoff_write`] 的校验风格）。
//
// 「core.db 无明文」不变式保持：表上只有 id/created_at_ms/device_id 三个明文列
// （created_at_ms 是调用方传入的发话钟，判据 12a），title/tags/body/source_turn_uid
// 全在 AEAD 内的 JSON 里；检索不上 FTS5（已拍板），是解密后线性匹配。

/// 小本本正文上限：**2000 字符，按字符不按字节**（照 [`HANDOFF_TEXT_MAX`] 的口径）。
pub const NOTEBOOK_BODY_MAX: usize = 2_000;
/// 标签 ≤8 条。
pub const NOTEBOOK_TAGS_MAX: usize = 8;
/// 每条标签 ≤32 字符（按字符）。
pub const NOTEBOOK_TAG_MAX: usize = 32;
/// `core_notebook_list` 的 limit 上限。
pub const NOTEBOOK_LIST_LIMIT_MAX: usize = 200;
/// `core_notebook_list` 的缺省 limit。
pub const NOTEBOOK_LIST_LIMIT_DEFAULT: usize = 50;
/// `core_notebook_search` 的缺省 limit（上限与 list 同一把尺：[`NOTEBOOK_LIST_LIMIT_MAX`]）。
pub const NOTEBOOK_SEARCH_LIMIT_DEFAULT: usize = 20;

/// AEAD 内的行载荷：演进只发生在密文内，物理表形状固定（与 [`crate::CorePayload`] 同一纪律）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct NotebookRow {
    #[serde(default)]
    title: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    body: String,
    /// 只是指针，不参与时间判定与排序；时间过滤永远按明文列 `created_at_ms`。
    #[serde(default)]
    source_turn_uid: Option<String>,
}

/// `core_notebook_append` 的回执：新条 id 与发话钟落库值。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotebookAppend {
    pub id: String,
    pub created_at_ms: i64,
}

/// list/search/索引栏共用的**零正文字段**投影：没有 body，也没有 source_turn_uid。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotebookEntry {
    pub id: String,
    pub title: String,
    pub tags: Vec<String>,
    pub created_at_ms: i64,
}

/// `core_notebook_list`/`core_notebook_search` 的回执。`unlisted` 是「过滤后在场但没列出」
/// 的条数（搜索时 = 没进前 limit 的命中数）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotebookList {
    pub notes: Vec<NotebookEntry>,
    pub unlisted: usize,
}

/// `core_notebook_expand` 的回执：全文逐字节。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotebookFull {
    pub id: String,
    pub title: String,
    pub tags: Vec<String>,
    pub body: String,
    pub source_turn_uid: Option<String>,
    pub created_at_ms: i64,
}

/// 「还有 N 条未列出」的 rc-core 侧读数。G7 的单一算法住 `rc_agent::unlisted_count`
/// （rc-agent/src/lib.rs:56），但依赖方向是 rc-agent → rc-core，rc-core 不能反向依赖；
/// 这里是同一口径的镜像（candidates − listed − silent，账目矛盾就 debug_assert 响），
/// 调用点固定传 `(candidates, listed, 0)`——与 rc-agent 的 recall 查询臂同形。
/// 若日后算法下沉到 rc-contract，这里是第一个换回单一真相的调用点。
pub fn notebook_unlisted(candidates: usize, listed: usize, silent: usize) -> usize {
    debug_assert!(
        listed.saturating_add(silent) <= candidates,
        "unlisted accounting contradicts itself: candidates {candidates} listed {listed} silent {silent}"
    );
    candidates.saturating_sub(listed).saturating_sub(silent)
}

impl CoreStore {
    /// 追加一条小本本记录。校验只有形状：body 非空且按字符 ≤[`NOTEBOOK_BODY_MAX`]、
    /// tags ≤[`NOTEBOOK_TAGS_MAX`] 条每条非空且按字符 ≤[`NOTEBOOK_TAG_MAX`]——形状错的
    /// 整条不落（照 [`CoreStore::handoff_write`] 的校验风格）。
    ///
    /// `now_ms` 是**发话钟**（调用方传入，库不取系统时间，判据 12a）；id =
    /// `"n-" + sha256("notebook|{agent_id}|{now_ms}|{index}|{body}")[..12]`，逐字照
    /// `handoff_write` 第 4 步——`index` 取当前行数，同毫秒同文本的第二条不撞。
    /// 计数与插入在同一个 `BEGIN IMMEDIATE` 里，并发的第二个写者看到的是 +1 后的行数。
    #[allow(clippy::too_many_arguments)]
    pub fn notebook_append(
        &self,
        agent_id: &str,
        title: Option<&str>,
        tags: &[String],
        body: &str,
        source_turn_uid: Option<&str>,
        device_id: &str,
        now_ms: i64,
    ) -> Result<NotebookAppend, CoreError> {
        let agent_id = checked_agent_id(agent_id)?;
        // 形状校验放在拿锁之前：错的条目不落，也不该占着写锁。
        if body.trim().is_empty() {
            return Err(CoreError::BadPayload("notebook body is empty".to_string()));
        }
        if body.chars().count() > NOTEBOOK_BODY_MAX {
            return Err(CoreError::BadPayload(format!(
                "notebook body is longer than {NOTEBOOK_BODY_MAX} chars"
            )));
        }
        if tags.len() > NOTEBOOK_TAGS_MAX {
            return Err(CoreError::BadPayload(format!(
                "notebook has more than {NOTEBOOK_TAGS_MAX} tags"
            )));
        }
        for tag in tags {
            if tag.trim().is_empty() {
                return Err(CoreError::BadPayload("notebook tag is empty".to_string()));
            }
            if tag.chars().count() > NOTEBOOK_TAG_MAX {
                return Err(CoreError::BadPayload(format!(
                    "notebook tag is longer than {NOTEBOOK_TAG_MAX} chars"
                )));
            }
        }
        let note_table = self.note_table_for(agent_id)?;
        let title = title.unwrap_or("").trim().to_string();
        let row = NotebookRow {
            title,
            tags: tags.to_vec(),
            body: body.to_string(),
            source_turn_uid: source_turn_uid.map(str::to_string),
        };
        let tx = Transaction::new_unchecked(self.conn(), TransactionBehavior::Immediate)?;
        // 4. 生成 id：`index` 是当前行数，同毫秒同文本的第二条不撞。
        let index: i64 =
            tx.query_row(&format!("SELECT COUNT(*) FROM \"{note_table}\""), [], |r| r.get(0))?;
        let id = format!(
            "n-{}",
            &sha256_hex_of(format!("notebook|{agent_id}|{now_ms}|{index}|{body}").as_bytes())[..12]
        );
        let plain = serde_json::to_vec(&row).map_err(|e| CoreError::BadPayload(e.to_string()))?;
        let aad_bytes = aad_notebook(agent_id, &id);
        let key = load_key(self.data_dir(), agent_id)?;
        let (nonce, ciphertext) = seal(&key, &aad_bytes, &plain)?;
        tx.execute(
            &format!(
                "INSERT INTO \"{note_table}\"(id, created_at_ms, device_id, nonce, ciphertext) \
                 VALUES (?1, ?2, ?3, ?4, ?5)"
            ),
            params![id, now_ms, device_id, nonce.as_slice(), ciphertext],
        )?;
        tx.commit()?;
        Ok(NotebookAppend { id, created_at_ms: now_ms })
    }

    /// 列出小本本，**新的在前**，`limit` 缺省 [`NOTEBOOK_LIST_LIMIT_DEFAULT`]、
    /// 封顶 [`NOTEBOOK_LIST_LIMIT_MAX`]（0 视为缺省）。`tag` 给了就只留带这个标签的行
    /// （标签在密文里，过滤只能解密后做）。回执**零正文字段**。
    /// **纯读**：不落版本、不写访问日志（那是 mcp 层 `call_core` 的事）。
    pub fn notebook_list(
        &self,
        agent_id: &str,
        limit: Option<usize>,
        tag: Option<&str>,
    ) -> Result<NotebookList, CoreError> {
        let limit = notebook_limit(limit, NOTEBOOK_LIST_LIMIT_DEFAULT);
        let rows = self.notebook_rows(agent_id)?;
        let rows = match tag {
            None => rows,
            Some(tag) => rows
                .into_iter()
                .filter(|(_, _, row)| row.tags.iter().any(|t| t == tag))
                .collect::<Vec<_>>(),
        };
        let candidates = rows.len();
        let listed = rows
            .into_iter()
            .take(candidates.min(limit))
            .map(|(id, created_at_ms, row)| NotebookEntry {
                id,
                title: row.title,
                tags: row.tags,
                created_at_ms,
            })
            .collect::<Vec<_>>();
        let unlisted = notebook_unlisted(candidates, listed.len(), 0);
        Ok(NotebookList { notes: listed, unlisted })
    }

    /// 检索小本本：解密该 agent 全量行后做子串匹配，**多词是「或」**（同 recall 检索纪律；
    /// 不上 FTS5 是已拍板裁定——给密文上 FTS5 需要明文列或盲索引，破「core.db 无明文」）。
    /// 匹配面是 title/tags/body；个人规模线性扫描够用。空命中 = 合法空读数，不报错。
    pub fn notebook_search(
        &self,
        agent_id: &str,
        query: &str,
        limit: Option<usize>,
    ) -> Result<NotebookList, CoreError> {
        if query.trim().is_empty() {
            return Err(CoreError::BadPayload("notebook query is empty".to_string()));
        }
        let limit = notebook_limit(limit, NOTEBOOK_SEARCH_LIMIT_DEFAULT);
        let words: Vec<&str> = query.split_whitespace().collect();
        let rows = self
            .notebook_rows(agent_id)?
            .into_iter()
            .filter(|(_, _, row)| {
                words.iter().any(|w| {
                    row.title.contains(w)
                        || row.body.contains(w)
                        || row.tags.iter().any(|t| t.contains(w))
                })
            })
            .collect::<Vec<_>>();
        let candidates = rows.len();
        let listed = rows
            .into_iter()
            .take(candidates.min(limit))
            .map(|(id, created_at_ms, row)| NotebookEntry {
                id,
                title: row.title,
                tags: row.tags,
                created_at_ms,
            })
            .collect::<Vec<_>>();
        let unlisted = notebook_unlisted(candidates, listed.len(), 0);
        Ok(NotebookList { notes: listed, unlisted })
    }

    /// 取一条的全文，逐字节回原文。id 指不到 ⇒ `CoreError::NotFound`——句柄形状没问题
    /// 只是没指到东西（mcp 层照 `core_episode_expand` 折成 empty）。
    pub fn notebook_expand(&self, agent_id: &str, id: &str) -> Result<NotebookFull, CoreError> {
        let agent_id = checked_agent_id(agent_id)?;
        let rows = self.notebook_rows(agent_id)?;
        let Some((_, created_at_ms, row)) = rows.into_iter().find(|(nid, _, _)| nid == id) else {
            return Err(CoreError::NotFound);
        };
        Ok(NotebookFull {
            id: id.to_string(),
            title: row.title,
            tags: row.tags,
            body: row.body,
            source_turn_uid: row.source_turn_uid,
            created_at_ms,
        })
    }

    /// brief 索引栏的读数：全量行、新的在前（[`crate::store::CoreStore::brief_with`] 与
    /// CLI 的 show/export 用；区上限/截断在 [`crate::render`] 的 `Section::push` 里，这里不裁）。
    /// **纯读**：不落版本、不写访问日志。
    pub fn notebook_index(&self, agent_id: &str) -> Result<Vec<NotebookEntry>, CoreError> {
        Ok(self
            .notebook_rows(agent_id)?
            .into_iter()
            .map(|(id, created_at_ms, row)| NotebookEntry {
                id,
                title: row.title,
                tags: row.tags,
                created_at_ms,
            })
            .collect())
    }

    /// 该 agent 的**全量**行解密，新的在前（`ORDER BY created_at_ms DESC, id DESC`，确定序）。
    /// 检索不上 FTS5（已拍板）：这里就是那条线性扫描的唯一实现。
    fn notebook_rows(
        &self,
        agent_id: &str,
    ) -> Result<Vec<(String, i64, NotebookRow)>, CoreError> {
        let agent_id = checked_agent_id(agent_id)?;
        let note_table = self.note_table_for(agent_id)?;
        let mut st = self.conn().prepare(&format!(
            "SELECT id, created_at_ms, nonce, ciphertext FROM \"{note_table}\" \
             ORDER BY created_at_ms DESC, id DESC"
        ))?;
        let raw = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let key = load_key(self.data_dir(), agent_id)?;
        let mut out = Vec::with_capacity(raw.len());
        for (id, created_at_ms, nonce, ciphertext) in raw {
            let nonce: [u8; NONCE_LEN] = nonce
                .as_slice()
                .try_into()
                .map_err(|_| CoreError::Crypto)?;
            let plain = open(&key, &aad_notebook(agent_id, &id), &nonce, &ciphertext)?;
            let row: NotebookRow =
                serde_json::from_slice(&plain).map_err(|e| CoreError::BadPayload(e.to_string()))?;
            out.push((id, created_at_ms, row));
        }
        Ok(out)
    }
}

/// list/search 共用的 limit 归一：缺省值、封顶 [`NOTEBOOK_LIST_LIMIT_MAX`]、0 视为缺省。
fn notebook_limit(limit: Option<usize>, default: usize) -> usize {
    match limit {
        None | Some(0) => default,
        Some(n) => n.min(NOTEBOOK_LIST_LIMIT_MAX),
    }
}

// ── 文档子串检索（CLI agent 面，2026-11-07 工单②）───────────────────────────
//
// 与小本本检索（[`CoreStore::notebook_search`]）同一条纪律：解密后线性子串匹配、
// 多词「或」、空命中 = 合法空读数；但检索面是**整份文档载荷**——小本本是独立的
// `core_note_<h>` 行，不在这把尺里，各有各的入口（`core notebook search`）。

/// 一次文档检索的命中：区名（render 的既有常量集）+ 上下文片段。
#[derive(Debug, Clone, PartialEq)]
pub struct DocSearchHit {
    pub section: &'static str,
    pub snippet: String,
}

/// 对解密后的整份载荷做子串检索（跨 section，多词「或」）。空查询 ⇒ `BadPayload`
/// （照 [`CoreStore::notebook_search`]）；空命中 = 合法空读数，不是错误。
/// chinese_expression 段只有开关、没有自由文本（固定规则在代码里），不入检索面。
pub fn doc_search(payload: &CorePayload, query: &str) -> Result<Vec<DocSearchHit>, CoreError> {
    if query.trim().is_empty() {
        return Err(CoreError::BadPayload("doc search query is empty".to_string()));
    }
    let words: Vec<&str> = query.split_whitespace().collect();
    let mut lines: Vec<(&'static str, String)> = Vec::new();
    for item in &payload.l0 {
        push_line(&mut lines, crate::render::SEC_L0, item);
    }
    let persona = &payload.persona;
    push_line(&mut lines, crate::render::SEC_PERSONA, &persona.core_text);
    for (k, v) in &persona.traits {
        push_line(&mut lines, crate::render::SEC_PERSONA, &format!("{k}: {}", value_scalar(v)));
    }
    for (k, v) in &persona.style {
        push_line(&mut lines, crate::render::SEC_PERSONA, &format!("{k}: {}", value_scalar(v)));
    }
    for quirk in &persona.quirks {
        push_line(&mut lines, crate::render::SEC_PERSONA, quirk);
    }
    for boundary in &persona.boundaries {
        push_line(&mut lines, crate::render::SEC_PERSONA, boundary);
    }
    for example in &persona.voice_examples {
        push_line(&mut lines, crate::render::SEC_PERSONA, example);
    }
    // 自编辑提案的 reason/evidence 是 agent 自己写的自由文本，也属这份文档。
    for proposal in &payload.persona_proposals {
        push_line(&mut lines, crate::render::SEC_PERSONA, &proposal.reason);
        for evidence in &proposal.evidence {
            push_line(&mut lines, crate::render::SEC_PERSONA, evidence);
        }
    }
    for subject in &payload.subjects {
        push_line(&mut lines, crate::render::SEC_SUBJECT, &subject.subject_id);
        push_line(&mut lines, crate::render::SEC_SUBJECT, &subject.profile_text);
        for (k, v) in &subject.preferences {
            push_line(&mut lines, crate::render::SEC_SUBJECT, &format!("{k}: {}", value_scalar(v)));
        }
        for trigger in &subject.triggers {
            push_line(&mut lines, crate::render::SEC_SUBJECT, trigger);
        }
        for taboo in &subject.taboos {
            push_line(&mut lines, crate::render::SEC_SUBJECT, taboo);
        }
    }
    // 关系与交互同属 brief 的「关系与交互」区，检索面跟着同一把分区尺。
    push_line(&mut lines, crate::render::SEC_RELATIONSHIP, &payload.relationship.text);
    push_line(&mut lines, crate::render::SEC_RELATIONSHIP, &payload.relationship.status);
    push_line(&mut lines, crate::render::SEC_RELATIONSHIP, &payload.interaction.current_topic);
    for affect in &payload.affects {
        push_line(&mut lines, crate::render::SEC_AFFECT, &affect.label);
        push_line(&mut lines, crate::render::SEC_AFFECT, &affect.cause);
    }
    for item in &payload.loops {
        push_line(&mut lines, crate::render::SEC_LOOPS, &item.kind);
        push_line(&mut lines, crate::render::SEC_LOOPS, &item.text);
    }
    // 交接条全量（含已关闭的）：检索面是文档，不是 brief 的可见性窗口。
    for handoff in &payload.handoffs {
        push_line(&mut lines, crate::render::SEC_HANDOFF, &handoff.text);
    }
    for pinned in &payload.pinned_episodes {
        push_line(&mut lines, crate::render::SEC_PINNED, &pinned.episode_id);
        push_line(&mut lines, crate::render::SEC_PINNED, &pinned.pin_reason);
    }
    Ok(lines
        .into_iter()
        .filter(|(_, line)| words.iter().any(|w| line.contains(w)))
        .map(|(section, line)| DocSearchHit { section, snippet: snippet(&line, &words) })
        .collect())
}

/// 一行检索面文本：非空才收（空白行不产生假命中）。
fn push_line(lines: &mut Vec<(&'static str, String)>, section: &'static str, text: &str) {
    let text = text.trim();
    if !text.is_empty() {
        lines.push((section, text.to_string()));
    }
}

/// JSON 值的检索面：字符串原样，其它形状走紧凑 JSON 文本（不猜业务语义）。
fn value_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// 长行的上下文片段：以最早命中处为中心取窗；短行原样。
fn snippet(line: &str, words: &[&str]) -> String {
    const WINDOW: usize = 120;
    let total = line.chars().count();
    if total <= WINDOW {
        return line.to_string();
    }
    let hit_byte = words.iter().filter_map(|w| line.find(w)).min().unwrap_or(0);
    let hit_char = line[..hit_byte].chars().count();
    let start = hit_char.saturating_sub(WINDOW / 4);
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(line.chars().skip(start).take(WINDOW));
    if start + WINDOW < total {
        out.push('…');
    }
    out
}
