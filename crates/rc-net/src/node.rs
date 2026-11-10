//! 节点侧的 HTTP 客户端（阶段 4 T4-A）。
//!
//! **它与中心侧的实现不共享**（架构的 crate 切法：`rc-net` = 协议一份 + 中心侧 + 节点侧两个实现）。
//! 共享的只有 `protocol` 里的常量与三个 canonical 串；中心侧那套是 `http.rs`
//! （accept 循环 + 请求读 + 响应写），这里是它的**对面**：连接、请求写、响应读。
//!
//! **零存储依赖**：本模块不认任何句柄类型。签名、nonce、清单验签都在这里，
//! 但"这一行数据写进哪张表"是 `node` 组合根的事。
//!
//! **不取时钟**：需要时间的参数由调用方传进来（`now_ms`）。

use crate::error::NetError;
use crate::key::{verify_b64, DeviceKey};
use crate::protocol::{
    challenge_canonical, request_canonical_v2, sha256_hex, MAX_BODY_BYTES, MAX_HEADER_BYTES,
    MAX_SNAPSHOT_CHUNK,
};
use crate::snapshot::Manifest;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

/// 一次下行最多重连几次。**重连不是无限次**：链路一直断着时，
/// 无限重连会让 `sync --once` 永远不返回，而"永远不返回"对一个外部调度器来说
/// 就是"这一轮没跑完"——比失败更难查。
pub const MAX_RESUMES: u32 = 3;

/// 端点清单里的一项。
///
/// `label` 是**路名**（给人看的、进状态文件与告警）；`host` / `port` 是网络事实。
/// 路名与地址**分开存**是为了"零路径"：状态文件里只该出现路名，
/// 而路名是人起的、可控的（不像地址那样会被当成"这台机器在哪"）。
///
/// `tls_fp` 是中心的证书指纹（小写 64 hex）。`Some` ⇒ TLS + 钉死；
/// `None` ⇒ 明文（回环开发面、或还没接线 TLS 的旧配置）。**没有中间态**：
/// 有 pin 就必须握手成功且证书对得上，否则这条连接失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub label: String,
    pub host: String,
    pub port: u16,
    pub tls_fp: Option<String>,
}

impl Endpoint {
    /// 解析成候选地址。**IPv6 优先**是这一层的责任。
    pub fn resolve(&self) -> Result<Vec<SocketAddr>, NetError> {
        let target = if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        };
        let addrs: Vec<SocketAddr> = target
            .to_socket_addrs()
            .map_err(|_| NetError::EndpointUnreachable)?
            .collect();
        if addrs.is_empty() {
            return Err(NetError::EndpointUnreachable);
        }
        Ok(order_addrs(addrs))
    }
}

/// **IPv6 优先**：IPv6 族搬到前面，族内保序。
///
/// 只在**同一个路名内部**排——**不跨路名重排**：配置顺序就是优先级，
/// 跨路名重排会让"第一条连上的就是当前路径"这句话失去依据。
pub fn order_addrs(mut addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    addrs.sort_by_key(|a| !a.is_ipv6());
    addrs
}

/// 上一次连通的 endpoint 排到最前，其余保持配置顺序。
///
/// 真实 LAN 与虚拟 LAN 切换后，上一次成功的路通常还是通的；先试它，
/// 避免每次同步都先等一条已经死掉的路超时。没有命中（空串/旧标签）就原样返回。
pub fn order_endpoints<'a>(endpoints: &'a [Endpoint], last_good: &str) -> Vec<&'a Endpoint> {
    let mut ordered: Vec<&Endpoint> = endpoints.iter().collect();
    if let Some(pos) = ordered.iter().position(|e| e.label == last_good) {
        let ep = ordered.remove(pos);
        ordered.insert(0, ep);
    }
    ordered
}

#[derive(Debug, Clone)]
pub struct NodeRequest {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct NodeResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl NodeResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        let want = name.to_ascii_lowercase();
        self.headers.iter().find(|(k, _)| *k == want).map(|(_, v)| v.as_str())
    }
}

