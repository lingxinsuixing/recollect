use rc_memory::keys::{AnchorRole, Scope, ScopeKind};
use rc_memory::schema::MEMORY_TABLES;
use rc_memory::snapshot::{build, digest, prune};
use rc_memory::writers_a::attest;
use rc_memory::writers_c::{retire, supersede};
use rc_memory::writers_x::{put_batch, BatchCtx, WriteItem};
use rc_memory::{MemKey, MemTable, Provenance, Stance};
use rc_store::{AgentRoot, Root};

fn ctx() -> BatchCtx {
    BatchCtx { source_agent: "agent-snap".into(), device_id: "dev-snap".into() }
}

fn item(table: MemTable, title: &str, seed: char) -> WriteItem {
    WriteItem {
        table,
        // anchor.kind 的 CHECK 没有 "lesson"：锚行用域内的 "concept"。
        kind: (if table == MemTable::Anchor { "concept" } else { "lesson" }).into(),
        title: title.into(),
        body: "快照过滤夹具正文".into(),
        scope: Scope { kind: ScopeKind::Project, key: "p-snap".into() },
        valid_to_ms: None,
        aliases: vec![],
        prov: vec![Provenance {
            turn_uid: seed.to_string().repeat(64),
            anchor_role: AnchorRole::Human,
        }],
    }
}

fn key_of(keys: &[Option<MemKey>], i: usize) -> MemKey {
    keys[i].clone().expect("fixture row must not be suppressed")
}

