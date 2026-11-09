use rc_net::http::{assert_bind, parse_range, Handler, Limits, Request, Response, RangeError};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

struct Echo;
impl Handler for Echo {
    fn handle(&mut self, req: &Request) -> Response {
        Response::json(200, serde_json::json!({
            "method": req.method,
            "path": req.path,
            "target": req.target,
            "body_len": req.body.len()
        }))
    }
}

struct TestServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl TestServer {
    fn start(limits: Limits) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let join = std::thread::spawn(move || {
            let mut h = Echo;
            rc_net::http::serve(listener, &mut h, &stop2, limits, 0);
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

fn raw(addr: SocketAddr, bytes: &[u8]) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(bytes).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

fn status(resp: &str) -> u16 {
    resp.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[test]
fn normal_get_reaches_the_handler() {
    let s = TestServer::start(Limits::default());
    let out = raw(s.addr, b"GET /v1/state?device_id=dev-1 HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_eq!(status(&out), 200, "{out}");
    assert!(out.contains("\"target\":\"/v1/state?device_id=dev-1\""), "{out}");
}

#[test]
fn oversized_header_is_431() {
    let s = TestServer::start(Limits { max_header_bytes: 128, max_headers: 8, max_body_bytes: 64, idle_timeout_ms: 30_000 });
    let mut req = b"GET / HTTP/1.1\r\n".to_vec();
    req.extend_from_slice(&[b'a'; 256]);
    req.extend_from_slice(b"\r\n\r\n");
    let out = raw(s.addr, &req);
    assert_eq!(status(&out), 431, "{out}");
    assert!(out.contains("header_too_large"));
}

#[test]
fn too_many_headers_is_431() {
    let s = TestServer::start(Limits { max_header_bytes: 4096, max_headers: 2, max_body_bytes: 64, idle_timeout_ms: 30_000 });
    let out = raw(s.addr, b"GET / HTTP/1.1\r\nA: 1\r\nB: 2\r\nC: 3\r\n\r\n");
    assert_eq!(status(&out), 431, "{out}");
    assert!(out.contains("too_many_headers"));
}

#[test]
fn post_without_content_length_is_411_and_chunked_is_411() {
    let s = TestServer::start(Limits::default());
    let out = raw(s.addr, b"POST /v1/ingest HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_eq!(status(&out), 411, "{out}");
    let out = raw(s.addr, b"POST /v1/ingest HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n");
    assert_eq!(status(&out), 411, "{out}");
}

#[test]
fn oversized_body_is_413() {
    let s = TestServer::start(Limits { max_header_bytes: 4096, max_headers: 16, max_body_bytes: 8, idle_timeout_ms: 30_000 });
    let out = raw(s.addr, b"POST /v1/ingest HTTP/1.1\r\nContent-Length: 9\r\n\r\n123456789");
    assert_eq!(status(&out), 413, "{out}");
    assert!(out.contains("body_too_large"));
}

#[test]
fn unspecified_bind_is_refused_and_loopback_needs_the_flag() {
    assert!(assert_bind("0.0.0.0:7420".parse().unwrap(), true).is_err());
    assert!(assert_bind("[::]:7420".parse().unwrap(), true).is_err());
    assert!(assert_bind("127.0.0.1:7420".parse().unwrap(), false).is_err());
    assert!(assert_bind("127.0.0.1:7420".parse().unwrap(), true).is_ok());
    assert!(assert_bind("203.0.113.6:7420".parse().unwrap(), false).is_ok());
}

#[test]
fn range_parser_is_inclusive_and_bounded() {
    assert_eq!(parse_range("bytes=0-9", 100, 10).unwrap(), (0, 9));
    assert_eq!(parse_range("bytes=10-", 100, 1000).unwrap(), (10, 99));
    assert_eq!(parse_range("bytes=90-200", 100, 1000).unwrap(), (90, 99));
    assert_eq!(parse_range("bytes=0-9", 100, 9), Err(RangeError::TooLarge));
    assert_eq!(parse_range("bytes=100-", 100, 10), Err(RangeError::NotSatisfiable));
    assert_eq!(parse_range("items=0-1", 100, 10), Err(RangeError::Malformed));
    assert_eq!(parse_range("bytes=0-1,3-4", 100, 10), Err(RangeError::Malformed));
}
