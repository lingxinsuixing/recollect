//! 同步编排：**push 与 pull 共用一条代码路径**，`--once` = 先 push 后 pull。
//!
//! 三条"不做事"的分支是判定的一部分，各自有名字：
//! - **上行短路**：待发条数为 0 ⇒ 不发任何请求（空 body 会被中心按 `bad_request` 拒）；
//! - **下行短路 · 无快照**：中心答 `no_snapshot` ⇒ 不下载，记"无可用记忆"；
//! - **下行短路 · 同一份清单**：`accept_manifest` 判 `AlreadyCurrent` ⇒ 不下载。
//!
//! **回并的硬序**（B3b）：暂存层存在且本轮含 Pull ⇒ `push → rows → 确认 → pull`。
//! rows 的 200 是 pull 的闸；rows 失败不 pull、不丢层，下一轮带着同一载荷重放（中心幂等）。
//! 只有 pull 成功（不是 `no_memory`）且层仍在时才丢层；丢层失败 ⇒ 层留着，下一轮重放。
//!
//! **端点降级**：按清单的顺序逐个试，第一个连上的就是"当前路径"。
//! 清单有两个来源（S4b2）：`node.toml` 里手写的那份在前；凭票配对时落进状态文件的
//! 那份接在后面（同 `host:port` 的不重复，配置优先）。合并发生在 `main.rs`，
//! 于是"配置里写了旧地址、状态里有发现来的新地址"两条都会试。
//! 换路**不重配对**——token、私钥、中心公钥都与路名无关。
//!
//! **自动重连（S4d1）**：中心换了 IP/端口或换了证书时，旧端点会一条都不通。
//! 这一轮的结果是 `endpoint_unreachable`（一条都没答过）或 `tls_failed`（答了但
//! 对端身份不对）时，才用已钉死的身份问一句局域网"我那台中心现在在哪"，问到
//! 新地址就改地址重试**一次**；只有重试真跑通才把新地址写进状态文件。
//! 三道门按顺序把"别家的中心"挡在外面：公钥必须是我钉的那
//! 一把、证书指纹必须是我钉的那一枚、地址必须是能走的 `https://IP:port`。同一把钥匙换了
//! 证书是**响亮失败**（`identity_mismatch`）：那时该查证书轮换，不是等网络恢复。

use crate::state::{CurrentManifest, PartialDownload, PushCursor, SelfDistill, SyncState};
use crate::staging;
use rc_contract::{ExchangeFile, RawSession, RawTurn, TurnTier};
use rc_ledger::transcript::sessions_after;
use rc_net::key::{write_private_file, DeviceKey};
use rc_net::node::{download_snapshot, signed_request, upload_rows, Endpoint, NodeAuth};
use rc_net::snapshot::{accept_manifest, AcceptOutcome, Manifest};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 一次同步的结果。**全是计数与路名**，没有正文、没有路径。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncOutcome {
    pub pushed: i64,
    pub push_skipped: bool,
    pub pull_skipped: bool,
    /// 中心还没有快照 ⇒ 节点"无可用记忆"。
    pub no_memory: bool,
    pub pulled: bool,
    pub endpoint: String,
    pub resumes: u32,
    pub rate_bps: u64,
    /// 本批被中心**吸收**的行数（重放时是 0）。
    pub rows_uploaded: usize,
    /// 暂存层这一轮走完了回并（rows 过了，pull 成功，层已丢）。
    pub reconciled: bool,
}

/// 同步失败的**封闭码**。`Display` 只放码，不放错误串、不放路径。
///
/// 码按**下一步动作**分，不按"哪一层报的"分：`no_pairing`（本机没有 token）与
/// `not_paired`（中心不接受这份身份）是两件事；`no_memory`（中心还没有快照）与
/// `bad_response`（中心答了，但不是契约里的东西）也是两件事。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncError {
    NoEndpoint,
    /// 本机没有 token / 没有钉死的中心公钥：**还没配对**。
    NotPaired,
    /// 中心不接受这份身份（token 撤销、设备头不符、配对码过期/用过）。
    AuthRejected,
    HubKeyMismatch,
    BadTokenFile,
    StoreUnavailable,
    EndpointUnreachable,
    /// TLS 握手或指纹钉死失败：对端不是钉住的那一枚。**不是** `endpoint_unreachable`——
    /// 下一步是查指纹（`hub tls show` 与 `hub_tls_fp`），不是等网络恢复。
    TlsFailed,
    DownloadFailed,
    ChunkMismatch,
    IngestRejected,
    /// 回并的行批次被拒 / 走不完：**层留着**，下一轮重放（中心按内容寻址幂等）。
    RowsRejected,
    BadResponse,
    /// 邀请票不能用：拆不开、版本不认、端点不是 `https://IP:port`、或中心答"这票不对"。
    InviteInvalid,
    /// 票过期了。**本地先判一次**（票里就写着 `expires_at_ms`），中心再判一次。
    InviteExpired,
    InviteUsed,
    InviteRevoked,
    InviteUnknown,
    /// 设备 id 已被一台活设备占着：换一台机器、或先用 `hub revoke` 叫停旧的。
    DeviceExists,
    /// `/v1/identity` 的应答不是票里那台中心：公钥/指纹/链路任一对不上。
    /// **不是** `endpoint_unreachable`——网络通着，说话的不是我以为的那一台。
    IdentityMismatch,
    /// 本地与中心的 schema 区间不相交：静态句 "schema incompatible — upgrade one side"。
    /// 配对阶段就响亮失败，别等到每轮 pull 都被折成 `bad_response`。
    SchemaUnsupported,
}

