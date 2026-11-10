// crates/mcp/tests/fault_triage.rs
//! I-10（B4 故障可分诊）的**工具面**两格：
//! ① **故障码不认成空读数**：`is_recall_empty_readout` 只把 `no_rows|no_episode|no_edges`
//!    翻成 `isError=false`；库级故障（坏 `evidence_json`，经修法后的真故障臂）必须
//!    `isError=true` 且落 `unavailable:store_unreachable`——宿主把"库坏了"重发成洪水
//!    与把"这次没命中"重发成洪水是两件事，判据不许放宽。
//! ② **闸拒分诊的 opt-in 透传**：`deny_diag=true` 只对 `view=json` 生效，缺省时返回里
//!    没有诊断键（默认视图逐字节不变），叙述面显式拒绝。
//!
//! 播种是白盒的（真生产者 `put_card` / `put_edges` / `put_batch`，不手搓 card 行）；
//! 宿主主语与既有 mcp 夹具同族（`synthetic-agent`）——被拒行的归属故意落在
//! `other-agent` 头上，读闸按归属拒它。

mod common;
use common::{call, call_error_text, call_json, init, Session};
use rc_memory::keys::AnchorRole;
use rc_memory::writers_x::{
    put_batch, put_card, put_edges, BatchCtx, EdgeContent, FactContent, WriteItem,
};
use rc_memory::{Provenance, Scope, ScopeKind};
use serde_json::{json, Value};
use std::path::Path;

const NOW: i64 = 1_700_000_000_000;
const AGENT: &str = "synthetic-agent";
/// 被拒行的写者：宿主自报的是 `synthetic-agent` ⇒ 读闸按归属拒这一行。
const OTHER: &str = "other-agent";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-triage".into() }
}

fn prov(c: char) -> Vec<Provenance> {
    vec![Provenance { turn_uid: c.to_string().repeat(64), anchor_role: AnchorRole::Human }]
}

fn strip(handle: &str) -> String {
    handle.split_once(':').map(|(_, id)| id.to_string()).unwrap_or_else(|| handle.to_string())
}

fn centre(data: &Path) -> Session {
    let mut s = Session::start(data);
    let _ = s.ask(&init(1, AGENT));
    s
}

/// 一条**别的 agent 署名**的卡（与 `common::seed` 的卡共用查询词族之外的自有词）。
fn seed_other_card(data: &Path, title: &str) -> String {
    let root = rc_store::Root::open(data).unwrap();
    let m = root.memory();
    let item = WriteItem {
        table: rc_memory::MemTable::Card,
        kind: "note".into(),
        title: title.into(),
        body: "分诊夹具的正文。".into(),
        scope: scope(),
        valid_to_ms: None,
        aliases: vec![],
        prov: prov('b'),
    };
    let ctx = BatchCtx { source_agent: OTHER.into(), device_id: "dev-other".into() };
    let k = put_batch(m, &[item], &ctx, NOW).unwrap()[0].clone().expect("行不被抑制");
    rc_index::rebuild(m, NOW).unwrap();
    k.key_string()
}

#[test]
fn deny_diag_opt_in_按原因计数_缺省无键_brief_面拒绝() {
    let dir = tempfile::tempdir().unwrap();
    let other = seed_other_card(dir.path(), "分诊别人的卡");
    let mut s = centre(dir.path());

    // ① opt-in：被拒句柄的消费面同形（unresolved_handles），诊断逐格。
    let doc: Value = {
        let r = s.ask(&call(2, "recollect_recall",
            json!({"view": "json", "handles": [other.clone()], "deny_diag": true})));
        assert!(r["error"].is_null(), "走了 JSON-RPC error 而不是 isError：{r}");
        assert_eq!(r["result"]["isError"], json!(false),
            "句柄臂不是空读数（absent 非空），必须 Ok：{r}");
        serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };
    assert!(doc["rows"].as_array().unwrap().is_empty(), "被拒句柄不许带回正文：{doc}");
    assert_eq!(doc["unresolved_handles"], json!([other.clone()]), "消费面同形不变：{doc}");
    let diag = &doc["gate_deny_diag"];
    assert_eq!(diag["denied_total"], json!(1), "恰一次拒绝：{doc}");
    let by = diag["by_reason"].as_object().expect("by_reason 是对象");
    assert_eq!(by.len(), 8, "八个原因格逐格在场（零填充）：{by:?}");
    assert_eq!(by["private_agent"], json!(1), "归属不符落在 private_agent 那一格：{by:?}");
    assert_eq!(diag["handle_absent"], json!(0), "行在库里，不算真没有：{doc}");

    // ② 缺省：同一次调用不传 deny_diag ⇒ 返回里没有诊断键。
    let doc2: Value = call_json(&mut s, 3, "recollect_recall",
        json!({"view": "json", "handles": [other.clone()]}));
    assert!(doc2.get("gate_deny_diag").is_none(), "缺省读数不许冒出诊断键：{doc2}");

    // ③ 叙述面显式拒绝（validation/deny_diag_json_only），不悄悄忽略 opt-in。
    let text = call_error_text(&mut s, 4, "recollect_recall",
        json!({"view": "brief", "query": "分诊别人的卡", "deny_diag": true}));
    assert!(text.contains("validation:deny_diag_json_only"), "叙述面要显式拒绝：{text}");
}

/// **故障码不认成空读数**：坏 `evidence_json` 的边在库侧以 `unavailable:store_unreachable`
/// 暴露；工具面这一格要钉的是 `isError=true`——`is_recall_empty_readout` 的判据
/// （只认 `no_rows|no_episode|no_edges`）不许放宽成"所有 Empty 都不算失败"的反向（故障认成空）。
#[test]
fn 故障码_is_error_为_true_不认成空读数() {
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    let m = root.memory();
    let ctx = BatchCtx::synthetic();
    let mk = |title: &str, c: char| -> String {
        put_card(m, &FactContent {
            kind: "decision".into(), title: title.into(), body: "故障臂的正文。".into(),
            scope: scope(), valid_to_ms: None,
        }, &prov(c), NOW).unwrap().key_string()
    };
    let a = mk("故障臂甲端", 'a');
    let b = mk("故障臂乙端", 'b');
    assert_eq!(put_edges(m, &[EdgeContent {
        from_id: strip(&a), to_id: strip(&b), kind: "depends_on".into(),
        reason: "正常落边。".into(), scope: scope(), basis_ids: vec![],
    }], &ctx, NOW).unwrap(), 1);
    // 注入库数据故障：`basis` 清空 + `evidence_json` 写坏 ⇒ 旧形状会把这条边吞成
    // "两格都空不渲染"，且若是唯一一条边连 `no_edges` 都不报——真故障装成"没有"。
    m.conn().execute_batch(&format!(
        "UPDATE fact_link SET basis='', evidence_json='{{bad json'
         WHERE from_id='card:{}'", strip(&a))).unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    let mut s = centre(dir.path());

    let text = call_error_text(&mut s, 2, "recollect_recall",
        json!({"view": "json", "handles": [a.clone()], "include_edges": true}));
    assert!(text.contains("unavailable:store_unreachable"),
        "故障要以既有 Unavailable 族暴露：{text}");
    assert!(!text.contains("no_edges"), "故障不许被读成那一层没有：{text}");
}
