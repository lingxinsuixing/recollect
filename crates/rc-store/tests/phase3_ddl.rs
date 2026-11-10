// crates/rc-store/tests/phase3_ddl.rs
//! canonical 记忆库（`MEMORY_VERSION=6`）的 fresh 形状执法。
//!
//! 旧文件测的是 v1→v2→v3→v5 的迁移梯子；canonical 基线**没有迁移**（v≤5 一律拒绝，
//! 见 `memory_v6_rebuild.rs`），所以这里只剩一条路径：空库 open ⇒ 32 表。
//! 逐字口径的来源是 `design/2026-10-24-memory-schema.md` §3（本体表）与 §4（消费/派生/审计表），
//! 加上 `design/2026-10-26-ownership-persistence.md` §2.1 的两张归属侧表（S1b-C2a），
//! 以及 `design/2026-10-26-review-queue-s3.md` §3.1/§3.2 的两张审查侧表（S3a-1）。

use rc_store::Root;
use rusqlite::Connection;

/// 冻结的 32 张表（§2：本体 10 + 消费/派生 4 + 审计/运维 11 + 归属侧表 2 + 审查侧表 2，
/// 再加 W15a 的 card_verdict_event 背书事件表，再加 G1 刀 B 的 card_sink 归档章表，
/// 再加波二刀 A 的 card_reuse_event 复用事件表）。
/// 排序后与 `sqlite_master` 对拍。
const CANONICAL_TABLES: [&str; 32] = [
    "alias", "anchor", "anchor_merge", "attention", "attribute", "attestation", "card",
    "card_reuse_event", "card_sink", "card_verdict_event", "embedding", "episode", "episode_fact",
    "extraction_batch", "extraction_issue", "extraction_run", "fact", "fact_link",
    "memory_mention", "memory_op", "memory_stats", "mention", "provenance", "relation",
    "resource_grant", "resource_owner", "retirement", "review_decision", "review_item",
    "state_assertion", "time_expr", "utterance",
];

/// §3/§4 里的全部索引（29 条，含一条 UNIQUE）加两张归属侧表的 4 条、两张审查侧表的 4 条
/// （共 39；H1c 的在途部分唯一索引是最后一条；W14c 的 `attribute_value` 是属性值解析那一格；
/// W15a 的 `card_verdict_event_by_card` 是背书事件按卡取最新那一格；波二刀 A 的
/// `card_reuse_event_by_card` 是复用事件按卡聚合/取最近那一格）。
/// 少建/改名/多建都在这里红。
const CANONICAL_INDEXES: [&str; 40] = [
    "alias_lookup", "anchor_lookup", "anchor_world", "attention_target", "attestation_target",
    "attribute_anchor", "attribute_value", "card_asof", "card_reuse_event_by_card", "card_scope",
    "card_verdict_event_by_card",
    "episode_fact_ord",
    "extraction_batch_run",
    "fact_due", "fact_link_from", "fact_link_to", "fact_scope", "fact_subject", "fact_time",
    "memory_mention_anchor", "memory_op_target", "memory_stats_mentioned", "memory_stats_salience",
    "mention_anchor", "mention_turn", "provenance_turn", "relation_object", "relation_subject",
    "resource_grant_to_agent", "resource_grant_to_principal", "resource_owner_agent",
    "resource_owner_scope", "review_decision_by_item", "review_item_batch_inflight",
    "review_item_by_state", "review_item_payload", "state_relation", "state_subject",
    "utterance_dedup", "utterance_fact",
];

/// 永久废弃的旧表名（§2.4 第 2 条）。`state` 只作概念词，表名是 `state_assertion`。
const LEGACY_TABLES: [&str; 9] = [
    "assertion", "entity", "entity_alias", "entity_merge", "episode_event", "event",
    "event_link", "relationship", "state",
];

fn tables(conn: &Connection) -> Vec<String> {
    let mut st = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
    ).unwrap();
    st.query_map([], |r| r.get::<_, String>(0)).unwrap().filter_map(|r| r.ok()).collect()
}

fn indexes(conn: &Connection) -> Vec<String> {
    let mut st = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%' ORDER BY name"
    ).unwrap();
    st.query_map([], |r| r.get::<_, String>(0)).unwrap().filter_map(|r| r.ok()).collect()
}

