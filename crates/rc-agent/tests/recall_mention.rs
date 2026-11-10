//! 批次 M 段 C · #11a：`recall` 的 entity 过滤新增 **mention 边**（旧支保留）。
//!
//! 一节一条用例，端到端走公开入口 `recall(m, &RecallRequest::Query(..), &filter, NOW)`：
//! - ①mention 支：正文提到张三、但不是 Entity scope 的 **event 行**经 `mention` 边命中；
//! - ②旧支：scope 指向张三实体、正文不含"张三"的卡照旧命中（新支没有把它顶掉）；
//! - ③负例：正文提到张三、project scope、但**没有** mention 边的卡不命中；
//! - ④阳性对照：同一查询 + 空过滤 ⇒ 三行都在结果里（证明 ③ 不是"rank 根本没召回"造成的假阴性）。
//!
//! 夹具纪律照 `recall_provenance.rs`：出处走 `Provenance`、mention 边走段 B 的公开写入口
//! `put_mentions`，不裸写 `mention` SQL；只有读回 id 与建影子用 `m.conn()` / `rc_index`。
//!
//! 变异电池 M-C1：把 `recall.rs` 的 `if !(by_scope || by_mention)` 改回 `if !by_scope`
//! ⇒ 本用例的 ① 那一格红（mention 边断掉）。

use rc_agent::recall::{recall, RecallFilter, RecallRequest};
use rc_memory::keys::{AnchorRole, Scope, ScopeKind};
use rc_memory::writers_x::{put_card, put_entity, put_event, put_mentions, BatchCtx, EntityContent,
                           FactContent, MentionContent};
use rc_memory::Provenance;
use rc_store::Root;
use serde_json::Value;

const NOW: i64 = 1_700_000_000_000;

/// 与 `recall_provenance.rs` 同形的出处：`turn_uid` 是身份值，锚角色走 Human。
fn pv() -> Vec<Provenance> {
    vec![Provenance { turn_uid: "t-recall-mention".into(), anchor_role: AnchorRole::Human }]
}

fn project_scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "p-mention".into() }
}

/// recall 结果里的句柄清单（`key_string()` 形：`card:<id>` / `anchor:<id>`）。
fn handles(v: &Value) -> Vec<String> {
    v["rows"].as_array().expect("rows 必须是数组").iter()
        .map(|r| r["id"].as_str().expect("每行都带句柄").to_owned())
        .collect()
}

#[test]
fn 正文提到张三的_event_行经_mention_边命中实体过滤() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    // 实体一行：kind=person、标题=张三、无别名（filter 的名字经 title 那一臂解析到它）。
    let zhang = put_entity(m, &EntityContent {
        kind: "person".into(), title: "张三".into(),
        scope: Scope { kind: ScopeKind::Person, key: "p-mention-person".into() },
        aliases: vec![],
    }, &pv(), NOW).unwrap();

    // ①mention 边的载体：一条 event 行（方案 §4.4 原话"正文提到张三、但不是 Entity scope"），
    // 正文含"张三"、scope 是 project；边由段 B 的 `put_mentions` 落。
    let meeting = put_event(m, &FactContent {
        kind: "meeting".into(), title: "周会".into(), body: "张三负责这次发布。".into(),
        scope: project_scope(), valid_to_ms: None,
    }, &pv(), NOW).unwrap();
    let ment = put_mentions(m, &[MentionContent { row_id: meeting.id.clone(), name: "张三".into() }],
                            &BatchCtx::synthetic(), NOW).unwrap();
    assert_eq!((ment.written, ment.unresolved), (1, 0),
               "前置：mention 边没落上，下面第 ① 条断言就是空绿");

    // ③负例：正文提到张三、project scope，但**不**落 mention ⇒ 新支与旧支都不该放过它。
    let ghost = put_card(m, &FactContent {
        kind: "decision".into(), title: "旧决定的摘要".into(), body: "张三的旧决定。".into(),
        scope: project_scope(), valid_to_ms: None,
    }, &pv(), NOW).unwrap();

    // ②旧支：Entity scope 指向张三实体，正文**不含**"张三"（标题含，才进得了检索候选——
    // 它要证的是"靠 scope 命中"而不是"靠正文命中"）。
    let by_scope = put_card(m, &FactContent {
        kind: "note".into(), title: "张三的归属卡".into(),
        body: "这一行只按归属命中实体过滤。".into(),
        scope: Scope { kind: ScopeKind::Entity, key: zhang.id.clone() }, valid_to_ms: None,
    }, &pv(), NOW).unwrap();

    // recall 的查询臂走 `rank` ⇒ 影子必须先建（同 `recall_provenance.rs` 的 T2）。
    rc_index::rebuild(m, NOW).unwrap();

    let req = RecallRequest::query("张三").unwrap();
    let filter = RecallFilter { entity: Some("张三".into()), ..Default::default() };
    let v: Value = serde_json::from_str(
        &recall(m, &req, &filter, NOW).unwrap().text()).unwrap();
    let got = handles(&v);
    let (meeting_h, ghost_h, scope_h) =
        (meeting.key_string(), ghost.key_string(), by_scope.key_string());

    // ① mention 支：event 行不是 Entity scope，唯一能让它进来的是 mention 边。
    assert!(got.contains(&meeting_h),
            "正文提到张三的 event 行没经 mention 边命中实体过滤：{got:?}");
    // ② 旧支照旧：scope 指向实体的那一行没有 mention 行，靠 by_scope 命中。
    assert!(got.contains(&scope_h),
            "旧支被新支顶掉了：Entity scope 的行不在结果里：{got:?}");
    // ③ 负例：没有 mention 边、也不是 Entity scope ⇒ 必须被拒。
    assert!(!got.contains(&ghost_h),
            "没有 mention 边的 project 行穿透了实体过滤（mention 支读错了行）：{got:?}");

    // ④ 阳性对照：同一查询 + 空过滤 ⇒ 三行都在结果里。
    // 没有这一格，③ 的"不在结果里"可能只是 rank 没召回它（假阴性），而不是过滤真的拒了它。
    let all: Value = serde_json::from_str(
        &recall(m, &req, &RecallFilter::default(), NOW).unwrap().text()).unwrap();
    let all_ids = handles(&all);
    for (h, why) in [(&meeting_h, "mention 载体 event 行"),
                     (&scope_h, "旧支 Entity scope 卡"),
                     (&ghost_h, "负例卡")] {
        assert!(all_ids.contains(h),
                "空过滤下 {why} 也没被召回 ⇒ 上面那几条断言分不出'过滤拒了'与'根本没召回'：{all_ids:?}");
    }
}
