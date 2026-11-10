//! attention 的两条判据各测正反两面，外加幂等、机械解除、只读副本、空库。
//! 牙的口径：每条正向用例都写成"断言恰好一条 + 逐条对上 `(kind, 表, id)`"——`sweep` 是写表那一支，
//! 返回 `SweepReport{raised, resolved}`，所以移除任一条款的判据必然让某一条用例从 1 变 0。
//! 反过来，只写"不为空"的用例挡不住"多升一条"，只写"len==1"的挡不住指认错行，两半都要在。

use rc_memory::anomaly::{find_contradictions, open_count, sweep, SweepReport};
use rc_memory::keys::{AnchorRole, MemTable, Scope, ScopeKind, Stance, Verdict};
use rc_memory::writers_a::attest;
use rc_memory::writers_c::{resolve_retirement, retire, set_verdict};
use rc_memory::writers_x::{put_card, FactContent};
use rc_memory::{MemError, Provenance};
use rc_store::{AgentRoot, Root};

const NOW: i64 = 1_700_000_000_000;

fn content(title: &str) -> FactContent {
    FactContent {
        kind: "note".into(),
        title: title.into(),
        body: "一条合成正文，不含任何生产文本".into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-x".into() },
        valid_to_ms: None,
    }
}
fn human_prov(uid: &str) -> Vec<Provenance> {
    vec![Provenance { turn_uid: uid.into(), anchor_role: AnchorRole::Human }]
}

fn fresh() -> (tempfile::TempDir, Root) {
    let d = tempfile::tempdir().unwrap();
    let r = Root::open(d.path()).unwrap();
    (d, r)
}

/// C24-N1 之后的生产形状：退场账被更晚的正面表态当场解除，条款①的"未解决退场账"不再成立。
/// 牙：删掉 `attest` 里那句 `resolve_retirement`（或只在否决时调），本条的解除时刻与空集断言会红。
#[test]
fn 正面表态解除退场账_不再算矛盾() {
    let (_d, root) = fresh();
    let m = root.memory();
    let k = put_card(m, &content("死后确认"), &human_prov("t-h1"), NOW).unwrap();
    retire(m, &k, "manual", NOW).unwrap();
    attest(m, &k, Stance::Confirmed, Some("t-h1"), None, NOW + 10).unwrap();

    let at: Option<i64> = m.conn().query_row(
        "SELECT resolved_at_ms FROM retirement WHERE mem_table='card' AND mem_id=?1",
        [&k.id], |r| r.get(0)).unwrap();
    assert_eq!(at, Some(NOW + 10), "前置：正面表态确实在同一事务里解除了退场账");
    assert!(find_contradictions(m).unwrap().is_empty(),
        "退场账已解除 ⇒ 条款①的『未解决退场账』不成立");
    assert_eq!(sweep(m, NOW + 20).unwrap(), SweepReport { raised: 0, resolved: 0 });
}

/// 条款①今天只剩**旧库/库外写入**的形状：C24-N1 之后产品写路径上"未解除退场账 +
/// 更晚的正面表态"造不出来（`attest` 在同一事务里解除）。直写两张账是取证形状
/// （与 `未知表名的告警_既不解也不崩` 同法），旧库升级上来就长这样，判据必须还认得它。
/// 牙：把 `confirmed_after_retirement` 的 `a.created_at_ms > r.created_at_ms` 反过来或删掉，
/// 本条的"恰一条"会红。
#[test]
fn 旧库里的退场之后又被确认_仍是一条矛盾() {
    let (_d, root) = fresh();
    let m = root.memory();
    let k = put_card(m, &content("旧库死后确认"), &human_prov("t-h1-legacy"), NOW).unwrap();
    m.conn().execute(
        "INSERT INTO retirement(mem_table, mem_id, reason, created_at_ms, resolved_at_ms)
         VALUES('card', ?1, 'legacy', ?2, NULL)",
        rusqlite::params![k.id, NOW + 1]).unwrap();
    m.conn().execute(
        "INSERT INTO attestation(mem_table, mem_id, stance, actor, anchor_turn_uid,
                                 reason, created_at_ms)
         VALUES('card', ?1, 'confirmed', 'agent', ?2, NULL, ?3)",
        rusqlite::params![k.id, "t-h1-legacy", NOW + 10]).unwrap();

    let at: Option<i64> = m.conn().query_row(
        "SELECT resolved_at_ms FROM retirement WHERE mem_table='card' AND mem_id=?1",
        [&k.id], |r| r.get(0)).unwrap();
    assert_eq!(at, None, "前置：旧库形状的退场账确实未解除");

    let hits = find_contradictions(m).unwrap();
    assert_eq!(hits, vec![(MemTable::Card, k.id.clone())], "恰一条，指认那张卡");
    assert_eq!(sweep(m, NOW + 20).unwrap(), SweepReport { raised: 1, resolved: 0 });
}

