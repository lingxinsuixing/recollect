// crates/rc-memory/tests/gate.rs
//! S1b-C4a：读闸 `rc_memory::gate` 的规则矩阵（设计 ownership-persistence §4.1）。
//!
//! 这里喂的是**真表里的归属行 + `RowFacts` 字面量**：归属从 `resource_owner` 读，
//! 行事实由 `reader::RowFacts` 的形状直接给（读闸只读 `key`/`scope`/`source_agent` 三格，
//! 不碰正文）。逐格钉住的是 §4.1 那张短路表，顺序也是判据的一部分：
//! 1) Maintenance 面永远放行；2) 缺表只放行"行 source_agent 恰等于 viewer"；
//! 3) 无归属行同上；4) owner 读自己；5) 有效 grant；6) public；7) project 同 scope；
//! 8) person 且 Human 面且 principal 相等且非空；9) org 无身份来源 ⇒ OrgUnsupported；
//! 10) 其余按 visibility 分格拒（private/person/project 三支）。
//!
//! **fail-closed 的方向**：每一格"认不出/没有/过期"都只往更窄的一档折，
//! 没有一条路径能把"不知道"读成"允许"。

use rc_memory::gate::{facts_visible, lookup_visible, Deny, GateIndex, Surface, Viewer};
use rc_memory::keys::{AnchorRole, MemKey, MemTable, Scope, ScopeKind, Verdict};
use rc_memory::reader::RowFacts;
use rc_memory::writers_x::{put_batch, BatchCtx, WriteItem};
use rc_memory::Provenance;
use rc_store::{MemoryHandle, Root};

/// 闸门判定用的"现在"。所有过期用例都相对它算。
const NOW: i64 = 10_000;

fn env() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    (dir, root)
}

/// 一行事实的**最小形状**：读闸只读 `key`/`scope`/`source_agent`，其余格填常量。
/// `source_agent` 就是"这一行是谁产的"那一格——无归属行时它是唯一的身份线索。
fn row(id: &str, scope: (ScopeKind, &str), source_agent: &str) -> RowFacts {
    RowFacts {
        key: MemKey { table: MemTable::Card, id: id.into() },
        created_at_ms: 1_000,
        kind: "note".into(),
        title: "闸门行".into(),
        body: "闸门合成正文".into(),
        as_of_ms: 1_000,
        valid_to_ms: None,
        superseded_by: None,
        retired: false,
        rejected: false,
        verdict: Verdict::Unreviewed,
        attestation: None,
        anchor_role: AnchorRole::Human,
        cooldown_until_ms: None,
        scope: Scope { kind: scope.0, key: scope.1.into() },
        device_id: "dev-gate".into(),
        source_agent: source_agent.into(),
    }
}

/// 一张锚行没有署名两列（`reader` 补空串）⇒ 读闸在无归属行时认不出主人。
fn anchor_row(id: &str) -> RowFacts {
    RowFacts { key: MemKey { table: MemTable::Anchor, id: id.into() }, ..row(id, (ScopeKind::Project, "proj-1"), "") }
}

/// 写一行归属（12 列全给）。`visibility` 是字面量：本文件的判据就是"库里那串 slug
/// 读成哪一档"，写侧归一（`owner::effective`）不在这条用例的射程里。
fn put_owner(
    m: &MemoryHandle,
    id: &str,
    owner_agent: &str,
    principal: Option<&str>,
    visibility: &str,
    scope: (ScopeKind, &str),
) {
    m.conn()
        .execute(
            "INSERT INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                        created_by_agent, visibility, retention, scope_kind,
                                        scope_key, source_agent, device_id, created_at_ms)
             VALUES('card', ?1, ?2, ?3, ?3, ?4, 'default', ?5, ?6, ?3, 'dev-gate', 1000)",
            rusqlite::params![id, principal, owner_agent, visibility, scope.0.as_str(), scope.1],
        )
        .unwrap();
}

/// 一条授权。字段给全是为了让每条用例只动**一格**（"哪一格让它失效"是判据本身）。
struct G<'a> {
    id: &'a str,
    from: &'a str,
    to_agent: Option<&'a str>,
    to_principal: Option<&'a str>,
    capability: &'a str,
    data_class: &'a str,
    scope_kind: &'a str,
    scope_key: &'a str,
    granted_at_ms: i64,
    expires_at_ms: Option<i64>,
    revoked_at_ms: Option<i64>,
}

/// 基线：发给 `friend` 的全局 recall/memory 授权、不过期、不撤回。
fn grant(id: &str) -> G<'_> {
    G {
        id,
        from: "owner-A",
        to_agent: Some("friend"),
        to_principal: None,
        capability: "recall",
        data_class: "memory",
        scope_kind: "global",
        scope_key: "",
        granted_at_ms: 1_000,
        expires_at_ms: None,
        revoked_at_ms: None,
    }
}

