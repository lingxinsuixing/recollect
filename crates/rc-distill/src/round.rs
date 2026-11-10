//! 一条 pass 的**编排**：选批 → 执行（`distill once` 的五族共用这一份）。
//!
//! 这一层只回答"这一轮干了什么"：选中了哪些轮、runner 交回的读数是什么、失败时错误是哪一个。
//! 它**不**回答运维面的问题——能力自测（守卫 14）、座位装配（`Rotating`）、失败类别词
//! （`llm`/`rejected`/`store`）与退出码、stdout 的读数形状，全都留在 hub。这条分工是下一步把
//! 四族接进 `cycle once` 的前提：cycle 与 `distill once` 从此共用同一个执行点，读数各写各的。
//!
//! 三条纪律（与抽取前的 hub 逐字对齐，这一片不改行为）：
//! 1. **空批是合法结果**：naming/fact/event 的选批为空 ⇒ `Ok(None)`——不调模型、不写状态，
//!    CLI 打印各自的 `state="none"` 读数并退出 0。time 的空批**不是**这一支：它的选批与回填
//!    共用一个扫描面（在 `run_time` 内部），空批仍写一次审查水位、仍是 `state="empty"` 的
//!    完整读数，所以 time 永远返回 `Ok(Some(..))`。
//! 2. **错误不折成词**：`DistillError` 原样交回，四族各自的 (state, fault) 映射留在 hub。
//!    [`RoundError`] 只补三格 hub 拿不到、又必须打出来的上下文：选中批的轮数、选中批里
//!    被跳过的 `role='system'` 轮数（转换器配套的读数），以及 fact/event 失败时 ledger 里
//!    **真落下**的 state（那是库里的读数，不是从错误变体猜的词）。
//! 3. **句柄分开收**：三个句柄走形参位（与 `run_*_with` 同形），不装进 [`RoundInput`]——
//!    那等于把 `Root` 抄一份到业务层（crate 文档那条纪律）；`RoundInput` 只放"这一轮怎么跑"。
//!
//! 选批里的副作用（压缩摘要当场落 `rejected(compact_summary)`、为会话补 agent/主人锚）照旧由
//! `select_*_batch` 自己承担：它们是"这一轮处理过什么"的事实，不是本层的成绩。

use crate::batch::TurnRow;
use crate::pass_event::{run_event_with_retry, select_event_batch, EventOutcome};
use crate::pass_fact::{run_fact_with, select_fact_batch, FactOutcome};
use crate::pass_naming::{run_naming_with, select_naming_batch, NamingOutcome};
use crate::pass_prompt::{FactFamily, PASS_EVENT, PASS_FACT, PASS_STATE};
use crate::pass_state::{run_state_with, select_state_batch, StateOutcome};
use crate::pass_time::{run_time, TimeOutcome};
use crate::review::ReviewMode;
use crate::DistillError;
use rc_ledger::pass_state::{self, PassKey};
use rc_llm::LlmBackend;
use rc_store::{LedgerHandle, MemoryHandle, TranscriptHandle};

/// 五族。`Fact` 多带一格，因为 fact 的账本键是三族各自表态的（`decision|promise|plan`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// naming：单一族，账本键 `family=''`。
    Naming,
    Fact(FactFamily),
    /// event：单一 pass，账本键 `family=''`。
    Event,
    /// P2 state：单一 pass，账本键 `family=''`；主体只能是已过审的锚（所以它排在
    /// `review naming` 之后，见 `tools/pipeline/daily-drain.sh`）。
    State,
    /// time：纯程序，没有模型面、没有座位。
    Time,
}

