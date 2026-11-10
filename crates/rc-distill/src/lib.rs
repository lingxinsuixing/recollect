//! 抽取 / 判同 / 审核三作业。**全系统唯一被允许同时命名会话句柄与记忆句柄的 crate**（架构 §4）。
//! 两个句柄一律分开收（形参位是 `&TranscriptHandle` 与 `&MemoryHandle` 两个），
//! 不许有"三柄结构"传进来——那等于把 `Root` 复制一份到业务层，破守卫 1 与守卫 4。
pub mod a1_eval;
pub mod a1_run;
pub mod adjudicate;
pub mod audit;
pub mod batch;
pub mod candidates;
pub mod contract;
pub mod contract_v4;
pub mod dedupe;
pub mod derive;
pub mod episode;
pub mod extract;
pub mod pass_event;
pub mod pass_event_contract;
pub mod pass_fact;
pub mod pass_fact_contract;
pub mod pass_lesson;
pub mod pass_lesson_contract;
pub mod pass_naming;
pub mod pass_naming_review_contract;
pub mod pass_prompt;
pub mod pass_rows_review_contract;
pub mod pass_state;
pub mod pass_state_contract;
pub mod pass_time;
pub mod prepare;
pub mod prompt;
pub mod prompt_v4;
pub mod review;
pub mod round;
pub mod value_backfill;

/// `extract_state.reason` 的三个非契约短语（第四个是 `ContractError::slug()` 的 `schema`）。
pub const FENCE_EMPTY_REASON: &str = "fence";
pub const QUOTA_REASON: &str = "quota";
pub const HALT_REASON: &str = "halt";

/// `extract_state.reason` 那一列的**封闭清单**。它是四词的集合，不是四词的取值之家：
/// `schema` 的家是 `ContractError::slug()`，其余三个是上面那三个常量。这一格存在的意义是让
/// "这一列只许出现这四个词"有一处可断言的地方——写状态的人按名字取词，读状态的人对着这张表判词。
pub const REASON_SLUGS: [&str; 4] = ["schema", FENCE_EMPTY_REASON, QUOTA_REASON, HALT_REASON];

pub use audit::{ApplyReport, AuditError, AuditOutcome, Vote};
pub use batch::{Batch, TurnRow};
pub use contract::{ContractError, Draft, ExtractOutput, KEYS};
pub use contract_v4::{parse_a1, plan_a1, stable_anchor_keys, A1Error, A1Output};
pub use a1_run::{run_a1_once, run_a1_pair};
pub use pass_naming::{run_naming, select_naming_batch, NamingOutcome, NamingPlan};
pub use pass_fact::{
    family_of_slug, run_fact, select_fact_batch, FactOutcome, FactPlan, FACT_PIPELINE_PREFIX,
};
pub use pass_fact_contract::{parse_fact_output, FactDraft, FactPassError};
pub use pass_event::{run_event, select_event_batch, EventOutcome, EventPlan, EVENT_PIPELINE};
pub use pass_event_contract::{parse_event_output, parse_event_review, EventDraft, EventPassError};
pub use pass_lesson::{
    run_lesson, select_lesson_batch, LessonOutcome, LessonPlan, LessonRunError, LESSON_PIPELINE,
};
pub use pass_lesson_contract::{
    parse_lesson_output, parse_lesson_review, LessonDraft, LessonPassError,
};
pub use pass_prompt::{
    event_system, event_user, fact_system, fact_user, state_review_system, state_system,
    state_review_user, state_user, FactCandidate, FactFamily,
};
pub use pass_state::{run_state_with, select_state_batch, StateOutcome, StatePlan, STATE_PIPELINE};
pub use pass_state_contract::{
    parse_state_output, parse_state_output_lenient, parse_state_review, LenientStates, StateDraft,
    StatePassError,
};
pub use pass_time::{
    parse_time_text, resolve_pending_time_expr, run_time, select_time_turns, ParsedTime,
    TimeOutcome, TimeResolveReport, TIME_PIPELINE,
};
pub use dedupe::JudgeError;
pub use derive::{DeriveExit, DeriveReadout};

