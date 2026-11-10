//! LLM 的唯一出口形状：进一段 prompt，出一个字符串或一种失败。
//! 没有第三种返回类型——把"解析后的结构化结果"放在这一层就等于让 LLM 面决定 schema 语义（那是 rc-distill 的事）。

#[derive(Debug, Clone)]
pub struct Prompt {
    pub system: String,
    pub user: String,
    /// 这一轮是不是推导作业：只有它走座位的 `derive_budget`，其余作业走默认预算。
    pub derive: bool,
}

/// 失败矩阵四态。`Timeout` 与 `BackendError` 分不开的话，"中心换机器"就会被读成"这一批是毒数据"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmFailure {
    Timeout,
    SchemaMismatch,
    QuotaExhausted,
    BackendError,
}

/// `DistillError` 用 `#[error(transparent)]` 包它，而 transparent 要求被包的那个类型自己是一个
/// `std::error::Error`。这一族的 Display 因此**手写**：`rc-llm` 的依赖清单必须保持为空（§1 那一行
/// "dependencies = 无"），为一个四态枚举开一条 `thiserror` 边，等于给"中心换机器"那条判断多一个理由。
/// Display 只给状态名，外部输入一个字节都不进（守卫 12 在错误面上的形状）。
impl std::fmt::Display for LlmFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LlmFailure::Timeout => "llm timeout",
            LlmFailure::SchemaMismatch => "llm output did not match",
            LlmFailure::QuotaExhausted => "llm quota exhausted",
            LlmFailure::BackendError => "llm backend failure",
        })
    }
}

impl std::error::Error for LlmFailure {}

pub trait LlmBackend {
    /// 注意形参里**没有** `now_ms`，也**没有**句柄：预算由调用方的 `Deadline` 决定，
    /// 而真实后端在它内部掐表。mock 不需要掐表（它按脚本返回）。
    fn complete(&self, prompt: &Prompt) -> Result<String, LlmFailure>;

    /// 当前后端是谁（模型 slug）。它是抽取指纹里"模型"那一格**唯一**的读法。
    ///
    /// 默认 `None`：回放座位（hub 的 `Seat`）与 mock 没有模型身份，默认实现让它们的清单
    /// 零改动；调用方把 `None` 写成 `"unknown"`（一个固定词，不是一个空串）。
    /// `Rotating` 覆盖它、交出**当前槽**的 slug——降级之后产地就换了。
    fn slug(&self) -> Option<&str> { None }
}