impl SyncError {
    pub fn code(self) -> &'static str {
        match self {
            SyncError::NoEndpoint => "no_endpoint",
            SyncError::NotPaired => "no_pairing",
            SyncError::AuthRejected => "not_paired",
            SyncError::HubKeyMismatch => "hub_key_mismatch",
            SyncError::BadTokenFile => "bad_token_file",
            SyncError::StoreUnavailable => "store_unavailable",
            SyncError::EndpointUnreachable => "endpoint_unreachable",
            SyncError::TlsFailed => "tls_failed",
            SyncError::DownloadFailed => "download_failed",
            SyncError::ChunkMismatch => "chunk_mismatch",
            SyncError::IngestRejected => "ingest_failed",
            SyncError::RowsRejected => "rows_failed",
            SyncError::BadResponse => "bad_response",
            SyncError::InviteInvalid => "invite_invalid",
            SyncError::InviteExpired => "invite_expired",
            SyncError::InviteUsed => "invite_used",
            SyncError::InviteRevoked => "invite_revoked",
            SyncError::InviteUnknown => "invite_unknown",
            SyncError::DeviceExists => "device_exists",
            SyncError::IdentityMismatch => "identity_mismatch",
            SyncError::SchemaUnsupported => "schema_unsupported",
        }
    }
}

/// 中心的 401 / 403 / 409：这份身份（token、设备头、配对码）不再被接受。
/// 与 `no_pairing` 分开——那两个码的下一步一个叫"重新配对"，一个叫"先配对"。
fn auth_rejected(status: u16) -> Option<SyncError> {
    matches!(status, 401 | 403 | 409).then_some(SyncError::AuthRejected)
}

/// 传输层错误 → 同步码。**身份被拒必须原样穿过**：折成 `EndpointUnreachable` 会让
/// "中心不接受这份身份"读成"网络不通"，而两者的下一步动作相反（重新配对 vs 重试）。
/// TLS 同理：错指纹/握手失败是**对端身份**的问题，不是链路不通。
fn transport_error(e: rc_net::error::NetError) -> SyncError {
    match e {
        rc_net::error::NetError::AuthRejected => SyncError::AuthRejected,
        rc_net::error::NetError::Tls => SyncError::TlsFailed,
        _ => SyncError::EndpointUnreachable,
    }
}

/// 一次同步要用的全部东西。**参数化到这一步**：时间由调用方给，
/// 于是整条同步路径可以在测试里跑成一个确定的过程。
///
/// `key` 是**借**的：`DeviceKey` 不实现 `Clone`，而"签名用的钥匙"与
/// "设备 id 的来源"**必须**是同一把——中心按 token 找设备、按设备验签，
/// 两把钥匙会让签名与设备名对不上，而那种失败长得像"中心不认我"。
pub struct SyncPlan<'a> {
    pub endpoints: Vec<Endpoint>,
    pub token: String,
    pub device_id: String,
    pub key: &'a DeviceKey,
    pub hub_pub: String,
    /// **有效**钉死指纹：配置 `hub_tls_fp` 优先，其次状态里的 `paired_tls_fp`。
    /// 连接与发现共用这一格——两套钉死值是"证书轮换后误报 `identity_mismatch`"的来源。
    /// `None` ⇒ 本计划不发现：拿不到指纹就判不了"答话的是不是同一台中心"。
    pub hub_tls_fp: Option<String>,
    pub data_dir: PathBuf,
    /// 发现目标（`IP:UDP端口`）。**空表 = 本计划不发现**：`discovery = false` 与测试
    /// 走的就是这条，于是"自动重连"永远是一个可以关掉的东西。
    pub discovery: Vec<SocketAddr>,
    /// 问一句"中心在哪"的窗口。与 `timeout` 分开：发现是顺手问一句，不该让一次同步
    /// 因为一个不肯应答的网段而多等一个完整的上行/下行超时。
    pub discovery_timeout: Duration,
}

/// `sync --once` 的真身。`mode` 决定做哪一半。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Both,
    Push,
    Pull,
}

pub fn sync_once<'a>(
    plan: &SyncPlan<'a>,
    state: &mut SyncState,
    mode: Mode,
    now_ms: i64,
    timeout: Duration,
) -> Result<SyncOutcome, SyncError> {
    if plan.endpoints.is_empty() {
        return Err(SyncError::NoEndpoint);
    }
    if plan.token.is_empty() || plan.hub_pub.is_empty() {
        return Err(SyncError::NotPaired);
    }
    // 第一轮：现有的"逐条路试"原样跑。只有**一条都没答过**（`endpoint_unreachable`）
    // 或**答了但对端身份不对**（`tls_failed`）才说明"中心可能换地址/换证书了"。
    // 后者也要给一次发现机会：否则同一地址上换了证书时 `identity_mismatch` 永远出不来。
    // 有哪条答过（拒绝/坏应答/坏行）就到此为止——问题不在路由上。
    let current_error = match run_round(&plan.endpoints, plan, state, mode, now_ms, timeout) {
        Ok(outcome) => return Ok(outcome),
        Err(e) => e,
    };
    if !matches!(current_error, SyncError::EndpointUnreachable | SyncError::TlsFailed) {
        return Err(current_error);
    }
    // `tls_failed` 这一支只在**状态里记过"我那台中心"**时才去发现：证书轮换是已配对节点
    // 的事件，发现的价值是"同键换证"能响亮报出来。状态里没有中心身份时，这一响只能来自
    // 配置里那枚 pin 与证书不一致——`tls_failed` 已经把下一步（`hub tls show` 对
    // `hub_tls_fp`）说全了，再发现一次只会撞上同一枚 pin，换个码不增加任何信息。
    if current_error == SyncError::TlsFailed && state.paired_hub_pub.is_empty() {
        return Err(current_error);
    }
    // 触发条件之一：**没有有效证书指纹就不发现**——拿不到它就无法判"答话的是不是
    // 同一台中心"，此时宁可不发现也不能随便连一台。有效值来自配置优先、其次状态。
    let Some(pinned_tls_fp) = plan.hub_tls_fp.clone() else {
        return Err(current_error);
    };
    // 另一个关闭开关：本计划不发现（`discovery = false` / 测试）。
    if plan.discovery.is_empty() {
        return Err(current_error);
    }
    // 一圈只问一次：这里没有循环，也没有退避——要不要再问是外部调度器的事。
    // 钟沿用本轮同步的 `now_ms`：发现与上行/下行必须活在同一个钟面上，库自己不去取。
    let candidates = match rc_net::discovery::discover(
        &plan.discovery,
        plan.discovery_timeout,
        now_ms,
    ) {
        Ok(v) => v,
        // 发现本身失败（绑不上、发不出去）不改变同步的失败码：这一轮的结论仍是"不通"，
        // 换成别的码会把"局域网里没人答"读成"中心拒绝了我"。
        Err(_) => return Err(current_error),
    };
    // **真的发了 probe**：三条出口（空表 / 同集 / 重试成败）都算"问过"，这里一次写掉。
    state.last_rediscover_at_ms = now_ms;
    // `identity_mismatch` 原样穿出去：同键换证书是响亮失败，不是"没找到"。
    let eps = pick_rediscovered(&plan.hub_pub, &pinned_tls_fp, &candidates)?;
    if eps.is_empty() {
        state.rediscover_miss += 1;
        return Err(current_error);
    }
    // 发现回来的还是同一批地址（顺序无关）：改地址重试没有意义，也不该把 miss 刷成 ok。
    // 但"问过"要可见：`last_rediscover_at_ms` 已在上面写过。
    if same_endpoints(&plan.endpoints, &eps) {
        state.rediscover_miss += 1;
        return Err(current_error);
    }
    // **先重试、后落盘**：新地址上真跑通这一轮才算"重发现成功"。
    // 失败就保留旧地址簿（否则一次坏公告会永久换掉唯一可用的地址），
    // 并原样返回重试的那个错误（折成别的码会把"新地址不通"读成"没发现"）。
    match run_round(&eps, plan, state, mode, now_ms, timeout) {
        Ok(outcome) => {
            state.paired_endpoints = eps.iter().map(endpoint_url).collect();
            state.rediscover_ok += 1;
            Ok(outcome)
        }
        Err(e) => {
            state.rediscover_miss += 1;
            Err(e)
        }
    }
}

