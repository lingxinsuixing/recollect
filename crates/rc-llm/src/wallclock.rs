//! 墙钟预算按**家族**计（原设计 §6：每次调用各取半份会顶爆整周期）。
//! `Derive` 家族本阶段没有读者（归阶段 3），但它住在同一份配置里是 D8 的原文要求：
//! 推导用同一份墙钟配置、独立游标。

use crate::LlmFailure;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobFamily {
    Extract,
    Dedupe,
    Audit,
    Derive,
}

impl JobFamily {
    /// 调用计数读数的主语（`usage_counter.subject_id`，`subject_kind = llm_job`）。
    pub fn slug(self) -> &'static str {
        match self {
            JobFamily::Extract => "extract",
            JobFamily::Dedupe => "dedupe",
            JobFamily::Audit => "audit",
            JobFamily::Derive => "derive",
        }
    }

    /// 档位 = **该家族一轮的调用数 × 单发上界**。调用数不是猜的，`rc-distill` 那边逐字写着：
    /// 一轮（`hub cycle once`）最多 抽取 1 + 判同 4（`dedupe::MAX_DEDUPE_CASES`）+ 审核
    /// 4 行（`audit::MAX_PER_PASS`）× 3 票（`audit::VOTES`，三票不短路）= 12 + 推导
    /// 1（`rc-derive/src/derive.rs:77` 一发）= **18 次**。
    /// 单发上界取 150 秒，来源是真 key 实测的两端：非推理型 1.69–22.5 秒、开思考 7.3–86.5 秒，
    /// 150 秒是"最慢那一档再留一倍"。抽取与推导各只有 1 次调用，但它们的单条 prompt 最长，
    /// 所以独得 300 秒（= 两个单发上界，也就是"一发干活 + 一发余量"）。
    ///
    /// **2026-11-03 更正（缺陷 A）：上面那句"抽取 1 次"只对 `hub cycle once` 那一条路成立**
    /// （那条路上 `run_extract` 确实只 `complete` 一次）。本机在用的是 `hub distill once --pass naming`，
    /// 它是 **2 次/批**：`rc-distill/src/a1_run.rs:29` 的 `run_a1_pair` 无条件顺序跑两遍
    /// （调用点 `pass_naming.rs:736`），这一格由
    /// `rc-distill/tests/a1_run.rs::pair_makes_two_calls_with_the_a1_prompt_and_no_derive_budget`
    /// 钉着（`backend.calls() == 2`）。⇒ 抽取一档的调用数是 2 不是 1，**一轮上限从 18 抬到 19**
    /// （2 + 4 + 12 + 1；判同/审核/推导三档没动，所以只多 1）。
    ///
    /// 连带结论（这条是本注释的主语）：`Extract` 的 300 秒是按"1 次 × 2 倍余量"定的，
    /// 实际要装 **2 次** ⇒ 每发只剩 150 秒 = 单发上界本身 ⇒ **余量为零**。
    /// 对照：`Derive` 的 300 秒仍只装 1 发 ⇒ 余量还是 1 发。
    /// Agnes 主座今天不受这一格咬（非思考档单发下界 2.8 秒 ⇒ 两发 5.6 秒；本批 naming 双跑合计的
    /// 中位 23.0 秒 ⇒ 单跑中位 ≈11.5 秒）；
    /// 但下面那条 2026-10-16 的读数说思考型座位单发 71–118 秒（吃满 32_000 输出预算时要 200 秒以上）
    /// ⇒ 两发 = 142–236 秒（只剩 64–158 秒），两发 = 400 秒那种直接爆 300 秒。
    /// 爆的形状不是慢，是判 `Timeout` 后整批不落库（`run_a1_pair` 第一跑坏不再烧第二跑、
    /// 第二跑坏则这一批整体不落，见 `a1_run.rs` 的档头）。
    ///
    /// **读数出处**（写进本注释的每个数都来自这一批，n=1 一个批次，口径见下；唯一例外是
    /// "非思考档单发下界 2.8 秒"——那是既有单发读数 2808 毫秒，与本批各 fact pass 的
    /// min 2729–2792 毫秒同一界，故可并用）：
    /// run id `extract-jia-2026-11-03`（`D:\recollect\.local\extract-jia-2026-11-03\READINGS.md`）、
    /// 代码基线 `72322f8`、产出时间 2026-11-03 13:49→14:11 +0800（墙钟 1131.9s）、
    /// `--batch-turns 8`、座位 Agnes `agnes-3.0-flash`（主座，本批未见切换证据）。
    /// 本批 **88 次抽取 LLM 调用**（账本 `usage_counter` 的 `llm_job`：naming 28 / fact 45 / event 15），
    /// 另有 **26 次审核调用不记账** ⇒ 真实烧 114 次。
    /// **分母口径**：这 88 次覆盖的是 **103 个入队轮**（`extract_pass_state` 每个 pass 103 格
    /// = human 18 + agent 85；`role='system'` 的 258 轮结构上不进队列——transcript.db 实测
    /// 361 = 18 + 85 + 258），**不是 361**。次/done：naming 0.56、event 0.52、fact 22.5
    /// （fact 的 done 只有 2 行，这一格不稳）。
    /// naming 的 2× 在本批精确复现：28 ÷ 14 个 `turns>0` 的实质批 = **2.00**；
    /// 除以 CLI 次数 16 得 1.75 是**错分母**（其中 2 次 `turns=0` 空跑，不烧 LLM）。
    ///
    /// **本注释不改值**：`budget_ms()` 四个常量一个没动（改它 = 改行为，属 §8 拍板项 P4）。
    /// 而且要说明的是这一族今天**没有生产侧读者**：出网座位的单发墙是
    /// `rc_llm_http::transport::CALL_WALL_MS`（300 秒，hub W19c1 / node W19d 同源），
    /// 家族预算不是任何一次调用的超时；`cycle_budget_ms` 只是"一轮最多烧多少"的账。
    /// ⇒ 上面关于"300 秒装两发"的论证是**设计口径对拍**，
    /// 不是对现网一次调用实际能跑多久的断言。
    ///
    /// 抬档是用户 2026-10-16 的裁定（原话见实施计划 §4 那条"质量优先"），代价写在
    /// `cycle_budget_ms`：最坏一轮从 160 秒抬到 50 分钟。抽取与维护都在后台，不计时。
    /// "拐点实测前不许当结论引用"那半句仍然成立——这组数买的是成功率，不是测出来的拐点。
    ///
    /// **但"单发上界 150 秒"这一格已被 2026-10-16 的真实批次读数推翻**（台账 §10(34)）：
    /// 把 16 条真人发言（10.4k 字符）喂给 MiniMax 两档，思考**不是问出来的而是自发的**，
    /// 长度 15k–67k 字符乱跳；16_000 预算那一发跑到 **116–118 秒仍被截成空正文**，32_000 那一发
    /// 71.5 秒完成。⇒ 思考吃满 32_000 时单发要 200 秒以上，150 秒不够（不够的后果是判 `Timeout`、
    /// 同形重发一发、整个家族预算被一发吃掉）。这里是 n=1，**不许照抄成一个新常数**：
    /// 补重复采样后按（座位档，输出预算）重定这一格，落点 A47-2 装配前。
    pub fn budget_ms(self) -> i64 {
        match self {
            JobFamily::Extract => 300_000,
            JobFamily::Dedupe => 600_000,
            JobFamily::Audit => 1_800_000,
            JobFamily::Derive => 300_000,
        }
    }
}

