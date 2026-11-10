//! 唯一的算分入口：整数乘除、无浮点、无 `bm25()`。
//!
//! 为什么不用 `bm25()`：它返回 REAL。REAL 就是"两次跑出 0.8999999 与 0.9000001"，
//! 就是"阈值定在哪都不算错"，也就是跨端/跨次不可复现那一族的源头。
//! 这里的替代物更粗——而粗是安全的一侧：匹配度只决定"要不要开口"（`above_floor`），
//! 永远不决定"谁有权威"。

/// §8 的阈值。它是**一个常量而不是配置项**（D2 闭账：一期不做配置面），
/// 且它必须在空库与一千行库上给出行数上限相同的结果——那条判据（G6①）在任务 11 的脚本里断。
///
/// 方向：拐点**松一档**。理由是消费侧第 3 条——阈值调紧的症状是"该提的不提"（沉默），
/// 调松的症状是"提得略多"（最多 3 行，硬上限在渲染层）。前者用户读成失忆，后者读成话多。
/// **默认值待阶段 5 的阈值-召回曲线定**；`1` 是"任何非零整数命中都过线"，
/// 也就是把一期实际行为交给"命中与否"而不是"命中比例"，这是当前唯一诚实的取值。
pub const FLOOR: i64 = 1;

/// 唯一算分入口。整数乘除、无浮点、无 `bm25()`（它返回 REAL，见本节开头）。
/// `matched` 数的是**不同的**查询词命中数：`seen` 去重是公式的一部分，不是优化。
///
/// `row_len` 是这一行的**总词数（含重复）**，来自影子表自己的口径（`rc_index::SearchHit`），
/// 不是唯一词数——去重会让长行看起来短。
/// 除零守卫（`row_len == 0` 短路）必须留着：`rc-index` 现在结构性地给 `row_len` 取下界 1
/// （裁定 R20-5），但那不等于这把尺子可以依赖运气（见 `tests/score.rs::零命中是_0_不是负数也不是_1`）。
pub fn score_row(
    query_tokens: &std::collections::HashSet<String>,
    row_tokens: &[String],
    row_len: usize,
) -> i64 {
    if row_len == 0 {
        return 0;
    }
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let matched = row_tokens
        .iter()
        .filter(|t| query_tokens.contains(*t) && seen.insert(t.as_str()))
        .count() as i64;
    1024 * matched / row_len as i64
}
