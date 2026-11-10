//! live 后端：把 preset + codec + 出网点装配成一个 `LlmBackend`（A47 节 1 那一行的全部职责）。
//!
//! 这一层**不知道有几家、也不知道换家**：轮转住在 `rc-llm::Rotating`（零 IO），
//! 这里只回答"这一家、这一次调用"的结果。

use rc_llm::{LlmBackend, LlmFailure, Prompt};

pub mod codec;
pub mod preset;
pub mod redact;
pub mod transport;

use codec::{build_body, classify, Verdict};
use preset::{check_params, custom_shape, custom_target, find, Preset, Protocol, Target};

/// 一次 `complete()` 的真实发出上限。两条各有自愈手段的分支（抬预算、原样重发）各占一次机会，
/// 用完还不行就上交失败态。上限必须存在：没有它，一家"恒回 `length` + 空正文"的坏家会把
/// 一轮 cycle 变成无限循环，而账本按 `complete()` 计数、看不见里面发了几次。
pub const MAX_SENDS_PER_CALL: usize = 2;

/// 抬预算那一支的封顶（输出 token，**不是毫秒**——墙钟那一本住在 `rc_llm::JobFamily::budget_ms()`）。
/// 2026-10-16 用户裁定「我们只是要提取最终结果，不是要求省 token」，从 8_000 抬到 32_000。
/// 代价按实测的形状说话：`finish_reason=length` + 正文空 是"思考吃满预算"的确定性形态，
/// 天花板太低会把一次好答案读成截断、白抬一档再发一次。
/// **缺口已于 2026-10-17 用两拍探法补掉**（设计档节 8 那条债；逐座读数住在下面 `MEASURED_CEILINGS`，
/// 探法与四条限制见实施计划 §4.7）：四座各再发一发 32_000 **全部 200** ⇒ 本常量今天不会在任何一行撞 400。
/// 但**最小天花板是 65_536（SenseNova 那两行），只比 32_000 高一倍** ⇒
/// **抬本常量过 65_536 之前必须先换掉那两行或给 preset 加按座位分档**，否则撞的还是 400 那条不前进的路径
/// （`SchemaMismatch`，要连续三次同形才换家）。这一条由用例执法，不只写在这里。
pub const MAX_BUDGET: u32 = 32_000;

/// 2026-10-17 两拍探法**探到天花板**的座位（键 = preset 表第 3 格的 `model_slug` 逐字，值 = 对端错误体自己写的合法域上界）。
/// `MiniMax-M3` **不在这张表里**：它那一只只有"32_000 这一档被接受"的读数（2026-10-16 两臂），天花板本身没探过 ⇒ 别把它当"已知安全"。
/// 这张表唯一的读者是 `tests/codec.rs` 那两条用例：一条钉"`MAX_BUDGET` 不能越过实测最小天花板"，一条钉"表上的座位换了名，这里的读数就过期"。
pub const MEASURED_CEILINGS: [(&str, u32); 4] = [
    ("deepseek-chat", 393_216),
    ("sensenova-6.8-flash-lite", 65_536),
    ("kimi-k3", 1_048_576),
    ("agnes-3.0-flash", 65_536),
];

/// 传输层交回来的原始形状。`slow_ms` 由出网点用单调钟掐表（不是墙上钟），
/// 它进运维那一行（`ops_line`）与观测写口（[`Live::with_call_ms`]，W33-A10）：判据不读它
/// （503 那一族按带内码分流，形状见 `codec::classify` 里那条更正）。
#[derive(Debug, Clone)]
pub struct Raw {
    pub status: u16,
    pub body: String,
    pub slow_ms: i64,
}

/// 出网口的形状。测试用桩实现它（"发出计数 = 0"那一族判据要的就是可计数），
/// 生产用自由函数 `transport::agent(deadline_ms)` 给的 `ureq::Agent`（树里没有 `transport::Http` 这个类型）：它第一个真调用者是 `hub` 的 `build_live_seat`，每座一个，`deadline_ms` 取单发墙 [`transport::CALL_WALL_MS`]（hub W19c1 / node W19d 同源），不是 `rc_llm::cycle_budget_ms()`。
pub trait Egress {
    fn post(&self, url: &str, headers: &[(String, String)], body: &str) -> Result<Raw, LlmFailure>;
}

