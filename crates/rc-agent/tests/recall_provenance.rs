//! 批次 N（证据面 甲′）的三条读者：`row_json` 的 `provenance`（两跳出处数组）与
//! `provenance_omitted`（被截的步数）。三条各守一格，缺一即整条改坏：
//! - T1（句柄臂）：`Turn` / `Basis` / `BasisTurn` 三形各自渲得出，且 `basis_exists=false`
//!   是**显式 missing**（键必须在、值必须 `false`、且不再有 `basis_turn`）；
//! - T2（查询臂——另一条臂）：每行上限 **3** 被执行、留下的是链头三条、被截数可观测；
//! - T3（句柄臂）：`role='link'` 的环节行**不**混进出处数组（那是 `chain_links` 的第四形）。
//!
//! 夹具纪律：造行只走公开写入口（`put_card` / `put_timeline_rows` / `put_chains`），
//! 出处行**不裸写 SQL**；只有"读回 event id"与"造 missing"两处用 `m.conn()`
//!（`export_doc.rs` 的同形先例：`conn()` 是 inherent 方法，不把 `rusqlite` 拉进依赖集）。

use rc_agent::recall::{recall, RecallFilter, RecallRequest};
use rc_memory::keys::{AnchorRole, Scope, ScopeKind};
use rc_memory::writers_x::{put_card, put_chains, put_timeline_rows, BatchCtx, ChainContent,
                           FactContent, TimelineContent};
use rc_memory::Provenance;
use rc_store::Root;
use serde_json::{json, Value};

const NOW: i64 = 1_700_000_000_000;

/// 按句柄在结果里找那一行。找不到即炸——那是"夹具没落库"或"recall 漏行"，不是可跳过的一格。
fn row_of<'a>(v: &'a Value, id: &str) -> &'a Value {
    v["rows"].as_array().expect("rows 必须是数组").iter()
        .find(|r| r["id"].as_str() == Some(id))
        .unwrap_or_else(|| panic!("recall 结果里没有这一行：{id} ⇒ {v}"))
}

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "p-prov".into() }
}

fn human(c: char) -> Vec<Provenance> {
    vec![Provenance { turn_uid: c.to_string().repeat(64), anchor_role: AnchorRole::Human }]
}

/// 造一张卡（出处走公开写入口）。返回句柄：`key_string()` 给 recall 用，
/// `.id` 给 `put_chains` 的 `link_ids` 用（那一族收的是裸 id，不是 `card:<id>` 形）。
fn card(m: &rc_store::MemoryHandle, title: &str, body: &str, prov: &[Provenance]) -> rc_memory::keys::MemKey {
    put_card(m, &FactContent {
        kind: "decision".into(), title: title.into(), body: body.into(),
        scope: scope(), valid_to_ms: None,
    }, prov, NOW).unwrap()
}

