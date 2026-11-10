//! 共享的 cycle 管线（B2b-1）：一轮 = 一个批次（§2.15）。
//!
//! 这段代码原先整段住在 `hub/src/main.rs`（`run_cycle_once` 及其读数、判定、错误面、扫描节奏），
//! B2b-1 把它原样搬到这里，**行为一个字节都没改**：hub 的既有 `cycle_once` 用例是这一搬的回归闸。
//! 搬的理由是节点离线自蒸要用同一条 pipeline（工单 §二.5），而"回放座位"与"读文件"是 hub 的事
//! （`Reply` / `Seat` / `replay::load` 留在 hub），所以这一层只有**纯管线**。
//!
//! 边界（派发档 §四）：
//! - 不依赖 hub / rc-net，也不碰文件系统；
//! - **不读钟、不读环境变量**：`now_ms` 与轮号一律由调用方递进来（判据 12a 同一条纪律）；
//! - 不打印、不决定退出码：判定（[`CycleExit`]）与读数（[`CycleOut`]）分开返回，
//!   打印与 0/2/3 的映射留在 hub 的 `finish_cycle`。
//!
//! 账本 [`Metered`] 也搬了进来（原住 hub 的 `replay`）：节点要与中心用同一本账的形状，
//! 而"这一轮烧了几次口"从来不属于回放。公开面只加了 `pub`，计数器口径一行没动（P-33）。

use rc_distill::{audit, dedupe, extract, DistillError};
use rc_ledger::{counters, water};
use rc_llm::{JobFamily, LlmBackend, LlmFailure, Prompt};
use rc_store::capability::CapReport;
use rc_store::{Root, StoreError};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// 账本（§4.8 ② 拆分的前一半）：计数器面全部留在原地——`calls` / `saw_quota_exhausted` /
/// `use_family` 的名字与语义、以及"真开过口才算一次调用"那条算术，一个字节都没动（P-33）。
///
/// **没有本地调用次数上限**（2026-11-06 用户裁定：调用配额是预算，砍掉）。账只累加，不设闸：
/// 一轮能问几次由 `rc-distill` 的批大小决定，而 `saw_quota_exhausted` 记的是**供应商**回的
/// 额度耗尽，不是本进程数出来的。
///
/// B2b-1 起它住这里而不是 hub 的 `replay`：节点离线自蒸要与中心共用同一种账本形状，
/// 而"这一轮烧了几次口"本来就是管线的账面，不是回放脚本的属性。**来源**（`reply` 脚本的
/// `Seat`）仍住 hub——它替这本账记账，但账不在它那里。
pub struct Metered {
    /// 当前正在记账的家族。调用方在每一支作业**之前**设它（`use_family`），
    /// 于是"哪一次调用属于哪个家族"这件事只有一个决定点。
    family: Cell<JobFamily>,
    tally: RefCell<Vec<(JobFamily, usize)>>,
    /// 本轮是否出现过 `QuotaExhausted`。两条生产者：脚本里那条 `{"fail":"quota_exhausted"}`，
    /// 以及下面 `MeteredBackend::complete` 收到后端回 `Err(QuotaExhausted)` 的那一支。
    /// 两条说的都是**供应商额度耗尽**。`CycleOut.quota_exhausted` 的唯一来源。
    saw_quota: Cell<bool>,
    /// 观测面（W33-A10）第一格：本进程最慢一发真实出网的耗时（毫秒，单调钟）。
    /// 写口在 rc-llm-http 的 `Live`（每次真实出网经 `MaxCell::note` 取 max）；`Rc` 是因为
    /// `Live` 不认识这本账，装配点把这一格递过去两边共享。读口 [`Metered::max_call_ms`]。
    /// 回放座位（`--replies`）没有真实出网 ⇒ 恒 0（stub 不发明耗时）。
    max_call_ms: Rc<rc_llm::MaxCell>,
    /// 观测面第二格：`LlmFailure::Timeout` 的计数。唯一写口 [`Metered::note_timeout`]。
    timeouts: Cell<u32>,
}

impl Metered {
    /// 账本的唯一构造点。不收任何上限：这一层既不读环境也不读文件，更没有闸。
    pub fn new() -> Metered {
        Metered { family: Cell::new(JobFamily::Extract),
                  tally: RefCell::new(vec![]), saw_quota: Cell::new(false),
                  max_call_ms: Rc::new(rc_llm::MaxCell::new()), timeouts: Cell::new(0) }
    }

