//! `node.toml` 的读与校验。
//!
//! **照 `rc_llm_cfg::load` 的形状**：缺文件 ⇒ `Ok(None)`（"这台机器还没配"），
//! 坏文件 ⇒ `Err(封闭短语)`（"配坏了"）。两种后果必须分开——
//! 缺配置时静默用默认值，等于让节点连到一台没人指定的中心。

use std::path::{Path, PathBuf};

/// 端点清单里的一项。`label` 是**路名**：它进状态文件与告警，所以必须是安全 token。
///
/// `tls_fp` 来自顶层 `hub_tls_fp`（**附着到每一项**）：同一个中心的所有路名共用一枚证书，
/// 逐项写 pin 只会制造"这条路忘了配"的漏洞。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub label: String,
    pub host: String,
    pub port: u16,
    pub tls_fp: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeConfig {
    pub data_dir: PathBuf,
    pub device_key_path: PathBuf,
    /// 钉死值。`None` = 不钉（用配对时中心给的那个）。
    pub hub_pub: Option<String>,
    /// 全局钉死的那枚中心证书指纹（小写 64 hex）。**端点上的 `tls_fp` 也来自这里**，
    /// 但顶层这一格另有读者：发现与状态文件里的地址都用它建 TLS（配置优先，其次状态）。
    pub hub_tls_fp: Option<String>,
    /// 手写端点清单。**允许为空**：凭票加入的节点把地址落在状态文件里
    /// （`SyncState::paired_endpoints`），配置里一个端点都不用写。
    pub endpoints: Vec<Endpoint>,
    /// `llm.toml` 住在哪儿（B2b 的离线自蒸才读它；本批只落配置）。
    /// **这不是 key**：配置里没有 key 字段，值只住在 `secret_ref` 指向的密文箱里。
    pub llm_config: Option<PathBuf>,
    /// B1a 的 `auto_unlock=false` 语义保持不变：只有它是 true 且离线满一天才自蒸。
    pub auto_unlock: bool,
    /// S4d1：发现应答的 UDP 端口（`None` = `rc_net::discovery::DISCOVERY_PORT`）。
    /// 之所以可配：同一个 LAN 里跑两台中心时，端口是唯一能把它们分开的东西。
    pub discovery_port: Option<u16>,
    /// S4d1：发现的目标地址，**只收字面 IPv4**（`None` = 广播）。
    /// 拿到它就能把"问一句"钉在本机或某一段，测试与多网口机器都要这个。
    pub discovery_target: Option<String>,
    /// 自动重发现的总开关（缺省 `true`）。`false` ⇒ 目标表为空，`sync` 永不问一句：
    /// 给运维一个关掉自动重连的可达入口，而不是只能靠改代码。
    pub discovery: bool,
    /// 一次发现的窗口。只影响"顺手问一句"，与 `--timeout-s` 分开：一个不肯应答的
    /// 网段不该让整轮同步多等一整个上行/下行超时。
    pub discovery_timeout_ms: u64,
    /// 显式接受**非回环**端点的明文传输（缺省 `false`）。
    /// 明文是开发面：不写它时，非回环端点必须有 `hub_tls_fp`，否则加载即失败。
    pub allow_plaintext: bool,
}

/// 主机是不是回环。**解析不出 IP 的一律按非回环算**：主机名要 DNS，而"明文只许
/// 在本机"这条纪律不该由一次解析结果决定。
fn host_is_loopback(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().map(|ip| ip.is_loopback()).unwrap_or(false)
}

