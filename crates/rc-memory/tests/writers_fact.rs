//! S2b-2a：fact 行的写库层（设计 `2026-10-26-s2-prompt-v0` §2.2 下游、§3 幂等、§4 归属）。
//!
//! 这一层钉五件事：
//!
//! 1. `compute_fact_id` 是内容寻址：同内容同 id；object/value 的缺席与空串不同；
//!    字段边界不同不撞；object/value/time_text/scope 任一变化即新 id。
//! 2. 一条合法 item 同时落五处行：`fact` + `provenance` + `memory_mention` + 最小 `card`
//!    + `resource_owner`（fact 与 card 各一行），且逐列关键值正确、模型面字段一个都不在。
//! 3. object-only 与 value-only 都合法；subject 与 object 相同 ⇒ mention 只一行。
//! 4. 非法 category/mode/双空/空 evidence/空 subject/空 text/空 turn_uid ⇒ 整批 `Err` 且零行。
//! 5. 重放计数全 0、库里行数不变、owner 不被覆盖；owner 继承与 visibility 上界正确。
//! 6. （S2d-1）`time_text` 非空 ⇒ 每条 fact 后跟一条**未解析**的 `time_expr`
//!    （`start/end=NULL`、`precision='unknown'`、`basis='none'`），kind 按类别+短语取，
//!    同一 `(turn_uid, text, kind)` 只一行、重放不增行；`time_text` 空 ⇒ 零行。
//!
//! 正反例只用通用占位名（张三/李四/某项目），不含任何语料专名或 holdout。
use rc_memory::identity::{compute_fact_id, compute_time_expr_id};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_fact::{
    is_time_phrase, put_fact_rows, put_fact_rows_with_extras, time_kind_for, FactExtras,
    FactWriteItem, FACT_CATEGORIES,
};
use rc_memory::{compute_mem_id, AnchorRole, BatchCtx, MemError, MemTable, Scope, ScopeKind};
use rc_store::{MemoryHandle, Root};

const NOW: i64 = 1_700_000_999_000;
const SRC_MS: i64 = 1_700_000_000_000;

fn uid(index: usize) -> String {
    format!("{:064x}", 0x11 + index)
}

/// 参与者锚的正名（`seed_anchors` 铺的那两行）：铺锚与取句柄共用这一串，免得两处各写一份。
fn person_canonical(index: usize) -> String {
    format!("锚-{index}")
}

/// 地点锚的正名（`seed_place_anchor` 铺的那一行）。
const PLACE_CANONICAL: &str = "某机房";

/// 锚的句柄按生产算法算：kind/canonical/scope 就是两条 `seed_*` 铺下的行上的三格。
/// 3 号是地点锚（`place`），其余是参与者锚（`person`）——两张行、两种 kind，不共用身份。
fn anchor(index: usize) -> String {
    match index {
        3 => compute_mem_id(MemTable::Anchor, "place", PLACE_CANONICAL, "", &scope()),
        n => compute_mem_id(MemTable::Anchor, "person", &person_canonical(n), "", &scope()),
    }
}

fn ctx() -> BatchCtx {
    // 与 `BatchCtx::synthetic()` 不同的一对值：P 独占列若偷偷用合成常量，下面会红。
    BatchCtx { source_agent: "agent-fact-w".into(), device_id: "dev-fact-w".into() }
}

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-fact".into() }
}

fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-fact".into()),
        owner_agent_id: "agent-owner".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}

/// 一条合法 promise：subject 张三、object 李四、两条 evidence。
fn item() -> FactWriteItem {
    FactWriteItem {
        subject_anchor_id: anchor(1),
        object_anchor_id: Some(anchor(2)),
        predicate: "promise".into(),
        value: Some("把对账单发给李四".into()),
        category: "promise".into(),
        mode: "stated".into(),
        time_text: "下周三之前".into(),
        quote: "我下周三之前把对账单发给你".into(),
        evidence: vec![(uid(1), AnchorRole::Human), (uid(2), AnchorRole::Agent)],
        text: "张三承诺在下周三之前把对账单发给李四，并在当天下午跟进确认一次。".into(),
        scope: scope(),
        source_ms: SRC_MS,
    }
}

fn count(m: &MemoryHandle, table: &str) -> i64 {
    m.conn().query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap()
}

/// 锚行是 `fact.subject_anchor_id/object_anchor_id` 的外键目标，由 naming pass 先写。
/// 本片只测写库层，所以测试自己铺两条锚；**kind/canonical 与 [`anchor`] 同一对值**，
/// 铺下的行与断言里那串 hex 因此同源（不是两处各写一份）。
fn seed_anchors(m: &MemoryHandle) {
    for index in [1, 2] {
        let id = anchor(index);
        m.conn().execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind,
                                scope_key, status, profile_text, created_at_ms, updated_at_ms,
                                norm_version)
             VALUES(?1,'person',?2,'unknown',NULL,'project','proj-fact','active',NULL,?3,?3,?4)",
            rusqlite::params![id, person_canonical(index), SRC_MS,
                              rc_store::capability::NORM_VERSION as i64],
        ).unwrap();
    }
}

/// `fact.place_anchor_id` 有 `REFERENCES anchor(id)`：地点锚与参与者锚一样，必须先由
/// naming pass 铺好。第三条锚只在 extras 用例里用。
fn seed_place_anchor(m: &MemoryHandle) {
    m.conn().execute(
        "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind,
                            scope_key, status, profile_text, created_at_ms, updated_at_ms,
                            norm_version)
         VALUES(?1,'place',?2,'unknown',NULL,'project','proj-fact','active',NULL,?3,?3,?4)",
        rusqlite::params![anchor(3), PLACE_CANONICAL, SRC_MS,
                          rc_store::capability::NORM_VERSION as i64],
    ).unwrap();
}

fn card_title(text: &str) -> String {
    text.chars().take(30).collect()
}

// ── ① compute_fact_id ────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn id_of(
    subject: &str, predicate: &str, object: Option<&str>, value: Option<&str>,
    category: &str, time_text: &str, text: &str, scope: &Scope,
) -> String {
    compute_fact_id(subject, predicate, object, value, category, time_text, text, scope)
}