    pub fn use_family(&self, f: JobFamily) { self.family.set(f); }

    pub fn calls(&self, f: JobFamily) -> usize {
        self.tally.borrow().iter().find(|(x, _)| *x == f).map(|(_, n)| *n).unwrap_or(0)
    }

    pub fn saw_quota_exhausted(&self) -> bool { self.saw_quota.get() }

    /// 真发生的一次调用在这里落账：主语 + 计数，两件事同一个入口（守卫 3）。
    /// 只累加、不判断、永不 `Err`——账房不设闸。
    pub fn record_call(&self) { self.counted(); }

    /// `CycleOut.quota_exhausted` 的唯一写口。两条生产者都走这里（脚本声明 / 后端回的额度耗尽）。
    pub fn mark_quota_exhausted(&self) { self.saw_quota.set(true); }

    /// 观测面：出网耗时观测格的**唯一颁发口**。装配点（hub 的 `live_slots_with_deadline`）
    /// 在把 `Live` 装进账本外套之前拿这一格递给 [`rc_llm_http::Live::with_call_ms`]，
    /// 于是写口（Live 每次真实出网）与读口（下面两个读数）落在同一份内存上。
    pub fn call_ms_cell(&self) -> Rc<rc_llm::MaxCell> { self.max_call_ms.clone() }

    /// 观测面读数：本进程最慢一发真实出网的耗时（毫秒）。没有真实出网（回放/空轮）⇒ 0。
    pub fn max_call_ms(&self) -> i64 { self.max_call_ms.get() }

    /// 观测面读数：`LlmFailure::Timeout` 计数（脚本里显式声明的那一支）。
    pub fn timeouts(&self) -> u32 { self.timeouts.get() }

    /// 观测面：`Timeout` 的唯一写口。只累加、不判断——账房不设闸（与 `record_call` 同一条律）。
    pub fn note_timeout(&self) { self.timeouts.set(self.timeouts.get() + 1); }

    /// 去重口径与 hub 的 `bump_rejects_by_device` 同形：一张 `Vec` 上找、找不到就 push。
    fn counted(&self) {
        let f = self.family.get();
        let mut t = self.tally.borrow_mut();
        match t.iter_mut().find(|(x, _)| *x == f) {
            Some(e) => e.1 += 1,
            None => t.push((f, 1)),
        }
    }
}

/// 给真后端（`Live`）套上同一本账：调后端 → 无论成败 `record_call`。
/// 回放座位 `replay::Seat` 的记账语义逐字相同；live 不能绕过 `llm_calls`。
pub struct MeteredBackend {
    inner: Box<dyn LlmBackend>,
    ledger: Rc<Metered>,
}

impl MeteredBackend {
    pub fn new(inner: Box<dyn LlmBackend>, ledger: Rc<Metered>) -> MeteredBackend {
        MeteredBackend { inner, ledger }
    }
}

/// `new()` 不收参数，所以默认构造与它同义（clippy 的 `new_without_default` 要的就是这一句：
/// 两个入口都不许长出自己的默认值，否则"一本空账"会有两份表示）。
impl Default for Metered {
    fn default() -> Metered { Metered::new() }
}

impl LlmBackend for MeteredBackend {
    fn complete(&self, prompt: &Prompt) -> Result<String, LlmFailure> {
        let result = self.inner.complete(prompt);
        self.ledger.record_call();
        if let Err(LlmFailure::QuotaExhausted) = &result {
            self.ledger.mark_quota_exhausted();
        }
        // 观测面（W33-A10）：真后端上交的 Timeout 记一笔。耗时那格不经这里——它由
        // `Live` 直接写观测格（真实传输的单调钟），这一层没有比它更准的数，不抄第二份。
        if let Err(LlmFailure::Timeout) = &result {
            self.ledger.note_timeout();
        }
        result
    }

    /// 账本没有身份，产地是里面那个后端的：这一层只委托，不发明。
    fn slug(&self) -> Option<&str> { self.inner.slug() }
}

