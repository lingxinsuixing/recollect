//! 原设计 §6"LLM 不可用矩阵"四态的 Rust 承担面。
//! 这一层的价值全在**不许合并**：四态的动作是同一组（停抽取、开视图、双向提醒），
//! 但**读数各有一条读者**——`QuotaExhausted` 落 P5 的 `quota_exhausted` 位（任务 13），
//! `Timeout` 与 `BackendError` 落在停轮格里必须是两种原因（否则"中心换机器"被读成"密码错了"，
//! 架构 D5 那条）。合并任何两个失败态 = 造出一处"症状相同、动作不同"的静默（GOALS G7 那一族）。

use crate::LlmFailure;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consequence {
    /// 停新增抽取：本批不产出、不推水位（见 `advances_watermark`）。
    StopNewExtract,
    /// 存量照常查：检索与注入视图不关。本 crate 管不到读路径，
    /// 这一格存在的意义是**让它有一条可断言的出处**——少了它，"停抽取"会被实现成"关视图"。
    KeepViewsOpen,
    /// 双向提醒：agent 的 `status()` 状态位 + `doctor` 的告警清单（任务 13 的读者）。
    BidirectionalAlert,
    /// 恢复即续：按双水位差补跑。**只在 `resume_applies` 为 `Some` 时出现**。
    ResumeAndCatchUp,
}

/// 三种失败态的固定动作。返回 `Vec` 而不是布尔字段，是因为调用方（任务 9）要按格打印，
/// 而"停抽取"与"提醒"在两处出口上是两条读数。
pub fn consequences(_f: &LlmFailure) -> Vec<Consequence> {
    vec![
        Consequence::StopNewExtract,
        Consequence::KeepViewsOpen,
        Consequence::BidirectionalAlert,
    ]
}

/// 故障期不推水位（原设计 §6 那句硬要求）。它在 rc-llm 里只能是一条**声明**：
/// 本 crate 拿不到句柄，所以真正的执法点是任务 9（`rc-distill` 只在 `Ok` 分支调 `advance`）。
pub fn advances_watermark(_f: &LlmFailure) -> bool {
    false
}

/// 故障期不计 attempts——同一条声明，同一个执法点。
pub fn counts_attempt(_f: &LlmFailure) -> bool {
    false
}

/// 恢复路径：只有"上一轮失败停轮、这一轮调用成功"才是补跑的形状。
/// 真判它的地方在 `rc-distill`（那里有游标），本函数只提供判据形状。
pub fn resume_applies(prev_failed: bool, now_ok: bool) -> Option<Consequence> {
    (prev_failed && now_ok).then_some(Consequence::ResumeAndCatchUp)
}

/// `BidirectionalAlert` 的载体名（任务 13 的 `status` 字段与 `doctor` 行共用这一个来源，守卫 3）。
pub const ALERT_METRIC: &str = "llm_calls";