fn insert_grant(m: &MemoryHandle, g: G<'_>) {
    m.conn()
        .execute(
            "INSERT INTO resource_grant(id, from_principal_id, from_agent_id, to_agent_id,
                                        to_principal_id, capability, data_class, scope_kind,
                                        scope_key, purpose, granted_at_ms, expires_at_ms, revoked_at_ms)
             VALUES(?1, 'p-owner', ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'gate matrix', ?9, ?10, ?11)",
            rusqlite::params![
                g.id, g.from, g.to_agent, g.to_principal, g.capability, g.data_class,
                g.scope_kind, g.scope_key, g.granted_at_ms, g.expires_at_ms, g.revoked_at_ms
            ],
        )
        .unwrap();
}

/// 一行**真卡**（`lookup_visible` 走 `reader::lookup`，要真行才读得回来）。
/// 归属行由 C3 的写入继承一起落下（owner=署名 agent、private_agent）。
fn seed_card(m: &MemoryHandle, agent: &str, title: &str) -> MemKey {
    let items = vec![WriteItem {
        table: MemTable::Card,
        kind: "note".into(),
        title: title.into(),
        body: "读闸句柄合成正文".into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-1".into() },
        valid_to_ms: None,
        aliases: vec![],
        prov: vec![Provenance { turn_uid: "9".repeat(64), anchor_role: AnchorRole::Human }],
    }];
    put_batch(m, &items, &BatchCtx { source_agent: agent.into(), device_id: "dev-gate".into() }, 1_000)
        .unwrap()
        .remove(0)
        .expect("读闸夹具的行不被抑制")
}

/// 一次装载 + 一次判定（`load` 是"每调用面一次"，矩阵里每格重新装一次，判定语义不受缓存影响）。
fn decide(m: &MemoryHandle, v: &Viewer, f: &RowFacts) -> Result<(), Deny> {
    GateIndex::load(m, v, NOW).unwrap().decide(f)
}

fn ids(rows: &[RowFacts]) -> Vec<String> {
    rows.iter().map(|f| f.key.key_string()).collect()
}

/// §4.1 第 1/4 条与第 6–10 条的主矩阵：五 visibility × {owner, other agent, maintenance}。
/// other 那一列是"没有任何授权、没有 scope、不是 owner"的 agent 面——它只能拿到 public。
#[test]
fn five_visibilities_against_owner_other_and_maintenance() {
    let (_d, root) = env();
    let m = root.memory();
    // `None` = 这一格必须放行（只有 public），`Some(d)` = 必须落到这一格拒。
    let visibilities: [(&str, Option<Deny>); 5] = [
        ("private_agent", Some(Deny::PrivateAgent)),
        ("person", Some(Deny::PersonNeedsGrant)),
        ("project", Some(Deny::ScopeMismatch)),
        ("org", Some(Deny::OrgUnsupported)),
        ("public", None),
    ];
    for (i, (visibility, other)) in visibilities.iter().enumerate() {
        let id = format!("row-{i}");
        put_owner(m, &id, "owner-A", Some("p-owner"), visibility, (ScopeKind::Project, "proj-1"));
        let f = row(&id, (ScopeKind::Project, "proj-1"), "owner-A");

        assert_eq!(decide(m, &Viewer::agent("owner-A"), &f), Ok(()), "{visibility}：owner 必须能读自己");
        assert_eq!(decide(m, &Viewer::maintenance(), &f), Ok(()), "{visibility}：Maintenance 面必须放行");
        let got = decide(m, &Viewer::agent("stranger"), &f);
        match other {
            None => assert_eq!(got, Ok(()), "{visibility}：public 行对陌生 agent 必须放行"),
            Some(d) => assert_eq!(got, Err(*d),
                                  "{visibility}：陌生 agent 那一格错了（第 10 条按 visibility 分格拒）"),
        }
    }
}

/// §4.1 第 7 条：project 的判据是**scope 全等**（kind 与 key 都比），不是 key 撞上就算。
#[test]
fn project_rows_need_the_identical_scope() {
    let (_d, root) = env();
    let m = root.memory();
    put_owner(m, "p-1", "owner-A", None, "project", (ScopeKind::Project, "proj-1"));
    let f = row("p-1", (ScopeKind::Project, "proj-1"), "owner-A");
    let same = Viewer::agent("friend").with_scope(Scope { kind: ScopeKind::Project, key: "proj-1".into() });
    let other_key = Viewer::agent("friend").with_scope(Scope { kind: ScopeKind::Project, key: "proj-2".into() });
    let other_kind = Viewer::agent("friend").with_scope(Scope { kind: ScopeKind::Person, key: "proj-1".into() });
    let no_scope = Viewer::agent("friend");
    assert_eq!(decide(m, &same, &f), Ok(()), "同 scope 的同行 agent 必须放行");
    assert_eq!(decide(m, &other_key, &f), Err(Deny::ScopeMismatch), "scope_key 不同却放行了");
    assert_eq!(decide(m, &other_kind, &f), Err(Deny::ScopeMismatch), "scope_kind 不同却放行了");
    assert_eq!(decide(m, &no_scope, &f), Err(Deny::ScopeMismatch), "viewer 没有 scope 却放行了");
}

