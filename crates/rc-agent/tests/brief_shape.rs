use rc_memory::{
    attest, put_assertion, put_card, put_entity, put_event, AnchorRole, EntityContent,
    FactContent, Provenance, Scope, ScopeKind, Stance,
};
use rc_store::AgentRoot;

const NOW: i64 = 1_700_000_000_000;

#[test]
fn brief_是物理一行_带围栏_剥掉围栏能反解成状态() {
    let dir = tempfile::tempdir().unwrap();
    let root = AgentRoot::open(dir.path()).unwrap();
    let json = rc_agent::tools::status(root.memory(), None, &rc_agent::tools::StatusOptions::default(), NOW).unwrap();
    let block = rc_agent::surface::status_block(&json.text());
    let text = block.text();
    assert_eq!(text.lines().count(), 1, "brief 必须是物理一行：{text}");
    assert!(text.starts_with(rc_agent::FENCE), "缺围栏：{text}");
    let payload = text.strip_prefix(rc_agent::FENCE).unwrap();
    let v: serde_json::Value = serde_json::from_str(payload).expect("剥掉围栏要能反解成 JSON");
    assert_eq!(v["schema_version"], 2);
    // 守卫 12 的零正文那一半：状态块里不许出现任何正文键。
    for banned in ["\"title\"", "\"body\"", "\"snippet\""] {
        assert!(!payload.contains(banned), "状态块出现正文字段 {banned}：{payload}");
    }
}

#[test]
fn 围栏只认行首那一处() {
    // 值里可能含围栏串本身（操作员把 `[recollect] ` 写进过记忆）。这条断的是剥法唯一：
    // 只有行首那一处算围栏，剥完之后仍是合法 JSON。
    let block = rc_agent::surface::status_block(r#"{"note":"[recollect] 里面也有一串"}"#);
    let text = block.text();
    assert_eq!(text.matches(rc_agent::FENCE).count(), 2, "围栏串出现次数异常：{text}");
    assert_eq!(text.lines().count(), 1);
    let payload = text.strip_prefix(rc_agent::FENCE).unwrap();
    let v: serde_json::Value = serde_json::from_str(payload).unwrap();
    assert_eq!(v["note"], "[recollect] 里面也有一串");
}

/// 派发档第一段承重判据最后一条的兑现处：任务 8 评审实测 `tools::status` 的计数字段
/// （`rows.anchor`/`rows.card`、`open_rejections`、
/// `index.norm_version`/`schema_version`/`rebuilt_at_ms`）此前只在**空库或单 card 库**上被读过
/// （`tools_shape.rs` 里 `open_rejections` 读 0、`index.rows_indexed` 读 1）⇒ 它们全是零读者。
/// `brief` 是同一串读数的另一个门面，所以两表齐全（旧 `event`/`assertion` 消费行现在都落 `card`，
/// 这里靠四行夹具把两张表都点亮）+ 建过索引 + 一条未解决 rejected 的夹具
/// 在这里补最便宜：下面每个格子第一次有了**非零**的活读者。
#[test]
fn 状态计数在两张表与非零索引上都有活读者() {
    let dir = tempfile::tempdir().unwrap();
    let root = AgentRoot::open(dir.path()).unwrap();
    let m = root.memory();
    let scope = || Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() };
    let human = || vec![Provenance { turn_uid: "d".repeat(64), anchor_role: AnchorRole::Human }];
    let card = put_card(
        m,
        &FactContent {
            kind: "decision".into(),
            title: "状态夹具 卡".into(),
            body: "状态夹具的正文。".into(),
            scope: scope(),
            valid_to_ms: None,
        },
        &human(),
        NOW,
    )
    .unwrap();
    put_event(
        m,
        &FactContent {
            kind: "episode".into(),
            title: "状态夹具 事件".into(),
            body: "状态夹具的事件正文。".into(),
            scope: scope(),
            valid_to_ms: None,
        },
        &human(),
        NOW,
    )
    .unwrap();
    put_assertion(
        m,
        &FactContent {
            kind: "claim".into(),
            title: "状态夹具 断言".into(),
            body: "状态夹具的断言正文。".into(),
            scope: scope(),
            valid_to_ms: None,
        },
        &human(),
        NOW,
    )
    .unwrap();
    put_entity(
        m,
        &EntityContent {
            kind: "person".into(),
            title: "状态夹具 实体".into(),
            scope: scope(),
            aliases: vec!["状态夹具别名".into()],
        },
        &human(),
        NOW,
    )
    .unwrap();
    // 一条未解决的 rejected 表态（免锚，见 `writers_a::attest`）⇒ `open_rejections` 有了非零读者。
    attest(m, &card, Stance::Rejected, None, None, NOW).unwrap();
    rc_index::rebuild(m, NOW).unwrap();

    let json = rc_agent::tools::status(m, None, &rc_agent::tools::StatusOptions::default(), NOW).unwrap();
    let text = json.text();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["rows"]["card"], 3, "rows.card（card + 旧 event + 旧 assertion 同表）：{text}");
    assert_eq!(v["rows"]["anchor"], 1, "rows.anchor（旧 entity 行）：{text}");
    assert_eq!(v["open_rejections"], 1, "open_rejections：{text}");

    let st = rc_index::index_status(m).unwrap().unwrap();
    assert_eq!(v["index"]["built"], true, "index.built：{text}");
    assert_eq!(st.rows_indexed, 4, "两表的四行都要进索引：{st:?}");
    assert_eq!(v["index"]["rows_indexed"], st.rows_indexed);
    assert_eq!(v["index"]["norm_version"], st.norm_version);
    assert_eq!(v["index"]["schema_version"], st.schema_version);
    assert_eq!(v["index"]["rebuilt_at_ms"], st.rebuilt_at_ms);
    assert_ne!(st.rebuilt_at_ms, 0, "rebuilt_at_ms 要有非零读者：{st:?}");

    // 同一串再过一遍载体：计数面变宽了，"物理一行"的承诺不许跟着变。
    let block = rc_agent::surface::status_block(&text);
    let rendered = block.text();
    assert_eq!(rendered.lines().count(), 1, "两表齐全的 brief 不再是物理一行：{rendered}");
    assert!(rendered.starts_with(rc_agent::FENCE), "缺围栏：{rendered}");
}
