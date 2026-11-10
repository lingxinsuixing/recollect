//! provider preset 表（A47 节 2）：**数据，不是代码**。换 provider 改的是 `llm.toml`，
//! 改这张表要发版（节 1 那条接缝）。
//!
//! 入表判据只有一条：某个形状项在厂家之间**真的取不同值**才进表；对所有人恒真的
//! （逐帧丢弃未知键、顶层未知字段静默容忍、回显比对）降级为下面的实现律 + 一条判据测试。
//!
//! 结构分三层，因为 custom 通道（节 1 接缝②）要的是其中一层：
//! - [`Preset`] = **形状**（节 2 那十格），可以是 `const`，四家各一条 + custom 两条；
//! - [`Row`] = 形状 + **表上的地址**（第 2、3 两格逐字），只有四家加转售那一行；
//! - [`Target`] = **运行期的地址**（`String`）：表上的行从 `Row` 抄，custom 通道从 `llm.toml` 读。
//!   `&'static str` 装不下运行期读出来的 URL，所以这一格必须是独立类型而不是 `Preset` 的字段。

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    OpenAiChat,
    AnthropicMessages,
}

impl Protocol {
    /// 实现律①：鉴权头与版本头**属协议不属 provider**（Agnes 同一条 base 下两套协议并存，
    /// 一边 `Authorization: Bearer`、一边 `x-api-key` + `anthropic-version`）。
    pub fn auth_headers(self, key: &str) -> Vec<(String, String)> {
        match self {
            Protocol::OpenAiChat => vec![("authorization".to_string(), format!("Bearer {}", key))],
            Protocol::AnthropicMessages => vec![
                ("x-api-key".to_string(), key.to_string()),
                ("anthropic-version".to_string(), ANTHROPIC_VERSION.to_string()),
            ],
        }
    }

    /// 实现律②：endpoint 路径同属协议。**DeepSeek 的 Anthropic 路由不带 `/v1`**，
    /// 所以这一格不能是 `format!("{}/messages", base_url)`——base_url 那一格已经把差异吃进去了。
    pub fn path(self) -> &'static str {
        match self {
            Protocol::OpenAiChat => "/chat/completions",
            Protocol::AnthropicMessages => "/messages",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Protocol::OpenAiChat => "openai-chat",
            Protocol::AnthropicMessages => "anthropic-messages",
        }
    }

    /// 协议名从 `llm.toml` 的字符串读进来（custom 通道那一侧）：拼错必须响亮失败，
    /// 不许回落默认（节 1 那条接缝）。与 `rc-llm-cfg::keys::PROTOCOL_NAMES` 的同源性
    /// 由 `hub` 那条对表用例执法（`hub` 是唯一同时依赖两边的 target）。
    pub fn from_slug(s: &str) -> Option<Protocol> {
        match s {
            "openai-chat" => Some(Protocol::OpenAiChat),
            "anthropic-messages" => Some(Protocol::AnthropicMessages),
            _ => None,
        }
    }

    /// 实现律⑤：**预算吃满**那一格的 `finish` 取值属协议不属 provider。
    /// 它不进 preset 表（节 2 的入表判据：同一协议内恒同值），但判据 1 必须读它——
    /// 硬编码 `"length"` 会让 Anthropic 形的截断永远走不到"抬预算"那一支
    /// （Anthropic 的对应值是 `max_tokens`，`length` 在那一侧根本不是一个 `stop_reason`）。
    pub fn truncated_finish(self) -> &'static str {
        match self {
            Protocol::OpenAiChat => "length",
            Protocol::AnthropicMessages => "max_tokens",
        }
    }

    /// 实现律⑥：工具通道同律（`tool_calls` vs `tool_use`）。
    pub fn tool_finish(self) -> &'static str {
        match self {
            Protocol::OpenAiChat => "tool_calls",
            Protocol::AnthropicMessages => "tool_use",
        }
    }
}

pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// 节 2 第 4 格 `param_slots` 的第一批落点：同一个语义（输出预算）在不同协议里是不同字段名，
/// 且**同一家两个名字都认、有优先级、不报错**（Agnes 实测：`max_tokens: 16` +
/// `max_completion_tokens: 4000` ⇒ 651 字，文档写的那个被静默压掉）。
/// 所以这一格记的是"这一行**只发哪一个**"，不是"支持哪些"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetField {
    MaxTokens,
    MaxCompletionTokens,
}

