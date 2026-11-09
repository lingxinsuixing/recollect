// crates/rc-distill/tests/ownership.rs
//! S1b-C3：中心抽取写记忆行时，owner/visibility/retention 从**会话**继承（设计 §3.1/§3.2、§8-C3）。
//!
//! 这一条链是：`sessions` 的 7 个归属列 → `select_batch` 的 `BatchPlan.owner` →
//! `put_batch_with_owner` → `resource_owner`（与行、出处、别名**同一个事务**）。
//! 这里只断"继承"这一层：owner 是会话的、retention 是会话的、visibility 是
//! `min(会话, ceiling(行 scope))`、created_by 是会话的 `source_agent`、scope 是**行自己的**。
//! 读闸与回填（C4）不在这一片里。
use rc_distill::extract::{run_extract, select_batch, MAX_TURNS_PER_BATCH};
use rc_llm::mock::MockBackend;
use rc_memory::owner::Visibility;
use rc_store::Root;

mod common;
use common::{payload, seed};

/// 一条事实行的四格读数（两张可指认表逐行读；owner 表的 scope/时刻要对拍它）。
struct FactRow {
    mem_table: String,
    id: String,
    scope_kind: String,
    scope_key: String,
    created_at_ms: i64,
}

fn facts(root: &Root) -> Vec<FactRow> {
    let conn = root.memory().conn();
    let mut st = conn
        .prepare(
            "SELECT 'card' AS t, id, scope_kind, scope_key, created_at_ms FROM card
             UNION ALL
             SELECT 'anchor', id, scope_kind, scope_key, created_at_ms FROM anchor
             ORDER BY t, id",
        )
        .unwrap();
    st.query_map([], |r| {
        Ok(FactRow {
            mem_table: r.get(0)?,
            id: r.get(1)?,
            scope_kind: r.get(2)?,
            scope_key: r.get(3)?,
            created_at_ms: r.get(4)?,
        })
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

/// 一行归属（只读断言需要的 8 格）。
struct OwnerRow {
    owner_principal_id: Option<String>,
    owner_agent_id: String,
    created_by_agent: String,
    visibility: String,
    retention: String,
    scope_kind: String,
    scope_key: String,
    created_at_ms: i64,
}

fn owner_of(root: &Root, fact: &FactRow) -> OwnerRow {
    root.memory()
        .conn()
        .query_row(
            "SELECT owner_principal_id, owner_agent_id, created_by_agent, visibility, retention,
                    scope_kind, scope_key, created_at_ms
             FROM resource_owner WHERE mem_table=?1 AND mem_id=?2",
            rusqlite::params![fact.mem_table, fact.id],
            |r| {
                Ok(OwnerRow {
                    owner_principal_id: r.get(0)?,
                    owner_agent_id: r.get(1)?,
                    created_by_agent: r.get(2)?,
                    visibility: r.get(3)?,
                    retention: r.get(4)?,
                    scope_kind: r.get(5)?,
                    scope_key: r.get(6)?,
                    created_at_ms: r.get(7)?,
                })
            },
        )
        .unwrap_or_else(|e| panic!("{}:{} 没有归属行：{e}", fact.mem_table, fact.id))
}

fn orphans(root: &Root) -> i64 {
    root.memory()
        .conn()
        .query_row(
            "SELECT count(*) FROM resource_owner o
              WHERE NOT EXISTS (SELECT 1 FROM card c   WHERE o.mem_table='card'   AND c.id=o.mem_id)
                AND NOT EXISTS (SELECT 1 FROM anchor a WHERE o.mem_table='anchor' AND a.id=o.mem_id)",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

fn owner_count(root: &Root) -> i64 {
    root.memory()
        .conn()
        .query_row("SELECT count(*) FROM resource_owner", [], |r| r.get(0))
        .unwrap()
}

/// 会话设了非默认归属 ⇒ 每条写成的行都继承它：
/// owner=(p1,alice)、retention=keep、visibility=min(project, ceiling(行 scope))、
/// created_by=会话 source_agent、scope/created_at 取行自己的。
#[test]
fn extracted_rows_inherit_the_session_owner_visibility_and_retention() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, 3, 1_000);
    // 唯一 upsert 点在 `rc_ledger::insert_batch`；测试直接改这一行（夹具，不碰生产写路径）。
    root.transcript_mut()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id='p1', owner_agent_id='alice',
                    visibility='project', retention='keep',
                    scope_kind='project', scope_key='proj-x', ownership_src='operator'
             WHERE session_key='sess-batch-A' AND device_id='dev-batch-A'",
            [],
        )
        .unwrap();

    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH)
        .unwrap()
        .unwrap();
    // `BatchPlan.owner` 是 select_batch 从 sessions 读出来的那四格。
    assert_eq!(plan.owner.owner_principal_id.as_deref(), Some("p1"));
    assert_eq!(plan.owner.owner_agent_id, "alice");
    assert_eq!(plan.owner.visibility, Visibility::Project);
    assert_eq!(plan.owner.retention, "keep");
    assert_eq!(plan.ctx.source_agent, "agent-batch-A", "ctx 与 owner 是两个来源，别混");

    let backend = MockBackend::scripted(vec![Ok(payload(&uids))]);
    let out = run_extract(root.memory(), root.ledger(), &plan, &backend, 3_000).unwrap();
    assert_eq!(out.rows_written, 4, "实体 + 卡片 + 事件 + 断言");
    assert_eq!(out.suppressed, 0);

    let facts = facts(&root);
    assert_eq!(facts.len(), 4, "每条产出都真有行（分母不为 0，下面的逐行断言才有牙）");
    assert_eq!(owner_count(&root), 4, "每一条行都要有一行归属");
    assert_eq!(orphans(&root), 0, "归属行不许指向不存在的记忆行");
    for fact in &facts {
        let o = owner_of(&root, fact);
        assert_eq!(o.owner_principal_id.as_deref(), Some("p1"), "{}:{}", fact.mem_table, fact.id);
        assert_eq!(o.owner_agent_id, "alice");
        assert_eq!(o.retention, "keep", "retention 从会话继承，不是写入点的 default");
        assert_eq!(o.created_by_agent, "agent-batch-A", "产出者是会话的 source_agent");
        // 可见性上界：min(会话 project, ceiling(行 scope))。
        // card 是 project ⇒ project；anchor（实体）ceiling 是 person ⇒ person。
        let want = if fact.scope_kind == "project" { "project" } else { "person" };
        assert_eq!(
            o.visibility, want,
            "{}:{} 的可见性不是 min(会话, ceiling(行 scope))",
            fact.mem_table, fact.id
        );
        assert_eq!(o.scope_kind, fact.scope_kind, "scope 是行自己的，不是会话的 proj-x");
        assert_eq!(o.scope_key, fact.scope_key);
        assert_eq!(o.created_at_ms, fact.created_at_ms, "归属与行同钟（源轮时刻）");
        // 两份夹具的代表值：card 的 scope_key 是会话键（project_hint 为空），
        // anchor 的 scope_key 是归一后的实体标题。
        match fact.mem_table.as_str() {
            "card" => {
                assert_eq!(o.scope_kind, "project");
                assert_eq!(o.scope_key, "sess-batch-A");
            }
            _ => {
                assert_eq!(o.scope_kind, "entity");
                assert_eq!(o.scope_key, "项目进度");
            }
        }
    }
}

/// 旧会话（7 个归属列全 NULL，C1 之前导入的行）⇒ `COALESCE` 的自动化默认：
/// owner=`source_agent`、private_agent、default。这是“未知 ⇒ 最保守”的唯一读法。
#[test]
fn a_legacy_session_with_null_ownership_falls_back_to_source_agent_private_default() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, 3, 1_000);
    root.transcript_mut()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id=NULL, owner_agent_id=NULL, scope_kind=NULL,
                    scope_key=NULL, visibility=NULL, retention=NULL, ownership_src=NULL
             WHERE session_key='sess-batch-A' AND device_id='dev-batch-A'",
            [],
        )
        .unwrap();

    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH)
        .unwrap()
        .unwrap();
    assert_eq!(plan.owner.owner_principal_id, None);
    assert_eq!(plan.owner.owner_agent_id, "agent-batch-A", "NULL ⇒ COALESCE 到 source_agent");
    assert_eq!(plan.owner.visibility, Visibility::PrivateAgent);
    assert_eq!(plan.owner.retention, "default");

    let backend = MockBackend::scripted(vec![Ok(payload(&uids))]);
    run_extract(root.memory(), root.ledger(), &plan, &backend, 3_000).unwrap();

    let facts = facts(&root);
    assert_eq!(facts.len(), 4);
    assert_eq!(owner_count(&root), 4);
    for fact in &facts {
        let o = owner_of(&root, fact);
        assert_eq!(o.owner_principal_id, None);
        assert_eq!(o.owner_agent_id, "agent-batch-A");
        assert_eq!(o.created_by_agent, "agent-batch-A");
        assert_eq!(o.visibility, "private_agent");
        assert_eq!(o.retention, "default");
        assert_eq!(o.scope_kind, fact.scope_kind);
        assert_eq!(o.scope_key, fact.scope_key);
    }
}
