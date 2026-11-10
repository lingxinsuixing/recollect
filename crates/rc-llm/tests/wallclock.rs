use rc_llm::{Deadline, JobFamily};

#[test]
fn budget_is_per_family_and_audit_is_the_largest() {
    for f in [JobFamily::Extract, JobFamily::Dedupe, JobFamily::Audit, JobFamily::Derive] {
        assert!(f.budget_ms() > 0, "{:?} 预算必须为正", f);
    }
    // 2026-10-16 抬档（用户裁定：质量优先，不为省 token 或省时间设限）之后，"谁最大"的主语换了：
    // 原来是单条 prompt 最长的 `Extract`，现在是**一轮调用数最多**的 `Audit`（12 次票）。
    // 档位 = 家族调用数 × 单发上界，所以这三条排序就是那条公式的形状。
    assert!(JobFamily::Audit.budget_ms() >= JobFamily::Dedupe.budget_ms());
    assert!(JobFamily::Dedupe.budget_ms() >= JobFamily::Extract.budget_ms());
    assert_eq!(JobFamily::Extract.budget_ms(), JobFamily::Derive.budget_ms());
    // Global Constraints 那句"`hub cycle once` 的墙钟 = 四个作业家族预算之和"在本阶段唯一的牙。
    // 它不加第十个用例（Step 4 的 9 条是钉死的），只把这条等式挂进已经在场的那一条：
    // 任何一个家族的预算被单独改动、或 `cycle_budget_ms` 漏掉一个家族，这一行就红。
    assert_eq!(
        rc_llm::cycle_budget_ms(),
        JobFamily::Extract.budget_ms() + JobFamily::Dedupe.budget_ms()
        + JobFamily::Audit.budget_ms() + JobFamily::Derive.budget_ms()
    );
    // 绝对值那一格是"等式对了"之外的第二道：它把四个家族的预算之和钉成一个具体数，
    // 于是"四项各改一点、总和恰好抵消"这一类也红。3_000_000 ms = 最坏一轮 50 分钟。
    assert_eq!(rc_llm::cycle_budget_ms(), 3_000_000);
}

#[test]
fn remaining_is_never_negative() {
    let d = Deadline::new(JobFamily::Dedupe, 1_000);
    assert_eq!(d.remaining_ms(1_000 + d.budget_ms() + 50_000), 0);
    assert!(d.expired(1_000 + d.budget_ms() + 1));
    assert!(!d.expired(1_000));
}

#[test]
fn deadline_never_reads_the_clock() {
    // 裁定 V14（阶段 1）：判定函数一律收 now_ms 形参，全仓 src 里一个取系统时钟的调用都不许有。
    // 这一行原来写的是那个 API 的字面形状，被阶段 1 判据 12a 的 grep 当命中读（它扫的是源码文本，
    // 分不清"注释里提到"与"代码里真的取"）；计划 R126 给过的纪律是**改注释，不动正则、不加排除项**。
    // rc-llm 是新增 crate，这条必须在它落地当天守住，不能等任务 13 的脚本才发现。
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/wallclock.rs")).unwrap();
    assert!(!src.contains("SystemTime"), "wallclock.rs 里出现了取时钟代码 ⇒ 判定不再可复现");
    assert!(!src.contains("Instant"), "同上：Instant::now 也是取时钟的一种写法");
}
