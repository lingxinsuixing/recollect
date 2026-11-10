//! 判同候选的来源是**记忆库直读**（§2.15 第 1 条），所以这一层的四条用例断的是
//! "谁在这个候选集合里、谁不在"，不断分数、不断顺序（那是 dedupe.rs 的事）。
use rc_memory::{directory_sql, entity_directory, owner::{OwnerCtx, Visibility}, rows_in_scope,
                scope_of, unreviewed_rows,
                writers_x::{put_batch, put_batch_with_owner, put_timeline_rows, BatchCtx,
                            TimelineContent, WriteItem},
                AnchorRole, MemKey, MemTable, Provenance, RowFacts, Scope, ScopeKind};
use rc_store::Root;

fn scope() -> Scope { Scope { kind: ScopeKind::Project, key: "proj-A".into() } }
fn other() -> Scope { Scope { kind: ScopeKind::Project, key: "proj-B".into() } }
fn prov() -> Vec<Provenance> { vec![Provenance { turn_uid: "f".repeat(64), anchor_role: AnchorRole::Human }] }

/// 直接写三张候选表：判同读的是**已经落库的行**，走 `put_*` 旧四把就够，
/// 不需要 `put_batch`（那条的夹具在任务 6 的 `put_batch.rs` 里，两处不共用是为了读得出各断什么）。
fn card(root: &Root, title: &str, body: &str, at: i64, sc: &Scope) -> MemKey {
    rc_memory::put_card(root.memory(), &rc_memory::FactContent {
        kind: "lesson".into(), title: title.into(), body: body.into(),
        scope: sc.clone(), valid_to_ms: None,
    }, &prov(), at).unwrap()
}

/// 归属人可指定的单条 card：`put_card` 的 owner 恒为 `synthetic-agent`（旧四把的合成默认），
/// 这里要造"同 scope、不同归属人"，所以走带 owner 的整批入口。
fn card_owned(root: &Root, title: &str, body: &str, at: i64, sc: &Scope, agent: &str) -> MemKey {
    card_owned_as(root, title, body, at, sc, None, agent)
}

/// 归属的**两格**都可指定的单条 card（C5c-fix）：principal 轴要能造出 `NULL` 与 `'p1'`
/// 两种邻居，而 `card_owned` 恒把 principal 写成 `None` —— 那样测到的只有 agent 轴。
/// 夹具不手写 `resource_owner`：走 `put_batch_with_owner` 的真写入路径，读数才与生产同形。
fn card_owned_as(root: &Root, title: &str, body: &str, at: i64, sc: &Scope,
                 principal: Option<&str>, agent: &str) -> MemKey {
    let items = vec![WriteItem {
        table: MemTable::Card,
        kind: "lesson".into(),
        title: title.into(),
        body: body.into(),
        scope: sc.clone(),
        valid_to_ms: None,
        aliases: vec![],
        prov: prov(),
    }];
    let ctx = BatchCtx { source_agent: agent.into(), device_id: "dev-test-1".into() };
    let owner = OwnerCtx {
        owner_principal_id: principal.map(|p| p.to_string()),
        owner_agent_id: agent.into(),
        visibility: Visibility::PrivateAgent,
        retention: "default".into(),
    };
    put_batch_with_owner(root.memory(), &items, &ctx, &owner, at, &[])
        .unwrap()
        .into_iter()
        .next()
        .flatten()
        .unwrap()
}

fn titles(rows: &[RowFacts]) -> Vec<String> {
    let mut v: Vec<String> = rows.iter().map(|r| r.title.clone()).collect();
    v.sort();
    v
}

#[test]
fn rows_in_scope_returns_only_same_table_same_scope_siblings() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let a = card(&root, "甲", "同一批正文", 1_000, &scope());
    let _b = card(&root, "乙", "同一批正文二", 1_001, &scope());   // 同表同归属 ⇒ 该出现
    let _c = card(&root, "丙", "别的归属", 1_002, &other());     // 归属不同 ⇒ 不该出现
    let _e = rc_memory::put_event(root.memory(), &rc_memory::FactContent {
        kind: "milestone".into(), title: "丁".into(), body: "同一批正文".into(),
        scope: scope(), valid_to_ms: None,
    }, &prov(), 1_003).unwrap();                                   // 旧 event 已并入 card ⇒ 同表，该出现
    let _anchor = rc_memory::put_entity(root.memory(), &rc_memory::EntityContent {
        kind: "person".into(), title: "戊".into(), scope: scope(), aliases: vec![],
    }, &prov(), 1_004).unwrap();                                   // 表不同（anchor）⇒ 不该出现

    let got = rows_in_scope(root.memory(), MemTable::Card, &scope(), &a.id).unwrap();
    let mut want = vec!["乙".to_string(), "丁".to_string()];
    want.sort();
    assert_eq!(titles(&got), want, "自指的那行要被排除，异归属异表的也要");
}

