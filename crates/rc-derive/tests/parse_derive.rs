use rc_derive::parse::{parse_derive_output, DeriveError, Mention};
use rc_memory::keys::{AnchorRole, MemKey, MemTable, Verdict};
use rc_memory::{RowFacts, Scope, ScopeKind};
use serde_json::json;

/// 一行 `RowFacts` 全字面量（17 字段，照 `prompt_shape.rs::mk_row` 的形状）。
/// mentions 的 verbatim 检查要读 title/body，所以夹具从"一串 id"升级成整批行。
fn row(id: &str) -> RowFacts {
    RowFacts {
        key: MemKey { table: MemTable::Card, id: id.into() },
        created_at_ms: 1,
        kind: "decision".into(),
        title: "t".into(),
        body: "b".into(),
        as_of_ms: 1,
        valid_to_ms: None,
        superseded_by: None,
        retired: false,
        rejected: false,
        verdict: Verdict::Unreviewed,
        attestation: None,
        anchor_role: AnchorRole::Human,
        cooldown_until_ms: None,
        scope: Scope { kind: ScopeKind::Project, key: "k".into() },
        device_id: "d".into(),
        source_agent: "a".into(),
    }
}

fn batch() -> Vec<RowFacts> { vec![row("id-A"), row("id-B"), row("id-C")] }

#[test]
fn missing_any_of_the_three_keys_drops_the_whole_batch() {
    // 缺任一三键 ⇒ Err(Contract)，调用方不推水位（与抽取的"五键缺一整批丢弃"同律）
    for missing in ["relationships", "chains", "timeline"] {
        let mut v = json!({"relationships": [], "chains": [], "timeline": []});
        v.as_object_mut().unwrap().remove(missing);
        let s = v.to_string();
        let e = parse_derive_output(&s, &batch()).unwrap_err();
        assert!(matches!(e, DeriveError::Contract(_)),
                "缺 {missing} 应当 Contract 错，拿到 {e:?}");
    }
}

#[test]
fn out_of_enum_relationship_is_rejected_one_at_a_time() {
    // 枚举外关系单条拒收（裁定 12）；其他条照落
    let s = json!({
        "relationships": [
            {"from":"r1","to":"r2","kind":"part_of","reason":"x","basis":["r1"]},
            {"from":"r1","to":"r3","kind":"uses_coincidence","reason":"y","basis":["r1"]},  // 枚举外
            {"from":"r2","to":"r3","kind":"owns","reason":"z","basis":["r2"]}
        ],
        "chains": [], "timeline": []
    }).to_string();
    let p = parse_derive_output(&s, &batch()).unwrap();
    assert_eq!(p.relationships.len(), 2, "枚举外的 uses_coincidence 拒收后剩 2 条");
    assert_eq!(p.rejected, 1, "拒收数 1");
    assert_eq!(p.rows, 3, "rows = 落库 + 拒收 = 2 + 1");
}

#[test]
fn co_occurs_is_rejected_in_both_relationships_and_chains() {
    // 裁定 12：co_occurs 当因果被拒——关系枚举外 ∧ chain.mechanism 字段，两处各判、同一计数
    let s = json!({
        "relationships": [
            {"from":"r1","to":"r2","kind":"co_occurs","reason":"","basis":["r1"]}
        ],
        "chains": [
            {"from":["r1","r2"],"mechanism":"co_occurs","type":"causes","basis":["r1"]}
        ],
        "timeline": []
    }).to_string();
    let p = parse_derive_output(&s, &batch()).unwrap();
    assert_eq!(p.relationships.len(), 0, "关系表 co_occurs 拒收");
    assert_eq!(p.chains.len(), 0, "chain.mechanism=co_occurs 拒收");
    assert_eq!(p.rejected, 2, "两处各计一次");
}

