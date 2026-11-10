//! W25：`hub pending` 的**薄读数**——抽干前的水位闸（主触发）。
//!
//! 这一面只回答一件事："现在还有没有活要干"。判定（跑不跑）住在 shell 侧
//! （`tools/pipeline/drain-if-pending.sh`），这里只交数字：**零判定、零阈值、零写**。
//!
//! 三条纪律：
//! 1. **只读装配**：调用方用 `rc_store::Root::open_readonly`（W24a）。这一面会被 10 分钟一次的
//!    定时器叫起来，`Root::open` 的迁移副作用（新建空表、撑大 WAL，P1-5 实测）不该被"读一眼"触发。
//! 2. **一格一个新判据都不写**：四组格全部搬既有读法——`pass_state::count_by_state` /
//!    `water::lag_by_scope` / `review_mark::all` / `water::read("derive","")` +
//!    `rc_memory::reader::count_rows_since` / `water::last_derive_at_ms` /
//!    `pass_state::last_terminal_at_ms`。同一个数不许有第二个家（守卫 3）。
//! 3. **读不出来就报错**：`None` 只用于"从来就没有"（`last_success` 两格），不用于"读不到"。
//!    "读不到"经 `Err` 交给调用方走退出码 3——水位闸的脚本靠这个码区分
//!    "没有待办"（退出 0）与"读不出来"（退出 3），绝不能把后者念成前者。

use rc_ledger::pass_state::{self, PassState, PASS_IDS};
use rc_ledger::water;
use rc_store::{Root, StoreError};

/// 两个库的错误面合成一格：这一层只搬数，不替调用方分型（分型在 hub 的退出码上）。
/// `Display` 只给类别词，零路径、零 SQL 串——与 hub 运维面的"零插值"同一条律。
#[derive(Debug)]
pub enum PendingError {
    Store(StoreError),
    Memory(rc_memory::MemError),
}

impl std::fmt::Display for PendingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PendingError::Store(_) => f.write_str("store"),
            PendingError::Memory(_) => f.write_str("memory"),
        }
    }
}

impl From<StoreError> for PendingError {
    fn from(e: StoreError) -> Self { PendingError::Store(e) }
}

impl From<rc_memory::MemError> for PendingError {
    fn from(e: rc_memory::MemError) -> Self { PendingError::Memory(e) }
}

/// 抽取轴：几条 pass 行还在排队/在跑，几轮还没蒸完。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExtractPending {
    /// `extract_pass_state` 里 `state='pending'` 的**行**数（单位是格，不是轮）。
    pub pending_passes: i64,
    /// `state='running'` 的行数。它是"有进程正在跑"的读数，不是"卡住"——判死活不在这里。
    pub running_passes: i64,
    /// 可抽取轮里还没有终态 pass 行的轮数（`water::lag_by_scope` 的和）。
    pub lag_turns: i64,
    /// 有落差的 scope 数（`lag_by_scope` 的长度；lag ≤ 0 的 scope 不在其中）。
    pub scopes_with_lag: i64,
}

/// 审核轴：待审与隔离的**条**数（`review_watermark` 两列的跨 pass 求和）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReviewPending {
    pub pending: i64,
    pub quarantined: i64,
}

/// 推导轴：游标之后还有几行可推（`count_rows_since` 现算，与 `status.derive_lag_rows` 同源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DerivePending {
    pub lag_rows: i64,
    /// `job_watermark` 的 derive 游标 `at_ms`。**没有游标行 ⇒ 0**：这一格与 `lag_rows` 同读一个
    /// "从头算"的口径（`count_rows_since(None)` 就是全量），0 在这里读作"从时间起点开始"，
    /// 不是"游标停在 0 点"——两者对闸的后果相同（都要跑），所以不为它单开一个 `null` 档。
    pub cursor_at_ms: i64,
    /// 这一族最后一次成功推进的时刻；`None` = 从没成功过（不是 0 点，也不是读失败）。
    pub last_success_at_ms: Option<i64>,
}

/// 抽取 pass 的心跳：最后一格终态落下的时刻。`None` = 一格终态都没有。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PassHeartbeat {
    pub last_success_at_ms: Option<i64>,
}

/// 四组格的整份读数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PendingReport {
    pub extract: ExtractPending,
    pub review: ReviewPending,
    pub derive: DerivePending,
    pub pass: PassHeartbeat,
}