/// 在一组端点上跑**一轮**：按顺序逐条试，第一个成功的就是这一轮的路径。
///
/// 抽出来是为了让"重发现之后重试"复用同一段降级逻辑——复制一份就是下一次
/// "两条路对'答过'的判断不一样"的来源。
fn run_round<'a>(
    endpoints: &[Endpoint],
    plan: &SyncPlan<'a>,
    state: &mut SyncState,
    mode: Mode,
    now_ms: i64,
    timeout: Duration,
) -> Result<SyncOutcome, SyncError> {
    let mut last_unreachable = SyncError::EndpointUnreachable;
    let mut answered: Option<SyncError> = None;
    for ep in rc_net::node::order_endpoints(endpoints, &state.current_endpoint) {
        match run_against(ep, plan, state, mode, now_ms, timeout) {
            Ok(outcome) => return Ok(outcome),
            Err(SyncError::EndpointUnreachable) => last_unreachable = SyncError::EndpointUnreachable,
            // 有一条路答过（拒绝/坏应答/库错），就说明网络通着；不能因为另一条路不可达
            // 把整轮读成“断网”，否则离线窗口会被错误累积。
            Err(e) => answered = answered.or(Some(e)),
        }
    }
    Err(answered.unwrap_or(last_unreachable))
}

/// 从发现到的候选里挑出"就是我那台中心"的地址，按输入顺序去重（`host:port` 相同算同一条）。
///
/// 三道门，缺一不可：
/// 1. `hub_pub` 与钉死值**逐字节相等**——网上只有它一台也不算数，别的中心不该被采纳；
/// 2. `tls_fp` 与钉死值相等——**先扫完整张表再判**：同一把钥匙的多条公告里只要有一条
///    指纹对上，就用对上的那几条（不再因为在前面先遇到一条不等的就整体失败）；
///    一条都不等才算证书轮换/冒充，那是 `identity_mismatch`；
/// 3. 地址要能过 [`endpoint_from_https_url`]（非 https / 非字面 IP / 坏端口丢掉）。
///
/// 其余候选**静默跳过**：包括"`hub_pub` 对不上但 `tls_fp` 撞上"的冒充者——指纹是公开的，
/// 撞上它不构成任何信任。
pub fn pick_rediscovered(
    pinned_hub_pub: &str,
    pinned_tls_fp: &str,
    candidates: &[rc_net::discovery::Candidate],
) -> Result<Vec<Endpoint>, SyncError> {
    let same_key: Vec<&rc_net::discovery::Candidate> =
        candidates.iter().filter(|c| c.hub_pub == pinned_hub_pub).collect();
    // 大小写不敏感：指纹是 hex，票里落的就是小写，但"同一枚指纹"不该因为抄成大写
    // 而被读成"换了证书"。
    let matching: Vec<&rc_net::discovery::Candidate> = same_key
        .iter()
        .copied()
        .filter(|c| c.tls_fp.eq_ignore_ascii_case(pinned_tls_fp))
        .collect();
    // 同键但一条指纹都不等：中心换了证书，或有人在冒充。响亮失败，一个地址都不返回。
    if matching.is_empty() && !same_key.is_empty() {
        return Err(SyncError::IdentityMismatch);
    }
    let mut picked: Vec<Endpoint> = Vec::new();
    for c in matching {
        for url in &c.endpoints {
            // 序号取自已经收下的条数：`paired-N` 与配对/`sync` 用的是同一套路名，
            // 于是 `order_endpoints` 的"上次那条路先试"在两条入口之间指同一条路。
            let Some(ep) = endpoint_from_https_url(url, Some(pinned_tls_fp), picked.len()) else {
                continue;
            };
            if picked.iter().any(|p| p.host == ep.host && p.port == ep.port) {
                continue;
            }
            picked.push(ep);
        }
    }
    Ok(picked)
}

/// 两组端点是不是"同一批地址"（**忽略顺序**：中心重启后公告顺序变了不代表换了地址；
/// 只比 `host:port`——路名是本地生成的序号，换地址前后都叫 `paired-1`，拿它比会得到
/// "一样"的假答案）。
fn same_endpoints(a: &[Endpoint], b: &[Endpoint]) -> bool {
    a.len() == b.len()
        && a.iter().all(|x| b.iter().any(|y| x.host == y.host && x.port == y.port))
}

/// 端点 → 状态文件里那种 URL 串（`https://IP:port`，IPv6 加方括号）。
/// 与 `endpoint_from_https_url` 是同一条格式的两端：写进去的东西下一次读得回来。
fn endpoint_url(ep: &Endpoint) -> String {
    if ep.host.contains(':') {
        format!("https://[{}]:{}", ep.host, ep.port)
    } else {
        format!("https://{}:{}", ep.host, ep.port)
    }
}