/// 列名清单，按 `cid` 序（= CREATE 段里的列序）。
fn cols(conn: &Connection, table: &str) -> Vec<String> {
    let mut st = conn
        .prepare(&format!("SELECT name FROM pragma_table_info('{table}') ORDER BY cid"))
        .unwrap();
    st.query_map([], |r| r.get::<_, String>(0)).unwrap().filter_map(|r| r.ok()).collect()
}

/// 建表原文压成单行（空白折叠）。设计里的 CHECK 跨行书写，折叠后与设计原文逐字对得上；
/// 这些表的字面量里没有含空格的字符串，折叠不会把两种形状读成相同。
fn flat_sql(conn: &Connection, table: &str) -> String {
    let sql: String = conn
        .query_row("SELECT sql FROM sqlite_master WHERE type='table' AND name=?1", [table], |r| r.get(0))
        .unwrap();
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 一列的 (类型, NOT NULL, 默认值原文)。
fn col_shape(conn: &Connection, table: &str, name: &str) -> (String, bool, Option<String>) {
    conn.query_row(
        &format!("SELECT type, \"notnull\", dflt_value FROM pragma_table_info('{table}') WHERE name=?1"),
        [name],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    ).unwrap()
}

/// 外键清单：`from->table:on_delete`，排序。
fn fks(conn: &Connection, table: &str) -> Vec<String> {
    let sql = format!(
        "SELECT \"from\", \"table\", on_delete FROM pragma_foreign_key_list('{table}')"
    );
    let mut st = conn.prepare(&sql).unwrap();
    let mut out: Vec<String> = st.query_map([], |r| {
        Ok(format!("{}->{}:{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    }).unwrap().filter_map(|r| r.ok()).collect();
    out.sort();
    out
}

#[test]
fn fresh_memory_db_has_exactly_the_29_canonical_tables() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let roster = tables(root.memory().conn());

    let mut want = CANONICAL_TABLES.to_vec();
    want.sort();
    assert_eq!(roster, want, "fresh memory.db 的表清单必须逐张等于 canonical 32 表");
    for old in LEGACY_TABLES {
        assert!(!roster.iter().any(|t| t == old),
                "fresh memory.db 出现了废弃表名 {old}（§2.4：永久废弃）");
    }
}

#[test]
fn key_tables_have_the_designed_column_order() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let conn = root.memory().conn();

    // §3 anchor
    assert_eq!(cols(conn, "anchor"), vec![
        "id", "kind", "canonical", "reality_status", "world_id", "scope_kind", "scope_key",
        "status", "profile_text", "created_at_ms", "updated_at_ms", "norm_version",
    ], "anchor 列序与设计 §3 不一致");
    // §3 fact
    assert_eq!(cols(conn, "fact"), vec![
        "id", "subject_anchor_id", "predicate", "object_anchor_id", "value_json", "value_text",
        "category", "participants_json", "place_anchor_id", "speaker_anchor_id",
        "occurred_at_ms", "occurred_end_ms", "speech_time_ms", "valid_from_ms", "valid_to_ms",
        "due_at_ms", "status", "mode", "confidence", "text", "evidence_turn_uid", "superseded_by",
        "world_id", "scope_kind", "scope_key", "source_agent", "device_id", "created_at_ms",
        "norm_version",
    ], "fact 列序与设计 §3 不一致");
    // §3 state_assertion
    assert_eq!(cols(conn, "state_assertion"), vec![
        "id", "subject_kind", "subject_anchor_id", "subject_relation_id", "dimension",
        "value_before", "value_after", "valid_from_ms", "valid_to_ms", "superseded_by",
        "cause_fact_id", "text", "mode", "confidence", "evidence_turn_uid", "world_id",
        "scope_kind", "scope_key", "source_agent", "device_id", "created_at_ms", "norm_version",
    ], "state_assertion 列序与设计 §3 不一致");
    // §4 card（消费行：kind 自由文本；event 的 causal/time_anchor 列并入）
    assert_eq!(cols(conn, "card"), vec![
        "id", "kind", "title", "text", "time_json", "pointers_json", "scope_kind", "scope_key",
        "world_id", "as_of_ms", "valid_to_ms", "superseded_by", "confidence", "mode", "verdict",
        "cooldown_until_ms", "causal_type", "time_anchor_ms", "time_anchor_id", "time_confidence",
        "source_agent", "device_id", "created_at_ms", "norm_version",
    ], "card 列序与设计 §4 不一致");
    // §4 fact_link（端点存 "<table>:<id>" 键）
    assert_eq!(cols(conn, "fact_link"), vec![
        "id", "from_id", "to_id", "relation", "basis", "evidence_json", "decided_by",
        "scope_kind", "scope_key", "world_id", "source_agent", "device_id", "created_at_ms",
        "norm_version",
    ], "fact_link 列序与设计 §4 不一致");
    // §4 provenance（ref_kind 的值域是 canonical 表名）
    assert_eq!(cols(conn, "provenance"), vec![
        "mem_table", "mem_id", "turn_uid", "anchor_role", "ref_kind", "ord", "role",
    ], "provenance 列序与设计 §4 不一致");
}

#[test]
fn key_tables_carry_the_designed_checks_and_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let conn = root.memory().conn();

    let anchor = flat_sql(conn, "anchor");
    for want in [
        "CHECK(kind IN ('person','agent','org','place','object','animal','work','event','concept'))",
        "CHECK(reality_status IN ('real','fictional','virtual','abstract','hypothetical','unknown'))",
        "CHECK(status IN ('active','provisional','merged','retired'))",
    ] {
        assert!(anchor.contains(want), "anchor 的建表原文缺 `{want}`：{anchor}");
    }
    assert_eq!(col_shape(conn, "anchor", "reality_status").2, Some("'unknown'".to_string()));
    assert_eq!(col_shape(conn, "anchor", "status").2, Some("'active'".to_string()));

    let fact = flat_sql(conn, "fact");
    for want in [
        "CHECK(category IN ('event','promise','plan','decision','observation','state_change','other'))",
        "CHECK(status IN ('pending','done','cancelled','expired'))",
        "CHECK(mode IN ('stated','inferred','reported'))",
    ] {
        assert!(fact.contains(want), "fact 的建表原文缺 `{want}`：{fact}");
    }
    assert_eq!(col_shape(conn, "fact", "participants_json").2, Some("'[]'".to_string()));
    assert_eq!(col_shape(conn, "fact", "status").2, None, "fact.status 必须是可空（无 DEFAULT）");

    let state = flat_sql(conn, "state_assertion");
    for want in [
        "CHECK(subject_kind IN ('anchor','relation'))",
        "CHECK(dimension IN ('emotion','mood','sentiment','affect_episode','trust','relationship','health','config','progress'))",
        "CHECK(mode IN ('expressed','inferred'))",
        "CHECK((subject_kind='anchor') = (subject_anchor_id IS NOT NULL))",
        "CHECK((subject_kind='relation') = (subject_relation_id IS NOT NULL))",
    ] {
        assert!(state.contains(want), "state_assertion 的建表原文缺 `{want}`：{state}");
    }

    let card = flat_sql(conn, "card");
    for want in [
        "kind TEXT NOT NULL",
        "CHECK(causal_type IS NULL OR causal_type IN ('causes','enables','prevents'))",
        "text TEXT NOT NULL",
        "verdict TEXT NOT NULL DEFAULT 'unreviewed'",
    ] {
        assert!(card.contains(want), "card 的建表原文缺 `{want}`：{card}");
    }
    assert!(!card.contains("CHECK(kind"), "card.kind 是自由文本（§2.2.1），不能再有闭集 CHECK：{card}");
    assert!(!card.contains("body"), "card.body 已废弃（§3.3），主表示只留 text：{card}");

    let link = flat_sql(conn, "fact_link");
    for want in [
        "from_id TEXT NOT NULL",
        "to_id TEXT NOT NULL",
        "CHECK(relation IN ('causes','enables','prevents','responds_to','part_of','depends_on','owns','located_in','role'))",
        "CHECK(from_id <> to_id)",
    ] {
        assert!(link.contains(want), "fact_link 的建表原文缺 `{want}`：{link}");
    }
    assert!(!link.contains("from_fact_id") && !link.contains("to_fact_id"),
            "fact_link 端点已改名 from_id/to_id（存 \"<table>:<id>\" 键）：{link}");

    let prov = flat_sql(conn, "provenance");
    for want in [
        "CHECK(anchor_role IN ('human','agent','system'))",
        "CHECK(ref_kind IN ('turn','card','fact','anchor','relation','state_assertion','utterance','episode','attribute','mention'))",
        "role TEXT NOT NULL DEFAULT 'basis' CHECK((role='link')=(ord IS NOT NULL))",
    ] {
        assert!(prov.contains(want), "provenance 的建表原文缺 `{want}`：{prov}");
    }
    assert_eq!(col_shape(conn, "provenance", "ref_kind").2, Some("'turn'".to_string()));
    assert_eq!(col_shape(conn, "provenance", "role").2, Some("'basis'".to_string()));
}

#[test]
fn foreign_keys_match_the_design() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let conn = root.memory().conn();

    assert_eq!(fks(conn, "fact"), vec![
        "object_anchor_id->anchor:SET NULL",
        "place_anchor_id->anchor:SET NULL",
        "speaker_anchor_id->anchor:SET NULL",
        "subject_anchor_id->anchor:CASCADE",
    ], "fact 的外键与 ON DELETE 与设计 §3 不一致");
    assert_eq!(fks(conn, "state_assertion"), vec![
        "cause_fact_id->fact:SET NULL",
        "subject_anchor_id->anchor:CASCADE",
        "subject_relation_id->relation:CASCADE",
    ], "state_assertion 的外键与 ON DELETE 与设计 §3 不一致");
    assert_eq!(fks(conn, "episode_fact"), vec![
        "episode_id->episode:CASCADE",
        "fact_id->fact:CASCADE",
    ], "episode_fact 的外键与设计 §3 不一致");
}

#[test]
fn checks_are_enforced_not_just_stored() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let conn = root.memory().conn();
    let base_anchor = |id: &str| format!(
        "INSERT INTO anchor(id,kind,canonical,scope_kind,scope_key,created_at_ms,updated_at_ms,norm_version)
         VALUES('{id}','person','{id}','relationship','couple:x',1,1,1)"
    );
    conn.execute_batch(&base_anchor("a1")).unwrap();
    conn.execute_batch(&base_anchor("a2")).unwrap();

    // anchor.kind 的封闭值域
    assert!(conn.execute_batch(
        "INSERT INTO anchor(id,kind,canonical,scope_kind,scope_key,created_at_ms,updated_at_ms,norm_version)
         VALUES('bad','robot','bad','relationship','s',1,1,1)").is_err(),
        "anchor.kind 的 CHECK 没生效：'robot' 被收下了");

    let fact = |id: &str, category: &str| format!(
        "INSERT INTO fact(id,subject_anchor_id,predicate,category,mode,text,evidence_turn_uid,
                          scope_kind,scope_key,source_agent,device_id,created_at_ms,norm_version)
         VALUES('{id}','a1','likes','{category}','stated','t','t1','relationship','s','ag','dev',1,1)"
    );
    conn.execute_batch(&fact("f-ok", "event")).unwrap();
    assert!(conn.execute_batch(&fact("f-bad", "bogus")).is_err(),
        "fact.category 的 CHECK 没生效：'bogus' 被收下了");

    // state_assertion 的两条主体互斥 CHECK：kind='anchor' 却给 relation 主体 ⇒ 拒。
    let state = |id: &str, anchor: &str, relation: &str| format!(
        "INSERT INTO state_assertion(id,subject_kind,subject_anchor_id,subject_relation_id,
                                     dimension,value_after,text,mode,scope_kind,scope_key,
                                     source_agent,device_id,created_at_ms,norm_version)
         VALUES('{id}','anchor',{anchor},{relation},'trust','up','t','expressed','relationship','s','ag','dev',1,1)"
    );
    conn.execute_batch(&state("s-ok", "'a1'", "NULL")).unwrap();
    assert!(conn.execute_batch(&state("s-bad", "NULL", "NULL")).is_err(),
        "state_assertion 的主体互斥 CHECK 没生效：subject_kind='anchor' 而两列全 NULL 被收下了");
    assert!(conn.execute_batch(&state("s-bad2", "NULL", "'r1'")).is_err(),
        "state_assertion 的主体互斥 CHECK 没生效：anchor 主体位空、relation 主体位非空被收下了");

    // provenance 的 role↔ord 耦合
    assert!(conn.execute_batch(
        "INSERT INTO provenance(mem_table,mem_id,turn_uid,anchor_role,role)
         VALUES('fact','f-ok','t1','human','link')").is_err(),
        "provenance 的 role↔ord CHECK 没生效：role='link' 而 ord 为空被收下了");
    conn.execute_batch(
        "INSERT INTO provenance(mem_table,mem_id,turn_uid,anchor_role,ord,role)
         VALUES('fact','f-ok','t2','human',2,'link')").unwrap();
    // ref_kind 值域（canonical 表名；旧名 'event' 已不在）
    assert!(conn.execute_batch(
        "INSERT INTO provenance(mem_table,mem_id,turn_uid,anchor_role,ref_kind)
         VALUES('fact','f-ok','t3','human','event')").is_err(),
        "provenance.ref_kind 的 CHECK 没生效：废弃表名 'event' 被收下了");
}

#[test]
fn all_canonical_indexes_are_present() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let got = indexes(root.memory().conn());
    let mut want = CANONICAL_INDEXES.to_vec();
    want.sort();
    assert_eq!(got, want, "记忆库的索引清单必须逐条等于 §3/§4 的 29 条 + 侧表 4 条 + 审查 4 条 + 背书 1 条");
}

#[test]
fn memory_sql_does_not_create_the_index_layer() {
    // 裁定 R15：索引层（rc_shadow / rc_index_marker）的唯一所有者是 rc-index，
    // MEMORY_SQL 建它会让"整层可丢弃、幂等重建"在结构上不成立（每次 open 复活空表）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let roster = tables(root.memory().conn());
    for t in ["rc_shadow", "rc_index_marker"] {
        assert!(!roster.iter().any(|n| n == t), "MEMORY_SQL 建出了索引层表 {t}（R15）");
    }
    let idx = indexes(root.memory().conn());
    assert!(!idx.iter().any(|n| n.starts_with("rc_")), "MEMORY_SQL 建出了索引层索引：{idx:?}");
}

#[test]
fn reopening_a_v6_memory_db_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let before = {
        let root = Root::open(dir.path()).unwrap();
        let conn = root.memory().conn();
        assert_eq!(rc_store::ddl::schema_version(conn).unwrap(), rc_store::ddl::MEMORY_VERSION);
        (tables(conn), flat_sql(conn, "fact"), flat_sql(conn, "card"))
    };
    let root = Root::open(dir.path()).unwrap();
    let conn = root.memory().conn();
    assert_eq!(rc_store::ddl::schema_version(conn).unwrap(), rc_store::ddl::MEMORY_VERSION,
               "重开一个 v6 库不该再动 user_version");
    assert_eq!(tables(conn), before.0, "重开改动了表清单");
    assert_eq!(flat_sql(conn, "fact"), before.1, "重开改动了 fact 的建表原文");
    assert_eq!(flat_sql(conn, "card"), before.2, "重开改动了 card 的建表原文");
}

