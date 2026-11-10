//! C24-N1 的三条读数：正面表态解除退场账、否决不解除、UPDATE+INSERT+解除在同一个事务里。

use rc_memory::keys::{AnchorRole, Scope, ScopeKind, Stance};
use rc_memory::writers_a::attest;
use rc_memory::writers_c::retire;
use rc_memory::writers_x::{put_card, FactContent};
use rc_memory::Provenance;
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

fn card(root: &Root, uid: &str) -> rc_memory::MemKey {
    let scope = Scope { kind: ScopeKind::Project, key: "p-attest".into() };
    put_card(root.memory(), &FactContent {
        kind: "decision".into(), title: "表态目标".into(), body: "目标。".into(),
        scope, valid_to_ms: None },
        &[Provenance { turn_uid: uid.into(), anchor_role: AnchorRole::Human }], NOW).unwrap()
}

fn open_retirement(root: &Root, key: &rc_memory::MemKey) -> i64 {
    root.memory().conn().query_row(
        "SELECT count(*) FROM retirement WHERE mem_table=?1 AND mem_id=?2 AND resolved_at_ms IS NULL",
        rusqlite::params![key.table.prefix(), key.id], |r| r.get(0)).unwrap()
}

#[test]
fn 正面表态解除退场账() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let uid = "a".repeat(64);
    let key = card(&root, &uid);
    retire(root.memory(), &key, "旧证据", NOW + 1).unwrap();
    assert_eq!(open_retirement(&root, &key), 1);
    attest(root.memory(), &key, Stance::Confirmed, Some(&uid), None, NOW + 2).unwrap();
    assert_eq!(open_retirement(&root, &key), 0, "正面表态必须解除退场账");
    let at: Option<i64> = root.memory().conn().query_row(
        "SELECT resolved_at_ms FROM retirement WHERE mem_table=?1 AND mem_id=?2",
        rusqlite::params![key.table.prefix(), key.id], |r| r.get(0)).unwrap();
    assert_eq!(at, Some(NOW + 2), "解除时刻 = 表态时刻（与 attest 的盖章同源）");
}

#[test]
fn 否决表态不解除退场账() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let key = card(&root, &"b".repeat(64));
    retire(root.memory(), &key, "旧证据", NOW + 1).unwrap();
    attest(root.memory(), &key, Stance::Rejected, None, Some("仍然不行"), NOW + 2).unwrap();
    assert_eq!(open_retirement(&root, &key), 1, "否决也是负面表态，不许解除退场");
}

#[test]
fn 表态的更新插入解除三件事在同一个事务里() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let uid = "c".repeat(64);
    let key = card(&root, &uid);
    // 先留一条在场表态（UPDATE 会把它解除）。
    attest(root.memory(), &key, Stance::Confirmed, Some(&uid), None, NOW + 1).unwrap();
    // 退场账也留一条（正面表态会解除它）。
    retire(root.memory(), &key, "旧证据", NOW + 2).unwrap();
    // INSERT 在 UPDATE 之后炸：触发器让 corroborated 这一条写不进去。
    root.memory().conn().execute_batch(
        "CREATE TRIGGER refuse_corroborated BEFORE INSERT ON attestation
         WHEN NEW.stance='corroborated' BEGIN SELECT RAISE(ABORT, 'boom'); END;").unwrap();

    let err = attest(root.memory(), &key, Stance::Corroborated, Some(&uid), None, NOW + 3);
    assert!(err.is_err(), "触发器必须让 INSERT 失败");
    let open_att: i64 = root.memory().conn().query_row(
        "SELECT count(*) FROM attestation WHERE mem_table=?1 AND mem_id=?2
           AND stance='confirmed' AND resolved_at_ms IS NULL",
        rusqlite::params![key.table.prefix(), key.id], |r| r.get(0)).unwrap();
    assert_eq!(open_att, 1, "UPDATE 必须随 INSERT 一起回滚：旧的 confirmed 仍是在场态");
    assert_eq!(open_retirement(&root, &key), 1, "解除退场账也必须随事务回滚");
    let n: i64 = root.memory().conn().query_row(
        "SELECT count(*) FROM attestation WHERE stance='corroborated'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
}