#[test]
fn aliases_in_range_map_to_real_ids_out_of_range_reject() {
    // 别名 r3→id-C 在表内 ⇒ 落库；r99 越界 ⇒ 单条拒收（不炸批）
    let s = json!({
        "relationships": [
            {"from":"r1","to":"r3","kind":"part_of","reason":"x","basis":["r1"]},
            {"from":"r99","to":"r2","kind":"owns","reason":"y","basis":["r1"]}
        ],
        "chains": [], "timeline": []
    }).to_string();
    let p = parse_derive_output(&s, &batch()).unwrap();
    assert_eq!(p.relationships.len(), 1);
    assert_eq!(p.relationships[0].from_id, "id-A");
    assert_eq!(p.relationships[0].to_id, "id-C");
    // basis 出 parse 也是真实行 id——位置别名不越过这一层（T4 拿它写归属，落库的必须是行 id）
    assert_eq!(p.relationships[0].basis_ids, vec!["id-A".to_string()],
               "basis 里的 r1 必须换成真实行 id");
    assert_eq!(p.rejected, 1, "r99 越界拒收一次");

    // basis 含越界别名 ⇒ 整条拒收（与 from/to 同一口径：说不出依据行的推导不进库）
    let s2 = json!({
        "relationships": [{"from":"r1","to":"r3","kind":"part_of","reason":"x","basis":["r98"]}],
        "chains": [], "timeline": []
    }).to_string();
    let p2 = parse_derive_output(&s2, &batch()).unwrap();
    assert!(p2.relationships.is_empty(), "basis 越界 ⇒ 该条不落库，拿到 {}/1 条", p2.relationships.len());
    assert_eq!(p2.rejected, 1, "basis 越界拒收一次");

    // 三支键同口径：chain 与 timeline 的 basis 越界同样单条拒收，能解析的同样换成真实行 id
    let s3 = json!({
        "relationships": [],
        "chains": [{"from":["r1","r2"],"mechanism":"because","type":"causes","basis":["r7"]}],
        "timeline": [{"anchor":"r1","offset_ms":1,"body_ref":"b","basis":["r2"]},
                     {"anchor":"r2","offset_ms":2,"body_ref":"c","basis":["r99"]}]
    }).to_string();
    let p3 = parse_derive_output(&s3, &batch()).unwrap();
    assert!(p3.chains.is_empty(), "chain 的 basis 越界 ⇒ 整条拒收");
    assert_eq!(p3.timeline.len(), 1, "timeline 只留 basis 能解析的那条");
    assert_eq!(p3.timeline[0].basis_ids, vec!["id-B".to_string()], "timeline 的 r2 ⇒ 真实行 id");
    assert_eq!(p3.rejected, 2, "chain 一次 + timeline 一次");
}

#[test]
fn non_empty_batch_with_three_empty_keys_is_an_error() {
    // 写零行 = 错误（rc-distill zero_output.rs 同律；非空批三键皆空 ⇒ Err）
    let s = json!({"relationships": [], "chains": [], "timeline": []}).to_string();
    let e = parse_derive_output(&s, &batch()).unwrap_err();
    assert!(matches!(e, DeriveError::ZeroOutput), "三键皆空应 ZeroOutput，拿到 {e:?}");
}

// ===== 批次③ 段 B-2 · chains 的三道形态拒收（工单 §2）与顺序守恒 =====

/// D1-②（环节数 < 2）：单环节的链与零环节的链各一支，都要 `rejected` +1 且 `plan.chains` 零行。
/// 变异形（本条实测过）：删掉 parse 里 `from_aliases.len() < 2` 那一道 ⇒ 本条红在"环节数 1 的那一支"
/// 那句 `chains.is_empty()`（一个节点的"链"穿过 parse、落库就是一行带 `ord = 0` 的孤行；
/// 零环节那一支是同一判据的另一形，红只报第一个未通过的断言）。
#[test]
fn chain_with_fewer_than_two_links_is_rejected_at_parse() {
    let one = json!({"relationships": [],
                     "chains": [{"from": ["r1"], "mechanism": "断不出方向的那一条", "type": "causes", "basis": ["r1"]}],
                     "timeline": []}).to_string();
    let p1 = parse_derive_output(&one, &batch()).unwrap();
    assert!(p1.chains.is_empty(), "环节数 1 的链不许穿过 parse，拿到 {:?}", p1.chains);
    assert_eq!(p1.rejected, 1, "环节数不足只计一次拒收（一条链一个计数）");
    assert_eq!(p1.rows, 1, "rows = 落库 + 拒收 = 0 + 1");

    let none = json!({"relationships": [],
                      "chains": [{"from": [], "mechanism": "一格环节都没有", "type": "causes", "basis": ["r1"]}],
                      "timeline": []}).to_string();
    let p2 = parse_derive_output(&none, &batch()).unwrap();
    assert!(p2.chains.is_empty(), "环节数 0 的链不许穿过 parse，拿到 {:?}", p2.chains);
    assert_eq!(p2.rejected, 1);
}

