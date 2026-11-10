use rc_memory::{put_assertion, put_card, put_entity, put_event, AnchorRole, EntityContent,
                 FactContent, MemError, MemKey, Provenance, Scope, ScopeKind, Stance, attest};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

fn scope() -> Scope { Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() } }
fn prov(role: AnchorRole) -> Vec<Provenance> {
    vec![Provenance { turn_uid: "a".repeat(64), anchor_role: role }]
}
fn card() -> FactContent {
    FactContent { kind: "decision".into(), title: "先跑测试再合并".into(),
                  body: "任何合并之前先跑全量测试，红的不许合。".into(),
                  scope: scope(), valid_to_ms: None }
}

#[test]
fn x_writes_content_but_the_p_columns_come_from_the_writer() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k = put_card(root.memory(), &card(), &prov(AnchorRole::Human), 1730000000000).unwrap();
    let (title, agent, dev, created, norm): (String, String, String, i64, i64) = root.memory().conn()
        .query_row("SELECT title, source_agent, device_id, created_at_ms, norm_version FROM card WHERE id=?1",
                   [&k.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
        .unwrap();
    assert_eq!(title, "先跑测试再合并");
    assert_eq!((agent.as_str(), dev.as_str()), ("synthetic-agent", "dev-synthetic-1"),
        "P 列的值只有一个来源：写入者的 p_stamps，X 的入参里根本没有这些字段");
    assert_eq!(created, 1730000000000);
    assert_eq!(norm, rc_store::capability::NORM_VERSION as i64);
}

#[test]
fn content_row_without_provenance_fails_to_be_written() {
    // §2 / Python 侧 §3.2.1 规则 2 原话：X 写内容列必带出处，缺出处即写入失败，无例外。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let e = put_card(root.memory(), &card(), &[], 1730000000000).unwrap_err();
    assert!(matches!(e, MemError::ProvenanceMissing("card")), "{e}");
    for t in ["card", "provenance"] {
        let n: i64 = root.memory().conn()
            .query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "{t} 留下了半行：缺出处是写不出来，不是写了一半");
    }
}

#[test]
fn an_empty_title_is_refused_not_stored_as_a_blank_handle() {
    // signpost 给的是 id + 标题原样；标题为空的那一行在注入块里是一行看不懂的提醒。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let mut c = card();
    c.title = "   ".into();
    assert!(matches!(put_card(root.memory(), &c, &prov(AnchorRole::Human), 1).unwrap_err(),
                     MemError::ContentEmpty));
}

#[test]
fn replica_handle_refuses_every_write() {
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("replica.db");
    {
        let root = Root::open(dir.path()).unwrap();
        put_card(root.memory(), &card(), &prov(AnchorRole::Human), 1730000000000).unwrap();
    }
    std::fs::copy(dir.path().join("memory.db"), &snap).unwrap();
    let ar = rc_store::AgentRoot::open_replica(dir.path(), &snap).unwrap();
    assert!(matches!(put_card(ar.memory(), &card(), &prov(AnchorRole::Human), 1).unwrap_err(),
                     MemError::ReadOnly));
}