/// §4.1 第 8 条：person 只对 **Human 面 + principal 相等且非空**放行；
/// Agent 面即便递了 principal 也不放行（"同 principal 不自动放行"的读闸形状）。
#[test]
fn person_rows_open_only_to_the_registered_principal_on_the_human_surface() {
    let (_d, root) = env();
    let m = root.memory();
    put_owner(m, "per-1", "owner-A", Some("p-owner"), "person", (ScopeKind::Person, "person-A"));
    put_owner(m, "per-2", "owner-A", None, "person", (ScopeKind::Person, "person-A"));
    let f = row("per-1", (ScopeKind::Person, "person-A"), "owner-A");
    let unowned = row("per-2", (ScopeKind::Person, "person-A"), "owner-A");

    let human_ok = Viewer::human("agent-H").with_principal("p-owner");
    let human_other = Viewer::human("agent-H").with_principal("p-someone");
    let human_anon = Viewer::human("agent-H");
    let agent_with_principal = Viewer::agent("agent-H").with_principal("p-owner");

    assert_eq!(decide(m, &human_ok, &f), Ok(()), "Human 面 principal 相等必须放行");
    assert_eq!(decide(m, &human_other, &f), Err(Deny::PersonNeedsGrant), "principal 不等却放行了");
    assert_eq!(decide(m, &human_anon, &f), Err(Deny::PersonNeedsGrant), "无 principal 却放行了");
    // W37-A1：4b 落在 visibility 分格**之前**——同 principal 的异 agent（含 Agent 面）
    // 经 4b 放行，旧口径"同 principal 不自动放行"已被 G12 裁定取代。
    assert_eq!(decide(m, &agent_with_principal, &f), Ok(()),
               "同 principal 异 agent 在 person 行上被拒了（4b 在 visibility 之前）");
    // person 格自身的判据还在：principal 带了但不等 ⇒ 仍落 PersonNeedsGrant。
    assert_eq!(decide(m, &Viewer::agent("agent-H").with_principal("p-someone"), &f),
               Err(Deny::PersonNeedsGrant),
               "principal 不等的 Agent 面越过了 person 格（4b 没命中就该拒）");
    assert_eq!(decide(m, &Viewer::human("agent-H"), &unowned), Err(Deny::PersonNeedsGrant),
               "owner 没有登记自然人时，读者没有 principal 也放行了（非空是判据之一）");
}

/// §4.1 第 9 条：org 今天没有身份来源 ⇒ 两个非维护面都只拿到 OrgUnsupported（不是放行）。
#[test]
fn org_rows_are_unsupported_on_both_live_surfaces() {
    let (_d, root) = env();
    let m = root.memory();
    put_owner(m, "org-1", "owner-A", Some("p-owner"), "org", (ScopeKind::Project, "proj-1"));
    let f = row("org-1", (ScopeKind::Project, "proj-1"), "owner-A");
    assert_eq!(decide(m, &Viewer::agent("friend"), &f), Err(Deny::OrgUnsupported));
    // W37-A1：4b 在 visibility 分格之前——同 principal 异 agent 经 4b 放行，
    // 不再落到 OrgUnsupported；principal 不等的那位仍落原格。
    assert_eq!(decide(m, &Viewer::human("agent-H").with_principal("p-owner"), &f), Ok(()),
               "同 principal 异 agent 读 org 行没走 4b");
    assert_eq!(decide(m, &Viewer::human("agent-H").with_principal("p-other"), &f),
               Err(Deny::OrgUnsupported), "principal 不等的读者越过了 org 格");
}