/// 一轮的输入。三个句柄不在这里（见模块文档第 3 条）。
pub struct RoundInput<'a> {
    pub family: Family,
    /// 这一轮最多选多少轮；`0` 的"取默认窗宽"判别在各族选批内部。
    pub batch_turns: usize,
    pub now_ms: i64,
    /// 座位。naming/fact/event 必须有（缺了是调用方的形状错误）；time 不读它。
    pub backend: Option<&'a dyn LlmBackend>,
    pub review_mode: ReviewMode,
    /// 空轮重试次数（工单 `2026-10-30-retry-empty`）。**只对 `Event` 有牙齿**：`0` = 今天的行为
    /// （一次抽取调用、原有读数），`N>0` = 第一次判空时同一批原样再采一次（最多 N 次）。
    /// 其余三族读都不读它——CLI 在入口就拒了"非 event 带这一格"的用法，这一层不替它辩护。
    pub retry_empty: usize,
}

/// 跑完的一批：各族 runner 的原样读数，由 hub 折成自己的读表格子。
#[derive(Debug)]
pub enum RoundOut {
    Naming(NamingOutcome),
    Fact(FactOutcome),
    Event(EventOutcome),
    State(StateOutcome),
    Time(TimeOutcome),
}

/// 失败的一轮。承载的是**阶段**（选批 / 执行），不是故障类别词——那个词由 hub 按族给。
#[derive(Debug)]
pub enum RoundError {
    /// 选批失败（读转录/读账本/读记忆任一处）。现状四条 CLI 对这一支一律报 store 面，
    /// 也不打读数：选批没成功就没有"这一轮选中了几轮"可言。
    Select(DistillError),
    /// 执行失败（选批已经成功、选中轮已落 `running`）。读数照打。
    Run {
        /// 原样上抛的错误（不在这里折词）。
        error: DistillError,
        /// 选中批的轮数——失败读数里那一格（退避重试靠它看得见）。
        turns: usize,
        /// 批区间内被跳过的 `role='system'` 轮数（转换器配套）。与 `turns` 同一条理由留在这里：
        /// 它是选批层面的事实，失败读数也要打得出来，而 hub 拿不到 plan。
        system_turns_skipped: usize,
        /// ledger 里 `extract_pass_state` 落下的 state slug（`pass_state::get` 读，读不到为
        /// `None`）。只有 fact/event/state 用它：naming 的失败态由错误变体给词，time 失败不打读数。
        landed: Option<&'static str>,
    },
}