impl BudgetField {
    pub fn name(self) -> &'static str {
        match self {
            BudgetField::MaxTokens => "max_tokens",
            BudgetField::MaxCompletionTokens => "max_completion_tokens",
        }
    }
}

/// 节 2 第 6 格：思考文本落在哪个键。`None` 与"键存在且为 null"是两件事（我犯过一次），所以是 `Option`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningKey {
    ReasoningContent,
    Reasoning,
    Thinking,
}

impl ReasoningKey {
    pub fn name(self) -> &'static str {
        match self {
            ReasoningKey::ReasoningContent => "reasoning_content",
            ReasoningKey::Reasoning => "reasoning",
            ReasoningKey::Thinking => "thinking",
        }
    }
}

/// 节 2 第 8 格：带内业务成功的判据 = 一条对象路径 + 该路径下代表"成功"的整数值。
/// 只有一家干"HTTP 200 里塞业务失败"（MiniMax `base_resp{status_code,status_msg}`），所以它是格不是律。
#[derive(Debug, Clone, Copy)]
pub struct InBandSuccess {
    pub table: &'static str,
    pub key: &'static str,
    pub ok: i64,
}

/// 第 14 格的取值。`PartialEq` 是给那条"三形都不 panic"的用例数命中用的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorBody {
    Json,
    Html,
    BareText,
}

/// **形状**：节 2 的十二格（协议 + 4/5/6/7/8/9/10/11/14/15/16；第 2、3 两格在 [`Row`] 上）。
#[derive(Debug, Clone, Copy)]
pub struct Preset {
    /// 1
    pub protocol: Protocol,
    /// 4
    pub budget_field: BudgetField,
    /// 5 Agnes 的文档自相矛盾 ⇒ 只能实测；默认通道不开思考（节 7.4）。
    /// `true` 的形状会被 `build_body` 拒绝出网，理由写在那一节。
    pub thinking_default: bool,
    /// 6
    pub reasoning_key: Option<ReasoningKey>,
    /// 7 观测白名单
    pub finish_success_set: &'static [&'static str],
    /// 8
    pub body_success_check: Option<InBandSuccess>,
    /// 9 语义 = "这一族的失败是**这家此刻**的状态，换一家可能成立"（含 401 凭据、402/403 额度与 429 RPM 墙）。
    /// 名单外 ⇒ 请求本身坏了 ⇒ 不前进。兑现点只有一处：`codec::classify`。
    pub retryable_status: &'static [u16],
    /// 10 带内 code 覆盖可重试性（Agnes 的 `model_not_found` ⇒ 不可重试）
    pub inband_code_overrides: &'static [&'static str],
    /// 14 观测到的错误体形状（`json` / `html` / `bare-text`）。**纯数据**：codec 的解析器
    /// 按形状通吃（上面那条三形用例执法），所以这一格没有任何分支读它——它存在的唯一理由
    /// 是把"Kimi 的 504 返 HTML、DeepSeek 缺 `Authorization` 返裸文本"钉在表里，
    /// 免得下一个人重新踩。入表判据（节 2）认它，因为三家的取值确实不同。
    pub error_body_kinds: &'static [ErrorBody],
    /// 11 `param_domain_overrides` 的第一批落点只有一个参数位（`temperature`）：
    /// SenseNova 转售的 `kimi-k3` 只许 1，发 0 得 400。
    pub temperature_locked: Option<f64>,
    /// 15 思考与正文**分居**那个开关的字段名（MiniMax 逐字 `reasoning_split`，值固定 `true`）。
    /// `None` ⇒ 一个额外字段都不发（实现律④：多发一个该协议没列的名字，正是"同义字段名静默压制"那一族）。
    /// 它挡的是实测形状：不发这个开关时 `message` 只有 `content` 一个键、思考混在正文里
    /// ⇒ 过不了"整条正文必须是且只是那一个 JSON 对象" ⇒ 判 `SchemaMismatch`、不前进。
    pub split_field: Option<&'static str>,
    /// 16 这一家在**生产实际发的那个字段名**下、实测被接受的输出预算上界。
    /// 只有 `thinking_default = true` 的行被读它：开思考 ⇒ 思考与正文共用同一预算，
    /// 实测形态是 `finish_reason=length` 且 `message` 键集里根本没有 `content`、正文 0 字。
    /// `None` ⇒ 该行拒绝出网（判据在 `codec::build_body`），因为它等于"没测过就上线"。
    pub budget_ceiling_verified: Option<u32>,
}

