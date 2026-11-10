//! 局域网发现（S4c1 应答 / S4c2 扫描）。
//!
//! 节点在扁平 LAN 上不知道中心在哪：它往固定端口发一条 probe，中心单播回一条**签名公告**。
//! 公告回答的是"应答者是谁"——`HubIdentity` 的那几个字段被中心私钥签在一起，nonce 被回显
//! 进签名体，因此一条公告只对问它的人有效，也抓不来重放。发现层**不承担首次信任**：
//! 有票的节点拿公告里的 `hub_fp/tls_fp` 与票对拍，没有票的只能把它列成"未验证中心"。
//!
//! 客户端那一半（[`discover`]）只做"把问话发出去、把答得上的公告收成一张候选表"；
//! 它不判"该连哪一个"——那是票的事。候选表的作用是让配对之前的人看见"这一片网上有谁"。
//!
//! 这一模块**不碰库、不写盘**：`serve_udp` 只吃一个已绑定的 socket、一把已加载的私钥和
//! 一份身份模板，所以它能在一条独立线程里跑到进程结束，也不给"发现应答顺手写了库"留入口。
//!
//! 协议形状（每条 datagram 一个完整 JSON 文档，无分片、无重传）：
//!
//! ```text
//! probe   : {"v":1,"type":"probe","nonce":"<16..64 base64url>"}
//! announce: {"v":1,"type":"announce","nonce":"<回显>","identity":{...},"sig":"<签 canonical>"}
//! canonical: recollect-discover-v1\n{nonce}\n{identity_canonical(identity)}
//! ```
//!
//! 两个方向都是**收不到就丢**：probe 坏了不应答（回一条错误会把应答者变成一台可被任意
//! 字符串点亮的放大器），announce 坏了 `verify_announce` 返回一个静态错误。

use crate::error::NetError;
use crate::invite::{hub_fingerprint, identity_canonical, HubIdentity};
use crate::key;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// 发现应答的固定 UDP 端口。它与 `hub.toml` 的 `bind` 无关：发现是"向整个 LAN 发问"，
/// 必须是一个所有节点事先就知道的数，而不是每台中心自己填的地址簿条目。
pub const DISCOVERY_PORT: u16 = 37420;

/// 一条 probe 的字节上限。接收缓冲就是 `PROBE_MAX + 1`，读满第 257 个字节就等于
/// "这条不要了"；上限不跟着输入一起涨，超长的包不会把应答者顶成一个内存放大器。
pub const PROBE_MAX: usize = 256;

/// 一条公告的字节上限（4 KiB）。装不下的公告不发，也不截断着发——半条 JSON 只会
/// 让收的人多一种错法。
const ANNOUNCE_MAX: usize = 4096;

/// 协议版本。版本是签名覆盖的一格，但"这条是不是我认识的格式"得在验签之前就问。
const VERSION: u32 = 1;

const TYPE_PROBE: &str = "probe";
const TYPE_ANNOUNCE: &str = "announce";
const CANONICAL_PREFIX: &str = "recollect-discover-v1";

/// 公告窗口的容差：只容忍时钟偏差，不接受过期公告。
///
/// 60 秒是"两台机器的钟差"的上界，不是"一份公告还能再用多久"——过期就是过期，
/// 只是允许对端的钟比我快/慢一分钟。
const SKEW: i64 = 60_000;

/// 错误面全静态短语：不回显输入。一条公告本身没有秘密，但它来自网络上任何一个会发包的
/// 人——错误串只说明"哪一格不对"，不替对端把字节抄进日志。
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DiscoveryError {
    #[error("discovery announce is malformed")]
    Format,
    #[error("discovery announce version is unsupported")]
    Version,
    #[error("discovery announce signature is invalid")]
    Signature,
    #[error("discovery announce nonce is malformed")]
    Nonce,
    /// 窗口不对（过期，或签发时刻在未来）。与坏签名一样是静默丢弃的理由：
    /// 一份抓下来过几天再拿出来的公告，靠这一格失效。
    #[error("discovery announce is outside its validity window")]
    Expired,
}

#[derive(Serialize)]
struct Probe<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'a str,
    nonce: &'a str,
}

/// 未知字段一律不认：v1 的 probe 只有三段，多一段就是"这条不是给我的"。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeWire {
    v: u32,
    #[serde(rename = "type")]
    kind: String,
    nonce: String,
}