/// 对**一条**路名跑完这一轮。降级由上一层的循环做。
fn run_against<'a>(
    ep: &Endpoint,
    plan: &SyncPlan<'a>,
    state: &mut SyncState,
    mode: Mode,
    now_ms: i64,
    timeout: Duration,
) -> Result<SyncOutcome, SyncError> {
    let auth = NodeAuth {
        token: &plan.token,
        device_id: &plan.device_id,
        key: plan.key,
        hub_pub: &plan.hub_pub,
    };
    let mut outcome = SyncOutcome { endpoint: ep.label.clone(), ..SyncOutcome::default() };

    if matches!(mode, Mode::Both | Mode::Push) {
        let pushed = push(ep, &auth, plan, state, now_ms, timeout)?;
        outcome.pushed = pushed;
        outcome.push_skipped = pushed == 0;
    }
    if matches!(mode, Mode::Both | Mode::Pull) {
        let staged = staging::is_active(&plan.data_dir);
        let mut rows_uploaded = 0usize;
        let mut reconciled = false;
        let pulled = if staged {
            // 进入回并：中心回来了，先回传暂存层，确认之后才许下拉。
            state.self_distill = SelfDistill::Reconciling;
            let (uploaded, pulled) = reconcile_then_pull(
                || upload_staged_rows(ep, &auth, plan, timeout),
                || pull(ep, &auth, plan, state, now_ms, timeout),
            )?;
            rows_uploaded = uploaded;
            pulled
        } else {
            pull(ep, &auth, plan, state, now_ms, timeout)?
        };
        if staged && !pulled.no_memory {
            // pull 成功（下载或已是最新）且层还在 ⇒ 整层丢弃。中心已经吸收过这批行，
            // 重放幂等；丢层失败就留着，下一轮再走一遍。
            staging::drop_layer(&plan.data_dir).map_err(|_| SyncError::StoreUnavailable)?;
            reconciled = true;
        }
        // `no_memory`（中心还没有快照）不丢层：这一轮的回并没有走完，下一轮重放。
        outcome.rows_uploaded = rows_uploaded;
        outcome.reconciled = reconciled;
        outcome.pulled = pulled.pulled;
        outcome.no_memory = pulled.no_memory;
        outcome.pull_skipped = !pulled.pulled && !pulled.no_memory;
        outcome.resumes = pulled.resumes;
        outcome.rate_bps = pulled.rate_bps;
    }
    state.current_endpoint = ep.label.clone();
    Ok(outcome)
}

/// 回并的硬序：**rows 先、pull 后**。抽成一个函数是为了让集成测试用两个记录顺序的闭包
/// 证明它（反序注入必须红）；生产调用点只传真上传与真 pull。
pub fn reconcile_then_pull<R, P>(rows: R, pull: P) -> Result<(usize, Pulled), SyncError>
where
    R: FnOnce() -> Result<usize, SyncError>,
    P: FnOnce() -> Result<Pulled, SyncError>,
{
    let uploaded = rows()?;
    let pulled = pull()?;
    Ok((uploaded, pulled))
}

/// 把暂存层的新行交给中心。空标记 ⇒ 0（不发请求，仍可 pull）。
///
/// 失败映射按派发档：身份被拒原样穿；中心的坏应答折 `RowsRejected`（层留着）；
/// 其余传输错误折 `EndpointUnreachable`（下一轮重放）。
fn upload_staged_rows(
    ep: &Endpoint,
    auth: &NodeAuth<'_>,
    plan: &SyncPlan<'_>,
    timeout: Duration,
) -> Result<usize, SyncError> {
    let Some(body) = crate::reconcile::build_rows_payload(&plan.data_dir)? else {
        return Ok(0);
    };
    let ack = upload_rows(ep, auth, &body, timeout).map_err(|e| match e {
        rc_net::error::NetError::AuthRejected => SyncError::AuthRejected,
        rc_net::error::NetError::Tls => SyncError::TlsFailed,
        rc_net::error::NetError::BadResponse => SyncError::RowsRejected,
        _ => SyncError::EndpointUnreachable,
    })?;
    Ok(ack.absorbed)
}

/// 上行。返回**发出去的轮数**（0 = 短路）。
fn push<'a>(
    ep: &Endpoint,
    auth: &NodeAuth<'_>,
    plan: &SyncPlan<'a>,
    state: &mut SyncState,
    now_ms: i64,
    timeout: Duration,
) -> Result<i64, SyncError> {
    let root = rc_store::NodeRoot::open_node(&plan.data_dir, None)
        .map_err(|_| SyncError::StoreUnavailable)?;
    let picked = sessions_after(
        root.transcript(),
        state.push_cursor.at_ms,
        &state.push_cursor.turn_uid,
    )
    .map_err(|_| SyncError::StoreUnavailable)?;
    let turns: usize = picked.iter().map(|s| s.turns.len()).sum();
    state.pending_turns = turns as i64;
    if turns == 0 {
        return Ok(0);
    }
    let body = build_ndjson(&picked);
    let resp = signed_request(ep, auth, "POST", "/v1/ingest", body.as_bytes(), &[], timeout)
        .map_err(transport_error)?;
    // 先看码再看体：401/403 的错误体可能根本不是 JSON，先解析会把"身份被拒"
    // 读成"应答坏了"（更早的版本里两者又一起被读成"无可用记忆"）。
    if let Some(e) = auth_rejected(resp.status) {
        return Err(e);
    }
    if resp.status == 422 {
        // 整批被拒：**不推水位**。那批还在队列里，下次重来。
        return Err(SyncError::IngestRejected);
    }
    if resp.status != 200 {
        return Err(SyncError::BadResponse);
    }
    let doc: serde_json::Value = serde_json::from_slice(&resp.body)
        .map_err(|_| SyncError::BadResponse)?;
    let accepted = doc.get("turns_accepted").and_then(|v| v.as_i64()).unwrap_or(0);
    let rejected = doc.get("turns_rejected").and_then(|v| v.as_i64()).unwrap_or(0);
    state.last_push_accepted = accepted;
    state.last_push_rejected = rejected;
    state.last_push_at_ms = now_ms;
    // 游标推到**发出去的那批**的最大值。逐条被拒的轮不会重来
    // （中心已经把它记进审计，再发一次还是被拒），所以推过去是对的；
    // 整批 422 的情形在上面已经 return 掉了，水位不动。
    //
    // 取的是所有轮的最大值，不是"最后一个会话的末轮"：`sessions_after` 按会话
    // **首次出现**排序、会话内的轮按全局序追加，两会话交错时全局最大轮可能
    // 落在前面那个会话里——取末轮会让游标退回去，下一轮重推。
    if let Some(last) = picked
        .iter()
        .flat_map(|s| s.turns.iter())
        .max_by(|a, b| (a.ingested_at_ms, &a.turn_uid).cmp(&(b.ingested_at_ms, &b.turn_uid)))
    {
        state.push_cursor = PushCursor { at_ms: last.ingested_at_ms, turn_uid: last.turn_uid.clone() };
    }
    Ok(turns as i64)
}

