//! G1 刀 A：召回主循环消费 + `value_mix` 信封（派发档语义裁定③⑥、测试钉①⑦）。
//!
//! 三格钉：
//! 1. 施工行 ⇒ archive、主循环不出现、`value_mix.archived` 含它（计数与过滤同一处产生）；
//! 2. `value_mix` 只数**本查询候选集**被挡的行——库里另有归档行不进候选集就不数；
//! 3. 显式寻址（handles 臂）不受 archive 影响：原文全留、指认即给，`archived` 恒 0。
//!
//! 全归档查询不是空读数错误：信封照旧返回（rows=[] + value_mix），brief 用省略行
//! 如实说"有 N 条已归档"——静默匹配视图内容，但归档这个事实可见（设计 §波次一第 3 层）。
//!
//! 夹具全部合成（守卫 12）。

use rc_agent::recall::{recall, recall_narrative, NarrativeView, RecallFilter, RecallRequest};
use rc_memory::keys::MemKey;
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_fact::{put_fact_rows, FactWriteItem};
use rc_memory::{compute_mem_id, AnchorRole, BatchCtx, MemTable, Scope, ScopeKind};
use rc_store::Root;
use serde_json::{json, Value};

const NOW: i64 = 1_700_000_999_000;
const SRC_MS: i64 = 1_700_000_000_000;
const AGENT: &str = "agent-g1a-agent";
const QUERY: &str = "价值信封合成词";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-g1a-agent".into() }
}
fn ctx() -> BatchCtx {
    BatchCtx { source_agent: AGENT.into(), device_id: "dev-g1a-agent".into() }
}
fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-g1a-agent".into()),
        owner_agent_id: AGENT.into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}
fn self_id() -> String {
    compute_mem_id(MemTable::Anchor, "agent", AGENT, "", &scope())
}
fn uid(index: usize) -> String {
    format!("{:064x}", 0x51 + index)
}

fn seed_anchor(m: &rc_store::MemoryHandle) {
    // 自锚走生产入口（与写点同一条算法）：rc-agent 的依赖集里没有 rusqlite，不直插。
    let key = rc_memory::ensure_agent_anchor(m, &ctx(), &owner(), &scope(), NOW).unwrap();
    assert_eq!(key.id, self_id(), "夹具自锚与写点判定同源");
}

fn item(index: usize, predicate: &str, category: &str, text: &str) -> FactWriteItem {
    FactWriteItem {
        subject_anchor_id: self_id(),
        object_anchor_id: None,
        predicate: predicate.into(),
        value: Some("已执行".into()),
        category: category.into(),
        mode: "stated".into(),
        time_text: String::new(),
        quote: text.into(),
        evidence: vec![(uid(index), AnchorRole::Agent)],
        text: text.into(),
        scope: scope(),
        source_ms: SRC_MS,
    }
}

/// 三条行：施工（归档）+ 豁免类别（main）+ 不含查询词的无关施工（归档、不进候选集）。
/// 返回施工卡的句柄串（显式寻址用例要指认它）。
fn seeded() -> (tempfile::TempDir, Root, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_anchor(m);
    let out = put_fact_rows(m, &[
        item(1, "run_tests", "observation", &format!("{QUERY} 的施工流水正文。")),
        item(2, "run_tests", "decision", &format!("{QUERY} 的决策正文。")),
        item(3, "stop_monitor", "state_change", "无关归档流水正文。"),
    ], &ctx(), &owner(), NOW).unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    let archived_handle = out.card_ids[0].key_string();
    (dir, root, archived_handle)
}

fn doc(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

#[test]
fn 施工行不在主循环_value_mix_数它_测试钉一() {
    let (_d, root, archived_handle) = seeded();
    let m = root.memory();
    let filter = RecallFilter::empty();
    let req = RecallRequest::Query(QUERY.into());
    let text = recall(m, &req, &filter, NOW).unwrap().text();
    let v = doc(&text);
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "主循环只剩决策卡：{v}");
    assert_eq!(rows[0]["kind"], json!("decision"));
    assert!(!rows.iter().any(|r| r["id"] == json!(archived_handle)),
        "归档行不许出现在主循环：{v}");
    assert_eq!(v["value_mix"], json!({"main": 1, "archived": 1}),
        "计数与过滤同一处产生：main=实际进主循环行数，archived=本查询被挡行数");
    assert_eq!(v["truncated"], json!(false));
}