/// 条款①反向半边（时间序）：先确认、后退场 ⇒ 不是矛盾，是一次正常的"先证后撤"。
/// 牙：撤掉判据里 `a.created_at_ms > r.created_at_ms` 那半句（把它变成无条件），本条从空变一条 ⇒ 红。
#[test]
fn 确认早于退场_不是矛盾() {
    let (_d, root) = fresh();
    let m = root.memory();
    let k = put_card(m, &content("顺序正常"), &human_prov("t-h2"), NOW).unwrap();
    attest(m, &k, Stance::Confirmed, Some("t-h2"), None, NOW).unwrap();
    retire(m, &k, "manual", NOW + 10).unwrap();

    assert!(find_contradictions(m).unwrap().is_empty(), "确认在退场之前 ⇒ 是一次正常的先证后撤");
    assert_eq!(sweep(m, NOW + 20).unwrap().raised, 0);
}

#[test]
fn 判失败却解了退场账_行还活着() {
    let (_d, root) = fresh();
    let m = root.memory();
    let k = put_card(m, &content("复活失败行"), &human_prov("t-h3"), NOW).unwrap();
    set_verdict(m, &k, Verdict::Failed, NOW).unwrap();
    retire(m, &k, "manual", NOW + 1).unwrap();
    resolve_retirement(m, &k, NOW + 2).unwrap();

    let hits = find_contradictions(m).unwrap();
    assert_eq!(hits.len(), 1, "已解除的退场账 + failed 判定 + 行活着 ⇒ 恰一条");
    assert_eq!(hits[0], (MemTable::Card, k.id.clone()));
    assert_eq!(sweep(m, NOW + 3).unwrap(), SweepReport { raised: 1, resolved: 0 });
}

/// 条款②的反向半边 ②-a：判定是 `passed` ⇒ 解除退场账是正当操作，不是矛盾。
/// 牙：撤掉 `f.verdict = 'failed'` 那一半，本条与 `矛盾消解后_下一轮扫描自动解除` 一起红
/// （两条分别守住"判据被放宽"与"解除面被误伤"两个方向）。
#[test]
fn 判通过_解退场账不算矛盾() {
    let (_d, root) = fresh();
    let m = root.memory();
    let k = put_card(m, &content("判通过"), &human_prov("t-h4"), NOW).unwrap();
    set_verdict(m, &k, Verdict::Passed, NOW).unwrap();
    retire(m, &k, "manual", NOW + 1).unwrap();
    resolve_retirement(m, &k, NOW + 2).unwrap();
    assert!(find_contradictions(m).unwrap().is_empty(), "passed ⇒ 解除退场账是正当的");
}

/// 条款②的反向半边 ②-b：退场账**还**没解除 ⇒ 走的是条款①那一支，而这里没有更晚的正面表态 ⇒ 空。
/// 这一条的牙**不在本文件的判据里**：`revived_despite_failed` 不写 `r.resolved_at_ms IS NOT NULL`，
/// "这一行没有未解决的退场账"由共享谓词 `reader::retirement_pred`（`alive_where` 的一支）执法。
/// 所以要让本条红，得撤 `{alive}` 里的退场那一半（撤了它，一条未解决的退场账也能配上 failed 判定
/// 被当成"复活"）；在 anomaly 这一侧另写一句 `IS NOT NULL` 再把撤它当变异，是一道不会红的假变异
/// （派发档记录的实测：两处都写着时撤掉本句九条全绿——它本来就是同一条件的第二个家）。
#[test]
fn 未解除的退场账_归条款一管_这里为空() {
    let (_d, root) = fresh();
    let m = root.memory();
    let k = put_card(m, &content("退场未解"), &human_prov("t-h5"), NOW).unwrap();
    set_verdict(m, &k, Verdict::Failed, NOW).unwrap();
    retire(m, &k, "manual", NOW + 1).unwrap();
    assert!(find_contradictions(m).unwrap().is_empty(),
        "退场账未解决又没有更晚的确认 ⇒ 两支都不成立（行确实是死的，没有复活）");
}