/// 九条共用码：401/403 凭据与授权、402 额度、408 超时、429 速率、5xx 网关。
/// **401 在这一族**是裁定而不是抄文档：用户 2026-10-15 深夜那句"当主 provider 不可用的时候，
/// 自动到列表下一个"里，一把被吊销的 key 就是"这家不可用"；不前进要连续撞三次才换家，
/// 还把这件事报成"回复形状不符"（运维看到的类别是错的）。
/// 逐家不同的一支是 Agnes 的 503：它有"上游忙"与"模型名不存在"两种，后者由
/// `inband_code_overrides` 在 classify 里先截走，所以这一族仍然可以共用一张名单。
/// 名单**外**的是请求本身坏了那一族（400/404/422）：换一家也一样，所以不许前进。
const RETRYABLE: [u16; 9] = [401, 402, 403, 408, 429, 500, 502, 503, 504];

/// 四条实测到的形状。千问／GLM／Kimi 三行**不在这一批**：本机无凭据，节 8 那条债写明它们只有
/// 文档轴、没有实测轴，`thinking_default` 与 `finish_success_set` 两格对它们是推测值 ⇒ 拿到 key 才进表。
/// 第 15/16 两格只有下面 `SHAPE_MINIMAX` 那一行取到值。其余三家取 `None` 今天**不构成任何拒绝**，
/// 因为它们同时取 `thinking_default = false`（那两个 `None` 只在开思考的行才被读）——
/// 这句话要写明，免得下一个人以为格子可以随手填、或者以为 `None` 已经在挡着它们。
const SHAPE_DEEPSEEK: Preset = Preset {
    protocol: Protocol::OpenAiChat,
    budget_field: BudgetField::MaxTokens,
    thinking_default: false,
    reasoning_key: Some(ReasoningKey::ReasoningContent),
    finish_success_set: &["stop", "length"],
    body_success_check: None,
    retryable_status: &RETRYABLE,
    inband_code_overrides: &[],
    error_body_kinds: &[ErrorBody::BareText, ErrorBody::Json],
    temperature_locked: None,
    split_field: None,
    budget_ceiling_verified: None,
};

const SHAPE_MINIMAX: Preset = Preset {
    protocol: Protocol::OpenAiChat,
    budget_field: BudgetField::MaxCompletionTokens,
    // M3 自发产出思考（台账 §10(34)：4_000 预算那一发思考 15,007–15,384 字符、正文 0 字）。
    // 这一格取 `true` 是**全表第一次** ⇒ `build_body` 里那条从写下就没落过地的拒绝第一次真的落下，
    // 所以第 15/16 两格必须同时到位，否则这个座位发不出包（台账 §10(35) 那条耦合）。
    thinking_default: true,
    reasoning_key: Some(ReasoningKey::ReasoningContent),
    // 不发这个开关时 `message` 只有 `content` 一个键、思考混在正文里（4 发对照，§10(34) 第一步）。
    split_field: Some("reasoning_split"),
    // 2026-10-16 的两臂（台账 §10(35)）：`max_completion_tokens: 32_000` ⇒ `stop` + 裸 JSON 正文；
    // 受控对照臂 `= 1` ⇒ `length` 且 `message` 只剩 `role`，证明这个字段名**被真读**不是静默忽略。
    // n=1，且这一格只证明**不被拒**、不证明**够用**——同一批里 16_000 那一发正文仍是 0 字。
    budget_ceiling_verified: Some(32_000),
    finish_success_set: &["stop", "length"],
    body_success_check: Some(InBandSuccess { table: "base_resp", key: "status_code", ok: 0 }),
    retryable_status: &RETRYABLE,
    // 1026/1027 = 涉敏拒答：同一句话换家也一样，重试与换家都不救 ⇒ 名单在此。
    inband_code_overrides: &["1026", "1027"],
    error_body_kinds: &[ErrorBody::Json],
    temperature_locked: None,
};

const SHAPE_SENSENOVA: Preset = Preset {
    protocol: Protocol::OpenAiChat,
    budget_field: BudgetField::MaxCompletionTokens,
    thinking_default: false,
    reasoning_key: Some(ReasoningKey::Reasoning),
    finish_success_set: &["stop", "length"],
    body_success_check: None,
    retryable_status: &RETRYABLE,
    inband_code_overrides: &[],
    error_body_kinds: &[ErrorBody::Json, ErrorBody::Html],
    temperature_locked: None,
    split_field: None,
    budget_ceiling_verified: None,
};