/// D1-③（链内重复句柄）：**两种形**都要拒收——同一别名出现两次、两个不同别名解到同一行 id。
/// 后一支供的是 parse 里那句"查重按**解析后的真实行 id**"：`["r1","r3"]` 两个别名都合法，
/// 但这一批的 `r1`/`r3` 解到同一行 ⇒ 落库时撞 `(mem_table, mem_id, turn_uid)` 那个主键、
/// 静默吞掉一个环节，所以模型面就得拒。
/// 变异形（本条实测过）：删掉 parse 里 `ids_resolved` 的查重那一句 ⇒ 本条红在形一那句
/// `chains.is_empty()`（形二是同一判据的另一形，红只报第一个未通过的断言）；第三支（对照）
/// 与上面两支共用同一份批与同一张别名表，所以本条的红只能来自查重那一句，不是来自别名越界
/// ——对照支保证"互不相同的两个句柄"是收的。
#[test]
fn chain_with_a_repeated_handle_is_rejected_at_parse_in_both_shapes() {
    // 形一：同一别名写两次
    let s1 = json!({"relationships": [],
                    "chains": [{"from": ["r1", "r1"], "mechanism": "甲顶掉乙", "type": "causes", "basis": ["r1"]}],
                    "timeline": []}).to_string();
    let p1 = parse_derive_output(&s1, &batch()).unwrap();
    assert!(p1.chains.is_empty(), "同一别名出现两次 = 重复句柄，拿到 {:?}", p1.chains);
    assert_eq!(p1.rejected, 1);

    // 形二：两个不同别名解到同一行 id（批里第 1、3 个位置是同一条行）
    let dup_batch = vec![row("id-A"), row("id-B"), row("id-A")];
    let s2 = json!({"relationships": [],
                    "chains": [{"from": ["r1", "r3"], "mechanism": "甲顶掉乙", "type": "causes", "basis": ["r2"]}],
                    "timeline": []}).to_string();
    let p2 = parse_derive_output(&s2, &dup_batch).unwrap();
    assert!(p2.chains.is_empty(), "两个别名解到同一行 id 同样是重复句柄，拿到 {:?}", p2.chains);
    assert_eq!(p2.rejected, 1, "越界的不是别名而是句柄本身 ⇒ 查重在解析之后那一层拦");

    // 对照支：同一份批、把 r3 换成 r2（两个真实句柄互不相同）⇒ 照收，且解出来的就是真实行 id。
    let s3 = json!({"relationships": [],
                    "chains": [{"from": ["r1", "r2"], "mechanism": "甲顶掉乙", "type": "causes", "basis": ["r2"]}],
                    "timeline": []}).to_string();
    let p3 = parse_derive_output(&s3, &dup_batch).unwrap();
    assert_eq!(p3.chains.len(), 1, "对照支必须收：不然上面两支红在别处而不是红在查重");
    assert_eq!(p3.chains[0].from_ids, vec!["id-A".to_string(), "id-B".to_string()]);
    assert_eq!(p3.rejected, 0);
}

/// D1-①（mechanism 只有空白）：`""` 与 `"   "` 两形 + `mechanism` 键缺失那一形（parse 折成 `""`）。
/// 三支都要 `rejected` +1 且 `plan.chains` 零行。
/// 变异形（本条实测过）：删掉 parse 里 `mechanism.trim().is_empty()` 那一道 ⇒ 本条红在形一那句
/// `chains.is_empty()`（落库侧收到一条正文为空的因果事件：标题 `"derive-"` 非空 ⇒ 写侧"标题非空"
/// 那道闸门拦不住它；另两形是同一道闸门的另两种输入，红只报第一个未通过的断言）。
/// 判据只用 `trim`：更强的 `normalize_body` 那道住 `rc_memory::writers_x::put_chains`（rc-derive
/// 不依赖 rc-contract，递不进来），两层各测各的、互不冗余。
#[test]
fn chain_with_a_blank_mechanism_is_rejected_at_parse() {
    // 形一：`mechanism` 是空串
    let empty = json!({"relationships": [],
                       "chains": [{"from": ["r1", "r2"], "mechanism": "", "type": "causes", "basis": ["r1"]}],
                       "timeline": []}).to_string();
    let p1 = parse_derive_output(&empty, &batch()).unwrap();
    assert!(p1.chains.is_empty(), "空串机制不许穿过 parse，拿到 {:?}", p1.chains);
    assert_eq!(p1.rejected, 1, "空串机制计一次拒收");

    // 形二：只有空白（两个环节都合法 ⇒ 拦住它的只有这一道闸门）
    let blank = json!({"relationships": [],
                       "chains": [{"from": ["r1", "r2"], "mechanism": "   ", "type": "causes", "basis": ["r1"]}],
                       "timeline": []}).to_string();
    let p2 = parse_derive_output(&blank, &batch()).unwrap();
    assert!(p2.chains.is_empty(), "只有空白的机制不许穿过 parse，拿到 {:?}", p2.chains);
    assert_eq!(p2.rejected, 1);

    // 形三：`mechanism` 键整个缺失 ⇒ parse 把它折成 `""`，仍由同一道闸门拦住
    let missing = json!({"relationships": [],
                         "chains": [{"from": ["r1", "r2"], "basis": ["r1"]}],
                         "timeline": []}).to_string();
    let p3 = parse_derive_output(&missing, &batch()).unwrap();
    assert!(p3.chains.is_empty(), "缺 mechanism 键那一形同样不许穿过 parse，拿到 {:?}", p3.chains);
    assert_eq!(p3.rejected, 1);
}

