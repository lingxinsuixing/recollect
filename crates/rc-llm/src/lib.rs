//! LLM 面：后端接口 + 失败矩阵四态 + 家族墙钟。
//! **本 crate 零内部依赖**是它的存在理由：抽取路径的产出必须经 `rc-distill` 才碰得到库，
//! 于是"故障期不污染水位"不是一条纪律，而是一个写不出来的调用。
//!
//! 这里**没有**本地调用次数上限：一轮烧几次由 `rc-distill` 的批大小决定，账由宿主的账本记。
//! 额度耗尽仍是一种真故障（`LlmFailure::QuotaExhausted`），但那说的是**供应商那一侧**。
pub mod backend;
pub mod failmatrix;
#[doc(hidden)]
pub mod mock;
pub mod observation;
pub mod rotating;
pub mod wallclock;

pub use backend::{LlmBackend, LlmFailure, Prompt};
pub use failmatrix::{advances_watermark, consequences, counts_attempt, resume_applies, Consequence, ALERT_METRIC};
pub use observation::MaxCell;
pub use rotating::{Rotating, Serving, Persisted, Seed, PROBE_MIN_INTERVAL_SECS, PROBE_SUCCESSES_TO_RECOVER};
pub use wallclock::{cycle_budget_ms, Deadline, JobFamily};