/// §4.1 第 5 条：有效 grant 的六道门——target、capability（按面）、data_class、
/// 撤回、过期、scope（global 或与 owner.scope 全等）。每一条失效都只落在拒上。
#[test]
fn a_grant_opens_only_when_every_clause_holds() {
    let (_d, root) = env();
    let m = root.memory();
    put_owner(m, "g-1", "owner-A", None, "project", (ScopeKind::Project, "proj-1"));
    let f = row("g-1", (ScopeKind::Project, "proj-1"), "owner-A");
    let friend = Viewer::agent("friend");
    let stranger = Viewer::agent("stranger");
    // 这是"没有授权时"的落格：project 行 + 无 scope 的读者 ⇒ ScopeMismatch。
    // 下面每条失效授权都必须回到这里，不许变成 Ok。
    assert_eq!(decide(m, &friend, &f), Err(Deny::ScopeMismatch), "前置：无授权时本不该放行");

    // ① 有效 global ⇒ 放行
    insert_grant(m, grant("g-global"));
    assert_eq!(decide(m, &friend, &f), Ok(()), "有效 global 授权没有放行");

    // ② 没被授权的那一位仍拒（授权不是对所有人的放行）
    assert_eq!(decide(m, &stranger, &f), Err(Deny::ScopeMismatch), "授权泄漏到了未指名的 agent");

    // ③ 同 scope ⇒ 放行
    insert_grant(m, G { id: "g-scope", scope_kind: "project", scope_key: "proj-1", ..grant("g-scope") });
    assert_eq!(decide(m, &friend, &f), Ok(()), "与 owner.scope 全等的授权没有放行");

    // ④ 过期（expires_at_ms <= now）⇒ 回落到拒
    insert_grant(m, G { id: "g-expired", expires_at_ms: Some(NOW), ..grant("g-expired") });
    assert_eq!(decide(m, &Viewer::agent("expired-friend"), &f), Err(Deny::ScopeMismatch),
               "过期授权仍在放行");
    insert_grant(m, G { id: "g-expired-2", to_agent: Some("expired-friend-2"),
                        expires_at_ms: Some(NOW - 1), ..grant("g-expired-2") });
    assert_eq!(decide(m, &Viewer::agent("expired-friend-2"), &f), Err(Deny::ScopeMismatch),
               "expires_at_ms < now 的授权仍在放行");

    // ⑤ 撤回 ⇒ 回落到拒（行保留、读时判定）
    insert_grant(m, G { id: "g-revoked", to_agent: Some("revoked-friend"),
                        revoked_at_ms: Some(NOW - 1), ..grant("g-revoked") });
    assert_eq!(decide(m, &Viewer::agent("revoked-friend"), &f), Err(Deny::ScopeMismatch),
               "已撤回的授权仍在放行");

    // ⑥ capability 按面取：Agent 面要 recall，"给了 export"不等于能读
    insert_grant(m, G { id: "g-cap", to_agent: Some("export-friend"), capability: "export",
                        ..grant("g-cap") });
    assert_eq!(decide(m, &Viewer::agent("export-friend"), &f), Err(Deny::ScopeMismatch),
               "export 授权在 Agent 面上被当成 recall 用了");

    // ⑦ data_class 必须是 memory
    insert_grant(m, G { id: "g-class", to_agent: Some("transcript-friend"), data_class: "transcript",
                        ..grant("g-class") });
    assert_eq!(decide(m, &Viewer::agent("transcript-friend"), &f), Err(Deny::ScopeMismatch),
               "transcript 授权放开了 memory 行");

    // ⑧ scope 不匹配（key 不同 / kind 不同）
    insert_grant(m, G { id: "g-scope-2", to_agent: Some("scope-friend"), scope_kind: "project",
                        scope_key: "proj-2", ..grant("g-scope-2") });
    assert_eq!(decide(m, &Viewer::agent("scope-friend"), &f), Err(Deny::ScopeMismatch),
               "scope_key 不同的授权放行了");
    insert_grant(m, G { id: "g-scope-3", to_agent: Some("scope-friend-3"), scope_kind: "person",
                        scope_key: "proj-1", ..grant("g-scope-3") });
    assert_eq!(decide(m, &Viewer::agent("scope-friend-3"), &f), Err(Deny::ScopeMismatch),
               "scope_kind 不同的授权放行了");

    // ⑨ 发给别的 agent ⇒ 拒（同 scope 那份授权不为"没点名的那位"背书）
    insert_grant(m, G { id: "g-elsewhere", to_agent: Some("someone-else"), ..grant("g-elsewhere") });
    assert_eq!(decide(m, &friend, &f), Ok(()), "前置：同 scope 的那条授权仍在 ⇒ 放行只能来自它");
    assert_eq!(decide(m, &Viewer::agent("elsewhere"), &f), Err(Deny::ScopeMismatch),
               "没有发给这一位的授权放行了");
}

/// §4.1 第 5 条（自然人那一支）：Human 面要 `export` 能力，`to_principal_id` 相等才放行。
#[test]
fn a_principal_grant_opens_the_human_surface_only_with_the_export_capability() {
    let (_d, root) = env();
    let m = root.memory();
    put_owner(m, "h-1", "owner-A", None, "project", (ScopeKind::Project, "proj-1"));
    let f = row("h-1", (ScopeKind::Project, "proj-1"), "owner-A");
    let human = Viewer::human("agent-H").with_principal("p-viewer");

    insert_grant(m, G { id: "h-export", to_agent: None, to_principal: Some("p-viewer"),
                        capability: "export", ..grant("h-export") });
    assert_eq!(decide(m, &human, &f), Ok(()), "发给这一位的 export 授权没有放行 Human 面");

    // 同一张表、发给别的自然人 ⇒ 拒（principal 相等是判据）
    let other = Viewer::human("agent-H").with_principal("p-other");
    assert_eq!(decide(m, &other, &f), Err(Deny::ScopeMismatch), "发给别人的授权放行了");
}