/// 判同会写 `superseded_by`（永久顶掉一行）⇒ 同 scope 但不属于同一个人（agent）的行
/// 不能进候选池：否则别人家的一条"部署在十点"会被并走，连同出处一起消失。
#[test]
fn rows_in_scope_never_returns_another_agents_row_in_the_same_scope() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let probe = card_owned(&root, "探针", "部署在十点", 1_000, &scope(), "agent-a");
    let _mine = card_owned(&root, "同人邻居", "部署在十点", 1_001, &scope(), "agent-a");
    let _theirs = card_owned(&root, "别家邻居", "部署在十点", 1_002, &scope(), "agent-b");

    let got = rows_in_scope(root.memory(), MemTable::Card, &scope(), &probe.id).unwrap();
    assert_eq!(titles(&got), vec!["同人邻居".to_string()],
               "同 scope 同人的行才是邻居；另一个 agent 的行必须留在集合外");
}

/// C5c-fix 用例 1：**principal 轴放宽**。探针 principal=`p1`、邻居 principal=NULL、同 agent
/// 同 scope ⇒ 邻居必须回来。NULL 是"未知"，不是"另一个 principal"：中心库 151/216 条 card 行
/// 的 principal 为 NULL，按严格 `IS` 比它们与 65 条 `'p1'` 行互不可见 ⇒ 判同看不到七成邻居。
#[test]
fn a_null_principal_neighbour_is_still_a_neighbour() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let probe = card_owned_as(&root, "探针", "部署在十点", 1_000, &scope(), Some("p1"), "agent-a");
    let _unknown = card_owned_as(&root, "无主的邻居", "部署在十点", 1_001, &scope(),
                                 None, "agent-a");

    let got = rows_in_scope(root.memory(), MemTable::Card, &scope(), &probe.id).unwrap();
    assert_eq!(titles(&got), vec!["无主的邻居".to_string()],
               "邻居的 principal 是 NULL（未知）⇒ 只比 agent，它必须在这个集合里");
}

/// C5c-fix 用例 2：两家 principal 都非空且不等 ⇒ 不是邻居（放宽只放宽 NULL 那一半）。
#[test]
fn a_different_principal_is_never_a_neighbour() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let probe = card_owned_as(&root, "探针", "部署在十点", 1_000, &scope(), Some("p1"), "agent-a");
    let _other = card_owned_as(&root, "别的人", "部署在十点", 1_001, &scope(), Some("p2"),
                               "agent-a");

    let got = rows_in_scope(root.memory(), MemTable::Card, &scope(), &probe.id).unwrap();
    assert!(got.is_empty(), "同 agent 但 principal 不同 ⇒ 不是邻居：{got:?}");
}

/// C5c-fix 用例 3：agent 轴保持**严格相等**（principal 都 NULL 也不许跨 agent 并）。
#[test]
fn a_different_agent_is_never_a_neighbour() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let probe = card_owned_as(&root, "探针", "部署在十点", 1_000, &scope(), None, "agent-a");
    let _other = card_owned_as(&root, "别家的", "部署在十点", 1_001, &scope(), None, "agent-b");

    let got = rows_in_scope(root.memory(), MemTable::Card, &scope(), &probe.id).unwrap();
    assert!(got.is_empty(), "principal 都为 NULL 时只比 agent，而 agent 不同 ⇒ 空：{got:?}");
}

/// C5c-fix 用例 4：探针**自己没有 `resource_owner` 行** ⇒ 空（fail-closed）。
/// 这一条钉的是审核批 R2 的另一半：没有归属证据的行不进任何候选，本片不改这个行为。
/// 夹具直接删掉探针的归属行——生产写入路径每行都写归属，只有旧库/库外改动造得出这种形状。
#[test]
fn a_probe_without_an_owner_row_sees_no_neighbours() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let probe = card_owned_as(&root, "探针", "部署在十点", 1_000, &scope(), Some("p1"), "agent-a");
    let _sibling = card_owned_as(&root, "同人邻居", "部署在十点", 1_001, &scope(),
                                 Some("p1"), "agent-a");
    root.memory().conn().execute(
        "DELETE FROM resource_owner WHERE mem_table='card' AND mem_id=?1",
        rusqlite::params![probe.id]).unwrap();

    let got = rows_in_scope(root.memory(), MemTable::Card, &scope(), &probe.id).unwrap();
    assert!(got.is_empty(), "探针没有归属行 ⇒ 一条邻居都不返回：{got:?}");
}

