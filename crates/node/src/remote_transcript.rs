//! W39 刀 D：边缘的**远端转录读**（细案 `2026-11-09-w39-remote-transcript` §3.2）。
//!
//! 四个转录工具在边缘（`--replica`）**远端优先、本地回落**：本模块只回答"向中心读
//! 一次"这一半，回落与信封标注是 `mcp` 门面的事（`source` / `degraded` / `remote_error`）。
//!
//! 客户端材料**全部来自边缘已有的文件**（不新增配置项）：
//! - `node-state.json` 的 `paired_endpoints` / `paired_tls_fp` / `paired_hub_pub`；
//! - `node-token` 的 token 文本；
//! - `<data_dir>/device.key` 的设备私钥。
//!
//! 端点解析用 [`crate::sync::endpoint_from_https_url`]（与 `sync` 同一条格式），
//! **按 `paired_endpoints` 的顺序逐条试**：第一个答 200 的就是这次读数；
//! 网络错误换下一条，一条都不通 ⇒ [`RemoteFault::Unreachable`]。
//!
//! **故障分型只按下一步动作分**（与 `sync` 的封闭码同一条纪律）：
//! - [`RemoteFault::NotPaired`]：本机还没有配对的材料 ⇒ 直接本地读数，**不是故障**；
//! - [`RemoteFault::Unreachable`]：材料齐但一条路都不通 ⇒ 回落本地并**显式降级**；
//! - [`RemoteFault::Rejected`]：中心不接受这份身份（401/403/409）⇒ 同样回落并降级，
//!   但"重新配对"与"等网络"是两个动作，所以码必须分开；
//! - [`RemoteFault::BadResponse`]：答了，但不是契约里的东西。
//!
//! 请求头 `extra`（`x-recollect-agent` / `x-recollect-principal`）**不进签名**——
//! canonical 只盖 method / target / 正文 sha / device / nonce / epoch；
//! 中心只把它们写进审计（"谁读了谁的会话"），不是准入条件。
//!
//! 时间：每次请求 5 秒超时（[`TIMEOUT`]）。转录面读数小，宁可一次读慢，
//! 也不让一次工具调用无限期挂在一条已经死掉的路由上。

use crate::state;
use crate::sync::endpoint_from_https_url;
use rc_net::key::DeviceKey;
use rc_net::node::{signed_request, Endpoint, NodeAuth};
use std::path::Path;
use std::time::Duration;

/// 一次远端读的超时。没有它，一条只建连不回包的链路会让工具调用挂死；
/// 5 秒比局域网里任何一次正常读数都长。
const TIMEOUT: Duration = Duration::from_secs(5);

/// 远端读的**封闭故障码**（`slug()` 是它进信封 `remote_error` 的唯一形状）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteFault {
    /// 本机没有 state / token / 设备私钥：**还没配对**，直接本地读数（不降级）。
    NotPaired,
    /// 材料齐了，但一条端点都不通（含"没有可用端点"）。
    Unreachable,
    /// 中心不接受这份身份：401 / 403 / 409（token 撤销、设备头不符、纪元过期）。
    Rejected,
    /// 中心答了，但不是 200 + 合法 JSON。
    BadResponse,
}

impl RemoteFault {
    /// 信封 `remote_error` 的取值。**封闭码**：不放错误串、不放地址。
    pub fn slug(&self) -> &'static str {
        match self {
            RemoteFault::NotPaired => "not_paired",
            RemoteFault::Unreachable => "unreachable",
            RemoteFault::Rejected => "rejected",
            RemoteFault::BadResponse => "bad_response",
        }
    }
}

/// 边缘的配对材料。**只在 [`fetch`] 里构造一次**，四个包装共用同一份解析。
struct Material {
    endpoints: Vec<Endpoint>,
    token: String,
    key: DeviceKey,
    hub_pub: String,
}

/// 从边缘的既有文件读齐材料。缺任何一件（state / token / 私钥）或状态里没有中心
/// 公钥 ⇒ `None`（= 还没配对）。**地址与指纹一起从状态来**，与 `sync` 同一口径。
fn material(data_dir: &Path) -> Option<Material> {
    let st = state::load(&state::state_path(data_dir)).ok().flatten()?;
    if st.paired_hub_pub.is_empty() {
        return None;
    }
    let token = std::fs::read_to_string(state::token_path(data_dir)).ok()?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return None;
    }
    let key = DeviceKey::load(&data_dir.join("device.key")).ok()?;
    // 路名序号按 `paired_endpoints` 的下标走：与配对/`sync` 同一套（`paired-N`）。
    let endpoints = st
        .paired_endpoints
        .iter()
        .enumerate()
        .filter_map(|(i, url)| endpoint_from_https_url(url, st.paired_tls_fp.as_deref(), i))
        .collect();
    Some(Material { endpoints, token, key, hub_pub: st.paired_hub_pub })
}

