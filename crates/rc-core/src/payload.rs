//! 核心层逻辑载荷（设计 §9.2）。
//!
//! 这是密文里的 JSON，不是物理表：表形状固定，演进只发生在 payload 内。
//! 所有集合字段都 `#[serde(default)]`，因此旧 payload 缺段也能读；未知字段不拒绝
//! （同向演进只加不减）。

use crate::ops::PersonaProposal;
use crate::CoreError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const SCHEMA_VERSION: u32 = 1;

/// 自由形状的键值段（`persona.traits`/`style`、`subject.preferences`）。
/// 值留给渲染层解释：这里只负责不丢东西。
pub type JsonMap = BTreeMap<String, serde_json::Value>;

/// 「中文表达优化」的固定规则。规则随代码走，不进载荷：agent 只切换开关，
/// 不需要自己再写一遍；改规则是一次产品变更，不是一次 agent 自编辑。
/// 第 3 条单独点“就”的用法：该用要用，不该用不要堆。
pub const CHINESE_EXPRESSION_RULES: [&str; 6] = [
    "用中文回复人类时，写完整的中文句子：保留主语、介词、连词与语气助词（的、了、着、在、因为、所以、但是、已经、仍然、目前）。",
    "宁可多写几个字，也不要把一句话压成关键词、标签或电报体；不要用“已处理”“见上”“OK”这类压缩回复代替解释。",
    "该用“就”的地方要写出来：表示前后承接、条件结果、时间紧接、范围限定或强调时，按中文习惯使用“就”（例如“看完就回”“到了就发消息”“就这些”）；不要为了压缩把它删掉，也不要在不需要强调的地方滥用。",
    "尽量在短句里也把主语写出来：不要因为句子短就省略“我”“你”“他”“她”或具体的人、物，避免让人猜是谁做了什么。",
    "用日常对话的说法，不要用说明书式的书面语言；把“用户应当”“本功能用于”“上述内容”这类写法改成对人说话的自然表达。",
    "生成代码、命令、JSON 键、标识符与路径时，建议用英文和英文符号；如果环境里的路径本来就是中文，就按实际情况写，不要为了英文去改路径。中文主要放在面向人类的说明与文案里。",
];

/// 每个 agent 的中文表达优化开关。`enabled=false` 是缺省；`version` 每次开关自增，
/// `updated_at_ms` 便于审计“什么时候打开的”。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChineseExpression {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CorePayload {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub agent_id: String,
    #[serde(default)]
    pub l0: Vec<String>,
    #[serde(default)]
    pub persona: Persona,
    /// AI 自编辑提案（§9.4）：只追加，按 `proposed → applied|rejected` 流转。
    /// patch 里没有 L0，所以提案永远碰不到身份底线。
    #[serde(default)]
    pub persona_proposals: Vec<PersonaProposal>,
    #[serde(default)]
    pub subjects: Vec<Subject>,
    #[serde(default)]
    pub interaction: Interaction,
    #[serde(default)]
    pub relationship: Relationship,
    #[serde(default)]
    pub affects: Vec<Affect>,
    #[serde(default)]
    pub loops: Vec<LoopItem>,
    /// 交接条（2026-10-28）：给下一个会话的便签，一次性的。**不进抽取、不进召回、
    /// 不进快照**（`core.db` 本来就不在 `DATA_FILES` 里）。旧载荷缺这一段 ⇒ 空。
    #[serde(default)]
    pub handoffs: Vec<HandoffItem>,
    #[serde(default)]
    pub pinned_episodes: Vec<PinnedEpisode>,
    /// 中文表达优化开关（2026-10-25）。旧载荷缺这一段 ⇒ 默认关闭。
    #[serde(default)]
    pub chinese_expression: ChineseExpression,
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

/// L1 人格核心（慢变，版本化）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Persona {
    #[serde(default)]
    pub core_text: String,
    #[serde(default)]
    pub traits: JsonMap,
    #[serde(default)]
    pub style: JsonMap,
    #[serde(default)]
    pub quirks: Vec<String>,
    #[serde(default)]
    pub boundaries: Vec<String>,
    #[serde(default)]
    pub voice_examples: Vec<String>,
}