// ---- T1（句柄臂）：三形各自渲得出 + missing 明写 ----
#[test]
fn 三种出处形各自渲得出且缺失依据明写() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let c_turn = 'c'.to_string().repeat(64);
    let d_turn = 'd'.to_string().repeat(64);
    // c1：一条 Human 轮的出处 ⇒ `Turn` 那一形。
    let c1_key = card(m, "三形标本甲", "任何合并之前先跑全量测试。", &human('c')).key_string();
    // c2：给下面的时间线当依据行（它自己那条 Human 轮 = `BasisTurn` 那一形）。
    let c2 = put_card(m, &FactContent {
        kind: "decision".into(), title: "三形标本乙".into(),
        body: "那条旧决定后来没有落。".into(), scope: scope(), valid_to_ms: None,
    }, &human('d'), NOW).unwrap().id;
    // 一条以 c2 为锚的推导时间线：它落一行 `ref_kind='card'` 的出处 ⇒ `Basis` 那一形。
    put_timeline_rows(m, &[TimelineContent {
        title: "三形时间线".into(),
        body: "那条决定排在发布前面。".into(),
        scope: scope(), time_anchor_ms: Some(NOW + 86_400_000),
        time_anchor_id: Some(c2.clone()), time_confidence: "anchored".into(),
        basis_ids: vec![],
    }], &BatchCtx::synthetic(), NOW).unwrap();
    let event_id = m.conn().query_row("SELECT id FROM card WHERE kind='timeline'", [],
                                      |r| r.get::<_, String>(0)).unwrap();
    let event_key = format!("card:{event_id}");

    let (req, rejected) = RecallRequest::handles(&[c1_key.clone(), event_key.clone()]).unwrap();
    assert_eq!(rejected, 0, "前置：两个句柄现算自库里的行，不该有条被拒");
    let v: Value = serde_json::from_str(
        &recall(m, &req, &RecallFilter::empty(), NOW).unwrap().text()).unwrap();

    // 一句断言同时钉三形在场：c1 行 = `Turn`；event 行 = `Basis` **紧跟** `BasisTurn`。
    assert_eq!(
        (row_of(&v, &c1_key)["provenance"].clone(), row_of(&v, &event_key)["provenance"].clone()),
        (json!([{ "kind": "turn", "turn_uid": c_turn, "role": "human" }]),
         json!([{ "kind": "basis", "ref_kind": "card", "target_id": c2, "role": "human",
                  "basis_exists": true },
                { "kind": "basis_turn", "turn_uid": d_turn, "role": "human" }])),
        "三形没有同时在场（Turn / Basis / BasisTurn 缺一即整条改坏）"
    );
    // 没截 = 0，两行都要报（`provenance_omitted` 恒在场）。
    assert_eq!(row_of(&v, &c1_key)["provenance_omitted"], json!(0),
               "未截断的行必须报 0，不许缺席");
    assert_eq!(row_of(&v, &event_key)["provenance_omitted"], json!(0),
               "未截断的行必须报 0，不许缺席");

    // missing 这一格怎么造：把依据行本身删掉。`put_timeline_rows` 落库前先过 `table_of`
    //（依据 id 不在两张表 `anchor`/`card` 里 ⇒ `MemError::Sql`），所以"依据行不存在"没有生产写入口，
    // 它是防御性读数（`provenance` 没有指向这两张表的外键）——同 `export_doc.rs:121-140` 的先例。
    m.conn().execute_batch(&format!("DELETE FROM card WHERE id='{c2}'")).unwrap();
    let after: Value = serde_json::from_str(
        &recall(m, &req, &RecallFilter::empty(), NOW).unwrap().text()).unwrap();
    let p = row_of(&after, &event_key)["provenance"].as_array()
        .expect("provenance 恒是数组").clone();
    assert_eq!(p.len(), 1, "依据行已不在，第二跳却不许再报（没有'然后'就不能报）：{p:?}");
    assert_eq!(p[0]["kind"], "basis", "第一格仍是那条依据（只是它已经不在了）：{p:?}");
    // 显式 missing：键**必须在**且为 `false`——字段缺席在这里等于"库里那条东西没有依据"。
    assert_eq!(p[0].get("basis_exists"), Some(&json!(false)),
               "basis_exists=false 必须是显式在场的一格，不许字段缺席：{p:?}");
    assert!(!p.iter().any(|s| s["kind"] == "basis_turn"),
            "依据已 missing，却仍渲出 basis_turn ⇒ 指认了一个不存在的锚：{p:?}");
    assert_eq!(row_of(&after, &event_key)["provenance_omitted"], json!(0),
               "missing 不是截断，这一格必须仍是 0");
}

