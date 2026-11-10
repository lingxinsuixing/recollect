//! W39 刀 C：中心转录只读路由（细案 `2026-11-09-w39-remote-transcript` §二/§3.1）。
//!
//! 四条 GET 是 `rc_search::transcript` 四个读数在中心 HTTP 面的**唯一出口**：
//! 可见性判定仍然只有 [`rc_search::transcript::session_visible`] 一处实现——这里
//! **不写**第二份 SQL、不复制归属表。远端通道走 `Trust::Lan`（已配对设备读全部会话，
//! W39 用户 2026-11-09 裁定）；本地/中心模式的工具面仍走 `Trust::Ownership`。
//!
//! 鉴权在 `serve` 里复用 [`HubState::authenticate`](crate::serve::HubState) 的五道
//! （token/未撤销/device 头/签名/nonce），**不加** capability：与 `/v1/ingest` 同档。
//! 请求头 `x-recollect-agent` / `x-recollect-principal` 只进审计（自报，不是准入条件）。
//!
//! 审计：每次成功读（含空读数）落一行 `audit_log(action='transcript_read')`；
//! `detail` 只有 `{route, agent, principal, hits}`，**不含正文**。审计写不进去 ⇒ 503：
//! 一条读不出去就不会发生（"谁读了谁的会话"是这一面的可溯源性本身）。
//!
//! 空读数分型与本地读数一致：会话不可见/不存在 ⇒ `turns:[]`（200，不 404）；
//! 轮不可见/不存在 ⇒ 空窗口（200，不泄露存在性）。

use rc_ledger::device::{append_audit, DeviceRow};
use rc_net::http::{Request, Response};
use rc_search::transcript::{
    self, Trust, DEFAULT_TURN_LIMIT, MAX_SESSION_LIMIT, MAX_TURN_LIMIT, MAX_WINDOW_SIDE,
};
use rc_store::{LedgerHandle, TranscriptHandle};
use serde_json::{json, Value};

/// 所有信封共用的来源标（与边缘回落时的 `"local"` 成对，细案 §3.2）。
const SOURCE: &str = "center";
const AUDIT_ACTION: &str = "transcript_read";
const ROUTE_SESSIONS: &str = "/v1/transcript/sessions";
const ROUTE_SESSION: &str = "/v1/transcript/session";
const ROUTE_SEARCH: &str = "/v1/transcript/search";
const ROUTE_TURN: &str = "/v1/transcript/turn";

fn bad_request() -> Response {
    Response::error(400, "bad_request")
}

fn store_unavailable() -> Response {
    Response::error(503, "store_unavailable")
}

/// 中心 epoch：与 `X-Recollect-Epoch` 头、签名规范串同一个读数（`serve` 的既有口径）。
fn epoch(t: &TranscriptHandle) -> Result<u64, Response> {
    rc_ledger::transcript::center_epoch(t).map_err(|_| store_unavailable())
}

/// 收口在 `min..=max` 的整数参数；缺省取 `default`。越界/非数字 ⇒ 400。
fn bounded(req: &Request, name: &str, min: usize, max: usize, default: usize) -> Result<usize, Response> {
    match req.query_get(name) {
        None => Ok(default),
        Some(raw) => match raw.parse::<usize>() {
            Ok(value) if (min..=max).contains(&value) => Ok(value),
            _ => Err(bad_request()),
        },
    }
}

/// 可选的上限参数：缺省 `None` = 全量；给了就必须在 `min..=max` 之内。
fn optional_bounded(
    req: &Request,
    name: &str,
    min: usize,
    max: usize,
) -> Result<Option<usize>, Response> {
    match req.query_get(name) {
        None => Ok(None),
        Some(raw) => match raw.parse::<usize>() {
            Ok(value) if (min..=max).contains(&value) => Ok(Some(value)),
            _ => Err(bad_request()),
        },
    }
}

/// `turn_uid` 的形状门。与 `rc-agent` 工具面的 `is_turn_uid` 同一形状
/// （64 位**小写** hex）：形状不合法是调用方的 bug，不是"查无此轮"。
fn is_turn_uid(raw: &str) -> bool {
    raw.len() == 64 && raw.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 一次成功读要落的审计两格：`subject` 是会话键（没有会话的路由为 `None`），
/// `hits` 是本次返回条数。
struct Audit<'a> {
    subject: Option<&'a str>,
    hits: usize,
}