/// 主用户画像与核心关系（主用户 1 个，最多再加 3 个核心关系）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Subject {
    #[serde(default)]
    pub subject_id: String,
    #[serde(default)]
    pub is_primary: bool,
    #[serde(default)]
    pub profile_text: String,
    #[serde(default)]
    pub preferences: JsonMap,
    #[serde(default)]
    pub triggers: Vec<String>,
    #[serde(default)]
    pub taboos: Vec<String>,
}

/// 关系 + 交互状态（每次更新）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Interaction {
    #[serde(default)]
    pub last_user_message_at_ms: i64,
    #[serde(default)]
    pub last_agent_initiated_at_ms: i64,
    #[serde(default)]
    pub unanswered_user_messages: u32,
    #[serde(default)]
    pub attention_debt_ms: i64,
    /// 当前话题。旧载荷没有这一段，缺省为空串（渲染时省略，不编造话题）。
    #[serde(default)]
    pub current_topic: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Relationship {
    #[serde(default = "default_half")]
    pub trust: f64,
    #[serde(default = "default_half")]
    pub intimacy: f64,
    #[serde(default)]
    pub tension: f64,
    #[serde(default = "default_status")]
    pub status: String,
    #[serde(default)]
    pub text: String,
    #[serde(default = "default_confidence")]
    pub confidence: f64,
}

fn default_half() -> f64 {
    0.5
}

fn default_confidence() -> f64 {
    0.6
}

fn default_status() -> String {
    "normal".to_string()
}

impl Default for Relationship {
    fn default() -> Self {
        Relationship {
            trust: default_half(),
            intimacy: default_half(),
            tension: 0.0,
            status: default_status(),
            text: String::new(),
            confidence: default_confidence(),
        }
    }
}

/// 用户/agent 各一条当前情绪（低置信、可衰减）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Affect {
    #[serde(default)]
    pub subject_kind: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub valence: f64,
    #[serde(default)]
    pub arousal: f64,
    /// 精力（arousal 是唤醒度，energy 是还剩多少力气）。旧载荷缺省 0。
    #[serde(default)]
    pub energy: f64,
    #[serde(default = "default_confidence")]
    pub confidence: f64,
    #[serde(default)]
    pub valid_from_ms: i64,
    /// 情绪原因。推断出来的原因由调用方标注，渲染只照抄。
    #[serde(default)]
    pub cause: String,
}

/// 未闭环事项（承诺、冲突、待聊话题）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LoopItem {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub due_at_ms: i64,
}

/// pinned 共同经历：只存指针与摘要理由，正文在普通记忆里。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PinnedEpisode {
    #[serde(default)]
    pub episode_id: String,
    #[serde(default)]
    pub pin_reason: String,
}

/// 交接条的四种用途（顺序即 `handoff_list` 的排序键）：
/// `continue` 接着做、`due` 到期要交付、`reminder` 定点提醒、`note` 纯备注。
pub const HANDOFF_KINDS: [&str; 4] = ["continue", "due", "reminder", "note"];
/// 关闭理由的闭集。`superseded` 只由「新 continue 取代旧条」写。
pub const HANDOFF_CLOSE_REASONS: [&str; 3] = ["done", "superseded", "manual"];
/// 每 agent 未关闭条数上限：满了就拒写，不静默丢。
pub const HANDOFF_OPEN_LIMIT: usize = 50;
/// 载荷里保留的总行数上限（写入时按 `closed_at_ms` 从旧到新删已关闭行）。
pub const HANDOFF_TOTAL_LIMIT: usize = 200;
/// 正文上限，**按字符不按字节**（200 个汉字 = 600 字节）。
pub const HANDOFF_TEXT_MAX: usize = 200;
/// 缺省有效期：7 天。
pub const HANDOFF_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// 一张交接条。它**不是**长期任务（那是 `LoopItem`）：到期即失效、关闭即消失，
/// 且只在写入时被物理清理。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HandoffItem {
    /// `"h-"` + 12 hex。
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub created_at_ms: i64,
    #[serde(default)]
    pub expires_at_ms: i64,
    /// 注入时自增（H2 才写这个数；H1 只建字段）。
    #[serde(default)]
    pub seen_count: u32,
    #[serde(default)]
    pub last_seen_at_ms: Option<i64>,
    #[serde(default)]
    pub closed_at_ms: Option<i64>,
    #[serde(default)]
    pub closed_reason: Option<String>,
}