fn ids(m: &rc_store::MemoryHandle, table: &str) -> Vec<String> {
    let mut st = m
        .conn()
        .prepare(&format!("SELECT id FROM {table} ORDER BY id"))
        .unwrap();
    st.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn count(m: &rc_store::MemoryHandle, table: &str) -> i64 {
    m.conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// 六行事实（canonical 两表）：两个锚（一活一退场）、四张卡（一活、一被顶替、一被否决、一退场）。
/// 每条都带 turn 出处。
fn seed_facts(m: &rc_store::MemoryHandle, keys: &mut Vec<Option<MemKey>>) {
    let items = vec![
        item(MemTable::Anchor, "锚甲", 'a'),
        item(MemTable::Anchor, "锚乙", 'b'),
        item(MemTable::Card, "活卡", 'c'),
        item(MemTable::Card, "被顶替的卡", 'd'),
        item(MemTable::Card, "被否决的卡", 'e'),
        item(MemTable::Card, "退场卡", 'f'),
    ];
    *keys = put_batch(m, &items, &ctx(), 1_000).unwrap();
    assert_eq!(keys.len(), 6);
    supersede(m, &key_of(keys, 3), &key_of(keys, 2).id, 1_100).unwrap();
    attest(m, &key_of(keys, 4), Stance::Rejected, None, Some("fixture"), 1_200).unwrap();
    retire(m, &key_of(keys, 1), "fixture", 1_300).unwrap();
    retire(m, &key_of(keys, 5), "fixture", 1_400).unwrap();
}

#[test]
fn dead_rows_are_dropped_and_live_rows_survive() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let mut keys = Vec::new();
    seed_facts(root.memory(), &mut keys);

    let report = prune(root.memory().conn()).unwrap();
    assert_eq!(report.dead_rows, 4, "退场 2 + 被顶替 1 + 被否决 1：{report:?}");
    assert_eq!(ids(root.memory(), "anchor"), vec![key_of(&keys, 0).id]);
    assert_eq!(ids(root.memory(), "card"), vec![key_of(&keys, 2).id]);
    // 判定族不删：两条退场账 + 一条否决表态都还在。
    assert_eq!(count(root.memory(), "retirement"), 2);
    assert_eq!(count(root.memory(), "attestation"), 1);
}

#[test]
fn orphan_edges_provenance_mentions_and_attention_are_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let mut keys = Vec::new();
    seed_facts(root.memory(), &mut keys);
    let m = root.memory();
    let e_live = key_of(&keys, 0);
    let e_dead = key_of(&keys, 1);
    let c_live = key_of(&keys, 2);
    let c_super = key_of(&keys, 3);
    let c_dead = key_of(&keys, 4);
    let c_retired = key_of(&keys, 5);

    // 另外造一个活卡，给"保留下来的边"当另一端。
    let extra = put_batch(
        m,
        &[item(MemTable::Card, "另一个活卡", 'i')],
        &ctx(),
        1_500,
    )
    .unwrap();
    let v_other = key_of(&extra, 0);

    m.conn()
        .execute_batch(&format!(
            "INSERT INTO fact_link(id,from_id,to_id,relation,basis,evidence_json,decided_by,
                scope_kind,scope_key,world_id,source_agent,device_id,created_at_ms,norm_version) VALUES
               ('r-keep','anchor:{e0}','card:{c0}','part_of','x','[]',NULL,'person','p',NULL,'a','d',1,1),
               ('r-dead-endpoint','anchor:{e1}','card:{c0}','part_of','x','[]',NULL,'person','p',NULL,'a','d',1,1),
               ('r-retired','card:{c0}','card:{cs}','part_of','x','[]',NULL,'person','p',NULL,'a','d',1,1),
               ('r-resolved','card:{c0}','card:{vo}','part_of','x','[]',NULL,'person','p',NULL,'a','d',1,1);
             INSERT INTO retirement(mem_table,mem_id,reason,created_at_ms,resolved_at_ms) VALUES
               ('fact_link','r-retired','x',1,NULL),
               ('fact_link','r-resolved','x',1,2);
             INSERT INTO provenance(mem_table,mem_id,turn_uid,anchor_role,ref_kind) VALUES
               ('fact_link','r-keep','{t0}','human','turn'),
               ('fact_link','r-dead-endpoint','{t0}','human','turn'),
               ('fact_link','r-resolved','{t0}','human','turn'),
               ('nope','x','{t0}','human','turn');
             INSERT INTO memory_mention(mem_table,mem_id,anchor_id,source_agent,device_id,created_at_ms,norm_version) VALUES
               ('card','{c0}','{e0}','a','d',1,1),
               ('card','{cd}','{e0}','a','d',1,1),
               ('card','{cr}','{e0}','a','d',1,1),
               ('nope','x','{e0}','a','d',1,1);
             INSERT INTO attention(kind,mem_table,mem_id,created_at_ms,resolved_at_ms) VALUES
               ('duplicate_title','card','{c0}',1,NULL),
               ('duplicate_title','card','{c0}',1,2),
               ('duplicate_title','card','{cs}',1,NULL);",
            e0 = e_live.id, e1 = e_dead.id, c0 = c_live.id, cs = c_super.id,
            cd = c_dead.id, cr = c_retired.id, vo = v_other.id,
            t0 = "t".repeat(64),
        ))
        .unwrap();

    let report = prune(m.conn()).unwrap();
    // r-dead-endpoint（端点不在）+ r-retired（自己的退场账）：两条被删。
    assert_eq!(report.orphan_edges, 2, "r-dead-endpoint / r-retired：{report:?}");

    let links: Vec<String> = m
        .conn()
        .prepare("SELECT id FROM fact_link ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        links,
        vec!["r-keep".to_string(), "r-resolved".to_string()],
        "已解除的退场账不该带走一条活边"
    );
    let prov: Vec<String> = m
        .conn()
        .prepare("SELECT mem_table||':'||mem_id FROM provenance WHERE ref_kind='turn' ORDER BY 1")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    // 活事实**自己的** turn 出处不在删除范围里（⑤ 只删"主人不在"的行，主人在 ⇒ 留）。
    // 活行：锚甲、活卡、另一个活卡；活边：r-keep、r-resolved。共五行。
    let mut want_prov: Vec<String> = vec![
        format!("anchor:{}", e_live.id),
        format!("card:{}", c_live.id),
        format!("card:{}", v_other.id),
        "fact_link:r-keep".to_string(),
        "fact_link:r-resolved".to_string(),
    ];
    want_prov.sort();
    assert_eq!(prov, want_prov, "主人不在的出处要删，活事实自己的出处要留");
    let mentions: Vec<String> = m
        .conn()
        .prepare("SELECT mem_table||':'||mem_id FROM memory_mention ORDER BY 1")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(mentions, vec![format!("card:{}", c_live.id)]);
    assert_eq!(count(m, "attention"), 1, "只留未解决且目标还在的那一条");
}