/// 发一发请求、读回响应。**只做传输**：不重试、不降级（降级是 `node::sync` 的编排）。
pub fn request(
    endpoint: &Endpoint,
    req: &NodeRequest,
    timeout: Duration,
) -> Result<NodeResponse, NetError> {
    let addrs = endpoint.resolve()?;
    let mut last = NetError::EndpointUnreachable;
    for addr in addrs {
        match request_to(&addr, endpoint.tls_fp.as_deref(), req, timeout) {
            Ok(resp) => return Ok(resp),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// 一条路的一次尝试。`tls_fp` 决定走哪条连接：`Some` ⇒ TLS（握手在 `connect` 里走完，
/// 错指纹当场失败）；`None` ⇒ 既有明文 TCP。两条路共用后面同一段请求/应答。
fn request_to(
    addr: &SocketAddr,
    tls_fp: Option<&str>,
    req: &NodeRequest,
    timeout: Duration,
) -> Result<NodeResponse, NetError> {
    match tls_fp {
        Some(fp) => {
            let mut stream =
                crate::tls::connect(*addr, fp, timeout).map_err(tls_connect_error)?;
            exchange(&mut stream, addr, req)
        }
        None => {
            let mut stream =
                TcpStream::connect_timeout(addr, timeout).map_err(|e| NetError::Io(e.kind()))?;
            stream
                .set_read_timeout(Some(timeout))
                .map_err(|e| NetError::Io(e.kind()))?;
            stream
                .set_write_timeout(Some(timeout))
                .map_err(|e| NetError::Io(e.kind()))?;
            exchange(&mut stream, addr, req)
        }
    }
}

/// TLS 连接失败的归类：**链路问题仍是"不可达"**（连不上、握手超时、对端半路关），
/// 只有证书/密钥/指纹这几类才配 `NetError::Tls`——那是"对面不是我以为的那一个"，
/// 下一步是核对 `hub_tls_fp`，不是等网络恢复。
fn tls_connect_error(e: crate::tls::TlsError) -> NetError {
    match e {
        crate::tls::TlsError::Io => NetError::EndpointUnreachable,
        _ => NetError::Tls,
    }
}

/// 写一个请求、读一个响应。明文与 TLS 只在这一层之上无差别。
fn exchange<S: Read + Write>(
    stream: &mut S,
    addr: &SocketAddr,
    req: &NodeRequest,
) -> Result<NodeResponse, NetError> {
    let mut head = format!("{} {} HTTP/1.1\r\n", req.method, req.target);
    head.push_str(&format!("Host: {addr}\r\n"));
    head.push_str(&format!("Content-Length: {}\r\n", req.body.len()));
    head.push_str("Connection: close\r\n");
    for (k, v) in &req.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .map_err(|e| NetError::Io(e.kind()))?;
    if !req.body.is_empty() {
        stream
            .write_all(&req.body)
            .map_err(|e| NetError::Io(e.kind()))?;
    }
    stream.flush().map_err(|e| NetError::Io(e.kind()))?;
    read_response(stream)
}

fn read_response<R: Read>(stream: &mut R) -> Result<NodeResponse, NetError> {
    // 借用而不是 `try_clone`：TLS 的 `StreamOwned` 不能克隆，
    // 而这里读完之后本来就不再碰这条连接（`Connection: close`）。
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| NetError::Io(e.kind()))?;
    let mut parts = line.trim_end_matches(['\r', '\n']).splitn(3, ' ');
    let version = parts.next().ok_or(NetError::BadResponse)?;
    if !version.starts_with("HTTP/1.") {
        return Err(NetError::BadResponse);
    }
    let status: u16 = parts
        .next()
        .ok_or(NetError::BadResponse)?
        .parse()
        .map_err(|_| NetError::BadResponse)?;
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut total = line.len();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| NetError::Io(e.kind()))?;
        if n == 0 {
            return Err(NetError::BadResponse);
        }
        total += line.len();
        if total > MAX_HEADER_BYTES {
            return Err(NetError::BadResponse);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        let (name, value) = trimmed.split_once(':').ok_or(NetError::BadResponse)?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    let length: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .map(|(_, v)| v.parse::<usize>().map_err(|_| NetError::BadResponse))
        .transpose()?
        .unwrap_or(0);
    if length > MAX_BODY_BYTES {
        return Err(NetError::BadResponse);
    }
    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .map_err(|e| NetError::Io(e.kind()))?;
    Ok(NodeResponse { status, headers, body })
}

/// 一次"要签名的请求"的身份。token、私钥、中心公钥都与路名无关 ⇒ 换路不重配对。
pub struct NodeAuth<'a> {
    pub token: &'a str,
    pub device_id: &'a str,
    pub key: &'a DeviceKey,
    /// 中心的公钥（钉死值）。用它核 challenge 那枚 nonce 的签名。
    pub hub_pub: &'a str,
}

/// 要一枚 nonce。**每个请求一枚**——nonce 一次性（中心侧消费之后即失效），
/// 所以分片下载的**每一片**都要先要一次。
///
/// 中心签了它给的那枚 nonce，节点**核一遍再用**：中心被冒充时，
/// 这一枚 nonce 换来的请求会被真的执行下去。
/// 中心发来的 challenge：nonce + 当前 epoch。节点把 epoch 签进每个请求，
/// 旧中心因此认不出新 epoch 的请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub nonce: String,
    pub epoch: u64,
}