#[test]
fn rows_in_scope_skips_superseded_and_rejected() {
    // P-31 的三支一次挡两支（被顶掉 / 有未解决的否决表态）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let probe = card(&root, "探针", "基准正文", 1_000, &scope());
    let supersed = card(&root, "被顶掉", "基准正文甲", 1_001, &scope());
    let vetoed = card(&root, "被否决", "基准正文乙", 1_002, &scope());
    let alive = card(&root, "活着", "基准正文丙", 1_003, &scope());
    rc_memory::supersede(root.memory(), &supersed, &alive.id, 2_000).unwrap();
    rc_memory::attest(root.memory(), &vetoed, rc_memory::Stance::Rejected,
                      None, Some("candidate-read-fixture"), 2_000).unwrap();

    let got = rows_in_scope(root.memory(), MemTable::Card, &scope(), &probe.id).unwrap();
    assert_eq!(titles(&got), vec!["活着".to_string()], "{got:?}");

    // 撤销否决 ⇒ 那一行回来。**被顶掉的那一支没有反向接口**（`writers_c` 只有 `supersede`），
    // 所以这里只断"回来的正是撤销的那一支"——它同时证明筛掉的是**账**而不是行本身。
    rc_memory::attest(root.memory(), &vetoed, rc_memory::Stance::Rejected,
                      None, Some("still-there"), 2_500).unwrap();  // 免锚的否决可以再打一条
    assert_eq!(rows_in_scope(root.memory(), MemTable::Card, &scope(), &probe.id).unwrap().len(), 1,
               "未解决的否决还在 ⇒ 它还在集合外，这是对的");
    assert!(titles(&rows_in_scope(root.memory(), MemTable::Card, &scope(), &probe.id).unwrap())
            .contains(&"活着".to_string()));
}

#[test]
fn a_retired_row_leaves_the_candidate_set_and_comes_back_when_resolved() {
    // 第三支：未解决的退场账。这一支能来回，所以它测的是**账**而不是标记位。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let probe = card(&root, "探针", "基准正文", 1_000, &scope());
    let other = card(&root, "邻居", "基准正文甲", 1_001, &scope());
    assert_eq!(titles(&rows_in_scope(root.memory(), MemTable::Card, &scope(), &probe.id).unwrap()),
               vec!["邻居".to_string()]);
    rc_memory::retire(root.memory(), &other, "audit:vote1", 2_000).unwrap();
    assert!(rows_in_scope(root.memory(), MemTable::Card, &scope(), &probe.id).unwrap().is_empty());
    rc_memory::resolve_retirement(root.memory(), &other, 3_000).unwrap();
    assert_eq!(rows_in_scope(root.memory(), MemTable::Card, &scope(), &probe.id).unwrap().len(), 1,
               "撤销退场账后邻居回来：筛掉的是账，不是行");
}

#[test]
fn scope_of_reads_the_two_ownership_columns_and_a_missing_id_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k = card(&root, "甲", "正文", 1_000, &scope());
    assert_eq!(scope_of(root.memory(), &k).unwrap().key, "proj-A");
    let e = scope_of(root.memory(), &MemKey { table: MemTable::Card, id: "b".repeat(64) }).unwrap_err();
    assert!(matches!(e, rc_memory::MemError::NotFound(_)), "{e}");
}

#[test]
fn unreviewed_rows_skips_rows_already_given_a_verdict() {
    // 游标的语义就是"还没被中心判过的行"：判过就永久离开，不需要第二个游标（§2.15）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let a = card(&root, "甲", "同一批正文", 1_000, &scope());
    let _b = card(&root, "乙", "同一批正文二", 1_001, &scope());
    assert_eq!(unreviewed_rows(root.memory(), 10).unwrap().len(), 2);
    rc_memory::set_verdict(root.memory(), &a, rc_memory::Verdict::Low, 2_000).unwrap();
    assert_eq!(titles(&unreviewed_rows(root.memory(), 10).unwrap()), vec!["乙".to_string()]);
}