/// 跑一轮：按 `input.family` 选批 → 执行。
///
/// `Ok(None)` 只表示"没有可选的批"（见模块文档第 1 条）；错误原样上抛。
pub fn round_once(
    t: &TranscriptHandle,
    l: &LedgerHandle,
    m: &MemoryHandle,
    input: RoundInput<'_>,
) -> Result<Option<RoundOut>, RoundError> {
    match input.family {
        Family::Naming => {
            let plan = match select_naming_batch(t, l, m, input.batch_turns, input.now_ms) {
                Ok(Some(plan)) => plan,
                Ok(None) => return Ok(None),
                Err(error) => return Err(RoundError::Select(error)),
            };
            let turns = plan.batch.rows.len();
            let system_turns_skipped = plan.system_turns_skipped;
            // 座位在装配阶段就该拒掉"没有来源"的形状；走到这里还没有座位是调用方的形状错误。
            let backend = input.backend.expect("round_once: naming needs a seat");
            match run_naming_with(m, t, l, &plan, backend, input.now_ms, input.review_mode) {
                Ok(out) => Ok(Some(RoundOut::Naming(out))),
                // naming 的失败态由错误变体给词（hub 那一支），这里不读台账。
                Err(error) => {
                    Err(RoundError::Run { error, turns, system_turns_skipped, landed: None })
                }
            }
        }
        Family::Fact(family) => {
            let plan = match select_fact_batch(t, l, m, input.batch_turns, family, input.now_ms) {
                Ok(Some(plan)) => plan,
                Ok(None) => return Ok(None),
                Err(error) => return Err(RoundError::Select(error)),
            };
            let turns = plan.batch.rows.len();
            let system_turns_skipped = plan.system_turns_skipped;
            let backend = input.backend.expect("round_once: fact needs a seat");
            match run_fact_with(m, t, l, &plan, backend, input.now_ms, input.review_mode) {
                Ok(out) => Ok(Some(RoundOut::Fact(out))),
                Err(error) => {
                    let landed = landed_state(l, plan.batch.rows.first(), PASS_FACT, family.slug());
                    Err(RoundError::Run { error, turns, system_turns_skipped, landed })
                }
            }
        }
        Family::Event => {
            let plan = match select_event_batch(t, l, m, input.batch_turns, input.now_ms) {
                Ok(Some(plan)) => plan,
                Ok(None) => return Ok(None),
                Err(error) => return Err(RoundError::Select(error)),
            };
            let turns = plan.batch.rows.len();
            let system_turns_skipped = plan.system_turns_skipped;
            let backend = input.backend.expect("round_once: event needs a seat");
            // 空轮重试只在这一支有牙齿：同一批、同一条 prompt、同一条闸与落库路径，
            // 由 runner 自己决定要不要再开口（`retry_empty=0` ⇒ 一个字节都不变）。
            match run_event_with_retry(
                m, t, l, &plan, backend, input.now_ms, input.review_mode, input.retry_empty,
            ) {
                Ok(out) => Ok(Some(RoundOut::Event(out))),
                Err(error) => {
                    // event 是单一 pass：账本键的 family 恒空串（不是"没有 family"）。
                    let landed = landed_state(l, plan.batch.rows.first(), PASS_EVENT, "");
                    Err(RoundError::Run { error, turns, system_turns_skipped, landed })
                }
            }
        }
        Family::State => {
            let plan = match select_state_batch(t, l, m, input.batch_turns, input.now_ms) {
                Ok(Some(plan)) => plan,
                Ok(None) => return Ok(None),
                Err(error) => return Err(RoundError::Select(error)),
            };
            let turns = plan.batch.rows.len();
            let system_turns_skipped = plan.system_turns_skipped;
            let backend = input.backend.expect("round_once: state needs a seat");
            match run_state_with(m, t, l, &plan, backend, input.now_ms, input.review_mode) {
                Ok(out) => Ok(Some(RoundOut::State(out))),
                Err(error) => {
                    let landed = landed_state(l, plan.batch.rows.first(), PASS_STATE, "");
                    Err(RoundError::Run { error, turns, system_turns_skipped, landed })
                }
            }
        }
        Family::Time => {
            // time 的选批住在 `run_time` 里（与回填同一个扫描面），所以这里没有 `Ok(None)`：
            // 空批也是一轮跑完的结果（`state="empty"`，审查水位照写）。
            match run_time(m, l, t, input.batch_turns, input.now_ms) {
                Ok(out) => Ok(Some(RoundOut::Time(out))),
                // 失败时 CLI 不打读数，`turns` 不参与；这里给 0 而不是编一个数。
                Err(error) => Err(RoundError::Run {
                    error,
                    turns: 0,
                    system_turns_skipped: 0,
                    landed: None,
                }),
            }
        }
    }
}

/// fact/event 失败那一格：`extract_pass_state` 里**真落下**的 state，不从错误变体猜。
///
/// 抽取契约拒收落 `rejected(schema)`、审核契约拒收落 `quarantined(review_schema)`，两者在
/// `DistillError::Fact`/`Event` 上是同一个变体，而 stdout 那一格必须与库里一致。读不到行
/// （或批为空，理论上到不了）时交 `None`，由 hub 按故障类别给同向的兜底词。
///
/// 键由调用方给：`pass_id` 是这一族的 pass 名，`family` 是账本里那一格（event/time 是空串）。
fn landed_state(
    l: &LedgerHandle,
    first: Option<&TurnRow>,
    pass_id: &'static str,
    family: &'static str,
) -> Option<&'static str> {
    let row = first?;
    pass_state::get(l, PassKey { turn_uid: &row.turn_uid, pass_id, family })
        .ok()
        .flatten()
        .map(|record| record.state.slug())
}
