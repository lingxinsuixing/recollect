//! 复核面：只给告警本行盖 `rechecked_at_ms`，绝不动 `resolved_at_ms`。
//! 七条用例：盖戳与解除两列各管各的（1）、幂等（2）、严格大于（3）、已解除的行不参与（4）、
//! 只读面在写入口拒、在读入口照读（5）、同行配对不是装饰（6）、戳一旦盖上就不许被抹（7）。
use rc_memory::anomaly::sweep;
use rc_memory::recheck::{recheck_attested, recheck_pending};
use rc_memory::{attest, put_card, retire, set_verdict, AnchorRole, FactContent,
                MemError, MemKey, Provenance, Scope, ScopeKind, Stance, Verdict};
use rc_store::{AgentRoot, MemoryHandle, Root};

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

/// 一条未解决的矛盾告警（条款②）：卡 ⇒ 判失败 ⇒ 退场账 ⇒ 更晚的正面确认。
/// C24-N1：正面表态在同一事务里解除退场账，行按 `alive_where` 重新活过来，但审核已判 `failed`
/// ⇒ 条款②成立 ⇒ `sweep` 升一行。
/// 返回 `(键, 锚)`：正面表态必须复用这张卡自己的人工轮出处，否则 `attest` 当场
/// `AnchorNotHuman`（V6）。前置断言是这里唯一的牙——少了它，后面每一个数都能空绿。
fn raise_one(m: &MemoryHandle) -> (MemKey, String) {
    let uid = "t-recheck".to_string();
    let k = put_card(m, &content("死后又被确认"),
                     &[Provenance { turn_uid: uid.clone(), anchor_role: AnchorRole::Human }],
                     NOW).unwrap();
    set_verdict(m, &k, Verdict::Failed, NOW).unwrap();
    retire(m, &k, "manual", NOW + 1).unwrap();
    attest(m, &k, Stance::Confirmed, Some(&uid), None, NOW + 10).unwrap();
    let rep = sweep(m, NOW + 20).unwrap();
    assert_eq!(rep.raised, 1, "前置：告警真的升了一条，否则下面每个断言都是空过的");
    (k, uid)
}

fn fresh() -> (tempfile::TempDir, Root) {
    let d = tempfile::tempdir().unwrap();
    let r = Root::open(d.path()).unwrap();
    (d, r)
}

/// `raise_one` 的复数形：同一条卡流水线走两遍，`sweep` 因此升起**两行**指向不同目标的告警。
/// 形状同 raise_one（条款②），只是走两张卡。
/// 前置断言数的是 `raised == 2`——少了它，用例 6 的三个数都能在"其实只升了一行"上空绿。
fn raise_two(m: &MemoryHandle) -> (MemKey, MemKey, String) {
    let uid = "t-recheck".to_string();
    let mut keys = Vec::new();
    for title in ["死后又被确认", "死后又被确认之二"] {
        let k = put_card(m, &content(title),
                         &[Provenance { turn_uid: uid.clone(), anchor_role: AnchorRole::Human }],
                         NOW).unwrap();
        set_verdict(m, &k, Verdict::Failed, NOW).unwrap();
        retire(m, &k, "manual", NOW + 1).unwrap();
        attest(m, &k, Stance::Confirmed, Some(&uid), None, NOW + 10).unwrap();
        keys.push(k);
    }
    let rep = sweep(m, NOW + 20).unwrap();
    assert_eq!(rep.raised, 2, "前置：两行告警真的都升了，否则配对谓词无从验起");
    (keys[0].clone(), keys[1].clone(), uid)
}

