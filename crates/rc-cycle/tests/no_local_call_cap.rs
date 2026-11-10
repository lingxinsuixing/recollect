//! 反向守卫：账本**只累加、不设闸**（2026-11-06 用户裁定：调用配额是预算，砍掉）。
//!
//! 这一份是"删对了"的证人，不是"删干净了"的证人：把那条本地次数上限加回去（不管是加在
//! `record_call` 里、还是加在 `MeteredBackend::complete` 的第一行），第一条与第二条用例当场红。
//!
//! 反过来，第三条钉住**没有一起砍掉**的那件事：后端自己回 `QuotaExhausted`（供应商额度耗尽）
//! 仍然是真故障信号，`saw_quota_exhausted()` 必须置位。砍本地配额不等于砍掉供应商的额度信号。
//!
//! 三条都用 `MockBackend`：不出网、不碰库、不读钟。

use rc_cycle::{Metered, MeteredBackend};
use rc_llm::mock::MockBackend;
use rc_llm::{JobFamily, LlmBackend, LlmFailure, Prompt};
use std::rc::Rc;

/// 旧上限是 40。用例一律越过它一格以上，否则"闸还在"这件事看不见。
const BEYOND_THE_OLD_CAP: usize = 45;

fn p() -> Prompt {
    Prompt { system: "s".into(), user: "u".into(), derive: false }
}

#[test]
fn a_ledger_records_far_past_the_old_cap_and_stays_cold() {
    // 最裸的一条：直接往账上记 45 次。旧的入口闸门在这里会拦下第 41 次并置位，
    // 现在没有闸，于是次数照记、格子照冷。
    let m = Metered::new();
    m.use_family(JobFamily::Extract);
    for n in 1..=BEYOND_THE_OLD_CAP {
        m.record_call();
        assert!(!m.saw_quota_exhausted(), "记账第 {n} 次之后本进程自己点了额度耗尽：本地闸又回来了");
    }
    assert_eq!(m.calls(JobFamily::Extract), BEYOND_THE_OLD_CAP, "记账只许累加，一次都不许少");
}

#[test]
fn forty_five_real_calls_all_reach_the_backend() {
    // 经过座位那一层再走一遍：`MeteredBackend` 若在调用前判上限，第 41 次就会拿到
    // `QuotaExhausted` 而不去问后端——那一次 `unwrap()` 当场红。
    let m = Rc::new(Metered::new());
    m.use_family(JobFamily::Audit);
    let script: Vec<Result<String, LlmFailure>> =
        (0..BEYOND_THE_OLD_CAP).map(|_| Ok("ok".to_owned())).collect();
    let backend = MeteredBackend::new(Box::new(MockBackend::scripted(script)), Rc::clone(&m));
    for n in 1..=BEYOND_THE_OLD_CAP {
        assert_eq!(backend.complete(&p()).unwrap(), "ok", "第 {n} 次调用被本地闸拦下了");
    }
    assert_eq!(m.calls(JobFamily::Audit), BEYOND_THE_OLD_CAP, "每一次真开口都要进账");
    assert!(!m.saw_quota_exhausted(), "一路答得出话的 45 次不许被读成额度耗尽");
}

#[test]
fn a_provider_quota_error_still_sets_the_cell() {
    // 砍的是本地次数上限，不是供应商额度信号：后端回 `QuotaExhausted` ⇒ 照旧置位、照旧上抛、
    // 照旧算一次调用（它真开过口）。
    let m = Rc::new(Metered::new());
    m.use_family(JobFamily::Extract);
    let backend = MeteredBackend::new(
        Box::new(MockBackend::scripted(vec![Err(LlmFailure::QuotaExhausted)])),
        Rc::clone(&m),
    );
    assert_eq!(backend.complete(&p()).unwrap_err(), LlmFailure::QuotaExhausted);
    assert!(m.saw_quota_exhausted(), "供应商额度耗尽必须置位：这一格是 status 与 doctor 的同一来源");
    assert_eq!(m.calls(JobFamily::Extract), 1, "开过口（哪怕答的是失败）就要进账");
}
