// crates/rc-memory/tests/owner_writes.rs
//! S1b-C3：记忆行的**写入面**同时写 `resource_owner`（设计 ownership-persistence §3.2）。
//!
//! 这一层钉五件事：
//! ① 旧四把单条写（`put_card`）与 `put_batch` 不传 owner 时，归属取合成默认
//!    （owner=署名 agent、private_agent、default）——A1 写入口没动，这里是它的代理；
//! ② `put_batch` 的 owner 取 `BatchCtx.source_agent`，`created_by_agent` 同值，
//!    `source_agent`/`device_id` 两格是 ctx 的副本，`created_at_ms` 是 `row_ms`；
//! ③ 行 scope（person|project|entity）是 `resource_owner.scope_*` 的来源，
//!    visibility 再过一次 `effective`（上界）；
//! ④ 同一 id 重写 ⇒ 归属行不重复、值不变（`INSERT OR IGNORE` 的先写者赢）；
//! ⑤ 命中抑制账（dead）的行**没有行**，因此也没有归属行。
use rc_memory::keys::{Scope, ScopeKind};
use rc_memory::writers_x::{
    put_batch, put_batch_at, put_card, BatchCtx, FactContent, WriteItem,
};
use rc_memory::{AnchorRole, MemKey, MemTable, Provenance};
use rc_store::{MemoryHandle, Root};

/// `resource_owner` 的一行（12 列全读回来：断言逐列，不给"只对一半"留缝）。
#[derive(Debug)]
struct OwnerRec {
    mem_table: String,
    mem_id: String,
    owner_principal_id: Option<String>,
    owner_agent_id: String,
    created_by_agent: String,
    visibility: String,
    retention: String,
    scope_kind: String,
    scope_key: String,
    source_agent: Option<String>,
    device_id: Option<String>,
    created_at_ms: i64,
}

fn ctx() -> BatchCtx {
    // 与 `BatchCtx::synthetic()` 不同的一对值（坑 P-29）：归属若偷偷继续用合成常量，下面会红。
    BatchCtx { source_agent: "agent-batch-A".into(), device_id: "dev-batch-A".into() }
}

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-A".into() }
}

fn item(table: MemTable, title: &str, uid: &str) -> WriteItem {
    WriteItem {
        // anchor.kind 的 CHECK 没有 "lesson"：锚行用域内的 "concept"。
        table,
        kind: (if table == MemTable::Anchor { "concept" } else { "lesson" }).into(),
        title: title.into(),
        body: "合成正文".into(),
        scope: scope(),
        valid_to_ms: None,
        aliases: vec![],
        prov: vec![Provenance { turn_uid: uid.into(), anchor_role: AnchorRole::Human }],
    }
}

fn owner_of(m: &MemoryHandle, table: &str, id: &str) -> OwnerRec {
    m.conn()
        .query_row(
            "SELECT mem_table, mem_id, owner_principal_id, owner_agent_id, created_by_agent,
                    visibility, retention, scope_kind, scope_key, source_agent, device_id,
                    created_at_ms
             FROM resource_owner WHERE mem_table=?1 AND mem_id=?2",
            rusqlite::params![table, id],
            |r| {
                Ok(OwnerRec {
                    mem_table: r.get(0)?,
                    mem_id: r.get(1)?,
                    owner_principal_id: r.get(2)?,
                    owner_agent_id: r.get(3)?,
                    created_by_agent: r.get(4)?,
                    visibility: r.get(5)?,
                    retention: r.get(6)?,
                    scope_kind: r.get(7)?,
                    scope_key: r.get(8)?,
                    source_agent: r.get(9)?,
                    device_id: r.get(10)?,
                    created_at_ms: r.get(11)?,
                })
            },
        )
        .unwrap()
}

fn owner_count(m: &MemoryHandle) -> i64 {
    m.conn()
        .query_row("SELECT count(*) FROM resource_owner", [], |r| r.get(0))
        .unwrap()
}

