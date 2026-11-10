//! W33-A10 的观测写口（`Live::with_call_ms`）：transport 侧的单调钟 `slow_ms` 透传进装配点递
//! 进来的观测格。零网络：出网点是计数桩（与 `codec.rs` 同一族桩，形状逐字不同源）。
//!
//! 判据三条：
//! ① 成功面：桩给的 `slow_ms` 原样进格（取 max）——透传的是 transport 掐的表，不是这里再造一个；
//! ② 失败面：出网在拿回 `Raw` 之前就失败（超时那一族），没有 `slow_ms` 可传 ⇒ 就地用同一族
//!    单调钟补测——300s 被切的那一发不许从读数里消失（桩睡一小段，断格子拿到了那段）；
//! ③ 缺省面：不调 `with_call_ms` 的 `Live` 与改造前同形——观测面关着，一次出网照常完成。

use rc_llm::{LlmBackend, LlmFailure, MaxCell, Prompt};
use rc_llm_http::{Egress, Live, Raw};
use std::rc::Rc;
use std::time::Duration;

const REPLY_OK: &str = include_str!("fixtures/openai-chat-200-reply.json");

/// 桩：按脚本交回 `Ok(Raw{slow_ms})` 或 `Err(LlmFailure)`；`err_delay` 让失败那一支
/// 真的花掉一点单调钟时间（成功面用桩的 `slow_ms`，失败面就地补测——两支各有各的判据）。
struct Scripted {
    ok_slow_ms: i64,
    fail_with: Option<LlmFailure>,
    err_delay: Duration,
}

impl Egress for Scripted {
    fn post(&self, _url: &str, _headers: &[(String, String)], _body: &str) -> Result<Raw, LlmFailure> {
        if let Some(f) = self.fail_with {
            std::thread::sleep(self.err_delay);
            return Err(f);
        }
        Ok(Raw { status: 200, body: REPLY_OK.to_string(), slow_ms: self.ok_slow_ms })
    }
}

fn prompt() -> Prompt {
    Prompt { system: "S".to_string(), user: "U".to_string(), derive: false }
}

fn live_with_cell(cell: Option<Rc<MaxCell>>, ok_slow_ms: i64, fail_with: Option<LlmFailure>) -> Live {
    let stub = Scripted { ok_slow_ms, fail_with, err_delay: Duration::from_millis(10) };
    let live = Live::new(
        "deepseek",
        "deepseek-chat",
        "sk-test-key-value-must-not-appear".to_string(),
        100,
        None,
        Box::new(stub),
    )
    .expect("preset 表里有的行必须构造得起来");
    match cell {
        Some(c) => live.with_call_ms(c),
        None => live,
    }
}

/// ① 成功面：transport 的 `slow_ms` 原样进格，`complete` 的返回值一个字节都不变。
#[test]
fn a_success_call_passes_the_transport_slow_ms_into_the_cell() {
    let cell = Rc::new(MaxCell::new());
    let live = live_with_cell(Some(cell.clone()), 1234, None);
    let reply = live.complete(&prompt()).expect("桩只回 200");
    assert!(!reply.is_empty());
    assert_eq!(cell.get(), 1234, "透传的是桩（= transport 侧）掐的表");
}

/// 两发都在同一格上取 max：观测面的口径是"最慢一发"，不是"最后一发"。
#[test]
fn two_calls_keep_the_max_not_the_last() {
    let cell = Rc::new(MaxCell::new());
    let slow = live_with_cell(Some(cell.clone()), 100, None);
    slow.complete(&prompt()).expect("第一发");
    slow.complete(&prompt()).expect("第二发");
    assert_eq!(cell.get(), 100);
}

/// ② 失败面：`Err(Timeout)` 没有 `Raw` 可传，就地补测拿到桩真花掉的那段（10ms）。
#[test]
fn a_timeout_call_still_lands_a_monotonic_reading() {
    let cell = Rc::new(MaxCell::new());
    let live = live_with_cell(Some(cell.clone()), 0, Some(LlmFailure::Timeout));
    let got = live.complete(&prompt());
    assert_eq!(got, Err(LlmFailure::Timeout));
    // 下界 5ms：桩睡了 10ms，补测取的是同一段单调钟；上界放宽到 5s 防 CI 抖动误伤。
    let ms = cell.get();
    assert!(ms >= 5, "失败那一发不许从读数里消失：{ms}ms");
    assert!(ms <= 5_000, "补测是一段实测，不是墙上钟编的数：{ms}ms");
}

/// ③ 缺省面：不给观测格，`Live` 照常完成——观测面关着不改变业务结果（let _ = 同律）。
#[test]
fn a_live_without_the_cell_ignores_the_observation_entirely() {
    let live = live_with_cell(None, 7, None);
    let reply = live.complete(&prompt()).expect("没有观测格也照常答");
    assert!(!reply.is_empty());
}