#[test]
fn fact_id_is_stable_for_the_same_content() {
    let a = id_of("s1", "promise", Some("o1"), Some("v1"), "promise", "下周", "某句正文", &scope());
    let b = id_of("s1", "promise", Some("o1"), Some("v1"), "promise", "下周", "某句正文", &scope());
    assert_eq!(a, b, "同内容重写必须算出同一个 id");
    assert_eq!(a.len(), 64, "句柄是 64 位小写 hex");
    assert!(a.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
}

#[test]
fn fact_id_distinguishes_absent_from_empty_string() {
    // None 若与 Some("") 撞，则"没有对象"与"对象是空串"这两条不同的行长成同一行。
    let absent = id_of("s1", "promise", None, Some("v1"), "promise", "下周", "正文", &scope());
    let empty = id_of("s1", "promise", Some(""), Some("v1"), "promise", "下周", "正文", &scope());
    assert_ne!(absent, empty, "object 的缺席与空串必须不同");

    let absent_v = id_of("s1", "promise", Some("o1"), None, "promise", "下周", "正文", &scope());
    let empty_v = id_of("s1", "promise", Some("o1"), Some(""), "promise", "下周", "正文", &scope());
    assert_ne!(absent_v, empty_v, "value 的缺席与空串必须不同");
}

#[test]
fn fact_id_does_not_collide_across_field_boundaries() {
    // 段间 0u8 分隔的牙：无分隔时 ("ab","c") 与 ("a","bc") 都拼成 "abc"。
    let left = id_of("ab", "c", None, Some("v"), "promise", "下周", "正文", &scope());
    let right = id_of("a", "bc", None, Some("v"), "promise", "下周", "正文", &scope());
    assert_ne!(left, right, "subject/predicate 的边界必须由分隔保护");

    // object 与 value 相邻两段同一把尺子：("ab","c") 与 ("a","bc") 不许撞。
    let o1 = id_of("s", "p", Some("ab"), Some("c"), "promise", "下周", "正文", &scope());
    let o2 = id_of("s", "p", Some("a"), Some("bc"), "promise", "下周", "正文", &scope());
    assert_ne!(o1, o2, "object/value 的边界必须由分隔保护");

    // 缺席标记不许被"值恰好是那个标记"冒充。
    let absent = id_of("s", "p", None, Some("v"), "promise", "下周", "正文", &scope());
    let literal_none = id_of("s", "p", Some("None"), Some("v"), "promise", "下周", "正文", &scope());
    assert_ne!(absent, literal_none, "缺席标记不能是能被内容写出来的字面量");
}

#[test]
fn fact_id_changes_when_any_identity_field_changes() {
    let base = id_of("s1", "promise", Some("o1"), Some("v1"), "promise", "下周", "正文", &scope());
    let variants = [
        id_of("s2", "promise", Some("o1"), Some("v1"), "promise", "下周", "正文", &scope()),
        id_of("s1", "plan", Some("o1"), Some("v1"), "promise", "下周", "正文", &scope()),
        id_of("s1", "promise", Some("o2"), Some("v1"), "promise", "下周", "正文", &scope()),
        id_of("s1", "promise", Some("o1"), Some("v2"), "promise", "下周", "正文", &scope()),
        id_of("s1", "promise", Some("o1"), Some("v1"), "plan", "下周", "正文", &scope()),
        id_of("s1", "promise", Some("o1"), Some("v1"), "promise", "下下周", "正文", &scope()),
        id_of("s1", "promise", Some("o1"), Some("v1"), "promise", "下周", "另一句正文", &scope()),
        id_of("s1", "promise", Some("o1"), Some("v1"), "promise", "下周", "正文",
               &Scope { kind: ScopeKind::Project, key: "proj-other".into() }),
        id_of("s1", "promise", Some("o1"), Some("v1"), "promise", "下周", "正文",
               &Scope { kind: ScopeKind::Person, key: "proj-fact".into() }),
    ];
    for (index, variant) in variants.iter().enumerate() {
        assert_ne!(&base, variant, "第 {index} 个字段的变化必须算出新 id");
    }
}

#[test]
fn fact_id_normalizes_text_time_and_scope_key() {
    // 与 `compute_mem_id` 同一把归一尺子：CRLF 与 NFC 的差异不许造出"同一条事实的两个 id"。
    let lf = id_of("s1", "promise", Some("o1"), Some("v1"), "promise", "下周", "第一行\n第二行", &scope());
    let crlf = id_of("s1", "promise", Some("o1"), Some("v1"), "promise", "下周", "第一行\r\n第二行", &scope());
    assert_eq!(lf, crlf, "正文的换行归一必须进 preimage");

    let spaced = Scope { kind: ScopeKind::Project, key: " proj-fact ".into() };
    assert_eq!(
        id_of("s1", "promise", Some("o1"), Some("v1"), "promise", " 下周 ", "正文", &scope()),
        id_of("s1", "promise", Some("o1"), Some("v1"), "promise", "下周", "正文", &spaced),
        "time_text 与 scope.key 都先过 normalize_body",
    );
}

// ── ② 合法 item 的五处行 ──────────────────────────────────────────────

#[test]
fn one_legal_item_writes_fact_provenance_mention_card_and_owner() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchors(m);
    let it = item();

    let out = put_fact_rows(m, std::slice::from_ref(&it), &ctx(), &owner(), NOW).unwrap();
    assert_eq!((out.facts, out.cards, out.mentions), (1, 1, 2), "两处锚 ⇒ 两条 mention");
    assert_eq!(count(m, "fact"), 1);
    assert_eq!(count(m, "card"), 1);
    // 2026-10-30（P0-2）：出处多了一组镜像卡的（与 fact 同证据轮、同角色），
    // 所以总数 4；两张表各自仍是"多证据全写"的 2 行。
    assert_eq!(count(m, "provenance"), 4, "fact 2 行 + 镜像卡 2 行");
    assert_eq!(
        m.conn().query_row("SELECT count(*) FROM provenance WHERE mem_table='fact'", [], |r| r.get::<_, i64>(0)).unwrap(),
        2, "多证据全写（fact 名下）");
    assert_eq!(
        m.conn().query_row("SELECT count(*) FROM provenance WHERE mem_table='card'", [], |r| r.get::<_, i64>(0)).unwrap(),
        2, "镜像卡带自己的 turn 出处（P0-2）");
    assert_eq!(count(m, "memory_mention"), 2);
    assert_eq!(count(m, "resource_owner"), 2, "fact 与 card 各一行归属");

    let expected_id = compute_fact_id(
        &it.subject_anchor_id, &it.predicate, it.object_anchor_id.as_deref(),
        it.value.as_deref(), &it.category, &it.time_text, &it.text, &it.scope,
    );
    // 逐列：内容列 + 程序盖章列 + 全部时间/状态列都必须留空（模型面不写时间）。
    let row = m.conn().query_row(
        "SELECT subject_anchor_id, predicate, object_anchor_id, value_json, value_text, category,
                participants_json, place_anchor_id, speaker_anchor_id, occurred_at_ms,
                occurred_end_ms, speech_time_ms, valid_from_ms, valid_to_ms, due_at_ms, status,
                mode, confidence, text, evidence_turn_uid, world_id, scope_kind, scope_key,
                source_agent, device_id, created_at_ms, norm_version
           FROM fact WHERE id=?1",
        [&expected_id],
        |r| Ok((
            r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, String>(5)?,
            r.get::<_, String>(6)?, r.get::<_, Option<String>>(7)?, r.get::<_, Option<String>>(8)?,
            r.get::<_, Option<i64>>(9)?, r.get::<_, Option<i64>>(10)?, r.get::<_, Option<i64>>(11)?,
            r.get::<_, Option<i64>>(12)?, r.get::<_, Option<i64>>(13)?, r.get::<_, Option<i64>>(14)?,
            r.get::<_, Option<String>>(15)?, r.get::<_, String>(16)?, r.get::<_, f64>(17)?,
            r.get::<_, String>(18)?, r.get::<_, String>(19)?, r.get::<_, Option<String>>(20)?,
            r.get::<_, String>(21)?, r.get::<_, String>(22)?, r.get::<_, String>(23)?,
            r.get::<_, String>(24)?, r.get::<_, i64>(25)?, r.get::<_, i64>(26)?,
        )),
    ).unwrap();

    assert_eq!(row.0, it.subject_anchor_id);
    assert_eq!(row.1, "promise");
    assert_eq!(row.2.as_deref(), Some(anchor(2).as_str()));
    assert_eq!(row.3.as_deref(), Some("\"把对账单发给李四\""), "value_json 是 JSON 字符串");
    assert_eq!(row.4.as_deref(), Some("把对账单发给李四"));
    assert_eq!(row.5, "promise");
    assert_eq!(row.6, format!("[\"{}\",\"{}\"]", anchor(1), anchor(2)), "participants 去重有序");
    assert!(row.7.is_none(), "place 先空");
    assert!(row.8.is_none(), "speaker 先空（v0 不填）");
    for (label, value) in [("occurred_at_ms", row.9), ("occurred_end_ms", row.10),
                           ("speech_time_ms", row.11), ("valid_from_ms", row.12),
                           ("valid_to_ms", row.13), ("due_at_ms", row.14)] {
        assert!(value.is_none(), "{label} 必须留空，等 time pass 回填");
    }
    assert!(row.15.is_none(), "status 先空");
    assert_eq!(row.16, "stated");
    assert!((row.17 - 0.8).abs() < 1e-9, "confidence 默认 0.8");
    assert_eq!(row.18, it.text);
    assert_eq!(row.19, uid(1), "evidence_turn_uid 取第一条证据");
    assert!(row.20.is_none(), "world_id 为空");
    assert_eq!((row.21.as_str(), row.22.as_str()), ("project", "proj-fact"));
    assert_eq!((row.23.as_str(), row.24.as_str()), ("agent-fact-w", "dev-fact-w"),
               "source_agent/device_id 只从 BatchCtx 来");
    assert_eq!(row.25, SRC_MS, "created_at_ms 取源轮时刻，不取导入钟");
    assert_eq!(row.26, rc_store::capability::NORM_VERSION as i64);

    // provenance：两条，turn 与角色各自对得上。
    let mut st = m.conn().prepare(
        "SELECT turn_uid, anchor_role FROM provenance WHERE mem_table='fact' AND mem_id=?1
          ORDER BY turn_uid").unwrap();
    let prov: Vec<(String, String)> = st
        .query_map([&expected_id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
        .collect::<Result<_, _>>().unwrap();
    assert_eq!(prov, vec![(uid(1), "human".to_string()), (uid(2), "agent".to_string())]);

    // memory_mention：subject 与 object 各一行，带 ctx 与源时刻。
    let mut st = m.conn().prepare(
        "SELECT anchor_id, source_agent, device_id, created_at_ms, norm_version
           FROM memory_mention WHERE mem_table='fact' AND mem_id=?1 ORDER BY anchor_id").unwrap();
    let mentions: Vec<(String, String, String, i64, i64)> = st
        .query_map([&expected_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        }).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(mentions.len(), 2);
    for (anchor_id, source_agent, device_id, created_at, norm) in mentions {
        assert!(anchor_id == anchor(1) || anchor_id == anchor(2));
        assert_eq!((source_agent.as_str(), device_id.as_str()), ("agent-fact-w", "dev-fact-w"));
        assert_eq!(created_at, SRC_MS);
        assert_eq!(norm, rc_store::capability::NORM_VERSION as i64);
    }

    // 最小消费卡：内容寻址、kind=category、title 截 30 字、指针指向 fact、verdict 不冒充已审。
    let title = card_title(&it.text);
    let card_id = rc_memory::compute_mem_id(
        rc_memory::MemTable::Card, &it.category, &title, &it.text, &it.scope);
    let card = m.conn().query_row(
        "SELECT kind, title, text, pointers_json, as_of_ms, verdict, mode, confidence,
                source_agent, device_id, created_at_ms, norm_version, time_json
           FROM card WHERE id=?1",
        [&card_id],
        |r| Ok((
            r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
            r.get::<_, String>(3)?, r.get::<_, i64>(4)?, r.get::<_, String>(5)?,
            r.get::<_, String>(6)?, r.get::<_, f64>(7)?, r.get::<_, String>(8)?,
            r.get::<_, String>(9)?, r.get::<_, i64>(10)?, r.get::<_, i64>(11)?,
            r.get::<_, Option<String>>(12)?,
        )),
    ).unwrap();
    assert_eq!(card.0, "promise");
    assert_eq!(card.1, title);
    assert!(card.1.chars().count() <= 30);
    assert_eq!(card.2, it.text);
    assert_eq!(card.3, format!("[\"fact:{expected_id}\"]"));
    assert_eq!(card.4, SRC_MS, "as_of 取源轮时刻");
    assert_eq!(card.5, "unreviewed", "审核未外置前不许冒充 passed");
    assert_eq!(card.6, "stated", "card.mode 走列的 DEFAULT");
    assert!((card.7 - 0.8).abs() < 1e-9, "card.confidence 走列的 DEFAULT");
    assert_eq!((card.8.as_str(), card.9.as_str()), ("agent-fact-w", "dev-fact-w"));
    assert_eq!(card.10, SRC_MS);
    assert_eq!(card.11, rc_store::capability::NORM_VERSION as i64);
    // T33：卡面时间在写卡这一刻是 NULL（这句短语还没解析、fact 的时间列也还是 NULL），
    // 回填由 `rc_distill::pass_time` 补——不拿源轮时刻冒充事件时刻。
    assert_eq!(card.12, None, "写卡路径不许硬造时间");

    // 归属：fact 与 card 各一行，owner 来自会话、created_by 是署名 agent、visibility 是上界。
    for (table, id) in [("fact", expected_id.as_str()), ("card", card_id.as_str())] {
        let o = m.conn().query_row(
            "SELECT owner_principal_id, owner_agent_id, created_by_agent, visibility, retention,
                    scope_kind, scope_key, source_agent, device_id, created_at_ms
               FROM resource_owner WHERE mem_table=?1 AND mem_id=?2",
            rusqlite::params![table, id],
            |r| Ok((
                r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
                r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?,
                r.get::<_, String>(6)?, r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?, r.get::<_, i64>(9)?,
            )),
        ).unwrap();
        assert_eq!(o.0.as_deref(), Some("p-fact"), "{table}");
        assert_eq!(o.1, "agent-owner");
        assert_eq!(o.2, "agent-fact-w", "created_by_agent 是署名 agent，不是 owner");
        assert_eq!(o.3, "project");
        assert_eq!(o.4, "keep");
        assert_eq!((o.5.as_str(), o.6.as_str()), ("project", "proj-fact"));
        assert_eq!(o.7.as_deref(), Some("agent-fact-w"));
        assert_eq!(o.8.as_deref(), Some("dev-fact-w"));
        assert_eq!(o.9, SRC_MS);
    }
}

#[test]
fn object_only_and_value_only_are_both_legal() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchors(m);

    let mut object_only = item();
    object_only.value = None;
    object_only.predicate = "plan".into();
    object_only.category = "plan".into();
    object_only.time_text = "下周".into();

    let mut value_only = item();
    value_only.object_anchor_id = None;
    value_only.category = "decision".into();

    let out = put_fact_rows(m, &[object_only, value_only], &ctx(), &owner(), NOW).unwrap();
    assert_eq!((out.facts, out.cards), (2, 2));
    // object-only 写 subject+object 两条 mention，value-only 只有 subject 一条。
    assert_eq!(out.mentions, 3);
    let null_values: i64 = m.conn().query_row(
        "SELECT count(*) FROM fact WHERE value_text IS NULL", [], |r| r.get(0)).unwrap();
    assert_eq!(null_values, 1, "value-only 那条的 object/value 一列空、一列非空");
    let null_objects: i64 = m.conn().query_row(
        "SELECT count(*) FROM fact WHERE object_anchor_id IS NULL", [], |r| r.get(0)).unwrap();
    assert_eq!(null_objects, 1);
    assert_eq!(count(m, "resource_owner"), 4);
}

#[test]
fn subject_equal_to_object_writes_one_mention_and_one_participant() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchors(m);
    let mut it = item();
    it.object_anchor_id = Some(it.subject_anchor_id.clone());

    let out = put_fact_rows(m, &[it], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(out.mentions, 1, "同一个锚不写两条 mention");
    assert_eq!(count(m, "memory_mention"), 1);
    let participants: String = m.conn().query_row(
        "SELECT participants_json FROM fact", [], |r| r.get(0)).unwrap();
    assert_eq!(participants, format!("[\"{}\"]", anchor(1)), "participants 去重");
}

// ── ④ 校验在事务前，坏一条整批不落 ────────────────────────────────────

#[test]
fn an_invalid_item_rejects_the_whole_batch_and_writes_nothing() {
    let mut bad_category = item();
    bad_category.category = "mood".into();
    let mut bad_mode = item();
    bad_mode.mode = "reported".into();
    let mut both_empty = item();
    both_empty.object_anchor_id = None;
    both_empty.value = None;
    let mut empty_evidence = item();
    empty_evidence.evidence.clear();
    let mut empty_subject = item();
    empty_subject.subject_anchor_id = "   ".into();
    let mut empty_object = item();
    empty_object.object_anchor_id = Some(String::new());
    let mut empty_predicate = item();
    empty_predicate.predicate = String::new();
    let mut empty_text = item();
    empty_text.text = "\r\n".into();
    let mut empty_uid = item();
    empty_uid.evidence = vec![(String::new(), AnchorRole::Human)];

    let cases: Vec<(&'static str, FactWriteItem)> = vec![
        ("category", bad_category), ("mode", bad_mode), ("value", both_empty),
        ("evidence", empty_evidence), ("subject", empty_subject), ("object", empty_object),
        ("predicate", empty_predicate), ("text", empty_text), ("evidence", empty_uid),
    ];

    let good = item();
    for (index, (field, bad)) in cases.into_iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let m = root.memory();
        seed_anchors(m);
        // 合法条在前：校验必须发生在任何一条写入之前，而不是"遇到坏条才停"。
        let err = put_fact_rows(m, &[good.clone(), bad], &ctx(), &owner(), NOW).unwrap_err();
        // 载荷是列名（`BadA1Field` 的复用理由见 `writers_fact::validate` 的注释）。
        match err {
            MemError::BadA1Field(got) => assert_eq!(got, field, "第 {index} 个坏例的字段名"),
            other => panic!("第 {index} 个坏例给错了错误：{other}"),
        }
        for table in ["fact", "card", "provenance", "memory_mention", "resource_owner"] {
            assert_eq!(count(m, table), 0, "第 {index} 个坏例落下了 {table} 行");
        }
    }
}

#[test]
fn every_ddl_category_is_accepted() {
    // 七值闭集：六个非本 family 的值也合法（写库层只认 DDL 闭集，不分 family）。
    assert_eq!(FACT_CATEGORIES, ["event", "promise", "plan", "decision", "observation",
                                 "state_change", "other"]);
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchors(m);
    let mut items = Vec::new();
    for (index, category) in FACT_CATEGORIES.iter().enumerate() {
        let mut it = item();
        it.category = (*category).to_string();
        it.text = format!("第 {index} 条：张三承诺下周把对账单发出去。");
        items.push(it);
    }
    let out = put_fact_rows(m, &items, &ctx(), &owner(), NOW).unwrap();
    assert_eq!(out.facts, FACT_CATEGORIES.len());
}

// ── ⑤ 幂等与归属 ──────────────────────────────────────────────────────

#[test]
fn replay_writes_no_new_rows_and_keeps_the_owner() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchors(m);
    let it = item();

    let first = put_fact_rows(m, std::slice::from_ref(&it), &ctx(), &owner(), NOW).unwrap();
    assert_eq!((first.facts, first.cards, first.mentions), (1, 1, 2));
    let counts_before: Vec<i64> = ["fact", "card", "provenance", "memory_mention", "resource_owner"]
        .iter().map(|t| count(m, t)).collect();

    // 重放：同一批同内容。计数必须全 0，行数不变。
    let second = put_fact_rows(m, std::slice::from_ref(&it), &ctx(), &owner(), NOW + 60_000).unwrap();
    assert_eq!((second.facts, second.cards, second.mentions), (0, 0, 0),
               "内容寻址 + OR IGNORE ⇒ 重放不写新行");
    let counts_after: Vec<i64> = ["fact", "card", "provenance", "memory_mention", "resource_owner"]
        .iter().map(|t| count(m, t)).collect();
    assert_eq!(counts_before, counts_after);

    // 换一个 owner 再写同一批：归属行"先写者赢"，不覆盖。
    let other = OwnerCtx {
        owner_principal_id: Some("p-other".into()),
        owner_agent_id: "agent-other".into(),
        visibility: Visibility::Public,
        retention: "days:7".into(),
    };
    let third = put_fact_rows(m, std::slice::from_ref(&it), &ctx(), &other, NOW + 120_000).unwrap();
    assert_eq!((third.facts, third.cards, third.mentions), (0, 0, 0));
    let owner_agent: String = m.conn().query_row(
        "SELECT owner_agent_id FROM resource_owner WHERE mem_table='fact'", [], |r| r.get(0)).unwrap();
    assert_eq!(owner_agent, "agent-owner", "已有归属不许被后写者改写");
    let owner_visibility: String = m.conn().query_row(
        "SELECT visibility FROM resource_owner WHERE mem_table='card'", [], |r| r.get(0)).unwrap();
    assert_eq!(owner_visibility, "project");
}

#[test]
fn visibility_is_capped_by_the_row_scope() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchors(m);

    // 会话声明 project，行 scope 是 person ⇒ 存 person（`owner::effective` 的上界）。
    let person_item = FactWriteItem {
        scope: Scope { kind: ScopeKind::Person, key: "u-fact".into() },
        text: "某项目承诺下周把某打印机搬到会议室。".into(),
        ..item()
    };
    // 会话声明 private，行 scope 是 project ⇒ 存 private_agent（会话是上界）。
    let private_owner = OwnerCtx {
        owner_principal_id: None,
        owner_agent_id: "agent-private".into(),
        visibility: Visibility::PrivateAgent,
        retention: "default".into(),
    };

    put_fact_rows(m, &[person_item], &ctx(), &owner(), NOW).unwrap();
    let capped: (String, String) = m.conn().query_row(
        "SELECT visibility, scope_kind FROM resource_owner
          WHERE mem_table='fact' AND scope_key='u-fact'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(capped, ("person".to_string(), "person".to_string()),
               "min(project, ceiling(person)) = person");

    let project_item = item();
    let project_id = compute_fact_id(
        &project_item.subject_anchor_id, &project_item.predicate,
        project_item.object_anchor_id.as_deref(), project_item.value.as_deref(),
        &project_item.category, &project_item.time_text, &project_item.text,
        &project_item.scope);
    put_fact_rows(m, &[project_item], &ctx(), &private_owner, NOW).unwrap();
    let narrowed: String = m.conn().query_row(
        "SELECT visibility FROM resource_owner WHERE mem_table='fact' AND mem_id=?1",
        [&project_id], |r| r.get(0)).unwrap();
    assert_eq!(narrowed, "private_agent", "会话可见性是上界，不许被行 scope 放宽");
}

#[test]
fn an_empty_batch_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchors(m);
    let out = put_fact_rows(m, &[], &ctx(), &owner(), NOW).unwrap();
    assert_eq!((out.facts, out.cards, out.mentions), (0, 0, 0));
    for table in ["fact", "card", "provenance", "memory_mention", "resource_owner"] {
        assert_eq!(count(m, table), 0);
    }
}