/// SenseNova 转售的 `kimi-k3` 只许 `temperature=1`（发 0 得 400）——同一家内按 model 变的合法域，
/// 所以它是表的一格而不是校验器里的常量。这一行与上面那行的区别只在这一个字段。
const SHAPE_SENSENOVA_KIMI: Preset = Preset { temperature_locked: Some(1.0), ..SHAPE_SENSENOVA };

const SHAPE_AGNES: Preset = Preset {
    protocol: Protocol::OpenAiChat,
    budget_field: BudgetField::MaxTokens,
    thinking_default: false,
    reasoning_key: Some(ReasoningKey::ReasoningContent),
    // `tool_calls` 单列分流：此时 `content` 是空串，把它当"库里没有"是假空。
    finish_success_set: &["stop", "length", "tool_calls"],
    body_success_check: None,
    retryable_status: &RETRYABLE,
    inband_code_overrides: &["model_not_found"],
    error_body_kinds: &[ErrorBody::Json],
    temperature_locked: None,
    split_field: None,
    budget_ceiling_verified: None,
};

/// 表的一行 = 一个可寻址的 `(provider, model_slug)` 键 + 它的形状。
#[derive(Debug, Clone, Copy)]
pub struct Row {
    /// 日志与 `doctor` 那一格的主语。
    pub provider: &'static str,
    /// 2（区域与账户位是一等维度：Kimi `.cn` 与 `.ai` 账户独立）
    pub base_url: &'static str,
    /// 3 逐字存、**不许归一**（MiniMax 的大小写混排小写化当场 404）
    pub model_slug: &'static str,
    pub shape: &'static Preset,
}

pub const ROWS: [Row; 5] = [
    Row { provider: "deepseek", base_url: "https://api.deepseek.com/v1", model_slug: "deepseek-chat", shape: &SHAPE_DEEPSEEK },
    Row {
        provider: "minimax",
        base_url: "https://api.minimax.cn/v1",
        model_slug: "MiniMax-M3",
        shape: &SHAPE_MINIMAX,
    },
    Row {
        provider: "sensenova",
        base_url: "https://token.sensenova.cn/v1",
        model_slug: "sensenova-6.8-flash-lite",
        shape: &SHAPE_SENSENOVA,
    },
    Row {
        provider: "sensenova",
        base_url: "https://token.sensenova.cn/v1",
        model_slug: "kimi-k3",
        shape: &SHAPE_SENSENOVA_KIMI,
    },
    Row { provider: "agnes", base_url: "https://api.agnes-ai.cn/v1", model_slug: "agnes-3.0-flash", shape: &SHAPE_AGNES },
];

/// custom 通道的两条形状（节 1 接缝②：**它不是表上的行**，地址来自 `llm.toml`）。
/// 五格是**文档轴推定**（没有实测轴，形状与节 8 那条债同类）：`thinking_default` 关、
/// `finish_success_set` 只放该协议那一对"成功 + 预算吃满"取值（openai 形 `stop`+`length`、
/// Anthropic 形 `end_turn`+`max_tokens`，见实现律⑤）、`error_body_kinds` 只放 `Json`，
/// 第 15/16 两格（`split_field`、`budget_ceiling_verified`）也推定 `None`——这一格推定错了的
/// 形状与上面那三条不同：`thinking_default` 推成关、而那一家其实自发思考 ⇒ 混进正文的思考过不了
/// 契约、判 `SchemaMismatch`（不前进、不动水位，仍然不是静默改写记忆）。按行补这两格的手在 A47-2。
/// 推定错了的后果由**此刻是中心的那一个 agent** 担：`llm.toml` 的写入权归中心（用户 2026-10-16
/// 裁定，替换掉 A43 原先那句"只给人"；非中心节点连可写的中心库都没有，见 `hub` 的 `qualify`），
/// 而错的推定落在 `SchemaMismatch` 那一族 ⇒ 不前进、不动水位，不会静默改写记忆质量。
const CUSTOM_SHAPES: [Preset; 2] = [
    Preset {
        protocol: Protocol::OpenAiChat,
        budget_field: BudgetField::MaxTokens,
        thinking_default: false,
        reasoning_key: None,
        finish_success_set: &["stop", "length"],
        body_success_check: None,
        retryable_status: &RETRYABLE,
        inband_code_overrides: &[],
        error_body_kinds: &[ErrorBody::Json],
        temperature_locked: None,
        split_field: None,
        budget_ceiling_verified: None,
    },
    Preset {
        // Anthropic 形：`max_tokens` 必填（第 4 格在协议这一侧只有一个取值），思考落在 `thinking` 块。
        protocol: Protocol::AnthropicMessages,
        budget_field: BudgetField::MaxTokens,
        thinking_default: false,
        reasoning_key: Some(ReasoningKey::Thinking),
        // `max_tokens` 必须在这一格里：它是 Anthropic 侧的"预算吃满"取值（实现律⑤），
        // 白名单不收它 ⇒ 判据 1 的白名单检查永远先把它打成 Malformed，抬预算那一支在这一形
        // 上成为死路。`tool_use` **不收**：工具通道这一批不接线，而它在 `locate` 里读出来是
        // 空文本 ⇒ 落在 `Malformed`，与 OpenAI 侧 `ToolChannel` 的上抛结果（`SchemaMismatch`）同族。
        finish_success_set: &["end_turn", "max_tokens"],
        body_success_check: None,
        retryable_status: &RETRYABLE,
        inband_code_overrides: &[],
        error_body_kinds: &[ErrorBody::Json],
        temperature_locked: None,
        split_field: None,
        budget_ceiling_verified: None,
    },
];

