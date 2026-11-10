use crate::protocol::{MAX_BODY_BYTES, MAX_HEADER_BYTES, MAX_HEADERS};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_header_bytes: usize,
    pub max_headers: usize,
    pub max_body_bytes: usize,
    /// 一条连接的空闲上限（毫秒）：握手、读请求、写响应都受它约束。
    ///
    /// 没有它，单线程 accept 会被一条"连上就什么都不发"的连接永久占住——那是停服，
    /// 不是慢；TLS 那条路的握手同样阻塞，一个字节不发就永远 `is_handshaking()`。
    pub idle_timeout_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_header_bytes: MAX_HEADER_BYTES,
            max_headers: MAX_HEADERS,
            max_body_bytes: MAX_BODY_BYTES,
            // 30 秒：比任何一次正常请求（含一次快照分片）都长，比"运维以为中心死了"短。
            idle_timeout_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// 请求行里的原样目标（含 query）。签名覆盖它，**不是**解码后的 path。
    pub target: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    /// 头部名一律小写。
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn new(method: &str, target: &str, headers: Vec<(String, String)>, body: Vec<u8>) -> Request {
        let (path, query) = split_target(target);
        Request {
            method: method.to_string(),
            target: target.to_string(),
            path,
            query,
            headers: headers
                .into_iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v))
                .collect(),
            body,
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        let want = name.to_ascii_lowercase();
        self.headers.iter().find(|(k, _)| *k == want).map(|(_, v)| v.as_str())
    }

    pub fn query_get(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

fn split_target(target: &str) -> (String, Vec<(String, String)>) {
    match target.split_once('?') {
        None => (target.to_string(), Vec::new()),
        Some((path, raw)) => {
            let query = raw
                .split('&')
                .filter(|s| !s.is_empty())
                .map(|pair| match pair.split_once('=') {
                    Some((k, v)) => (percent_decode(k), percent_decode(v)),
                    None => (percent_decode(pair), String::new()),
                })
                .collect();
            (path.to_string(), query)
        }
    }
}

/// 只处理 `%XX` 与 `+`。查询值是设备 id 一类的 token，不做更宽的编码解释。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, value: serde_json::Value) -> Response {
        Response {
            status,
            content_type: "application/json",
            headers: Vec::new(),
            body: serde_json::to_vec(&value).expect("json value serializes"),
        }
    }

    pub fn error(status: u16, code: &'static str) -> Response {
        Response {
            status,
            content_type: "application/json",
            headers: Vec::new(),
            body: format!("{{\"error\":\"{code}\"}}").into_bytes(),
        }
    }

    pub fn bytes(status: u16, content_type: &'static str, body: Vec<u8>) -> Response {
        Response { status, content_type, headers: Vec::new(), body }
    }
}