impl Default for CorePayload {
    fn default() -> Self {
        CorePayload {
            schema_version: SCHEMA_VERSION,
            agent_id: String::new(),
            l0: Vec::new(),
            persona: Persona::default(),
            persona_proposals: Vec::new(),
            subjects: Vec::new(),
            interaction: Interaction::default(),
            relationship: Relationship::default(),
            affects: Vec::new(),
            loops: Vec::new(),
            handoffs: Vec::new(),
            pinned_episodes: Vec::new(),
            chinese_expression: ChineseExpression::default(),
        }
    }
}

impl CorePayload {
    /// 空载荷：注册后第一次写入的起点。
    pub fn empty(agent_id: &str) -> Self {
        CorePayload { agent_id: agent_id.to_owned(), ..CorePayload::default() }
    }

    /// 结构自检。只查"写进去就会让读侧含混"的四件事，不猜业务语义。
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.agent_id.trim().is_empty() {
            return Err(CoreError::BadPayload("agent_id is empty".to_string()));
        }
        let mut seen = BTreeSet::new();
        for item in &self.loops {
            if !seen.insert(item.id.as_str()) {
                return Err(CoreError::BadPayload(format!(
                    "duplicate loop id: {}",
                    item.id
                )));
            }
        }
        let mut seen_proposals = BTreeSet::new();
        for proposal in &self.persona_proposals {
            if proposal.id.trim().is_empty() {
                return Err(CoreError::BadPayload(
                    "persona proposal id is empty".to_string(),
                ));
            }
            if !seen_proposals.insert(proposal.id.as_str()) {
                return Err(CoreError::BadPayload(format!(
                    "duplicate persona proposal id: {}",
                    proposal.id
                )));
            }
            if !matches!(proposal.status.as_str(), "proposed" | "applied" | "rejected") {
                return Err(CoreError::BadPayload(format!(
                    "persona proposal status must be proposed|applied|rejected, got {:?}",
                    proposal.status
                )));
            }
        }
        for affect in &self.affects {
            if !matches!(affect.subject_kind.as_str(), "user" | "agent") {
                return Err(CoreError::BadPayload(format!(
                    "affect subject_kind must be user or agent, got {:?}",
                    affect.subject_kind
                )));
            }
        }
        for pinned in &self.pinned_episodes {
            if pinned.episode_id.trim().is_empty() {
                return Err(CoreError::BadPayload(
                    "pinned episode_id is empty".to_string(),
                ));
            }
        }
        // 交接条：id 唯一、kind 闭集、正文按字符计、有效期晚于创建时刻，
        // 以及「关闭时刻」与「关闭理由」必须同进同退。写路径的闸只有这一处，
        // 所以 `mutate_doc → save_doc → validate` 之外没有任何第二道。
        let mut seen_handoffs = BTreeSet::new();
        for handoff in &self.handoffs {
            if handoff.id.trim().is_empty() {
                return Err(CoreError::BadPayload("handoff id is empty".to_string()));
            }
            if !seen_handoffs.insert(handoff.id.as_str()) {
                return Err(CoreError::BadPayload(format!(
                    "duplicate handoff id: {}",
                    handoff.id
                )));
            }
            if !HANDOFF_KINDS.contains(&handoff.kind.as_str()) {
                return Err(CoreError::BadPayload(format!(
                    "handoff kind must be continue|due|reminder|note, got {:?}",
                    handoff.kind
                )));
            }
            if handoff.text.trim().is_empty() {
                return Err(CoreError::BadPayload("handoff text is empty".to_string()));
            }
            if handoff.text.chars().count() > HANDOFF_TEXT_MAX {
                return Err(CoreError::BadPayload(format!(
                    "handoff text is longer than {HANDOFF_TEXT_MAX} chars"
                )));
            }
            if handoff.expires_at_ms <= handoff.created_at_ms {
                return Err(CoreError::BadPayload(
                    "handoff expires_at_ms must be later than created_at_ms".to_string(),
                ));
            }
            match (&handoff.closed_at_ms, &handoff.closed_reason) {
                (None, None) => {}
                (Some(_), Some(reason)) if HANDOFF_CLOSE_REASONS.contains(&reason.as_str()) => {}
                _ => {
                    return Err(CoreError::BadPayload(
                        "handoff closed_at_ms and closed_reason must be set together, \
                         reason done|superseded|manual"
                            .to_string(),
                    ))
                }
            }
        }
        Ok(())
    }
}