pub fn challenge(
    endpoint: &Endpoint,
    auth: &NodeAuth<'_>,
    timeout: Duration,
) -> Result<Challenge, NetError> {
    let resp = request(
        endpoint,
        &NodeRequest {
            method: "POST".to_string(),
            target: "/v1/challenge".to_string(),
            headers: vec![
                ("Authorization".to_string(), format!("Bearer {}", auth.token)),
                // 中心按这个头核对"token 的主人 == 声称的设备"，缺了直接 403。
                ("X-Recollect-Device".to_string(), auth.device_id.to_string()),
            ],
            body: Vec::new(),
        },
        timeout,
    )?;
    // 身份被拒要原样穿出去；折成坏应答会让"中心不认我"读成"应答坏了"。
    if matches!(resp.status, 401 | 403 | 409) {
        return Err(NetError::AuthRejected);
    }
    if resp.status != 200 {
        return Err(NetError::BadResponse);
    }
    let doc: serde_json::Value =
        serde_json::from_slice(&resp.body).map_err(|_| NetError::BadResponse)?;
    let nonce = doc
        .get("nonce")
        .and_then(|v| v.as_str())
        .ok_or(NetError::BadResponse)?;
    let expires_at_ms = doc
        .get("expires_at_ms")
        .and_then(|v| v.as_i64())
        .ok_or(NetError::BadResponse)?;
    let sig = doc
        .get("sig")
        .and_then(|v| v.as_str())
        .ok_or(NetError::BadResponse)?;
    let epoch = doc
        .get("epoch")
        .and_then(|v| v.as_u64())
        .ok_or(NetError::BadResponse)?;
    let canonical = challenge_canonical(auth.device_id, nonce, expires_at_ms);
    if !verify_b64(auth.hub_pub, canonical.as_bytes(), sig) {
        return Err(NetError::Signature);
    }
    Ok(Challenge {
        nonce: nonce.to_string(),
        epoch,
    })
}

/// 签名的请求：先要一枚 nonce，再签、再发。
///
/// 签名盖的是**正文本身**（压缩与否不改变签名；本节点上行发明文，签的就是它自己）。
/// `extra` 是额外请求头（`Range` 走这里）：**签名不含请求头**——
/// canonical 只盖 method / target / 正文 sha / device / nonce，所以加头不影响签名。
pub fn signed_request(
    endpoint: &Endpoint,
    auth: &NodeAuth<'_>,
    method: &str,
    target: &str,
    body: &[u8],
    extra: &[(String, String)],
    timeout: Duration,
) -> Result<NodeResponse, NetError> {
    let challenge = challenge(endpoint, auth, timeout)?;
    let canonical = request_canonical_v2(
        method,
        target,
        &sha256_hex(body),
        auth.device_id,
        &challenge.nonce,
        challenge.epoch,
    );
    let sig = auth.key.sign_b64(canonical.as_bytes());
    let mut headers = vec![
        ("Authorization".to_string(), format!("Bearer {}", auth.token)),
        ("X-Recollect-Device".to_string(), auth.device_id.to_string()),
        ("X-Recollect-Nonce".to_string(), challenge.nonce),
        ("X-Recollect-Epoch".to_string(), challenge.epoch.to_string()),
        ("X-Recollect-Sig".to_string(), sig),
    ];
    headers.extend(extra.iter().cloned());
    request(
        endpoint,
        &NodeRequest {
            method: method.to_string(),
            target: target.to_string(),
            headers,
            body: body.to_vec(),
        },
        timeout,
    )
}

/// `POST /v1/rows` 的回应：四个计数，与中心 `hub::rows::Absorbed` 逐格同形。
///
/// `absorbed` 是**本批真正落库的行数**（重放时是 0），节点把它记进 `SyncOutcome.rows_uploaded`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RowsAck {
    pub rows_in: usize,
    pub absorbed: usize,
    pub skipped_duplicate: usize,
    pub turns_marked: usize,
}

