use rc_memory::writers_c::retire_edge;
use rc_memory::{attest, bump_cooldown, clear_expired, put_card, put_entity, lookup, retire,
                 resolve_retirement, set_verdict, supersede, AnchorRole, EntityContent, FactContent,
                 MemError, MemKey, MemTable, Provenance, Scope, ScopeKind, Stance, Verdict};
use rc_store::Root;

fn scope() -> Scope { Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() } }
fn prov() -> Vec<Provenance> { vec![Provenance { turn_uid: "t".repeat(64), anchor_role: AnchorRole::Human }] }
fn card() -> FactContent {
    FactContent { kind: "decision".into(), title: "先跑测试再合并".into(),
                  body: "红的不许合。".into(), scope: scope(), valid_to_ms: None }
}
fn one() -> (tempfile::TempDir, Root, MemKey) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k = put_card(root.memory(), &card(), &prov(), 1).unwrap();
    (dir, root, k)
}

/// `NotFound` 的回执要点名"传进去的那一把"句柄，所以这里只把载荷取出来，变体名留给调用方比。
/// 用例不许写成 `matches!(err, MemError::NotFound(_))`——那种断言对任何一把键都能过。
fn notfound_key(err: MemError) -> MemKey {
    match err {
        MemError::NotFound(key) => key,
        other => panic!("期望 NotFound，实际是 {other:?}"),
    }
}

#[test]
fn 缺席句柄三支回执皆点名句柄且取代指针读得回来() {
    // NotFound 的三支（`fact_table` 的两个入口 + `exactly_one` 的 `rowcount == 0`）此前一次都没红过，
    // 而 `supersede` 全仓零调用者 ⇒ 这一条把四点一次钉住。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let ent = put_entity(root.memory(), &EntityContent {
        kind: "person".into(), title: "老王".into(),
        scope: Scope { kind: ScopeKind::Person, key: "p-synthetic".into() },
        aliases: vec!["老王".into()],
    }, &prov(), 1).unwrap();
    let by = "0".repeat(64);

    // ①② anchor 在 canonical DDL 里没有 verdict / superseded_by 列。这里刻意用一条**真实存在**的锚：
    // 报的"没有这个形态"就不是"没有这一行"，两支都必须 NotFound 且点同一把柄。
    assert_eq!(notfound_key(set_verdict(root.memory(), &ent, Verdict::Passed, 2).unwrap_err()), ent,
               "set_verdict 打向 anchor 的回执没点名传进去的那把句柄");
    assert_eq!(notfound_key(supersede(root.memory(), &ent, &by, 3).unwrap_err()), ent,
               "supersede 打向 anchor 的回执没点名传进去的那把句柄");
    // ③ 合法（64 hex）但库里根本不存在的 card 键 ⇒ `exactly_one` 的 `rowcount == 0` 那一支。
    let ghost = MemKey { table: MemTable::Card, id: "9".repeat(64) };
    assert_eq!(notfound_key(supersede(root.memory(), &ghost, &by, 4).unwrap_err()), ghost,
               "rowcount == 0 的回执没点名那把不存在的 card 句柄");

    // ④ 让 `supersede` 有真实调用者：慢路径的落点要能从 reader 读回来。
    let older = put_card(root.memory(), &card(), &prov(), 5).unwrap();
    let mut newer = card();
    newer.body = "红的不许合。补一句：先复现再判。".into();
    let newer = put_card(root.memory(), &newer, &prov(), 6).unwrap();
    assert_ne!(older, newer, "正文不同就是两条内容；同柄的话这轮取代变成自我指向");
    supersede(root.memory(), &older, &newer.id, 7).unwrap();
    assert_eq!(lookup(root.memory(), &older).unwrap().unwrap().superseded_by.as_deref(),
               Some(newer.id.as_str()), "被取代指针没落库 ⇒ 慢路径的账读不回来");
    assert_eq!(lookup(root.memory(), &newer).unwrap().unwrap().superseded_by, None,
               "顶替者自己也被写成了已取代：指针一次只该落在被顶掉的那一行");
}

#[test]
fn positive_attestation_requires_an_anchor_resolving_to_a_human_turn() {
    let (_d, root, k) = one();
    // 正面表态免锚 ⇒ 拒（§2：A 只经表态入口，正面四型必带解析到人工轮的锚）
    assert!(matches!(attest(root.memory(), &k, Stance::Confirmed, None, None, 10).unwrap_err(),
                     MemError::AnchorNotHuman));
    // 锚 uid 存在但不是人工轮的出处 ⇒ 同样拒（"必带锚"不等于"随便填一串"）
    assert!(matches!(attest(root.memory(), &k, Stance::Confirmed, Some(&"x".repeat(64)), None, 11).unwrap_err(),
                     MemError::AnchorNotHuman));
    assert!(attest(root.memory(), &k, Stance::Confirmed, Some(&"t".repeat(64)), Some("我确认过"), 12).is_ok());
    // 免锚只限 rejected 与退场两型（继承原规则）
    assert!(attest(root.memory(), &k, Stance::Rejected, None, Some("与最新结论冲突"), 13).is_ok());
    assert!(attest(root.memory(), &k, Stance::Retired, None, Some("无价值"), 14).is_ok());
}