/// 告警本行的两列时间戳。`attention` 此刻只有一行，所以不需要 `WHERE`。
fn stamps(m: &MemoryHandle) -> (Option<i64>, Option<i64>) {
    m.conn().query_row("SELECT rechecked_at_ms, resolved_at_ms FROM attention", [],
                       |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
}

/// 按目标 id 取盖戳那一列。库里有两行以上时 `stamps` 那种"整表一行"的读法不够用。
fn stamp_of(m: &MemoryHandle, id: &str) -> Option<i64> {
    m.conn().query_row("SELECT rechecked_at_ms FROM attention WHERE mem_id = ?1",
                       [id], |r| r.get(0)).unwrap()
}

#[test]
fn 更晚的表态把告警盖戳_且绝不动_resolved_at_ms() {
    let (_d, root) = fresh();
    let m = root.memory();
    let (k, uid) = raise_one(m);
    assert_eq!(recheck_pending(m).unwrap(), 1, "前置：有一条待复核");

    attest(m, &k, Stance::Corroborated, Some(&uid), None, NOW + 30).unwrap();
    assert_eq!(recheck_attested(m, NOW + 31).unwrap(), 1, "更晚的表态要盖这一行");
    assert_eq!(recheck_pending(m).unwrap(), 0, "盖过戳就不再待复核");
    assert_eq!(stamps(m), (Some(NOW + 31), None),
               "盖的是调用方给的那一戳；解除列必须由复核留着不动");
}

#[test]
fn 重复调用是幂等的_第二遍一个字节都不改() {
    let (_d, root) = fresh();
    let m = root.memory();
    let (k, uid) = raise_one(m);
    attest(m, &k, Stance::Corroborated, Some(&uid), None, NOW + 30).unwrap();

    assert_eq!(recheck_attested(m, NOW + 31).unwrap(), 1);
    assert_eq!(recheck_attested(m, NOW + 32).unwrap(), 0, "已经盖过的行第二遍不许再改");
    assert_eq!(stamps(m), (Some(NOW + 31), None),
               "第二遍不许把戳换成 NOW+32——否则『什么时候被复核过』读成『最近一次被扫过』");
}

#[test]
fn 表态不晚于告警_一条都不盖() {
    let (_d, root) = fresh();
    let m = root.memory();
    let (k, uid) = raise_one(m);
    // 告警的 `created_at_ms` 就是 `sweep(m, NOW + 20)` 那一戳。补一条**恰好同 ms** 的表态。
    attest(m, &k, Stance::Corroborated, Some(&uid), None, NOW + 20).unwrap();
    assert_eq!(recheck_attested(m, NOW + 21).unwrap(), 0, "同 ms 不算更晚：严格大于才成立");
    assert_eq!(recheck_pending(m).unwrap(), 1, "没盖 ⇒ 照旧待复核");
}

#[test]
fn 已解除的告警不参与复核() {
    let (_d, root) = fresh();
    let m = root.memory();
    let (k, uid) = raise_one(m);
    attest(m, &k, Stance::Corroborated, Some(&uid), None, NOW + 30).unwrap();
    // 判通过 ⇒ 条款②的 f.verdict = 'failed' 不再成立 ⇒ sweep 把告警解了（写 resolved_at_ms）。
    set_verdict(m, &k, Verdict::Passed, NOW + 40).unwrap();
    let rep = sweep(m, NOW + 41).unwrap();
    assert_eq!(rep.resolved, 1, "前置：告警真的被解除");

    assert_eq!(recheck_attested(m, NOW + 42).unwrap(), 0, "已解除的行不许被盖戳");
    assert_eq!(recheck_pending(m).unwrap(), 0, "待复核只数活着的告警");
    assert_eq!(stamps(m), (None, Some(NOW + 41)), "解除归解除，复核那一列一条都不许碰");
}

#[test]
fn 只读副本上写入口拒_读数照给() {
    // 形状照 `crates/rc-memory/tests/judgement.rs` 的 `writers_all_refuse_a_read_only_handle_before_touching_sql`：
    // `Root` 必须先进块作用域关掉——记忆库是 WAL（`ddl.rs` 的 `PRAGMA journal_mode=WAL`），
    // 句柄没关就 `fs::copy`，副本会缺未整理的帧 ⇒ 这一条红在"没数据"而不是红在守卫上。
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("replica.db");
    let rw_pending;
    {
        let root = Root::open(dir.path()).unwrap();
        raise_one(root.memory());
        rw_pending = recheck_pending(root.memory()).unwrap();
    }
    std::fs::copy(dir.path().join("memory.db"), &snap).unwrap();
    let ar = AgentRoot::open_replica(dir.path(), &snap).unwrap();

    let err = recheck_attested(ar.memory(), NOW + 31).unwrap_err();
    assert!(matches!(err, MemError::ReadOnly), "盖戳是写，只读面必须在碰 SQL 之前就拒，实际 {err:?}");
    assert_eq!(recheck_pending(ar.memory()).unwrap(), rw_pending,
               "复核计数是读：hook 那一侧拿着副本柄也要能读到同一格（与 `anomaly::open_count` 同族）");
}

#[test]
fn 只给自己有更晚表态的那一行盖戳_同行配对不是装饰() {
    // 牙（第九道变异）：把 `a.mem_table = attention.mem_table AND a.mem_id = attention.mem_id`
    // 折成 `1=1` ⇒ 第三句红（B 被 A 的表态连带盖了戳），第一句的 `1` 也会变成 `2`。
    // 这一格为什么必须有牙：T13 的重影两族一升起就是多目标同行共存，配对谓词是那时唯一的守门人。
    let (_d, root) = fresh();
    let m = root.memory();
    let (a, b, uid) = raise_two(m);
    attest(m, &a, Stance::Corroborated, Some(&uid), None, NOW + 30).unwrap();

    assert_eq!(recheck_attested(m, NOW + 31).unwrap(), 1, "只有 A 有一行更晚的表态");
    assert_eq!(stamp_of(m, &a.id), Some(NOW + 31), "A 那一行盖上了");
    assert_eq!(stamp_of(m, &b.id), None, "B 没有更晚的表态，不许被 A 的那一行连带盖戳");
    assert_eq!(recheck_pending(m).unwrap(), 1, "待复核那一格此刻只数 B");
}

#[test]
fn 盖过戳之后被解除_那一戳必须留着() {
    // 牙（第十道变异）：`resolve_attention` 的 UPDATE 加一句 `rechecked_at_ms = NULL` ⇒ 第二句红。
    // 守的是审计面：解除之后还要能分清"复核过才解"与"从没复核就解"——`recheck_pending` 只数活行，
    // 这一格是死行上那一戳唯一的证人。
    let (_d, root) = fresh();
    let m = root.memory();
    let (k, uid) = raise_one(m);
    attest(m, &k, Stance::Corroborated, Some(&uid), None, NOW + 30).unwrap();
    assert_eq!(recheck_attested(m, NOW + 31).unwrap(), 1, "前置：先盖上一戳");

    set_verdict(m, &k, Verdict::Passed, NOW + 40).unwrap();
    let rep = sweep(m, NOW + 41).unwrap();
    assert_eq!(rep.resolved, 1, "前置：告警随后被机械解除");
    assert_eq!(stamps(m), (Some(NOW + 31), Some(NOW + 41)),
               "解除只写解除那一列；已经盖过的戳不许被抹掉");
}