/// 审计 + 回包：成功读（含空读数）一行 `transcript_read`，`hits` 是本次返回条数。
/// `agent` / `principal` 只从可选请求头自报，缺省 `"-"`（不决定可读性）。
fn respond(
    ledger: &LedgerHandle,
    now_ms: i64,
    device: &DeviceRow,
    req: &Request,
    route: &'static str,
    audit: Audit<'_>,
    body: Value,
) -> Response {
    let detail = json!({
        "route": route,
        "agent": req.header("x-recollect-agent").unwrap_or("-"),
        "principal": req.header("x-recollect-principal").unwrap_or("-"),
        "hits": audit.hits,
    });
    match append_audit(
        ledger,
        now_ms,
        &device.device_id,
        AUDIT_ACTION,
        audit.subject,
        &detail.to_string(),
    ) {
        Ok(()) => Response::json(200, body),
        Err(_) => store_unavailable(),
    }
}

/// `GET /v1/transcript/sessions?limit=`：中心全部会话（`Trust::Lan`）。
/// `limit` 可选（1..=200），缺省全量；`truncated` 如实上报（G7）。
pub(crate) fn sessions(
    t: &TranscriptHandle,
    ledger: &LedgerHandle,
    device: &DeviceRow,
    req: &Request,
    now_ms: i64,
) -> Response {
    let epoch = match epoch(t) {
        Ok(epoch) => epoch,
        Err(response) => return response,
    };
    let limit = match optional_bounded(req, "limit", 1, MAX_SESSION_LIMIT) {
        Ok(limit) => limit,
        Err(response) => return response,
    };
    let mut list = match transcript::visible_sessions(t, Trust::Lan) {
        Ok(list) => list,
        Err(_) => return store_unavailable(),
    };
    let truncated = limit.is_some_and(|n| list.len() > n);
    if let Some(n) = limit {
        list.truncate(n);
    }
    let hits = list.len();
    let body = json!({
        "epoch": epoch,
        "source": SOURCE,
        "sessions": list,
        "truncated": truncated,
    });
    respond(
        ledger,
        now_ms,
        device,
        req,
        ROUTE_SESSIONS,
        Audit { subject: None, hits },
        body,
    )
}

/// `GET /v1/transcript/session?session_key=&after_uid=&limit=`：会话轮正文回放
/// （`Trust::Lan`）。不可见/不存在 ⇒ `turns:[]`（200，与查无同形，归属两列与
/// `device_id` 为 null）。W39 刀 E：有可见轮时信封带 `device_id / owner_agent_id /
/// owner_principal_id`（细案 §二.4）。
/// `limit` 1..=200 缺省 200；`truncated` 由最后一行之后再探一条得出。
pub(crate) fn session(
    t: &TranscriptHandle,
    ledger: &LedgerHandle,
    device: &DeviceRow,
    req: &Request,
    now_ms: i64,
) -> Response {
    let epoch = match epoch(t) {
        Ok(epoch) => epoch,
        Err(response) => return response,
    };
    let session_key = match req.query_get("session_key") {
        Some(key) if !key.is_empty() => key.to_string(),
        _ => return bad_request(),
    };
    let limit = match bounded(req, "limit", 1, MAX_SESSION_LIMIT, MAX_SESSION_LIMIT) {
        Ok(limit) => limit,
        Err(response) => return response,
    };
    let after_uid = req.query_get("after_uid").map(ToString::to_string);
    let replay =
        match transcript::session_turns(t, Trust::Lan, &session_key, limit, after_uid.as_deref()) {
            Ok(replay) => replay,
            Err(_) => return store_unavailable(),
        };
    // 空页与"不可见/查无"同形：200 + turns:[]，归属三格为 null。
    let (device_id, owner_agent_id, owner_principal_id, turns) = match replay {
        Some(r) => (r.device_id, r.owner_agent_id, r.owner_principal_id, r.turns),
        None => (None, None, None, Vec::new()),
    };
    // 截断读数：正好取满 limit 时，再看最后一行之后还有没有一行（多一条不返回）。
    let truncated = if turns.len() == limit {
        let last = turns.last().map(|row| row.turn_uid.clone()).unwrap_or_default();
        match transcript::session_turns(t, Trust::Lan, &session_key, 1, Some(&last)) {
            Ok(more) => more.is_some(),
            Err(_) => return store_unavailable(),
        }
    } else {
        false
    };
    let hits = turns.len();
    let body = json!({
        "epoch": epoch,
        "source": SOURCE,
        "session_key": session_key,
        "device_id": device_id,
        "owner_agent_id": owner_agent_id,
        "owner_principal_id": owner_principal_id,
        "turns": turns,
        "truncated": truncated,
    });
    respond(
        ledger,
        now_ms,
        device,
        req,
        ROUTE_SESSION,
        Audit { subject: Some(&session_key), hits },
        body,
    )
}

