//! S4fix-A R2：一条"连上什么都不发"的连接不许把中心变成停机。
//!
//! `serve` / `serve_tls` 都是单线程 accept 循环，`read_request` 与 TLS 握手都是阻塞的。
//! 没有空闲上限时，一条只完成 TCP 握手、一个字节都不发的连接就能让整台中心不再 accept——
//! 所有节点超时 ⇒ 离线窗口累积 ⇒ 一天自蒸、五天 detached。这三条用例起真 socket、真握手。
//!
//! 判据有两条：正常请求**仍然拿得到应答**（accept 不被永久占住），并**在 ~1s 内**返回；
//! 哑连接最终被服务端关掉（读返回 0 或错误）。

use rc_net::http::{serve, serve_tls, Handler, Limits, Request, Response};
use rc_net::tls;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 哑连接的预算。300 ms 足够"正常请求在一秒内返回"这个判据成立，又短到用例本身不慢。
const IDLE_MS: u64 = 300;
const BUDGET: Duration = Duration::from_secs(1);

struct Echo;

impl Handler for Echo {
    fn handle(&mut self, _req: &Request) -> Response {
        Response::bytes(200, "text/plain", b"ok".to_vec())
    }
}

fn limits() -> Limits {
    Limits { idle_timeout_ms: IDLE_MS, ..Limits::default() }
}

/// 起服务的那一半：`serve` 与 `serve_tls` 各一条，用同一个形状。
struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn plaintext() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let join = std::thread::spawn(move || {
            let mut h = Echo;
            serve(listener, &mut h, &stop2, limits(), 0);
        });
        Server { addr, stop, join: Some(join) }
    }

    fn tls() -> (Server, String) {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let id = tls::ensure_identity(dir.path(), addr).unwrap();
        let pin = id.fingerprint.clone();
        let config = tls::server_config(&id).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let join = std::thread::spawn(move || {
            let mut h = Echo;
            serve_tls(listener, &mut h, &stop2, limits(), 0, config);
        });
        (Server { addr, stop, join: Some(join) }, pin)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // 唤醒 accept：这条连接会在 `stop` 检查处被丢掉。
        let _ = TcpStream::connect(self.addr);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// 一条只连不发的连接：TCP 三次握手完成，之后一个字节都不写。
fn dumb(addr: SocketAddr) -> TcpStream {
    let s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    // 让服务端先 accept 到它，再让第二条连接排队：accept 是单线程，顺序就是这里的顺序。
    std::thread::sleep(Duration::from_millis(50));
    s
}

/// 读到 EOF（对端关闭）或错误为止；返回是否等到了"这条连接被服务端关掉"。
fn wait_closed(s: &mut TcpStream, deadline: Duration) -> bool {
    let until = Instant::now() + deadline;
    let mut buf = [0u8; 4096];
    loop {
        match s.read(&mut buf) {
            Ok(0) => return true,
            Ok(_) => {}
            // 复位也是关：服务端没有义务先发 close_notify。
            Err(_) => return true,
        }
        if Instant::now() >= until {
            return false;
        }
    }
}

fn raw_get(addr: SocketAddr) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"GET /v1/health HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

#[test]
fn a_silent_connection_does_not_hold_the_plaintext_accept_loop() {
    let server = Server::plaintext();
    let mut silent = dumb(server.addr);

    let started = Instant::now();
    let out = raw_get(server.addr);
    let elapsed = started.elapsed();
    assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "正常请求被哑连接挡住了：{out:?}");
    assert!(
        elapsed < BUDGET,
        "正常请求等了 {elapsed:?}（哑连接的预算只有 {IDLE_MS} ms）"
    );

    // 哑连接最终被服务端关掉：读返回 0 或错误。
    assert!(wait_closed(&mut silent, Duration::from_secs(5)), "哑连接一直没被关");
}

#[test]
fn a_silent_connection_does_not_hold_the_tls_accept_loop() {
    let (server, pin) = Server::tls();
    // 连上不发 ClientHello：握手的 `complete_io` 会在同一个上限上超时。
    let mut silent = dumb(server.addr);

    let started = Instant::now();
    let mut stream = tls::connect(server.addr, &pin, Duration::from_secs(5))
        .expect("哑连接之后的正常 TLS 握手必须成功");
    let req = "GET /v1/health HTTP/1.1\r\nHost: recollect-hub\r\n\r\n";
    stream.write_all(req.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    while !out.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        out.push(byte[0]);
    }
    let elapsed = started.elapsed();
    let head = String::from_utf8(out).unwrap();
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "正常 TLS 请求被挡住了：{head:?}");
    assert!(
        elapsed < BUDGET,
        "正常 TLS 请求等了 {elapsed:?}（哑连接的预算只有 {IDLE_MS} ms）"
    );

    assert!(wait_closed(&mut silent, Duration::from_secs(5)), "哑连接一直没被关");
}

/// 默认值就是 30 秒：`Limits::default()` 不带 `idle_timeout_ms` 的显式值时，
/// 中心不会退回到"永远等下去"。
#[test]
fn default_idle_timeout_is_thirty_seconds() {
    assert_eq!(Limits::default().idle_timeout_ms, 30_000);
}
