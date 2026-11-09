use rc_net::pair::NonceStore;

#[test]
fn nonce_is_single_use() {
    let mut s = NonceStore::new(120_000);
    let (n, exp) = s.issue("dev-1", 1_000).unwrap();
    assert_eq!(exp, 121_000);
    assert!(s.consume("dev-1", &n, 1_001), "第一次必须通过");
    assert!(!s.consume("dev-1", &n, 1_002), "第二次必须失败（重放）");
}

#[test]
fn nonce_expires_and_is_not_usable_by_another_device() {
    let mut s = NonceStore::new(120_000);
    let (n, _) = s.issue("dev-1", 1_000).unwrap();
    assert!(!s.consume("dev-2", &n, 1_001), "别人的 nonce 不许用");
    // 到期边界与 `pair_code.expires_at_ms < now_ms` 同一套约定：`expires_at_ms` 那一刻仍有效，
    // 过期是**严格越过**它（计划字面块的 121_000 落在边界上，与 `pair` 的判法自相矛盾）。
    let (n2, _) = s.issue("dev-1", 1_000).unwrap();
    assert!(!s.consume("dev-1", &n2, 121_001), "过期必须失败");
}

#[test]
fn nonce_store_is_bounded_and_gc_reclaims_expired_entries() {
    let mut s = NonceStore::with_capacity(120_000, 2);
    let _ = s.issue("dev-1", 1_000).unwrap();
    let _ = s.issue("dev-1", 1_001).unwrap();
    assert!(s.issue("dev-1", 1_002).is_err(), "容量满必须响亮拒绝");
    let _ = s.issue("dev-1", 200_000).unwrap();
    assert_eq!(s.len(), 1, "过期项应被 gc 回收");
}
