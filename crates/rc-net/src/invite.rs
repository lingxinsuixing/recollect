//! 邀请票与中心身份（S4b1）。
//!
//! 票的签名只证明"某个私钥签过这串字节"；它**不是**"这是我要的中心"。
//! 首次信任的锚是"人把这张票交到你手上"这个动作——票里带的是 hub 指纹与 TLS 指纹，
//! 节点据此钉住链路并核验应答者。这里只负责三件事：没被改（签名）、是这张纸（版本/格式）、
//! 是这台中心的纸（`hub_fp`）。
//!
//! 两条 canonical 都住在这里，签发与验证共用一份：客户端与服务端各拼一遍字符串，
//! 就是下一次"签名只在一边对"的来源。

use crate::key;
use crate::protocol::sha256_hex;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};

/// 文本票前缀：`rc1.<b64url(canonical JSON)>.<sig>`。三段，多一段少一段都是格式错。
pub const TICKET_PREFIX: &str = "rc1";
pub const TICKET_VERSION: u32 = 1;
/// `/v1/identity` 的协议版本。与票版本是两个数：票换代不等于公告换代。
pub const IDENTITY_VERSION: u32 = 1;
/// `/v1/identity` 公告的有效期。短过期让"抓一份公告过几天再拿出来"这条重放路失效；
/// 发现层按它决定多久重问一次。
pub const IDENTITY_TTL_MS: i64 = 300_000;
/// 票的默认有效期：10 分钟。人把一条文本票抄到另一台机器上，这个窗口够用；
/// 再长就是"一张纸在聊天记录里躺着还能用"。
pub const DEFAULT_INVITE_TTL_MS: i64 = 600_000;

/// 票的载荷。字段顺序即 JSON 的键序（`serde` 按声明序写结构体），
/// 但**签名覆盖的是 [`invite_canonical`]，不是这段 JSON**：JSON 只是信封，
/// 它的键序稳定只是为了让同一张票编出同一串文本，不承担任何安全性质。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteTicket {
    pub v: u32,
    pub id: String,
    pub hub_fp: String,
    pub tls_fp: String,
    pub endpoints: Vec<String>,
    pub expires_at_ms: i64,
}

/// 错误面全静态短语：解析失败不回显输入（它整段都是秘密）。
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum InviteError {
    #[error("invite ticket is malformed")]
    Format,
    #[error("invite ticket version is unsupported")]
    Version,
    #[error("invite ticket cannot be encoded")]
    Encode,
    #[error("invite ticket signature is invalid")]
    Signature,
    #[error("invite ticket is not for this hub")]
    HubMismatch,
}

/// 中心指纹的**唯一**导出规则：`sha256(hub_pub 的 base64 字符串)` 的小写 hex。
///
/// 与 `device_id_of` 同一类东西——哈希的是**文本**而不是公钥字节，因为节点手上
/// 拿到的就是文本（配置、票、公告），多一次 base64 解码就多一处"两边的字节不一样"。
pub fn hub_fingerprint(pub_b64: &str) -> String {
    sha256_hex(pub_b64.as_bytes())
}

/// 票的签名体：换行分隔的七行，绝不在调用点手拼。
pub fn invite_canonical(t: &InviteTicket) -> String {
    format!(
        "recollect-invite-v1\n{}\n{}\n{}\n{}\n{}\n{}",
        t.v,
        t.id,
        t.hub_fp,
        t.tls_fp,
        t.endpoints.join(","),
        t.expires_at_ms
    )
}

/// `/v1/identity` 的响应形状。命名成结构体而不是两边各拼一遍 `json!`：
/// 节点（S4b2）解出来的就是这一份，`identity_canonical` 直接吃它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubIdentity {
    pub v: u32,
    pub hub_pub: String,
    pub hub_fp: String,
    pub tls_fp: String,
    pub epoch: u64,
    pub endpoints: Vec<String>,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    pub sig: String,
}

/// 身份公告的签名体。字段序与 JSON 无关：这里数的是"签了哪八个数"。
pub fn identity_canonical(id: &HubIdentity) -> String {
    format!(
        "recollect-identity-v1\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        id.v,
        id.hub_pub,
        id.hub_fp,
        id.tls_fp,
        id.epoch,
        id.endpoints.join(","),
        id.issued_at_ms,
        id.expires_at_ms
    )
}

/// 票的 JSON 信封。写成 `Result` 只有一个原因：`Encode` 这条错误变体得有个真的产生点，
/// 而不是让调用方以为"编码不可能失败"。
fn ticket_json(t: &InviteTicket) -> Result<Vec<u8>, InviteError> {
    serde_json::to_vec(t).map_err(|_| InviteError::Encode)
}

/// 文本票：`rc1.<b64url(JSON)>.<sig_b64>`。`sig_b64` 原样拼进来（它已是 base64url）。
pub fn encode_ticket(t: &InviteTicket, sig_b64: &str) -> String {
    let json = ticket_json(t).expect("an invite ticket always serializes");
    format!("{TICKET_PREFIX}.{}.{sig_b64}", URL_SAFE_NO_PAD.encode(json))
}

/// 只拆信封，不验签、不判版本：签名要等调用方拿到 hub 公钥才谈得上。
/// 失败一律 `Format`，不回显任何一个字节的输入。
pub fn decode_ticket(s: &str) -> Result<(InviteTicket, String), InviteError> {
    let mut parts = s.trim().split('.');
    let (Some(prefix), Some(payload), Some(sig), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(InviteError::Format);
    };
    if prefix != TICKET_PREFIX || sig.is_empty() || payload.is_empty() {
        return Err(InviteError::Format);
    }
    let json = URL_SAFE_NO_PAD.decode(payload).map_err(|_| InviteError::Format)?;
    let ticket: InviteTicket = serde_json::from_slice(&json).map_err(|_| InviteError::Format)?;
    Ok((ticket, sig.to_string()))
}

/// 验票：拆信封 → 验签 → 判版本 → 判"是不是这台中心的票"。
///
/// 版本的判定排在验签之后：`v` 虽然被签名覆盖，但"这张票是哪个时代的格式"
/// 只有在签名成立之后才值得信。`hub_fp` 的判定也住在这里而不是调用方——
/// "本中心的票"只有一处规则，pair 路由不该再抄一遍。
pub fn verify_ticket(s: &str, hub_pub_b64: &str) -> Result<InviteTicket, InviteError> {
    let (ticket, sig) = decode_ticket(s)?;
    if !key::verify_b64(hub_pub_b64, invite_canonical(&ticket).as_bytes(), &sig) {
        return Err(InviteError::Signature);
    }
    if ticket.v != TICKET_VERSION {
        return Err(InviteError::Version);
    }
    if ticket.hub_fp != hub_fingerprint(hub_pub_b64) {
        return Err(InviteError::HubMismatch);
    }
    Ok(ticket)
}