/// 运维面只许有这十七个读数（守卫 12）：没有正文、没有路径、没有分数。
/// `derive_drop_reason` 是**丢批时才出现**的第十八格（`skip_serializing_if`），
/// happy path 的十七格形状一个字节不变。
/// 比大纲的六格多 `merged` 与 `suppressed`，理由见 §2.16 第 4 条（判同与审核是新装的三支里的
/// 两支，没有读数格就等于装配成功了但运维面看不见）。
#[derive(serde::Serialize)]
pub struct CycleOut {
    pub schema_version: u32,
    pub turns_scanned: usize,
    pub rows_written: usize,
    pub batches_dropped: usize,
    pub quota_exhausted: bool,
    pub halted: bool,
    pub merged: usize,
    pub suppressed: usize,
    // P6 的读数（裁定 20：只加不减，schema_version 仍 = 1）
    pub derive_rows: usize,
    pub derive_rejected: usize,
    pub derive_dropped: usize,
    /// 丢批的封闭类别。只在真丢批时出现（`skip_serializing_if`），所以 happy path 的
    /// 十七格 JSON 形状不变；值域是 `schema` 这类固定词，永不带正文/路径（守卫 12）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derive_drop_reason: Option<&'static str>,
    // 批次 M 段 C：段 B 落在 `DeriveOutcome` 的三个 mention 计数器送到 cycle 读数面（十一 → 十四；E1 再 → 十七）。
    // `derive_mentions_missing` 是"缺第四键"的唯一可观测出口；`derive_mentions_unresolved` 是
    // "名字解析不到实体"的计数。三格都是计数，没有正文/路径/分数（守卫 12 照旧）。
    pub derive_mentions_written: usize,
    pub derive_mentions_unresolved: usize,
    pub derive_mentions_missing: usize,
    // A45：抽取闸门三数（E1）。只加不减，schema_version 仍 = 1。
    pub entities_seen: usize,
    pub kinds_rejected: usize,
    pub aliases_dropped: usize,
}

/// 判定与读数是两件事，所以 `run_cycle_once` 返回这一对而不是 `Result`（§2.19）。
/// `Fault` 载荷只可能是 `&'static str`：任何带路径或带正文的失败原串都不许走到 stderr（守卫 12）。
/// 打印与退出码（0/2/3）**不在这里**：那是 hub 的 `finish_cycle`，这一层只交判定。
pub enum CycleExit {
    Ran,
    BatchDropped,
    Fault(&'static str),
}

/// 三个家族各自的类别串。一个变体一条、零插值、`cycle_failure` 不带 `_` 兜底——
/// 与 `index_failure` 同形：`DistillError` 新增变体时这一层编译不过，逼着这里跟着表态。
pub const FAULT_STORE_READ: &str = "store read failed — run: doctor";
pub const FAULT_STORE_WRITE: &str = "store write failed — run: doctor";
pub const FAULT_COUNTERS: &str = "usage counter write failed — run: doctor";
pub const FAULT_UNQUALIFIED: &str = "store not qualified for extraction — run: doctor";
/// 上面四条都是从既有错误形状折出来的（一个变体一条），这一条不是：attention 扫描没有自己的
/// 错误枚举，`sweep` / `set_one` / `advance` 三支失败都只说明"这一轮的扫描没跑成"，
/// 所以它们共用一条类别串——运维面上要区分是谁挡的，看 stderr 之外还得开新格子，那不是本轮的预算。
pub const FAULT_ANOMALY: &str = "attention scan failed — run: doctor";

/// 守卫 14：资格只在入口判一次，判完把**报告**交给作业，业务层不再判第二次。
/// 做成纯函数（收 `&CapReport`，不收 `&Path`）是为了它**能跑红**——见本文件 `tests` 那一条。
pub fn qualify(r: &CapReport) -> bool {
    r.fts5
        && r.memory_schema == Some(rc_store::ddl::MEMORY_VERSION)
        && r.transcript_schema == rc_store::ddl::TRANSCRIPT_VERSION
        && r.ledger_schema == rc_store::ddl::LEDGER_VERSION
}

/// 只读**本轮处理过的那个会话**的停轮位。`None` 是"本轮根本没有会话可问"（空库那条分支），
/// 此时返回 false 而不是去查别的会话（§2.19 的 A15：那条查询会把一轮的语义从"一个批次"
/// 改成"扫全库"）。
pub fn halted_of(l: &rc_store::LedgerHandle, water_scope: Option<&str>) -> Result<bool, StoreError> {
    match water_scope {
        Some(scope) => water::halted(l, "distill", scope),
        None => Ok(false),
    }
}

/// `DistillError` → 判定。契约族不会走到这里（`run_cycle_once` 先用 `reason_slug()` 把它分流成
/// "整批丢弃"），最后那一支仍然写出来：它挡住的是"新增变体被 `_` 静默兜底"那一型（守卫 12）。
pub fn cycle_failure(e: &DistillError) -> CycleExit {
    CycleExit::Fault(match e {
        DistillError::Llm(LlmFailure::Timeout) => "llm timeout — retry this round later",
        DistillError::Llm(LlmFailure::SchemaMismatch) => "llm reply shape mismatch — run: doctor",
        DistillError::Llm(LlmFailure::QuotaExhausted) => "llm quota exhausted — run: doctor",
        DistillError::Llm(LlmFailure::BackendError) => "llm backend unavailable — check the service",
        DistillError::Memory(_) => FAULT_STORE_WRITE,
        DistillError::Store(_) => FAULT_STORE_WRITE,
        DistillError::Judge(_) => "judge reply malformed — run: doctor",
        DistillError::Audit(_) => "audit reply malformed — run: doctor",
        // S2b-2b：fact pass 的契约拒收（`schema_version=0` 的新契约，不冒充 v4）与旧契约族同一去向。
        // S2c-2：event pass 同族（单一 pass，`family=''`），并进同一臂，行为不变。
        // S3b-3c：naming 窄审的拒收同族（cycle 不跑 naming，这一臂只挡"新增变体被 `_` 静默兜底"）。
        // 这一支今天没有 cycle 侧生产者——加它是因为上面那张逐变体点名的表不许被 `_` 静默兜底。
        // P2：state pass 同族（单一 pass，`family=''`）。cycle 今天不跑 state，这一臂与
        // naming 那一支同一条理由：逐变体点名的表不许被 `_` 静默兜底。
        DistillError::Contract(_) | DistillError::A1(_) | DistillError::Fact(_)
        | DistillError::Event(_) | DistillError::State(_) | DistillError::NamingReview(_)
        | DistillError::EmptyBatch | DistillError::ZeroOutput =>
            "batch rejected by the output contract — see: hub import",
    })
}

/// 四个家族的调用次数在这一处进计数器。`Derive` 从阶段 3 起是 cycle 的第四支作业，
/// 与前三个一样只有开过口才建行。零增量不建行的那条纪律与 `bump_rejects_by_device` 同形：
/// `doctor` 读不到幽灵计数。
pub fn charge_counters(l: &rc_store::LedgerHandle, meter: &Metered, now_ms: i64)
    -> Result<(), &'static str> {
    for f in [JobFamily::Extract, JobFamily::Dedupe, JobFamily::Audit, JobFamily::Derive] {
        let n = meter.calls(f) as i64;
        if n == 0 { continue; }
        counters::bump(l, "llm_job", f.slug(), rc_llm::ALERT_METRIC, n, now_ms)
            .map_err(|_| FAULT_COUNTERS)?;
    }
    Ok(())
}