#[test]
fn unreviewed_rows_skips_all_three() {
    // P-31 的第三支在此刻成立：`rows_in_scope`（任务 7）与 `put_batch` 的写前查账（任务 6）
    // 与这条游标**共用同一个谓词**（守卫 3）。三支一支都不能少，这里一次挡三支。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let alive = card(&root, "活着", "基准正文", 1_000, &scope());
    let gone = card(&root, "已退场", "基准正文甲", 1_001, &scope());
    let sup = card(&root, "被顶掉", "基准正文乙", 1_002, &scope());
    let vetoed = card(&root, "被否决", "基准正文丙", 1_003, &scope());
    rc_memory::retire(root.memory(), &gone, "audit:vote1", 2_000).unwrap();
    rc_memory::supersede(root.memory(), &sup, &alive.id, 2_000).unwrap();
    rc_memory::attest(root.memory(), &vetoed, rc_memory::Stance::Rejected, None,
                      Some("still-dead"), 2_000).unwrap();

    let got = titles(&unreviewed_rows(root.memory(), 10).unwrap());
    assert_eq!(got, vec!["活着".to_string()], "{got:?}：游标少挡了一支 ⇒ 死行会被重新审");
    // 撤销退场账 ⇒ 那一行**回到**游标。这一支能来回，所以它证明筛掉的是账不是行。
    rc_memory::resolve_retirement(root.memory(), &gone, 3_000).unwrap();
    assert_eq!(unreviewed_rows(root.memory(), 10).unwrap().len(), 2);
}

#[test]
fn unreviewed_rows_never_lists_an_entity() {
    // 坑 P-30 的第二半：`select_sql(Entity)` 给 verdict 补的是**字面量** 'unreviewed'，
    // 所以任何"全表找未审"的查询会把每条实体当未审拉进来，而审核一碰到它就整轮 Err。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    rc_memory::put_entity(root.memory(), &rc_memory::EntityContent { kind: "person".into(),
        title: "只有实体".into(), scope: scope(), aliases: vec![] }, &prov(), 1_000).unwrap();
    assert!(unreviewed_rows(root.memory(), 10).unwrap().is_empty(),
            "库里只有 entity 时游标必须是空的，不是报错也不是把实体拉进来");
}

#[test]
fn unreviewed_rows_orders_by_as_of_ms_then_table_then_id_and_limit_is_a_row_count() {
    // 顺序是"哪 4 行被审"的唯一决定因素 ⇒ 它必须与插入顺序无关（两个操作员跑同一份库）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let late = card(&root, "晚", "正文甲", 3_000, &scope());
    let early = card(&root, "早", "正文乙", 1_000, &scope());
    let ev = rc_memory::put_event(root.memory(), &rc_memory::FactContent { kind: "milestone".into(),
        title: "事件早".into(), body: "正文丙".into(), scope: scope(), valid_to_ms: None },
        &prov(), 1_000).unwrap();
    // 同 as_of_ms 的两行（旧 event 现在也落 card）按 id 定序，与插入序无关。
    let (lo, hi) = if early.id < ev.id { (&early, &ev) } else { (&ev, &early) };
    let got = unreviewed_rows(root.memory(), 10).unwrap();
    assert_eq!(got.iter().map(|r| r.key.id.clone()).collect::<Vec<_>>(),
               vec![lo.id.clone(), hi.id.clone(), late.id.clone()],
               "as_of_ms 相同的两行按 (tbl, id) 定序：同一张 card 表，id 是唯一 tie-break");
    assert_eq!(unreviewed_rows(root.memory(), 2).unwrap().len(), 2, "limit 是行数上界");
    assert_eq!(unreviewed_rows(root.memory(), 0).unwrap().len(), 0);
}

// ── 任务 11（P3 实体目录）—— 七条用例 + 五个夹具助手 ────────────────────────────────

/// 目录夹具的出处轮：64 位小写 hex，但**不来自转录库**（本 crate 拿不到那个柄）。
/// `provenance` 没有跨库外键，所以这一串在转录库里查无此轮——那是事实，不是错误：
/// 目录数的是"记忆库里共享这一轮的行"，它压根不看转录库（看了就是第二条缝）。
fn uid(n: u8) -> String { format!("{n:02x}").repeat(32) }

fn prov_of(turns: &[u8]) -> Vec<Provenance> {
    turns.iter().map(|n| Provenance { turn_uid: uid(*n), anchor_role: AnchorRole::Human }).collect()
}

/// 实体与卡片**共用同一个 `scope()`**（纪律 3）——这是把 P-56 的反面武装进每一条目录用例。
fn entity_on(root: &Root, title: &str, aliases: &[&str], turns: &[u8], at: i64) -> MemKey {
    rc_memory::put_entity(root.memory(), &rc_memory::EntityContent {
        kind: "person".into(), title: title.into(), scope: scope(),
        aliases: aliases.iter().map(|s| s.to_string()).collect(),
    }, &prov_of(turns), at).unwrap()
}