#[test]
fn promote_verdict_只把_unreviewed_推到_passed_且降级不被翻案() {
    // S4 的写入口：review 只有"unreviewed → passed"这一条单向 CAS。三态逐个点名——
    // 提升命中、重复调用 0 行（幂等）、audit 的 low/failed 不许被翻案。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let k = put_card(m, &card(), &prov(AnchorRole::Human), NOW).unwrap();
    let verdict = |id: &str| -> String {
        m.conn()
            .query_row("SELECT verdict FROM card WHERE id=?1", [id], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(verdict(&k.id), "unreviewed", "put_card 的默认判决必须是 unreviewed（本用例的前提）");
    assert_eq!(
        rc_memory::promote_verdict(m, &k, NOW).unwrap(),
        1,
        "unreviewed → passed 必须命中一行"
    );
    assert_eq!(verdict(&k.id), "passed");
    assert_eq!(
        rc_memory::promote_verdict(m, &k, NOW).unwrap(),
        0,
        "已提升再提是 0 行：幂等不是错误（与 resolve_retirement 同一把尺子）"
    );
    // audit 的降级权：`low`/`failed` 只能由 set_verdict 写，review 的 CAS 不许把它们翻案。
    for v in ["low", "failed"] {
        m.conn()
            .execute("UPDATE card SET verdict=?1 WHERE id=?2", rusqlite::params![v, k.id])
            .unwrap();
        assert_eq!(rc_memory::promote_verdict(m, &k, NOW).unwrap(), 0, "{v} 被 review 提升了");
        assert_eq!(verdict(&k.id), v, "{v} 被改写");
    }
    // 只认 card：锚没有这一列，指到它是"根本没有这个形态"（NotFound），不是"写坏了"（Sql）。
    let anchor = MemKey::parse(&format!("anchor:{}", "a".repeat(64))).unwrap();
    assert!(matches!(
        rc_memory::promote_verdict(m, &anchor, NOW),
        Err(MemError::NotFound(_))
    ));
}

#[test]
fn promote_verdict_在只读副本上拒绝而不是静默零行() {
    // 0 行是"那张卡不是 unreviewed"的业务读数；只读句柄连语句都不许发 ⇒ 必须是 ReadOnly，
    // 两者混在一格会让调用方把"没权限"读成"已经审过了"。
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("replica.db");
    let k = {
        let root = Root::open(dir.path()).unwrap();
        let k = put_card(root.memory(), &card(), &prov(AnchorRole::Human), NOW).unwrap();
        std::fs::copy(dir.path().join("memory.db"), &snap).unwrap();
        k
    };
    let ar = rc_store::AgentRoot::open_replica(dir.path(), &snap).unwrap();
    assert!(matches!(
        rc_memory::promote_verdict(ar.memory(), &k, NOW),
        Err(MemError::ReadOnly)
    ));
}

#[test]
fn revert_verdict_只把_passed_退回_unreviewed_且降级不被翻案() {
    // `hub review backfill-verdict --revert` 的写入口：回滚是 `passed → unreviewed` 的单向
    // CAS。三态逐个点名——退回命中、重复回滚 0 行（幂等）、audit 的 low/failed 不许被
    // "回滚"顺手改写（那正是禁用 set_verdict 的理由：它的 from 是 None，认得下任何一行）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let k = put_card(m, &card(), &prov(AnchorRole::Human), NOW).unwrap();
    let verdict = |id: &str| -> String {
        m.conn()
            .query_row("SELECT verdict FROM card WHERE id=?1", [id], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(
        rc_memory::revert_verdict(m, &k, NOW).unwrap(),
        0,
        "还没提升过的卡回滚是 0 行：from 对不上就是读数，不是错误"
    );
    assert_eq!(verdict(&k.id), "unreviewed");
    rc_memory::promote_verdict(m, &k, NOW).unwrap();
    assert_eq!(verdict(&k.id), "passed");
    assert_eq!(
        rc_memory::revert_verdict(m, &k, NOW).unwrap(),
        1,
        "passed → unreviewed 必须命中一行"
    );
    assert_eq!(verdict(&k.id), "unreviewed");
    assert_eq!(
        rc_memory::revert_verdict(m, &k, NOW).unwrap(),
        0,
        "重复回滚是 0 行：幂等不是错误"
    );
    // audit 的降级权：`low`/`failed` 不是回滚的目标，CAS 的 from 必须挡住它们。
    for v in ["low", "failed"] {
        m.conn()
            .execute("UPDATE card SET verdict=?1 WHERE id=?2", rusqlite::params![v, k.id])
            .unwrap();
        assert_eq!(
            rc_memory::revert_verdict(m, &k, NOW).unwrap(),
            0,
            "{v} 被回滚改写了"
        );
        assert_eq!(verdict(&k.id), v, "{v} 被改写");
    }
    // 只认 card：锚没有这一列 ⇒ NotFound（"没有这个形态"），不是 Sql。
    let anchor = MemKey::parse(&format!("anchor:{}", "a".repeat(64))).unwrap();
    assert!(matches!(
        rc_memory::revert_verdict(m, &anchor, NOW),
        Err(MemError::NotFound(_))
    ));
}

#[test]
fn revert_verdict_在只读副本上拒绝而不是静默零行() {
    // 与 promote 同一条尺子：只读句柄连语句都不许发（ReadOnly 不是"0 行"）。
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("replica.db");
    let k = {
        let root = Root::open(dir.path()).unwrap();
        let k = put_card(root.memory(), &card(), &prov(AnchorRole::Human), NOW).unwrap();
        rc_memory::promote_verdict(root.memory(), &k, NOW).unwrap();
        std::fs::copy(dir.path().join("memory.db"), &snap).unwrap();
        k
    };
    let ar = rc_store::AgentRoot::open_replica(dir.path(), &snap).unwrap();
    assert!(matches!(
        rc_memory::revert_verdict(ar.memory(), &k, NOW),
        Err(MemError::ReadOnly)
    ));
}

#[test]
fn 出处按句柄取回且顺序稳定() {
    // 任务 7 的 export 要一跳溯源，而 rc-agent 的依赖集里没有 rusqlite ⇒ 这条 SQL 只能住在
    // 拥有 provenance 这张表的 crate 里（守卫 11 的列归属）。
    // 两条出处**倒着播**（先 b 后 a）：返回顺序仍是 uid 升序 ⇒ 断的是 ORDER BY，不是插入顺序。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k = put_card(root.memory(), &card(),
        &[Provenance { turn_uid: "b".repeat(64), anchor_role: AnchorRole::Agent }], 1).unwrap();
    put_card(root.memory(), &card(), &prov(AnchorRole::Human), 2).unwrap();
    let got = rc_memory::reader::provenance_of(root.memory(), &k).unwrap();
    assert_eq!(got.len(), 2, "出处是一张集合，读回来两条都得在");
    assert_eq!(got[0].0, "a".repeat(64));
    assert_eq!(got[1].0, "b".repeat(64));
    assert_eq!(got[0].1, AnchorRole::Human, "角色各自要能还原回枚举");
    assert_eq!(got[1].1, AnchorRole::Agent);
}

#[test]
fn 跨表读取按表名与句柄排序且两次读逐位置相等() {
    // `reader.rs` 的 `facts()` 承诺"同一份库两次读出来一样"，而 `UNION ALL` 的分段顺序 SQLite
    // 根本不承诺 ⇒ 兑现这句承诺的只有查询末尾的 ORDER BY tbl, id。canonical 之后只剩两张表
    // （anchor / card）：写入顺序刻意是 card 行在前、anchor 行在后，一旦 ORDER BY 被摘掉，
    // 读回来的序退回臂序（card 在前），下面的 assert_eq 当场对不上。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let mut other = card();
    other.body = "任何合并之前先跑全量测试，红的不许合。补一句。".into();
    let c1 = put_card(root.memory(), &card(), &prov(AnchorRole::Human), 1).unwrap();
    let c2 = put_card(root.memory(), &other, &prov(AnchorRole::Human), 2).unwrap();
    let ev = put_event(root.memory(), &FactContent {
        kind: "meeting".into(), title: "会议记录归档".into(), body: "会上记录了一条决定".into(),
        scope: scope(), valid_to_ms: None,
    }, &prov(AnchorRole::Agent), 3).unwrap();
    let asrt = put_assertion(root.memory(), &FactContent {
        kind: "claim".into(), title: "处境类断言一行".into(), body: "那条断言到明天还成立".into(),
        scope: scope(), valid_to_ms: None,
    }, &prov(AnchorRole::System), 4).unwrap();
    let en = put_entity(root.memory(), &EntityContent {
        kind: "person".into(), title: "老王".into(),
        scope: Scope { kind: ScopeKind::Person, key: "p-synthetic".into() },
        aliases: vec!["老王".into()],
    }, &prov(AnchorRole::Human), 5).unwrap();

    // 期望序 = tbl 升序（anchor < card），同表内再按 id 升序。
    // id 的序在 Rust 里按字节比：两边都是小写 hex，与 SQLite 的 BINARY collation 逐字符等价。
    let mut cards = [c1, c2, ev, asrt];
    cards.sort_by(|a, b| a.id.cmp(&b.id));
    let want: Vec<String> = std::iter::once(en.key_string())
        .chain(cards.iter().map(|k| k.key_string())).collect();
    let once = rc_memory::facts(root.memory()).unwrap();
    let twice = rc_memory::facts(root.memory()).unwrap();
    let got: Vec<String> = once.iter().map(|r| r.key.key_string()).collect();
    let again: Vec<String> = twice.iter().map(|r| r.key.key_string()).collect();
    assert_eq!(got, want, "facts() 的读序不是 ORDER BY tbl, id 给的序");
    assert_eq!(again, got, "同一份库两次 facts() 读出来的顺序不一样");
    let titles: Vec<String> = rc_memory::all_titles(root.memory()).unwrap().iter()
        .map(|(k, _)| k.key_string()).collect();
    assert_eq!(titles, got, "all_titles() 是第二条 UNION ALL，两次读的序必须同源同序");
    assert_eq!(titles, want, "all_titles() 没跟上同一条 ORDER BY tbl, id");
}

/// 本文件新写的辅助函数（**不是 import**）：从 `provenance_of` 里取第一条
/// `role == AnchorRole::Human` 的 `turn_uid`。任务 3 的用例里没有同名辅助，而 `tests/*.rs`
/// 互不 import，所以这里各写一份（阶段 0 起的纪律）。
/// `seed` 的第一行（`ks[0]`，同分三张卡的第一张）出处角色就是 `Human`（`synth.rs` 的
/// `prov(AnchorRole::Human, 0)`），所以本用例不需要换夹具——换了就是要改用例迁就实现。
fn seeded_human_uid(m: &rc_store::MemoryHandle, key: &MemKey) -> String {
    rc_memory::reader::provenance_of(m, key)
        .unwrap()
        .into_iter()
        .find(|(_, role)| *role == AnchorRole::Human)
        .map(|(uid, _)| uid)
        .unwrap_or_else(|| panic!("夹具前提破了：{} 没有一条人工轮出处", key.key_string()))
}

#[test]
fn 悬空表态数得出来() {
    use rc_memory::reader::dangling_attestations;
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    assert_eq!(dangling_attestations(m).unwrap(), 0, "空库的读数必须是 0 而不是错误");
    let ks = rc_memory::synth::seed(m, NOW).unwrap();
    // 有锚的正面表态：不算悬空
    attest(m, &ks[0], Stance::Confirmed, Some(&seeded_human_uid(m, &ks[0])), Some("格子"), NOW).unwrap();
    assert_eq!(dangling_attestations(m).unwrap(), 0, "正常表态被数成悬空");
    // 悬空的一条：形状合法但库里没有这行，而且**免锚的型绕得过 `attest` 的锚校验**
    let ghost = MemKey::parse(&format!("card:{}", "f".repeat(64))).unwrap();
    attest(m, &ghost, Stance::Rejected, None, Some("无价值"), NOW).unwrap();
    assert_eq!(dangling_attestations(m).unwrap(), 1, "悬空表态数不出来 ⇒ status 那条读数是装饰");
}

#[test]
fn the_anchor_arm_of_the_dangling_count_is_a_live_branch() {
    // canonical 之后 `dangling_attestations` 的锚分支（'anchor'）在本阶段
    // 第一次被非零读数跑到。**若那条分支被删**，本条会以 2 != 1 红而不是 0 != 1：
    // 真锚的那条表态也会因为"两张表里查不到它"而被数成悬空。这才是"分支活着"的读数。
    use rc_memory::{attest, MemKey, Stance};
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let e = put_entity(root.memory(), &EntityContent { kind: "person".into(), title: "老王".into(),
        scope: Scope { kind: ScopeKind::Person, key: "p-synthetic".into() },
        aliases: vec!["老王".into()] }, &prov(AnchorRole::Human), NOW).unwrap();
    // ① 存在行上的正面表态：不算悬空。锚 = 上面那条 human 出处。
    attest(root.memory(), &e, Stance::Confirmed, Some(&"a".repeat(64)), Some("A6 夹具"), NOW).unwrap();
    // ② 幽灵 anchor 句柄上免锚的否决：绕得过 `attest` 的锚校验，数得出来才算有牙。
    let ghost = MemKey::parse(&format!("anchor:{}", "f".repeat(64))).unwrap();
    attest(root.memory(), &ghost, Stance::Rejected, None, Some("A6 夹具"), NOW).unwrap();
    assert_eq!(rc_memory::reader::dangling_attestations(root.memory()).unwrap(), 1,
        "锚分支死了会读成 2：两条表态都查不到行");
}