// ── ⑥ 事件附加字段（S2c-1）：participants / value_json / place ────────────────
//
// 这一层只加三列，不碰 id、幂等、归属、card：event 与 fact 落同一张表，差别只在
// 「参与者不止主客体」「value 不是一句动作文本」「有地点」。旧入口必须逐字不变。

/// 一条 event 形状的 item：category=state_change、value 是动作词。id 仍按内容寻址算。
fn event_item() -> FactWriteItem {
    FactWriteItem {
        subject_anchor_id: anchor(1),
        object_anchor_id: None,
        predicate: "restore_config".into(),
        value: Some("restore_config".into()),
        category: "state_change".into(),
        mode: "stated".into(),
        time_text: "昨晚".into(),
        quote: "我把配置改回去，服务就恢复了".into(),
        evidence: vec![(uid(1), AnchorRole::Human)],
        text: "张三昨晚把配置改回旧版本，服务恢复。".into(),
        scope: scope(),
        source_ms: SRC_MS,
    }
}

/// 1. extras 与 items 长度不符 ⇒ `MemError::Sql`，一行不写（不静默跳过）。
#[test]
fn extras_length_must_match_items() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchors(m);

    let items = [event_item()];
    for extras in [vec![], vec![FactExtras::default(), FactExtras::default()]] {
        let err = put_fact_rows_with_extras(m, &items, &extras, &ctx(), &owner(), NOW)
            .unwrap_err();
        match err {
            MemError::Sql(message) => assert!(message.contains("extras"), "载荷应说明是 extras 长度：{message}"),
            other => panic!("长度不符必须归 Sql，得到 {other}"),
        }
        for table in ["fact", "card", "provenance", "memory_mention", "resource_owner"] {
            assert_eq!(count(m, table), 0, "长度不符时落下了 {table} 行");
        }
    }

    // items 为空、extras 非空同样是长度不符。
    let err = put_fact_rows_with_extras(m, &[], &[FactExtras::default()], &ctx(), &owner(), NOW)
        .unwrap_err();
    assert!(matches!(err, MemError::Sql(_)));
    assert_eq!(count(m, "fact"), 0);
}

