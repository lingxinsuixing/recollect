//! `Rotating` 的三张表（前进／不许前进／恢复）在**纯逻辑层**测（A47 节 5）：零 HTTP 桩、零临时目录、零时钟。
//! `now_ms` 一律是注入的常数（裁定 V14），所以"距上次探测 299 秒 vs 300 秒"这一格是**算出来的**，
//! 不是等出来的。

use rc_llm::{LlmBackend, LlmFailure, Prompt, Rotating, Seed, PROBE_SUCCESSES_TO_RECOVER};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

/// 一个座位：按脚本返回，并把每一次调用记进**共享**的账本。
/// 用 `Rc<RefCell<…>>` 而不是 `MockBackend`：`MockBackend` 的 `calls()` 在 `Box<dyn LlmBackend>`
/// 里取不回来，而这三张表的一半判据是"**另一个座位一次都没被调用**"。
struct Spy {
    tag: &'static str,
    script: RefCell<VecDeque<Result<String, LlmFailure>>>,
    log: Rc<RefCell<Vec<String>>>,
}

impl Spy {
    fn new(tag: &'static str, script: Vec<Result<String, LlmFailure>>,
           log: Rc<RefCell<Vec<String>>>) -> Spy {
        Spy { tag, script: RefCell::new(script.into()), log }
    }
}

impl LlmBackend for Spy {
    fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
        self.log.borrow_mut().push(self.tag.to_string());
        // 耗尽 ⇒ `Timeout`（与 `mock.rs` 同一条语义）：绝不返回 `Ok(空串)`，那会被下游读成"库里没有"。
        self.script.borrow_mut().pop_front().unwrap_or(Err(LlmFailure::Timeout))
    }
}

fn ok(s: &str) -> Result<String, LlmFailure> {
    Ok(s.to_string())
}
fn bad(f: LlmFailure) -> Result<String, LlmFailure> {
    Err(f)
}