pub fn load(path: &Path) -> Result<Option<NodeConfig>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("node.toml is unreadable".to_string()),
    };
    let doc: toml::Table = toml::from_str(&text).map_err(|_| "node.toml is not valid TOML")?;
    let data_dir = doc
        .get("data_dir")
        .and_then(toml::Value::as_str)
        .ok_or("node.toml has no `data_dir`")?;
    let device_key_path = doc
        .get("device_key_path")
        .and_then(toml::Value::as_str)
        .ok_or("node.toml has no `device_key_path`")?;
    let hub_pub = doc
        .get("hub_pub")
        .and_then(toml::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    // `llm_config` 与 `auto_unlock` 是**顶层键**（不加 `[llm]` 段）：前者是路径、后者是开关。
    // 给了就必须是对的类型；空串路径等同没给到一个能读的文件，按错配拒绝（沉默的 `Some("")`
    // 只会在 B2b 变成"读一个不存在的空路径"）。
    let llm_config = match doc.get("llm_config") {
        None => None,
        Some(toml::Value::String(s)) if s.is_empty() => {
            return Err("node.toml: `llm_config` is empty".to_string());
        }
        Some(toml::Value::String(s)) => Some(PathBuf::from(s)),
        Some(_) => return Err("node.toml: `llm_config` is not a string".to_string()),
    };
    let auto_unlock = match doc.get("auto_unlock") {
        None => false,
        Some(toml::Value::Boolean(b)) => *b,
        Some(_) => return Err("node.toml: `auto_unlock` is not a boolean".to_string()),
    };
    // 中心证书指纹：大小写不敏感（归一成小写），但**形状必须正好 64 个 hex**。
    // 空串不是"不钉"：不钉靠在配置里完全不写这个键表达，写一个空值是把"配坏了"装成"没配"。
    // 不做 trim：多一个空格就是抄错了，静默吃掉它会让下一次核对无从下手。
    let hub_tls_fp = match doc.get("hub_tls_fp") {
        None => None,
        Some(toml::Value::String(s)) => {
            if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("node.toml: `hub_tls_fp` must be 64 hex characters".to_string());
            }
            Some(s.to_ascii_lowercase())
        }
        Some(_) => return Err("node.toml: `hub_tls_fp` is not a string".to_string()),
    };
    // S4d1 的两个发现键：**缺省 = 开**（广播 + 固定端口）。`discovery = false` 才关；
    // 不写它们的老配置行为**不再**是一字不变——每轮失败会多问一句（缺省 600ms），
    // 这正是自动重连要付的代价，所以这里不再声称"无变化"。
    // 给了就必须能用：端口 0 与主机名都是"配坏了"，静默退回默认值等于让一次自动重连
    // 去问一个没人听的地址。发现层只绑 `0.0.0.0`，IPv6 目标在本批是明确的配置错，
    // 而不是"连不上"（后者会被离线窗口读成中心离线）。
    let discovery_port = match doc.get("discovery_port") {
        None => None,
        Some(toml::Value::Integer(n)) if (1..=65535).contains(n) => Some(*n as u16),
        Some(_) => return Err("node.toml: `discovery_port` is out of range".to_string()),
    };
    let discovery_target = match doc.get("discovery_target") {
        None => None,
        Some(toml::Value::String(s))
            if s.parse::<std::net::IpAddr>().map(|ip| ip.is_ipv4()).unwrap_or(false) =>
        {
            Some(s.clone())
        }
        Some(_) => {
            return Err("node.toml: `discovery_target` must be an IPv4 address".to_string())
        }
    };
    let discovery = match doc.get("discovery") {
        None => true,
        Some(toml::Value::Boolean(b)) => *b,
        Some(_) => return Err("node.toml: `discovery` is not a boolean".to_string()),
    };
    let discovery_timeout_ms = match doc.get("discovery_timeout_ms") {
        None => 600,
        Some(toml::Value::Integer(n)) if (50..=10000).contains(n) => *n as u64,
        Some(_) => {
            return Err("node.toml: `discovery_timeout_ms` is out of range".to_string())
        }
    };
    let allow_plaintext = match doc.get("allow_plaintext") {
        None => false,
        Some(toml::Value::Boolean(b)) => *b,
        Some(_) => return Err("node.toml: `allow_plaintext` is not a boolean".to_string()),
    };
    // 端点**可以是空的**：S4b2 起，节点可以凭一张邀请票加入，地址由票带来并落在
    // 状态文件里。于是"能读但还没地方可连"是配置的合法状态，而"没有端点"这条
    // 错误挪到真要用地址的那一步（`pair`/`sync` 的 `no_endpoint`）——
    // 一个还没配对的新机器不该因为这份配置读不出来而连 `--invite` 都跑不了。
    let arr: &[toml::Value] = match doc.get("endpoints") {
        None => &[],
        Some(toml::Value::Array(a)) => a.as_slice(),
        Some(_) => return Err("node.toml: `endpoints` is not an array".to_string()),
    };
    let mut endpoints = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let t = item
            .as_table()
            .ok_or("node.toml: an `endpoints` entry is not a table")?;
        let label = t
            .get("label")
            .and_then(toml::Value::as_str)
            .ok_or("node.toml: an `endpoints` entry has no `label`")?;
        if rc_contract::safe_token(label) != label {
            return Err(format!("node.toml: endpoints[{i}] label is not a safe token"));
        }
        let host = t
            .get("host")
            .and_then(toml::Value::as_str)
            .ok_or("node.toml: an `endpoints` entry has no `host`")?;
        let port = t
            .get("port")
            .and_then(toml::Value::as_integer)
            .ok_or("node.toml: an `endpoints` entry has no `port`")?;
        if !(1..=65535).contains(&port) {
            return Err(format!("node.toml: endpoints[{i}] port is out of range"));
        }
        endpoints.push(Endpoint {
            label: label.to_string(),
            host: host.to_string(),
            port: port as u16,
            tls_fp: hub_tls_fp.clone(),
        });
    }
    // R1：明文只能是开发面。**判定只能在配置这一侧做**——状态文件里票带来的那枚
    // 指纹在加载时还不知道；凭票加入的节点配置里没有端点，因此不受这条规则影响。
    // 静态句、不回显地址：地址不是秘密，但错误面没有理由多印一份。
    if hub_tls_fp.is_none()
        && !allow_plaintext
        && endpoints.iter().any(|e| !host_is_loopback(&e.host))
    {
        return Err(
            "node.toml endpoint without hub_tls_fp must be loopback (plaintext is dev-only)"
                .to_string(),
        );
    }
    Ok(Some(NodeConfig {
        data_dir: PathBuf::from(data_dir),
        device_key_path: PathBuf::from(device_key_path),
        hub_pub,
        hub_tls_fp,
        endpoints,
        llm_config,
        auto_unlock,
        discovery_port,
        discovery_target,
        discovery,
        discovery_timeout_ms,
        allow_plaintext,
    }))
}