/// 下行。返回"下到了没有 / 中心有没有快照 / 断了几次 / 速率多少"。
///
/// `pub` 只为 [`reconcile_then_pull`] 的测试接缝：集成测试用闭包记录调用顺序，
/// 需要一个可构造的返回值（生产读者仍只经 `run_against`）。
pub struct Pulled {
    pub pulled: bool,
    pub no_memory: bool,
    pub resumes: u32,
    pub rate_bps: u64,
}

fn pull<'a>(
    ep: &Endpoint,
    auth: &NodeAuth<'_>,
    plan: &SyncPlan<'a>,
    state: &mut SyncState,
    now_ms: i64,
    timeout: Duration,
) -> Result<Pulled, SyncError> {
    let resp = signed_request(ep, auth, "GET", "/v1/state", b"", &[], timeout)
        .map_err(transport_error)?;
    if let Some(e) = auth_rejected(resp.status) {
        return Err(e);
    }
    if resp.status == 404 {
        // 中心还没有快照：不是失败，是"无可用记忆"。**只有这一条 404 配这个码**。
        return Ok(Pulled { pulled: false, no_memory: true, resumes: 0, rate_bps: 0 });
    }
    if resp.status != 200 {
        return Err(SyncError::BadResponse);
    }
    let doc: serde_json::Value = serde_json::from_slice(&resp.body)
        .map_err(|_| SyncError::BadResponse)?;
    let manifest = manifest_of(&doc)?;
    let current = current_manifest(state);
    let outcome = accept_manifest(
        &plan.hub_pub,
        &manifest,
        current.as_ref(),
        rc_store::capability::NORM_VERSION,
    )
    .map_err(|e| match e {
        // 验签失败的唯一现实来源：节点钉死的公钥不是签这份清单的那把。
        rc_net::error::NetError::Signature => SyncError::HubKeyMismatch,
        // schema/norm 不兼容、sha 形状不对、纪元倒退、同号不同字节：中心答的东西
        // 这一代节点用不了，或不该出现。都不是"没有记忆"。
        _ => SyncError::BadResponse,
    })?;
    // 中心签名清单的**原文**落档（含 `signature`）。`AlreadyCurrent` 也落：
    // 同一份清单可能上一轮没写成。自备份的下行段靠它证明"可验签"。
    save_center_manifest(&plan.data_dir, manifest.epoch, manifest.seq, &resp.body)?;
    let dir = crate::state::replica_dir(&plan.data_dir);
    let versioned = crate::state::replica_file(&plan.data_dir, manifest.epoch, manifest.seq);
    if outcome == AcceptOutcome::AlreadyCurrent {
        // 状态文件说"已是最新"只覆盖元数据；**副本字节可能已经不在了**
        // （人工删过，或 mcp 在旧句柄上退化成无副本）。这时必须重下，
        // 否则 `node sync` 永远判当前，而读面永远说"无可用记忆"。
        // 拿清单的字节数顺手核一下长度：它是一个免费的、只在异常时才触发的自愈入口。
        if complete(&versioned, manifest.bytes) {
            return Ok(Pulled { pulled: false, no_memory: false, resumes: 0, rate_bps: 0 });
        }
        // 升级前的固定名：能迁就迁；被读着（Windows）就照旧当当前副本。
        // 下一次真换代自然切到代际名，清理只在当前代落地后才动手。
        let legacy = dir.join("memory.db");
        if complete(&legacy, manifest.bytes) {
            let _ = std::fs::rename(&legacy, &versioned);
            return Ok(Pulled { pulled: false, no_memory: false, resumes: 0, rate_bps: 0 });
        }
    }
    std::fs::create_dir_all(&dir).map_err(|_| SyncError::StoreUnavailable)?;
    let part = versioned.with_extension("part");
    let already = if state.partial.epoch == manifest.epoch
        && state.partial.seq == manifest.seq
    {
        state.partial.received
    } else {
        0
    };
    let started = std::time::Instant::now();
    let got = match download_snapshot(ep, auth, &manifest, &part, already, timeout) {
        Ok(g) => g,
        Err(e) => {
            // 失败时把盘上已有的字节数留给下一轮。`download_snapshot` 只认
            // "盘长 == 声称的偏移"，所以这里记的必须是**实际落盘长度**；
            // 记 `received` 之类的内存读数会让下一轮从头来。
            let on_disk = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
            state.partial =
                PartialDownload { epoch: manifest.epoch, seq: manifest.seq, received: on_disk };
            return Err(match e {
                rc_net::error::NetError::ChunkMismatch => SyncError::ChunkMismatch,
                rc_net::error::NetError::AuthRejected => SyncError::AuthRejected,
                rc_net::error::NetError::Tls => SyncError::TlsFailed,
                _ => SyncError::DownloadFailed,
            });
        }
    };
    let elapsed = started.elapsed().as_secs_f64().max(0.001);
    let rate_bps = (got.bytes as f64 / elapsed) as u64;
    // 整份 sha 终验：**分片各自核过还不够**，整份拼起来仍可能不是那一份。
    let whole = std::fs::read(&part).map_err(|_| SyncError::DownloadFailed)?;
    if rc_net::protocol::sha256_hex(&whole) != manifest.sha256 {
        let _ = std::fs::remove_file(&part);
        return Err(SyncError::ChunkMismatch);
    }
    std::fs::rename(&part, &versioned).map_err(|_| SyncError::DownloadFailed)?;
    state.current_manifest = CurrentManifest {
        epoch: manifest.epoch,
        seq: manifest.seq,
        sha256: manifest.sha256.clone(),
        created_at_ms: manifest.created_at_ms,
    };
    state.partial = Default::default();
    state.resume_count += got.resumes as u64;
    state.last_rate_bps = rate_bps;
    state.last_pull_at_ms = now_ms;
    Ok(Pulled { pulled: true, no_memory: false, resumes: got.resumes, rate_bps })
}