#[derive(Serialize)]
struct Announce<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'a str,
    nonce: &'a str,
    identity: &'a HubIdentity,
    sig: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnnounceWire {
    v: u32,
    #[serde(rename = "type")]
    kind: String,
    nonce: String,
    identity: HubIdentity,
    sig: String,
}

/// 一条 probe 的字节。`nonce` 由问的人现生成（16 字节随机数的 base64url 落在 22 字符，
/// 在 16..=64 的窗口里）；这里不校验它——校验在应答者那一侧，问的人编错了只会收到沉默。
pub fn probe_payload(nonce: &str) -> Vec<u8> {
    serde_json::to_vec(&Probe { v: VERSION, kind: TYPE_PROBE, nonce })
        .expect("a probe always serializes")
}

/// 公告的签名体：前缀 + 回显的 nonce + `identity_canonical`。**换行分隔、绝不在调用点手拼**：
/// 签发与验证各拼一遍字符串，就是下一次"签名只在一边对"的来源。
///
/// nonce 进到 canonical 的第一行，是被签的字节；`verify_announce` 返回它，正是让问的人
/// 能把自己发出去的那一串与公告里的这一串逐字节对拍。
pub fn announce_canonical(identity: &HubIdentity, nonce: &str) -> String {
    format!("{CANONICAL_PREFIX}\n{nonce}\n{}", identity_canonical(identity))
}

/// 公告的 JSON 信封。`identity.sig` 在这里被**清空**：一条公告只有外层这一个签名
/// （签的是 [`announce_canonical`]），内层那份身份签名既不在这条 canonical 里、
/// 也就不该出现在线上——留着它，读的人会以为它验过。
pub fn encode_announce(identity: &HubIdentity, nonce: &str, sig_b64: &str) -> Vec<u8> {
    let mut identity = identity.clone();
    identity.sig.clear();
    serde_json::to_vec(&Announce {
        v: VERSION,
        kind: TYPE_ANNOUNCE,
        nonce,
        identity: &identity,
        sig: sig_b64,
    })
    .expect("an announce always serializes")
}

/// 解包 + 用 `identity.hub_pub` 验 `sig` + 判版本/窗口/nonce 形状；返回 `(nonce, identity)`。
///
/// 判定的顺序是有意的：格式与信封版本先（"这是不是一条公告"），内层签名字段必须为空
/// （线上不许出现第二个"看起来像签名"的东西），然后是外层签名，最后是被签名覆盖的
/// 三格（identity 版本、时间窗口、nonce 形状）。签名不成立时"这几格长什么样"没有意义；
/// 而签名成立之后它们不对，只可能出自一把真的中心私钥——那是中心的 bug 或有人重放旧公告，
/// 各自单独报错而不是压成坏签名。
///
/// 版本与窗口都读 `identity` 里被签过的那几格，**不是**信封的 `v`：信封谁都能改，
/// 改了签名就碎；只剩被签的那一格能说明"这条公告自称说哪种协议、什么时候说的"。
pub fn verify_announce(bytes: &[u8], now_ms: i64) -> Result<(String, HubIdentity), DiscoveryError> {
    if bytes.is_empty() || bytes.len() > ANNOUNCE_MAX {
        return Err(DiscoveryError::Format);
    }
    let doc: AnnounceWire = serde_json::from_slice(bytes).map_err(|_| DiscoveryError::Format)?;
    if doc.kind != TYPE_ANNOUNCE {
        return Err(DiscoveryError::Format);
    }
    if doc.v != VERSION {
        return Err(DiscoveryError::Version);
    }
    // 内层 `sig` 不在任何签名里，只允许为空：多出一个字段就多一条"它能被单独篡改"的路。
    if !doc.identity.sig.is_empty() {
        return Err(DiscoveryError::Signature);
    }
    let canonical = announce_canonical(&doc.identity, &doc.nonce);
    if !key::verify_b64(&doc.identity.hub_pub, canonical.as_bytes(), &doc.sig) {
        return Err(DiscoveryError::Signature);
    }
    // 版本：被签覆盖的 identity.v。信封版本对上了、身份版本是新版，说明这条公告按另一套
    // 字段语义签发——按 v1 读它等于把没见过的格式当成见过的。
    if doc.identity.v != crate::invite::IDENTITY_VERSION {
        return Err(DiscoveryError::Version);
    }
    // 窗口：过期与"未来票"都拒。方向是失败关闭——判不了时间的公告不许变成候选。
    if doc.identity.expires_at_ms < now_ms - SKEW {
        return Err(DiscoveryError::Expired);
    }
    if doc.identity.issued_at_ms > now_ms + SKEW {
        return Err(DiscoveryError::Expired);
    }
    if !is_safe_nonce(&doc.nonce) {
        return Err(DiscoveryError::Nonce);
    }
    Ok((doc.nonce, doc.identity))
}