/// 把暂存层的新行交给中心吸收。**签名盖原始 body**，不 gzip（与 `/v1/ingest` 同一条约定）。
///
/// 401 / 403 / 409 折 `AuthRejected`（身份被拒必须原样穿过，理由同 [`challenge`]）；
/// 非 200 折 `BadResponse`；200 但四计数缺一格也折 `BadResponse`——"中心答了但答的不是
/// 契约里的东西"不许读成"吸收了 0 行"，后者会让节点把层丢掉而中心其实没收。
pub fn upload_rows(
    endpoint: &Endpoint,
    auth: &NodeAuth<'_>,
    body: &[u8],
    timeout: Duration,
) -> Result<RowsAck, NetError> {
    let resp = signed_request(endpoint, auth, "POST", "/v1/rows", body, &[], timeout)?;
    if matches!(resp.status, 401 | 403 | 409) {
        return Err(NetError::AuthRejected);
    }
    if resp.status != 200 {
        return Err(NetError::BadResponse);
    }
    let doc: serde_json::Value =
        serde_json::from_slice(&resp.body).map_err(|_| NetError::BadResponse)?;
    let count = |name: &str| -> Result<usize, NetError> {
        let n = doc
            .get(name)
            .and_then(|v| v.as_u64())
            .ok_or(NetError::BadResponse)?;
        usize::try_from(n).map_err(|_| NetError::BadResponse)
    };
    Ok(RowsAck {
        rows_in: count("rows_in")?,
        absorbed: count("absorbed")?,
        skipped_duplicate: count("skipped_duplicate")?,
        turns_marked: count("turns_marked")?,
    })
}

/// 一次下行收完了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downloaded {
    /// 收到的总字节数。
    pub bytes: u64,
    /// 这一次是从哪个偏移接着收的（0 = 从头）。
    pub resumed_from: u64,
    /// 本次调用里断线重连了几次。
    pub resumes: u32,
}

/// 分片下行 + **断点续传**。
///
/// - `part_path` 是分片落地的临时文件；**只有整份 sha 校验通过后**，调用方才把它 rename 成正式副本。
/// - `already` 是"上一段已经收下多少字节"（来自节点状态文件）。
///   盘上那个文件的长度与它对不上时**从头来**——拿一个不存在的偏移续传，
///   结果是"看起来下完了、其实中间少了一段"，而 sha 校验会把它抓住。
/// - 每一片都核中心给的分片 sha；核不过就当这一片没收到。
/// - 断线重连后**从下一片继续**，不重下已收下的片。
pub fn download_snapshot(
    endpoint: &Endpoint,
    auth: &NodeAuth<'_>,
    manifest: &Manifest,
    part_path: &Path,
    already: u64,
    timeout: Duration,
) -> Result<Downloaded, NetError> {
    let total = manifest.bytes;
    let on_disk = file_len(part_path).unwrap_or(0);
    let mut received = if already > 0 && on_disk == already {
        already
    } else {
        0
    };
    if received == 0 {
        // 从头来 ⇒ 那个残段**必须被截掉**。append 写在一个 100 字节的旧残段后面，
        // 拼出来的是"残段 + 整份"，长度多出旧残段那截，而 sha 校验要到最后一刻才发现。
        File::create(part_path).map_err(|e| NetError::Io(e.kind()))?;
    }
    let resumed_from = received;
    let mut resumes = 0u32;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(part_path)
        .map_err(|e| NetError::Io(e.kind()))?;
    while received < total {
        let start = received;
        let end = (start + MAX_SNAPSHOT_CHUNK as u64 - 1).min(total - 1);
        let range = format!("bytes={start}-{end}");
        match download_chunk(endpoint, auth, &range, timeout) {
            Ok(chunk) => {
                file.write_all(&chunk).map_err(|e| NetError::Io(e.kind()))?;
                received += chunk.len() as u64;
            }
            Err(e) => {
                resumes += 1;
                if resumes > MAX_RESUMES {
                    return Err(e);
                }
            }
        }
    }
    file.flush().map_err(|e| NetError::Io(e.kind()))?;
    Ok(Downloaded { bytes: received, resumed_from, resumes })
}

/// 收一片。**分片 sha 核不过就是这一片没收到**——收下一片半的字节再续传，
/// 拼出来的整份一定不对，而"整份不对"要到最后一刻才暴露。
fn download_chunk(
    endpoint: &Endpoint,
    auth: &NodeAuth<'_>,
    range: &str,
    timeout: Duration,
) -> Result<Vec<u8>, NetError> {
    let resp = signed_request(
        endpoint,
        auth,
        "GET",
        "/v1/snapshot/memory",
        b"",
        &[("Range".to_string(), range.to_string())],
        timeout,
    )
    .and_then(|resp| {
        if resp.status == 206 {
            Ok(resp)
        } else {
            Err(NetError::BadResponse)
        }
    })?;
    let expected = resp
        .header("x-recollect-chunk-sha256")
        .ok_or(NetError::BadResponse)?;
    if expected != sha256_hex(&resp.body) {
        return Err(NetError::ChunkMismatch);
    }
    Ok(resp.body)
}

fn file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}