/// 把中心 `/v1/state` 的**原始 JSON** 落到 `replica/manifest-<epoch>-<seq>.json`。
///
/// **不重新序列化**：签名只覆盖五个字段，重排 JSON 不影响验签，但原样落盘最省事，
/// 也不会在将来引入"我们把签名体改了"这种无法解释的差异。同目录临时名 + rename：
/// 崩在两步之间不会留下半截清单。
fn save_center_manifest(
    data_dir: &Path,
    epoch: u64,
    seq: u64,
    body: &[u8],
) -> Result<(), SyncError> {
    let path = crate::state::manifest_file(data_dir, epoch, seq);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|_| SyncError::StoreUnavailable)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(|_| SyncError::StoreUnavailable)?;
    std::fs::rename(&tmp, &path).map_err(|_| SyncError::StoreUnavailable)
}

/// "这一份副本已经完整落地了吗"：文件在且长度与清单的 `bytes` 相等。
/// 只核长度不核 sha 是有意的——sha 在下载那一轮已经整份验过，这里做的是**存在性自愈**，
/// 把每一次跳过都变成一次整份读会让 `node sync` 白白重读几十兆。
fn complete(path: &std::path::Path, bytes: u64) -> bool {
    std::fs::metadata(path).map(|m| m.is_file() && m.len() == bytes).unwrap_or(false)
}

/// 中心 `/v1/state` 的响应 → 清单。**只认认识的字段**，
/// 多出来的字段忽略（契约演进：HTTP JSON 只增字段不减）。
fn manifest_of(doc: &serde_json::Value) -> Result<Manifest, SyncError> {
    let num = |k: &str| doc.get(k).and_then(|v| v.as_u64()).ok_or(SyncError::BadResponse);
    let schema = doc
        .get("schema_version")
        .and_then(|v| v.as_i64())
        .ok_or(SyncError::BadResponse)? as i32;
    let norm = doc
        .get("norm_version")
        .and_then(|v| v.as_u64())
        .ok_or(SyncError::BadResponse)? as u32;
    Ok(Manifest {
        epoch: num("epoch")?,
        seq: num("seq")?,
        sha256: doc.get("sha256").and_then(|v| v.as_str()).ok_or(SyncError::BadResponse)?.to_string(),
        schema_version: schema,
        norm_version: norm,
        bytes: num("bytes")?,
        created_at_ms: doc
            .get("created_at_ms")
            .and_then(|v| v.as_i64())
            .ok_or(SyncError::BadResponse)?,
        signature: doc
            .get("signature")
            .and_then(|v| v.as_str())
            .ok_or(SyncError::BadResponse)?
            .to_string(),
    })
}

/// 本地当前那份清单。**还没有过快照时是 `None`**——那不是"epoch 0 的空清单"，
/// 后者会让 `accept_manifest` 把第一份当成同号冲突。
fn current_manifest(state: &SyncState) -> Option<Manifest> {
    if state.current_manifest.epoch == 0 && state.current_manifest.seq == 0 {
        return None;
    }
    Some(Manifest {
        epoch: state.current_manifest.epoch,
        seq: state.current_manifest.seq,
        sha256: state.current_manifest.sha256.clone(),
        schema_version: rc_store::ddl::MEMORY_VERSION,
        norm_version: rc_store::capability::NORM_VERSION,
        bytes: 0,
        created_at_ms: state.current_manifest.created_at_ms,
        signature: String::new(),
    })
}

/// 把挑出来的会话变成 NDJSON。**一行一个会话**（契约：一条会话只许出现在一行里）。
///
/// tier 的还原是承重的：`IdBearing` 的轮**带着自己的 id** 上行，
/// 中心照单全收；其余两级的轮不带 id，中心按 `(device_id, session_key, role, body, created_at_ms)`
/// 重算——而那五个输入与本机落库时用的完全一样 ⇒ 重算出来是同一个 `turn_uid` ⇒ 幂等。
fn build_ndjson(sessions: &[rc_ledger::transcript::LocalSession]) -> String {
    let mut out = String::new();
    for s in sessions {
        let turns: Vec<RawTurn> = s
            .turns
            .iter()
            .map(|t| {
                let tier = TurnTier::from_rank(t.tier);
                RawTurn {
                    turn_uid: match tier {
                        Some(TurnTier::IdBearing) => Some(t.turn_uid.clone()),
                        _ => None,
                    },
                    seq: None,
                    tier,
                    role: t.role.clone(),
                    body: t.body.clone(),
                    created_at_ms: Some(t.created_at_ms),
                    // 会话库没有轮级 speaker（本地转录层不存这一格）：节点上行一律不带，
                    // 旧路径与今天逐字相同。
                    speaker: None,
                }
            })
            .collect();
        let file = ExchangeFile {
            format_version: 1,
            sessions: vec![RawSession {
                session_key: s.session_key.clone(),
                device_id: s.device_id.clone(),
                source_agent: s.source_agent.clone(),
                machine: s.machine.clone(),
                project_hint: s.project_hint.clone(),
                last_saved_at_ms: s.last_saved_at_ms,
                // 身份层 I1：节点上行透传的三格主人身份。`LocalSession` 里没有这三格
                // （它们是 Agent 侧的口述/运行时信息，不出现在本机转录层），所以这里只能是
                // `None` ⇒ 中心不盖章；要盖章走 `hub principal claim` 或导入侧透传。
                owner_principal_id: None,
                principal_name: None,
                principal_source: None,
                turns,
            }],
        };
        out.push_str(&serde_json::to_string(&file).expect("the exchange file serializes"));
        out.push('\n');
    }
    out
}

