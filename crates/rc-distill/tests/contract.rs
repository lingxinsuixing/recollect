use rc_distill::batch::{Batch, TurnRow};
use rc_distill::contract::{parse_output, ContractError, KEYS};
use serde_json::{json, Value};

fn row(i: usize, role: &str, body: &str) -> TurnRow {
    TurnRow { turn_uid: format!("{:02}{}", i, "a".repeat(62)), role: role.into(),
              body: body.into(), created_at_ms: 1_000 + i as i64,
              ingested_at_ms: 1_000 + i as i64,
              speaker: None, speaker_anchor_id: None }
}

fn batch() -> Batch {
    Batch {
        session_key: "sess-synthetic-1".into(),
        device_id: "dev-synthetic-1".into(),
        source_agent: "synthetic-agent".into(),
        project_hint: Some("proj-synthetic".into()),
        rows: vec![row(0, "human", "我在赶进度"), row(1, "agent", "好的，先验证再上线"),
                   row(2, "human", "上次没验证就栽了")],
    }
}

/// 五键齐全的最小产出。`u` = 批次里第 0 行的 uid（脚本里直接用，不手抄 hex）。
fn ok_output() -> Value {
    let u = row(0, "human", "").turn_uid.clone();
    json!({
        "entities": [],
        "cards": [{"kind":"lesson","title":"上线前先验证","body":"上次没验证就栽了","valid_to_ms":null,"provenance":[u]}],
        "events": [],
        "assertions": [],
        "worthless_turn_uids": []
    })
}

fn dump(v: &Value) -> String { v.to_string() }

#[test]
fn key_list_is_exactly_the_five_from_the_contract() {
    assert_eq!(KEYS, ["entities", "cards", "events", "assertions", "worthless_turn_uids"]);
}

#[test]
fn happy_path_parses_and_counts_content_items() {
    let out = parse_output(&dump(&ok_output()), &batch()).unwrap();
    assert_eq!(out.cards.len(), 1);
    assert_eq!(out.content_items, 1);
    assert!(out.has_content());
    assert_eq!(out.cards[0].uids, vec![row(0, "human", "").turn_uid]);
}

#[test]
fn each_missing_key_drops_the_whole_batch() {
    // §7 原文："输出五个键缺任何一个 ⇒ 整批丢弃 ⇒ 不推水位"。四条各一用例，不是"任取一条"。
    for key in ["entities", "cards", "events", "assertions"] {
        let mut v = ok_output().as_object().unwrap().clone();
        v.remove(key);
        let e = parse_output(&dump(&Value::Object(v)), &batch()).unwrap_err();
        assert_eq!(e, ContractError::MissingKey(match key {
            "entities" => "entities", "cards" => "cards", "events" => "events", _ => "assertions" }),
            "缺 {key} 必须报 MissingKey");
    }
    // 第五条：worthless 也不能缺。
    let mut v = ok_output().as_object().unwrap().clone();
    v.remove("worthless_turn_uids");
    assert!(matches!(parse_output(&dump(&Value::Object(v)), &batch()),
                     Err(ContractError::MissingKey("worthless_turn_uids"))));
}

#[test]
fn malformed_json_is_an_error_never_an_empty_object() {
    // "写零行 = 错误"的第三个执法点：JSON 解析绝不返回空对象。
    let e = parse_output("{ this is not json", &batch()).unwrap_err();
    assert_eq!(e, ContractError::NotAnObject);
}

#[test]
fn non_object_document_is_not_an_empty_output() {
    for text in ["[]", "null", "\"\"", "5"] {
        assert_eq!(parse_output(text, &batch()).unwrap_err(), ContractError::NotAnObject, "{text}");
    }
}

#[test]
fn key_present_but_not_an_array_is_its_own_error() {
    // 与"缺键"分开报：缺键是模型没表态，类型错是模型表了个态但形状不对——两者的修法不同。
    let mut v = ok_output().as_object().unwrap().clone();
    v.insert("cards".into(), json!({"0": "not an array"}));
    assert!(matches!(parse_output(&dump(&Value::Object(v)), &batch()),
                     Err(ContractError::KeyNotArray("cards"))));
}