/// 造一台 `Rotating`，同时拿到那本共享账本。
fn rig(scripts: Vec<(&'static str, Vec<Result<String, LlmFailure>>)>)
    -> (Rotating, Rc<RefCell<Vec<String>>>) {
    let log = Rc::new(RefCell::new(Vec::new()));
    let slots = scripts
        .into_iter()
        .map(|(tag, s)| (tag.to_string(),
                         Box::new(Spy::new(tag, s, log.clone())) as Box<dyn LlmBackend>))
        .collect();
    (Rotating::new(slots).unwrap(), log)
}

fn prompt() -> Prompt {
    Prompt { system: "s".into(), user: "u".into(), derive: false }
}

fn calls(log: &Rc<RefCell<Vec<String>>>, tag: &str) -> usize {
    log.borrow().iter().filter(|t| t.as_str() == tag).count()
}

// ---- 表一：前进 ----

#[test]
fn backend_error_advances_and_redoes_the_same_call_on_the_next_slot() {
    let (r, log) = rig(vec![("a", vec![bad(LlmFailure::BackendError)]), ("b", vec![ok("B")])]);
    assert_eq!(r.complete(&prompt()).unwrap(), "B", "本次调用必须在新家重做，不是把失败直接报上去");
    assert_eq!(calls(&log, "a"), 1);
    assert_eq!(calls(&log, "b"), 1);
    let s = r.serving();
    assert_eq!((s.slug.as_str(), s.index, s.failed_over), ("b", 1, 1), "{s:?}");
}

#[test]
fn timeout_is_retried_as_is_before_advancing() {
    let (r, log) = rig(vec![("a", vec![bad(LlmFailure::Timeout), bad(LlmFailure::Timeout)]),
                            ("b", vec![ok("B")])]);
    assert_eq!(r.complete(&prompt()).unwrap(), "B");
    assert_eq!(calls(&log, "a"), 2, "Timeout 的处置是先原样重试一次");
    assert_eq!(calls(&log, "b"), 1);
    assert_eq!(r.serving().failed_over, 1);
}

#[test]
fn timeout_that_heals_does_not_advance() {
    let (r, log) = rig(vec![("a", vec![bad(LlmFailure::Timeout), ok("A")]), ("b", vec![])]);
    assert_eq!(r.complete(&prompt()).unwrap(), "A");
    assert_eq!(calls(&log, "a"), 2);
    assert_eq!(calls(&log, "b"), 0, "自愈的那一家不该被换掉");
    let s = r.serving();
    assert_eq!((s.slug.as_str(), s.failed_over), ("a", 0), "{s:?}");
}

#[test]
fn single_schema_mismatch_does_not_advance() {
    // 瞬时契约逃逸（实测 3.8%）会自愈：单次就前进 ⇒ 一家好家被一次坏回复换掉。
    let (r, log) = rig(vec![("a", vec![bad(LlmFailure::SchemaMismatch)]), ("b", vec![ok("B")])]);
    assert_eq!(r.complete(&prompt()).unwrap_err(), LlmFailure::SchemaMismatch);
    assert_eq!(r.serving().index, 0, "单次脱约不许前进");
    assert_eq!(calls(&log, "b"), 0, "不许把同一次调用搬到另一家重做");
}

#[test]
fn three_consecutive_schema_mismatches_advance() {
    let (r, log) = rig(vec![("a", vec![bad(LlmFailure::SchemaMismatch); 3]), ("b", vec![ok("B")])]);
    let p = prompt();
    assert_eq!(r.complete(&p).unwrap_err(), LlmFailure::SchemaMismatch);
    assert_eq!(r.complete(&p).unwrap_err(), LlmFailure::SchemaMismatch);
    assert_eq!(r.serving().index, 0);
    assert_eq!(r.complete(&p).unwrap(), "B", "连续第 3 次 ⇒ 视为这家坏了");
    assert_eq!(r.serving().index, 1);
    assert_eq!(calls(&log, "a"), 3);
}

// ---- 表二：唯一不许前进的那一种 ----

#[test]
fn quota_exhausted_never_advances_and_touches_no_other_slot() {
    // 判据 N7 的行为面。`Rotating` 不持任何调用账，所以这一条钉的是"万一有后端自己回了
    // 这个态"（provider 侧额度错映射）时不许悄悄换家——换家就等于把同一次失败搬到另一家重做。
    let (r, log) = rig(vec![("a", vec![bad(LlmFailure::QuotaExhausted)]), ("b", vec![ok("B")])]);
    assert_eq!(r.complete(&prompt()).unwrap_err(), LlmFailure::QuotaExhausted);
    assert_eq!(r.serving().index, 0, "额度耗尽那一本账不许被搬到下一家");
    assert_eq!(calls(&log, "b"), 0, "不许前进，更不许在新家重做");
    assert_eq!(r.serving().failed_over, 0);
}

#[test]
fn empty_provider_list_fails_loudly_at_construction() {
    assert!(Rotating::new(vec![]).is_err(), "空列表必须在构造点响亮失败");
}

// ---- 表三：恢复 ----

/// 先把它降级（a 坏、b 好；a 的后续脚本是探针要吃的三次正常）。
fn degraded() -> (Rotating, Rc<RefCell<Vec<String>>>) {
    rig(vec![("a", vec![bad(LlmFailure::BackendError), ok("A1"), ok("A2"), ok("A3")]),
             ("b", vec![ok("B"), ok("B"), ok("B")])])
}

#[test]
fn probe_is_not_issued_while_healthy() {
    let (r, log) = rig(vec![("a", vec![ok("A")]), ("b", vec![ok("B")])]);
    assert!(!r.probe_primary(10_000, &prompt()));
    assert_eq!(calls(&log, "a"), 0, "没降级就不许发探针（探针是真调用、真花钱）");
    assert_eq!(r.serving().next_probe_at_ms, None, "未降级时这一格读作\"没有待做的探测\"");
}

#[test]
fn first_probe_after_degradation_fires_and_the_next_is_gated_by_300s() {
    let (r, log) = degraded();
    let p = prompt();
    r.complete(&p).unwrap(); // a 的 BackendError ⇒ 前进到 b
    assert_eq!(r.serving().index, 1);
    assert!(!r.probe_primary(1_000, &p), "第一次探测成功但没凑够 3 次 ⇒ 不切回");
    assert_eq!(calls(&log, "a"), 2, "降级后第一个 tick 就探测（此前没有\"上次探测\"可等）");
    r.probe_primary(1_300_000, &p); // 第 2 次，距上次 1_299_000 毫秒
    let before = calls(&log, "a");
    assert!(!r.probe_primary(1_599_999, &p), "距上次探测 299.999 秒不许发");
    assert_eq!(calls(&log, "a"), before, "299 秒那一支真的一个请求都没发");
    assert_eq!(r.serving().next_probe_at_ms, Some(1_600_000), "下一次探测时刻要读得出来");
    assert!(r.probe_primary(1_600_000, &p), "第 3 次正常 ⇒ 这一次就切回主家");
    assert_eq!(calls(&log, "a"), before + 1, "300 秒整必须发一次");
    assert_eq!(r.serving().index, 0, "连续 3 次正常 ⇒ 切回主家");
    assert_eq!(r.serving().next_probe_at_ms, None, "切回之后没有待做的探测");
}

#[test]
fn a_failed_probe_clears_the_success_streak() {
    // 脚本：降级那一次失败，之后 ok / 失败 / ok / ok。没有中间那一次失败的话，
    // 第 3 个 ok 就该切回主家；有了它，四个 ok 只攒到 3−1 档 ⇒ 切回不生效。
    let (r, log) = rig(vec![("a", vec![bad(LlmFailure::BackendError), ok("A"),
                                        bad(LlmFailure::BackendError), ok("A"), ok("A")]),
                             ("b", vec![ok("B")])]);
    let p = prompt();
    r.complete(&p).unwrap();
    assert_eq!(r.serving().index, 1);
    for t in [3_000_000, 3_300_000, 3_600_000, 3_900_000] {
        r.probe_primary(t, &p);
    }
    assert_eq!(r.serving().index, 1, "中间那次失败把成功计数清零了 ⇒ 不许切回");
    assert_eq!(calls(&log, "a"), 5, "4 次探测 + 1 次触发降级的那一次");
    assert_eq!(r.serving().probe_failures, 0, "最后两次都成功 ⇒ 连续失败数已清零");
    assert_eq!(r.serving().next_probe_at_ms, Some(4_200_000));
}

#[test]
fn exhausted_list_returns_the_existing_failure_state() {
    // 列表走完 ⇒ 复用它已有的那一态（`BackendError`），不新造第五种状态：
    // hub 的 `cycle_failure` 因此照旧走"停新蒸馏 + 视图照开 + 双向告警"那三格。
    let (r, log) = rig(vec![("a", vec![bad(LlmFailure::BackendError); 4]),
                            ("b", vec![bad(LlmFailure::BackendError); 4])]);
    assert_eq!(r.complete(&prompt()).unwrap_err(), LlmFailure::BackendError);
    assert_eq!(calls(&log, "a"), 2);
    assert_eq!(calls(&log, "b"), 2);
    assert_eq!(r.serving().failed_over, 4, "走完一圈 = 四次前进，读数要如实报");
}

#[test]
fn recovery_waits_for_the_whole_streak() {
    let (r, _log) = degraded();
    let p = prompt();
    r.complete(&p).unwrap();
    for i in 0..PROBE_SUCCESSES_TO_RECOVER - 1 {
        assert!(!r.probe_primary(i as i64 * 300_000, &p), "第 {} 次成功就切回 ⇒ 闸门没生效", i + 1);
    }
    assert_eq!(r.serving().index, 1, "凑够次数之前必须还待在备用家");
    assert_eq!(r.serving().probe_ok, 2, "降级后连探两次成功 ⇒ 计数读数跟着攒（这一格是 doctor 与落盘的同一来源）");
}

// ---- 轮转态落盘：`seeded` / `persisted`（设计档节 4 第 7 条） ----

/// 手造一份种。`probe_failures` 写死 0 是刻意的：下面四条用例都不测种里的这一格，
/// 不许因此给 `Seed` 加 `Default` 或构造函数。
fn seed(slug: &str, due: Option<i64>, ok: u32, written_at_ms: i64) -> Seed {
    Seed { slug: slug.into(), probe_due_at_ms: due, probe_ok: ok, probe_failures: 0, written_at_ms }
}

/// `seeded` 要的裸座位列表就地造：0 号主家 `primary`、1 号备用 `backup`。
/// `Spy` 照 `rig` 的装法塞进 `Box<dyn LlmBackend>`——不为测试改 `src/` 的任何可见性。
fn two_houses(log: Rc<RefCell<Vec<String>>>) -> Vec<(String, Box<dyn LlmBackend>)> {
    vec![
        ("primary".to_string(),
         Box::new(Spy::new("primary", vec![ok("p"), ok("p"), ok("p")], log.clone())) as Box<dyn LlmBackend>),
        ("backup".to_string(),
         Box::new(Spy::new("backup", vec![], log.clone())) as Box<dyn LlmBackend>),
    ]
}

// 有牙的那一条：整条机制的存在理由就是这一条。
#[test]
fn a_seeded_streak_needs_only_the_remaining_probes_not_a_fresh_three() {
    // 降级态由真实行为产出（不许测试后门）：`complete()` 撞 `BackendError` 自己前进到备用家，
    // 再连探两次成功攒到 `probe_ok = 2`；它的 `persisted()` 就是种——上是真进程、下是新进程。
    let (prev, _prev_log) = rig(vec![
        ("primary", vec![bad(LlmFailure::BackendError), ok("p1"), ok("p2")]),
        ("backup", vec![ok("b1")]),
    ]);
    let p = prompt();
    prev.complete(&p).unwrap();
    assert_eq!(prev.serving().index, 1, "{:?}", prev.serving());
    prev.probe_primary(1_000, &p);
    prev.probe_primary(301_000, &p);
    let st = prev.persisted();
    assert_eq!(st.probe_ok, 2, "{st:?}");
    let seed = Seed { slug: st.slug, probe_due_at_ms: st.probe_due_at_ms, probe_ok: st.probe_ok,
                      probe_failures: st.probe_failures, written_at_ms: 301_000 };

    let log = Rc::new(RefCell::new(Vec::new()));
    let r = Rotating::seeded(two_houses(log.clone()), &seed, 601_000).unwrap();
    assert_eq!(r.serving().slug, "backup");
    assert!(r.probe_primary(601_000, &p), "种里已有两次正常 ⇒ 这第三次就该切回");
    assert_eq!(r.serving().slug, "primary");
    assert_eq!(calls(&log, "primary"), 1);
}

#[test]
fn a_seed_naming_a_seat_that_is_gone_is_treated_as_empty() {   // 作废规则第 2 条
    let log = Rc::new(RefCell::new(Vec::new()));
    let r = Rotating::seeded(two_houses(log.clone()),
                             &seed("deleted-provider", None, 2, 1_000), 2_000).unwrap();
    assert_eq!(r.serving().index, 0, "认不到的 slug 不许按下标沿用——改过顺序的列表会指到别家");
    assert!(!r.probe_primary(3_000, &prompt()));
    assert_eq!(calls(&log, "primary"), 0, "空种 = 健康态，探针一发都不许发");
}

#[test]
fn a_seed_written_by_a_clock_ahead_of_this_one_is_discarded() { // 作废规则第 3 条
    let log = Rc::new(RefCell::new(Vec::new()));
    let r = Rotating::seeded(two_houses(log), &seed("backup", Some(9_000), 1, 10_000), 5_000).unwrap();
    assert_eq!(r.serving().index, 0, "written_at_ms 比 now 还靠后 ⇒ 本机钟倒退过，整行作废");
}

#[test]
fn a_seed_holds_the_throttle_across_processes() {               // 跨进程那半句"间隔不低于 5 分钟"
    let log = Rc::new(RefCell::new(Vec::new()));
    let r = Rotating::seeded(two_houses(log.clone()), &seed("backup", Some(9_000), 0, 5_000), 8_999).unwrap();
    assert!(!r.probe_primary(8_999, &prompt()), "还没到期的判断必须跟着行活过进程边界");
    assert_eq!(calls(&log, "primary"), 0, "没到点一发都不许发");
    {
        // 强断言（不是恒真的或式）：到点那一 tick 恰好发一发。
        r.probe_primary(9_000, &prompt());
        assert_eq!(calls(&log, "primary"), 1, "到点那一 tick 恰好发一发");
    }
    assert_eq!(r.serving().index, 1, "只成一次 ⇒ 不切回，仍待备用家");
}

#[test]
fn a_seed_naming_the_seat_that_is_already_serving_is_treated_as_empty() {
    // `seeded` 的第三个条件（`seed.slug != slots[0].0`）：种里本来就是主家 ⇒ 当空种。
    // 这一条**不在设计档节 4 第 7 条那四条里**，是 `seeded` 自己加的，所以必须由用例钉成合同而不是靠注释：
    // `persisted()` 保证主家的种是 `None`/0，但表里那一行还可能来自**改过顺序的座位列表**
    // （Task 7 之后 `llm.toml` 挪一行就够），那时 0 号座位上的 slug 会带着上一世的连击次数回来。
    let log = Rc::new(RefCell::new(Vec::new()));
    let r = Rotating::seeded(two_houses(log.clone()),
                             &seed("primary", Some(9_000), 2, 1_000), 2_000).unwrap();
    assert_eq!(r.serving().index, 0);
    assert_eq!(r.serving().probe_ok, 0, "主家的种不许把连击次数带进来——带着 2 回来 ⇒ 本进程一降级就只欠一发");
    assert!(!r.probe_primary(3_000, &prompt()), "主家态下探针一发都不许发");
    assert_eq!(calls(&log, "primary"), 0);
}

#[test]
fn a_seed_over_an_empty_slot_list_is_refused_not_panicked() {
    // `known` 那一支不是冗余：`inherit` 的第三个条件要读 `slots[0]`，空列表会在**判**的时候越界。
    // 变异 S-B 的字面写法（删掉 `known`）在四条种子用例上一条都不红——`position()` 认不到时本来就不继承，
    // 两条路径的可观察后果相同。红的是这一条：Task 7 的装配面会把 `llm.toml` 里读出的空列表递进来。
    // 且删 `known` 能走到越界，前提是本用例的种满足 `written_at_ms <= now_ms`：`&&` 是短路的，
    // 钟那一支若先为假就当空种返回，永远碰不到 `slots[0]`——变异放的位置决定它红不红。
    let err = Rotating::seeded(Vec::new(), &seed("backup", None, 0, 1_000), 2_000)
        .err()
        .expect("空座位列表必须是 Err，不许 panic");
    assert_eq!(err, "empty provider list");
}
