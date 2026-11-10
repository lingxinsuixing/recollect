//! `secret_ref` 与协议名的校验（A47 节 3 那三条闸门的落点）。
//!
//! 分成两个文件不是洁癖：判据 N5 要 grep "redact 那个模块里没有厂家名字面"，
//! 而这一边**注定要有**厂家名与协议名（它就是名单）。混在一个文件里那条判据就自相矛盾了。

/// 协议名的唯一一份合法取值。与 `rc-llm-http` 那边 `Protocol::slug()` 是同两个字符串，
/// 同源性由 hub 侧的对表用例钉（`crates/hub/tests/protocol_names.rs`，A47-2 落地）——
/// **这一层不许依赖那一层**：doctor 只要配置面，绝不能因为读配置而把 ureq 拉进依赖树。
pub const PROTOCOL_NAMES: [&str; 2] = ["openai-chat", "anthropic-messages"];

/// `secret_ref` 的形状：**箱名 + `:` + 格名**，两侧都只许 safe token。
///
/// 用 `:` 不用 `/` 的那条理由在这里落地（节 3）：`rc_contract::safe_token` 的白名单不含斜杠，
/// 取 `:` 就把 A43 那句"挡 `openai/../../..`"落成了真码——路径分隔符一旦进合法集，
/// "值只经过不留下"就变成"值可以走到任何目录"。
pub fn check_secret_ref(s: &str) -> Result<(), String> {
    let mut parts = s.split(':');
    let box_name = parts.next().ok_or("secret_ref needs a `box:slot` shape")?;
    let slot = parts.next().ok_or("secret_ref needs a `box:slot` shape")?;
    if parts.next().is_some() {
        return Err("secret_ref has more than one `:`".to_string());
    }
    for part in [box_name, slot] {
        if part.is_empty() {
            return Err("secret_ref has an empty half".to_string());
        }
        if !is_safe_token(part) {
            // 报错串里回显的必须是**脱敏后**的那一份：被拒的正是"长成路径的串"那一族，
            // 把原样拼进错误消息等于让这条规则自己破自己守的那件事。
            return Err(format!(
                "secret_ref half `{}` is not a safe token",
                rc_contract::safe_token(part)
            ));
        }
    }
    Ok(())
}

/// **调用 `rc_contract::safe_token` 而不是重写它**（那条规则自己的注释："这条规则只有一个家……
/// 一个形状判据写两遍=下一次漂移"）。它本身是脱敏函数（坏了返回 `"(redacted)"` 而不是 `Err`），
/// 所以校验的写法是"回显相等"：原样出来才是合法串。空串与斜杠、空格、反斜杠、`..` 全在这一条下死。
pub fn is_safe_token(s: &str) -> bool {
    rc_contract::safe_token(s) == s
}

/// 协议名只许那两个，且逐字。拼错不许回落默认（节 1 那条接缝的 custom 那一支）。
/// 报错里回显同样走脱敏：这一格的值来自 `llm.toml`，而 `llm.toml` 的坏值正是要被指着说的东西——
/// 定位由调用方补的序号负责，原串不进消息。
pub fn check_protocol(s: &str) -> Result<(), String> {
    if PROTOCOL_NAMES.contains(&s) {
        Ok(())
    } else {
        Err(format!(
            "unknown protocol name `{}`",
            rc_contract::safe_token(s)
        ))
    }
}