/// §4.1 第 2/3 条：没有归属行时**唯一**的放行是"行 source_agent 恰等于 viewer"。
/// 锚行没有署名两列（reader 补空串）⇒ 永远走不进这一支。
#[test]
fn a_row_without_an_owner_row_is_readable_only_by_its_named_producer() {
    let (_d, root) = env();
    let m = root.memory();
    let f = row("orphan-1", (ScopeKind::Project, "proj-1"), "producer-A");
    assert_eq!(decide(m, &Viewer::agent("producer-A"), &f), Ok(()),
               "行 source_agent 恰等于 viewer 时必须放行（否则旧库的抽取行全部消失）");
    assert_eq!(decide(m, &Viewer::agent("producer-B"), &f), Err(Deny::NoOwnerRow),
               "没有归属行的行被别的 agent 读到了");
    assert_eq!(decide(m, &Viewer::human("agent-H").with_principal("p-owner"), &f),
               Err(Deny::NoOwnerRow), "没有归属行的行被 Human 面读到了");
    assert_eq!(decide(m, &Viewer::agent("producer-A"), &anchor_row("anchor-1")), Err(Deny::NoOwnerRow),
               "锚行（source_agent 为空串）在无归属行时被放行了");

    // 有归属行时，第 3 条不再适用：行 source_agent 与 owner 不同 ⇒ 按归属判。
    put_owner(m, "orphan-1", "owner-A", None, "private_agent", (ScopeKind::Project, "proj-1"));
    assert_eq!(decide(m, &Viewer::agent("producer-A"), &f), Err(Deny::PrivateAgent),
               "写了归属行之后 source_agent 仍然能读（第 3 条越过了第 4 条）");
}

/// §4.1 第 2 条：缺 `resource_owner` 表（旧副本/旧快照）不报错，只降可见性；
/// 旧库里"自己的行"仍要读得到（否则一次快照倒退整库变空）。
#[test]
fn a_missing_owner_table_degrades_to_producer_only_instead_of_failing() {
    let (_d, root) = env();
    let m = root.memory();
    m.conn().execute("DROP TABLE resource_owner", []).unwrap();
    let f = row("legacy-1", (ScopeKind::Project, "proj-1"), "producer-A");
    assert_eq!(decide(m, &Viewer::agent("producer-A"), &f), Ok(()),
               "缺表时本 agent 自己的行没读出来（整库变空）");
    assert_eq!(decide(m, &Viewer::agent("producer-B"), &f), Err(Deny::TableMissing),
               "缺表时别的 agent 读到了行");
    assert_eq!(decide(m, &Viewer::human("agent-H"), &f), Err(Deny::TableMissing),
               "缺表时 Human 面读到了别的 agent 的行");
    assert_eq!(decide(m, &Viewer::maintenance(), &f), Ok(()), "Maintenance 面必须照旧放行");
    // 缺表也不许把"不过闸"的那条路弄坏。
    assert_eq!(ids(&facts_visible(m, None, NOW, vec![f.clone()]).unwrap()),
               vec!["card:legacy-1".to_string()], "viewer=None 在缺表时也不再原样返回");
}

/// `viewer == None`（中心 CLI/测试/旧调用）⇒ 原样返回：不装载、不筛、不报错。
#[test]
fn facts_visible_without_a_viewer_returns_the_input_untouched() {
    let (_d, root) = env();
    let m = root.memory();
    // 两行分属两个 agent、都没有归属行、visibility 也无从谈起：有闸的话至少有一行会被拒。
    let a = row("none-a", (ScopeKind::Project, "proj-1"), "producer-A");
    let b = row("none-b", (ScopeKind::Project, "proj-1"), "producer-B");
    let out = facts_visible(m, None, NOW, vec![a, b]).unwrap();
    assert_eq!(ids(&out), vec!["card:none-a".to_string(), "card:none-b".to_string()],
               "viewer=None 改了输入（顺序或数量）");
}

/// `facts_visible` 的闸门分支：只留允许的行、其余静默缺席（不报"无权限"）。
#[test]
fn facts_visible_filters_denied_rows_silently() {
    let (_d, root) = env();
    let m = root.memory();
    put_owner(m, "fv-mine", "owner-A", None, "private_agent", (ScopeKind::Project, "proj-1"));
    put_owner(m, "fv-open", "owner-B", None, "public", (ScopeKind::Project, "proj-1"));
    put_owner(m, "fv-theirs", "owner-B", None, "private_agent", (ScopeKind::Project, "proj-1"));
    let facts = vec![
        row("fv-mine", (ScopeKind::Project, "proj-1"), "owner-A"),
        row("fv-open", (ScopeKind::Project, "proj-1"), "owner-B"),
        row("fv-theirs", (ScopeKind::Project, "proj-1"), "owner-B"),
    ];
    let out = facts_visible(m, Some(&Viewer::agent("owner-A")), NOW, facts).unwrap();
    assert_eq!(ids(&out), vec!["card:fv-mine".to_string(), "card:fv-open".to_string()],
               "被拒的行没有静默缺席，或该留的被筛掉了");
}