/// G1 刀 B 第 0 件：归档章表升 canonical 正路。列序/外键/闭集逐格钉住，且 DDL 只有一个家
/// （`CARD_SINK_SQL`）——写路径的幂等补表重放的是同一根字符串，不许漂成第二份。
#[test]
fn card_sink_is_canonical_with_one_ddl_home_and_an_open_reason() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let conn = root.memory().conn();
    assert_eq!(
        cols(conn, "card_sink"),
        vec!["card_id", "sink", "reason", "at_ms", "norm_version"],
        "card_sink 的列序必须与 CARD_SINK_SQL 逐字一致"
    );
    assert_eq!(
        fks(conn, "card_sink"),
        vec!["card_id->card:NO ACTION"],
        "章指向卡：外键必须在（hub rows 的 INSERT_ORDER 因此把 card_sink 排在 card 之后）"
    );
    let sql = flat_sql(conn, "card_sink");
    // sqlite_master 存的是去掉 `IF NOT EXISTS` 的建表原文（SQLite 自己剥的），对拍前先剥同一格。
    let home = rc_store::ddl::CARD_SINK_SQL
        .replace("IF NOT EXISTS ", "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(sql, home, "开库建出来的原文必须与唯一 DDL 家逐字一致");
    assert!(sql.contains("CHECK(sink IN ('archive'))"), "sink 闭集只有 archive：{sql}");

    // 行为面：两族 reason 都收（程序章 + LLM 价值轴章），sink 的闭集真的执法。
    conn.execute_batch(
        "INSERT INTO card(id,kind,title,text,scope_kind,scope_key,as_of_ms,
                          source_agent,device_id,created_at_ms,norm_version)
         VALUES('c1','observation','甲','正文一','project','p',1,'ag','dev',1,1),
               ('c2','observation','乙','正文二','project','p',1,'ag','dev',1,1),
               ('c3','observation','丙','正文三','project','p',1,'ag','dev',1,1);
         INSERT INTO card_sink(card_id,sink,reason,at_ms,norm_version)
         VALUES('c1','archive','program_gate',1,1),('c2','archive','value_axis',1,1);",
    )
    .unwrap();
    let reasons: Vec<String> = {
        let mut st = conn.prepare("SELECT reason FROM card_sink ORDER BY card_id").unwrap();
        st.query_map([], |r| r.get(0)).unwrap().filter_map(|r| r.ok()).collect()
    };
    assert_eq!(reasons, vec!["program_gate".to_string(), "value_axis".to_string()],
               "reason 不许加 CHECK：两族章同表共存（加 CHECK 就得重建表）");
    assert!(
        conn.execute_batch(
            "INSERT INTO card_sink(card_id,sink,reason,at_ms,norm_version)
             VALUES('c3','main','value_axis',1,1)"
        )
        .is_err(),
        "sink 的 CHECK 没生效：'main' 被收下了"
    );
}