/// 2. participants 并入并去重（subject → object → extras 顺序，已存在的锚不重复）、
///    value_json 覆盖默认值、place 落列；value_text / mention 仍是旧入口那一份。
#[test]
fn extras_merge_participants_override_value_json_and_write_place() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchors(m);
    seed_place_anchor(m);

    let mut it = event_item();
    it.object_anchor_id = Some(anchor(2));
    let extras = FactExtras {
        // a1 是 subject：必须被去重；a3 是新参与者；a1 再写一次也不许重复。
        participants: vec![anchor(3), anchor(1), anchor(3)],
        value_json: Some("{\"action\":\"restore_config\",\"result\":\"done\"}".into()),
        place_anchor_id: Some(anchor(3)),
        value_demote: false,
    };
    let out = put_fact_rows_with_extras(m, &[it.clone()], &[extras], &ctx(), &owner(), NOW).unwrap();
    // 行/卡/提及计数与旧入口同形：extras 的参与者只进 participants_json。
    assert_eq!((out.facts, out.cards, out.mentions), (1, 1, 2));

    let id = compute_fact_id(
        &it.subject_anchor_id, &it.predicate, it.object_anchor_id.as_deref(),
        it.value.as_deref(), &it.category, &it.time_text, &it.text, &it.scope,
    );
    let row: (String, Option<String>, Option<String>, i64) = m.conn().query_row(
        "SELECT participants_json, value_json, place_anchor_id,
                (SELECT count(*) FROM memory_mention WHERE mem_table='fact' AND mem_id=?1)
           FROM fact WHERE id=?1",
        [&id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    ).unwrap();
    assert_eq!(
        row.0,
        format!("[\"{}\",\"{}\",\"{}\"]", anchor(1), anchor(2), anchor(3)),
        "participants = subject → object → extras，去重保序"
    );
    assert_eq!(row.1.as_deref(), Some("{\"action\":\"restore_config\",\"result\":\"done\"}"),
               "value_json 被 extras 覆盖");
    assert_eq!(row.2.as_deref(), Some(anchor(3).as_str()), "place 落列");
    assert_eq!(row.3, 2, "mention 仍只写 subject/object（与旧入口逐字一致）");

    // value_text 仍是 item.value：覆盖只发生在一列，不连带改文本。
    let value_text: Option<String> = m.conn().query_row(
        "SELECT value_text FROM fact WHERE id=?1", [&id], |r| r.get(0)).unwrap();
    assert_eq!(value_text.as_deref(), Some("restore_config"));

    // 归属与 card 与旧入口同一套：card 指针指回这条 fact。
    let card_id = rc_memory::compute_mem_id(
        rc_memory::MemTable::Card, &it.category, &card_title(&it.text), &it.text, &it.scope);
    let pointer: String = m.conn().query_row(
        "SELECT pointers_json FROM card WHERE id=?1", [&card_id], |r| r.get(0)).unwrap();
    assert_eq!(pointer, format!("[\"fact:{id}\"]"));
    assert_eq!(count(m, "resource_owner"), 2);
}

