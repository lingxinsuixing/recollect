//! W19c1：单发墙真的点火（真 socket，不碰外网）。
//!
//! 窗口里起一个只 `accept`、不写一个字节回复的本机 listener：建连与发请求都会成功，
//! 卡住的是"等响应头"那一格。deadline 取 200ms（远小于墙本身），于是这条用例证明的是
//! **超时链路通了**（`Agent` 的四格超时 → `Error::Timeout` → `LlmFailure::Timeout`），
//! 不是"300 秒会等 300 秒"。墙上限本身由第二条用例钉住。

use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_llm_http::transport::{agent, CALL_WALL_MS};
use rc_llm_http::Live;
use std::io::Read;
use std::net::TcpListener;
use std::time::{Duration, Instant};

/// 只收请求、不回响应：对端挂在那里，客户端只能靠自己的墙回到调用方。
fn hanging_listener() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local listener");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        // 只收一条连接：读一次请求头就挂着。读失败（对端已走）也照样挂着——这一支要的是
        // "对端不说话"，不是"对端说了什么"。
        let Ok((mut sock, _)) = listener.accept() else { return };
        let mut buf = [0u8; 1024];
        let _ = sock.read(&mut buf);
        std::thread::sleep(Duration::from_secs(3));
        drop(sock);
    });
    addr
}

#[test]
fn a_call_that_never_answers_hits_the_wall_and_comes_back_as_timeout() {
    let addr = hanging_listener();
    let wall = agent(200);
    let live = Live::new_custom(
        &format!("http://{addr}/v1"),
        "openai-chat",
        "local-hang-model",
        "sk-test-value".to_string(),
        8_000,
        None,
        Box::new(wall),
    )
    .expect("custom 通道的座位形状必须成立");

    let started = Instant::now();
    let got = live.complete(&Prompt {
        system: "S".to_string(),
        user: "U".to_string(),
        derive: false,
    });
    let elapsed = started.elapsed();
    println!("wall test: deadline=200ms elapsed={elapsed:?} got={got:?}");

    assert_eq!(
        got,
        Err(LlmFailure::Timeout),
        "挂起的对端必须落到 Timeout，实到 {got:?}（{elapsed:?}）"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "200ms 的 deadline 却在 {elapsed:?} 之后才回来——墙没点火"
    );
}

#[test]
fn the_call_wall_is_three_hundred_seconds_and_lives_in_one_constant() {
    // 这一条挡的是"把墙悄悄抬大"：300 秒是用户裁定的硬墙，不是一个可调的缺省值。
    // 调小只在 `hub` 的装配点发生（`--llm-timeout-ms` 与它取 min），不在这一格。
    assert_eq!(CALL_WALL_MS, 300_000, "单发墙被改了：它不是一个可调的参数");
}
