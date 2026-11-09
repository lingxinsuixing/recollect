//! `llm.toml` 的读取与校验 + `secret_ref` 解析 + custom 通道的端点装配（A47 节 3）。
//!
//! 这一层**只碰值的位置，不碰值的值**（A43 那条不变式在这一层的形状）：它交出的内容是
//! "该去哪个箱子里取哪一格"（`secret_ref` 的原样字符串），不是 key 本身；取钥匙那条路在 hub 那一侧，
//! 拿回来以后**不写回任何 `Config` 字段**，所以 `Config`/`Seat` 里没有一处能存下密钥的 `String`。
//! 配置解析不许取时钟（判据 12a 点名的那三个文件之外，这一层一格都不许多 ⇒ 全仓计数仍是 3）。

pub mod keys;

use std::path::Path;

/// 一条座位：有序列表里的一项。`provider` 与 custom 二选一，互斥且必居其一。
#[derive(Debug, Clone)]
pub struct Seat {
    pub provider: Option<String>,
    pub model: Option<String>,
    /// `secret_ref` 的原样字符串（`箱:格`）。值一个字节都不在这里落。
    pub secret_ref: String,
    /// custom 通道：直接给端点，preset 表里没有它的行。
    pub endpoint: Option<Endpoint>,
    /// 这一座的单次输出预算（token）。缺省走 `rc-llm-http::MAX_BUDGET`；
    /// 只有确实需要更大思考预算的座位才显式覆盖，超过对端上界会在发送时被拒。
    pub budget: Option<u32>,
    /// 只覆盖推导作业的输出预算；缺省与 `budget` 同值。抽取/审计/去重仍走默认预算。
    pub derive_budget: Option<u32>,
    /// 采样温度（`0.0..=2.0`）。**缺省是 `None`，即不发这一格、走 provider 默认**——
    /// 不许在这一层补成 `0.0`，那会把"没配"变成一次静默的行为改变。
    /// 锁死温度的行（preset 的 `temperature_locked`）由构造点拒，不在这里查表。
    pub temperature: Option<f64>,
    /// 思考深度档位（OpenAI 兼容协议的 `reasoning_effort`）。`None` 就是不发这一格、
    /// 走 provider 默认——与 `temperature` 同一条纪律，不在这里补默认值。
    /// 闭集为 MiniMax M3.1 文档轴的 `low|medium|high|xhigh|max`：`none` 不被接受
    /// （M3.1-Flash 关不掉思考，传 `disabled`/`none` 会 400）。
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub base_url: String,
    /// 协议形状二选一，逐字沿用 `Protocol::slug()` 那两个名字。
    pub protocol: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub seats: Vec<Seat>,
}

/// 读 `llm.toml`。**缺文件返回 `Ok(None)`**：这一层的"没配"与"配坏了"必须是两种后果——
/// 前者是"这台机器还没轮到配 live"（回放跑得住），后者是响亮失败。
/// 至于"没配就跑 live 会怎样"，那是调用方（hub）的分支，不在这里判。
pub fn load(path: &Path) -> Result<Option<Config>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("llm.toml is unreadable".to_string()),
    };
    let doc: toml::Table = toml::from_str(&text).map_err(|_| "llm.toml is not valid TOML")?;
    let arr = doc
        .get("providers")
        .and_then(toml::Value::as_array)
        .ok_or("llm.toml has no `providers` array")?;
    let mut seats = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        // 序号是唯一的定位手段：坏值在消息里一律是脱敏后的形状（见 `keys::check_secret_ref`），
        // "第几项"才是运维真正要看的那一样。
        seats.push(read_seat(item).map_err(|e| format!("providers[{i}]: {e}"))?);
    }
    if seats.is_empty() {
        // 空列表不是"没配"，是"配了但一家都没有"——`Rotating::new` 那边也会拒，
        // 但拒在这一层才说得出是哪个文件哪一行坏了。
        return Err("`providers` list is empty".to_string());
    }
    Ok(Some(Config { seats }))
}

fn read_seat(item: &toml::Value) -> Result<Seat, String> {
    let table = item.as_table().ok_or("a `providers` entry is not a table")?;
    let get = |k: &str| table.get(k).and_then(toml::Value::as_str).map(str::to_string);
    let provider = get("provider");
    let endpoint = match table.get("endpoint") {
        Some(toml::Value::Table(t)) => {
            let base_url = t
                .get("base_url")
                .and_then(toml::Value::as_str)
                .ok_or("`endpoint` needs `base_url`")?
                .to_string();
            let protocol = t
                .get("protocol")
                .and_then(toml::Value::as_str)
                .ok_or("`endpoint` needs `protocol`")?
                .to_string();
            keys::check_protocol(&protocol)?;
            Some(Endpoint { base_url, protocol })
        }
        Some(_) => return Err("`endpoint` must be a table".to_string()),
        None => None,
    };
    if provider.is_some() == endpoint.is_some() {
        return Err("a seat must be either `provider = ...` or `endpoint = {...}`, not both"
            .to_string());
    }
    if endpoint.is_some() && get("model").is_none() {
        // custom 那一侧没有 preset 行可查，模型名只能来自配置。
        return Err("custom seat needs `model`".to_string());
    }
    let secret_ref = get("secret_ref").ok_or("seat has no `secret_ref`")?;
    keys::check_secret_ref(&secret_ref)?;
    let parse_budget = |key: &str| -> Result<Option<u32>, String> {
        match table.get(key) {
            Some(v) => {
                let n = v
                    .as_integer()
                    .ok_or_else(|| format!("`{key}` must be an integer"))?;
                if n <= 0 || n > u32::MAX as i64 {
                    return Err(format!("`{key}` must be a positive u32"));
                }
                Ok(Some(n as u32))
            }
            None => Ok(None),
        }
    };
    let budget = parse_budget("budget")?;
    let derive_budget = parse_budget("derive_budget")?;
    // 类型错走与 `budget` 同一条坏配置路径（不静默回落）：`as_float` 只收 TOML 浮点，
    // 整数写法 `1` 也被拒——这一层的规矩是"逐字读，不归一"。
    let temperature = match table.get("temperature") {
        Some(v) => {
            let t = v.as_float().ok_or("`temperature` must be a float")?;
            // `contains` 对 NaN/inf 也是 false，于是这两种形状同样落进"越界"。
            if !(0.0..=2.0).contains(&t) {
                return Err("`temperature` must be within 0.0..=2.0".to_string());
            }
            Some(t)
        }
        None => None,
    };
    // 思考档位：闭集，逐字读。`none` 是 SenseNova 文档里的"关思考"
    // （`reasoning_effort:"none"`）；它**不是每家都收**——MiniMax M3.1-Flash 对 `none`
    // 会 400，所以它只在明确支持该值的座位上配，校验层不做厂家白名单。
    let reasoning_effort = match table.get("reasoning_effort") {
        Some(v) => {
            let s = v.as_str().ok_or("`reasoning_effort` must be a string")?;
            if !["none", "low", "medium", "high", "xhigh", "max"].contains(&s) {
                return Err(
                    "`reasoning_effort` must be one of none|low|medium|high|xhigh|max".to_string()
                );
            }
            Some(s.to_string())
        }
        None => None,
    };
    Ok(Seat { provider, model: get("model"), secret_ref, endpoint, budget, derive_budget,
              temperature, reasoning_effort })
}