/// 固定窗口限速：**每源**每个窗口最多 [`PER_SOURCE_MAX`] 条应答，**全局**最多
/// [`GLOBAL_MAX`] 条。超限静默丢弃。
///
/// 为什么要有：一条 49–55 B 的 probe 换一条 ≈500 B（上限 4 KiB）的公告，等于一台
/// 不用登录就能点亮的固定端口放大器；源地址可伪造时，这台中心就成了别人打 UDP 洪水的
/// 工具。限速在**应答**这一侧，而不是"信不信这条 probe"——问话本身不花什么，签名才花。
///
/// 为什么不写日志：超限的包顺着 LAN 能被打进来无数次，逐条记录等于把限速本身变成日志放大器。
struct RateLimiter {
    window_start_ms: i64,
    global: u32,
    per_source: HashMap<IpAddr, u32>,
}

/// 窗口长度。1 秒与本模块的应答粒度同量级：节点一次扫描只问一条，无需更细的分辨率。
const RATE_WINDOW_MS: i64 = 1_000;
/// 每源每窗口应答数：一台节点一次扫描一条，5 条留给同一台机器上的并发扫描与重问。
const PER_SOURCE_MAX: u32 = 5;
/// 全局每窗口应答数：200 条 × ≈500 B ≈ 100 KB/s 的出站上限，放大器到此为止。
const GLOBAL_MAX: u32 = 200;

impl RateLimiter {
    fn new(start_ms: i64) -> RateLimiter {
        RateLimiter { window_start_ms: start_ms, global: 0, per_source: HashMap::new() }
    }

    /// 这个源在这一刻还能不能拿到一条应答。允许则记账，拒绝则什么都不改。
    fn allow(&mut self, source: IpAddr, now_ms: i64) -> bool {
        // 窗口过了（或钟往回走）就整窗清零：固定窗口，不做滑动平均——判据越简单越不会
        // 在"限速把自己的状态吃满"这种边上出错。
        if now_ms - self.window_start_ms >= RATE_WINDOW_MS || now_ms < self.window_start_ms {
            self.window_start_ms = now_ms;
            self.global = 0;
            self.per_source.clear();
        }
        if self.global >= GLOBAL_MAX {
            return false;
        }
        let count = self.per_source.entry(source).or_insert(0);
        if *count >= PER_SOURCE_MAX {
            return false;
        }
        *count += 1;
        self.global += 1;
        true
    }
}

/// 在给定的已绑定 `UdpSocket` 上无限应答（线程用）。不写盘、不取库。
///
/// 每个 datagram 只认 probe：坏 JSON、别的 `type`、非 v1、nonce 形状不对、超过
/// [`PROBE_MAX`] 的一律不应答，且**一个坏包不会让这条线程退出**——发现应答挂掉，
/// 整片 LAN 就再没人回答 probe。
///
/// `now_ms` 是这条线程起跑时的墙钟；每次应答的 `issued_at_ms/expires_at_ms` 由它加上
/// 已跑过的时长现算，所以长跑的中心不会把一份早已过期的公告一直发下去。
pub fn serve_udp(
    socket: std::net::UdpSocket,
    key: key::HubKey,
    identity_template: HubIdentity,
    now_ms: i64,
) {
    let started = std::time::Instant::now();
    let mut limiter = RateLimiter::new(now_ms);
    let mut buf = [0u8; PROBE_MAX + 1];
    loop {
        let (n, peer) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            // recv 失败（Windows 上 ICMP 端口不可达会回灌成 WSAECONNRESET）不是退出理由：
            // 休眠一小会儿只为不空转，然后接着服务下一条。
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
        };
        let Some(nonce) = parse_probe(&buf[..n]) else { continue };
        let now = now_ms + started.elapsed().as_millis() as i64;
        // 限速在签名之前：被限的这一条连一次 Ed25519 都不做。
        if !limiter.allow(peer.ip(), now) {
            continue;
        }
        let identity = HubIdentity {
            // 模板里的时间只是占位：一条公告的寿命从"这一次应答"算起，不是从进程起跑算起。
            issued_at_ms: now,
            expires_at_ms: now + crate::invite::IDENTITY_TTL_MS,
            ..identity_template.clone()
        };
        let sig = key.sign_b64(announce_canonical(&identity, &nonce).as_bytes());
        let bytes = encode_announce(&identity, &nonce, &sig);
        if bytes.len() > ANNOUNCE_MAX {
            continue;
        }
        // 发不出去（对端已走、缓冲满）不退出也不重传：发现是"再问一次"的协议。
        let _ = socket.send_to(&bytes, peer);
    }
}