/// `GET /v1/transcript/search?q=&limit=`：中心 FTS 命中（`Trust::Lan`）。
/// `q` 必填（切不出词 ⇒ 400）；`limit` 1..=20 缺省 10。
pub(crate) fn search(
    t: &TranscriptHandle,
    ledger: &LedgerHandle,
    device: &DeviceRow,
    req: &Request,
    now_ms: i64,
) -> Response {
    let epoch = match epoch(t) {
        Ok(epoch) => epoch,
        Err(response) => return response,
    };
    let query = match req.query_get("q") {
        Some(q) if !q.trim().is_empty() => q.to_string(),
        _ => return bad_request(),
    };
    let limit = match bounded(req, "limit", 1, MAX_TURN_LIMIT, DEFAULT_TURN_LIMIT) {
        Ok(limit) => limit,
        Err(response) => return response,
    };
    let out = match transcript::search_turns(t, Trust::Lan, &query, limit) {
        Ok(out) => out,
        // 查询切不出词是调用方的输入问题（参数非法），不是读数故障。
        Err(transcript::TranscriptSearchError::NoQueryTokens) => return bad_request(),
        Err(_) => return store_unavailable(),
    };
    let hits = out.hits.len();
    let body = json!({
        "epoch": epoch,
        "source": SOURCE,
        "matched": out.matched,
        "hits": out.hits,
        "truncated": out.truncated,
        "coverage": out.coverage,
    });
    respond(
        ledger,
        now_ms,
        device,
        req,
        ROUTE_SEARCH,
        Audit { subject: None, hits },
        body,
    )
}

/// `GET /v1/transcript/turn?turn_uid=&before=&after=`：目标轮 + 同会话前后邻居
/// （`Trust::Lan`）。`turn_uid` 必须 64 位小写 hex；`before`/`after` 0..=10 缺省 0。
/// 不可见/不存在 ⇒ 空窗口（200，`session_key` 等为 null，不泄露存在性）。
pub(crate) fn turn(
    t: &TranscriptHandle,
    ledger: &LedgerHandle,
    device: &DeviceRow,
    req: &Request,
    now_ms: i64,
) -> Response {
    let epoch = match epoch(t) {
        Ok(epoch) => epoch,
        Err(response) => return response,
    };
    let turn_uid = match req.query_get("turn_uid") {
        Some(uid) if is_turn_uid(uid) => uid.to_string(),
        _ => return bad_request(),
    };
    let before = match bounded(req, "before", 0, MAX_WINDOW_SIDE, 0) {
        Ok(before) => before,
        Err(response) => return response,
    };
    let after = match bounded(req, "after", 0, MAX_WINDOW_SIDE, 0) {
        Ok(after) => after,
        Err(response) => return response,
    };
    let window = match transcript::turn_window(t, Trust::Lan, &turn_uid, before, after) {
        Ok(window) => window,
        Err(_) => return store_unavailable(),
    };
    let (body, subject, hits) = match window {
        Some(w) => {
            let hits = w.turns.len();
            let subject = w.session_key.clone();
            let body = json!({
                "epoch": epoch,
                "source": SOURCE,
                "session_key": w.session_key,
                "device_id": w.device_id,
                "owner_agent_id": w.owner_agent_id,
                "owner_principal_id": w.owner_principal_id,
                "turns": w.turns,
                "truncated_before": w.truncated_before,
                "truncated_after": w.truncated_after,
            });
            (body, Some(subject), hits)
        }
        // 轮不存在与被闸挡同形：空窗口，不泄露存在性。
        None => (
            json!({
                "epoch": epoch,
                "source": SOURCE,
                "session_key": Value::Null,
                "device_id": Value::Null,
                "owner_agent_id": Value::Null,
                "owner_principal_id": Value::Null,
                "turns": [],
                "truncated_before": false,
                "truncated_after": false,
            }),
            None,
            0,
        ),
    };
    respond(
        ledger,
        now_ms,
        device,
        req,
        ROUTE_TURN,
        Audit { subject: subject.as_deref(), hits },
        body,
    )
}