#[test]
fn provenance_is_mandatory_and_must_point_inside_the_batch() {
    let mut v = ok_output().as_object().unwrap().clone();
    v.insert("cards".into(), json!([{"kind":"lesson","title":"甲","body":"乙"}]));
    assert!(matches!(parse_output(&dump(&Value::Object(v)), &batch()),
                     Err(ContractError::ProvenanceMissing { table: "card", .. })));

    let mut v2 = ok_output().as_object().unwrap().clone();
    v2.insert("cards".into(), json!([{"kind":"lesson","title":"甲","body":"乙","provenance":[]}]));
    assert!(matches!(parse_output(&dump(&Value::Object(v2)), &batch()),
                     Err(ContractError::ProvenanceEmpty { .. })));

    let mut v3 = ok_output().as_object().unwrap().clone();
    v3.insert("cards".into(), json!([{"kind":"lesson","title":"甲","body":"乙",
                                      "provenance":["f".repeat(64)]}]));
    assert!(matches!(parse_output(&dump(&Value::Object(v3)), &batch()),
                     Err(ContractError::UnlistedUid { .. })));
}

#[test]
fn empty_content_fields_are_rejected_not_silently_dropped() {
    // 内容列归一后为空 ⇒ 契约错（整批丢弃），不是"这一条跳过"。
    // 理由：让"少一条"成为正常结局 = 把 provenance 覆盖率 100% 那条判据变成统计口号。
    // 三个字段各钉一次：`title` 与 `body` 是正文（body 只在要它的表上查），`kind` 是分类子——
    // 空 kind 会写成一行 `kind=''`（DDL 只有 NOT NULL、没有 CHECK），而 P3 的目录按 kind 分桶。
    for item in [
        json!({"kind":"lesson","title":"   ","body":"乙","provenance":[row(0,"","").turn_uid]}),
        json!({"kind":"lesson","title":"甲","body":"  ","provenance":[row(0,"","").turn_uid]}),
        json!({"kind":"  ","title":"甲","body":"乙","provenance":[row(0,"","").turn_uid]}),
        json!({"title":"甲","body":"乙","provenance":[row(0,"","").turn_uid]}),
    ] {
        let mut v = ok_output().as_object().unwrap().clone();
        v.insert("cards".into(), json!([item]));
        assert!(matches!(parse_output(&dump(&Value::Object(v)), &batch()),
                         Err(ContractError::ContentEmpty { table: "card", index: 0 })), "{item:?}");
    }
}

#[test]
fn an_unreadable_field_is_its_own_error_not_a_default() {
    // 坑 P-71 的模型输入侧：字段**在场而读不出来**绝不许折成默认值。
    // 这里的默认值不是"没写"，是"把模型说过的话抹掉"——`valid_to_ms` 折成 None 就是永不过期，
    // `aliases` 折成 [] 就是少一个别名，`provenance` 里的非字符串折掉就是少一条出处。
    let u = row(0, "", "").turn_uid;
    for (key, item, want) in [
        ("cards", json!({"kind":"lesson","title":"甲","body":"乙","valid_to_ms":"1700000000000",
                         "provenance":[u.clone()]}),
         ContractError::FieldUnreadable { table: "card", index: 0, field: "valid_to_ms" }),
        ("cards", json!({"kind":"lesson","title":"甲","body":"乙","valid_to_ms":1.5,
                         "provenance":[u.clone()]}),
         ContractError::FieldUnreadable { table: "card", index: 0, field: "valid_to_ms" }),
        ("entities", json!({"kind":"person","title":"张三","aliases":"张三,李四",
                            "provenance":[u.clone()]}),
         ContractError::FieldUnreadable { table: "entity", index: 0, field: "aliases" }),
        ("cards", json!({"kind":"lesson","title":"甲","body":"乙","provenance":[u.clone(), 7]}),
         ContractError::FieldUnreadable { table: "card", index: 0, field: "provenance" }),
        ("worthless_turn_uids", json!(7),
         ContractError::FieldUnreadable { table: "worthless_turn_uids", index: 0, field: "uid" }),
    ] {
        let mut v = ok_output().as_object().unwrap().clone();
        v.insert(key.into(), json!([item]));
        assert_eq!(parse_output(&dump(&Value::Object(v)), &batch()).unwrap_err(), want, "{key}");
    }
    // 反面：**缺席与 `null` 仍然是合法的"没有"**，这条收紧不许误伤它们。
    let mut v = ok_output().as_object().unwrap().clone();
    v.insert("cards".into(), json!([{"kind":"lesson","title":"甲","body":"乙",
                                     "valid_to_ms":null,"provenance":[u]}]));
    assert!(parse_output(&dump(&Value::Object(v)), &batch()).is_ok(), "null 仍是合法的没有");
    // 两支可选字段同一句话：`null` 对清单也是"没有"，不是"读不出来"。
    let mut v = ok_output().as_object().unwrap().clone();
    v.insert("entities".into(), json!([{"kind":"person","title":"张三",
                                        "aliases":null,"provenance":[row(0,"","").turn_uid]}]));
    let text = dump(&Value::Object(v.clone()));
    assert!(parse_output(&text, &batch()).is_ok(), "null 清单同样是没有");
    let with_aliases = parse_output(&text, &batch()).unwrap();
    assert!(with_aliases.entities[0].aliases.is_empty(), "缺席与 null 落到同一个空清单");
}