/// 只认 v1 probe：解析失败、类型不符、nonce 形状不对都折成 `None`。
/// 失败不写日志：一个坏包会顺着 LAN 被打进来无数次，逐条记录等于给人一个日志放大器。
fn parse_probe(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() || bytes.len() > PROBE_MAX {
        return None;
    }
    let doc: ProbeWire = serde_json::from_slice(bytes).ok()?;
    if doc.v != VERSION || doc.kind != TYPE_PROBE || !is_safe_nonce(&doc.nonce) {
        return None;
    }
    Some(doc.nonce)
}

/// nonce 是一条 safe token：16..=64 个 base64url 字符。
///
/// 短了容易被猜（公告可被预签名后拿来回放），长了让 probe 撑出缓冲；字符集限制保证它
/// 能原样进 JSON，也能原样进 canonical 的换行分隔体——换行与空白会切开字段，
/// 让"签的字节"与"解的字节"变成两件事。
fn is_safe_nonce(nonce: &str) -> bool {
    (16..=64).contains(&nonce.len())
        && nonce
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

// ---- 客户端（S4c2）：问一次，收一段，按指纹去重 -------------------------------------------

/// 收包的缓冲上限取 UDP 报文的理论上限，而不是 [`ANNOUNCE_MAX`]：超缓冲的 datagram 在
/// Windows 上是一次错误而不是截断，那会把"网线上有人发了长包"变成一次扫描失败。
/// 用协议上限读进来，"太长"就永远只是 `verify_announce` 的 `Format`。
const RECV_MAX: usize = 65_535;

/// 收包的节拍。读超时不等于"等这么久就结束"，它只是让循环有机会看一眼 deadline：
/// 什么时候停由 [`discover`] 的 `timeout` 说了算。
const READ_TICK: Duration = Duration::from_millis(200);

/// 一次扫描最多收多少条候选。公告由任何人在窗口内各签各的，不设上限就是"包速率越高、
/// 表越长"的输出与内存放大器；64 已经远多于一片 LAN 上该有的中心数。
const MAX_CANDIDATES: usize = 64;

/// 一条 probe 的 nonce：16 字节随机数的 base64url（22 字符），落在应答者要求的 16..=64 里。
///
/// 每次扫描现生成：它是"这条公告只在回我这一次问话"的凭据。复用 nonce 等于让上一次
/// 扫描收到的公告还能被这一次认领——回放的门就是这么开的。
pub fn new_nonce() -> Result<String, NetError> {
    let mut raw = [0u8; 16];
    getrandom::fill(&mut raw).map_err(|_| NetError::NoEntropy)?;
    Ok(URL_SAFE_NO_PAD.encode(raw))
}

/// 一个答了本次 probe 的中心。字段就是公告里被签名的那几格的可读投影。
///
/// `seen_from` 只用于排障与测试：它是"这条公告从哪台机器的哪个端口来的"，不是身份的一部分
/// （身份由签名与 `hub_fp` 承担），所以 CLI 不打印它——本机网络细节不该出现在候选表里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub hub_fp: String,
    pub tls_fp: String,
    pub epoch: u64,
    pub endpoints: Vec<String>,
    pub hub_pub: String,
    pub seen_from: SocketAddr,
}