/// 一轮 = 一个批次（§2.15）。顺序：段 0 前置扫描 → 选批 → 抽取 → 判同 → 并条 → 审核 → 推导 → 重建影子 → 收尾读数。
/// 两支作业之间的 `use_family` 是计数器主语的**唯一**决定点。
///
/// 两个参数是 §4.8 ② 那一次拆分的落点：**`meter` 是账本，`llm` 是来源**。
/// 从前这两个角色由同一个 `Metered` 兼任（`run_extract(.., meter, ..)` 里那个 `&dyn LlmBackend`
/// 就是账本自己），于是"第二条座位来源"没有形状可挂。§4.8 ③ 段 B 之后 `llm` 递进来的是 `Rotating`，
/// 它包住的每一座仍是回放座位（hub 的 `replay::Seat`），落账落的是这里同一个 `Metered`——
/// **座位可以两份脚本，账只许一本**，否则 P-33 那条算术
/// （"计数器总和 == 这一轮真发生的调用次数"）会静默变成"只数回放"。这一格由
/// hub 的 `cycle_once.rs` 里 `giving_replies_takes_the_replay_seat_even_when_a_config_is_present`
/// 那条的 `llm_calls_total` 钉着（把来源接进去、账本忘了接 ⇒ 那一格红）。
/// 一轮抽取默认最多多少 turn；`hub cycle once --batch-turns` 与节点离线轮共用这个默认。
/// A/B 把它调小（10/15）时只影响这一轮的选批，不改 `MAX_TURNS_PER_BATCH` 这个绝对上界。
pub const DEFAULT_BATCH_TURNS: usize = extract::MAX_TURNS_PER_BATCH;