/// 本层的错误面。`Memory` 那一支原名 `Write`——任务 7 起它也用于**读**失败
/// （`rows_in_scope` / `scope_of` / `lookup` 的 `MemError`），名字必须跟着真相走。
/// 改名要动的两处：枚举定义这一行，与 `extract.rs` 里那个把 `MemError` 折进本层错误面的 `.map_err`。
/// 这些变体全都不带自由文本载荷（守卫 12：`DistillError` 会顺着 hub 的 stderr 变成运维输出）。
/// S2b-2b 多一个 `Fact`：它装的是 [`FactPassError`] 的**变体**（没有任何模型正文），
/// 且 Display 只给一句静态话——载荷留给测试与排障，不进运维面。
#[derive(Debug, thiserror::Error)]
pub enum DistillError {
    #[error(transparent)] Contract(#[from] crate::contract::ContractError),
    #[error(transparent)] A1(#[from] crate::contract_v4::A1Error),
    /// fact pass 的契约拒收（抽取 parser / 审核 parser 共用这一格）。`schema_version=0`
    /// 的新契约不冒充 v4，所以不塞进 `A1`；也不塞进 `Contract`，那是旧五键契约的主语。
    #[error("fact pass output rejected by its contract")]
    Fact(crate::pass_fact_contract::FactPassError),
    /// event pass 的契约拒收（S2c-2，抽取 parser / 审核 parser 共用这一格）。与 `Fact` 同形：
    /// 载荷是 [`crate::pass_event_contract::EventPassError`] 的**变体**（没有任何模型正文），
    /// Display 只给一句静态话。
    #[error("event pass output rejected by its contract")]
    Event(crate::pass_event_contract::EventPassError),
    /// P2 状态 pass 的契约拒收（抽取 parser / 审核 parser 共用这一格）。与 `Event` 同形：
    /// 载荷是 [`crate::pass_state_contract::StatePassError`] 的**变体**（没有任何模型正文），
    /// Display 只给一句静态话。
    #[error("state pass output rejected by its contract")]
    State(crate::pass_state_contract::StatePassError),
    /// naming 的窄审拒收（S3b-3c）：`pass-naming-review-v0.2` 的 parser 只有一个 slug 面
    /// （六码闭集 + 形状格），所以载荷就是那枚 slug。Display 仍是一句静态话——运维面读不到
    /// 模型正文（守卫 12）；slug 只进台账与测试。
    #[error("naming review output rejected by its contract")]
    NamingReview(String),
    #[error(transparent)] Llm(#[from] rc_llm::LlmFailure),
    #[error(transparent)] Memory(#[from] rc_memory::MemError),
    #[error(transparent)] Store(#[from] rc_store::StoreError),
    #[error(transparent)] Judge(#[from] JudgeError),
    #[error(transparent)] Audit(#[from] AuditError),
    #[error("batch has no rows to extract")] EmptyBatch,
    #[error("batch produced no content item and no worthless verdict")] ZeroOutput,
}

impl DistillError {
    /// **只对契约族**返回 `Some("schema")`。任务 9 的 hub 用 `reason_slug().is_some()`
    /// 当"整批丢弃"的判据（P-40），所以这里多一个变体、hub 就多跳一次分支——
    /// `Judge` 与 `Audit` 是运维失败（exit 3），不是"这批被拒收"（exit 2）。
    pub fn reason_slug(&self) -> Option<&'static str> {
        match self {
            Self::Contract(_) | Self::A1(_) | Self::Fact(_) | Self::Event(_) | Self::State(_)
            | Self::EmptyBatch | Self::ZeroOutput => Some("schema"),
            // `Lesson`（波三刀 A）与 fact/event 的契约族同一支：它是"这一批被契约拒收"，
            // hub 按 exit 2 分类。放在 `Some(..)` 这一侧是读数算术（P-40 的判据用的正是这个）。
            // `Audit` 与 `Judge` 是运维失败（hub 的 exit 3），不是"这批被拒收"（exit 2）。
            // 它们落在这里而不是 `Some(..)` 是**读数算术**：P-40 的判据用的正是这个 `is_some()`。
            // `NamingReview`（S3b-3c）与它们同一支：它是 naming 窄审的拒收，不是"抽取契约族"
            // （`reason_slug` 只给抽取面那一格 `schema`），hub 自己按契约族分类（与 `Contract|A1` 同支）。
            Self::Llm(_) | Self::Memory(_) | Self::Store(_) | Self::Judge(_) | Self::Audit(_)
            | Self::NamingReview(_) => None,
        }
    }
}

/// 手写而不是 `#[from]` 再开一个 `Sql` 变体：`StoreError` 自己已经**手写**了
/// `impl From<rusqlite::Error> for StoreError`（rc-store/src/lib.rs，函数体
/// `StoreError::Sql(e.to_string())`），两层 `into` 复用它已有的消毒路径，而不是在
/// `DistillError` 上留一个"原样转述 rusqlite"的口子。少了这一条本任务编译不过：
/// 证据表事务里的三处 `?` 直接落在 `rusqlite::Error` 上，不经过 `rc-store` 的类型。
impl From<rusqlite::Error> for DistillError {
    fn from(e: rusqlite::Error) -> Self { Self::Store(e.into()) }
}
