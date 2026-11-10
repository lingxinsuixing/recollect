//! 波二刀 B：复用反馈在**既有 `--json` 读数**上的可观测性（派发档测试钉⑦③）。
//!
//! 不新造第二套读数：分层通过 `view=json` 的行序可观测（同分对照 + 升档卡上移），
//! stale 判定通过同一面的 `form`/`silence_reason` 两格可观测（这两格早就在行键里）。
//! 夹具全部合成（守卫 12）。

use rc_agent::recall::{recall, RecallFilter, RecallRequest};
use rc_memory::reuse::record_reuse;
use rc_memory::{put_card, AnchorRole, FactContent, MemKey, Provenance, Scope, ScopeKind};
use rc_store::Root;
use serde_json::Value;

const NOW: i64 = 1_700_000_800_000;
const QUERY: &str = "复用读数合成词";
const BODY: &str = "复用读数合成词 一句足够短的正文";
const AGENT: &str = "agent-reuse-json";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-reuse-json".into() }
}

fn human_prov() -> Vec<Provenance> {
    vec![Provenance { turn_uid: "9".repeat(64), anchor_role: AnchorRole::Human }]
}

fn seed_pair(m: &rc_store::MemoryHandle) -> (MemKey, MemKey) {
    let a = put_card(
        m,
        &FactContent { kind: "note".into(), title: "甲卡".into(), body: BODY.into(), scope: scope(), valid_to_ms: None },
        &human_prov(),
        NOW,
    )
    .unwrap();
    let b = put_card(
        m,
        &FactContent { kind: "note".into(), title: "乙卡".into(), body: BODY.into(), scope: scope(), valid_to_ms: None },
        &human_prov(),
        NOW,
    )
    .unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    (a, b)
}

fn json_rows(text: &str) -> Vec<Value> {
    let v: Value = serde_json::from_str(text).unwrap();
    v["rows"].as_array().unwrap().clone()
}

/// 钉⑦：`view=json` 的行序就是分层的读数——同分的两张卡里，`worked=2` 的那张排到首位。
#[test]
fn json_行序可观测worked_升档() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let (a, b) = seed_pair(m);
    let req = RecallRequest::Query(QUERY.into());
    let control = json_rows(&recall(m, &req, &RecallFilter::empty(), NOW).unwrap().text());
    assert_eq!(control.len(), 2, "夹具应恰好两行：{control:?}");
    let (boosted, other) = if a.key_string() > b.key_string() { (a, b) } else { (b, a) };
    assert_eq!(control[0]["id"], serde_json::json!(other.key_string()), "对照前提：同分按句柄序");
    record_reuse(m, &boosted.id, "worked", None, NOW, AGENT).unwrap();
    record_reuse(m, &boosted.id, "worked", None, NOW, AGENT).unwrap();
    let after = json_rows(&recall(m, &req, &RecallFilter::empty(), NOW).unwrap().text());
    // 读数取证（`-- --nocapture` 可见）：view=json 的行序就是分层读数。
    println!(
        "reuse-rank json: ids={:?}",
        after.iter().map(|r| r["id"].as_str().unwrap_or("")).collect::<Vec<_>>()
    );
    assert_eq!(after[0]["id"], serde_json::json!(boosted.key_string()),
        "json 读数里升档卡没有上移：{after:?}");
    assert_eq!(after[1]["id"], serde_json::json!(other.key_string()));
}

/// 钉③（json 面）：`stale` 反馈 ⇒ 同一面 `silence_reason` 落 `stale`、`form` 落 `silent`，
/// 且 brief 视图不再出声（注入面）。
#[test]
fn json_面可观测stale反馈的读取侧判定() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let (a, _b) = seed_pair(m);
    record_reuse(m, &a.id, "stale", Some("事实已变更"), NOW, AGENT).unwrap();
    let req = RecallRequest::Query(QUERY.into());
    let rows = json_rows(&recall(m, &req, &RecallFilter::empty(), NOW).unwrap().text());
    let stale_row = rows
        .iter()
        .find(|r| r["id"] == serde_json::json!(a.key_string()))
        .unwrap_or_else(|| panic!("json 读数里没有那一行：{rows:?}"));
    println!(
        "reuse-stale json: id={} form={} silence_reason={}",
        stale_row["id"], stale_row["form"], stale_row["silence_reason"]
    );
    assert_eq!(stale_row["silence_reason"], serde_json::json!("stale"));
    assert_eq!(stale_row["form"], serde_json::json!("silent"));
    // 另一张卡不受影响（单变量：只有一条 stale 反馈）。
    let other = rows.iter().find(|r| r["id"] != serde_json::json!(a.key_string())).unwrap();
    assert_eq!(other["silence_reason"], Value::Null, "无反馈的行不许被连坐：{other}");
}

// 说明（钉③的形状边界，写在这里防后人误加断言）：`brief`/`card` 这些**召回叙述面**
// 不做闸门判定——它们报的是"库里有什么、它当时是什么形态"（`recall.rs` 档头同一条），
// stale 的读取侧判定落在**注入面** `gate::probe_view`（`crates/rc-search/tests/reuse_rank.rs`
// 的 `stale_反馈进_stale_判定且不再出声` 钉它）与 `view=json` 的 `form`/`silence_reason`。
// 把这条判定塞进 brief 会改掉"brief 与 view=json 同批同行"的既有契约。