/// C4b2b：`resource_owner` 的孤儿行跟着目标行一起走。
///
/// 判据三条：① 死行（退场/被顶替/被否决）的归属行在**快照副本**里没了、活行的还在；
/// ② 目标行根本不存在（`card:'ghost'`）的归属行也被清掉；③ `card`/`anchor` 之外的
/// 归属行（`fact:'x'`）不动——与 C4b2a 的回填口径一致（回填只写这两张表）。
/// 中心库一行不动：prune 只跑在副本上。
#[test]
fn orphan_ownership_rows_are_dropped_with_their_rows() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let mut keys = Vec::new();
    seed_facts(root.memory(), &mut keys);
    let m = root.memory();
    let e_live = key_of(&keys, 0);
    let c_live = key_of(&keys, 2);
    let c_super = key_of(&keys, 3); // 被顶替 ⇒ 退场
    // 前置：六行每行都带一行归属（C3 的写入继承），否则本用例测的是空气。
    assert_eq!(count(m, "resource_owner"), 6, "前置：归属行没随 put_batch 落下");
    m.conn()
        .execute_batch(
            "INSERT INTO resource_owner(mem_table,mem_id,owner_principal_id,owner_agent_id,
                                        created_by_agent,visibility,retention,scope_kind,
                                        scope_key,source_agent,device_id,created_at_ms) VALUES
               ('card','ghost',NULL,'agent-snap','agent-snap','private_agent','default',
                'project','p-snap','agent-snap','dev-snap',1),
               ('fact','x',NULL,'agent-snap','agent-snap','private_agent','default',
                'project','p-snap','agent-snap','dev-snap',1);",
        )
        .unwrap();
    assert_eq!(count(m, "resource_owner"), 8, "前置：两条夹具归属行没写进去");

    let snaps = dir.path().join("snapshots");
    let built = build(m, &snaps, 2_000, true).unwrap();
    assert_eq!(built.pruned.dead_rows, 4, "前置：死行口径变了：{:?}", built.pruned);
    // 9 = ⑤ 删的 4 条死行的 turn 出处 + ⑩ 删的 4 条死行归属行 + 1 条 ghost。
    // `fact:x` 不算孤儿（不在这两张表的口径里），所以它不进这个数。
    assert_eq!(built.pruned.orphan_derived, 9, "孤儿归属行的计数不对：{:?}", built.pruned);

    let copy = rusqlite::Connection::open(&built.path).unwrap();
    let left: Vec<String> = copy
        .prepare("SELECT mem_table||':'||mem_id FROM resource_owner ORDER BY 1")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        left,
        vec![
            format!("anchor:{}", e_live.id),
            format!("card:{}", c_live.id),
            "fact:x".to_string(),
        ],
        "副本里该留的归属行（活卡/活锚 + 非 card/anchor 的表）或该清的行不对"
    );
    // 中心库一行不动（prune 只在副本上跑；被顶替的那张卡的归属行仍在中心库）。
    assert_eq!(count(m, "resource_owner"), 8, "prune 改了中心库");
    let center_super: i64 = m
        .conn()
        .query_row(
            "SELECT count(*) FROM resource_owner WHERE mem_table='card' AND mem_id=?1",
            [c_super.id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(center_super, 1, "中心库里被顶替卡的归属行被删了");
}

/// 直接对任意副本连接数行数（本文件里的 `count` 只收中心库句柄）。
fn rows_in(conn: &rusqlite::Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// H7b：`keep_review` 是快照的**受众**开关。
///
/// 判据：同一份库分别构建两条通道——备份副本两张审查表行数不变、agent 副本两表为 0；
/// 两份 digest 不同；两边 `rc_shadow` 都被丢掉；中心库本身一行不动（过滤只在副本上）。
#[test]
fn the_agent_copy_drops_review_tables_and_the_backup_copy_keeps_them() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let mut keys = Vec::new();
    seed_facts(root.memory(), &mut keys);
    let m = root.memory();
    // 中心库里真有审查队列与判定台账（H1a/H1c/H4b 的正常产物），索引层也在。
    m.conn()
        .execute_batch(
            "INSERT INTO review_item(source,pass_id,family,session_key,device_id,turn_uids_json,
                owner_json,payload_json,payload_sha256,state,created_at_ms,updated_at_ms) VALUES
               ('distill','naming','','s-1','dev-snap','[]','{}','{}','aa','pending_review',1,1),
               ('distill','fact','decision','s-1','dev-snap','[]','{}','{}','bb','approved',1,1);
             INSERT INTO review_decision(item_id,stage,verdict,reason_code,rules_version,
                input_sha256,decider,created_at_ms) VALUES
               (1,'program','pass','ok','v1','aa','program',1),
               (2,'review','reject','bad','v1','bb','center',1);
             CREATE VIRTUAL TABLE rc_shadow USING fts5(key, toks);
             INSERT INTO rc_shadow VALUES('card:1','a b');",
        )
        .unwrap();
    assert_eq!(
        (count(m, "review_item"), count(m, "review_decision")),
        (2, 2),
        "前置：审查两表的夹具没写进去"
    );

    let snaps = dir.path().join("snapshots");
    let backup = build(m, &snaps, 2_000, true).unwrap();
    let agent = build(m, &snaps, 2_001, false).unwrap();

    let backup_copy = rusqlite::Connection::open(&backup.path).unwrap();
    assert_eq!(rows_in(&backup_copy, "review_item"), 2, "备份副本丢了审查队列");
    assert_eq!(
        rows_in(&backup_copy, "review_decision"),
        2,
        "备份副本丢了判定台账"
    );

    let agent_copy = rusqlite::Connection::open(&agent.path).unwrap();
    assert_eq!(rows_in(&agent_copy, "review_item"), 0, "agent 副本带了审查队列");
    assert_eq!(
        rows_in(&agent_copy, "review_decision"),
        0,
        "agent 副本带了判定台账"
    );

    // 两表的清空发生在算摘要之前：两条通道的 digest 必然不同。
    assert_ne!(
        agent.digest, backup.digest,
        "agent 副本与备份副本的摘要相同——受众过滤没有进 digest"
    );

    // 索引层两边都不带（这是两条通道共同的性质，不是 `keep_review` 的一部分）。
    for copy in [&backup_copy, &agent_copy] {
        let left: i64 = copy
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name LIKE 'rc_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0, "副本里还剩索引层：{left}");
    }

    // 中心库一行不动：过滤只发生在副本上。
    assert_eq!(count(m, "review_item"), 2, "构建快照改了中心库的审查队列");
    assert_eq!(count(m, "review_decision"), 2, "构建快照改了中心库的判定台账");
}