fn card_on(root: &Root, title: &str, body: &str, turns: &[u8], at: i64) -> MemKey {
    // 不走既有的 `card()`：它把出处轮写死成 `prov()` 那一串，而目录的 `refs` 连的就是出处轮
    // （P-56），收下 `turns` 又丢掉等于夹具说谎。归属仍然共用 `scope()`（纪律 3）。
    rc_memory::put_card(root.memory(), &rc_memory::FactContent {
        kind: "lesson".into(), title: title.into(), body: body.into(),
        scope: scope(), valid_to_ms: None,
    }, &prov_of(turns), at).unwrap()
}

/// 目录的 (`key`, `refs`, `latest`) 三元组，按 `key` 升序（`directory_sql` 末尾就是 `ORDER BY e.id`）。
fn dir_rows(root: &Root) -> Vec<(String, u64, Option<i64>)> {
    entity_directory(root.memory()).unwrap().into_iter()
        .map(|e| (e.key.clone(), e.refs, e.latest_as_of_ms)).collect()
}

/// 同一条 SQL 的查询计划文本（`directory_sql()` 是公开的，这里 EXPLAIN 的
/// 就是 `entity_directory` 真正执行的那串文本——P-57 的判据成立的前提）。
///
/// **第 0 列（sequential id）不参与比较**：SQLite 在两份计划里给同一条逻辑步骤排的 id
/// 不一致（删索引后插入自动索引会让所有后续 id 整体偏移一行）—— 只看 `parent|notused|text`
/// 三列才能断"内容只差索引那一行"。
fn plan(m: &rc_store::MemoryHandle, sql: &str) -> String {
    let mut st = m.conn().prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let rows = st.query_map([], |r| Ok(format!("|{}|{}",
        r.get::<_, i64>(1)?, r.get::<_, String>(3)?)))
        .unwrap();
    rows.map(|x| x.unwrap()).collect::<Vec<_>>().join("\n")
}

#[test]
fn the_directory_has_one_entry_per_entity_not_per_alias() {
    // P-50 + P-51 的前半：条目数 == **活的 `entity` 行数**，与别名数无关。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let e = entity_on(&root, "老王", &["老王", "某老板", "老王先生"], &[1], 100);
    entity_on(&root, "老李", &["老李"], &[2], 110);
    card_on(&root, "甲", "老王定的口径", &[1], 300);

    let d = entity_directory(root.memory()).unwrap();
    assert_eq!(d.len(), 2, "三个别名不许把一条实体乘成三行：{d:?}");
    let one = d.iter().find(|x| x.key == e.id).expect("老王的条目必须在场");
    assert_eq!(one.aliases, vec!["某老板", "老王", "老王先生"], "别名按 SQL 的 ORDER BY 回来（UTF-8 字节序：王 < 老，短前缀在前）");
    assert_eq!(one.refs, 1, "共享出处轮 1 的那张 card 是唯一邻居");
    assert_eq!(one.latest_as_of_ms, Some(300));
    assert_eq!(one.name, "老王");
    assert_eq!(one.kind, "person");
}

#[test]
fn the_directory_counts_by_shared_turn_not_by_scope() {
    // P-56 的执法点。**两张卡都与实体同归属**（`scope()` 是同一个 Project/proj-A），
    // 但只有出处轮相同的那一张算邻居。按 `scope_key` 连的实现在这里会读出 refs=2 / latest=300。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let e = entity_on(&root, "老王", &["老王"], &[1], 100);
    card_on(&root, "同轮", "共享出处轮的那条", &[1], 200);
    card_on(&root, "同归属不同轮", "另一轮蒸出来的", &[9], 300);

    let one = entity_directory(root.memory()).unwrap().into_iter()
        .find(|x| x.key == e.id).unwrap();
    assert_eq!(one.refs, 1, "连的是出处轮不是归属：按 scope 连会读成 2");
    assert_eq!(one.latest_as_of_ms, Some(200), "同上：按 scope 连会读成 300");
}

#[test]
fn a_row_with_two_shared_turns_counts_once() {
    // P-57 的"DISTINCT"半边：一条 card 同时出自两轮（重蒸、跨批），它与实体共享两轮
    // ⇒ `refs` 是**邻居行数**而不是共享轮数。少了 `count(DISTINCT …)` 这条读 2。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let e = entity_on(&root, "老王", &["老王"], &[1, 2], 100);
    let c = card_on(&root, "甲", "两轮都蒸出它", &[1, 2], 200);
    let one = entity_directory(root.memory()).unwrap().into_iter()
        .find(|x| x.key == e.id).unwrap();
    assert_eq!(one.refs, 1, "一条邻居行不该被数两遍：{one:?}");
    assert_eq!(one.latest_as_of_ms, Some(200));
    assert_eq!(c.table, MemTable::Card, "夹具本身别写错表——这条断句只为让未用的 `c` 有去处");
}