/// D2（顺序在这层就得留住）：一条合法两环节链 ⇒ parse 通过，`from_ids` 顺序与答复逐字相同。
/// 这一格是 P2 的前提：顺序若在 parse 就丢（排序/去重），落库的 `ord` 再怎么算也读不回原序。
/// 变异形（本条实测过）：在 parse push 之前给 `ids_resolved` 加一句 `sort()` ⇒ 本条在
/// `from_ids` 那句红（答复给的是 `r3,r1`，排完序成了 `id-A,id-C`）。
#[test]
fn a_valid_two_link_chain_keeps_the_order_of_the_reply_verbatim() {
    let s = json!({"relationships": [],
                   "chains": [{"from": ["r3", "r1"], "mechanism": "排期被先立的决定顶掉",
                              "type": "causes", "basis": ["r2"]}],
                   "timeline": []}).to_string();
    let p = parse_derive_output(&s, &batch()).unwrap();
    assert_eq!(p.rejected, 0, "合法的链不该被新加的三道闸门误伤");
    assert_eq!(p.chains.len(), 1);
    assert_eq!(p.chains[0].from_ids, vec!["id-C".to_string(), "id-A".to_string()],
        "环节顺序必须逐字跟着答复走：这里丢了，写侧的 ord 就无从还原方向");
    assert_eq!(p.chains[0].basis_ids, vec!["id-B".to_string()], "basis 与 from 走同一支别名解析");
    assert_eq!(p.rows, 1, "rows = 落库 + 拒收 = 1 + 0");
}

// ===== 批次 M 段 B · 四键契约（mentions + chain type + R-M-1）=====

/// mentions 是第四键但**不整批丢**：缺键/非数组 ⇒ 空 + `mentions_missing = 1`；
/// 正常数组（含空数组）⇒ 0。三键核心读数一字不动。
#[test]
fn missing_or_non_array_mentions_downgrades_to_empty_plus_counter() {
    let rel = json!({"from":"r1","to":"r2","kind":"part_of","reason":"x","basis":["r1"]});
    // 缺键
    let missing = json!({"relationships":[rel.clone()], "chains":[], "timeline":[]}).to_string();
    let p = parse_derive_output(&missing, &batch()).unwrap();
    assert!(p.mentions.is_empty(), "缺 mentions 键 ⇒ 空向量，拿到 {:?}", p.mentions);
    assert_eq!(p.mentions_missing, 1, "缺键必须可观测（降级不是静默）");
    assert_eq!(p.rejected, 0, "缺键不拒收任何条（三键那条关系照收）");
    assert_eq!(p.relationships.len(), 1);
    // 非数组
    let non_array = json!({"relationships":[rel.clone()], "chains":[], "timeline":[],
                           "mentions": {}}).to_string();
    let p = parse_derive_output(&non_array, &batch()).unwrap();
    assert!(p.mentions.is_empty());
    assert_eq!(p.mentions_missing, 1, "非数组与缺键同一口径");
    assert_eq!(p.rejected, 0);
    // 对照：正常空数组 ⇒ 计数 0
    let empty = json!({"relationships":[rel], "chains":[], "timeline":[], "mentions": []}).to_string();
    let p = parse_derive_output(&empty, &batch()).unwrap();
    assert!(p.mentions.is_empty());
    assert_eq!(p.mentions_missing, 0, "空数组是契约内答复，不许计缺键");
}

