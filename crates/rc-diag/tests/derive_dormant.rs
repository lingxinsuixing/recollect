//! S5：推导休眠告警的**判定**两向。`derive_dormant_alert` 是这条告警唯一的家（判定与 slug），
//! `doctor` 只做"从账与库里取两个数 → 渲染"。所以这一条不需要库、不需要子进程：
//! 与 `reconcile.rs` 里那两条 `quota_exhausted_alerts` 同一条纪律（纯函数递一次）。
//!
//! 为什么判据是"从没跑过 + 池里有活行"而不是"距上次推导超过 N 小时"：后者需要一个窗口常数，
//! 而全仓没有任何生产者给那个常数定过值；前者是结构性的休眠——推导家族一次都没成功推进过，
//! 却有活行等着它，这不需要时钟就是事实。

/// 正支：水位行的 `last_success_at_ms` 从没写过（`None`）+ 池里还有 3 行 ⇒ 恰一条。
#[test]
fn a_never_run_derive_with_pending_rows_is_dormant() {
    assert_eq!(
        rc_diag::doctor::derive_dormant_alert(None, 3),
        vec!["derive_dormant"],
        "水位从没推进 + 池里有活行 ⇒ 恰一条 derive_dormant"
    );
}

/// 反支两条，各少一格判据：
/// - 跑过（`Some`）⇒ 不告警。哪怕池里还压着行——"落后"是另一个读数，不是"休眠"；
/// - 池子空（`0`）⇒ 不告警。这条同时是判据 43 的护身符：空库上 `doctor` 的 `alerts`
///   必须是 `[]`，把 `None` 单独当告警会让每个新库一出生就响。
#[test]
fn a_derive_that_ran_or_has_nothing_pending_is_not_dormant() {
    assert!(
        rc_diag::doctor::derive_dormant_alert(Some(1), 3).is_empty(),
        "跑过就不再是休眠（落后由 derive_lag_rows 自己说）"
    );
    assert!(
        rc_diag::doctor::derive_dormant_alert(None, 0).is_empty(),
        "池子空不是休眠：空库的 alerts 必须是 []"
    );
}