/// `lookup_visible`：被拒与查不到**同形**（都 `None`）——不泄露存在性。
#[test]
fn lookup_visible_hides_denied_rows_behind_the_same_none_as_absent_ones() {
    let (_d, root) = env();
    let m = root.memory();
    // 真行：`lookup_visible` 的第一跳是 `reader::lookup`，归属行随写落下。
    let mine = seed_card(m, "owner-A", "自己的行");
    let theirs = seed_card(m, "owner-B", "别人的行");
    let ghost = MemKey { table: MemTable::Card, id: "f".repeat(64) };

    assert!(lookup_visible(m, Some(&Viewer::agent("owner-A")), NOW, &mine).unwrap().is_some(),
            "自己的行没读回来");
    assert!(lookup_visible(m, Some(&Viewer::agent("owner-A")), NOW, &theirs).unwrap().is_none(),
            "别人的 private 行被读回来了");
    assert!(lookup_visible(m, Some(&Viewer::agent("owner-A")), NOW, &ghost).unwrap().is_none(),
            "查不到的句柄不再是 None（与拒绝不同形 ⇒ 泄露存在性）");
    assert!(lookup_visible(m, None, NOW, &theirs).unwrap().is_some(),
            "viewer=None 时 lookups 被闸住了");
}

/// 认不出的 `visibility` **只降可见性**：这里绕过表的 CHECK 塞一个脏值
/// （`PRAGMA ignore_check_constraints`），读侧必须落成 private_agent，而不是"看不懂就放行"。
#[test]
fn an_unrecognized_visibility_slug_collapses_to_private_agent() {
    let (_d, root) = env();
    let m = root.memory();
    m.conn().execute_batch("PRAGMA ignore_check_constraints=ON").unwrap();
    put_owner(m, "dirty-1", "owner-A", None, "visible_to_friends", (ScopeKind::Project, "proj-1"));
    m.conn().execute_batch("PRAGMA ignore_check_constraints=OFF").unwrap();
    // 前置：脏值真的写进去了（否则本用例什么都没测）。
    let stored: String = m.conn()
        .query_row("SELECT visibility FROM resource_owner WHERE mem_id='dirty-1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, "visible_to_friends", "前置：脏 visibility 没写进去，本用例空过");

    let f = row("dirty-1", (ScopeKind::Project, "proj-1"), "owner-A");
    assert_eq!(decide(m, &Viewer::agent("stranger"), &f), Err(Deny::PrivateAgent),
               "认不出的 visibility 被读成了比 private_agent 更宽的一档");
    assert_eq!(decide(m, &Viewer::agent("owner-A"), &f), Ok(()), "owner 自己必须仍能读");
}

/// C4b2b：`allows`（只知道 `table:id`）与 `decide`（带行事实）必须是**同一套规则**。
///
/// 逐格对比：五档 visibility × {owner, 陌生 agent, grant 收件人, 同 scope 的陌生 agent,
/// Human + principal, Maintenance}，外加"没有归属行"那一格。夹具行的 `source_agent`
/// 故意不等于任何 viewer——`allows` 看不到这一格，所以 `decide` 的第 2/3 条也不许被它触发，
/// 两条路在每一格上必须**同号**（差异只允许出现在 `allows` 更窄的方向）。
#[test]
fn allows_and_decide_agree_cell_by_cell() {
    let (_d, root) = env();
    let m = root.memory();
    let visibilities = ["private_agent", "person", "project", "org", "public"];
    for (i, visibility) in visibilities.iter().enumerate() {
        put_owner(m, &format!("agree-{i}"), "owner-A", Some("p-owner"), visibility,
                  (ScopeKind::Project, "proj-1"));
    }
    // 一条有效 grant：第 5 条在两条路上都必须放行（否则"owner 自读"会掩盖 grant 那一路的差异）。
    insert_grant(m, G { id: "g-agree", to_agent: Some("friend"), ..grant("g-agree") });
    let viewers = vec![
        Viewer::agent("owner-A"),
        Viewer::agent("stranger"),
        Viewer::agent("friend"),
        Viewer::agent("stranger").with_scope(Scope { kind: ScopeKind::Project, key: "proj-1".into() }),
        Viewer::human("agent-H").with_principal("p-owner"),
        Viewer::maintenance(),
    ];
    let mut cells = 0usize;
    for v in &viewers {
        let gate = GateIndex::load(m, v, NOW).unwrap();
        for (i, visibility) in visibilities.iter().enumerate() {
            let id = format!("agree-{i}");
            let f = row(&id, (ScopeKind::Project, "proj-1"), "producer-x");
            assert_eq!(decide(m, v, &f).is_ok(), gate.allows("card", &id),
                       "{} 在 {id}（{visibility}）上两条路的结论不同", v.agent_id);
            cells += 1;
        }
        // 没有归属行：署名不是 viewer ⇒ 第 3 条不放行；`allows` 看不到署名 ⇒ 也拒。
        let no_row = row("agree-missing", (ScopeKind::Project, "proj-1"), "producer-x");
        assert_eq!(decide(m, v, &no_row).is_ok(), gate.allows("card", "agree-missing"),
                   "{} 在「没有归属行」那一格上两条路的结论不同", v.agent_id);
        cells += 1;
    }
    assert_eq!(cells, viewers.len() * (visibilities.len() + 1), "逐格对比漏了格：{cells}");
}

/// C4b2b：`allows` 在**缺表**（旧副本/旧快照）时只能 false——第 2 条那条"行署名恰等于
/// viewer"的放行在句柄路上没有行事实可看，所以它比 `decide` 更窄，绝不会更宽。
#[test]
fn allows_is_fail_closed_when_the_owner_table_is_missing() {
    let (_d, root) = env();
    let m = root.memory();
    m.conn().execute("DROP TABLE resource_owner", []).unwrap();
    let f = row("legacy-1", (ScopeKind::Project, "proj-1"), "producer-B");
    // 同号格：署名的不是 viewer ⇒ `decide` 也拒（TableMissing），`allows` 同样拒。
    assert_eq!(decide(m, &Viewer::agent("producer-A"), &f), Err(Deny::TableMissing));
    let same = GateIndex::load(m, &Viewer::agent("producer-A"), NOW).unwrap();
    assert!(!same.allows("card", "legacy-1"), "缺表时 allows 按句柄放行了（它看不到行署名）");
    let human = GateIndex::load(m, &Viewer::human("agent-H"), NOW).unwrap();
    assert!(!human.allows("card", "legacy-1"), "缺表时 Human 面的 allows 放行了");
    // 唯一比 `decide` 窄的那一格：署名恰等于 viewer 时 `decide` 放行、`allows` 仍拒。
    let own = row("legacy-1", (ScopeKind::Project, "proj-1"), "producer-A");
    assert_eq!(decide(m, &Viewer::agent("producer-A"), &own), Ok(()));
    assert!(!same.allows("card", "legacy-1"), "allows 在缺表时把「证明不了来源」读成了放行");
}

/// `Surface` 的三个构造器与 agent_id 契约（`maintenance` 那一位是常量）。
#[test]
fn the_viewer_constructors_carry_their_surface_and_identity() {
    let agent = Viewer::agent("agent-A");
    assert_eq!(agent.surface, Surface::Agent);
    assert_eq!(agent.agent_id, "agent-A");
    assert_eq!(agent.principal_id, None);
    assert!(agent.scope.is_none());

    let human = Viewer::human("agent-H").with_principal("p-1")
        .with_scope(Scope { kind: ScopeKind::Person, key: "person-1".into() });
    assert_eq!(human.surface, Surface::Human);
    assert_eq!(human.principal_id.as_deref(), Some("p-1"));
    assert_eq!(human.scope.as_ref().map(|s| (s.kind, s.key.as_str())),
               Some((ScopeKind::Person, "person-1")));

    let maintenance = Viewer::maintenance();
    assert_eq!(maintenance.surface, Surface::Maintenance);
    assert_eq!(maintenance.agent_id, "maintenance");
}

// ── W37-A1（G12）：规则 4b（同 principal 异 agent 互读）与环境锚例外 ──────────

/// 一行真锚 + 归属（`mem_table='anchor'`）。`kind` 由调用方给：`'object'` 是环境锚
/// （[`rc_memory::ENV_ANCHOR_KIND`]），其余 kind 的锚不是。
fn put_anchor(
    m: &MemoryHandle,
    id: &str,
    kind: &str,
    owner_agent: &str,
    principal: Option<&str>,
    visibility: &str,
    scope: (ScopeKind, &str),
) {
    let conn = m.conn();
    conn.execute(
        "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key, status,
                            created_at_ms, updated_at_ms, norm_version)
         VALUES(?1, ?2, ?1, 'real', ?3, ?4, 'active', 1000, 1000, 1)",
        rusqlite::params![id, kind, scope.0.as_str(), scope.1],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                    created_by_agent, visibility, retention, scope_kind,
                                    scope_key, source_agent, device_id, created_at_ms)
         VALUES('anchor', ?1, ?2, ?3, ?3, ?4, 'default', ?5, ?6, ?3, 'dev-gate', 1000)",
        rusqlite::params![id, principal, owner_agent, visibility, scope.0.as_str(), scope.1],
    )
    .unwrap();
}