/// 波二刀 A：复用反馈事件表走 canonical 正路（与 `card_sink` 同一条）。列序/外键/闭集/
/// 索引逐格钉住，且 DDL 只有一个家（`CARD_REUSE_EVENT_SQL`）——写路径的幂等补表重放的是
/// 同一根字符串，不许漂成第二份。append-only 由"同卡同 outcome 两次 = 两行"钉住。
#[test]
fn card_reuse_event_is_canonical_append_only_with_one_ddl_home() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let conn = root.memory().conn();
    assert_eq!(
        cols(conn, "card_reuse_event"),
        vec!["id", "card_id", "outcome", "note", "at_ms", "by_agent", "norm_version"],
        "card_reuse_event 的列序必须与 CARD_REUSE_EVENT_SQL 逐字一致"
    );
    assert_eq!(
        fks(conn, "card_reuse_event"),
        vec!["card_id->card:NO ACTION"],
        "反馈指向卡：外键必须在（hub rows 的 INSERT_ORDER 因此把 card_reuse_event 排在 card 之后）"
    );
    let sql = flat_sql(conn, "card_reuse_event");
    // sqlite_master 存的是去掉 `IF NOT EXISTS` 的建表原文（SQLite 自己剥的），对拍前先剥同一格。
    let home = rc_store::ddl::CARD_REUSE_EVENT_SQL
        .replace("IF NOT EXISTS ", "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(sql, home, "开库建出来的原文必须与唯一 DDL 家逐字一致");
    for want in [
        "CHECK(outcome IN ('worked','failed','partial','mismatch','stale'))",
        "note TEXT",
        "by_agent TEXT NOT NULL",
        "norm_version INTEGER NOT NULL",
    ] {
        assert!(sql.contains(want), "card_reuse_event 的建表原文缺 `{want}`：{sql}");
    }
    // 索引：按卡（刀 B 的聚合与"最近一条"都从这一条走）。
    let idx: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='card_reuse_event_by_card'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx, 1, "card_reuse_event 的按卡索引不在");

    // 行为面：闭集执法 + append-only（同卡同 outcome 两次 = 两行）。
    conn.execute_batch(
        "INSERT INTO card(id,kind,title,text,scope_kind,scope_key,as_of_ms,
                          source_agent,device_id,created_at_ms,norm_version)
         VALUES('c1','observation','甲','正文一','project','p',1,'ag','dev',1,1);
         INSERT INTO card_reuse_event(card_id,outcome,note,at_ms,by_agent,norm_version)
         VALUES('c1','worked','先这样',1,'ag',1),('c1','worked',NULL,2,'ag',1);",
    )
    .unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM card_reuse_event WHERE card_id='c1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2, "append-only：同卡同 outcome 写两次必须是两行");
    assert!(
        conn.execute_batch(
            "INSERT INTO card_reuse_event(card_id,outcome,note,at_ms,by_agent,norm_version)
             VALUES('c1','helped',NULL,3,'ag',1)"
        )
        .is_err(),
        "outcome 的 CHECK 没生效：闭集外的值被收下了"
    );
    // 悬空卡被外键挡住（先认卡再写的那道门的库层兜底）。
    conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    assert!(
        conn.execute_batch(
            "INSERT INTO card_reuse_event(card_id,outcome,note,at_ms,by_agent,norm_version)
             VALUES('no-such-card','worked',NULL,4,'ag',1)"
        )
        .is_err(),
        "外键没生效：反馈指向了不存在的卡"
    );
}
