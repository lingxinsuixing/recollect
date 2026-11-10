use rc_derive::derive::derive_pass;
use rc_llm::backend::{LlmBackend, LlmFailure, Prompt};
use rc_memory::{keys::{MemTable, Scope, ScopeKind, AnchorRole},
                writers_x::{put_timeline_rows, BatchCtx, TimelineContent},
                RowFacts, put_card, put_event, put_entity, EntityContent, FactContent, MemKey,
                Provenance, reader};
use rc_store::Root;

fn scope() -> Scope { Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() } }
fn prov() -> Vec<Provenance> { vec![Provenance { turn_uid: "t".repeat(64), anchor_role: AnchorRole::Human }] }
// 本文件**不**造 `RowFacts` 字面量夹具：四条测试都从真库 `reader::lookup` 取行
// （`derive_pass` 要验的是"真读数进、真库行出"，手搓的 RowFacts 会让"选批读出来的行"
// 这一环没有读者——它是 T3 的 `prompt_shape.rs` 里那一处字面量的用途，这里不需要第二份）。
fn card_c(title: &str, body: &str) -> FactContent {
    FactContent { kind: "decision".into(), title: title.into(), body: body.into(),
                  scope: scope(), valid_to_ms: None }
}

/// I4 的锚夹具：一个真实体，正文里逐字提到它 ⇒ derive 的别名补铸会先铸出两端的
/// mention，`part_of` 的"共享 mention"判据因此过门槛。
fn shared_entity(m: &rc_store::MemoryHandle, name: &str, at: i64) -> String {
    put_entity(m, &EntityContent {
        kind: "concept".into(), title: name.into(), scope: scope(), aliases: vec![],
    }, &prov(), at).unwrap().id
}

/// 脚本化后端：按调用顺序消费预制回复；耗尽即 SchemaMismatch。
struct Scripted { replies: Vec<String>, i: usize }
impl LlmBackend for Scripted {
    fn complete(&self, _p: &Prompt) -> Result<String, LlmFailure> {
        self.replies.get(self.i).cloned()
            .ok_or(LlmFailure::SchemaMismatch)
    }
}

#[test]
fn happy_path_writes_one_edge_and_one_timeline_row_idempotently() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    // I4：两端共享一个真实体（`Postgres`）⇒ 别名补铸先落两条 mention，part_of 才有锚。
    shared_entity(root.memory(), "Postgres", 900);
    let k1 = put_card(root.memory(), &card_c("ship behind a flag", "Until the migration finishes. Postgres stays."),
                      &prov(), 1000).unwrap();
    let k2 = put_card(root.memory(), &card_c("flagged rollout", "Same plan reworded; Postgres first."),
                      &prov(), 2000).unwrap();
    let rows: Vec<RowFacts> = vec![
        reader::lookup(root.memory(), &k1).unwrap().unwrap(),
        reader::lookup(root.memory(), &k2).unwrap().unwrap(),
    ];
    let reply = serde_json::json!({
        "relationships": [
            {"from":"r1","to":"r2","kind":"part_of","reason":"r2 says same plan","basis":["r1","r2"]}
        ],
        "chains": [],
        "timeline": [
            {"anchor":"r1","offset_ms":86400000,"body_ref":"ship behind a flag","basis":["r1"]}
        ]
    }).to_string();
    let backend = Scripted { replies: vec![reply], i: 0 };
    let out = derive_pass(root.memory(), &rows, &backend, 9999).unwrap();
    // 1 边 + 1 timeline 行 + 1 条时间线旁路 fact_link（锚到消费行）+ 2 条别名 mention = 5
    assert_eq!(out.rows_written, 5, "边 + timeline + 旁路 fact_link + 两条补铸 mention");
    assert_eq!(out.rejected, 0);
    // 重跑同批 ⇒ 零新增（幂等 = 内容寻址 id + `INSERT OR IGNORE`）
    let backend2 = Scripted { replies: vec![serde_json::json!({
        "relationships":[{"from":"r1","to":"r2","kind":"part_of","reason":"x","basis":["r1"]}],
        "chains":[],"timeline":[]
    }).to_string()], i: 0 };
    let out2 = derive_pass(root.memory(), &rows, &backend2, 10000).unwrap();
    assert_eq!(out2.rows_written, 0, "同 (from, kind, to) 重写零新增");
    assert_eq!(out2.rejected, 0);
}