/// 配对。**不带签名**（配对码就是凭证），但**带自己的设备公钥**——
/// 中心从公钥导出设备 id，所以"顶着别人的 id 配对"在中心那一侧就被拒了。
pub fn pair(
    ep: &Endpoint,
    code: &str,
    key: &DeviceKey,
    timeout: Duration,
) -> Result<Paired, SyncError> {
    let body = serde_json::json!({
        "code": code,
        "device_id": key.device_id(),
        "pubkey": key.public_b64(),
    });
    let body = serde_json::to_vec(&body).expect("the pair request serializes");
    let resp = rc_net::node::request(
        ep,
        &rc_net::node::NodeRequest {
            method: "POST".to_string(),
            target: "/v1/pair".to_string(),
            headers: Vec::new(),
            body,
        },
        timeout,
    )
    .map_err(|e| match e {
        // 配对阶段同样要把 TLS 失败原样穿出去：折成不可达会让"指纹配错了"
        // 读成"中心没开"，而前者只能靠 `hub tls show` 修。
        rc_net::error::NetError::Tls => SyncError::TlsFailed,
        _ => SyncError::EndpointUnreachable,
    })?;
    if let Some(e) = auth_rejected(resp.status) {
        return Err(e);
    }
    if resp.status != 200 {
        return Err(SyncError::BadResponse);
    }
    read_paired(&resp)
}

/// 配对成功的应答 → [`Paired`]。两条入口（码/票）共用一份解析：
/// 字段少认一个就是"其中一条路拿不到 token"。
///
/// `/v1/pair` 里中心主动给了 `schema: {min, max}`：区间与本地不相交 ⇒ 响亮失败
/// （`schema_unsupported`），别让"版本不兼容"一路配对成功、之后每轮 pull 都读成
/// `bad_response`。**应答缺这一格不判**：旧中心不印它，缺省不是"不兼容"。
fn read_paired(resp: &rc_net::node::NodeResponse) -> Result<Paired, SyncError> {
    let doc: serde_json::Value =
        serde_json::from_slice(&resp.body).map_err(|_| SyncError::BadResponse)?;
    if let Some(schema) = doc.get("schema") {
        let min = schema.get("min").and_then(|v| v.as_i64()).ok_or(SyncError::BadResponse)?;
        let max = schema.get("max").and_then(|v| v.as_i64()).ok_or(SyncError::BadResponse)?;
        let local_min = rc_net::protocol::SCHEMA_MIN as i64;
        let local_max = rc_net::protocol::SCHEMA_MAX as i64;
        if max < local_min || min > local_max {
            return Err(SyncError::SchemaUnsupported);
        }
    }
    let token = doc
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or(SyncError::BadResponse)?
        .to_string();
    let hub_pub = doc
        .get("hub_pub")
        .and_then(|v| v.as_str())
        .ok_or(SyncError::BadResponse)?
        .to_string();
    let endpoints: Vec<String> = doc
        .get("endpoints")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    Ok(Paired { token, hub_pub, endpoints })
}

/// 配对的结果。`endpoints` 是中心公告的**端点清单**（里面是地址）。
///
/// 输出**只印条数，不印地址**（`announced_endpoints`）。凭票加入时票里的地址会落进
/// 状态文件（`SyncState::paired_endpoints`）——那是"这台机器该去哪儿找中心"的读面，
/// 不是秘密；token 仍然只住 `node-token`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paired {
    pub token: String,
    pub hub_pub: String,
    pub endpoints: Vec<String>,
}

/// 把 token 写到盘上（私钥级权限）。**写盘失败即响亮失败**：
/// 一个权限没收紧的 token 文件比没有 token 更坏。
pub fn write_token(data_dir: &std::path::Path, token: &str) -> Result<(), SyncError> {
    let path = crate::state::token_path(data_dir);
    write_private_file(&path, format!("{token}\n").as_bytes())
        .map_err(|_| SyncError::BadTokenFile)
}

/// 票/状态里的端点 URL → 一条路。**只认 `https://IP:port`**：
///
/// - 非 `https://` ⇒ `None`。票钉的是证书指纹，明文走不了这一支；
/// - 主机必须是**字面 IP**（IPv6 可带方括号）：票是"人转抄的一行文本"，
///   在这里做 DNS 解析会把"连哪台"交给另一层，与"钉死"矛盾；
/// - 端口 1..=65535；
/// - 指纹必须是 64 hex：没有指纹的 https 端点在这条路上没有意义。
///
/// `label` 由序号生成（`paired-1`、`paired-2`…）：路名要进状态文件与 `status`，
/// 而票里只有地址。**配对与 sync 用同一套序号**，于是 `order_endpoints` 的
/// "上次那条路先试"在两条入口之间指同一条路。
pub fn endpoint_from_https_url(
    url: &str,
    tls_fp: Option<&str>,
    index: usize,
) -> Option<Endpoint> {
    let rest = url.strip_prefix("https://")?;
    let (host, port) = rest.rsplit_once(':')?;
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let host: std::net::IpAddr = host.parse().ok()?;
    let port: u16 = port.parse().ok()?;
    if port == 0 {
        return None;
    }
    let fp = tls_fp?;
    if fp.len() != 64 || !fp.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(Endpoint {
        label: format!("paired-{}", index + 1),
        host: host.to_string(),
        port,
        tls_fp: Some(fp.to_ascii_lowercase()),
    })
}

/// 票里的端点 → 逐条的路，**顺序同票**（票是人手抄的一行，"先试哪条"是签发方的选择）。
///
/// 整张票要么全认、要么全不认：一张签好的票里混进一条不认得的地址，说明发/收两侧
/// 对格式的理解已经分叉，静默丢掉它只会让"少试了一条路"变成无法解释的现象。
pub fn ticket_endpoints(ticket: &rc_net::invite::InviteTicket) -> Result<Vec<Endpoint>, SyncError> {
    if ticket.endpoints.is_empty() {
        return Err(SyncError::InviteInvalid);
    }
    ticket
        .endpoints
        .iter()
        .enumerate()
        .map(|(i, url)| {
            endpoint_from_https_url(url, Some(&ticket.tls_fp), i).ok_or(SyncError::InviteInvalid)
        })
        .collect()
}