/// 3. 旧入口 = 全默认 extras 的那条路：同内容重放 0 行，列值与 `put_fact_rows` 逐列相同。
#[test]
fn old_entry_is_the_default_extras_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchors(m);

    let it = item();
    let first = put_fact_rows(m, std::slice::from_ref(&it), &ctx(), &owner(), NOW).unwrap();
    assert_eq!((first.facts, first.cards, first.mentions), (1, 1, 2));

    let default: Vec<FactExtras> = vec![FactExtras::default()];
    let second = put_fact_rows_with_extras(m, std::slice::from_ref(&it), &default,
                                           &ctx(), &owner(), NOW + 60_000).unwrap();
    assert_eq!((second.facts, second.cards, second.mentions), (0, 0, 0),
               "旧入口写过的行，默认 extras 重放必须仍是 0 行");

    let row: (String, Option<String>, Option<String>) = m.conn().query_row(
        "SELECT participants_json, value_json, place_anchor_id FROM fact", [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!(row.0, format!("[\"{}\",\"{}\"]", anchor(1), anchor(2)));
    assert_eq!(row.1.as_deref(), Some("\"把对账单发给李四\""), "默认仍是 value 的 JSON 字符串");
    assert!(row.2.is_none(), "默认 place 为空");
}

/// 4. extras 里的空串不静默写进列：参与者/覆盖值/地点都按坏字段拒，整批不落。
#[test]
fn blank_extras_fields_are_rejected_before_the_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchors(m);
    seed_place_anchor(m);

    let cases: Vec<(&'static str, FactExtras)> = vec![
        ("participants", FactExtras {
            participants: vec![anchor(3), "   ".into()], ..FactExtras::default() }),
        ("value_json", FactExtras {
            value_json: Some("\r\n".into()), ..FactExtras::default() }),
        ("place", FactExtras {
            place_anchor_id: Some(String::new()), ..FactExtras::default() }),
    ];
    for (index, (field, extras)) in cases.into_iter().enumerate() {
        let err = put_fact_rows_with_extras(m, &[event_item()], &[extras], &ctx(), &owner(), NOW)
            .unwrap_err();
        match err {
            MemError::BadA1Field(got) => assert_eq!(got, field, "第 {index} 个坏 extras 的字段名"),
            other => panic!("第 {index} 个坏 extras 给错了错误：{other}"),
        }
        for table in ["fact", "card", "provenance", "memory_mention", "resource_owner"] {
            assert_eq!(count(m, table), 0, "第 {index} 个坏 extras 落下了 {table} 行");
        }
    }
}

