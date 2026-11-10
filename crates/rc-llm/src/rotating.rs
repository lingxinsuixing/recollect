//! provider 轮转状态机（A47 节 4）：**零 IO、零时钟、零计数**。
//!
//! 三件事都必须为真，否则这个模块住不进 `rc-llm`：
//! - 零 IO：它只调 `LlmBackend`，不认识 `ureq`；
//! - 零时钟：`now_ms` 是**参数**（裁定 V14 与 `Deadline` 同形），库里一行都不许出现取时钟的调用；
//! - 零计数：它**不持**任何调用账（那本账在宿主侧）。于是"fallback 不许把一轮预算搬到另一家去
//!   双倍计数"（判据 N7）在这里是**结构事实**，不是纪律：这个模块数不了任何东西。

use crate::{LlmBackend, LlmFailure, Prompt};
use std::cell::Cell;

/// 两次探测之间的最小间隔（秒）。**不许从 `llm.toml` 读**：可配 ⇒ 下一个人会把它调成 0。
pub const PROBE_MIN_INTERVAL_SECS: i64 = 300;
/// 连续几次正常回答才切回主家。同上一条纪律。
pub const PROBE_SUCCESSES_TO_RECOVER: u32 = 3;
/// 连续几次 `SchemaMismatch` 才认定"这家坏了"。单次不前进：瞬时契约逃逸（实测 3.8%）会自愈。
const SCHEMA_MISMATCHES_TO_ADVANCE: u32 = 3;

/// 一次调用的最大请求数：每个座位试一次，外加 `Timeout` 那一支的原样重试。
/// 它是**循环上界**，不是第二条政策——没有它，一家只有一项的列表会在 `Timeout` 上死循环。
const VISITS_PER_CALL: usize = 2;

/// 当前服务者的可读快照（A47 节 4 第 5 条：批级戳与 `doctor` 读数的同一个来源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Serving {
    pub slug: String,
    pub index: usize,
    /// 本轮已前进次数。
    pub failed_over: usize,
    /// 下一次探测允许的最早时刻；未降级时是 `None`（"没有待做的探测"，不是"探测坏了"）。
    pub next_probe_at_ms: Option<i64>,
    /// 连续探测失败数（一次成功即清零）。
    pub probe_failures: usize,
    /// 本轮连续探测成功数（一次失败即清零）。`doctor` 读数与落盘的**唯一共用来源**。
    pub probe_ok: u32,
}

struct Slot {
    slug: String,
    backend: Box<dyn LlmBackend>,
}

/// 跨进程带进来的那份判断（节 4 第 7 条）。字段就是表里那几列，一列不多：
/// `written_at_ms` 只用来判"本机钟有没有倒退过"这一件事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seed {
    pub slug: String,
    pub probe_due_at_ms: Option<i64>,
    pub probe_ok: u32,
    pub probe_failures: usize,
    pub written_at_ms: i64,
}

/// 交回给落盘的那一份。**存 `slug` 不存下标**：下标在 `llm.toml` 改过顺序之后指向另一家。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Persisted {
    pub slug: String,
    pub probe_due_at_ms: Option<i64>,
    pub probe_ok: u32,
    pub probe_failures: usize,
}

pub struct Rotating {
    slots: Vec<Slot>,
    current: Cell<usize>,
    failed_over: Cell<usize>,
    mismatches: Cell<u32>,
    /// `Timeout` 的"先原样重试一次"：每**次调用**进来时清零，所以它是单次调用的局部账。
    timeout_retried: Cell<bool>,
    probe_due: Cell<Option<i64>>,
    probe_ok: Cell<u32>,
    probe_failures: Cell<usize>,
}