#[test]
fn 重复扫描_不长出第二条未解决告警() {
    let (_d, root) = fresh();
    let m = root.memory();
    let k = put_card(m, &content("幂等卡"), &human_prov("t-h6"), NOW).unwrap();
    set_verdict(m, &k, Verdict::Failed, NOW).unwrap();
    retire(m, &k, "manual", NOW).unwrap();
    attest(m, &k, Stance::Corroborated, Some("t-h6"), None, NOW + 10).unwrap();

    assert_eq!(sweep(m, NOW + 20).unwrap().raised, 1,
        "第一次升一条（条款②：判失败后又被正面表态复活）");
    let again = sweep(m, NOW + 30).unwrap();
    assert_eq!(again, SweepReport { raised: 0, resolved: 0 }, "第二次既不升也不解");
    assert_eq!(open_count(m).unwrap(), 1, "未解决行仍恰一条");
}

#[test]
fn 矛盾消解后_下一轮扫描自动解除() {
    let (_d, root) = fresh();
    let m = root.memory();
    let k = put_card(m, &content("可消解"), &human_prov("t-h7"), NOW).unwrap();
    set_verdict(m, &k, Verdict::Failed, NOW).unwrap();
    retire(m, &k, "manual", NOW).unwrap();
    attest(m, &k, Stance::Confirmed, Some("t-h7"), None, NOW + 10).unwrap();
    assert_eq!(sweep(m, NOW + 20).unwrap().raised, 1);

    set_verdict(m, &k, Verdict::Passed, NOW + 30).unwrap();
    let rep = sweep(m, NOW + 40).unwrap();
    assert_eq!(rep, SweepReport { raised: 0, resolved: 1 },
        "判通过之后条款②不再成立：只解除、不新升");
    assert_eq!(open_count(m).unwrap(), 0, "未解决告警归零");
}

#[test]
fn 空库扫出空_不报错() {
    let (_d, root) = fresh();
    assert!(find_contradictions(root.memory()).unwrap().is_empty());
    assert_eq!(open_count(root.memory()).unwrap(), 0);
    assert_eq!(sweep(root.memory(), NOW).unwrap(), SweepReport { raised: 0, resolved: 0 });
}

/// `attention.mem_table` 在 DDL 里**没有 CHECK**（只有 `kind` 有），所以"表名写着产品不认的东西"
/// 这个形状外部改库就能造出来。产品自己造不出：`raise_attention` 收的是 `MemTable`，
/// 于是这里按取证形状直写 SQL。
/// 牙：把 `open_attention_targets` 的 `filter_map` 换成 `MemTable::parse(&t).unwrap()` ⇒ 本条 panic；
/// 换成"照名字解除"（未知名字也进解除面）⇒ 本条的 `resolved: 0` 与"仍挂着"双双红。
#[test]
fn 未知表名的告警_既不解也不崩() {
    let (_d, root) = fresh();
    let m = root.memory();
    m.conn().execute(
        "INSERT INTO attention(kind, mem_table, mem_id, created_at_ms, resolved_at_ms)
         VALUES('contradiction','bogus_table','x-1',1,NULL)", []).unwrap();
    let rep = sweep(m, NOW).unwrap();
    assert_eq!(rep, SweepReport { raised: 0, resolved: 0 },
        "未知表名不在本族的解除面上：不知道那个名字指什么就去解，就是静默销毁别人的账");
    assert_eq!(open_count(m).unwrap(), 1, "那一行必须还挂着未解决");
}

#[test]
#[allow(non_snake_case)] // 名字里的 ReadOnly 是 MemError 变体的实名，改小写就丢了它指认的那一格
fn 只读副本上_sweep_报_ReadOnly() {
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("replica.db");
    {
        let root = Root::open(dir.path()).unwrap();
        let m = root.memory();
        let k = put_card(m, &content("副本用卡"), &human_prov("t-h8"), NOW).unwrap();
        set_verdict(m, &k, Verdict::Failed, NOW).unwrap();
        retire(m, &k, "manual", NOW).unwrap();
        attest(m, &k, Stance::Confirmed, Some("t-h8"), None, NOW + 10).unwrap();
    }
    std::fs::copy(dir.path().join("memory.db"), &snap).unwrap();
    let ar = AgentRoot::open_replica(dir.path(), &snap).unwrap();
    let err = sweep(ar.memory(), NOW + 20).unwrap_err();
    assert!(matches!(err, MemError::ReadOnly), "只读面必须拒绝，实际 {err:?}");
}
