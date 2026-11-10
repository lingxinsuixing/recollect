// crates/rc-memory/tests/ownership_tables.rs
//! S1b-C2a：资源归属侧表 `resource_owner` / `resource_grant` 的形状与约束。
//!
//! 这两张表是**资源生命周期数据**（设计 ownership-persistence §2），不是记忆内容：
//! 列序、PK、索引与 CHECK 都是判定面的一部分——`visibility`/`retention` 的封闭值域
//! 挡住"含糊的可见性"，`scope_kind` 用的是记忆行的 `ScopeKind`（person|project|entity），
//! **不是**契约的五值（陷阱：写 `agent`/`org` 就是把两套词表搅在一起）。
use rc_memory::schema::{Actor, OWNERSHIP};
use rc_store::Root;
use rusqlite::{params, Connection};

fn open() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    (dir, root)
}

fn exists(conn: &Connection, kind: &str, name: &str) -> bool {
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type=?1 AND name=?2",
            params![kind, name],
            |r| r.get(0),
        )
        .unwrap();
    n == 1
}

/// `resource_owner` 的一行：合法形状（`retention` 用 `days:7`，`scope_kind` 用行 scope）。
fn insert_owner(
    conn: &Connection,
    mem_table: &str,
    mem_id: &str,
    visibility: &str,
    retention: &str,
    scope_kind: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                    created_by_agent, visibility, retention, scope_kind,
                                    scope_key, source_agent, device_id, created_at_ms)
         VALUES(?1, ?2, 'p-1', 'agent-a', 'agent-a', ?3, ?4, ?5, 'proj-x',
                'agent-a', 'dev-a', 1000)",
        params![mem_table, mem_id, visibility, retention, scope_kind],
    )
}

/// `resource_grant` 的一行：指向一个 agent 的 recall 授权。
fn insert_grant(
    conn: &Connection,
    id: &str,
    from_agent: &str,
    to_agent: Option<&str>,
    to_principal: Option<&str>,
    expires_at_ms: Option<i64>,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO resource_grant(id, from_principal_id, from_agent_id, to_agent_id,
                                    to_principal_id, capability, data_class, scope_kind,
                                    scope_key, purpose, granted_at_ms, expires_at_ms, revoked_at_ms)
         VALUES(?1, 'p-1', ?2, ?3, ?4, 'recall', 'memory', 'project', 'proj-x',
                'share the project card', 1000, ?5, NULL)",
        params![id, from_agent, to_agent, to_principal, expires_at_ms],
    )
}

#[test]
fn the_two_side_tables_exist_with_their_keys_and_indexes() {
    let (_dir, root) = open();
    let conn = root.memory().conn();
    for table in ["resource_owner", "resource_grant"] {
        assert!(exists(conn, "table", table), "memory.db 里没有 {table}");
    }
    for index in [
        "resource_owner_agent",
        "resource_owner_scope",
        "resource_grant_to_agent",
        "resource_grant_to_principal",
    ] {
        assert!(exists(conn, "index", index), "侧表索引 {index} 不在");
    }
    // `resource_owner` 的主键是 (mem_table, mem_id)：归属行挂在"哪张表的哪一行"上。
    let pk: Vec<String> = {
        let mut st = conn
            .prepare("SELECT name FROM pragma_table_info('resource_owner') WHERE pk > 0 ORDER BY pk")
            .unwrap();
        st.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    };
    assert_eq!(pk, vec!["mem_table", "mem_id"], "resource_owner 的 PK 不是 (mem_table, mem_id)");
    // `resource_grant` 的主键是 id（内容寻址的授权串）。
    let grant_pk: Vec<String> = {
        let mut st = conn
            .prepare("SELECT name FROM pragma_table_info('resource_grant') WHERE pk > 0 ORDER BY pk")
            .unwrap();
        st.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    };
    assert_eq!(grant_pk, vec!["id"], "resource_grant 的 PK 不是 id");
}

