use node::offline::{observe_failure, observe_success, OFFLINE_GATE_MS};
use node::state::{SelfDistill, SyncState};
use node::sync::SyncError;

const T0: i64 = 1_700_000_000_000;

fn fail(st: &mut SyncState, e: SyncError, now: i64, auto: bool) -> SelfDistill {
    observe_failure(st, e, now, 0, auto)
}

#[test]
fn the_gate_stays_shut_one_millisecond_before_a_day() {
    let mut st = SyncState::default();
    assert_eq!(
        fail(&mut st, SyncError::EndpointUnreachable, T0, true),
        SelfDistill::OfflineWaiting
    );
    assert_eq!(
        fail(
            &mut st,
            SyncError::EndpointUnreachable,
            T0 + OFFLINE_GATE_MS - 1,
            true
        ),
        SelfDistill::OfflineWaiting
    );
}

#[test]
fn the_gate_opens_exactly_at_a_day() {
    let mut st = SyncState::default();
    fail(&mut st, SyncError::EndpointUnreachable, T0, true);
    assert_eq!(
        fail(
            &mut st,
            SyncError::EndpointUnreachable,
            T0 + OFFLINE_GATE_MS,
            true
        ),
        SelfDistill::OfflineDistilling
    );
}

#[test]
fn a_reboot_does_not_reset_the_offline_window() {
    let mut st = SyncState::default();
    fail(&mut st, SyncError::EndpointUnreachable, T0, true);
    let text = serde_json::to_string(&st).unwrap();
    let mut back: SyncState = serde_json::from_str(&text).unwrap();
    assert_eq!(
        fail(
            &mut back,
            SyncError::EndpointUnreachable,
            T0 + OFFLINE_GATE_MS,
            true
        ),
        SelfDistill::OfflineDistilling
    );
}

#[test]
fn a_wall_clock_rollback_does_not_shorten_the_window() {
    let mut st = SyncState::default();
    fail(&mut st, SyncError::EndpointUnreachable, T0, true);
    fail(
        &mut st,
        SyncError::EndpointUnreachable,
        T0 + OFFLINE_GATE_MS,
        true,
    );
    assert_eq!(
        fail(&mut st, SyncError::EndpointUnreachable, T0 - 1_000, true),
        SelfDistill::OfflineDistilling
    );
}

#[test]
fn a_monotonic_elapsed_opens_the_gate_even_when_the_wall_clock_did_not() {
    let mut st = SyncState::default();
    observe_failure(&mut st, SyncError::EndpointUnreachable, T0, 0, true);
    assert_eq!(
        observe_failure(
            &mut st,
            SyncError::EndpointUnreachable,
            T0,
            OFFLINE_GATE_MS,
            true
        ),
        SelfDistill::OfflineDistilling
    );
}

#[test]
fn an_auth_rejection_is_not_offline() {
    let mut st = SyncState::default();
    assert_eq!(
        fail(&mut st, SyncError::AuthRejected, T0, true),
        SelfDistill::Normal
    );
    assert_eq!(st.offline_since_ms, 0);
    assert_eq!(st.center_seen_at_ms, T0, "中心答了 401，它活着");
}

#[test]
fn a_bad_response_is_not_offline() {
    let mut st = SyncState::default();
    assert_eq!(
        fail(&mut st, SyncError::BadResponse, T0, true),
        SelfDistill::Normal
    );
    assert_eq!(st.offline_since_ms, 0);
}

#[test]
fn a_local_error_keeps_the_existing_window() {
    let mut st = SyncState::default();
    fail(&mut st, SyncError::EndpointUnreachable, T0, true);
    assert_eq!(
        fail(&mut st, SyncError::NotPaired, T0 + 1_000, true),
        SelfDistill::OfflineWaiting
    );
    assert_eq!(st.offline_since_ms, T0, "本地缺 token 不证明中心死了");
}