pub struct Live {
    target: Target,
    shape: &'static Preset,
    key: String,
    base_budget: u32,
    /// 推导作业的输出预算；缺省与 `base_budget` 同值，由 `llm.toml` 的 `derive_budget` 覆盖。
    derive_budget: u32,
    temperature: Option<f64>,
    /// 思考档位（`reasoning_effort`）；`None` 不发这一格。默认 `None`，由
    /// `llm.toml` 的座位在装配点用 [`Live::with_reasoning_effort`] 覆盖。
    reasoning_effort: Option<String>,
    egress: Box<dyn Egress>,
    /// 观测写口（W33-A10）：`Some` 时，每一次真实出网的耗时（transport 的单调钟 `slow_ms`）
    /// 都经 [`rc_llm::MaxCell::note`] 写进这一格（取 max）。`None`（缺省）= 观测面关着，
    /// 一次分配都没有。这里**只写不读**；读口与聚合在装配方（hub 的 `Metered`）那边。
    call_ms: Option<std::rc::Rc<rc_llm::MaxCell>>,
}

impl Live {
    /// preset 表那一支：构造点做两件**先于出网**的校验——查不到键 ⇒ 响亮失败（不许回落默认，
    /// 节 1 那条接缝）；参数落在合法域外 ⇒ 同样在构造点拒（实现律③，那条判据要求"一次请求都不许发出"）。
    ///
    /// `key` 的值只在这里经过一次：存进字段、拼进鉴权头，不落日志、不进 `LlmFailure`、不进 agent 面。
    pub fn new(
        provider: &str,
        model_slug: &str,
        key: String,
        base_budget: u32,
        temperature: Option<f64>,
        egress: Box<dyn Egress>,
    ) -> Result<Live, &'static str> {
        let row = find(provider, model_slug).ok_or("provider/model not in the preset table")?;
        check_params(row.shape, temperature)?;
        Ok(Live { target: row.target(), shape: row.shape, key, base_budget,
                  derive_budget: base_budget, temperature, reasoning_effort: None, egress,
                  call_ms: None })
    }

    /// custom 通道那一支（节 1 接缝②）：地址来自 `llm.toml`，形状来自 `custom_shape`。
    /// 协议名拼错同样在构造点拒——它不许回落 `openai-chat`，因为回落出来的请求会打到一条
    /// 没人要它打的路上。
    pub fn new_custom(
        base_url: &str,
        protocol_slug: &str,
        model_slug: &str,
        key: String,
        base_budget: u32,
        temperature: Option<f64>,
        egress: Box<dyn Egress>,
    ) -> Result<Live, &'static str> {
        let protocol = Protocol::from_slug(protocol_slug).ok_or("unknown protocol name")?;
        let shape = custom_shape(protocol);
        check_params(shape, temperature)?;
        Ok(Live {
            target: custom_target(base_url, model_slug),
            shape,
            key,
            base_budget,
            derive_budget: base_budget,
            temperature,
            reasoning_effort: None,
            egress,
            call_ms: None,
        })
    }

    /// 这一格是日志与 `doctor` 的主语：表上的行是厂家名，custom 那一支是 `custom`。
    pub fn label(&self) -> &'static str {
        self.target.label
    }

    /// 这一座的起始输出预算（token）。只读；装配测试用它证明 `llm.toml` 的覆盖值真的传到了出网口。
    pub fn base_budget(&self) -> u32 {
        self.base_budget
    }

    /// 推导作业的输出预算。只读；装配测试用它证明 `llm.toml` 的 `derive_budget` 真的传到了出网口。
    pub fn derive_budget(&self) -> u32 {
        self.derive_budget
    }

    /// 给推导作业设置独立预算；构造点保持原签名，避免所有调用方跟着加参数。
    pub fn with_derive_budget(mut self, budget: u32) -> Live {
        self.derive_budget = budget;
        self
    }

    /// 设置思考档位；构造点保持原签名（同上）。`None` 维持"不发这一格"。
    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Live {
        self.reasoning_effort = effort;
        self
    }

    /// 观测写口（W33-A10）：把"每一次真实出网的耗时"经 [`rc_llm::MaxCell::note`]
    /// 写进调用方递进来的这一格（取 max 是格自己的语义，不靠这里的自律）。
    /// 装配方（hub 的 `Metered`）造格、递进来、之后从同一格读——`Live` 自己不持读数。
    /// 不调它 = 观测面关着，与改造前的形状逐字节等价。
    pub fn with_call_ms(mut self, cell: std::rc::Rc<rc_llm::MaxCell>) -> Live {
        self.call_ms = Some(cell);
        self
    }

    /// 每次真实出网的唯一写口：transport 给了 `slow_ms` 就用它（transport.rs:60 的单调钟），
    /// 出网在拿回 `Raw` 之前就失败（超时/连接断）的那一支用**同一族单调钟**就地补测——
    /// 失败那一发（300s 被切的那一发）恰恰是观测面最要看见的一发，不能因为它没有
    /// `Raw` 就从读数里消失。两支都不是墙上钟，都没有估算。
    fn note_call_ms(&self, started: std::time::Instant, real_ms: Option<i64>) {
        if let Some(cell) = &self.call_ms {
            let ms = real_ms.unwrap_or_else(|| started.elapsed().as_millis() as i64);
            cell.note(ms);
        }
    }

    /// 落在列表第几项由 `Rotating` 知道，这里只交出"是哪一家、什么形状"。
    pub fn shape(&self) -> &'static Preset {
        self.shape
    }
}

