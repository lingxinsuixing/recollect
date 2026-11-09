//! 出网点：`ureq::` 字面全仓只许出现在这一层（判据 N4）。
//!
//! 阻塞、单线程、无异步运行时——与 `hub` 的单线程 cycle 相容（节 6）。
//! `http_status_as_error(false)` 是**必须**的：默认配置在 4xx/5xx 上直接返回 `Err`，
//! 而这一层的职责恰恰是"拿到状态码与原文，再按 preset 分派四态"。

use crate::{Egress, Raw};
use rc_llm::LlmFailure;
use std::time::{Duration, Instant};
use ureq::Agent;

/// 单次模型调用的墙（毫秒）：**全仓唯一的一格**（W19c1）。
///
/// 它管的是"这一发最多能挂多久"，不是"这一轮能烧多少"：一个永不回复的对端在 300 秒处
/// 被切掉，交回来的是 [`LlmFailure::Timeout`]，于是上面的重试/隔离逻辑才有机会跑。
/// 下面四条超时（建连/发请求/收响应头/收正文）都从同一个数来，缺一条就等于那一格没有墙。
///
/// **只许调小**：`hub` 在装配点把 `--llm-timeout-ms` 与这一格取 min（见
/// `hub/src/main.rs::call_deadline`），旗标因此不能把墙调大；没有旗标的那一支直接用它。
/// 墙的读数由 `tests/transport_wall.rs` 钉住（挂起的本机 listener + 200ms deadline）。
pub const CALL_WALL_MS: u64 = 300_000;

/// 墙钟上限由调用方给（`wallclock.rs` 那一格，或 [`CALL_WALL_MS`]），在这一层只表现为
/// ureq 的四个阶段超时。超时之后 ureq 给 `Error::Timeout` ⇒ 归 `LlmFailure::Timeout`，
/// 于是 `Rotating` 走"原样重试一次再前进"那一支（节 4 第 1 条）。
pub fn agent(deadline_ms: u64) -> Agent {
    let d = Some(Duration::from_millis(deadline_ms));
    Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(d)
        .timeout_send_request(d)
        .timeout_recv_response(d)
        .timeout_recv_body(d)
        .build()
        .into()
}

impl Egress for Agent {
    fn post(&self, url: &str, headers: &[(String, String)], body: &str) -> Result<Raw, LlmFailure> {
        let started = Instant::now();
        let mut req = self.post(url);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let resp = req.send(body).map_err(|e| match e {
            ureq::Error::Timeout(_) => LlmFailure::Timeout,
            _ => LlmFailure::BackendError,
        })?;
        let status = resp.status().as_u16();
        let mut reader = resp.into_body();
        let text = reader
            .read_to_string()
            // 头读完了、体读不完：这一族是连接被中途掐断，不是这家拒绝我们。
            .map_err(|_| LlmFailure::BackendError)?;
        Ok(Raw {
            status,
            body: text,
            // 取的是单调钟（`Instant`）而不是墙上钟：判据 12a 与 N8 扫的是墙上钟那一族的字面形状，
            // 而这里要的从来不是"现在几点"，是"这一次走了多久"（裁定 V14 的形状）。
            slow_ms: started.elapsed().as_millis() as i64,
        })
    }
}