/// mention 的 verbatim 单条拒收：名字必须逐字出现在**该行** title 或 body 里；
/// 空白名字、越界别名、别的行里的名字都拒。收下的两条 (row_id, name) 逐字对。
#[test]
fn mention_name_must_appear_verbatim_in_that_rows_title_or_body() {
    let mut r1 = row("id-A");
    r1.title = "张三来了".into();
    r1.body = "周五上线".into();
    let mut r2 = row("id-B");
    r2.title = "排期".into();
    r2.body = "周五上线".into();
    let b = vec![r1, r2];
    let s = json!({"relationships":[], "chains":[], "timeline":[],
                   "mentions":[
                       {"row":"r1","name":"张三"},      // 收：命中 r1 的 title
                       {"row":"r2","name":"张三"},      // 拒：r2 的 title/body 都没有张三
                       {"row":"r1","name":"  "},        // 拒：纯空白名字
                       {"row":"r9","name":"张三"},      // 拒：行别名越界
                       {"row":"r1","name":"周五"}       // 收：命中 r1 的 body
                   ]}).to_string();
    let p = parse_derive_output(&s, &b).unwrap();
    assert_eq!(p.mentions,
        vec![Mention { row_id: "id-A".into(), name: "张三".into() },
             Mention { row_id: "id-A".into(), name: "周五".into() }],
        "只有逐字命中的两条能穿过 parse");
    assert_eq!(p.rejected, 3, "非逐字 + 纯空白 + 越界 = 三条单条拒收");
    assert_eq!(p.rows, 3, "mention 拒收进 rejected ⇒ 自动进 rows（accepted mentions 不进）");
}

/// chain type 是三值封闭枚举：causes/enables/prevents 全收且逐字保留；
/// 缺键、域外值、null 各拒一条，**其余照收**（与关系 kind 同口径的单条拒收）。
#[test]
fn chain_type_is_a_three_value_enum_rejected_one_at_a_time() {
    let s = json!({"relationships":[], "timeline":[], "chains":[
        {"from":["r1","r2"],"mechanism":"甲","type":"causes","basis":["r1"]},
        {"from":["r2","r3"],"mechanism":"乙","type":"enables","basis":["r2"]},
        {"from":["r1","r3"],"mechanism":"丙","type":"prevents","basis":["r3"]}
    ]}).to_string();
    let p = parse_derive_output(&s, &batch()).unwrap();
    assert_eq!(p.chains.len(), 3, "三值都合法 ⇒ 三条全收");
    let types: Vec<&str> = p.chains.iter().map(|c| c.causal_type.as_str()).collect();
    assert_eq!(types, vec!["causes", "enables", "prevents"], "causal_type 必须逐字保留");
    assert_eq!(p.rejected, 0);

    let bads = vec![
        json!({"from":["r1","r2"],"mechanism":"缺类型","basis":["r1"]}),          // 缺 type 键
        json!({"from":["r1","r2"],"mechanism":"域外","type":"because","basis":["r1"]}), // 域外值
        json!({"from":["r1","r2"],"mechanism":"空值","type":null,"basis":["r1"]}), // null
    ];
    for bad in bads {
        let good = json!({"from":["r2","r3"],"mechanism":"对照","type":"causes","basis":["r2"]});
        let s = json!({"relationships":[], "timeline":[], "chains":[bad, good]}).to_string();
        let p = parse_derive_output(&s, &batch()).unwrap();
        assert_eq!(p.chains.len(), 1, "坏 type 只拒那一条，其余照收：{s}");
        assert_eq!(p.rejected, 1, "一条坏链只计一次拒收：{s}");
        assert_eq!(p.chains[0].causal_type, "causes", "收下的那条类型逐字对：{s}");
    }
}

/// R-M-1（控制器代拍）：三键皆空 + **一条合法 mention** 是契约内答复（不是 ZeroOutput）；
/// 对照：三键皆空 + 无 mentions 键仍是 ZeroOutput（不放宽旧案）。
#[test]
fn a_mentions_only_reply_is_not_zero_output() {
    let b = batch();  // title 都是 "t" ⇒ mention 名字 "t" 逐字命中 r1 的 title
    let s = json!({"relationships":[], "chains":[], "timeline":[],
                   "mentions":[{"row":"r1","name":"t"}]}).to_string();
    let p = parse_derive_output(&s, &b).unwrap();
    assert_eq!(p.rows, 0, "rows 仍是三键口径：accepted mention 不混进去");
    assert_eq!(p.mentions.len(), 1);
    assert_eq!(p.mentions[0].row_id, "id-A");
    assert_eq!(p.mentions[0].name, "t");

    let old = json!({"relationships":[], "chains":[], "timeline":[]}).to_string();
    let e = parse_derive_output(&old, &b).unwrap_err();
    assert!(matches!(e, DeriveError::ZeroOutput),
            "三键空且无 mentions ⇒ 仍是 ZeroOutput，拿到 {e:?}");
}
