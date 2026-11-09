//! 离线闸：连续不可达满一天才允许节点自蒸；满五天则**自升**（`detached_distilling`，
//! 自己当中心并开始自备份）。
//!
//! **只有传输不可达算离线**。中心答了但拒绝我们（401/403/409、坏 JSON、整批 422、分片 sha 不符）
//! 说明中心活着，下一步是重新配对或告警，不是自蒸；否则一个被吊销的节点会自己去烧额度。
//! 本地缺配置/缺 token 同样不算——那不是「中心不可达」。

use crate::state::{SelfDistill, SyncState};
use crate::sync::SyncError;

/// 一天。判据是 `offline_elapsed_ms >= OFFLINE_GATE_MS`。
pub const OFFLINE_GATE_MS: i64 = 86_400_000;

/// 五天：连续离线满五天 ⇒ 节点**自己当中心**（`DetachedDistilling`），并开始自备份。
/// 与一天的闸是同一个累计量，只是门槛更高。
pub const DETACH_GATE_MS: i64 = 5 * OFFLINE_GATE_MS;

/// 传输不可达：唯一会累积离线时长的错误。
pub fn counts_as_offline(e: SyncError) -> bool {
    matches!(e, SyncError::EndpointUnreachable)
}

/// 中心确实答了（哪怕是拒绝）：这说明它活着，离线窗口应当结束。
///
/// 后三类码必须在这一档：
/// - `tls_failed` / `identity_mismatch`：**证书不对是身份问题，不是断网**。对端在说话，
///   只是名字/链路对不上；不结束窗口会让旧节点一直自蒸、自升、自备份，而中心其实活着；
/// - `rows_rejected`：中心的 4xx/5xx 应答，同样证明它活着。
///
/// 结束窗口**不改变**"本轮失败"：错误码仍由调用方照常记进 `last_error`。
pub fn center_answered(e: SyncError) -> bool {
    matches!(
        e,
        SyncError::AuthRejected
            | SyncError::HubKeyMismatch
            | SyncError::IngestRejected
            | SyncError::BadResponse
            | SyncError::DownloadFailed
            | SyncError::ChunkMismatch
            | SyncError::TlsFailed
            | SyncError::IdentityMismatch
            | SyncError::RowsRejected
    )
}

/// "对端答了，但不是成功"：窗口结束，状态回落到 [`SelfDistill::OfflineWaiting`]，
/// **不是** `Normal`——这一轮确实失败了，写成 normal 会让读数与 `last_error` 互相矛盾。
fn answered_but_refused(e: SyncError) -> bool {
    matches!(
        e,
        SyncError::TlsFailed | SyncError::IdentityMismatch | SyncError::RowsRejected
    )
}

/// 闸开到哪里。**五天优先于一天**：同一个累计量，门槛高的那一档先判。
fn gate_state(state: &SyncState, auto_unlock: bool) -> SelfDistill {
    if !auto_unlock {
        return SelfDistill::OfflineWaiting;
    }
    if state.offline_elapsed_ms >= DETACH_GATE_MS {
        SelfDistill::DetachedDistilling
    } else if state.offline_elapsed_ms >= OFFLINE_GATE_MS {
        SelfDistill::OfflineDistilling
    } else {
        SelfDistill::OfflineWaiting
    }
}

/// 记一次失败。`monotonic_elapsed_ms` 是进程内单调累计（无单调钟时传 0）；
/// 时长取 `max(已存, 单调, 墙上差)`——重启或墙钟回拨都不许把窗口缩短。
///
/// 返回更新后的状态，方便调用方直接读。
pub fn observe_failure(
    state: &mut SyncState,
    error: SyncError,
    now_ms: i64,
    monotonic_elapsed_ms: i64,
    auto_unlock: bool,
) -> SelfDistill {
    if center_answered(error) {
        // 中心答了：窗口结束。错误码由调用方照常记在 `last_error`。
        let refused = answered_but_refused(error);
        observe_success(state, now_ms);
        if refused {
            // 窗口已清零，但这一轮是失败（身份不对/行被拒）：不能写成 `normal`。
            state.self_distill = SelfDistill::OfflineWaiting;
        }
        return state.self_distill;
    }
    if !counts_as_offline(error) {
        // 本地错误（缺端点/缺 token/坏 token 文件/库不可用）不证明中心活着，也不证明它死了；
        // 保留已有窗口，不改状态。
        return state.self_distill;
    }
    if state.offline_since_ms == 0 {
        state.offline_since_ms = now_ms;
    }
    let wall = now_ms.saturating_sub(state.offline_since_ms).max(0);
    state.offline_elapsed_ms = state
        .offline_elapsed_ms
        .max(monotonic_elapsed_ms.max(0))
        .max(wall);
    state.self_distill = gate_state(state, auto_unlock);
    state.self_distill
}

/// 一次成功同步：窗口清零，中心见过的时刻更新。
pub fn observe_success(state: &mut SyncState, now_ms: i64) {
    state.center_seen_at_ms = now_ms;
    state.offline_since_ms = 0;
    state.offline_elapsed_ms = 0;
    state.self_distill = SelfDistill::Normal;
}