#[test]
fn resource_owner_checks_reject_values_outside_the_contract() {
    let (_dir, root) = open();
    let conn = root.memory().conn();

    // 合法行先钉住：下面的拒收不是因为整张表写不进去。
    assert_eq!(insert_owner(conn, "card", "c-ok", "private_agent", "days:7", "project"), Ok(1));
    assert_eq!(insert_owner(conn, "card", "c-keep", "person", "keep", "person"), Ok(1));
    assert_eq!(insert_owner(conn, "card", "c-entity", "public", "default", "entity"), Ok(1));

    assert!(
        insert_owner(conn, "card", "c-vis", "visible_to_friends", "default", "project").is_err(),
        "visibility 的封闭值域没有生效"
    );
    assert!(
        insert_owner(conn, "card", "c-ret2", "private_agent", "sometimes", "project").is_err(),
        "retention='sometimes' 被收下了"
    );
    // 设计 §9-9 的已知宽松：`retention LIKE 'days:%'` 只兜底形状，"days 后面必须是数字"
    // 由 `rc_contract::ingest_v3::valid_retention` 在写入面执法（C3 才接上写入方）。
    // 这里**钉住现状**而不是假装它被拒：CHECK 收下了 `days:x`，真值校验不在这一层。
    assert_eq!(
        insert_owner(conn, "card", "c-ret1", "private_agent", "days:x", "project"),
        Ok(1),
        "CHECK 只按 days:% 的形状兜底（§9-9）；值域真值在 valid_retention，若哪天收紧了这条要跟着改"
    );
    // 契约五值（`ingest_v3` 的 ScopeKind）不是记忆行的 ScopeKind：
    // 行 scope 只有 person|project|entity，写成 `agent`/`org` 是把两套词表搅在一起。
    assert!(
        insert_owner(conn, "card", "c-sc1", "private_agent", "default", "agent").is_err(),
        "scope_kind='agent' 被收下了（那是契约五值，不是行 scope）"
    );
    assert!(
        insert_owner(conn, "card", "c-sc2", "private_agent", "default", "org").is_err(),
        "scope_kind='org' 被收下了（那是契约五值，不是行 scope）"
    );
    // 被拒的行一行都不许留下（合法：c-ok/c-keep/c-entity/c-ret1 共 4 行）。
    let n: i64 = conn
        .query_row("SELECT count(*) FROM resource_owner", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 4, "被 CHECK 拒掉的行仍然落库了");
}

#[test]
fn resource_owner_primary_key_rejects_a_second_row_for_the_same_memory() {
    let (_dir, root) = open();
    let conn = root.memory().conn();
    assert_eq!(insert_owner(conn, "card", "c-1", "private_agent", "default", "project"), Ok(1));
    assert!(
        insert_owner(conn, "card", "c-1", "public", "keep", "entity").is_err(),
        "同一 (mem_table, mem_id) 的第二次 INSERT 没有被 PK 拒掉"
    );
    // 同一个 mem_id 挂在另一张表上不算冲突（键是二元组）。
    assert_eq!(insert_owner(conn, "fact", "c-1", "private_agent", "default", "project"), Ok(1));
    let n: i64 = conn
        .query_row("SELECT count(*) FROM resource_owner", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2);
}

#[test]
fn resource_grant_checks_reject_inconsistent_grants() {
    let (_dir, root) = open();
    let conn = root.memory().conn();

    // 合法：指向另一个 agent。
    assert_eq!(insert_grant(conn, "g-ok", "agent-a", Some("agent-b"), None, Some(2000)), Ok(1));
    // 合法：指向一个自然人（to_agent 为 NULL）。
    assert_eq!(insert_grant(conn, "g-person", "agent-a", None, Some("p-2"), None), Ok(1));

    // to_agent / to_principal 恰好二选一。
    assert!(
        insert_grant(conn, "g-both", "agent-a", Some("agent-b"), Some("p-2"), None).is_err(),
        "to_agent 与 to_principal 同时非空被收下了"
    );
    assert!(
        insert_grant(conn, "g-none", "agent-a", None, None, None).is_err(),
        "to_agent 与 to_principal 同时为空被收下了"
    );
    // 授权不能授给自己。
    assert!(
        insert_grant(conn, "g-self", "agent-a", Some("agent-a"), None, None).is_err(),
        "to_agent == from_agent 被收下了"
    );
    // 到期必须晚于签发。
    assert!(
        insert_grant(conn, "g-exp-past", "agent-a", Some("agent-b"), None, Some(999)).is_err(),
        "expires_at_ms < granted_at_ms 被收下了"
    );
    assert!(
        insert_grant(conn, "g-exp-same", "agent-a", Some("agent-b"), None, Some(1000)).is_err(),
        "expires_at_ms == granted_at_ms 被收下了"
    );
    let ids: Vec<String> = {
        let mut st = conn.prepare("SELECT id FROM resource_grant ORDER BY id").unwrap();
        st.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    };
    assert_eq!(ids, vec!["g-ok", "g-person"], "被 CHECK 拒掉的授权仍然落库了");
}

#[test]
fn every_column_of_the_side_tables_is_written_by_the_program() {
    // 设计 §8-C2 第 1 条：两张侧表的每一列都是 `Actor::Program` ⇒ 抽取器与 agent
    // 在结构上写不出归属/授权。ownership.rs 只管"登记存在且恰好一次"，这一条管"写者是谁"。
    for table in ["resource_owner", "resource_grant"] {
        let actors: Vec<Actor> = OWNERSHIP
            .iter()
            .filter(|o| o.table == table)
            .map(|o| o.actor)
            .collect();
        assert!(!actors.is_empty(), "{table} 一列都没登记");
        assert!(
            actors.iter().all(|a| *a == Actor::Program),
            "{table} 有非 Program 的登记：{actors:?}"
        );
    }
}