pub fn run_cycle_once(root: &Root, meter: &Metered, llm: &dyn LlmBackend, now_ms: i64,
                      batch_turns: usize) -> (CycleOut, CycleExit) {
    let (m, l, t) = (root.memory(), root.ledger(), root.transcript());
    let mut out = CycleOut { schema_version: CapReport::schema_version(), turns_scanned: 0,
                             rows_written: 0, batches_dropped: 0, quota_exhausted: false,
                             halted: false, merged: 0, suppressed: 0,
                             derive_rows: 0, derive_rejected: 0, derive_dropped: 0,
                             derive_drop_reason: None,
                             derive_mentions_written: 0, derive_mentions_unresolved: 0,
                             derive_mentions_missing: 0,
                             entities_seen: 0, kinds_rejected: 0, aliases_dropped: 0 };
    // 段 0（连带抑制前置扫描）：**每轮 cycle 的第一件事**，与有没有活行、有没有推导批无关
    // （裁定 10 / A42②）。它原先挂在 `rc_derive::derive_pass` 里，而 `run_derive` 在空批上提前
    // return、hub 在契约丢弃轮次整个不跑推导 ⇒ 空闲轮次段 0 根本不执行；更糟的是段 0 退场的行
    // 立刻退出活行集合 ⇒ 它把下一轮的推导池缩小到空 ⇒ 自己饿死，此后新死的依据再也扫不到。
    // 失败即整轮终止，与同函数 `water::note_attempt` 的处置同形；类别串不新增第四个
    // （`DistillError::Memory` / `Store` 两支早就映射到它）。
    if rc_memory::orphans::suppress_orphans(m, now_ms).is_err() {
        return (out, CycleExit::Fault(FAULT_STORE_WRITE));
    }
    // 上提：derive 插入点要看到这一位（原 `if let Some(plan)` 块里的局部变量）
    let mut contract_discarded = false;
    let plan = match extract::select_batch(t, l, batch_turns) {
        Ok(p) => p,
        Err(_) => return (out, CycleExit::Fault(FAULT_STORE_READ)),
    };
    let water_scope: Option<String> = plan.as_ref().map(|p| p.water_scope.clone());
    out.turns_scanned = plan.as_ref().map(|p| p.batch.rows.len()).unwrap_or(0);

    if let Some(plan) = &plan {
        meter.use_family(JobFamily::Extract);
        // S1：指纹在**调 run_extract 之前**算一次。它描述的是"这一批将被谁用哪套 prompt 蒸"，
        // 所以三样输入都取自即将发出的那一次调用：模型身份来自来源（`slug()`，未知写 "unknown"），
        // 两段 prompt 由 `rc-distill` 的 prompt 层现算——**同一个家**，不许在 cycle 里拼第二份。
        let model = llm.slug().unwrap_or("unknown");
        let fingerprint = water::extraction_fingerprint(
            water::EXTRACT_PIPELINE_VERSION, model,
            &rc_distill::prompt::extract_system(),
            &rc_distill::prompt::extract_user(&plan.batch));
        match extract::run_extract(m, l, plan, llm, now_ms) {
            Ok(o) => {
                out.rows_written += o.rows_written;
                out.suppressed += o.suppressed;
                out.entities_seen += o.entities_seen;
                out.kinds_rejected += o.kinds_rejected;
                out.aliases_dropped += o.aliases_dropped;
            }
            Err(e) if e.reason_slug().is_some() => {
                // P-40：`rc_extract_row` 是在记忆库提交之后才清的，契约族错误在它之前就返回，
                // 所以表里躺着的是**上一轮**的行。判同读它 = 把上一批重判一遍、重烧一遍配额。
                // `note_attempt` 唯一的真 caller 就是这一行（P-32：活锁的可见性靠它）。
                contract_discarded = true;
                out.batches_dropped += 1;
                if water::note_attempt(l, "distill", &plan.water_scope).is_err() {
                    return (out, CycleExit::Fault(FAULT_STORE_WRITE));
                }
            }
            // LLM 故障支：没有状态行，也不盖章——它在盖章之前就 return 了（第四态"恢复即续"）。
            Err(e) => return (out, cycle_failure(&e)),
        }
        // 上面两支都写过状态行（成功 ⇒ extracted/pending/worthless；契约族 ⇒ rejected），
        // 所以盖章只在这里有一次：逐轮 UPDATE 已存在的行。写失败按既有故障语义处置，不吞错。
        for row in &plan.batch.rows {
            if water::set_extraction_fingerprint(l, &row.turn_uid, &fingerprint).is_err() {
                return (out, CycleExit::Fault(FAULT_STORE_WRITE));
            }
        }
        if !contract_discarded {
            meter.use_family(JobFamily::Dedupe);
            let plans = match dedupe::run_dedupe(m, l, plan, llm, now_ms) {
                Ok(d) => d.plans,
                Err(e) => return (out, cycle_failure(&e)),
            };
            if !plans.is_empty() {
                match audit::apply_merges(m, &plans, now_ms) {
                    Ok(r) => out.merged = r.merged,
                    Err(e) => return (out, cycle_failure(&e)),
                }
            }
        }
    }
    // 审核在这一支**照跑**，包括上面契约丢弃的那一轮：它的游标是库内的 `verdict` 列，
    // 与派生表无关（§2.15 的"三个作业各有各的推进，互不借道"）。
    meter.use_family(JobFamily::Audit);
    if let Err(e) = audit::run_audit(t, m, llm, now_ms) {
        return (out, cycle_failure(&e));
    }
    // 推导：事实批契约丢弃时**不跑**（§6.4-3"事实批丢弃时推导不烧配额"）
    if !contract_discarded {
        meter.use_family(JobFamily::Derive);
        let (dr, de) = rc_distill::derive::run_derive(m, l, llm, now_ms);
        out.derive_rows += dr.rows;
        out.derive_rejected += dr.rejected;
        out.derive_mentions_written += dr.mentions_written;
        out.derive_mentions_unresolved += dr.mentions_unresolved;
        out.derive_mentions_missing += dr.mentions_missing;
        if dr.dropped {
            out.derive_dropped += 1;
            out.derive_drop_reason = dr.drop_reason;
        }
        if let rc_distill::derive::DeriveExit::Fault(cat) = de {
            return (out, CycleExit::Fault(cat));
        }
    }
    // 重建影子（顺序注释在 derive 段之后；裁定 21① "增量归阶段 4" 改字）
    // cycle 写完行之后影子必须跟着，否则任务 12 的 `similar` 读的是旧影子。
    // 它是全量重建（`rc_index::rebuild` 的语义），一期的库量下无所谓；增量归阶段 4 容量面。
    // 错误面复用 `index_failure`（守卫 3：`IndexError` 的类别串只有一个家）。
    if let Err(e) = rc_index::rebuild(m, now_ms) {
        return (out, CycleExit::Fault(index_failure(&e)));
    }
    match halted_of(l, water_scope.as_deref()) {
        Ok(h) => out.halted = h,
        Err(_) => return (out, CycleExit::Fault(FAULT_STORE_READ)),
    }
    // 先取判定再交读数：写成 `(out, if out.batches_dropped > 0 ..)` 会在元组第一个元素
    // 移走 `out` 之后还读它的字段，E0382（本机实测）。
    let exit = if out.batches_dropped > 0 || out.derive_dropped > 0 {
        CycleExit::BatchDropped
    } else { CycleExit::Ran };
    (out, exit)
}

