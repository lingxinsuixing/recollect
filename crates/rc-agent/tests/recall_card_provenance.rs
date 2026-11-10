//! 召回可追溯性（审计 2026-10-27）的 API 面：`row_json` 必须把 card 沿
//! `pointers_json → fact → turn` 的出处报出来（而不是空数组 + 恒 0），
//! 且 anchor 行的 `as_of_ms` 不许是 0。
//!
//! 夹具纪律同 `recall_provenance.rs`：行只走公开写入口（`put_entity` / `put_fact_rows`）。

use rc_agent::recall::{recall, RecallFilter, RecallRequest};
use rc_memory::keys::{AnchorRole, Scope, ScopeKind};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_fact::{put_fact_rows, FactWriteItem};
use rc_memory::writers_x::{put_entity, BatchCtx, EntityContent};
use rc_store::Root;
use serde_json::Value;

const NOW: i64 = 1_700_000_000_000;
const SRC_MS: i64 = 1_699_999_000_000;

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "p-recall-card-prov".into() }
}

fn uid(index: usize) -> String {
    format!("{:064x}", 0x41 + index)
}

fn human(turn: char) -> Vec<rc_memory::Provenance> {
    vec![rc_memory::Provenance {
        turn_uid: turn.to_string().repeat(64), anchor_role: AnchorRole::Human }]
}

fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-recall-card-prov".into()),
        owner_agent_id: "agent-owner".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}

fn row_of<'a>(v: &'a Value, id: &str) -> &'a Value {
    v["rows"].as_array().expect("rows 必须是数组").iter()
        .find(|r| r["id"].as_str() == Some(id))
        .unwrap_or_else(|| panic!("recall 结果里没有这一行：{id} ⇒ {v}"))
}

fn recall_one(m: &rc_store::MemoryHandle, handle: &str) -> Value {
    let (req, rejected) = RecallRequest::handles(&[handle.to_string()]).unwrap();
    assert_eq!(rejected, 0, "前置：句柄现算自库里的行，不该被拒");
    serde_json::from_str(&recall(m, &req, &RecallFilter::empty(), NOW).unwrap().text()).unwrap()
}

#[test]
fn recall_reports_card_provenance_from_fact_pointers_and_omits_the_tail() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let subject = put_entity(m, &EntityContent {
        kind: "person".into(), title: "召回卡主体".into(), scope: scope(), aliases: vec![],
    }, &human('a'), NOW).unwrap().id;
    // 4 条证据 ⇒ 兜底链 4 步，JSON 只给前 3 步、省略数必须是 1（不是恒 0）。
    let evidence = vec![
        (uid(0), AnchorRole::Human), (uid(1), AnchorRole::Agent),
        (uid(2), AnchorRole::Human), (uid(3), AnchorRole::Agent),
    ];
    let item = FactWriteItem {
        subject_anchor_id: subject,
        object_anchor_id: None,
        predicate: "note".into(),
        value: Some("某件事".into()),
        category: "observation".into(),
        mode: "stated".into(),
        time_text: String::new(),
        quote: String::new(),
        evidence,
        text: "召回卡正文，带四条证据轮。".into(),
        scope: scope(),
        source_ms: SRC_MS,
    };
    put_fact_rows(m, std::slice::from_ref(&item), &BatchCtx::synthetic(), &owner(), NOW).unwrap();
    let card_id: String = m.conn().query_row(
        "SELECT id FROM card WHERE text=?1", [&item.text], |r| r.get(0)).unwrap();
    let card_handle = format!("card:{card_id}");

    let v = recall_one(m, &card_handle);
    let row = row_of(&v, &card_handle);
    let prov = row["provenance"].as_array().expect("provenance 恒是数组");
    assert_eq!(prov.len(), 3, "card 的出处必须非空且受上限 3 截断：{row}");
    let got: Vec<&str> = prov.iter()
        .map(|s| s["turn_uid"].as_str().expect("turn_uid 是串")).collect();
    assert_eq!(got, vec![uid(0).as_str(), uid(1).as_str(), uid(2).as_str()],
               "截下来的是链头三步：{prov:?}");
    assert!(prov.iter().all(|s| s["kind"] == "turn"),
            "指针兜底只许报 turn，不许编造 basis：{prov:?}");
    assert_eq!(row["provenance_omitted"], serde_json::json!(1),
               "被截 1 步必须可观测（恒 0 就是静默截断）：{row}");
    assert_eq!(row["as_of_ms"], serde_json::json!(SRC_MS.to_string()),
               "card 的非零 as_of_ms 不许被兜底改写：{row}");
}

#[test]
fn recall_anchor_row_as_of_falls_back_to_created_at() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let anchor = put_entity(m, &EntityContent {
        kind: "person".into(), title: "锚时间主体".into(), scope: scope(), aliases: vec![],
    }, &human('b'), NOW).unwrap();
    let handle = anchor.key_string();

    let v = recall_one(m, &handle);
    let row = row_of(&v, &handle);
    assert_ne!(row["as_of_ms"], serde_json::json!("0"),
               "anchor 表没有 as_of 列，读侧必须用 created_at_ms 兜底（0 过不了时间核对）：{row}");
    assert_eq!(row["as_of_ms"], serde_json::json!(NOW.to_string()),
               "兜底值就是这一行的写入钟：{row}");
}