/// 向中心发一次签名的 `GET target`，把 200 的 JSON 原样交回。
///
/// 顺序是判定的一部分：先读材料（没有 ⇒ `NotPaired`），再逐条试端点
/// （网络错误换下一条），最后才分型中心的状态码。**401/403/409 立刻停**：
/// 换一条路也还是同一份 token，重试只会把"重新配对"拖成"等网络"。
pub fn fetch(
    data_dir: &Path,
    target: &str,
    extra: &[(String, String)],
) -> Result<serde_json::Value, RemoteFault> {
    let Some(m) = material(data_dir) else {
        return Err(RemoteFault::NotPaired);
    };
    if m.endpoints.is_empty() {
        // 材料在、地址一条可用都没有：读不出去是**当前事实**，不是"没配对"。
        return Err(RemoteFault::Unreachable);
    }
    let device_id = m.key.device_id();
    let auth = NodeAuth {
        token: &m.token,
        device_id: &device_id,
        key: &m.key,
        hub_pub: &m.hub_pub,
    };
    for ep in &m.endpoints {
        let resp = match signed_request(ep, &auth, "GET", target, b"", extra, TIMEOUT) {
            Ok(resp) => resp,
            // 身份被拒原样穿出去（与 `sync` 同一条映射）。
            Err(rc_net::error::NetError::AuthRejected) => return Err(RemoteFault::Rejected),
            // 答了但答的不是契约里的东西（challenge 坏、签名验不过）：不是网络问题。
            Err(rc_net::error::NetError::BadResponse)
            | Err(rc_net::error::NetError::Signature) => return Err(RemoteFault::BadResponse),
            // 其余（连不上、TLS 失败、读超时）：试下一条路。
            Err(_) => continue,
        };
        return match resp.status {
            200 => serde_json::from_slice(&resp.body).map_err(|_| RemoteFault::BadResponse),
            401 | 403 | 409 => Err(RemoteFault::Rejected),
            _ => Err(RemoteFault::BadResponse),
        };
    }
    Err(RemoteFault::Unreachable)
}

/// **percent 编码的唯一实现**（四个目标串共用）。只放行 RFC 3986 的 unreserved
/// （`A-Z a-z 0-9 - _ . ~`），其余按 UTF-8 字节写成大写 `%XX`。
/// 服务端 `rc_net::http` 解 `%XX` 与 `+`；这里**不许**借 `+` 表示空格——
/// 那会让"查询里的加号"与"空格"在入参上不可分。
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `limit` 收口：`None` / `0` ⇒ 不带这个参数（用中心缺省），否则 `&limit=N`。
/// 中心的 `bounded()` 只收 `1..=max`，`0` 会被读成坏参数——缺省要在这一层说清。
fn push_limit(target: &mut String, limit: Option<usize>) {
    if let Some(n) = limit.filter(|n| *n > 0) {
        target.push_str(&format!("&limit={n}"));
    }
}

/// `GET /v1/transcript/sessions`：中心全部会话（`Trust::Lan`）。
pub fn sessions(
    data_dir: &Path,
    limit: Option<usize>,
    extra: &[(String, String)],
) -> Result<serde_json::Value, RemoteFault> {
    let mut target = String::from("/v1/transcript/sessions");
    if let Some(n) = limit.filter(|n| *n > 0) {
        target.push_str(&format!("?limit={n}"));
    }
    fetch(data_dir, &target, extra)
}

/// `GET /v1/transcript/session`：一个会话的轮正文回放（`limit` / `after_uid` 可选）。
pub fn session(
    data_dir: &Path,
    session_key: &str,
    after_uid: Option<&str>,
    limit: Option<usize>,
    extra: &[(String, String)],
) -> Result<serde_json::Value, RemoteFault> {
    let mut target = format!(
        "/v1/transcript/session?session_key={}",
        percent_encode(session_key)
    );
    if let Some(after) = after_uid {
        target.push_str(&format!("&after_uid={}", percent_encode(after)));
    }
    push_limit(&mut target, limit);
    fetch(data_dir, &target, extra)
}

/// `GET /v1/transcript/search`：中心 FTS 命中（`q` 必填）。
pub fn search(
    data_dir: &Path,
    q: &str,
    limit: Option<usize>,
    extra: &[(String, String)],
) -> Result<serde_json::Value, RemoteFault> {
    let mut target = format!("/v1/transcript/search?q={}", percent_encode(q));
    push_limit(&mut target, limit);
    fetch(data_dir, &target, extra)
}

/// `GET /v1/transcript/turn`：目标轮 + 同会话前后邻居（`before`/`after` 各 ≤ 10）。
pub fn turn(
    data_dir: &Path,
    turn_uid: &str,
    before: usize,
    after: usize,
    extra: &[(String, String)],
) -> Result<serde_json::Value, RemoteFault> {
    let target = format!(
        "/v1/transcript/turn?turn_uid={}&before={before}&after={after}",
        percent_encode(turn_uid)
    );
    fetch(data_dir, &target, extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 中文、空格、`&`、`%`、`/`：五格各自钉一个字节形状（服务端的 `%XX` 解码是唯一的消费者）。
    #[test]
    fn percent_encode_leaves_unreserved_alone_and_escapes_the_rest() {
        assert_eq!(percent_encode("abcXYZ019-_.~"), "abcXYZ019-_.~");
        assert_eq!(percent_encode("会话"), "%E4%BC%9A%E8%AF%9D");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("a&b=c"), "a%26b%3Dc");
        assert_eq!(percent_encode("100%"), "100%25");
        assert_eq!(percent_encode("a/b"), "a%2Fb");
    }

    /// 每个故障码的信封取值是封闭的（`remote_error` 里只许出现这四个）。
    #[test]
    fn fault_slugs_are_the_closed_set() {
        assert_eq!(RemoteFault::NotPaired.slug(), "not_paired");
        assert_eq!(RemoteFault::Unreachable.slug(), "unreachable");
        assert_eq!(RemoteFault::Rejected.slug(), "rejected");
        assert_eq!(RemoteFault::BadResponse.slug(), "bad_response");
    }
}