/// 每多少轮跑一次全量扫描。**首轮必跑**（`scan_due` 的 `last_full == 0` 那一支），于是一个刚初始化
/// 的库在第一轮就有心跳读数——`attention_open: 0` 随之立刻有了"刚扫过"而不是"根本没扫"的含义。
/// 触发轮号是 1, 21, 41, …（差 20）；本文件 `tests` 里那条递推用例逐字钉这一串，
/// 改 `FULL_SCAN_EVERY` 必须同时改那一条，否则"每 N 轮"这句注释与真相分叉。
pub const FULL_SCAN_EVERY: i64 = 20;
/// 全库一条扫描线：`scope_key` 用空串，与 derive 家族同一把约定（`water::read(l, "derive", "")`）。
pub const ANOMALY_SCOPE: &str = "";

/// 本轮该不该跑全量扫描。**纯函数、两个数**，所以它能在本文件 `tests` 里逐字对拍，
/// 不需要真库也不需要读时钟——轮号只能由调用方从账上带进来（判据 12a：取时钟的住在三个 bin 里，
/// 库代码与扫描判据都不许自己读时间）。
pub fn scan_due(rounds: i64, last_full: i64) -> bool {
    last_full == 0 || rounds - last_full >= FULL_SCAN_EVERY
}

/// 从 `usage_counter` 的两个 metric 里取本轮轮号与上次全量轮号。
/// `counters` 没有按 key 的读法（公开面是 `read_all`），所以这里读全表再筛：一共那几行 metric，
/// 比再加一支 `read_one` 便宜，也别在这里缓存（缓存就是给轮号造第二个家）。
pub fn scan_rounds(l: &rc_store::LedgerHandle) -> Result<(i64, i64), StoreError> {
    let rows = rc_ledger::counters::read_all(l)?;
    let get = |metric: &str| rows.iter().find(|r| r.subject_kind == "maintenance"
                                        && r.metric == metric).map(|r| r.count).unwrap_or(0);
    Ok((get("rounds"), get("last_full_scan_round")))
}