pub fn custom_shape(protocol: Protocol) -> &'static Preset {
    match protocol {
        Protocol::OpenAiChat => &CUSTOM_SHAPES[0],
        Protocol::AnthropicMessages => &CUSTOM_SHAPES[1],
    }
}

/// 键是 `(provider, model_slug)` **复合键**：同一个模型名跨家不是同一个东西
/// （SenseNova 的官方名单里就装着 `kimi-k3` 与 `deepseek-v4-flash`）。
/// 线性查而不是哈希表：`ROWS` 是 `const`，行数是五，而 `std::lazy` 那条路要额外一层
/// 初始化——查表在这套代码里没有可测的收益，别为它加状态。
pub fn find(provider: &str, model_slug: &str) -> Option<&'static Row> {
    ROWS.iter().find(|r| r.provider == provider && r.model_slug == model_slug)
}

/// 运行期的地址。`Target` 与 `Preset` 分开的理由只有一条：custom 通道的 URL 是读出来的，
/// 不是 `const` 里写出来的。
#[derive(Debug, Clone)]
pub struct Target {
    /// 日志与 `doctor` 的主语：表上的行给厂家名，custom 那一支固定给 `"custom"`。
    pub label: &'static str,
    pub base_url: String,
    /// 逐字来自表或配置（第 3 格那条"不许归一"对两条通道同样成立）。
    pub model_slug: String,
}

impl Row {
    pub fn target(&self) -> Target {
        Target {
            label: self.provider,
            base_url: self.base_url.to_string(),
            model_slug: self.model_slug.to_string(),
        }
    }
}

/// custom 那一侧的地址装配（`rc-llm-cfg` 交出 `Endpoint`，hub 在这里落地）。
pub fn custom_target(base_url: &str, model_slug: &str) -> Target {
    Target { label: "custom", base_url: base_url.to_string(), model_slug: model_slug.to_string() }
}

/// 实现律③（本地校验先于出网）：参数合法域检查。**一次请求都不许发出**——
/// 拼错参数与拼错模型名是同两类操作员错误，而 provider 侧那一类会白烧配额。
pub fn check_params(p: &Preset, temperature: Option<f64>) -> Result<(), &'static str> {
    if let (Some(lock), Some(t)) = (p.temperature_locked, temperature) {
        if t != lock {
            return Err("temperature outside the domain for this model");
        }
    }
    Ok(())
}

/// 带内业务状态：`Some(table)` 时那一格里 `key` 必须等于 `ok`。
/// 返回 `Some(code)` = 业务失败的原始 code（只进运维日志，进 agent 面前必过 redact）。
pub fn inband_failure(p: &Preset, doc: &Value) -> Option<String> {
    let chk = p.body_success_check?;
    let code = doc.get(chk.table)?.get(chk.key)?.as_i64()?;
    (code != chk.ok).then(|| code.to_string())
}

/// 带内 code 覆盖（`inband_code_overrides`）：命中名单 ⇒ 这一族**不许重试也不许换家**
/// （`model_not_found` 重试一万次还是 not found，换到别家这个名字照样不存在）。
pub fn code_is_overridden(p: &Preset, code: &str) -> bool {
    p.inband_code_overrides.contains(&code)
}