// ── ⑦ time_expr 的未解析行（S2d-1）────────────────────────────────────
//
// 写库层只做"记下这个短语"，**不猜**：start/end 留 NULL、precision=unknown、basis=none，
// 等 time pass 的确定性解析（或 S2d-2 的模型）回填。kind 是短语的意图分类，不依赖解析结果。

/// `(turn_uid, text, kind)` 三元组的内容寻址：稳定、任一格变化即新 id、字段边界不撞。
#[test]
fn time_expr_id_is_stable_and_sensitive_to_every_part() {
    let base = compute_time_expr_id(&uid(1), "下周三之前", "deadline");
    assert_eq!(base, compute_time_expr_id(&uid(1), "下周三之前", "deadline"), "同输入稳定");
    assert_eq!(base.len(), 64, "句柄是 64 位小写 hex");
    assert!(base.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));

    assert_ne!(base, compute_time_expr_id(&uid(2), "下周三之前", "deadline"), "换 turn 即新 id");
    assert_ne!(base, compute_time_expr_id(&uid(1), "下周三", "deadline"), "换 text 即新 id");
    assert_ne!(base, compute_time_expr_id(&uid(1), "下周三之前", "validity"), "换 kind 即新 id");

    // normalize_body 那把尺子：CRLF 与首尾空白不造"同一条短语的两个 id"。
    assert_eq!(compute_time_expr_id(&uid(1), " 下周三之前\r\n", "deadline"), base);
}

