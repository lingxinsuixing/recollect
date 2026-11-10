//! S4b1：邀请票的账本面（issue/revoke/consume）。
//!
//! 每条用例的名字就是它盯的那一格变异：去掉事务、去掉 `used_at_ms IS NULL` 条件、
//! 把"先吃票后落设备"反过来——任一条都会让其中一条红。

use rc_ledger::device;
use rc_ledger::invite::{self, InviteOutcome};
use rc_store::Root;

fn root() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    (dir, root)
}

fn consume(root: &mut Root, id: &str, device_id: &str, token_hash: &str, now_ms: i64) -> InviteOutcome {
    invite::consume(root.ledger_mut(), id, device_id, "cHVia2V5", token_hash, now_ms).unwrap()
}

fn count(root: &Root, table: &str) -> i64 {
    root.ledger()
        .conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn consume_pairs_the_device_and_marks_the_ticket_used_atomically() {
    let (_dir, mut root) = root();
    invite::issue(root.ledger_mut(), "inv-1", 900, 10_000).unwrap();
    assert_eq!(consume(&mut root, "inv-1", "dev-1", "th-1", 1_000), InviteOutcome::Paired);

    let d = device::find_by_token_hash(root.ledger(), "th-1").unwrap().unwrap();
    assert_eq!(d.device_id, "dev-1");
    assert_eq!(d.pubkey, "cHVia2V5");

    let (used_at, used_by): (Option<i64>, Option<String>) = root
        .ledger()
        .conn()
        .query_row("SELECT used_at_ms, used_by_device FROM invite WHERE id='inv-1'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(used_at, Some(1_000));
    assert_eq!(used_by.as_deref(), Some("dev-1"));

    // 审计与设备同生共死：配对过就一定有那一条。
    let audits: i64 = root
        .ledger()
        .conn()
        .query_row("SELECT count(*) FROM audit_log WHERE action='pair' AND subject='dev-1'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(audits, 1);
}

#[test]
fn a_second_device_reusing_the_same_ticket_is_refused_and_creates_nothing() {
    let (_dir, mut root) = root();
    invite::issue(root.ledger_mut(), "inv-1", 900, 10_000).unwrap();
    assert_eq!(consume(&mut root, "inv-1", "dev-1", "th-1", 1_000), InviteOutcome::Paired);
    assert_eq!(
        consume(&mut root, "inv-1", "dev-2", "th-2", 1_001),
        InviteOutcome::Used,
        "同一张票第二次必须拒"
    );
    assert!(device::find_by_token_hash(root.ledger(), "th-2").unwrap().is_none());
    assert_eq!(count(&root, "device"), 1, "被拒的那一发不许留下设备行");
}

#[test]
fn expired_ticket_is_refused_and_stays_unused() {
    let (_dir, mut root) = root();
    invite::issue(root.ledger_mut(), "inv-1", 900, 1_000).unwrap();
    assert_eq!(consume(&mut root, "inv-1", "dev-1", "th-1", 1_001), InviteOutcome::Expired);
    assert!(device::find_by_id(root.ledger(), "dev-1").unwrap().is_none());
    let used: Option<i64> = root
        .ledger()
        .conn()
        .query_row("SELECT used_at_ms FROM invite WHERE id='inv-1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(used, None, "过期票被拒时不许顺手把它标成已用");
}

#[test]
fn ticket_boundary_is_inclusive_at_the_expiry_millisecond() {
    let (_dir, mut root) = root();
    invite::issue(root.ledger_mut(), "inv-1", 900, 1_000).unwrap();
    assert_eq!(
        consume(&mut root, "inv-1", "dev-1", "th-1", 1_000),
        InviteOutcome::Paired,
        "恰好到期的那一毫秒仍可用（与 pair_code 的判据一致）"
    );
}

#[test]
fn unknown_ticket_is_refused() {
    let (_dir, mut root) = root();
    assert_eq!(consume(&mut root, "inv-nope", "dev-1", "th-1", 1_000), InviteOutcome::Unknown);
    assert_eq!(count(&root, "device"), 0);
}

#[test]
fn revoked_ticket_is_refused_and_revoke_is_idempotent() {
    let (_dir, mut root) = root();
    invite::issue(root.ledger_mut(), "inv-1", 900, 10_000).unwrap();
    assert!(invite::revoke(root.ledger_mut(), "inv-1", 950).unwrap());
    assert_eq!(consume(&mut root, "inv-1", "dev-1", "th-1", 1_000), InviteOutcome::Revoked);
    assert!(device::find_by_id(root.ledger(), "dev-1").unwrap().is_none());

    // 再作废一次：仍是 true（这条存在），且第一次的时刻不被覆盖。
    assert!(invite::revoke(root.ledger_mut(), "inv-1", 2_000).unwrap());
    let revoked_at: Option<i64> = root
        .ledger()
        .conn()
        .query_row("SELECT revoked_at_ms FROM invite WHERE id='inv-1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(revoked_at, Some(950), "第二次作废覆盖了第一次的时刻");
}

#[test]
fn revoking_an_unknown_ticket_reports_false() {
    let (_dir, mut root) = root();
    assert!(!invite::revoke(root.ledger_mut(), "inv-nope", 1_000).unwrap());
}

/// `DeviceExists` 是"票不许被烧掉"的那一格：事务回滚之后，同一张票
/// 仍然可以被另一台设备使用——否则一台占着 device_id 的活设备能靠重复提交把票耗光。
#[test]
fn device_exists_rolls_back_the_consumption_and_the_ticket_stays_usable() {
    let (_dir, mut root) = root();
    invite::issue(root.ledger_mut(), "inv-1", 900, 10_000).unwrap();
    assert_eq!(consume(&mut root, "inv-1", "dev-1", "th-1", 1_000), InviteOutcome::Paired);

    invite::issue(root.ledger_mut(), "inv-2", 1_100, 10_000).unwrap();
    assert_eq!(
        consume(&mut root, "inv-2", "dev-1", "th-2", 1_200),
        InviteOutcome::DeviceExists,
        "活着的设备不许被第二次配对"
    );
    let used: Option<i64> = root
        .ledger()
        .conn()
        .query_row("SELECT used_at_ms FROM invite WHERE id='inv-2'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(used, None, "DeviceExists 时票被消费了（事务没回滚）");

    // 换一台设备，同一张票照常可用。
    assert_eq!(consume(&mut root, "inv-2", "dev-2", "th-3", 1_300), InviteOutcome::Paired);
    assert_eq!(count(&root, "device"), 2);
}

/// token 唯一约束在**落设备**那一步炸：票的 UPDATE 必须跟着回滚。
/// 这一条盯着"把 `unchecked_transaction` 换成直接连"这个变异。
#[test]
fn token_hash_collision_rolls_back_the_ticket_consumption() {
    let (_dir, mut root) = root();
    invite::issue(root.ledger_mut(), "inv-1", 900, 10_000).unwrap();
    assert_eq!(consume(&mut root, "inv-1", "dev-1", "th-shared", 1_000), InviteOutcome::Paired);

    invite::issue(root.ledger_mut(), "inv-2", 1_100, 10_000).unwrap();
    let err = invite::consume(
        root.ledger_mut(),
        "inv-2",
        "dev-2",
        "cHVia2V5",
        "th-shared",
        1_200,
    );
    assert!(err.is_err(), "撞了 token 唯一约束却不是错误");
    let used: Option<i64> = root
        .ledger()
        .conn()
        .query_row("SELECT used_at_ms FROM invite WHERE id='inv-2'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(used, None, "唯一约束报错之后票却已经被标成已用（事务没回滚）");
    assert!(device::find_by_id(root.ledger(), "dev-2").unwrap().is_none());
}

/// 撤销过的设备允许用新票复活（与覆盖配对码的语义一致）。
#[test]
fn a_revoked_device_can_come_back_with_a_new_ticket() {
    let (_dir, mut root) = root();
    invite::issue(root.ledger_mut(), "inv-1", 900, 10_000).unwrap();
    assert_eq!(consume(&mut root, "inv-1", "dev-1", "th-1", 1_000), InviteOutcome::Paired);
    assert_eq!(
        device::revoke(root.ledger_mut(), "dev-1", 1_100).unwrap(),
        device::RevokeOutcome::Revoked
    );

    invite::issue(root.ledger_mut(), "inv-2", 1_200, 10_000).unwrap();
    assert_eq!(consume(&mut root, "inv-2", "dev-1", "th-2", 1_300), InviteOutcome::Paired);
    let d = device::find_by_token_hash(root.ledger(), "th-2").unwrap().unwrap();
    assert_eq!(d.device_id, "dev-1");
    assert_eq!(d.revoked_at_ms, None, "复活之后仍是撤销态");
}

/// `used` 优先于 `expired`：一张用过的票再拿出来，报的是"已用"而不是"过期"。
#[test]
fn used_wins_over_expired_for_a_replay() {
    let (_dir, mut root) = root();
    invite::issue(root.ledger_mut(), "inv-1", 900, 1_000).unwrap();
    assert_eq!(consume(&mut root, "inv-1", "dev-1", "th-1", 1_000), InviteOutcome::Paired);
    assert_eq!(consume(&mut root, "inv-1", "dev-2", "th-2", 5_000), InviteOutcome::Used);
}

/// 出票不给同一 id 开两行：撞主键是错误，不是"覆盖成新的有效期"。
#[test]
fn issuing_the_same_id_twice_is_an_error_and_the_first_row_survives() {
    let (_dir, mut root) = root();
    invite::issue(root.ledger_mut(), "inv-1", 900, 10_000).unwrap();
    assert!(invite::issue(root.ledger_mut(), "inv-1", 950, 20_000).is_err());
    let expires: i64 = root
        .ledger()
        .conn()
        .query_row("SELECT expires_at_ms FROM invite WHERE id='inv-1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(expires, 10_000);
}

/// S4fix-A R11：发行与撤销各留一条审计行，`subject` 是票 id、`detail` 留空。
/// 票文本（连它的签名）绝不进审计——这里用"整行里不出现任何 `rc1.` 票前缀"把这条钉住。
#[test]
fn issuing_and_revoking_leave_audit_rows_without_the_ticket() {
    let (_dir, mut root) = root();
    invite::issue(root.ledger_mut(), "inv-1", 900, 10_000).unwrap();

    let rows: Vec<String> = root
        .ledger()
        .conn()
        .prepare("SELECT actor, action, subject, detail FROM audit_log ORDER BY id")
        .unwrap()
        .query_map([], |r| {
            Ok(format!(
                "{}|{}|{}|{}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                r.get::<_, Option<String>>(3)?.unwrap_or_default()
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows, vec!["operator|invite_issued|inv-1|"], "发行的审计行不对：{rows:?}");
    assert!(rows.iter().all(|r| !r.contains("rc1.")), "票文本进了审计：{rows:?}");

    assert!(invite::revoke(root.ledger_mut(), "inv-1", 950).unwrap());
    // 幂等：第二次作废不再写第二行（否则"撤销了几次"会被读成"撤了几张票"）。
    assert!(invite::revoke(root.ledger_mut(), "inv-1", 2_000).unwrap());
    let revoked: i64 = root
        .ledger()
        .conn()
        .query_row(
            "SELECT count(*) FROM audit_log WHERE action='invite_revoked' AND subject='inv-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(revoked, 1, "重复作废写了不止一条审计");
    let at: i64 = root
        .ledger()
        .conn()
        .query_row("SELECT at_ms FROM audit_log WHERE action='invite_revoked'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(at, 950, "审计记的是第二次作废的时刻");

    // 撤销一张不存在的票：不写审计。
    assert!(!invite::revoke(root.ledger_mut(), "inv-nope", 3_000).unwrap());
    let total: i64 = root
        .ledger()
        .conn()
        .query_row("SELECT count(*) FROM audit_log", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 2, "不存在的票也留了审计行：{total}");
}

/// S4fix-A R13：钟在 unix epoch 之前 ⇒ `now_ms` 被折成 0 ⇒ 一律按过期拒。
/// 判据是 `now_ms <= 0 || expires_at_ms < now_ms`——坏钟不许让旧票复活。
#[test]
fn a_clock_at_or_before_the_epoch_makes_every_ticket_expired() {
    let (_dir, mut root) = root();
    // 票的有效期远在未来：只看 `expires_at_ms < now_ms` 会判它"还能用"。
    invite::issue(root.ledger_mut(), "inv-1", 0, 60_000).unwrap();
    assert_eq!(
        consume(&mut root, "inv-1", "dev-1", "th-1", 0),
        InviteOutcome::Expired,
        "now_ms = 0 时旧票被当成可用"
    );
    assert!(device::find_by_id(root.ledger(), "dev-1").unwrap().is_none());
    // 负数同样是坏钟。
    assert_eq!(consume(&mut root, "inv-1", "dev-1", "th-1", -1), InviteOutcome::Expired);
    // 票没被烧：钟恢复正常之后仍可用。
    assert_eq!(consume(&mut root, "inv-1", "dev-1", "th-1", 1_000), InviteOutcome::Paired);
}