/// 向每个 target 发一次 probe，最多等 `timeout`，把验得过的公告折成候选表。
///
/// 一次扫描只用一个 nonce（同一份问话发给所有目标），这是"收下的公告确实在回我"可判的
/// 唯一办法：公告里的 nonce 与它对不上就丢——别人那条问话的公告不能被我认领。
///
/// 静默丢弃四类：`verify_announce` 判坏的（拆不开/版本/窗口/形状/签名）、nonce 不是我这一个、
/// `hub_fp` 不是由这份 `hub_pub` 算出来的、表已满（[`MAX_CANDIDATES`]）。**坏包不报错**：
/// 扫描的结果是"谁答了"，不是"网线上有谁乱发了什么"；为一条伪造包让整次扫描失败，
/// 等于把 LAN 交给任何人摆布。
///
/// 发送是**按目标降级**的：一个目标写不出去（地址族不对、接口没了）只丢这一个，
/// 其余照发；只有一个目标都没发出去才是"这次扫描没做成"。
///
/// `now_ms` 是**调用方取好的**墙钟毫秒（判据 12a / 69④：库里一律不取钟）：窗口是"现在"
/// 对公告那两格的比，不是对扫描起跑时刻的比，所以这一格必须来自同一个钟面。钟在 epoch
/// 之前时调用方给 0 ⇒ 任何一份真公告的 `issued_at_ms` 都晚于 `0 + SKEW`，于是全部按
/// "窗口判不了"丢掉：宁可空表，也不认一份自己判不了时间的公告。
pub fn discover(
    targets: &[SocketAddr],
    timeout: Duration,
    now_ms: i64,
) -> Result<Vec<Candidate>, NetError> {
    let nonce = new_nonce()?;
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| NetError::Io(e.kind()))?;
    // 广播地址要求 socket 显式打开 SO_BROADCAST；对单播目标它没有副作用，
    // 所以默认目标（255.255.255.255）与测试目标（127.0.0.1）走的是同一条路。
    socket.set_broadcast(true).map_err(|e| NetError::Io(e.kind()))?;
    // 节拍不取 0：零时长的读超时是非法参数，而 `timeout` 为 0 的调用方本来就会在
    // 循环第一次看 deadline 时直接结束。
    let tick = timeout.min(READ_TICK).max(Duration::from_millis(1));
    socket.set_read_timeout(Some(tick)).map_err(|e| NetError::Io(e.kind()))?;

    let probe = probe_payload(&nonce);
    let mut sent = 0usize;
    let mut last_error = None;
    for target in targets {
        match socket.send_to(&probe, target) {
            Ok(_) => sent += 1,
            // 记下来只为"一个都没发出去"时给出那一发的错误类别，不在这里返回。
            Err(e) => last_error = Some(e.kind()),
        }
    }
    if sent == 0 {
        return Err(NetError::Io(last_error.unwrap_or(std::io::ErrorKind::Other)));
    }

    let deadline = Instant::now() + timeout;
    let mut found: Vec<Candidate> = Vec::new();
    let mut buf = vec![0u8; RECV_MAX];
    loop {
        if Instant::now() >= deadline {
            break;
        }
        match socket.recv_from(&mut buf) {
            Ok((n, seen_from)) => {
                let Ok((echoed, identity)) = verify_announce(&buf[..n], now_ms) else { continue };
                if echoed != nonce {
                    continue;
                }
                // 指纹是去重与过滤的唯一键，所以它必须由公钥现算：公告里自报的那一格
                // 谁都能填，拿它去对 `--expect-fp` 等于让对端决定自己叫什么名字。
                if hub_fingerprint(&identity.hub_pub) != identity.hub_fp {
                    continue;
                }
                // 同一台中心可能有多个网口都在答（或广播与单播各答一次）：同指纹取第一个。
                if found.iter().any(|c| c.hub_fp == identity.hub_fp) {
                    continue;
                }
                // 先去重、后判上限：满了之后"又见到同一台"仍然只是重复，不是溢出。
                if found.len() >= MAX_CANDIDATES {
                    continue;
                }
                found.push(Candidate {
                    hub_fp: identity.hub_fp,
                    tls_fp: identity.tls_fp,
                    epoch: identity.epoch,
                    endpoints: identity.endpoints,
                    hub_pub: identity.hub_pub,
                    seen_from,
                });
            }
            Err(e) => match e.kind() {
                // 读超时只是"这一拍没东西"，接着看 deadline。
                std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::Interrupted => continue,
                // Windows 上 ICMP 端口不可达会回灌成 ECONNRESET：那是"这个目标没人听"，
                // 不是扫描失败——否则问一台没开发现服务的机器就等于整次扫描报错，
                // 而"对着一片没有中心的网扫一遍"正是发现命令的常态。
                std::io::ErrorKind::ConnectionReset => continue,
                _ => return Err(NetError::Io(e.kind())),
            },
        }
    }
    // 升序：输出稳定，脚本与用例都不必自己再排一次。
    found.sort_by(|a, b| a.hub_fp.cmp(&b.hub_fp));
    Ok(found)
}