#[test]
fn value_mix_只数本查询候选集被挡行_测试钉七() {
    let (_d, root, _) = seeded();
    let m = root.memory();
    let filter = RecallFilter::empty();
    // 全库有两条归档（夹具体检），但本查询只命中其中一条。
    let sink_total: i64 = m.conn().query_row(
        "SELECT count(*) FROM card_sink", [], |r| r.get(0)).unwrap();
    assert_eq!(sink_total, 2);
    let req = RecallRequest::Query(QUERY.into());
    let v = doc(&recall(m, &req, &filter, NOW).unwrap().text());
    assert_eq!(v["value_mix"]["archived"], json!(1),
        "不进候选集的归档行不许数进来：{v}");
    // 只命中无关归档行的查询：它自己的候选集里被挡一条。
    // 查询词取"无关归档"（fact3 独有），不取"流水"——后者与 fact1 共享，会多挡一条。
    let req2 = RecallRequest::Query("无关归档".into());
    let v2 = doc(&recall(m, &req2, &filter, NOW).unwrap().text());
    assert_eq!(v2["value_mix"], json!({"main": 0, "archived": 1}),
        "全归档不是空读数错误：信封照旧、归档事实可见：{v2}");
    assert!(v2["rows"].as_array().unwrap().is_empty());
}

#[test]
fn 显式寻址不受_archive_影响() {
    let (_d, root, archived_handle) = seeded();
    let m = root.memory();
    let filter = RecallFilter::empty();
    let req = RecallRequest::Handles(vec![MemKey::parse(&archived_handle).unwrap()]);
    let v = doc(&recall(m, &req, &filter, NOW).unwrap().text());
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "原文全留：指认即给（archive 只挡查询主循环）");
    assert_eq!(rows[0]["id"], json!(archived_handle));
    assert_eq!(v["value_mix"], json!({"main": 1, "archived": 0}),
        "句柄臂没有候选集过滤，archived 恒 0");
}

#[test]
fn brief_主循环不渲染归档行_省略行如实报数() {
    let (_d, root, _) = seeded();
    let m = root.memory();
    let filter = RecallFilter::empty();
    let req = RecallRequest::Query(QUERY.into());
    let text = recall_narrative(m, &req, &filter, NOW, NarrativeView::Brief).unwrap().text();
    assert!(text.contains("决策正文"), "决策卡照旧渲染：{text}");
    assert!(!text.contains("施工流水正文"), "归档行不许进 brief 主循环：{text}");
    assert!(text.contains("（另有 1 条记忆已归档，未列入本次简报。）"),
        "归档事实可见（设计第 3 层）：{text}");
    // 全归档查询：不是 no_rows 错误，也不冒充"预算不足"。
    let req2 = RecallRequest::Query("无关归档".into());
    let text2 = recall_narrative(m, &req2, &filter, NOW, NarrativeView::Brief).unwrap().text();
    assert!(text2.contains("（另有 1 条记忆已归档，未列入本次简报。）"), "{text2}");
    assert!(!text2.contains("没有可以在预算内列出的条目"),
        "全归档不是预算问题，不许冒充预算措辞：{text2}");
    // 没有归档候选的查询：省略行一个字都不许多（默认字节兼容）。
    // 取"决策"（fact2 独有）：fact1/fact3 都含"正文"，用"决策正文"会因 OR 命中归档行。
    let req3 = RecallRequest::Query("决策".into());
    let text3 = recall_narrative(m, &req3, &filter, NOW, NarrativeView::Brief).unwrap().text();
    assert!(!text3.contains("已归档"), "无归档候选时不许冒出归档省略行：{text3}");
}