impl Rotating {
    /// 空列表是配置错误，不是"暂时没有可用家"：它必须在构造点响亮失败，
    /// 而不是让 `complete()` 在 `slots[0]` 上 panic。
    pub fn new(slots: Vec<(String, Box<dyn LlmBackend>)>) -> Result<Rotating, &'static str> {
        if slots.is_empty() {
            return Err("empty provider list");
        }
        let slots = slots.into_iter().map(|(slug, backend)| Slot { slug, backend }).collect();
        Ok(Rotating { slots, current: Cell::new(0), failed_over: Cell::new(0),
                      mismatches: Cell::new(0), timeout_retried: Cell::new(false),
                      probe_due: Cell::new(None), probe_ok: Cell::new(0),
                      probe_failures: Cell::new(0) })
    }

    /// `new` 的落盘版。四条作废规则里有**两条**住在这里，且**全在纯逻辑层**（零 IO ⇒ 判据 6/N3 不破）：
    /// 钟倒退 ⇒ 整行不读；slug 认不到 ⇒ 当作没降级。
    /// ①机器不符**不归这里也不归调用方**——它由 `rc-ledger::llm_state::read` 的 `WHERE machine=?1` 承担，
    /// 别的机器那一行根本读不出来；④缺身份归 `hub`（不读也不写、响亮一句）。
    /// 这一层拿不到"机器"这个概念，所以它只会收到两种输入：一行同机器的种，或者没有种。
    pub fn seeded(slots: Vec<(String, Box<dyn LlmBackend>)>, seed: &Seed, now_ms: i64) -> Result<Rotating, &'static str> {
        let known = slots.iter().any(|(slug, _)| slug == &seed.slug);
        // 作废规则第 3 条（钟倒退）与第 2 条（认不到的座位），外加"种里本来就是主家"：
        // 三种情况都落到同一个处置 = 空种，因为它们的后果相同——"这台机器没有可继承的判断"。
        let inherit = known && seed.written_at_ms <= now_ms && seed.slug != slots[0].0;
        if !inherit {
            return Rotating::new(slots);
        }
        let r = Rotating::new(slots)?;
        if let Some(i) = r.slots.iter().position(|s| s.slug == seed.slug) {
            r.current.set(i);
            r.probe_due.set(seed.probe_due_at_ms);
            r.probe_ok.set(seed.probe_ok);
            r.probe_failures.set(seed.probe_failures);
        }
        Ok(r)
    }

    /// 写回用的那一份。`current == 0` 时 `probe_due_at_ms` 交 `None`：主家没有"待做的探测"，
    /// 把 0 写进表会被下一个进程读成"随时可以探测"。
    pub fn persisted(&self) -> Persisted {
        let i = self.current.get();
        Persisted {
            slug: self.slots[i].slug.clone(),
            probe_due_at_ms: if i == 0 { None } else { self.probe_due.get() },
            probe_ok: if i == 0 { 0 } else { self.probe_ok.get() },
            probe_failures: if i == 0 { 0 } else { self.probe_failures.get() },
        }
    }

    pub fn serving(&self) -> Serving {
        let i = self.current.get();
        Serving {
            slug: self.slots[i].slug.clone(),
            index: i,
            failed_over: self.failed_over.get(),
            next_probe_at_ms: if i == 0 { None } else { Some(self.probe_due_at_ms()) },
            probe_failures: self.probe_failures.get(),
            probe_ok: self.probe_ok.get(),
        }
    }

    /// 降级态下每个 cycle tick 调一次；`probe` 是**同一套 codec 成功判据下的一次真实契约调用**
    /// （prompt 由宿主给：本 crate 不认识输出契约，探针的"正常回答"因此不是第二套判据）。
    /// 返回 `true` = 这次探测凑够 `PROBE_SUCCESSES_TO_RECOVER` 次正常、已切回主家。
    pub fn probe_primary(&self, now_ms: i64, probe: &Prompt) -> bool {
        if self.current.get() == 0 {
            return false;
        }
        if now_ms < self.probe_due_at_ms() {
            return false;
        }
        self.probe_due.set(Some(now_ms + PROBE_MIN_INTERVAL_SECS * 1_000));
        if self.slots[0].backend.complete(probe).is_ok() {
            self.probe_failures.set(0);
            let n = self.probe_ok.get() + 1;
            self.probe_ok.set(n);
            if n >= PROBE_SUCCESSES_TO_RECOVER {
                self.current.set(0);
                self.probe_ok.set(0);
                self.probe_due.set(None);
                return true;
            }
        } else {
            self.probe_ok.set(0);
            self.probe_failures.set(self.probe_failures.get() + 1);
        }
        false
    }

    /// 没探测过时是 0：于是"降级后的第一个 tick 就探测"，此后每两次探测之间 ≥ 300 秒。
    /// （`advance()` 拿不到 `now_ms`——`complete()` 的签名里没有它，那是裁定 V14 定的形状，
    /// 所以降级时刻只能记在第一次探测上。）
    fn probe_due_at_ms(&self) -> i64 {
        self.probe_due.get().unwrap_or(0)
    }

    /// 前进到列表下一项。**换家不推水位**（`failmatrix::advances_watermark` 为 `false` 那一族），
    /// 也不动调用账（见文件头第三条）。
    fn advance(&self) {
        let n = self.slots.len();
        self.current.set((self.current.get() + 1) % n);
        self.failed_over.set(self.failed_over.get() + 1);
        self.mismatches.set(0);
    }
}

impl LlmBackend for Rotating {
    fn complete(&self, prompt: &Prompt) -> Result<String, LlmFailure> {
        self.timeout_retried.set(false);
        let visits = self.slots.len() * VISITS_PER_CALL;
        let mut last = LlmFailure::BackendError;
        for _ in 0..visits {
            let r = self.slots[self.current.get()].backend.complete(prompt);
            last = match r {
                Ok(s) => {
                    self.mismatches.set(0);
                    return Ok(s);
                }
                Err(e) => e,
            };
            match last {
                LlmFailure::BackendError => self.advance(),
                LlmFailure::Timeout => {
                    if self.timeout_retried.get() {
                        self.advance();
                    } else {
                        self.timeout_retried.set(true);
                    }
                }
                LlmFailure::SchemaMismatch => {
                    let c = self.mismatches.get() + 1;
                    self.mismatches.set(c);
                    if c >= SCHEMA_MISMATCHES_TO_ADVANCE {
                        self.advance();
                    } else {
                        // 不前进：这一支的处置是"本批丢掉、水位不动"，不是"换一家再问一遍"。
                        return Err(last);
                    }
                }
                // 额度耗尽那一态**绝不前进**（N7 的行为面）。`Rotating` 不持任何调用账，所以这条路
                // 只在"某个后端自己回了 `QuotaExhausted`"时出现——那是 provider 侧额度的误映射，
                // 正确映射是归一化成 `BackendError`（`rc-llm-http` 那条），这里原样上抛、不前进。
                LlmFailure::QuotaExhausted => return Err(last),
            }
        }
        Err(last)
    }

    /// 当前槽的 slug。与 `serving()` 同源（同一个 `current`），但**不 clone**：
    /// 抽取指纹每一轮读一次，而 `Serving` 是给运维面的一整份快照。
    /// 槽列表非空由构造点保证（`new` 拒绝空列表），所以这里没有 `None` 的分支。
    fn slug(&self) -> Option<&str> { Some(self.slots[self.current.get()].slug.as_str()) }
}