/// 钉①（design §5-1）＋ 变异 c 的两个入口：agent-A(p1) 的 private 卡，
/// viewer = agent-B **with_principal(p1)** ⇒ `decide` 臂与 `allows_reason`（句柄臂）都放行。
/// 钉②（§5-2）阴性对照：agent-C(p2) 与"无 principal 的 agent-B"都拒，且与"查不到"同形
/// （这里落到 `Deny::PrivateAgent`；消费面上被拒行进 `unresolved_handles`，同形由 recall 集成臂钉）。
#[test]
fn rule_4b_same_principal_cross_agent_reads_and_its_negative_controls() {
    let (_d, root) = env();
    let m = root.memory();
    put_owner(m, "g12-1", "agent-A", Some("p1"), "private_agent", (ScopeKind::Project, "proj-1"));
    let f = row("g12-1", (ScopeKind::Project, "proj-1"), "agent-A");

    // 钉①·decide 臂
    assert_eq!(decide(m, &Viewer::agent("agent-B").with_principal("p1"), &f), Ok(()),
               "同 principal 异 agent 的 private 行被拒了（4b 没生效）");
    // 变异 c·句柄臂：`allows_reason` 与 `decide` 必须同号（两条路共用 decide_owner_row，
    // 但这条断言防的是"有人把 4b 摘进了其中一个入口"）。
    let gate = GateIndex::load(m, &Viewer::agent("agent-B").with_principal("p1"), NOW).unwrap();
    assert_eq!(gate.allows_reason("card", "g12-1"), Ok(()),
               "句柄臂上同 principal 异 agent 被拒了");

    // 钉②：异 principal ⇒ 拒；无 principal ⇒ 拒（4b 永不命中，fail-closed）。
    assert_eq!(decide(m, &Viewer::agent("agent-C").with_principal("p2"), &f),
               Err(Deny::PrivateAgent), "异 principal 的 agent 被放行了");
    assert_eq!(decide(m, &Viewer::agent("agent-B"), &f),
               Err(Deny::PrivateAgent), "无 principal 的 agent 被放行了（principal_eq 的空半边失效）");
    // 钉④：无 principal 的读数与改动前逐位相同——落格还是 PrivateAgent，不是别的类。
    // owner 自读走第 4 条，与 4b 无关，照旧放行。
    assert_eq!(decide(m, &Viewer::agent("agent-A"), &f), Ok(()), "owner 自读被 4b 改动了");
}