#[test]
fn the_directory_counts_only_alive_rows() {
    // P-58：邻居行三支都算数，而且 `max` 跟着活行走（不是"重算 refs、latest 留在全表"）。
    // 夹具：两张活 card as_of 100 / 200 ⇒ refs=2 / latest=200；顶掉 as_of=200 那张 ⇒ refs=1 / latest=100。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let e = entity_on(&root, "老王", &["老王"], &[1], 100);
    let old = card_on(&root, "旧", "旧口径", &[1], 100);
    let new = card_on(&root, "新", "新口径", &[1], 200);
    let read = |root: &Root| {
        entity_directory(root.memory()).unwrap().into_iter()
            .find(|x| x.key == e.id).unwrap()
    };
    assert_eq!((read(&root).refs, read(&root).latest_as_of_ms), (2, Some(200)));

    // ① 被顶掉（`superseded_by` 那一支）。
    rc_memory::supersede(root.memory(), &old, &new.id, 2_000).unwrap();
    let after = read(&root);
    assert_eq!((after.refs, after.latest_as_of_ms), (1, Some(200)), "顶掉 as_of=100 ⇒ 停在 200");

    // ② 未解决的否决票（第三支）。
    rc_memory::attest(root.memory(), &new, rc_memory::Stance::Rejected,
                      None, Some("directory-fixture"), 2_100).unwrap();
    let after = read(&root);
    assert_eq!((after.refs, after.latest_as_of_ms), (0, None),
               "邻居全死了 ⇒ refs=0 而 latest 必须回 null（`max` 跟着全表走会停在 200）");

    // ③ 撤销**方向不存在**：本阶段 `attestation.resolved_at_ms` 没有写入口（brief 任务 11
    // 文档"否决账的 `attestation.resolved_at_ms` 本阶段没有任何写入口"），所以这条用例只能
    // 断"调用了 `resolve_retirement`、读数没漂"——`resolve_retirement(new)` 在 new 没有退场账时
    // 幂等地报 0 行（`writer.rs:62` 那条放宽），不动 `new` 的否决标注 ⇒ 读数仍为 (0, None)。
    // 这正是 P-58 的第三支：方向的不对称。
    rc_memory::resolve_retirement(root.memory(), &new, 2_200).unwrap();
    assert_eq!((read(&root).refs, read(&root).latest_as_of_ms), (0, None),
               "`resolve_retirement` 不是 `attestation` 的反悔 ⇒ 读数不变");
}

#[test]
fn an_entity_with_no_provenance_is_listed_with_zero_refs() {
    // `remember` 只写转录不写记忆行，而 `put_entity` 必带出处（`ProvenanceMissing`）。
    // 但"实体一条邻居都没有"是正常状态（刚认识一个人），必须是 `refs=0 / latest=null` 而不是缺席。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let e = entity_on(&root, "新来的", &["新来的"], &[7], 100);
    let d = entity_directory(root.memory()).unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!((d[0].key.as_str(), d[0].refs, d[0].latest_as_of_ms), (e.id.as_str(), 0, None));
}

#[test]
fn a_retired_entity_leaves_the_directory_while_rows_entity_stays() {
    // P-66：实体行自己也过 `alive_where(Entity)`。同时钉住那条**必须写死的区分**：
    // `entities_total` 可以小于 `rows.entity`——后者是全表行数口径（阶段 1 就是这个口径，本任务不改）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let e1 = entity_on(&root, "人1", &["人1"], &[1], 100);
    let _e2 = entity_on(&root, "人2", &["人2"], &[2], 110);
    let e3 = entity_on(&root, "人3", &["人3"], &[3], 120);
    let _e4 = entity_on(&root, "人4", &["人4"], &[4], 130);
    let e5 = entity_on(&root, "人5", &["人5"], &[5], 140);
    card_on(&root, "甲", "两条实体的共享邻居", &[1], 200);
    assert_eq!(entity_directory(root.memory()).unwrap().len(), 5);

    rc_memory::retire(root.memory(), &e3, "已确认不是同一个人", 300).unwrap();
    let d = dir_rows(&root);
    assert_eq!(d.len(), 4, "退场的实体必须离开目录（P-66）");
    assert!(!d.iter().any(|r| r.0 == e3.id));
    // 全表行数口径**不动**：`reader::facts` 仍数到 5。两边答的是两个问题。
    let all = rc_memory::facts(root.memory()).unwrap();
    assert_eq!(all.iter().filter(|f| f.key.table == MemTable::Anchor).count(), 5,
               "有人为了'让它们一致'去改其中一边，这条就是拦它的");

    // 账能来回 ⇒ 它测的是账而不是标记位（与上一条同一条理由）。
    rc_memory::resolve_retirement(root.memory(), &e3, 400).unwrap();
    assert_eq!(dir_rows(&root).len(), 5);
    assert!(e1.id != e5.id, "夹具的五个 id 必须互不相同");
}