#[test]
fn duplicate_anchor_uids_collapse_to_one() {
    // `provenance` 的主键是 (mem_table, mem_id, turn_uid)（`rc-store` 的 DDL），模型把同一个 uid
    // 写两遍语义上是同一个锚——留在清单里就是任务 6 写侧的一次约束冲突。去重住在契约层，
    // 因为"把模型输出变成规范形状"本来就是这一层的活；顺序 = 首次出现序。
    let (a, c) = (row(0, "", "").turn_uid, row(2, "", "").turn_uid);
    let mut v = ok_output().as_object().unwrap().clone();
    v.insert("cards".into(), json!([{"kind":"lesson","title":"甲","body":"乙",
                                     "provenance":[a.clone(), c.clone(), a.clone()]}]));
    let out = parse_output(&dump(&Value::Object(v)), &batch()).unwrap();
    assert_eq!(out.cards[0].uids, vec![a, c], "去重保序，不多不少");
}

#[test]
fn error_payloads_never_echo_unbounded_model_text() {
    // 守卫 12 的模型输入侧（坑 P-72）：错误载荷里的 `uid` 只许是身份值。
    // `UnlistedUid` 恰恰装的是"不在我们批次里"的那段文本，也就是**模型的原文**，
    // 所以它进 `Display` 之前必须过形状关：64 位小写十六进制才回显，否则只回显长度。
    let leak = "我在赶进度，别把这段正文回显到运维面".repeat(8);
    assert!(leak.chars().count() > 120, "夹具要够长，才测得出折叠有没有发生");

    let mut v = ok_output().as_object().unwrap().clone();
    v.insert("cards".into(), json!([{"kind":"lesson","title":"甲","body":"乙","provenance":[leak.clone()]}]));
    let e = parse_output(&dump(&Value::Object(v)), &batch()).unwrap_err();
    assert_eq!(e, ContractError::UnlistedUid { uid: format!("<not-a-uid {} chars>", leak.chars().count()) });
    let text = e.to_string();
    assert!(!text.contains("我在赶进度"), "正文从 Display 漏出去了：{text}");
    assert!(text.contains("not-a-uid"), "折叠要留下可见的形状判据：{text}");

    let mut w = ok_output().as_object().unwrap().clone();
    w.insert("worthless_turn_uids".into(), json!([leak]));
    let e2 = parse_output(&dump(&Value::Object(w)), &batch()).unwrap_err();
    assert!(!e2.to_string().contains("我在赶进度"), "{e2}");

    // 正面对照：形状对的 64 位十六进制仍然**原样回显**——运维要靠它定位到具体那一轮。
    let ghost = "e".repeat(64);
    let mut g = ok_output().as_object().unwrap().clone();
    g.insert("cards".into(), json!([{"kind":"lesson","title":"甲","body":"乙","provenance":[ghost.clone()]}]));
    assert_eq!(parse_output(&dump(&Value::Object(g)), &batch()).unwrap_err(),
               ContractError::UnlistedUid { uid: ghost });
}

#[test]
fn worthless_uids_also_have_to_be_in_the_batch() {
    let mut v = ok_output().as_object().unwrap().clone();
    v.insert("worthless_turn_uids".into(), json!(["f".repeat(64)]));
    assert!(matches!(parse_output(&dump(&Value::Object(v)), &batch()),
                     Err(ContractError::WorthlessUnlisted { .. })));
}