/// 钉③（design §5-3）环境锚例外两臂：同 principal 异 agent
/// 读**环境锚**（含句柄臂）⇒ 拒（仍 `PrivateAgent`，不新增 Deny 变体）；
/// 读同 principal 的**非环境锚**（kind='person' 的锚行）与卡 ⇒ 放行——
/// 例外只减 4b 的放行范围，没吞掉别的行。owner 自读环境锚照旧（第 4 条先命中）。
#[test]
fn environment_anchor_exception_holds_while_other_rows_still_pass_4b() {
    let (_d, root) = env();
    let m = root.memory();
    // 前臂：环境锚（kind='object'）
    put_anchor(m, "env-anchor", "object", "agent-A", Some("p1"), "private_agent",
               (ScopeKind::Project, "proj-1"));
    let env_row = anchor_row("env-anchor");
    // 后臂参照：同库两张非环境锚的行——person 锚 + agent-A 的卡。
    put_anchor(m, "person-anchor", "person", "agent-A", Some("p1"), "private_agent",
               (ScopeKind::Project, "proj-1"));
    put_owner(m, "g12-card", "agent-A", Some("p1"), "private_agent", (ScopeKind::Project, "proj-1"));

    let viewer = Viewer::agent("agent-B").with_principal("p1");
    assert_eq!(decide(m, &viewer, &env_row), Err(Deny::PrivateAgent),
               "同 principal 异 agent 读到了环境锚（例外没生效）");
    let gate = GateIndex::load(m, &viewer, NOW).unwrap();
    assert_eq!(gate.allows_reason("anchor", "env-anchor"), Err(Deny::PrivateAgent),
               "句柄臂上环境锚例外失效");

    // 例外没吞掉别的行：person 锚与卡都经 4b 放行（person 锚证明判据是 kind 不是 mem_table）。
    assert_eq!(decide(m, &viewer, &row("g12-card", (ScopeKind::Project, "proj-1"), "agent-A")), Ok(()),
               "同 principal 的卡被环境锚例外误吞了");
    assert_eq!(decide(m, &viewer, &anchor_row("person-anchor")), Ok(()),
               "同 principal 的非环境锚被例外误吞了（判据漂成了 mem_table）");
    // 护栏：owner 自读环境锚仍走第 4 条。
    assert_eq!(decide(m, &Viewer::agent("agent-A"), &env_row), Ok(()), "owner 自读环境锚被例外改掉了");
}