// ---- T2（查询臂——另一条臂）：每行出处上限 3 + 被截数可观测 ----
#[test]
fn 每行出处上限三条且被截数可观测() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    // 合成查询词（照 `recall_filter_wiring.rs` 的风格：整串只在这两张卡里出现）。
    let q = "出处上限合成词";
    let uids: Vec<String> = ['1', '2', '3', '4'].iter().map(|c| c.to_string().repeat(64)).collect();
    let prov_a: Vec<Provenance> = uids.iter().map(|u| Provenance {
        turn_uid: u.clone(), anchor_role: AnchorRole::Human }).collect();
    let prov_b: Vec<Provenance> = ['5', '6', '7'].iter().map(|c| Provenance {
        turn_uid: c.to_string().repeat(64), anchor_role: AnchorRole::Human }).collect();
    let a_key = card(m, "出处上限卡甲", q, &prov_a).key_string();
    let b_key = card(m, "出处上限卡乙", q, &prov_b).key_string();
    rc_index::rebuild(m, NOW).unwrap();

    let req = RecallRequest::query(q).unwrap();
    let v: Value = serde_json::from_str(
        &recall(m, &req, &RecallFilter::empty(), NOW).unwrap().text()).unwrap();

    // 卡 A：4 条出处 ⇒ 只给 3 条，且留下的是**链头**三条（`turn_uid` 升序前三条），不是随手三条。
    let ra = row_of(&v, &a_key);
    let pa = ra["provenance"].as_array().expect("provenance 恒是数组");
    // 期望值用**字面量 3**：上限值本身是被钉的设计（方案 §三"上限 3"），不是可替换的实现细节。
    assert_eq!(pa.len(), 3, "每行出处上限 3 没被执行（4 条全给了）：{pa:?}");
    let got: Vec<&str> = pa.iter().map(|s| s["turn_uid"].as_str().expect("turn_uid 是串")).collect();
    assert_eq!(got, vec![uids[0].as_str(), uids[1].as_str(), uids[2].as_str()],
               "截下来的不是链头三条 ⇒ 截的位置漂了（截链尾才是设计）：{pa:?}");
    assert_eq!(ra["provenance_omitted"], json!(1),
               "被截 1 步必须可观测（常量 0 就是静默截断）：{ra}");

    // 卡 B：恰好 3 条 ⇒ 边界不多截。
    let rb = row_of(&v, &b_key);
    assert_eq!(rb["provenance"].as_array().expect("provenance 恒是数组").len(), 3,
               "恰好 3 条被多截了（边界条件错）：{rb}");
    assert_eq!(rb["provenance_omitted"], json!(0),
               "恰好 3 条时被截数必须是 0：{rb}");
}

// ---- T3（句柄臂）：环节行不混进出处数组 ----
#[test]
fn 环节行不混进出处数组() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let a = card(m, "环节甲", "甲这一行是链的第一个环节。", &human('e')).id;
    let b = card(m, "环节乙", "乙这一行是链的第二个环节。", &human('f')).id;
    put_chains(m, &[ChainContent {
        mechanism: "甲先于乙是这条链的机制句".into(),
        link_ids: vec![a.clone(), b.clone()],
        basis_ids: vec![],
        causal_type: "causes".into(),
        scope: scope(),
    }], &BatchCtx::synthetic(), NOW).unwrap();
    let head = m.conn().query_row("SELECT id FROM card WHERE kind='causal'", [],
                                  |r| r.get::<_, String>(0)).unwrap();
    let head_key = format!("card:{head}");

    let (req, _) = RecallRequest::handles(std::slice::from_ref(&head_key)).unwrap();
    let v: Value = serde_json::from_str(
        &recall(m, &req, &RecallFilter::empty(), NOW).unwrap().text()).unwrap();
    let r = row_of(&v, &head_key);
    // 环节**不是**依据：它们由 `chain_links` 按 `ord` 单独读出、在 `export` 里另渲一形。
    // 少了 `reader.rs` 的 `role == "link"` 筛，这两个环节会以两个 `Basis` 对象出现在这里。
    assert_eq!(r["provenance"], json!([]),
               "环节行（role='link'）混进了出处数组 ⇒ 把链条的一步说成了'它凭什么'：{r}");
    assert_eq!(r["provenance_omitted"], json!(0),
               "空数组不是截断，这一格必须是 0：{r}");
}