#[test]
fn the_in_memory_alive_predicate_agrees_with_the_sql_predicate() {
    // 坑 P-67 的执法点，也是守卫 3 的第五次复对：`alive_where`（SQL）与 `RowFacts::alive()`
    // （内存）是同一支三支谓词的**两条实现路**。任何一支漂了——比如有人给 `alive_where`
    // 加第四支而忘了 `alive()`——这一条当场红，而不是等到 agent 面的提示说谎。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let probe = card(&root, "探针", "基准正文", 1_000, &scope());
    let live = card(&root, "活着", "基准正文甲", 1_001, &scope());
    let supersed = card(&root, "被顶掉", "基准正文乙", 1_002, &scope());
    let vetoed = card(&root, "被否决", "基准正文丙", 1_003, &scope());
    let gone = card(&root, "被退场", "基准正文丁", 1_004, &scope());
    rc_memory::supersede(root.memory(), &supersed, &live.id, 2_000).unwrap();
    rc_memory::attest(root.memory(), &vetoed, rc_memory::Stance::Rejected,
                      None, Some("p4-fixture"), 2_000).unwrap();
    rc_memory::retire(root.memory(), &gone, "p4:vote", 2_000).unwrap();

    // SQL 那一路：`rows_in_scope` 把 `alive_where` 挂在 WHERE 里。
    let by_sql = titles(&rows_in_scope(root.memory(), MemTable::Card, &scope(), &probe.id).unwrap());
    // 内存那一路：全量读数 + 三列组合。**不经过任何 SQL 谓词**。
    // D-1：brief 原文只按 `f.key.table == Card && f.alive()` 过滤，但 `rows_in_scope` 同时
    // 还按 `exclude_id` 排除了探针；为让两条路真正可比，`by_facts` 也要把 `probe.id` 排掉
    // ——这条剔除与 alive 谓词无关，是两侧过滤口径对齐。
    let by_facts = titles(
        &rc_memory::reader::facts(root.memory())
            .unwrap()
            .into_iter()
            .filter(|f| f.key.table == MemTable::Card && f.key.id != probe.id && f.alive())
            .collect::<Vec<_>>(),
    );
    assert_eq!(by_sql, by_facts, "两条路的活行集不同 ⇒ 三支谓词漂了一支：{by_sql:?} vs {by_facts:?}");
    // 并且它数出来的**是**那个答案（`titles` 自带 sort，所以逐位可比）。
    assert_eq!(by_sql, vec!["活着".to_string()], "{by_sql:?}");

    // 实体那一支的区分：`alive_where(Entity)` 的 `superseded` 支是 `"1=1"`，而 `select_sql`
    // 给实体行的 `superseded_by` 是字面量 `NULL`——**两种写法必须是同一件事**，否则
    // `alive()` 对实体行会给出与 SQL 相反的答案，而 P4 恰好一条实体行都不要（坑 P-60）。
    let ent = rc_memory::put_entity(root.memory(), &rc_memory::EntityContent {
        kind: "person".into(), title: "活实体".into(), scope: scope(), aliases: vec![],
    }, &prov(), 1_005).unwrap();
    let ent_facts = rc_memory::reader::lookup(root.memory(), &ent).unwrap().unwrap();
    assert!(ent_facts.alive(), "实体行被 `alive()` 判死 ⇒ 那条 `NULL` 与那句 `1=1` 不同口径");
}