/// `put_batch` 不传 owner：每一行（card 与 anchor）都写归属，取值是合成默认
/// （owner=ctx.source_agent、private_agent、default），scope 是**行自己的** scope。
#[test]
fn put_batch_without_owner_writes_a_private_owner_row_for_every_written_row() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let items = vec![
        item(MemTable::Card, "先看进度", &"a".repeat(64)),
        item(MemTable::Anchor, "合成锚", &"b".repeat(64)),
    ];
    let keys = put_batch(root.memory(), &items, &ctx(), 1_000).unwrap();
    assert_eq!(keys.len(), 2);
    let card = keys[0].as_ref().expect("第一条不被抑制");
    let anchor = keys[1].as_ref().expect("第二条不被抑制");
    assert_eq!(owner_count(root.memory()), 2, "每一条写成的行都要有且只有一行归属");

    for key in [card, anchor] {
        let o = owner_of(root.memory(), key.table.prefix(), &key.id);
        assert_eq!(o.mem_table, key.table.prefix());
        assert_eq!(o.mem_id, key.id);
        assert_eq!(o.owner_principal_id, None, "合成默认没有登记自然人");
        assert_eq!(o.owner_agent_id, "agent-batch-A", "owner 取批次上下文的署名 agent");
        assert_eq!(o.created_by_agent, "agent-batch-A", "产出者同 ctx");
        assert_eq!(o.visibility, "private_agent", "默认可见性是最保守的一档");
        assert_eq!(o.retention, "default");
        assert_eq!(o.scope_kind, "project", "归属的 scope 是行自己的 scope");
        assert_eq!(o.scope_key, "proj-A");
        assert_eq!(o.source_agent.as_deref(), Some("agent-batch-A"), "行 producer 副本取 ctx");
        assert_eq!(o.device_id.as_deref(), Some("dev-batch-A"));
        assert_eq!(o.created_at_ms, 1_000, "created_at_ms 与行同钟（row_ms）");
    }

    // 与事实行逐列对拍：scope 两列必须是**行**的那两列，不是另造一份。
    let row: (String, String, String, String, i64) = root
        .memory()
        .conn()
        .query_row(
            "SELECT scope_kind, scope_key, source_agent, device_id, created_at_ms FROM card WHERE id=?1",
            [&card.id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    let o = owner_of(root.memory(), "card", &card.id);
    assert_eq!(
        (o.scope_kind, o.scope_key, o.source_agent.unwrap(), o.device_id.unwrap(), o.created_at_ms),
        row,
        "resource_owner.scope_* 与 producer 两列必须与事实行逐列相等"
    );
}

/// 旧四把单条写（`put_card`）走 `OwnerCtx::synthetic`：owner=署名 agent（合成值）、
/// private_agent、default。这是"没传 owner 的入口不写空归属"的那一格。
#[test]
fn put_card_writes_the_synthetic_owner() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let c = FactContent {
        kind: "lesson".into(),
        title: "单条卡".into(),
        body: "合成正文".into(),
        scope: Scope { kind: ScopeKind::Person, key: "person-a".into() },
        valid_to_ms: None,
    };
    let key = put_card(
        root.memory(),
        &c,
        &[Provenance { turn_uid: "c".repeat(64), anchor_role: AnchorRole::Human }],
        2_000,
    )
    .unwrap();
    let o = owner_of(root.memory(), "card", &key.id);
    assert_eq!(o.owner_principal_id, None);
    assert_eq!(o.owner_agent_id, "synthetic-agent");
    assert_eq!(o.created_by_agent, "synthetic-agent");
    assert_eq!(o.visibility, "private_agent");
    assert_eq!(o.retention, "default");
    assert_eq!(o.scope_kind, "person", "行 scope 是 person");
    assert_eq!(o.scope_key, "person-a");
    assert_eq!(o.source_agent.as_deref(), Some("synthetic-agent"));
    assert_eq!(o.device_id.as_deref(), Some("dev-synthetic-1"), "合成 ctx 的 device");
    assert_eq!(o.created_at_ms, 2_000);
}

/// 同一 id 的第二次写：归属行 `INSERT OR IGNORE` 不覆盖（先写者赢）。
/// 这一次连 `row_ms` 都换了 ⇒ 值一变就是覆盖发生了。
#[test]
fn rewriting_the_same_row_keeps_the_first_owner_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let items = vec![item(MemTable::Card, "先看进度", &"a".repeat(64))];
    let first = put_batch_at(root.memory(), &items, &ctx(), 1_000, &[Some(900)]).unwrap();
    let id = first[0].as_ref().unwrap().id.clone();
    let before = owner_of(root.memory(), "card", &id);
    assert_eq!(before.created_at_ms, 900, "row_ms 是 item_times 给的源轮时刻");

    let second = put_batch_at(root.memory(), &items, &ctx(), 5_000, &[Some(7_000)]).unwrap();
    assert_eq!(second[0].as_ref().unwrap().id, id, "内容寻址：同一个 id");
    let after = owner_of(root.memory(), "card", &id);
    assert_eq!(owner_count(root.memory()), 1, "第二次写不许长出第二行归属");
    assert_eq!(after.created_at_ms, before.created_at_ms, "归属的 created_at_ms 被覆盖了");
    assert_eq!(after.owner_agent_id, before.owner_agent_id);
    assert_eq!(after.visibility, before.visibility);
}

/// 命中抑制账的行没有行（`put_batch` 在写行之前就 `continue`），因此也没有归属行：
/// 归属与"行存在"是同一个事务里的同一件事，不许出现"只有归属、没有内容"的孤儿。
#[test]
fn a_suppressed_row_gets_no_owner_row() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let ghost = MemKey {
        table: MemTable::Card,
        id: rc_memory::compute_mem_id(MemTable::Card, "lesson", "幽灵退场", "合成正文", &scope()),
    };
    rc_memory::retire(root.memory(), &ghost, "audit:ghost", 2_000).unwrap();
    let got = put_batch(
        root.memory(),
        &[item(MemTable::Card, "幽灵退场", &"d".repeat(64))],
        &ctx(),
        3_000,
    )
    .unwrap();
    assert!(got[0].is_none(), "前提：这张幽灵账此刻确实在抑制");
    assert_eq!(owner_count(root.memory()), 0, "被抑制的行本来就没行，不许有归属行");
}