#[test]
fn resolve_retirement_is_the_one_shape_and_it_is_idempotent() {
    // 守卫 11 ⑤：解除只有一种落库形状（写 resolved_at_ms），不新增"反悔"表。
    let (_d, root, k) = one();
    retire(root.memory(), &k, "判同后保留另一条", 2).unwrap();
    assert!(lookup(root.memory(), &k).unwrap().unwrap().retired);
    resolve_retirement(root.memory(), &k, 3).unwrap();
    assert!(!lookup(root.memory(), &k).unwrap().unwrap().retired);
    resolve_retirement(root.memory(), &k, 4).unwrap();
    let n: i64 = root.memory().conn()
        .query_row("SELECT count(*) FROM retirement WHERE resolved_at_ms IS NOT NULL", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "解除两次不许长出第二条账");
    let stale: i64 = root.memory().conn().query_row(
        "SELECT count(*) FROM retirement WHERE mem_id=?1 AND resolved_at_ms IS NULL", [&k.id], |r| r.get(0)).unwrap();
    assert_eq!(stale, 0);
}

#[test]
fn verdict_is_c_only_and_reads_back_as_a_fact() {
    let (_d, root, k) = one();
    assert_eq!(lookup(root.memory(), &k).unwrap().unwrap().verdict, Verdict::Unreviewed);
    set_verdict(root.memory(), &k, Verdict::Failed, 2).unwrap();
    assert_eq!(lookup(root.memory(), &k).unwrap().unwrap().verdict, Verdict::Failed);
    set_verdict(root.memory(), &k, Verdict::Passed, 3).unwrap();
    assert_eq!(lookup(root.memory(), &k).unwrap().unwrap().verdict, Verdict::Passed);
}

#[test]
fn cooldown_is_a_column_not_a_rank_and_the_window_is_explicit() {
    // §8：冷却只让这一轮不再提，不改形态判定；`recall()` 里仍然返回。
    let (_d, root, k) = one();
    assert_eq!(bump_cooldown(root.memory(), std::slice::from_ref(&k), 1_000, 500).unwrap(), 1);
    assert_eq!(lookup(root.memory(), &k).unwrap().unwrap().cooldown_until_ms, Some(1_500));
    assert_eq!(clear_expired(root.memory(), 1_400).unwrap(), 0, "还没到期不许清");
    assert_eq!(clear_expired(root.memory(), 1_500).unwrap(), 1);
    assert_eq!(lookup(root.memory(), &k).unwrap().unwrap().cooldown_until_ms, None);
}

#[test]
fn writers_all_refuse_a_read_only_handle_before_touching_sql() {
    // conn() 是逃生口（阶段 0 任务 7 Step 4 那条妥协），所以"只读"这一半必须由写入者守住。
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("replica.db");
    let k;
    {
        let root = Root::open(dir.path()).unwrap();
        k = put_card(root.memory(), &card(), &prov(), 1).unwrap();
    }
    std::fs::copy(dir.path().join("memory.db"), &snap).unwrap();
    let ar = rc_store::AgentRoot::open_replica(dir.path(), &snap).unwrap();
    assert!(matches!(attest(ar.memory(), &k, Stance::Retired, None, None, 1).unwrap_err(), MemError::ReadOnly));
    assert!(matches!(bump_cooldown(ar.memory(), std::slice::from_ref(&k), 1, 1).unwrap_err(), MemError::ReadOnly));
    assert!(matches!(retire(ar.memory(), &k, "r", 1).unwrap_err(), MemError::ReadOnly));
    assert!(matches!(set_verdict(ar.memory(), &k, Verdict::Passed, 1).unwrap_err(), MemError::ReadOnly));
    // T6 加的 `retire_edge` 同属 writers_c 那一族的写入口，同样第一句必须是只读闸门——
    // 它落在这一条断言里而不是新写一条用例：这条断言覆盖的是上面那几个写入口。
    // **不写全称**：`writers_c` 的 `supersede` 与 `resolve_retirement` 两个写入口至今没有被
    // 任何用例钉住（既存缺口，不是 T6 引入的），留待别处补——这里不假装已经全了。
    assert!(matches!(retire_edge(ar.memory(), &"0".repeat(64), "r", 1).unwrap_err(), MemError::ReadOnly));
}