/// 一轮扫描：判据现算 → 升/解告警 → **只有扫描成功**才记轮号、才推水位。
///
/// 顺序是有意的：**先 sweep 后 advance**。反过来会让"扫描失败的那一轮"也把游标推过去，
/// 于是下一次扫描跨过这一轮的行——§6 失败矩阵要的是"失败不前进"。而 `last_full_scan_round`
/// 与 `advance` 都只在 `sweep` 返回 `Ok` 之后写，所以失败轮的节奏语义是"下一轮再试"
/// （`last_full` 没动 ⇒ `scan_due` 下一轮仍为真）。
///
/// 返回 `&'static str` 而不是错误原串：这一串会顺着 stderr 出去进运维面（守卫 12），
/// 只许是封闭的类别名。触发判定只在调用方做一次，这里不再自筛一遍。
pub fn run_anomaly_scan(root: &Root, now_ms: i64, rounds: i64) -> Result<(), &'static str> {
    rc_memory::anomaly::sweep(root.memory(), now_ms).map_err(|_| FAULT_ANOMALY)?;
    // §13 A40 的重影作业与矛盾扫描同批、同节奏、同心跳：它读的也是"上一轮到现在的库"，
    // 它的告警与矛盾告警同住一张 `attention`。这里不加第二次触发判定——一轮扫描一次判定。
    // 错误按 `run_anomaly_scan` 的既有形状折成封闭类别名（`&'static str`，守卫 12）：写成
    // `map_err(|e| (FAULT_ANOMALY, e.to_string()))` 编译不过——`?` 要把错误转成 `&'static str`，
    // 元组转不过去（E0277，E57 在 T12 那一行上实测过这一形状；E59 把本节初稿的同一形状改掉）。
    rc_memory::anomaly::sweep_ghosts(root.memory(), now_ms).map_err(|_| FAULT_ANOMALY)?;
    // 复核与扫描同批：**必须挂在 `sweep` 之后**。挂在前面时，本轮新升的告警行还不存在，
    // `recheck_attested` 的 `UPDATE` 一行也看不见（E57 实测：同形状下先跑盖 0 行、后跑盖 1 行），
    // 于是一条"判失败后又被更晚表态复活"的矛盾要白等一整轮才拿到戳。
    // 两者都成功才 `set_one` + `advance`，所以复核失败同样不推水位（§6 失败矩阵同一条）。
    // 戳用同一个 `now_ms`：一轮之内不许有两个"现在"。
    // 错误按 `run_anomaly_scan` 的既有形状折成封闭类别名（`&'static str`，守卫 12）：
    // 写成 `map_err(|e| (FAULT_ANOMALY, e.to_string()))` 编译不过——`?` 要把错误转成
    // `&'static str`，元组转不过去（E0277，E57 实测过这一形状）。
    rc_memory::recheck::recheck_attested(root.memory(), now_ms).map_err(|_| FAULT_ANOMALY)?;
    rc_ledger::counters::set_one(root.ledger(), "maintenance", "", "last_full_scan_round",
                                 rounds, now_ms).map_err(|_| FAULT_ANOMALY)?;
    // 空 `turn_uid` 是这一族的合法形状：扫描不配对任何轮次，而 `water::advance` 的倒退判定
    // 只在"已有的 cursor_turn_uid 非空"时才按 uid 比序——本族两端都写空串，所以单调性只看
    // `at_ms`（这一族的轴是扫描时刻），同毫秒重放不算倒退。
    rc_ledger::water::advance(root.ledger(), "anomaly_scan", ANOMALY_SCOPE,
        &rc_ledger::water::Cursor { at_ms: now_ms, turn_uid: String::new() }, now_ms)
        .map_err(|_| FAULT_ANOMALY)?;
    Ok(())
}