/// `hub cycle once` 的整周期预算 = 四家族之和（Global Constraints"下游 ≥ 上游预算之和"）。
pub fn cycle_budget_ms() -> i64 {
    JobFamily::Extract.budget_ms() + JobFamily::Dedupe.budget_ms()
        + JobFamily::Audit.budget_ms() + JobFamily::Derive.budget_ms()
}

#[derive(Debug, Clone, Copy)]
pub struct Deadline {
    start_ms: i64,
    budget_ms: i64,
}

impl Deadline {
    pub fn new(family: JobFamily, start_ms: i64) -> Deadline {
        Deadline { start_ms, budget_ms: family.budget_ms() }
    }
    /// 时钟从形参进来（裁定 V14），本模块一次都不取系统时间。
    pub fn remaining_ms(&self, now_ms: i64) -> i64 {
        (self.start_ms + self.budget_ms - now_ms).max(0)
    }
    pub fn expired(&self, now_ms: i64) -> bool {
        now_ms >= self.start_ms + self.budget_ms
    }
    pub fn budget_ms(&self) -> i64 {
        self.budget_ms
    }
    /// 超预算时的唯一出口：一个失败态，不是一个 panic。
    pub fn failure(&self) -> LlmFailure {
        LlmFailure::Timeout
    }
}
