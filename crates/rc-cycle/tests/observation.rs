//! W33-A10 的观测账（`Metered` 的两格新读数 + `MeteredBackend` 的 Timeout 落账）。
//! 与 `no_local_call_cap.rs` 同族：账本层零网络、零库，判据只碰内存。

use rc_cycle::{Metered, MeteredBackend};
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use std::rc::Rc;

fn prompt() -> Prompt {
    Prompt { system: "S".into(), user: "U".into(), derive: false }
}

/// 账本颁发观测格，写进格里的数从读口看得见；两发取 max（口径是"最慢一发"）。
#[test]
fn call_ms_cell_round_trips_and_keeps_the_max() {
    let m = Metered::new();
    assert_eq!(m.max_call_ms(), 0, "没有真实出网 ⇒ 0，不是 -1 也不是缺键");

    let cell = m.call_ms_cell();
    cell.note(300);
    assert_eq!(m.max_call_ms(), 300);
    cell.note(120); // 更慢的一发在前面 ⇒ 后写的小数不许覆盖（max 在格子里，不靠自律）
    assert_eq!(m.max_call_ms(), 300);

    // 装配点拿到的格与账本内部是同一份内存（不是颁发出一个副本）。
    let again = m.call_ms_cell();
    again.note(900);
    assert_eq!(m.max_call_ms(), 900);
}

/// `MeteredBackend` 把真后端上交的 `Timeout` 记进账：timeout 枚举的透传，不造不发。
#[test]
fn a_backend_timeout_is_recorded_in_the_ledger() {
    struct Timeouts;
    impl LlmBackend for Timeouts {
        fn complete(&self, _p: &Prompt) -> Result<String, LlmFailure> {
            Err(LlmFailure::Timeout)
        }
        fn slug(&self) -> Option<&str> { Some("stub") }
    }
    let m = Rc::new(Metered::new());
    let b = MeteredBackend::new(Box::new(Timeouts), m.clone());
    assert_eq!(m.timeouts(), 0);
    let got = b.complete(&prompt());
    assert_eq!(got, Err(LlmFailure::Timeout));
    assert_eq!(m.timeouts(), 1);
    let _ = b.complete(&prompt());
    assert_eq!(m.timeouts(), 2, "只累加、不判断：账房不设闸");
}

/// 别的失败态不冒充超时：`BackendError` 上交时 `timeouts` 纹丝不动（观测面分得开两类事故）。
#[test]
fn a_backend_error_is_not_counted_as_a_timeout() {
    struct Errors;
    impl LlmBackend for Errors {
        fn complete(&self, _p: &Prompt) -> Result<String, LlmFailure> {
            Err(LlmFailure::BackendError)
        }
    }
    let m = Rc::new(Metered::new());
    let b = MeteredBackend::new(Box::new(Errors), m.clone());
    assert!(b.complete(&prompt()).is_err());
    assert_eq!(m.timeouts(), 0);
}