impl LlmBackend for Live {
    fn complete(&self, prompt: &Prompt) -> Result<String, LlmFailure> {
        let mut budget = if prompt.derive { self.derive_budget } else { self.base_budget };
        let mut sends = 0usize;
        loop {
            sends += 1;
            let body = build_body(
                self.shape,
                &self.target,
                prompt,
                budget,
                self.temperature,
                self.reasoning_effort.as_deref(),
            )
                // `thinking_default` 为真的那一格的拒绝：这是配置事故，不是这家拒绝我们。
                .map_err(|_| LlmFailure::SchemaMismatch)?;
            let url = format!("{}{}", self.target.base_url, self.shape.protocol.path());
            let mut headers = self.shape.protocol.auth_headers(&self.key);
            headers.push(("content-type".to_string(), "application/json".to_string()));
            let started = std::time::Instant::now();
            let raw = match self.egress.post(&url, &headers, &body) {
                Ok(raw) => {
                    self.note_call_ms(started, Some(raw.slow_ms));
                    raw
                }
                Err(e) => {
                    self.note_call_ms(started, None);
                    return Err(e);
                }
            };
            match classify(self.shape, raw.status, &raw.body) {
                Verdict::Reply(text) => return Ok(text),
                Verdict::Failed(f) => {
                    self.ops_line(&raw, "provider failure");
                    return Err(f);
                }
                // 判据 1 那一族：只有抬预算才有效。到顶还截断 ⇒ 节 7.4 那条连锁的读数，上交。
                Verdict::Truncated if sends < MAX_SENDS_PER_CALL && budget < MAX_BUDGET => {
                    self.ops_line(&raw, "self-heal: budget raised");
                    budget = (budget * 2).min(MAX_BUDGET);
                }
                // 判据 2 那一族：**原样**重发（同一个 body、同一个预算）。实测 3/78 ≈ 3.8% 自愈。
                Verdict::Escape if sends < MAX_SENDS_PER_CALL => {
                    self.ops_line(&raw, "self-heal: identical resend");
                }
                Verdict::Truncated | Verdict::Escape => {
                    self.ops_line(&raw, "self-heal exhausted");
                    return Err(LlmFailure::SchemaMismatch);
                }
                // 判据 3 与工具通道：都不许被读成"库里没有"（守卫 8 的假空那一族）。
                // 工具通道在这一批**不接线**（preset 表没有 `tools` 那一格），落在这里就是
                // "这一家把正文放在了第二个落点"的读数 ⇒ 脱约，不换家。
                Verdict::Malformed | Verdict::ToolChannel => {
                    self.ops_line(&raw, "reply shape");
                    return Err(LlmFailure::SchemaMismatch);
                }
            }
        }
    }
}

/// 运维日志的唯一落点：**stderr，不落文件**（节 4）。原文先过 `redact` 才进这一行——
/// 落文件等于开出第二条无守卫的正文下行/备份路径，与 A48 备份面相乘。
/// 这一行永不带 `LlmFailure` 之外的语义：失败矩阵四态是唯一的上抛面。
///
/// `provider` 与 `slow_ms` 的读者有两处：这一行，以及装配点递进来的观测写口
/// （[`Live::with_call_ms`]，W33-A10 起第二读者；判据仍不读它）。换家之后要看得出是哪一家，
/// 快失败与慢超时是两类事故。
/// 自愈那两支也必须留痕——账本按 `complete()` 计数、看不见里面真发了几次，这一行是唯一把
/// "一次调用发了两次真请求"显示出来的地方。
impl Live {
    fn ops_line(&self, raw: &Raw, category: &str) {
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr(),
            "[llm {} {} {}ms {category}] {}",
            self.target.label,
            raw.status,
            raw.slow_ms,
            redact::redact(&raw.body)
        );
    }
}