#[test]
fn index_layer_is_dropped_from_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let mut keys = Vec::new();
    seed_facts(root.memory(), &mut keys);
    // 中心库里真有索引层（rc-index 的正常产物）；副本必须整层不带。
    root.memory()
        .conn()
        .execute_batch(
            "CREATE VIRTUAL TABLE rc_shadow USING fts5(key, toks);
             INSERT INTO rc_shadow VALUES('card:1','a b');
             CREATE TABLE rc_index_marker(norm_version INTEGER);
             INSERT INTO rc_index_marker VALUES(1);",
        )
        .unwrap();

    let snaps = dir.path().join("snapshots");
    let built = build(root.memory(), &snaps, 2_000, true).unwrap();
    let copy = rusqlite::Connection::open(&built.path).unwrap();
    let left: i64 = copy
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name LIKE 'rc_%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 0, "快照里还剩索引层：{left}");
    let version: i32 = copy.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(version, 6, "副本的 schema 版本必须与记忆库一致");
    // 中心库那一层不受影响（过滤发生在副本上）。
    let center: i64 = root
        .memory()
        .conn()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name LIKE 'rc_%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(center >= 2, "中心库的索引层被误删了：{center}");
}

#[test]
fn built_snapshot_has_no_wal_sidecar_and_the_digest_is_stable() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let mut keys = Vec::new();
    seed_facts(root.memory(), &mut keys);

    let snaps = dir.path().join("snapshots");
    let first = build(root.memory(), &snaps, 2_000, true).unwrap();
    let second = build(root.memory(), &snaps, 2_001, true).unwrap();
    assert_eq!(first.digest, second.digest, "同一份内容的摘要必须相同");
    assert!(first.bytes > 0);
    for built in [&first, &second] {
        assert!(!built.path.with_extension("db-wal").exists(), "副本带了 -wal");
        assert!(
            !std::path::Path::new(&format!("{}-shm", built.path.display())).exists(),
            "副本带了 -shm"
        );
    }
    // 副本以 readonly 打开、schema 检查通过、内容就是过滤后的那几行。
    let replica = AgentRoot::open_replica(dir.path(), &first.path).unwrap();
    assert!(replica.memory().is_read_only());
    let cards: i64 = replica
        .memory()
        .conn()
        .query_row("SELECT count(*) FROM card", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cards, 1, "副本里的活卡应当只有一张");
    // 摘要函数在副本上重算一遍，与 `build` 返回的那一格相同。
    assert_eq!(digest(replica.memory().conn()).unwrap(), first.digest);
    // 表清单是 25 张冻结记忆表 + S1b-C2a 的两张归属侧表 + S3a 的两张审查侧表
    // + W15a 的 card_verdict_event 背书事件表 + G1 刀 B 的 card_sink 归档章表
    // （侧表无记忆内容，副本里恒为空）。
    assert_eq!(MEMORY_TABLES.len(), 32);
}
