//! 配对 / 设备 / 审计的唯一写入口。
//!
//! **与计划字面块的唯一差异**：照抄的 `code_hash()` 是一个固定串，而 `pair_code.code_hash`
//! 是主键——同一枚码第二次 `issue_pair_code` 必然撞主键（实测 `UNIQUE constraint failed:
//! pair_code.code_hash`），"重新签一次码"的场景（撤销后重配、唯一约束那一例）根本跑不到断言。
//! 这里改成每次签一枚**新**码（走生产实现的 `new_pair_code`，不在测试里造第二份码面），
//! 并把 hash 显式传给 `pair`；每条用例的名字与断言保持计划原文。
use rc_ledger::device::{self, PairOutcome, RevokeOutcome};
use rc_store::{LedgerHandle, Root};

/// 签一枚新码并落库，返回它的 code_hash（展示形不落这里，它不该被任何断言持有）。
fn issue(l: &mut LedgerHandle, now_ms: i64, expires_at_ms: i64) -> String {
    let hash = rc_net::pair::new_pair_code().unwrap().1;
    device::issue_pair_code(l, &hash, now_ms, expires_at_ms).unwrap();
    hash
}

fn pair(l: &mut LedgerHandle, code_hash: &str, id: &str, token_hash: &str, now_ms: i64) -> PairOutcome {
    device::pair(l, code_hash, id, "cHVia2V5", token_hash, now_ms).unwrap()
}

#[test]
fn pair_consumes_code_writes_device_and_audit_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let h = issue(root.ledger_mut(), 900, 10_000);
    assert_eq!(pair(root.ledger_mut(), &h, "dev-1", "th-1", 1_000), PairOutcome::Paired);
    assert_eq!(pair(root.ledger_mut(), &h, "dev-2", "th-2", 1_000), PairOutcome::CodeUsed,
               "同一个码第二次必须拒");
    let d = device::find_by_token_hash(root.ledger(), "th-1").unwrap().unwrap();
    assert_eq!(d.device_id, "dev-1");
    let audits: i64 = root.ledger().conn()
        .query_row("SELECT count(*) FROM audit_log WHERE action='pair' AND subject='dev-1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(audits, 1);
    let devices: i64 = root.ledger().conn()
        .query_row("SELECT count(*) FROM device", [], |r| r.get(0)).unwrap();
    assert_eq!(devices, 1, "第二次配对必须整体回滚，不许留下 dev-2");
}

#[test]
fn expired_code_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let h = issue(root.ledger_mut(), 900, 1_000);
    let out = device::pair(root.ledger_mut(), &h, "dev-1", "pk", "th", 1_001).unwrap();
    assert_eq!(out, PairOutcome::CodeExpired);
    assert!(device::find_by_id(root.ledger(), "dev-1").unwrap().is_none());
}

#[test]
fn unknown_code_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let h = rc_net::pair::hash_pair_code("ZZZZZ-ZZZZZ").unwrap();
    let out = device::pair(root.ledger_mut(), &h, "dev-1", "pk", "th", 1).unwrap();
    assert_eq!(out, PairOutcome::CodeUnknown);
}

#[test]
fn active_device_cannot_be_repaired_but_revoked_one_can() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let h1 = issue(root.ledger_mut(), 900, 10_000);
    assert_eq!(pair(root.ledger_mut(), &h1, "dev-1", "th-1", 1_000), PairOutcome::Paired);

    let h2 = issue(root.ledger_mut(), 1_100, 10_000);
    assert_eq!(pair(root.ledger_mut(), &h2, "dev-1", "th-2", 1_000), PairOutcome::DeviceExists);

    assert_eq!(device::revoke(root.ledger_mut(), "dev-1", 1_200).unwrap(), RevokeOutcome::Revoked);
    let h3 = issue(root.ledger_mut(), 1_300, 10_000);
    assert_eq!(pair(root.ledger_mut(), &h3, "dev-1", "th-3", 1_000), PairOutcome::Paired,
               "撤销后的设备允许重新配对");
    assert_eq!(device::find_by_token_hash(root.ledger(), "th-3").unwrap().unwrap().device_id, "dev-1");
}

#[test]
fn token_hash_unique_violation_rolls_back_the_code_consumption() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let h1 = issue(root.ledger_mut(), 900, 10_000);
    assert_eq!(pair(root.ledger_mut(), &h1, "dev-1", "same", 1_000), PairOutcome::Paired);

    let h2 = issue(root.ledger_mut(), 1_100, 10_000);
    let err = device::pair(root.ledger_mut(), &h2, "dev-2", "pk", "same", 1_200);
    assert!(err.is_err(), "token_hash 唯一约束必须响亮失败");
    let used: Option<i64> = root.ledger().conn()
        .query_row("SELECT used_at_ms FROM pair_code WHERE code_hash=?1", [&h2], |r| r.get(0))
        .unwrap();
    assert!(used.is_none(), "失败的配对不许把码标记为已用");
    assert!(device::find_by_id(root.ledger(), "dev-2").unwrap().is_none());
}

#[test]
fn revoke_is_idempotent_and_audited() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let h = issue(root.ledger_mut(), 900, 10_000);
    assert_eq!(pair(root.ledger_mut(), &h, "dev-1", "th-1", 1_000), PairOutcome::Paired);
    assert_eq!(device::revoke(root.ledger_mut(), "dev-1", 1_100).unwrap(), RevokeOutcome::Revoked);
    assert_eq!(device::revoke(root.ledger_mut(), "dev-1", 1_200).unwrap(), RevokeOutcome::AlreadyRevoked);
    assert_eq!(device::revoke(root.ledger_mut(), "ghost", 1_300).unwrap(), RevokeOutcome::Unknown);
    let n: i64 = root.ledger().conn()
        .query_row("SELECT count(*) FROM audit_log WHERE action='revoke'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "重复撤销不许再写一条审计");
}

#[test]
fn audit_actions_are_closed_and_details_are_empty() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let h = issue(root.ledger_mut(), 900, 10_000);
    let _ = pair(root.ledger_mut(), &h, "dev-1", "th-1", 1_000);
    let _ = device::revoke(root.ledger_mut(), "dev-1", 1_100);
    let mut st = root.ledger().conn().prepare("SELECT action, detail FROM audit_log ORDER BY id").unwrap();
    let rows: Vec<(String, Option<String>)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap().collect::<Result<_, _>>().unwrap();
    assert!(!rows.is_empty());
    for (action, detail) in rows {
        assert!(device::AUDIT_ACTIONS.contains(&action.as_str()), "未登记动作：{action}");
        assert!(detail.unwrap_or_default().is_empty(), "detail 只许空串");
    }
}