#[test]
fn relative_offset_with_anchor_sets_time_anchor_ms_relative_anchored() {
    // 有锚（"yesterday" + 锚行 r1 的钟=1000 + offset_ms=86400000 ⇒ ms=86401000）⇒ confidence=anchored。
    // 这一行的 `as_of_ms` 与 `created_at_ms` 同值（`put_card` 把两者都落成源轮时刻），
    // 所以本条同时是"回落 `as_of_ms`"那一支的既有读数；两钟分岔的两支在下面两条用例里。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k1 = put_card(root.memory(), &card_c("anchor row", "yesterday we said x"),
                      &prov(), 1000).unwrap();
    let k2 = put_card(root.memory(), &card_c("dependent row", "depends on yesterday's x"),
                      &prov(), 2000).unwrap();
    let rows: Vec<RowFacts> = vec![
        reader::lookup(root.memory(), &k1).unwrap().unwrap(),
        reader::lookup(root.memory(), &k2).unwrap().unwrap(),
    ];
    let reply = serde_json::json!({
        "relationships":[],
        "chains":[],
        "timeline":[{"anchor":"r1","offset_ms":86400000,"body_ref":"anchor row","basis":["r1","r2"]}]
    }).to_string();
    let out = derive_pass(root.memory(), &rows, &Scripted { replies: vec![reply], i: 0 }, 9999).unwrap();
    assert_eq!(out.rows_written, 2, "时间行 + 它那条旁路 fact_link（锚到消费行）");
    // time_anchor_ms = 1000 + 86400000 = 86401000；time_anchor_id = r1.id；time_confidence = 'anchored'
    let row: (i64, String, String) = root.memory().conn().query_row(
        "SELECT time_anchor_ms, time_anchor_id, time_confidence FROM card
         WHERE id = (SELECT id FROM card WHERE kind='timeline' ORDER BY created_at_ms DESC LIMIT 1)",
        [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!(row.0, 1000 + 86400000);
    assert_eq!(row.1, k1.id);
    assert_eq!(row.2, "anchored");
}

/// 回读最后一条时间线卡的 `(time_anchor_ms, time_anchor_id, time_confidence)`。
fn last_timeline(root: &Root) -> (Option<i64>, Option<String>, String) {
    root.memory().conn().query_row(
        "SELECT time_anchor_ms, time_anchor_id, time_confidence FROM card
         WHERE id = (SELECT id FROM card WHERE kind='timeline' ORDER BY created_at_ms DESC LIMIT 1)",
        [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap()
}

/// 夹具：把一张卡的两个钟改成分岔的值（`put_card` 只会把 as_of/created_at 落成同一个源轮时刻，
/// 而这一族要验的正是"两个钟不同时读哪一个"）。id 与毫秒都是本文件自己造的值，
/// 直接拼进语句（没有外部输入；测试壳不引 `rusqlite` 这个依赖名）。
fn set_clocks(root: &Root, id: &str, set: &str) {
    root.memory().conn()
        .execute(&format!("UPDATE card SET {set} WHERE id='{id}'"), [])
        .unwrap();
}

/// C5：锚行的 `time_json` 事件时间在场 ⇒ 时间线的锚钟是**它**（不是 `as_of_ms`，更不是入库钟）。
/// 夹具把三个钟刻意拉开一天：`created_at_ms=1000`、`as_of_ms=2000`、事件时间=90000000。
#[test]
fn relative_offset_anchors_on_the_event_time_when_the_anchor_row_has_one() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k1 = put_card(root.memory(), &card_c("anchor row", "yesterday we said x"),
                      &prov(), 1000).unwrap();
    set_clocks(&root, &k1.id,
        r#"as_of_ms=2000, time_json='{"kind":"event_time","field":"occurred","start_ms":90000000,"end_ms":null,"precision":"day","basis":"explicit"}'"#);
    let k2 = put_card(root.memory(), &card_c("dependent row", "depends on yesterday's x"),
                      &prov(), 2000).unwrap();
    let rows: Vec<RowFacts> = vec![
        reader::lookup(root.memory(), &k1).unwrap().unwrap(),
        reader::lookup(root.memory(), &k2).unwrap().unwrap(),
    ];
    assert_eq!(rows[0].as_of_ms, 2000, "夹具先自检：as_of 读回来就是 2000");
    let reply = serde_json::json!({
        "relationships":[], "chains":[],
        "timeline":[{"anchor":"r1","offset_ms":1000,"body_ref":"anchor row","basis":["r1"]}]
    }).to_string();
    derive_pass(root.memory(), &rows, &Scripted { replies: vec![reply], i: 0 }, 9999).unwrap();
    let (ms, anchor_id, conf) = last_timeline(&root);
    assert_eq!(ms, Some(90000000 + 1000), "锚钟是事件时间，不是 as_of(2000)、不是 created_at(1000)");
    assert_eq!(anchor_id.as_deref(), Some(k1.id.as_str()));
    assert_eq!(conf, "anchored");
}

/// C5：锚行没有事件时间 ⇒ 回落**该行的 `as_of_ms`**；`created_at_ms`（入库钟）不许再被读。
#[test]
fn relative_offset_falls_back_to_as_of_and_never_to_created_at() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k1 = put_card(root.memory(), &card_c("anchor row", "yesterday we said x"),
                      &prov(), 1000).unwrap();
    // 只有 as_of 被改写，created_at 仍是 1000；没有 time_json。
    set_clocks(&root, &k1.id, "as_of_ms=5000");
    let k2 = put_card(root.memory(), &card_c("dependent row", "depends on yesterday's x"),
                      &prov(), 2000).unwrap();
    let rows: Vec<RowFacts> = vec![
        reader::lookup(root.memory(), &k1).unwrap().unwrap(),
        reader::lookup(root.memory(), &k2).unwrap().unwrap(),
    ];
    assert_eq!((rows[0].as_of_ms, rows[0].created_at_ms), (5000, 1000), "夹具先自检：两钟分岔");
    let reply = serde_json::json!({
        "relationships":[], "chains":[],
        "timeline":[{"anchor":"r1","offset_ms":1000,"body_ref":"anchor row","basis":["r1"]}]
    }).to_string();
    derive_pass(root.memory(), &rows, &Scripted { replies: vec![reply], i: 0 }, 9999).unwrap();
    let (ms, _, conf) = last_timeline(&root);
    assert_eq!(ms, Some(5000 + 1000), "回落 as_of(5000)，不许回落 created_at(1000)");
    assert_eq!(conf, "anchored");
}

/// C5 的阴性对照：锚行**两个钟都没有**（`as_of_ms` 是读侧"这一行没有这一列"的 0）⇒
/// 时间线 `time_anchor_ms=NULL` + `time_confidence='unknown'`，**不许拿 `created_at_ms` 造一个**。
#[test]
fn relative_offset_without_any_row_clock_yields_unknown_with_null_ms() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k1 = put_card(root.memory(), &card_c("anchor row", "yesterday we said x"),
                      &prov(), 1000).unwrap();
    set_clocks(&root, &k1.id, "as_of_ms=0");
    let k2 = put_card(root.memory(), &card_c("dependent row", "depends on yesterday's x"),
                      &prov(), 2000).unwrap();
    let rows: Vec<RowFacts> = vec![
        reader::lookup(root.memory(), &k1).unwrap().unwrap(),
        reader::lookup(root.memory(), &k2).unwrap().unwrap(),
    ];
    let reply = serde_json::json!({
        "relationships":[], "chains":[],
        "timeline":[{"anchor":"r1","offset_ms":1000,"body_ref":"anchor row","basis":["r1"]}]
    }).to_string();
    derive_pass(root.memory(), &rows, &Scripted { replies: vec![reply], i: 0 }, 9999).unwrap();
    let (ms, anchor_id, conf) = last_timeline(&root);
    assert_eq!(ms, None, "没有钟 ⇒ NULL，created_at(1000) 不许顶上来");
    assert_eq!(anchor_id.as_deref(), Some(k1.id.as_str()), "锚身份还在，只有时刻未知");
    assert_eq!(conf, "unknown");
}

#[test]
fn no_anchor_must_yield_unknown_with_null_ms_not_guess() {
    // 无锚 + mock 给具体 ms ⇒ 必须落 NULL（猜日期即红）
    // 注：mock 给 offset_ms 是另一回事；这条验的是"无 anchor 字段 ⇒ 不写 anchor"
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k1 = put_card(root.memory(), &card_c("r1", "x"), &prov(), 1000).unwrap();
    let k2 = put_card(root.memory(), &card_c("r2", "y"), &prov(), 2000).unwrap();
    let rows = vec![
        reader::lookup(root.memory(), &k1).unwrap().unwrap(),
        reader::lookup(root.memory(), &k2).unwrap().unwrap(),
    ];
    // anchor 字段不在 ⇒ parse 阶段 anchor_id=None；写入段不允许兜底当前时间
    let reply = serde_json::json!({
        "relationships":[],
        "chains":[],
        "timeline":[{"offset_ms":12345,"body_ref":"x","basis":["r1"]}]  // 没 anchor
    }).to_string();
    let out = derive_pass(root.memory(), &rows, &Scripted { replies: vec![reply], i: 0 }, 9999).unwrap();
    assert_eq!(out.rows_written, 1);
    let row: (Option<i64>, Option<String>, String) = root.memory().conn().query_row(
        "SELECT time_anchor_ms, time_anchor_id, time_confidence FROM card
         WHERE id = (SELECT id FROM card WHERE kind='timeline' ORDER BY created_at_ms DESC LIMIT 1)",
        [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert!(row.0.is_none(), "无锚必须 NULL ms，不能猜当前时间");
    assert!(row.1.is_none());
    assert_eq!(row.2, "unknown");
}

#[test]
fn anchor_role_takes_strongest_value_human_beats_agent_beats_system() {
    // 多依据取最强：Human + System ⇒ Human（裁定 11）
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    shared_entity(root.memory(), "共同实体", 900);   // I4：共享 mention 的锚
    let k1 = put_card(root.memory(), &card_c("r1", "共同实体 x"),
        &[Provenance { turn_uid: "h".repeat(64), anchor_role: AnchorRole::Human }], 1000).unwrap();
    let k2 = put_card(root.memory(), &card_c("r2", "共同实体 y"),
        &[Provenance { turn_uid: "s".repeat(64), anchor_role: AnchorRole::System }], 2000).unwrap();
    let rows = vec![
        reader::lookup(root.memory(), &k1).unwrap().unwrap(),
        reader::lookup(root.memory(), &k2).unwrap().unwrap(),
    ];
    let reply = serde_json::json!({
        "relationships":[{"from":"r1","to":"r2","kind":"part_of","reason":"x","basis":["r1","r2"]}],
        "chains":[],"timeline":[]
    }).to_string();
    let out = derive_pass(root.memory(), &rows, &Scripted { replies: vec![reply], i: 0 }, 9999).unwrap();
    assert_eq!(out.rows_written, 3, "边 + 两端各一条别名 mention");
    let role: String = root.memory().conn().query_row(
        "SELECT anchor_role FROM provenance
         WHERE mem_table='fact_link' AND turn_uid=?1",
        [&k1.id], |r| r.get(0)).unwrap();
    assert_eq!(role, "human", "Human + System ⇒ Human（最强）");
}

#[test]
fn zero_provenance_row_may_be_an_edge_endpoint_without_killing_the_batch() {
    // I-1 止血：`anchor_role_of` 对零出处行降级为 `System`，**不许**炸整批。
    // **这条路径今天已不可达**（T6b 断根：推导行不进推导批），但止血与断根是两笔账——
    // 断根让形状不可达，止血保证它一旦出现（历史库 / 手搓批 / 将来别处的调用者）也不炸整批。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    shared_entity(root.memory(), "共同实体", 900);   // I4：零出处端点也要有共享 mention
    let k1 = put_card(root.memory(), &card_c("r1", "共同实体 x"),
        &[Provenance { turn_uid: "h".repeat(64), anchor_role: AnchorRole::Human }], 1000).unwrap();
    let rows = vec![reader::lookup(root.memory(), &k1).unwrap().unwrap()];
    let first = serde_json::json!({
        "relationships":[], "chains":[],
        "timeline":[{"anchor":"r99","offset_ms":1000,"body_ref":"无锚的一条 共同实体","basis":["r1"]}]
    }).to_string();
    let out1 = derive_pass(root.memory(), &rows, &Scripted { replies: vec![first], i: 0 }, 3000).unwrap();
    assert_eq!(out1.rows_written, 2, "无锚 timeline 行照落 + 别名补铸一条 mention（裁定 13/I6）");
    let tl_id: String = root.memory().conn().query_row(
        "SELECT id FROM card WHERE kind='timeline' ORDER BY created_at_ms DESC LIMIT 1",
        [], |r| r.get(0)).unwrap();
    let n_prov: i64 = root.memory().conn().query_row(
        "SELECT COUNT(*) FROM provenance WHERE mem_table='card' AND mem_id=?1",
        [&tl_id], |r| r.get(0)).unwrap();
    assert_eq!(n_prov, 0, "无锚 ⇒ provenance 零行（这是被钉住的形状，不是缺陷）");

    // 第二轮：零出处行作边端点。旧形状在这里 `anchor_role_of` 返 `QueryReturnedNoRows`
    // ⇒ 整批回滚、`derive_pass` 返 `Err`、T5 不推水位 ⇒ cycle 卡死。
    assert!(!reader::rows_since(root.memory(), None, 50).unwrap()
        .iter().any(|r| r.key.id == tl_id),
        "断根后推导行不再被 rows_since 喂回批里——这一格是 T6b 断的那个根");
    // canonical `fact_link` 有 `CHECK(from_id <> to_id)` ⇒ 自环边再也写不进库（`OR IGNORE` 会把它
    // 静默吞成 0 行）。"两端都是零出处行"的形状因此必须用**两条不同的推导行**来立：第二条经
    // 真写入路径 `put_timeline_rows` 落库，同样零 provenance、同样有共享 mention。
    put_timeline_rows(root.memory(), &[TimelineContent {
        title: "derive-无锚的第二条".into(), body: "无锚的第二条 共同实体".into(),
        scope: scope(), time_anchor_ms: None, time_anchor_id: None,
        time_confidence: "unknown".into(), basis_ids: vec![],
    }], &BatchCtx { source_agent: "rc-derive".into(), device_id: "unknown".into() }, 3500).unwrap();
    let tl2_id: String = root.memory().conn().query_row(
        "SELECT id FROM card WHERE kind='timeline' AND text='无锚的第二条 共同实体'",
        [], |r| r.get(0)).unwrap();
    let n_prov2: i64 = root.memory().conn().query_row(
        "SELECT COUNT(*) FROM provenance WHERE mem_table='card' AND mem_id=?1",
        [&tl2_id], |r| r.get(0)).unwrap();
    assert_eq!(n_prov2, 0, "第二条推导行同样零出处：两条端点都落在降级支上");
    // 下面的批**显式**含那两条推导行：手搓这一批等于"有个调用者不管过滤"，
    // 而本条要验的是 `anchor_role_of` 的降级行为——它与"该形状可不可达"是两件事。
    let all: Vec<RowFacts> = vec![
        rows[0].clone(),
        reader::lookup(root.memory(),
            &MemKey { table: MemTable::Card, id: tl_id.clone() }).unwrap().unwrap(),
        reader::lookup(root.memory(),
            &MemKey { table: MemTable::Card, id: tl2_id.clone() }).unwrap().unwrap(),
    ];
    // 批内位置别名按批的顺序给：human card 是 r1，两条推导行是 r2/r3。
    let alias_of = |id: &str| if id == rows[0].key.id { "r1" }
                              else if id == tl_id { "r2" } else { "r3" };
    let second = serde_json::json!({
        "relationships":[{"from":alias_of(&tl_id),"to":alias_of(&tl2_id),
                          "kind":"part_of","reason":"第二轮","basis":["r2"]}],
        "chains":[], "timeline":[]
    }).to_string();
    let out2 = derive_pass(root.memory(), &all, &Scripted { replies: vec![second], i: 0 }, 5000);
    let out2 = out2.expect("零出处行作端点不许炸整批：旧形状在这里返 Err（T5 据此不推水位、反复重试）");
    assert_eq!(out2.rows_written, 1);
    let role: String = root.memory().conn().query_row(
        "SELECT anchor_role FROM provenance
         WHERE mem_table='fact_link' AND turn_uid=?1 ORDER BY rowid DESC LIMIT 1",
        [&tl_id], |r| r.get(0)).unwrap();
    assert_eq!(role, "system", "**两端都是零出处行** ⇒ 这一格就是降级值本身");
}

#[test]
fn mixed_endpoint_edge_takes_the_strongest_side_and_the_zero_side_stays_visible() {
    // 生产里的常见形状是**混合端点**（真人 card ↔ 零出处的推导行），不是上一条那种自环。
    // 在这一支里 `strongest(System, Human) = Human` ⇒ 边那一格的 `anchor_role` 是 `human`，
    // **降级的"最弱"被合成抵消了**（`anchor_role_of` 的承诺只兑现到函数边界）。
    // 这不是缺陷，是裁定 11 的推论；它是 T5 接线前必须裁的那一支（E25 的 O-1 同族）。
    // 本条把**两件必须被看见的事**钉在库上：边那一格取的是最强端，而被抵消的那一端
    // 没有从库里消失——它自己的 provenance 行与 `ref_kind` 仍指得回它（canonical 之后两端同落 card）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    shared_entity(root.memory(), "共同实体", 900);   // I4：混排端点同样要有共享 mention
    let k1 = put_card(root.memory(), &card_c("r1", "共同实体 真人说的"),
        &[Provenance { turn_uid: "h".repeat(64), anchor_role: AnchorRole::Human }], 1000).unwrap();
    let rows = vec![reader::lookup(root.memory(), &k1).unwrap().unwrap()];
    let first = serde_json::json!({
        "relationships":[], "chains":[],
        "timeline":[{"anchor":"r99","offset_ms":1000,"body_ref":"混排夹具 共同实体","basis":["r1"]}]
    }).to_string();
    derive_pass(root.memory(), &rows, &Scripted { replies: vec![first], i: 0 }, 3000).unwrap();
    let tl_id: String = root.memory().conn().query_row(
        "SELECT id FROM card WHERE kind='timeline' ORDER BY created_at_ms DESC LIMIT 1",
        [], |r| r.get(0)).unwrap();
    let all: Vec<RowFacts> = vec![
        rows[0].clone(),
        reader::lookup(root.memory(),
            &MemKey { table: MemTable::Card, id: tl_id.clone() }).unwrap().unwrap(),
    ];
    // 批内位置别名按批的顺序给：card 在 1000ms、推导行在 3000ms ⇒ `r1`/`r2`
    let idx = |id: &str| if all[0].key.id == id { "r1" } else { "r2" };
    let (zero_side, human_side) = (idx(&tl_id), idx(&k1.id));
    let second = serde_json::json!({
        "relationships":[{"from":zero_side,"to":human_side,"kind":"part_of","reason":"混排","basis":["r1"]}],
        "chains":[], "timeline":[]
    }).to_string();
    let out = derive_pass(root.memory(), &all, &Scripted { replies: vec![second], i: 0 }, 5000)
        .expect("混合端点同样不许炸整批");
    assert_eq!(out.rows_written, 1);
    let got: Vec<(String, String, String)> = {
        let mut st = root.memory().conn().prepare(
            "SELECT turn_uid, ref_kind, anchor_role FROM provenance
             WHERE mem_table='fact_link' ORDER BY turn_uid").unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
          .unwrap().map(|x| x.unwrap()).collect()
    };
    assert_eq!(got.len(), 2, "边的两端各一行出处：零出处端没有被『最强端合成』吞掉");
    assert!(got.iter().any(|(uid, _, _)| uid == &tl_id),
        "零出处那一端仍然指得回推导行（canonical 之后它与消费行同落 card，不再有 event/card 分表）");
    assert!(got.iter().any(|(uid, _, _)| uid == &k1.id), "Human 那一端也在 provenance 里");
    assert!(got.iter().all(|(_, ref_kind, _)| ref_kind == "card"),
        "两端的 ref_kind 都是 card：推导行与消费行同表，分表那一格已由 canonical 合并");
    assert!(got.iter().all(|(_, _, role)| role == "human"),
        "边那一格取最强端（Human），并被盖到两端行上——零端的 System 在这一格被合成抵消");
}

#[test]
fn anchored_timeline_row_writes_exactly_one_provenance_row() {
    // I-5 补牙：`put_timeline_rows` 的 provenance 支原先零读者（撤掉它四条用例照绿）。
    // 这条断言把署名口径钉在库上：被锚行的表 + 被锚行 id，一条不多一条不少。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k1 = put_card(root.memory(), &card_c("r1", "x"),
        &[Provenance { turn_uid: "h".repeat(64), anchor_role: AnchorRole::Human }], 1000).unwrap();
    let rows = vec![reader::lookup(root.memory(), &k1).unwrap().unwrap()];
    let reply = serde_json::json!({
        "relationships":[], "chains":[],
        "timeline":[{"anchor":"r1","offset_ms":86400000,"body_ref":"锚住的一条","basis":["r1"]}]
    }).to_string();
    let out = derive_pass(root.memory(), &rows, &Scripted { replies: vec![reply], i: 0 }, 3000).unwrap();
    assert_eq!(out.rows_written, 2, "时间行 + 它那条旁路 fact_link（锚到消费行）");
    let tl_id: String = root.memory().conn().query_row(
        "SELECT id FROM card WHERE kind='timeline' ORDER BY created_at_ms DESC LIMIT 1",
        [], |r| r.get(0)).unwrap();
    let prov: Vec<(String, String, String)> = {
        let mut st = root.memory().conn().prepare(
            "SELECT ref_kind, turn_uid, anchor_role FROM provenance
             WHERE mem_table='card' AND mem_id=?1").unwrap();
        let v = st.query_map([&tl_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                 .unwrap().map(|x| x.unwrap()).collect();
        v
    };
    assert_eq!(prov.len(), 1, "有锚 ⇒ provenance 恰好一行");
    assert_eq!(prov[0].0, "card", "ref_kind = 被锚行所在表（canonical 之后与落库行同表）");
    assert_eq!(prov[0].1, k1.id, "出处是**被锚行**，不是端点行");
    assert_eq!(prov[0].2, "human", "锚角色取被锚行最强值（裁定 11）");
}

/// I-4 的后果格：**锚到推导行 ⇒ 落空，而不是落一个"锚在写入钟上"的假 `anchored`**。
/// `derive_pass` 的锚行 ms 只从**本批**找，而批永远来自 `select_batch`（已滤掉推导行）
/// ⇒ 模型若指名一条推导行当锚，`parse` 的 `alias_to_id` 对不上 ⇒ `anchor_id = None`
/// ⇒ `time_anchor_ms` NULL + `time_confidence='unknown'`（裁定 13：不能猜当前时间）。
///
/// 为什么这条要占一条用例：推导读的 `created_at_ms` 是**那次推导 pass 的写入钟**不是事件时刻，
/// 锚上去算出的时间"看着很像真的"——这一格必须是可观察的 NULL，不是一句推断。
#[test]
fn an_anchor_naming_a_derived_row_yields_null_ms_and_unknown_confidence() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k1 = put_card(root.memory(), &card_c("r1", "真人说的"),
        &[Provenance { turn_uid: "h".repeat(64), anchor_role: AnchorRole::Human }], 1000).unwrap();
    let rows = vec![reader::lookup(root.memory(), &k1).unwrap().unwrap()];
    // 先在库里造一条**推导写的**行（真写入路径），拿到它的 id 当锚名。
    let ctx = BatchCtx { source_agent: "rc-derive".into(), device_id: "unknown".into() };
    put_timeline_rows(root.memory(), &[TimelineContent {
        title: "derive-先落的那行".into(), body: "先落".into(), scope: scope(),
        time_anchor_ms: Some(9_000), time_anchor_id: Some(k1.id.clone()),
        time_confidence: "anchored".into(), basis_ids: vec![],
    }], &ctx, 3000).unwrap();
    let derived_id: String = root.memory().conn().query_row(
        "SELECT id FROM card WHERE kind='timeline' AND text='先落'", [], |r| r.get(0))
        .unwrap();
    // 模型指名那条推导行当锚。批里只有 `r1`，而 `derived_id` 不是批内别名 ⇒ anchor 解析不动。
    let reply = serde_json::json!({
        "relationships":[], "chains":[],
        "timeline":[{"anchor":derived_id,"offset_ms":1000,"body_ref":"锚在被锚的那一行",
                     "basis":["r1"]}]
    }).to_string();
    let out = derive_pass(root.memory(), &rows, &Scripted { replies: vec![reply], i: 0 }, 5000).unwrap();
    assert_eq!(out.rows_written, 1, "锚解析不动时这条照收、不拒收（parse 面 anchor 是唯一例外）");
    let (ms, conf): (Option<i64>, String) = root.memory().conn().query_row(
        "SELECT time_anchor_ms, time_confidence FROM card WHERE text='锚在被锚的那一行'",
        [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert!(ms.is_none(), "锚点名一条推导行 ⇒ 落 NULL，不许拿它的写入钟当事件时刻");
    assert_eq!(conf, "unknown", "没有可重算的锚 ⇒ unknown（裁定 13 两口径的 fail-closed 那一支）");
}

// ===== 批次③ 段 B-2 · 因果链的端到端（E1/E2）=====

/// 一条链的答复夹具：环节 `r1,r2`（有序）、依据 `r2,r3`（`r2` 既是环节又是依据 ⇒ C-3 那一形）。
/// 三行批都是真库里的抽取行（`ref_kind='turn'`），所以 `table_of` 与 `anchor_role_of` 都有真读数。
fn one_chain_reply() -> String {
    serde_json::json!({
        "relationships": [], "timeline": [],
        "chains": [{"from": ["r1", "r2"], "mechanism": "先立的决定把排期顶掉了",
                    "type": "causes", "basis": ["r2", "r3"]}]
    }).to_string()
}

/// E1（落库 + 读数口径）：一次 `derive_pass` 带一条链 ⇒ `card` 里有 `kind='causal'` 行、
/// `provenance` 有它的 link/basis 行、`rows_written` 等于**链条数**（1），不是行数之和
/// （头行 1 + 出处行 3 = 4 行，`put_chains` 返回的是"这次真落了几条链"）。
/// 变异形（本条实测过）：删掉 `derive_pass` 里 `put_chains` 那一支（连同那行累加）⇒ 本条红在
/// `rows_written` 那句（读数 0 而不是 1），同一发让 E2 的第一句红。顺序那一格的牙不在本条：
/// 在 parse push 之前给 `ids_resolved` 加 `sort()` 时，这条夹具的 `r1,r2` 恰好与 id 升序撞成
/// 同一个序 ⇒ 本条照绿（实测），钉住顺序的是 rc-derive 的 parse 用例那一条。
#[test]
fn one_chain_reply_lands_a_causal_head_and_counts_one_chain() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k1 = put_card(root.memory(), &card_c("链的甲", "甲正文"), &prov(), 1000).unwrap();
    let k2 = put_card(root.memory(), &card_c("链的乙", "乙正文"), &prov(), 2000).unwrap();
    let k3 = put_card(root.memory(), &card_c("链的丙", "丙正文"), &prov(), 3000).unwrap();
    let rows = vec![reader::lookup(root.memory(), &k1).unwrap().unwrap(),
                    reader::lookup(root.memory(), &k2).unwrap().unwrap(),
                    reader::lookup(root.memory(), &k3).unwrap().unwrap()];
    let out = derive_pass(root.memory(), &rows, &Scripted { replies: vec![one_chain_reply()], i: 0 }, 9_000)
        .unwrap();
    assert_eq!(out.rejected, 0, "合法的链不该被 parse 的三道新闸门误伤");
    assert_eq!(out.rows_written, 1,
        "rows_written = 链条数（1），不是这次落的行数之和（头行 1 + 出处 3 = 4）");
    let head: String = root.memory().conn().query_row(
        "SELECT id FROM card WHERE kind='causal'", [], |r| r.get(0)).unwrap();
    let links: Vec<(i64, String)> = {
        let mut st = root.memory().conn().prepare(
            "SELECT ord, turn_uid FROM provenance WHERE mem_table='card' AND mem_id=?1
             AND role='link' ORDER BY ord").unwrap();
        st.query_map([&head], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap().map(|x| x.unwrap()).collect()
    };
    assert_eq!(links, vec![(0, k1.id.clone()), (1, k2.id.clone())],
        "环节按答复顺序带 ord：这一支把 parse 的顺序一路送到库里（工单 C-3）");
    let basis: Vec<String> = {
        let mut st = root.memory().conn().prepare(
            "SELECT turn_uid FROM provenance WHERE mem_table='card' AND mem_id=?1
             AND role='basis' ORDER BY turn_uid").unwrap();
        st.query_map([&head], |r| r.get::<_, String>(0))
            .unwrap().map(|x| x.unwrap()).collect()
    };
    assert_eq!(basis, vec![k3.id.clone()],
        "答复给的依据是 r2、r3，而 r2 已经在环节里 ⇒ 库里只留 link 那一行，basis 只剩丙这一格");
    assert_eq!(root.memory().conn().query_row(
        "SELECT count(*) FROM provenance WHERE mem_table='card' AND mem_id=?1",
        [&head], |r| r.get::<_, i64>(0)).unwrap(), 3, "两格环节 + 一格依据 = 三行出处，不多不少");
}

/// E2（与 P6 同一条口径的另一头证人）：同一批答复重跑 ⇒ `rows_written` 报 0。
/// 变异形（本条实测过）：`put_chains` 里那句 `if n_ins == 0 { continue; }` 摘掉（= 返回计划条数）
/// ⇒ 本条红在第二遍那句（读数 1）；删掉 `derive_pass` 里 `put_chains` 那一支则红在第一句（0）。
/// 库面那两句（头行数、环节行数）是同一批读数的对照臂：读数与库面若打架，本条也会红。
#[test]
fn the_same_chain_reply_reruns_as_zero_rows_written() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k1 = put_card(root.memory(), &card_c("链的甲", "甲正文"), &prov(), 1000).unwrap();
    let k2 = put_card(root.memory(), &card_c("链的乙", "乙正文"), &prov(), 2000).unwrap();
    let k3 = put_card(root.memory(), &card_c("链的丙", "丙正文"), &prov(), 3000).unwrap();
    let rows = vec![reader::lookup(root.memory(), &k1).unwrap().unwrap(),
                    reader::lookup(root.memory(), &k2).unwrap().unwrap(),
                    reader::lookup(root.memory(), &k3).unwrap().unwrap()];
    let first = derive_pass(root.memory(), &rows, &Scripted { replies: vec![one_chain_reply()], i: 0 }, 9_000)
        .unwrap();
    assert_eq!(first.rows_written, 1);
    let second = derive_pass(root.memory(), &rows, &Scripted { replies: vec![one_chain_reply()], i: 0 }, 9_500)
        .unwrap();
    assert_eq!(second.rows_written, 0,
        "同一条链重跑必须报 0：报 1 就是「计划条数」，重复批看起来像又落了一批新行");
    assert_eq!(root.memory().conn().query_row(
        "SELECT count(*) FROM card WHERE kind='causal'", [], |r| r.get::<_, i64>(0)).unwrap(), 1,
        "读数与库面对得上：重跑没有多出第二个头行");
    assert_eq!(root.memory().conn().query_row(
        "SELECT count(*) FROM provenance WHERE role='link'", [], |r| r.get::<_, i64>(0)).unwrap(), 2,
        "环节行也没有被插成双份");
}

// ===== 批次 M 段 B · 四键端到端（mentions + chain type + R-M-1）=====

/// 一条合法链（带 `type`）与一条逐字 mention 的答复：两族都从回复走到库里、读数分族可观测；
/// 同答复重跑两族都幂等（`rows_written` 与 `mentions_written` 同时归零）。
#[test]
fn mentions_and_chain_type_flow_from_reply_to_db_and_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let e = put_entity(m, &EntityContent {
        kind: "person".into(), title: "张三".into(), scope: scope(), aliases: vec![],
    }, &prov(), 1000).unwrap().id;
    let k1 = put_card(m, &card_c("张三来了", "张三说周五上线"), &prov(), 1000).unwrap();
    let k2 = put_card(m, &card_c("排期", "周五上线"), &prov(), 2000).unwrap();
    let rows = vec![reader::lookup(m, &k1).unwrap().unwrap(),
                    reader::lookup(m, &k2).unwrap().unwrap()];
    let reply = serde_json::json!({
        "relationships": [], "timeline": [],
        "chains": [{"from":["r1","r2"], "mechanism":"张三的排期被提前",
                    "type":"prevents", "basis":["r1"]}],
        "mentions": [{"row":"r1", "name":"张三"}]
    }).to_string();
    let out = derive_pass(m, &rows, &Scripted { replies: vec![reply.clone()], i: 0 }, 9_000).unwrap();
    assert_eq!(out.rejected, 0, "合法的链与 mention 不该被拒");
    assert_eq!(out.rows_written, 2, "1 链 + 1 mention = 2（mention 按实际插入行计）");
    assert_eq!(out.mentions_written, 1);
    assert_eq!(out.mentions_unresolved, 0);
    assert_eq!(out.mentions_missing, 0, "答复带 mentions 键 ⇒ 缺键计数为 0");
    // 库面：链头类型逐字落库
    let ct: Option<String> = m.conn().query_row(
        "SELECT causal_type FROM card WHERE kind='causal'", [], |r| r.get(0)).unwrap();
    assert_eq!(ct.as_deref(), Some("prevents"), "chain type 必须从 parse 一路落到链头行");
    // 库面：mention 三列 + 两列署名
    let ment: (String, String, String, String, String) = m.conn().query_row(
        "SELECT mem_table, mem_id, anchor_id, source_agent, device_id FROM memory_mention",
        [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).unwrap();
    assert_eq!(ment, ("card".into(), k1.id.clone(), e,
                      "rc-derive".into(), "unknown".into()),
        "memory_mention 的 mem_table/mem_id/anchor_id 与署名必须逐字对");
    // 同答复重跑：两族都幂等
    let out2 = derive_pass(m, &rows, &Scripted { replies: vec![reply], i: 0 }, 10_000).unwrap();
    assert_eq!(out2.rows_written, 0, "同一条链 + 同一条 mention 重跑必须报 0");
    assert_eq!(out2.mentions_written, 0, "mention 的幂等读数单独可观测");
}

/// 解析不到实体的名字只计数、不写行，**不炸整批**：timeline 那条照落。
#[test]
fn unresolved_mention_is_counted_not_written_and_does_not_kill_the_batch() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let k1 = put_card(m, &card_c("排期", "李四说下周上线"), &prov(), 1000).unwrap();
    let rows = vec![reader::lookup(m, &k1).unwrap().unwrap()];
    let reply = serde_json::json!({
        "relationships": [], "chains": [],
        "timeline": [{"anchor":"r1","offset_ms":86400000,"body_ref":"排期","basis":["r1"]}],
        "mentions": [{"row":"r1","name":"李四"}]
    }).to_string();
    let out = derive_pass(m, &rows, &Scripted { replies: vec![reply], i: 0 }, 9_000).unwrap();
    assert_eq!(out.rows_written, 2, "timeline 那条照落 + 旁路 fact_link：mention 解析不到实体不许炸批");
    assert_eq!(out.mentions_written, 0);
    assert_eq!(out.mentions_unresolved, 1, "零匹配必须计进 unresolved");
    assert_eq!(m.conn().query_row("SELECT count(*) FROM memory_mention", [],
                                  |r| r.get::<_, i64>(0)).unwrap(), 0, "零匹配不许写行");
}

/// `mentions` 缺键 ⇒ 降级不丢批：三键核心照写，缺键计数器可观测。
#[test]
fn missing_mentions_key_keeps_core_writes_and_reports_the_counter() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let k1 = put_card(m, &card_c("排期", "周五上线"), &prov(), 1000).unwrap();
    let rows = vec![reader::lookup(m, &k1).unwrap().unwrap()];
    let reply = serde_json::json!({
        "relationships": [], "chains": [],
        "timeline": [{"anchor":"r1","offset_ms":86400000,"body_ref":"排期","basis":["r1"]}]
    }).to_string();
    let out = derive_pass(m, &rows, &Scripted { replies: vec![reply], i: 0 }, 9_000).unwrap();
    assert_eq!(out.rows_written, 2, "缺 mentions 键不丢三键的核心写入（时间行 + 旁路 fact_link）");
    assert_eq!(out.mentions_missing, 1, "缺键必须可观测");
    assert_eq!(out.mentions_written, 0);
    assert_eq!(out.rejected, 0, "缺键不是拒收（四键里的后三键缺才是）");
}

/// R-M-1 端到端：三键皆空 + 一条合法 mention ⇒ `derive_pass` 返 Ok 且真落库。
#[test]
fn mentions_only_reply_writes_mentions_and_returns_ok() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let _e = put_entity(m, &EntityContent {
        kind: "person".into(), title: "王五".into(), scope: scope(), aliases: vec![],
    }, &prov(), 1000).unwrap().id;
    let k1 = put_card(m, &card_c("排期", "王五说下周上线"), &prov(), 1000).unwrap();
    let rows = vec![reader::lookup(m, &k1).unwrap().unwrap()];
    let reply = serde_json::json!({
        "relationships": [], "chains": [], "timeline": [],
        "mentions": [{"row":"r1","name":"王五"}]
    }).to_string();
    let out = derive_pass(m, &rows, &Scripted { replies: vec![reply], i: 0 }, 9_000).unwrap();
    assert_eq!(out.rows_written, 1, "三键空 + 一条合法 mention 是契约内答复（R-M-1）");
    assert_eq!(out.mentions_written, 1);
    assert_eq!(out.rejected, 0);
}

fn card_in(scope: Scope, title: &str, body: &str) -> FactContent {
    FactContent { kind: "decision".into(), title: title.into(), body: body.into(),
                  scope, valid_to_ms: None }
}

#[test]
fn 跨归属批_每条推导行取自己依据端的归属而不是批首行() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let a = Scope { kind: ScopeKind::Person, key: "alice".into() };
    let b = Scope { kind: ScopeKind::Project, key: "beta".into() };
    shared_entity(root.memory(), "共同实体", 900);   // I4：two-card edge 的共享 mention
    // 批首行是 alice，所有推导都挂在 beta 的行上 ⇒ 旧代码（batch[0]）给 person/alice。
    let k1 = put_card(root.memory(), &card_in(a.clone(), "r1", "甲的卡 共同实体"), &prov(), 1000).unwrap();
    let k2 = put_card(root.memory(), &card_in(b.clone(), "r2", "乙的卡 共同实体"), &prov(), 2000).unwrap();
    let rows = vec![
        reader::lookup(root.memory(), &k1).unwrap().unwrap(),
        reader::lookup(root.memory(), &k2).unwrap().unwrap(),
    ];
    let reply = serde_json::json!({
        "relationships":[{"from":"r2","to":"r1","kind":"part_of","reason":"乙引出甲","basis":["r2"]}],
        "chains":[{"from":["r2","r1"],"mechanism":"乙引出甲","type":"causes","basis":["r2"]}],
        "timeline":[{"anchor":"r2","offset_ms":1000,"body_ref":"乙的时间","basis":["r2"]}]
    }).to_string();
    derive_pass(root.memory(), &rows, &Scripted { replies: vec![reply], i: 0 }, 5000).unwrap();

    let edge: (String, String) = root.memory().conn().query_row(
        "SELECT scope_kind, scope_key FROM fact_link", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(edge, ("project".into(), "beta".into()), "边必须取 from 端的归属");
    let tl: (String, String) = root.memory().conn().query_row(
        "SELECT scope_kind, scope_key FROM card WHERE kind='timeline'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(tl, ("project".into(), "beta".into()), "时间线必须取锚行的归属");
    let head: (String, String) = root.memory().conn().query_row(
        "SELECT scope_kind, scope_key FROM card WHERE kind='causal'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(head, ("project".into(), "beta".into()), "链必须取首环节的归属");
}

#[test]
fn 无锚且无依据的时间线被拒收_因为归属无从谈起() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k1 = put_card(root.memory(), &card_c("r1", "x"), &prov(), 1000).unwrap();
    let rows = vec![reader::lookup(root.memory(), &k1).unwrap().unwrap()];
    let reply = serde_json::json!({
        "relationships":[], "chains":[],
        "timeline":[{"offset_ms":12345,"body_ref":"谁的时间","basis":[]}]
    }).to_string();
    let out = derive_pass(root.memory(), &rows, &Scripted { replies: vec![reply], i: 0 }, 3000).unwrap();
    assert_eq!(out.rejected, 1, "无锚且无依据 ⇒ 没有归属可落，必须单条拒收");
    assert_eq!(out.rows_written, 0);
    let n: i64 = root.memory().conn().query_row(
        "SELECT count(*) FROM card WHERE kind='timeline'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "拒收的条目不许落行");
}

#[test]
fn 绝对日期只在依据行原文里出现才落锚_猜日期即拒收() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    // r1 的正文写着这个日期；r2 的正文没有。
    let k1 = put_card(root.memory(), &card_c("r1", "发布于 2026-10-15。"), &prov(), 1000).unwrap();
    let k2 = put_card(root.memory(), &card_c("r2", "没有日期的一行。"), &prov(), 2000).unwrap();
    let rows = vec![
        reader::lookup(root.memory(), &k1).unwrap().unwrap(),
        reader::lookup(root.memory(), &k2).unwrap().unwrap(),
    ];
    // ① 依据行写着日期 ⇒ 落锚。
    let good = serde_json::json!({
        "relationships":[], "chains":[],
        "timeline":[{"date":"2026-10-15","body_ref":"发布日","basis":["r1"]}]
    }).to_string();
    let out = derive_pass(root.memory(), &rows, &Scripted { replies: vec![good], i: 0 }, 3000).unwrap();
    assert_eq!(out.rows_written, 2, "时间行 + 绝对日期锚出的旁路 fact_link");
    let row: (i64, String, String) = root.memory().conn().query_row(
        "SELECT time_anchor_ms, time_anchor_id, time_confidence FROM card
         WHERE kind='timeline' AND text='发布日'", [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!(row.0, 1_792_022_400_000, "日期必须换成 UTC 零点毫秒");
    assert_eq!(row.1, k1.id, "锚指向那条写着日期的依据行");
    assert_eq!(row.2, "anchored");

    // ② 日期只出现在 body_ref（模型自己的话）⇒ 拒收。
    let guessed = serde_json::json!({
        "relationships":[], "chains":[],
        "timeline":[{"date":"2026-10-20","body_ref":"我自己说的 2026-10-20","basis":["r2"]}]
    }).to_string();
    let out2 = derive_pass(root.memory(), &rows, &Scripted { replies: vec![guessed], i: 0 }, 4000).unwrap();
    assert_eq!(out2.rejected, 1, "依据行没写这个日期 ⇒ 猜日期，必须拒收");
    let n: i64 = root.memory().conn().query_row(
        "SELECT count(*) FROM card WHERE kind='timeline' AND text LIKE '%我自己说的%'", [],
        |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "被拒收的时间线不许落行");

    // ③ date 与 anchor/offset 同时给 ⇒ 两种落地口径打架，拒收。
    let both = serde_json::json!({
        "relationships":[], "chains":[],
        "timeline":[{"date":"2026-10-15","anchor":"r1","offset_ms":1,"body_ref":"两口径","basis":["r1"]}]
    }).to_string();
    let out3 = derive_pass(root.memory(), &rows, &Scripted { replies: vec![both], i: 0 }, 5000).unwrap();
    assert_eq!(out3.rejected, 1, "绝对日期与相对锚只许二选一");
}

/// I4：纯共现的两条 card 不建边；被门槛挡下的条数进 `rejected` 读数（不是静默丢弃）。
#[test]
fn a_pure_cooccurrence_edge_is_dropped_and_counted_as_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k1 = put_card(root.memory(), &card_c("阈值已提高", "从 0.5 提高到 0.75"), &prov(), 1000).unwrap();
    let k2 = put_card(root.memory(), &card_c("触发点", "约 384K 触发压缩"), &prov(), 2000).unwrap();
    let rows = vec![
        reader::lookup(root.memory(), &k1).unwrap().unwrap(),
        reader::lookup(root.memory(), &k2).unwrap().unwrap(),
    ];
    let reply = serde_json::json!({
        "relationships":[{"from":"r1","to":"r2","kind":"depends_on","reason":"x","basis":["r1","r2"]}],
        "chains":[], "timeline":[]
    }).to_string();
    let out = derive_pass(root.memory(), &rows, &Scripted { replies: vec![reply], i: 0 }, 9999).unwrap();
    assert_eq!(out.rows_written, 0, "无锚边不落库");
    assert_eq!(out.rejected, 1, "被门槛挡下的边必须可数（I4）");
    let n: i64 = root.memory().conn().query_row("SELECT count(*) FROM fact_link", [],
                                                |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "纯共现不许在库里留下 fact_link 行");
}

/// I4 + I6：别名锚先补铸 mention，`part_of` 的共享 mention 判据因此过门槛；
/// 派生事件（timeline）自己也补上 mention，不是图上的孤点。
#[test]
fn alias_mentions_are_minted_before_edges_and_derived_rows_get_mentions() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    put_entity(m, &EntityContent {
        kind: "org".into(), title: "Agnes AI".into(), scope: scope(),
        aliases: vec!["agnes-ai".into()],
    }, &prov(), 900).unwrap();
    let k1 = put_card(m, &card_c("视觉绑定", "agnes-ai 提供视觉通道"), &prov(), 1000).unwrap();
    let k2 = put_card(m, &card_c("回退链", "agnes-ai 仍在上游回退链里"), &prov(), 2000).unwrap();
    let rows = vec![
        reader::lookup(m, &k1).unwrap().unwrap(),
        reader::lookup(m, &k2).unwrap().unwrap(),
    ];
    let reply = serde_json::json!({
        "relationships":[{"from":"r1","to":"r2","kind":"part_of","reason":"同一提供方","basis":["r1","r2"]}],
        "chains":[],
        "timeline":[{"anchor":"r1","offset_ms":1000,"body_ref":"agnes-ai 的后续观察","basis":["r1"]}]
    }).to_string();
    let out = derive_pass(m, &rows, &Scripted { replies: vec![reply], i: 0 }, 9999).unwrap();
    // 2 条别名 mention（两张卡）+ 1 条派生行 mention + 1 边 + 1 timeline
    assert_eq!(out.mentions_written, 2, "别名补铸必须覆盖两端（派生行那条在写入口内部铸）");
    assert_eq!(out.rejected, 0);
    let edge: i64 = m.conn().query_row(
        "SELECT count(*) FROM fact_link WHERE relation='part_of'", [], |r| r.get(0)).unwrap();
    assert_eq!(edge, 1, "别名锚铸出的共享 mention ⇒ part_of 过门槛");
    let event_mentions: i64 = m.conn().query_row(
        "SELECT count(*) FROM memory_mention
         WHERE mem_table='card' AND mem_id=(SELECT id FROM card WHERE kind='timeline')",
        [], |r| r.get(0)).unwrap();
    assert_eq!(event_mentions, 1, "I5：派生事件必须补 mention（不许是孤点）");
}

/// I5：因果事件的标题不截断、时间锚取最后一环、正文里的实体补 mention。
#[test]
fn a_causal_event_gets_a_full_title_a_turn_anchor_and_mentions() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    put_entity(m, &EntityContent {
        kind: "org".into(), title: "Agnes AI".into(), scope: scope(),
        aliases: vec!["agnes-ai".into()],
    }, &prov(), 900).unwrap();
    let k1 = put_card(m, &card_c("视觉绑定", "agnes-ai 提供视觉通道"), &prov(), 1000).unwrap();
    let k2 = put_card(m, &card_c("回退链", "agnes-ai 仍在上游回退链里"), &prov(), 2000).unwrap();
    let rows = vec![
        reader::lookup(m, &k1).unwrap().unwrap(),
        reader::lookup(m, &k2).unwrap().unwrap(),
    ];
    let mech = "agnes-ai 保留在回退链里让主提供方失败时仍能接管，这一整句必须原样进标题不被截断";
    let reply = serde_json::json!({
        "relationships":[], "timeline":[],
        "chains":[{"from":["r1","r2"],"mechanism":mech,"type":"enables","basis":["r1"]}]
    }).to_string();
    let out = derive_pass(m, &rows, &Scripted { replies: vec![reply], i: 0 }, 9999).unwrap();
    assert_eq!(out.rows_written, 3, "1 条链 + 2 条别名 mention");
    let (title, body, anchor_ms, anchor_id, conf): (String, String, Option<i64>, Option<String>, String) =
        m.conn().query_row(
            "SELECT title, text, time_anchor_ms, time_anchor_id, time_confidence
             FROM card WHERE kind='causal'", [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).unwrap();
    assert_eq!(title, format!("derive-{mech}"), "I5：标题完整，不按 20 字符截断");
    assert_eq!(body, mech, "正文 = 机制句本身（不翻译、不截断）");
    assert_eq!(anchor_ms, Some(2000), "链头锚在最后一环（r2）的源侧时刻");
    assert_eq!(anchor_id.as_deref(), Some(k2.id.as_str()));
    assert_eq!(conf, "anchored", "有源侧时刻锚 ⇒ anchored");
    let ment: i64 = m.conn().query_row(
        "SELECT count(*) FROM memory_mention
         WHERE mem_table='card' AND mem_id=(SELECT id FROM card WHERE kind='causal')",
        [], |r| r.get(0)).unwrap();
    assert_eq!(ment, 1, "I5：因果事件补 mention（agnes-ai ⇒ Agnes AI）");
}

/// I3 + R3：derive 时间线的 as_of/created_at 取源侧时刻（锚行时刻 + offset），不是导入钟；
/// `event_link` 的 canonical 落点 `fact_link` 是旁路写入，必须进 `rows_written` 读数（否则写了没人知道）。
#[test]
fn timeline_rows_take_source_time_and_the_event_link_is_counted() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let ev = put_event(root.memory(), &FactContent {
        kind: "meeting".into(), title: "季度会".into(), body: "会上定了十月发布。".into(),
        scope: scope(), valid_to_ms: None,
    }, &prov(), 1000).unwrap();
    let k2 = put_card(root.memory(), &card_c("发布决定", "十月发布。"), &prov(), 2000).unwrap();
    let rows = vec![
        reader::lookup(root.memory(), &ev).unwrap().unwrap(),
        reader::lookup(root.memory(), &k2).unwrap().unwrap(),
    ];
    let reply = serde_json::json!({
        "relationships":[], "chains":[],
        "timeline":[{"anchor":"r1","offset_ms":5000,"body_ref":"发布排在季度会之后","basis":["r1","r2"]}]
    }).to_string();
    let out = derive_pass(root.memory(), &rows, &Scripted { replies: vec![reply], i: 0 }, 9999).unwrap();
    // timeline 1 + event_link 1（锚到事件）+ 0 mention
    assert_eq!(out.rows_written, 2, "R3：旁路写入的 event_link 必须进 rows_written 读数");
    let (as_of, created, anchor, conf): (i64, i64, Option<i64>, String) =
        root.memory().conn().query_row(
            "SELECT as_of_ms, created_at_ms, time_anchor_ms, time_confidence
             FROM card WHERE kind='timeline'", [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
    assert_eq!((as_of, created, anchor), (6000, 6000, Some(6000)),
        "I3：as_of/created_at/锚都取源侧时刻 1000+5000，不是导入钟 9999");
    assert_eq!(conf, "anchored");
    let links: i64 = root.memory().conn().query_row(
        "SELECT count(*) FROM fact_link WHERE relation='responds_to'", [], |r| r.get(0)).unwrap();
    assert_eq!(links, 1, "A44 软边确实经旁路（canonical fact_link）写入了");
}