#[test]
fn time_expr_id_does_not_collide_across_field_boundaries() {
    // 段间 0u8 分隔的牙：无分隔时 ("ab","c") 与 ("a","bc") 都拼成 "abc"。
    assert_ne!(compute_time_expr_id("a", "bc", "d"), compute_time_expr_id("ab", "c", "d"));
    assert_ne!(
        compute_time_expr_id("a", "b", "cd"),
        compute_time_expr_id("a", "bc", "d"),
        "text/kind 的边界必须由分隔保护",
    );
}

/// `time_kind_for`：承诺类的截止短语是 deadline，事件三类是 event_time，其余是 validity。
/// 不认类别（写库层的 `validate` 已经拒过）不在本函数处理，落 validity。
#[test]
fn time_kind_follows_category_and_deadline_markers() {
    for category in ["promise", "plan", "decision"] {
        assert_eq!(time_kind_for(category, "下周三之前"), "deadline");
        assert_eq!(time_kind_for(category, "本月底以前"), "deadline");
        assert_eq!(time_kind_for(category, "10 月 31 日截止"), "deadline");
        assert_eq!(time_kind_for(category, "下周三"), "validity", "无截止词 ⇒ validity");
    }
    for category in ["event", "state_change", "observation"] {
        assert_eq!(time_kind_for(category, "昨晚"), "event_time");
        assert_eq!(time_kind_for(category, "下周三之前"), "event_time", "事件不因截止词改类");
    }
    assert_eq!(time_kind_for("other", "下周三之前"), "validity", "残值兜底");
    assert_eq!(time_kind_for("other", ""), "validity");
}

/// 一条 promise（time_text=下周三之前）⇒ 一条未解析的 `time_expr`：kind=deadline、
/// turn 取第一条证据、created_at_ms 取源轮时刻、start/end/precision/basis 都留"没解析"。
#[test]
fn fact_with_time_text_writes_one_unresolved_time_expr() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchors(m);
    let it = item();

    let out = put_fact_rows(m, std::slice::from_ref(&it), &ctx(), &owner(), NOW).unwrap();
    assert_eq!(out.facts, 1);
    assert_eq!(count(m, "time_expr"), 1, "time_text 非空 ⇒ 一条未解析行");

    let expected = compute_time_expr_id(&it.evidence[0].0, &it.time_text, "deadline");
    let row = m.conn().query_row(
        "SELECT id, turn_uid, text, start_ms, end_ms, precision, basis, kind, created_at_ms
           FROM time_expr WHERE id=?1",
        [&expected],
        |r| Ok((
            r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
            r.get::<_, Option<i64>>(3)?, r.get::<_, Option<i64>>(4)?,
            r.get::<_, String>(5)?, r.get::<_, String>(6)?, r.get::<_, String>(7)?,
            r.get::<_, i64>(8)?,
        )),
    ).unwrap();
    assert_eq!(row.0, expected);
    assert_eq!(row.1, it.evidence[0].0, "turn_uid 取第一条证据");
    assert_eq!(row.2, it.time_text, "text 原样存短语");
    assert!(row.3.is_none(), "不猜 start");
    assert!(row.4.is_none(), "不猜 end");
    assert_eq!(row.5, "unknown", "未解析 ⇒ precision=unknown");
    assert_eq!(row.6, "none", "未解析 ⇒ basis=none");
    assert_eq!(row.7, "deadline");
    assert_eq!(row.8, SRC_MS, "created_at_ms 取源轮时刻（解析的参照钟）");
}