#[test]
fn a_successful_sync_clears_the_offline_window() {
    let mut st = SyncState::default();
    fail(&mut st, SyncError::EndpointUnreachable, T0, true);
    fail(
        &mut st,
        SyncError::EndpointUnreachable,
        T0 + OFFLINE_GATE_MS,
        true,
    );
    observe_success(&mut st, T0 + OFFLINE_GATE_MS + 1);
    assert_eq!(st.self_distill, SelfDistill::Normal);
    assert_eq!(st.offline_since_ms, 0);
    assert_eq!(st.offline_elapsed_ms, 0);
    assert_eq!(st.center_seen_at_ms, T0 + OFFLINE_GATE_MS + 1);
}

#[test]
fn auto_unlock_false_keeps_the_gate_shut() {
    let mut st = SyncState::default();
    fail(&mut st, SyncError::EndpointUnreachable, T0, false);
    assert_eq!(
        fail(
            &mut st,
            SyncError::EndpointUnreachable,
            T0 + OFFLINE_GATE_MS,
            false
        ),
        SelfDistill::OfflineWaiting
    );
    assert!(st.offline_elapsed_ms >= OFFLINE_GATE_MS, "读数仍要说满了一天");
}

#[test]
fn the_offline_start_does_not_move_on_each_failure() {
    let mut st = SyncState::default();
    fail(&mut st, SyncError::EndpointUnreachable, T0, true);
    fail(&mut st, SyncError::EndpointUnreachable, T0 + 1_000, true);
    assert_eq!(st.offline_since_ms, T0);
}

/// R8：证书不对 / 身份不符 / 行被拒都证明**对端在说话**。
/// 已经过了 1 天闸的节点遇到它们时必须停下自蒸：窗口清零、状态回落到等待态。
/// 不结束窗口会让旧节点一直自蒸、自升，而中心其实活着。
#[test]
fn an_identity_failure_ends_the_offline_window() {
    for e in [SyncError::TlsFailed, SyncError::IdentityMismatch, SyncError::RowsRejected] {
        let mut st = SyncState::default();
        fail(&mut st, SyncError::EndpointUnreachable, T0, true);
        assert_eq!(
            fail(
                &mut st,
                SyncError::EndpointUnreachable,
                T0 + OFFLINE_GATE_MS,
                true
            ),
            SelfDistill::OfflineDistilling,
            "{e:?} 之前必须真的开过闸"
        );
        assert_eq!(fail(&mut st, e, T0 + OFFLINE_GATE_MS + 1, true), SelfDistill::OfflineWaiting);
        assert_eq!(st.offline_elapsed_ms, 0, "{e:?} 之后离线时长必须归零");
        assert_eq!(st.offline_since_ms, 0, "{e:?} 之后窗口起点必须清掉");
        assert_eq!(st.center_seen_at_ms, T0 + OFFLINE_GATE_MS + 1, "对端答了 ⇒ 见过中心");
        // 读数印出来，报告里的"闸前后"有原始行可引。
        println!(
            "R8 {e:?}: before=offline_distilling after={} offline_elapsed_ms={}",
            st.self_distill.as_str(),
            st.offline_elapsed_ms
        );
    }
}

/// 阳性对照：`endpoint_unreachable` 仍照旧累积（否则上一条可能只是"闸根本不开"）。
#[test]
fn endpoint_unreachable_still_accumulates_after_an_identity_failure() {
    let mut st = SyncState::default();
    fail(&mut st, SyncError::EndpointUnreachable, T0, true);
    fail(&mut st, SyncError::EndpointUnreachable, T0 + OFFLINE_GATE_MS, true);
    // 身份错误结束窗口；下一轮不可达要从这个新起点重新累积。
    fail(&mut st, SyncError::TlsFailed, T0 + OFFLINE_GATE_MS + 1, true);
    assert_eq!(
        fail(&mut st, SyncError::EndpointUnreachable, T0 + OFFLINE_GATE_MS + 2, true),
        SelfDistill::OfflineWaiting
    );
    assert_eq!(st.offline_since_ms, T0 + OFFLINE_GATE_MS + 2);
    assert_eq!(
        fail(
            &mut st,
            SyncError::EndpointUnreachable,
            T0 + 2 * OFFLINE_GATE_MS + 2,
            true
        ),
        SelfDistill::OfflineDistilling,
        "新窗口满一天照样开闸"
    );
}