pub trait Handler {
    fn handle(&mut self, req: &Request) -> Response;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindError {
    Unspecified,
    LoopbackWithoutFlag,
}

/// mesh-only 绑定断言：拒绝 unspecified（`0.0.0.0` / `::`）；loopback 只有显式
/// `--dev-loopback` 才放行。失败即启动失败，不静默降级。
pub fn assert_bind(addr: SocketAddr, dev_loopback: bool) -> Result<(), BindError> {
    if addr.ip().is_unspecified() {
        return Err(BindError::Unspecified);
    }
    if addr.ip().is_loopback() && !dev_loopback {
        return Err(BindError::LoopbackWithoutFlag);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeError {
    Malformed,
    NotSatisfiable,
    TooLarge,
}

/// `Range: bytes=a-b`（闭区间），只支持单段。`total=0` 时任何区间都不可满足。
pub fn parse_range(header: &str, total: u64, max_len: u64) -> Result<(u64, u64), RangeError> {
    let rest = header.strip_prefix("bytes=").ok_or(RangeError::Malformed)?;
    if rest.contains(',') {
        return Err(RangeError::Malformed);
    }
    let (a, b) = rest.split_once('-').ok_or(RangeError::Malformed)?;
    if a.is_empty() {
        return Err(RangeError::Malformed);
    }
    let start: u64 = a.parse().map_err(|_| RangeError::Malformed)?;
    let end: u64 = if b.is_empty() {
        total.saturating_sub(1)
    } else {
        b.parse().map_err(|_| RangeError::Malformed)?
    };
    if total == 0 || start >= total || start > end {
        return Err(RangeError::NotSatisfiable);
    }
    let end = end.min(total - 1);
    if end - start + 1 > max_len {
        return Err(RangeError::TooLarge);
    }
    Ok((start, end))
}

/// 单线程 accept 循环。`stop` 只在一轮 accept 前检查；测试用一条唤醒连接让阻塞退出。
pub fn serve(
    listener: TcpListener,
    handler: &mut dyn Handler,
    stop: &AtomicBool,
    limits: Limits,
    max_bps: u64,
) {
    for incoming in listener.incoming() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Ok(stream) = incoming else { continue };
        // accept 之后立刻设：在这一格之下，任何一条连接最多占用这个循环 `idle_timeout_ms`，
        // 它读不完就按"这条连接坏了"关掉，下一条照常进来。
        apply_idle_timeout(&stream, limits);
        let _ = handle_connection(stream, handler, limits, max_bps);
    }
}

/// accept 出来的裸 socket 上的空闲上限。超时在 `read_request` 里折成一次读失败 ⇒
/// 走"坏请求"那条路：写一个静态错误、关连接、不打印、不退出 accept 循环。
///
/// `idle_timeout_ms = 0` 读作"不限"（零时长的 socket 超时在各平台上是非法参数）。
fn apply_idle_timeout(stream: &std::net::TcpStream, limits: Limits) {
    if limits.idle_timeout_ms == 0 {
        return;
    }
    let d = Some(Duration::from_millis(limits.idle_timeout_ms));
    // 设不上只说明这条连接的 socket 不支持超时；发现失败也不该把服务带停。
    let _ = stream.set_read_timeout(d);
    let _ = stream.set_write_timeout(d);
}

/// 同一套读请求/写响应/pacing，外面套一层 TLS。
///
/// 握手在这里显式走完：明文打到 TLS 端口、版本对不上、客户端证书被拒——
/// 全部只关这一条连接，**不退出 accept 循环**（一次错握手不能变成服务停机）。
pub fn serve_tls(
    listener: TcpListener,
    handler: &mut dyn Handler,
    stop: &AtomicBool,
    limits: Limits,
    max_bps: u64,
    config: Arc<rustls::ServerConfig>,
) {
    for incoming in listener.incoming() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Ok(tcp) = incoming else { continue };
        // 超时设在握手**之前**：一个连上不发 ClientHello 的对端会让 `complete_io` 永远阻塞，
        // 同一个上限因此也约束握手本身——一次不说话不等于这台中心停机。
        apply_idle_timeout(&tcp, limits);
        let Ok(conn) = rustls::ServerConnection::new(config.clone()) else {
            continue;
        };
        let mut stream = rustls::StreamOwned::new(conn, tcp);
        let mut handshook = true;
        while handshook && stream.conn.is_handshaking() {
            handshook = stream.conn.complete_io(&mut stream.sock).is_ok();
        }
        if !handshook {
            continue;
        }
        let _ = handle_connection(stream, handler, limits, max_bps);
    }
}

/// 明文与 TLS 共用的一条连接处理：先读一个请求，再写一个响应。
fn handle_connection<S: Read + Write>(
    stream: S,
    handler: &mut dyn Handler,
    limits: Limits,
    max_bps: u64,
) -> std::io::Result<()> {
    let mut stream = stream;
    let req = match read_request(&mut stream, limits) {
        Ok(r) => r,
        Err(e) => {
            let resp = e.response();
            write_response(&mut stream, &resp, 0)?;
            return Ok(());
        }
    };
    let resp = handler.handle(&req);
    write_response(&mut stream, &resp, max_bps)
}

struct HttpError {
    status: u16,
    code: &'static str,
}

impl HttpError {
    fn response(&self) -> Response {
        Response::error(self.status, self.code)
    }
}

fn read_request<R: Read>(reader: &mut R, limits: Limits) -> Result<Request, HttpError> {
    // 借用 `&mut R` 而不是克隆：TLS 的 `StreamOwned` 不能克隆，
    // 明文那条路也不再需要一次 `try_clone`。
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    let n = reader.read_line(&mut line).map_err(|_| bad())?;
    if n == 0 {
        return Err(bad());
    }
    if line.len() > limits.max_header_bytes {
        return Err(HttpError { status: 431, code: "header_too_large" });
    }
    let mut parts = line.trim_end_matches(['\r', '\n']).split(' ');
    // `method` / `target` 必须立刻转成 owned：下面那个循环会 `line.clear()` 再读头部，
    // 借着一片切片就会撞 E0502（计划字面块里的那一形编译不过）。
    let method = parts.next().ok_or_else(bad)?.to_string();
    let target = parts.next().ok_or_else(bad)?.to_string();
    let version = parts.next().ok_or_else(bad)?;
    if parts.next().is_some() || !version.starts_with("HTTP/1.") {
        return Err(bad());
    }
    if !matches!(method.as_str(), "GET" | "POST") {
        return Err(HttpError { status: 405, code: "unsupported_method" });
    }

    let mut headers: Vec<(String, String)> = Vec::new();
    let mut total = line.len();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(|_| bad())?;
        if n == 0 {
            return Err(bad());
        }
        total += line.len();
        if total > limits.max_header_bytes {
            return Err(HttpError { status: 431, code: "header_too_large" });
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if trimmed.starts_with(' ') || trimmed.starts_with('\t') {
            return Err(bad()); // obs-fold 不支持
        }
        if headers.len() >= limits.max_headers {
            return Err(HttpError { status: 431, code: "too_many_headers" });
        }
        let (name, value) = trimmed.split_once(':').ok_or_else(bad)?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }

    if headers.iter().any(|(k, _)| k == "transfer-encoding") {
        return Err(HttpError { status: 411, code: "length_required" });
    }
    let length = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .map(|(_, v)| v.parse::<usize>().map_err(|_| bad()))
        .transpose()?
        .unwrap_or(0);
    if method == "POST" && !headers.iter().any(|(k, _)| k == "content-length") {
        return Err(HttpError { status: 411, code: "length_required" });
    }
    if length > limits.max_body_bytes {
        return Err(HttpError { status: 413, code: "body_too_large" });
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).map_err(|_| bad())?;
    Ok(Request::new(&method, &target, headers, body))
}

fn bad() -> HttpError {
    HttpError { status: 400, code: "bad_request" }
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

fn write_response<W: Write>(stream: &mut W, resp: &Response, max_bps: u64) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n",
        resp.status,
        status_reason(resp.status),
        resp.content_type,
        resp.body.len()
    );
    for (k, v) in &resp.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    let mut pacer = Pacer::new(max_bps);
    for chunk in resp.body.chunks(64 * 1024) {
        stream.write_all(chunk)?;
        stream.flush()?;
        pacer.after(chunk.len());
    }
    Ok(())
}

/// 令牌桶的简化形：按**累计字节 / 速率**应到时刻与当前时刻的差睡觉。
/// 只用于快照这类大 body；小响应即使 `max_bps > 0` 也最多睡几毫秒。
struct Pacer {
    max_bps: u64,
    start: Instant,
    written: u64,
}

impl Pacer {
    fn new(max_bps: u64) -> Pacer {
        Pacer { max_bps, start: Instant::now(), written: 0 }
    }

    fn after(&mut self, n: usize) {
        self.written += n as u64;
        if self.max_bps == 0 {
            return;
        }
        let expected = Duration::from_secs_f64(self.written as f64 / self.max_bps as f64);
        let elapsed = self.start.elapsed();
        if expected > elapsed {
            std::thread::sleep(expected - elapsed);
        }
    }
}