#[test]
fn the_directory_plan_uses_the_turn_index() {
    // P-57 的判据落点。三条断句各挡一种红法：
    //   ① EXPLAIN 的必须是 `directory_sql()` 的返回**本身**（同一次调用产生的同一串文本），
    //      否则"证明了索引被用上"证明的是另一条 SQL；
    //   ② 有索引时计划里出现 `provenance_turn`；
    //   ③ DROP 掉索引后计划里不出现它，**且 `pf` 这一行随之变化**——
    //      少了这句，用例证的是"两次 EXPLAIN 不同"，SQLite 换任何执行计划它都可能一直红或一直绿。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    entity_on(&root, "老王", &["老王"], &[1], 100);
    card_on(&root, "甲", "邻居", &[1], 200);

    let sql = directory_sql();
    let with = plan(root.memory(), &sql);
    assert!(with.contains("provenance_turn"), "索引在场而计划没用上它 ⇒ 形状选错了：\n{with}");

    root.memory().conn().execute("DROP INDEX provenance_turn", []).unwrap();
    let without = plan(root.memory(), &sql);
    assert!(!without.contains("provenance_turn"), "删了索引它还出现 ⇒ 这条断句在断字符串而不是断计划");

    // 删索引 ⇒ SQLite 改写 `pf` 那条 lookup。其它行允许因 AUTOMATIC INDEX 的中间步而
    // 整体偏移一两行，但 `pf` 行必须**真的变了**——否则上面两行只是巧合地恒真。
    // （不是要严格 `== 1`：自动索引构造会把整份 plan 的 parent id 整体偏移一到两行，
    // 那是 SQLite 内部细节，不是我们要断的事。）
    let pf_with = with.lines().find(|l| l.contains("provenance_turn"))
        .expect("provenance_turn 行必须在 with 计划里");
    let pf_without = without.lines().find(|l| l.contains("SEARCH pf"))
        .expect("`pf` 行必须在 without 计划里");
    assert_ne!(pf_with, pf_without, "删索引后 `pf` 那行应替换为自动索引：\n{pf_with}\n---\n{pf_without}");
}

// ===== I-4 的审核面：推导写的行不占审核额度 =====

/// 审核额度是有限的：一条 `kind='timeline'` 的推导行（本文件用真写入路径造，不是手搓 SQL）
/// 不该被推到判同/审核面面前。**断言它连一条都不占**，而不是断"排在后面"——
/// 排序在这里是实现细节，可观察的性质是"不在候选集合里"。
///
/// 与上面那条同律：**推导行必须带锚**（`time_anchor_id: None` ⇒ `put_timeline_rows` 不写 provenance
/// ⇒ 零出处的行按判别谓词不算推导行，那这条用例测的就不是谓词而是别的东西了）。
#[test]
fn unreviewed_rows_skips_a_derived_timeline_row() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let extracted = rc_memory::put_event(root.memory(), &rc_memory::FactContent {
        kind: "milestone".into(), title: "抽取来的那一行".into(), body: "抽取正文".into(),
        scope: scope(), valid_to_ms: None,
    }, &prov(), 1_000).unwrap();
    let ctx = BatchCtx { source_agent: "rc-derive".into(), device_id: "unknown".into() };
    put_timeline_rows(root.memory(), &[TimelineContent {
        title: "derive-一行".into(), body: "推导正文".into(), scope: scope(),
        time_anchor_ms: Some(9_000), time_anchor_id: Some(extracted.id.clone()),
        time_confidence: "anchored".into(), basis_ids: vec![],
    }], &ctx, 2_000).unwrap();

    let got = unreviewed_rows(root.memory(), 10).unwrap();
    assert_eq!(titles(&got), vec!["抽取来的那一行".to_string()],
        "推导行不进审核候选——它已经是结论，不该再花一次判定额度");
}

/// **这条用例钉住判别谓词的不可伪造性**：一条由 `put_batch` 写下的抽取行，
/// 而它的 `BatchCtx::source_agent` 字面量就写 `"rc-derive"`——冒充推导作业。
/// 若判别谓词错用 `source_agent`（事实表与 `sessions` 表同名不同物，而 `BatchCtx` 那两列
/// 取自会话自报家门），这一行会被当成推导行筛掉 ⇒ 本条红。
/// 红形：断言拿到空集合而实际有一行。
#[test]
fn unreviewed_rows_keeps_a_session_row_whose_source_agent_impersonates_rc_derive() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let liar = BatchCtx { source_agent: "rc-derive".into(), device_id: "dev-lying".into() };
    put_batch(root.memory(), &[WriteItem {
        table: MemTable::Card, kind: "lesson".into(), title: "自称 rc-derive 的抽取行".into(),
        body: "抽取正文".into(), scope: scope(), valid_to_ms: None, aliases: vec![],
        prov: vec![Provenance { turn_uid: "c".repeat(64), anchor_role: AnchorRole::Human }],
    }], &liar, 1_000).unwrap();

    let got = unreviewed_rows(root.memory(), 10).unwrap();
    assert_eq!(titles(&got), vec!["自称 rc-derive 的抽取行".to_string()],
        "判别列是 ref_kind（写入路径决定）不是 source_agent（客户端自报，可冒充）");
}