impl PendingReport {
    /// 机器面：`hub pending --json` 的键面（契约见派发档 W25 §1，逐格断言在
    /// `crates/hub/tests/pending_cli.rs`）。`Option` 的 `None` 由 serde 渲染成 `null`，
    /// 所以"键在场、值可为 null"这一条由类型兑现，不靠额外分支。
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "extract": {
                "pending_passes": self.extract.pending_passes,
                "running_passes": self.extract.running_passes,
                "lag_turns": self.extract.lag_turns,
                "scopes_with_lag": self.extract.scopes_with_lag,
            },
            "review": {
                "pending": self.review.pending,
                "quarantined": self.review.quarantined,
            },
            "derive": {
                "lag_rows": self.derive.lag_rows,
                "cursor_at_ms": self.derive.cursor_at_ms,
                "last_success_at_ms": self.derive.last_success_at_ms,
            },
            "pass": {
                "last_success_at_ms": self.pass.last_success_at_ms,
            },
        })
    }

    /// 人读面：一行、`k=v`、`null` 写成 `none`（与 `derive status` 的文本面同一个写法——
    /// "没有"与"0"在这一面必须长得不一样）。
    pub fn human(&self) -> String {
        fn or_none(v: Option<i64>) -> String {
            match v { Some(ms) => ms.to_string(), None => "none".to_owned() }
        }
        format!(
            "pending_passes={}  running_passes={}  lag_turns={}  scopes_with_lag={}  \
             review_pending={}  quarantined={}  derive_lag_rows={}  cursor_at_ms={}  \
             last_derive_success_at_ms={}  last_pass_success_at_ms={}",
            self.extract.pending_passes,
            self.extract.running_passes,
            self.extract.lag_turns,
            self.extract.scopes_with_lag,
            self.review.pending,
            self.review.quarantined,
            self.derive.lag_rows,
            self.derive.cursor_at_ms,
            or_none(self.derive.last_success_at_ms),
            or_none(self.pass.last_success_at_ms),
        )
    }
}

/// 读四组格。**只读**：所有调用都是 `SELECT`，写口一个都不碰。
pub fn read(root: &Root) -> Result<PendingReport, PendingError> {
    // 抽取轴①：pending / running 是**行**数。`count_by_state` 是按 pass 摊开的读数（五个 pass
    // 各一次），这里只把它加起来——不另写一条 `SELECT count(*) … state IN (…)`，那会变成
    // 第二份"哪些状态算中间态"的清单（坏 slug 响亮的纪律也只在那一条读法里）。
    let mut pending_passes = 0i64;
    let mut running_passes = 0i64;
    for pass in PASS_IDS {
        for (state, n) in pass_state::count_by_state(root.ledger(), pass)? {
            match state {
                PassState::Pending => pending_passes += n,
                PassState::Running => running_passes += n,
                _ => {}
            }
        }
    }
    // 抽取轴②：与 `rc-agent` 状态面的 `lag_scopes` 同一个函数（分母 = 可抽取轮、减数 = 终态行）。
    let lag = water::lag_by_scope(root.ledger(), root.transcript())?;
    let lag_turns = lag.iter().map(|(_, n)| *n).sum();
    let scopes_with_lag = lag.len() as i64;

    // 审核轴：水位表是**每 pass 一行**，两格都是"当前值"（`review_mark` 的写口做的是覆盖），
    // 所以跨 pass 求和就是全库的待审/隔离条数；不做增量、不读 `review_item` 明细。
    let marks = rc_ledger::review_mark::all(root.ledger())?;
    let review_pending = marks.iter().map(|m| m.pending_review).sum();
    let review_quarantined = marks.iter().map(|m| m.quarantined).sum();

    // 推导轴：游标整对搬进 `count_rows_since`（`cursor_turn_uid` 存的正是 `tbl:id` 形状）。
    // **没有游标行 ⇒ `from=None`**（全量），与派发档同一条口径。
    let cursor = water::read(root.ledger(), "derive", "")?;
    let from = cursor.as_ref().map(|c| (c.at_ms, c.turn_uid.clone()));
    let lag_rows = rc_memory::reader::count_rows_since(root.memory(), from)?;
    let cursor_at_ms = cursor.as_ref().map(|c| c.at_ms).unwrap_or(0);

    Ok(PendingReport {
        extract: ExtractPending { pending_passes, running_passes, lag_turns, scopes_with_lag },
        review: ReviewPending { pending: review_pending, quarantined: review_quarantined },
        derive: DerivePending {
            lag_rows,
            cursor_at_ms,
            last_success_at_ms: water::last_derive_at_ms(root.ledger())?,
        },
        // pass 的心跳**不读 `job_watermark`**：那一族今天没有写者（`pass_state` 文件头点过名——
        // "job_watermark 的 daily 路径今天根本不写"），读它只会在现网恒得 `null`。
        // "最后一格终态落下的时刻"的家是 `pass_state::last_terminal_at_ms`，`status.last_extract_at_ms`
        // 与它同源（守卫 3）。
        pass: PassHeartbeat { last_success_at_ms: pass_state::last_terminal_at_ms(root.ledger())? },
    })
}