/// event 形状的 item（category=state_change）⇒ kind=event_time；与 fact 的 deadline 同一张表。
#[test]
fn event_item_writes_event_time_kind() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchors(m);
    seed_place_anchor(m);
    let mut it = event_item();
    it.time_text = "昨晚".into();

    put_fact_rows(m, std::slice::from_ref(&it), &ctx(), &owner(), NOW).unwrap();
    let kind: String = m.conn().query_row("SELECT kind FROM time_expr", [], |r| r.get(0)).unwrap();
    assert_eq!(kind, "event_time");
    let text: String = m.conn().query_row("SELECT text FROM time_expr", [], |r| r.get(0)).unwrap();
    assert_eq!(text, "昨晚");
}

/// `time_text` 为空（`None` 语义的两种写法：空串与全空白）⇒ 零行 `time_expr`。
#[test]
fn empty_time_text_writes_no_time_expr() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchors(m);

    let mut first = item();
    first.time_text = String::new();
    first.text = "第一条：张三承诺把对账单发出去。".into();
    let mut second = item();
    second.time_text = "   \r\n".into();
    second.text = "第二条：张三承诺把对账单发出去。".into();
    let out = put_fact_rows(m, &[first, second], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(out.facts, 2, "两条 fact 各自成立");
    assert_eq!(count(m, "time_expr"), 0, "没有短语就没有时间行");
}

/// 同一批重放 ⇒ `time_expr` 行数不变（内容寻址 + `INSERT OR IGNORE`）；
/// 同一 turn 上两条不同短语 ⇒ 两行，不互相顶掉。
#[test]
fn replay_keeps_one_time_expr_per_phrase_and_turn() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchors(m);
    let it = item();

    put_fact_rows(m, std::slice::from_ref(&it), &ctx(), &owner(), NOW).unwrap();
    assert_eq!(count(m, "time_expr"), 1);
    let again = put_fact_rows(m, std::slice::from_ref(&it), &ctx(), &owner(), NOW + 60_000).unwrap();
    assert_eq!(again.facts, 0);
    assert_eq!(count(m, "time_expr"), 1, "重放不造第二行");

    // 同一 turn（同一证据轮）、不同短语 ⇒ 另一条时间行（两条 fact 各自的时间说法）。
    let mut other = item();
    other.time_text = "本月底之前".into();
    other.text = "张三承诺在本月底之前把对账单发给李四。".into();
    put_fact_rows(m, std::slice::from_ref(&other), &ctx(), &owner(), NOW).unwrap();
    assert_eq!(count(m, "time_expr"), 2, "同 turn 的两个短语是两行");
    let turns: i64 = m.conn().query_row(
        "SELECT count(DISTINCT turn_uid) FROM time_expr", [], |r| r.get(0)).unwrap();
    assert_eq!(turns, 1, "两行都挂在第一条证据轮上");
}

// ── ⑧ 时间短语谓词（S2e-5）──────────────────────────────────────────────
//
// 真实冒烟（`design/2026-10-26-dogfood-a-smoke.md` 发现 5）里，`再`/`先`/`下一格` 被写进
// `time_expr`。写库层只认"像时间表达"的短语：含钟面/ISO 日期，或含一个时间词；否则不落行。
// C2 起"含 ASCII 数字"不再单独算数：`H7c` 里也有数字，但它是批次编号。

/// 表驱动：钟面/ISO 与时间词是入口，缺入口的短语（`再`/`先`/`下一格`/`回头`/`H7c`）不是时间。
#[test]
fn is_time_phrase_recognises_clocks_and_time_words_only() {
    for phrase in [
        "下周三之前", "昨晚", "3天后", "下个月", "今天", "2026-10-26", "最近", "年底",
        "下月起", "10 月 31 日截止", "之前提过", "20:00 前", "十点半", "2026-10-27T08:04+08:00",
    ] {
        assert!(is_time_phrase(phrase), "该认的没认：{phrase:?}");
    }
    for phrase in ["", "   ", "\r\n", "再", "先", "下一格", "回头", "等迁移完"] {
        assert!(!is_time_phrase(phrase), "不该认的认了：{phrase:?}");
    }
    // C2：里程碑/批次编号没有日历锚，不建行（`H7c` 曾经因"含数字"被误收，gold 第 20 条）。
    for phrase in [
        "H7c", "H5", "H7b", "H5 之前", "H7b 前", "再进 H1c", "然后进 H2",
        "先核 commit 与新增用例，再进 H1c",
    ] {
        assert!(!is_time_phrase(phrase), "批次编号不是时间：{phrase:?}");
    }
    // `那阵子` 是含糊但确实是时间的说法（time pass 仍解析不出，行留在 `basis='none'`）。
    assert!(is_time_phrase("那阵子"), "含糊的时间说法不该被当成非时间");
}

/// `再`/`先`/`下一格` 的 fact 照写，但**不落** `time_expr`：审计表不再被语气词污染。
#[test]
fn non_time_phrases_write_the_fact_without_a_time_expr() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchors(m);

    let mut items = Vec::new();
    for (index, phrase) in ["再", "先", "下一格"].iter().enumerate() {
        let mut it = item();
        it.time_text = (*phrase).to_string();
        it.text = format!("第 {index} 条：张三把对账单发给李四。");
        items.push(it);
    }
    let out = put_fact_rows(m, &items, &ctx(), &owner(), NOW).unwrap();
    assert_eq!(out.facts, 3, "fact 本身仍然成立");
    assert_eq!(count(m, "time_expr"), 0, "不是时间短语就不落时间行");
}

/// 三个入口各一条：截止短语、事件短语、数字短语都仍是一条未解析的 `time_expr`。
#[test]
fn time_phrases_still_write_one_unresolved_time_expr_each() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchors(m);

    let mut items = Vec::new();
    for (index, phrase) in ["下周三之前", "昨晚", "3天后"].iter().enumerate() {
        let mut it = item();
        it.time_text = (*phrase).to_string();
        it.text = format!("第 {index} 条：张三把对账单发给李四。");
        items.push(it);
    }
    let out = put_fact_rows(m, &items, &ctx(), &owner(), NOW).unwrap();
    assert_eq!(out.facts, 3);
    assert_eq!(count(m, "time_expr"), 3, "三个短语各一行");
    let unresolved: i64 = m
        .conn()
        .query_row("SELECT count(*) FROM time_expr WHERE basis='none'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(unresolved, 3, "写库层只记短语，不解析");
}

