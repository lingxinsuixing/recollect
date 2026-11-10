use rc_net::http::{serve, Handler, Limits, Request, Response};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const BODY: usize = 8 * 1024;
const MAX_BPS: u64 = 64 * 1024;

struct Fixed;

impl Handler for Fixed {
    fn handle(&mut self, _req: &Request) -> Response {
        Response::bytes(200, "application/octet-stream", vec![7u8; BODY])
    }
}

struct TestServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl TestServer {
    fn start(max_bps: u64) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let join = std::thread::spawn(move || {
            let mut h = Fixed;
            serve(listener, &mut h, &stop2, Limits::default(), max_bps);
        });
        TestServer { addr, stop, join: Some(join) }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn fetch(addr: SocketAddr) -> (usize, Duration) {
    let started = Instant::now();
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(b"GET /v1/snapshot/memory HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    let elapsed = started.elapsed();
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    (out.len() - split, elapsed)
}

#[test]
fn a_paced_response_takes_at_least_body_over_rate() {
    // 8 KiB / 64 KiB/s = 125 ms；阈值取 80 ms，给调度抖动留 45 ms。
    let paced = TestServer::start(MAX_BPS);
    let (n, slow) = fetch(paced.addr);
    assert_eq!(n, BODY, "量到的不是那 8 KiB 的体");
    assert!(slow >= Duration::from_millis(80), "限速没生效：{slow:?}");
    assert!(slow < Duration::from_secs(5), "慢得不合理：{slow:?}");

    // 对照臂：同一个处理器、同一个体，`max_bps = 0` 读作不限速。
    // 少了这一臂，"实现里写死 sleep(200ms)"也能让上面那条绿。
    let fast = TestServer::start(0);
    let (n, quick) = fetch(fast.addr);
    assert_eq!(n, BODY);
    assert!(quick < Duration::from_millis(80), "不限速也睡：{quick:?}");
}
