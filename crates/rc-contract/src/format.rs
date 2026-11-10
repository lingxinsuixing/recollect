use serde::{Deserialize, Serialize};

pub const SUPPORTED_FORMAT_VERSIONS: &[u32] = &[1];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnTier {
    IdBearing,
    StableSeq,
    ContentOnly,
}

impl TurnTier {
    pub fn rank(self) -> u8 {
        match self {
            TurnTier::IdBearing => 3,
            TurnTier::StableSeq => 2,
            TurnTier::ContentOnly => 1,
        }
    }

    /// `rank()` 的**逆映射**。节点上行时要把本机转录层里存着的 `turn_key_tier`
    /// （1/2/3）还原成契约的 tier 形状。
    ///
    /// 还原不了就是**还原不了**：返回 `None`，由调用方响亮失败。
    /// 悄悄折成 `ContentOnly` 会让一条 `IdBearing` 的轮丢掉它的 id 身份，
    /// 中心于是拿另一条规则重算 `turn_uid` —— 症状是"同一条话在中心成了另一条"。
    pub fn from_rank(rank: u8) -> Option<TurnTier> {
        match rank {
            3 => Some(TurnTier::IdBearing),
            2 => Some(TurnTier::StableSeq),
            1 => Some(TurnTier::ContentOnly),
            _ => None,
        }
    }
}

/// C1 输入侧：`deny_unknown_fields` 是「未知字段在输入侧拒收」的落地
/// （计划 Task 2 Step 5；GOALS §5.11 的「忽略未知字段」只管我们往外给的东西）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawTurn {
    pub turn_uid: Option<String>,
    pub seq: Option<i64>,
    pub tier: Option<TurnTier>,
    pub role: String,
    pub body: String,
    pub created_at_ms: Option<i64>,
    /// 轮级说话人（D 子 agent 接线）：`Some` = 这一轮不是会话本 agent 说的，而是这个
    /// `kind='agent'` 锚说的（canonical 就是这串，例如 `subagent:<call_id>`）。
    /// `None`/空串 = 今天的语义：说话人由证据轮的 `role` 推导（C1）。
    ///
    /// additive：缺省不报错、不改 `format_version`；旧生产者不发这一格。
    #[serde(default)]
    pub speaker: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawSession {
    pub session_key: String,
    pub device_id: String,
    pub source_agent: String,
    pub machine: Option<String>,
    pub project_hint: Option<String>,
    pub last_saved_at_ms: Option<i64>,
    /// 身份层 I1（设计 `2026-10-27-principal-identity.md` §5）：Agent 侧透传的主人身份三格。
    /// 三格全部可选、**缺省不报错**（`format_version` 仍是 1：这是 additive 扩展，
    /// 旧生产者不发这三格，新生产者发了也只能被这一层原样转交，不做任何推断/合并）。
    #[serde(default)]
    pub owner_principal_id: Option<String>,
    /// 可读名（昵称/称呼）。**不是主键**，不参与任何身份合并。
    #[serde(default)]
    pub principal_name: Option<String>,
    /// 章的来源，闭集见 `validate::PRINCIPAL_SOURCES`；缺省由导入侧取 `agent_runtime`。
    #[serde(default)]
    pub principal_source: Option<String>,
    pub turns: Vec<RawTurn>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExchangeFile {
    pub format_version: u32,
    pub sessions: Vec<RawSession>,
}
