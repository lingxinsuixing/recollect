//! `hub.toml` 的读取与校验。形状照 `rc-llm-cfg::load`：**缺文件是 `Ok(None)`**，
//! 由调用方响亮地说"没给配置"；配坏了是 `Err`。两者塌成"用默认值"就是把
//! 一台还没配的中心悄悄绑到一个没人要它绑的地址上。

use std::path::Path;

#[derive(Debug, Clone)]
pub struct HubConfig {
    pub bind: String,
    /// pair 响应原样返回的清单；顺序就是节点的尝试顺序。
    pub endpoints: Vec<String>,
    pub max_snapshot_bps: u64,
}

pub fn load(path: &Path) -> Result<Option<HubConfig>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("hub.toml is unreadable".to_string()),
    };
    let doc: toml::Table = toml::from_str(&text).map_err(|_| "hub.toml is not valid TOML")?;
    let hub = doc
        .get("hub")
        .and_then(toml::Value::as_table)
        .ok_or("hub.toml has no [hub] table")?;
    let bind = hub
        .get("bind")
        .and_then(toml::Value::as_str)
        .ok_or("hub.toml needs hub.bind")?
        .to_string();
    if bind.parse::<std::net::SocketAddr>().is_err() {
        return Err("hub.bind is not a socket address".to_string());
    }
    let arr = hub
        .get("endpoints")
        .and_then(toml::Value::as_array)
        .ok_or("hub.toml needs hub.endpoints")?;
    let mut endpoints = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let e = v
            .as_str()
            .ok_or_else(|| format!("endpoints[{i}]: not a string"))?;
        check_endpoint(e).map_err(|m| format!("endpoints[{i}]: {m}"))?;
        endpoints.push(e.to_string());
    }
    if endpoints.is_empty() {
        return Err("hub.endpoints is empty".to_string());
    }
    let max_snapshot_bps = match hub.get("max_snapshot_bps") {
        None => rc_net::protocol::DEFAULT_MAX_SNAPSHOT_BPS,
        Some(v) => v
            .as_integer()
            .ok_or("hub.max_snapshot_bps is not an integer")? as u64,
    };
    if max_snapshot_bps == 0 {
        // 0 在 `rc-net` 的节拍器里读作"不限速"，但在配置面它是配坏了：
        // 一个想限速却写成 0 的运维会得到与预期相反的后果。
        return Err("hub.max_snapshot_bps must be positive".to_string());
    }
    Ok(Some(HubConfig { bind, endpoints, max_snapshot_bps }))
}

/// endpoint 是 `http://host:port` 或 `https://host:port`，不许带路径。
/// 带路径的 endpoint 会让调用方把"服务根"和"某个前缀"混成一件事。
///
/// 中心自己**不消费** scheme（pair 响应原样公告出去），它只承诺"这台中心的地址长这样"：
/// `https://` 是给节点的信号（配 `hub_tls_fp` 并走 TLS），`http://` 留给回环开发面。
fn check_endpoint(s: &str) -> Result<(), &'static str> {
    let rest = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .ok_or("endpoint must be http:// or https://")?;
    if rest.is_empty() || rest.contains('/') {
        return Err("endpoint must be host:port with no path");
    }
    rest.parse::<std::net::SocketAddr>()
        .map_err(|_| "endpoint must be host:port")?;
    Ok(())
}