/// `GET /v1/identity`：匿名、不签名。**只解析，不判真伪**——判定在 [`verify_identity`]。
fn fetch_identity(
    ep: &Endpoint,
    timeout: Duration,
) -> Result<rc_net::invite::HubIdentity, SyncError> {
    let resp = rc_net::node::request(
        ep,
        &rc_net::node::NodeRequest {
            method: "GET".to_string(),
            target: "/v1/identity".to_string(),
            headers: Vec::new(),
            body: Vec::new(),
        },
        timeout,
    )
    .map_err(|e| match e {
        // 链路的身份问题原样穿出去：错指纹/握手失败是"对端不是那一个"，
        // 折成不可达会让人去查网络。
        rc_net::error::NetError::Tls => SyncError::TlsFailed,
        _ => SyncError::EndpointUnreachable,
    })?;
    if resp.status != 200 {
        return Err(SyncError::BadResponse);
    }
    serde_json::from_slice(&resp.body).map_err(|_| SyncError::BadResponse)
}

/// 三比较 + 一验签，缺一不可：
///
/// 1. 公告用它自己的 `hub_pub` 验签 —— "这八个字段是某把私钥签的"；
/// 2. `hub_fp == 票的 hub_fp` —— "签它的这把就是发票的那把"（票里钉的中心名）；
/// 3. `hub_fp == hub_fingerprint(hub_pub)` —— "这个名字真是这把公钥的"，
///    挡住"签名自洽但名字对不上"的自签公钥；
/// 4. `tls_fp == 票的 tls_fp` —— "这条链路就是票说的那条"。
///
/// 任一不符都只有一个码：`identity_mismatch`。分开报会让"哪一格不对"变成
/// 一张可枚举的表，而票与人各持一半，攻击面只该有一个答案。
pub fn verify_identity(
    id: &rc_net::invite::HubIdentity,
    ticket: &rc_net::invite::InviteTicket,
    ticket_text: &str,
) -> Result<(), SyncError> {
    let canonical = rc_net::invite::identity_canonical(id);
    if !rc_net::key::verify_b64(&id.hub_pub, canonical.as_bytes(), &id.sig) {
        return Err(SyncError::IdentityMismatch);
    }
    if id.hub_fp != ticket.hub_fp
        || id.hub_fp != rc_net::invite::hub_fingerprint(&id.hub_pub)
    {
        return Err(SyncError::IdentityMismatch);
    }
    if id.tls_fp != ticket.tls_fp {
        return Err(SyncError::IdentityMismatch);
    }
    // S4b2-fix：拿到 hub_pub 后**先验票自身的签名**。只改签名段的票不应被发到中心，
    // 更不该被中心折成 `bad_response`；它是"票不可信"，本地就该报 `invite_invalid`。
    rc_net::invite::verify_ticket(ticket_text, &id.hub_pub)
        .map_err(|_| SyncError::InviteInvalid)?;
    Ok(())
}

/// `POST /v1/pair` 的票这一支：`{ticket, device_id, pubkey}`。
///
/// 非 200 时**读 body 的 `error` 码**再翻译：中心已经把"这张票哪里不能用"
/// 分成了五个动作不同的答案（再试/过期/叫停/伪造/换机器），
/// 折成一个 `bad_response` 等于让人去猜。认不出的码仍折 `BadResponse`。
fn pair_with_ticket(
    ep: &Endpoint,
    ticket: &str,
    key: &DeviceKey,
    timeout: Duration,
) -> Result<Paired, SyncError> {
    let body = serde_json::json!({
        "ticket": ticket,
        "device_id": key.device_id(),
        "pubkey": key.public_b64(),
    });
    let body = serde_json::to_vec(&body).expect("the pair request serializes");
    let resp = rc_net::node::request(
        ep,
        &rc_net::node::NodeRequest {
            method: "POST".to_string(),
            target: "/v1/pair".to_string(),
            headers: Vec::new(),
            body,
        },
        timeout,
    )
    .map_err(|e| match e {
        rc_net::error::NetError::Tls => SyncError::TlsFailed,
        _ => SyncError::EndpointUnreachable,
    })?;
    if resp.status != 200 {
        let code = serde_json::from_slice::<serde_json::Value>(&resp.body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(|s| s.to_string()))
            .unwrap_or_default();
        return Err(match code.as_str() {
            "invite_used" => SyncError::InviteUsed,
            "invite_expired" => SyncError::InviteExpired,
            "invite_revoked" => SyncError::InviteRevoked,
            "invite_unknown" => SyncError::InviteUnknown,
            "device_exists" => SyncError::DeviceExists,
            // 票这一侧还有两件事要分开报：中心/链路的身份不对（查证书、查中心配置）
            // 与"票本身不可用"（重新出票）。折成 `bad_response` 等于让人去猜。
            "invite_hub_mismatch" | "invite_tls_mismatch" => SyncError::IdentityMismatch,
            "invite_version" | "invite_invalid" => SyncError::InviteInvalid,
            _ => SyncError::BadResponse,
        });
    }
    read_paired(&resp)
}

/// 凭票配对：**逐个端点、顺序同票**，每条都先问"你是谁"再拿票换 token。
///
/// 全部失败 ⇒ 返回**最后一个**端点的封闭码：票里的顺序就是签发方给的优先级，
/// 最后一个也就是"最像真的那台中心"的说法。
pub fn pair_invite(
    endpoints: &[Endpoint],
    ticket_text: &str,
    ticket: &rc_net::invite::InviteTicket,
    key: &DeviceKey,
    timeout: Duration,
) -> Result<Paired, SyncError> {
    let mut last = SyncError::NoEndpoint;
    for ep in endpoints {
        let step = fetch_identity(ep, timeout)
            .and_then(|id| verify_identity(&id, ticket, ticket_text))
            .and_then(|()| pair_with_ticket(ep, ticket_text, key, timeout));
        match step {
            // 应答里的 `hub_pub` 必须就是票钉的那一把。链路已由票的 `tls_fp` 钉住，
            // 所以这一格正常永远不会响；它守的是"票是地址与信任的唯一来源"这句承诺本身。
            Ok(paired) if rc_net::invite::hub_fingerprint(&paired.hub_pub) != ticket.hub_fp => {
                return Err(SyncError::IdentityMismatch);
            }
            Ok(paired) => return Ok(paired),
            Err(e) => last = e,
        }
    }
    Err(last)
}