#[test]
fn an_output_with_zero_items_and_zero_worthless_is_an_error() {
    // 坑 P-04：五键齐全但什么都不产出 ⇒ 这不是"蒸完了"，这是失败。
    // 它必须是**自己的变体**，不许复用 `NotAnObject`：那一格表达的是"根本不是对象"，
    // 这一格表达的是"对象合法但零产出"。把它们混成一格，P2 的读者就读不出"为什么没蒸出东西"。
    let v = json!({"entities":[],"cards":[],"events":[],"assertions":[],"worthless_turn_uids":[]});
    assert_eq!(parse_output(&dump(&v), &batch()).unwrap_err(), ContractError::ZeroOutput);
}

#[test]
fn provenance_anchors_come_from_the_database_row_not_from_the_model() {
    use rc_memory::AnchorRole;
    let out = parse_output(&dump(&ok_output()), &batch()).unwrap();
    let prov = out.cards[0].to_provenance(&batch()).unwrap();
    assert_eq!(prov.len(), 1);
    assert_eq!(prov[0].anchor_role, AnchorRole::Human, "锚角色是从 turns.role 查出来的（归属表里它是 P）");
}

#[test]
fn unreadable_role_is_its_own_error_and_its_own_reason_slug() {
    let mut b = batch();
    b.rows[0].role = "banana".into();
    let out = parse_output(&dump(&ok_output()), &b).unwrap();
    assert!(matches!(out.cards[0].to_provenance(&b), Err(ContractError::RoleUnreadable { .. })));
}

#[test]
fn scope_is_derived_and_never_comes_from_the_model() {
    use rc_memory::{MemTable, ScopeKind};
    let b = batch();
    let e = rc_distill::contract::scope_for(MemTable::Anchor, "person", "张三", &b);
    assert_eq!(e.kind, ScopeKind::Person);
    let f = rc_distill::contract::scope_for(MemTable::Card, "lesson", "标题", &b);
    assert_eq!((f.kind.as_str(), f.key.as_str()), ("project", "proj-synthetic"));
    let mut b2 = batch();
    b2.project_hint = None;
    assert_eq!(rc_distill::contract::scope_for(MemTable::Card, "lesson", "标题", &b2).key, "sess-synthetic-1",
               "没有 project_hint 时落到会话键：归属永远有值，但不许是模型给的");
}

#[test]
fn reason_vocabulary_is_a_closed_set() {
    // `extract_state.reason` 只许装词表里的短语之一（任务 3 把长度钉在 ≤32 是一条防线，这条是另一条）。
    // 词表住 `rc_distill` 的 crate 根：写它的四个作业（契约 / 围栏 / 配额 / 停轮）全在本 crate，
    // 而水位层只该认识 (family, scope, cursor)，不该认识抽取业务的语义。
    assert_eq!(rc_distill::REASON_SLUGS, ["schema", "fence", "quota", "halt"]);
    for r in rc_distill::REASON_SLUGS {
        assert!(r.len() <= 32, "{r} 过不了任务 3 钉的长度上界");
    }
    // 契约族全部落 `schema`：P2 的读者要区分的是"为什么没蒸出东西"这一层，不是"哪个键错了"那一层。
    assert_eq!(ContractError::NotAnObject.slug(), "schema");
    assert_eq!(ContractError::ZeroOutput.slug(), "schema");
    assert_eq!(ContractError::MissingKey("cards").slug(), "schema");
    assert_eq!(ContractError::RoleUnreadable { uid: "x".into() }.slug(), "schema");
    // 每个 slug 都必须在词表里：新增错误分型却忘了进词表 ⇒ 这条红，
    // 而不是往 reason 里塞一段自由文本（那会绕过"封闭集"这一整条守卫）。
    for e in [
        ContractError::NotAnObject,
        ContractError::ZeroOutput,
        ContractError::MissingKey("cards"),
        ContractError::KeyNotArray("cards"),
        ContractError::ItemNotObject { table: "card", index: 0 },
        ContractError::ContentEmpty { table: "card", index: 0 },
        ContractError::ProvenanceMissing { table: "card", index: 0 },
        ContractError::ProvenanceEmpty { table: "card", index: 0 },
        ContractError::FieldUnreadable { table: "card", index: 0, field: "valid_to_ms" },
        ContractError::UnlistedUid { uid: "x".into() },
        ContractError::WorthlessUnlisted { uid: "x".into() },
        ContractError::RoleUnreadable { uid: "x".into() },
    ] {
        assert!(rc_distill::REASON_SLUGS.contains(&e.slug()), "{} 不在词表", e.slug());
    }
}