/// 守卫 12（裁定 R18-5）：`rc-index` 的错误面在这里只留**类别**，一个变体一条常量串、零插值。
/// `LayerUnreadable` 单独一条，不与"没建索引"共用消息——那是把"索引层坏了"播报成"你还没跑重建"。
/// 这个 `match` 不带 `_` 兜底：`IndexError` 新增变体时这一层编译不过，逼着这里跟着表态。
/// hub 的 `run_index` 与 `run_cycle_once` 读的是同一个家（`use rc_cycle::index_failure`）。
pub fn index_failure(e: &rc_index::IndexError) -> &'static str {
    use rc_index::IndexError as E;
    match e {
        E::NotBuilt => "index not built — run: hub index rebuild",
        E::LayerUnreadable => "index layer unreadable — run: doctor",
        E::NormMismatch { .. } => "index built by an older tokenizer — run: hub index rebuild",
        E::NoQueryTokens => "index query yields no tokens",
        E::ReadOnly(_) => "index write refused on read-only handle",
        E::Store(_) => "store write failed — run: doctor",
        E::Sql(_) => "sqlite failure — run: doctor",
        E::Memory(_) => "memory read failed — run: doctor",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 全量扫描的触发轮号是_1_21_41() {
        // 递推环：每命中一次就把 `last_full` 推进到那一轮，这才是"连续跑"的形状
        // （一张手写的 match 表只会证明表本身）。这一条就是改 `FULL_SCAN_EVERY` 时的那道报警：
        // 实测把常量从 20 改成 25 ⇒ 本用例 FAILED（左 1,26,51 右 1,21,41），所以"每 N 轮"这句
        // 注释不会悄悄与真相分叉。它盖不到的只有 `FULL_SCAN_EVERY` 上方那句"差 20"的字面描述——
        // 那一格若改常量忘了改，全靠人眼。
        let mut last_full = 0i64;
        let mut due: Vec<i64> = Vec::new();
        for r in 1..=60 {
            if scan_due(r, last_full) { due.push(r); last_full = r; }
        }
        assert_eq!(due, vec![1, 21, 41]);
    }

    #[test]
    fn 落后太多轮_一次扫描只补一轮() {
        // 追平是**一次**扫描的事，不是把中间的轮补回来：`scan_due` 只看"距上次全量够不够 N 轮"，
        // 所以 999 轮没跑与 20 轮没跑走的是同一条路径（一轮扫全量，判据本来就是全量的）。
        assert!(scan_due(999, 1), "久没跑 ⇒ 该跑");
        assert!(!scan_due(999, 999), "同一轮不重复跑");
    }

    fn cap() -> CapReport {
        CapReport {
            schema_version: CapReport::schema_version(),
            arch: "x".into(), os: "y".into(), target: "z".into(),
            sqlite_version: "3".into(), fts5: true, cjk_analyzer: "jieba-rs".into(),
            norm_version: rc_store::capability::NORM_VERSION,
            transcript_schema: rc_store::ddl::TRANSCRIPT_VERSION,
            ledger_schema: rc_store::ddl::LEDGER_VERSION,
            memory_schema: Some(rc_store::ddl::MEMORY_VERSION),
        }
    }

    #[test]
    fn qualify_refuses_the_two_shapes_a_store_can_have() {
        // §2.16 第 3 条把谓词做成纯函数的全部理由：这两支在 `&Path` 那一形上构造不出来
        //（`Root::open` 之前就 Err，而 fts5 是编译选项），判据就成了装饰。
        assert!(qualify(&cap()));
        let mut no_fts = cap();
        no_fts.fts5 = false;
        assert!(!qualify(&no_fts), "fts5 缺席 ⇒ 影子建不出来，cycle 之后的读数面会静默空转");
        let mut replica = cap();
        replica.memory_schema = None;
        assert!(!qualify(&replica), "副本半装态没有可写的中心库");
        // 分词版本漂移**不**拦住 cycle：三支作业全部直读记忆库，一个都不碰影子（P-35），
        // 拦住它是把"该跑一次 hub index rebuild"播报成"cycle 不能跑"。
        let mut stale = cap();
        stale.norm_version = rc_store::capability::NORM_VERSION + 1;
        assert!(qualify(&stale), "不读影子的作业不许被影子版本拦住");
    }
}
