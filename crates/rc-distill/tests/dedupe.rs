//! 判同的八条用例分四组：**来源**（探针读派生表、候选读库，P-35）、
//! **形状**（`same` 是布尔、`keep` 是渲染过的别名，P-41）、
//! **不写**（Q8：本模块一行都不写）、**上限**（A16：第五个探针没被判，但它被数到了）。
mod common;
use common::{content_rows, one_card_payload, seed};
use rc_distill::dedupe::{run_dedupe, CaseRows};
use rc_distill::extract::{run_extract, select_batch, BatchPlan, MAX_TURNS_PER_BATCH};
use rc_distill::DistillError;
use rc_ledger::water;
use rc_llm::mock::MockBackend;
use rc_memory::MemTable;
use rc_store::Root;
use serde_json::json;

/// 两行近似重复的卡片。判同要比的就是这种形状，所以夹具必须**真的**词面重叠：
/// 标题共 3 个词、正文逐字相同 ⇒ `score_row` 的 `matched >= 1` 且 `row_len` 小 ⇒ 过 `FLOOR`。
fn dup_payload(uids: &[String]) -> String {
    json!({
        "entities": [],
        "cards": [
            {"kind":"lesson","title":"上线前先验证","body":"上次没验证就栽了",
             "valid_to_ms":null,"provenance":[uids[0]]},
            {"kind":"lesson","title":"上线前需要验证","body":"上次没验证就栽了",
             "valid_to_ms":null,"provenance":[uids[1]]}
        ],
        "events": [], "assertions": [], "worthless_turn_uids": []
    }).to_string()
}

/// 走一遍**真链路**：导入 → `select_batch` → `run_extract`（把夹具函数产出的 payload 当唯一回复）。
/// 判同的探针来源是 `rc_extract_row`，所以任何一条判同用例都要经过一次真抽取；
/// 直接往 `card` 表塞行会让 P-35 那条"影子没重建也看得见"失去意义。
/// 夹具因此是 `fn(&[String]) -> String` 而不是模板字符串：uid 由 `seed` 现给，不做占位替换。
fn extracted(dir: &std::path::Path, payload: fn(&[String]) -> String) -> (Root, BatchPlan) {
    let mut root = Root::open(dir).unwrap();
    let uids = seed(&mut root, 2, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH).unwrap().unwrap();
    let backend = MockBackend::scripted(vec![Ok(payload(&uids))]);
    run_extract(root.memory(), root.ledger(), &plan, &backend, 3_000).unwrap();
    (root, plan)
}

#[test]
fn a_near_duplicate_pair_becomes_exactly_one_plan() {
    let dir = tempfile::tempdir().unwrap();
    let (root, plan) = extracted(dir.path(), dup_payload);
    // 两条探针都会被判（第二条的候选是第一条），所以脚本必须有两条回复。
    // 反向也答 same:true 会写出两条互指的 plan ——那是 P-42，本条用例先把它挡住。
    let backend = MockBackend::scripted(vec![
        Ok(r#"{"same":true,"keep":"r1"}"#.to_string()),
        Ok(r#"{"same":false}"#.to_string()),
    ]);
    let out = run_dedupe(root.memory(), root.ledger(), &plan, &backend, 4_000).unwrap();
    assert_eq!(out.cases_judged, 2, "两条卡片都是本轮产出的探针");
    assert_eq!(out.plans.len(), 1, "两个方向都判同 ⇒ 会写出互相顶掉的两条 plan（P-42）");
    assert_ne!(out.plans[0].winner.id, out.plans[0].loser.id);
    assert_eq!(out.plans[0].winner.table, MemTable::Card);
    assert_eq!(backend.calls(), 2);
}

#[test]
fn five_mutual_near_duplicates_leave_the_fifth_probe_counted_not_judged() {
    // A16 的落点。这一条断的不是"上限这件事被解决了"，而是"**它被看见了**"：
    // 五个互不相同但正文同词的卡片 ⇒ 每个探针都有邻居 ⇒ 前四个进案例，第五个进 `probes_over_cap`。
    // 顺序由 SQL 的 `ORDER BY table_kind, mem_id` 定（内容寻址 id 的字典序，不是写入顺序），
    // 所以"哪一个被跳过"在同一份库上是可复现的——这正是把顺序定在 SQL 里的理由。
    fn five_cards(uids: &[String]) -> String {
        json!({"entities":[],
               "cards": (0..5).map(|i| json!({
                   "kind":"lesson","title":format!("进度第 {} 条", i),
                   "body":"同一批正文内容","valid_to_ms":null,
                   "provenance":[uids[i % uids.len()]]})).collect::<Vec<_>>(),
               "events":[],"assertions":[],"worthless_turn_uids":[]}).to_string()
    }
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, 3, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH).unwrap().unwrap();
    let extract = MockBackend::scripted(vec![Ok(five_cards(&uids))]);
    run_extract(root.memory(), root.ledger(), &plan, &extract, 3_000).unwrap();
    assert_eq!(content_rows(&root), 5, "五条卡片必须真的都写成了行，否则下面读的是空集合");

    let backend = MockBackend::scripted(vec![
        Ok(r#"{"same":false}"#.to_string()), Ok(r#"{"same":false}"#.to_string()),
        Ok(r#"{"same":false}"#.to_string()), Ok(r#"{"same":false}"#.to_string()),
    ]);
    let out = run_dedupe(root.memory(), root.ledger(), &plan, &backend, 4_000).unwrap();
    assert_eq!(out.cases_judged, 4, "MAX_DEDUPE_CASES 是硬上限");
    assert_eq!(out.probes_over_cap, 1, "第五个探针没有被判，但它必须被数到（A16 唯一的读数）");
    assert_eq!(backend.calls(), 4, "上限之外的探针不许真开口——那才是「一轮 18 次」那笔算术成立的前提");
    assert!(out.plans.is_empty());
}

#[test]
fn dedupe_writes_nothing_at_all() {
    // Q8 / §2.15 末段："谁能写 C 的列"在本阶段只有一个答案。这条断的是**运行期事实**：
    // 判同跑完，库里的 superseded_by 一列仍然全空，行一条不少。
    let dir = tempfile::tempdir().unwrap();
    let (root, plan) = extracted(dir.path(), dup_payload);
    let before = content_rows(&root);
    let backend = MockBackend::scripted(vec![
        Ok(r#"{"same":true,"keep":"r1"}"#.to_string()),
        Ok(r#"{"same":false}"#.to_string()),
    ]);
    let out = run_dedupe(root.memory(), root.ledger(), &plan, &backend, 4_000).unwrap();
    assert_eq!(out.plans.len(), 1, "有 plan 但不落笔");
    assert_eq!(content_rows(&root), before, "判同不许写任何一行的任何一列");
    let n: i64 = root.memory().conn()
        .query_row("SELECT count(*) FROM card WHERE superseded_by IS NOT NULL", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
    let n: i64 = root.memory().conn()
        .query_row("SELECT count(*) FROM card WHERE verdict <> 'unreviewed'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn the_probe_is_always_alias_r1_and_no_handle_or_fingerprint_is_rendered() {
    use rc_distill::prompt::judge_user;
    let dir = tempfile::tempdir().unwrap();
    let (root, _) = extracted(dir.path(), dup_payload);
    let ids: Vec<String> = root.memory().conn()
        .prepare("SELECT id FROM card").unwrap()
        .query_map([], |r| r.get::<_, String>(0)).unwrap()
        .map(|x| x.unwrap()).collect();
    let all = rc_memory::facts(root.memory()).unwrap();
    let probe = all.iter().find(|r| r.key.table == MemTable::Card && r.key.id == ids[0]).unwrap().clone();
    let cand = all.iter().find(|r| r.key.table == MemTable::Card && r.key.id == ids[1]).unwrap().clone();
    let case = CaseRows { probe, candidates: vec![cand] };
    let text = judge_user(&case);
    assert!(text.contains("r1 |") && text.contains("r2 |"), "别名顺序即渲染顺序：{text}");
    // 下面两格才是有牙的：`r1` 那一行必须**正是探针**、`r2` 必须**正是那一个候选**。
    // 只数 `r1 |` / `r2 |` 的话，把 `CaseRows::rows()` 改成"候选在前、探针在后"仍然全绿，
    // 而那一次翻转改的是判同的方向（模型说"保 r1"会被读成保另一条）——方向是本任务的标题级接口。
    assert!(text.contains(&format!("r1 | {} | {} | {}\n",
                                   case.probe.kind, case.probe.title, case.probe.body)),
            "r1 必须渲染的是探针：{text}");
    assert!(text.contains(&format!("r2 | {} | {} | {}\n", case.candidates[0].kind,
                                   case.candidates[0].title, case.candidates[0].body)),
            "r2 必须渲染的是那一个候选：{text}");
    // 别名 → 行的映射与渲染顺序是两条独立的缝，各自钉一遍（`find_alias` 走 `rows()`）。
    assert_eq!(case.find_alias("r1").unwrap().key, case.probe.key, "r1 解析回来必须是探针本身");
    for id in &ids {
        assert!(!text.contains(id), "行 id 不许进 prompt（§7 位置别名那条）：{text}");
    }
    assert!(!text.contains("card:"), "句柄的文本形状同样不许进 prompt");
}

#[test]
fn a_shape_error_is_not_read_as_not_the_same_thing() {
    // P-36 的判同孪生：形状错 ⇒ Err 原样上抛，一条 plan 都不产。
    // 把它折成 same:false 的症状是"判同什么都不做且看起来成功"，那比报错坏。
    let dir = tempfile::tempdir().unwrap();
    let (root, plan) = extracted(dir.path(), dup_payload);
    for bad in [r#"{"same":"true","keep":"r1"}"#, r#"{"keep":"r1"}"#, r#"{}"#, r#"not json"#,
                r#"{"same":true,"keep":1}"#, r#"{"same":true,"keep":"r9"}"#] {
        let backend = MockBackend::scripted(vec![Ok(bad.to_string())]);
        let e = run_dedupe(root.memory(), root.ledger(), &plan, &backend, 4_000).unwrap_err();
        assert!(matches!(e, DistillError::Judge(_)), "{bad} ⇒ {e}");
    }
}

#[test]
fn no_candidates_means_no_llm_call_at_all() {
    // P-19 的同一条纪律在判同面的形状：没有可比的东西就不开口，不烧配额。
    let dir = tempfile::tempdir().unwrap();
    let (root, plan) = extracted(dir.path(), one_card_payload);
    let backend = MockBackend::scripted(vec![]);   // 空脚本：一旦被调用就是 Timeout
    let out = run_dedupe(root.memory(), root.ledger(), &plan, &backend, 4_000).unwrap();
    assert_eq!((out.cases_judged, backend.calls()), (0, 0), "库里没有邻居就不该开口");
}

#[test]
fn an_entity_row_is_skipped_and_counted_not_judged() {
    // P-30：entity 既不判同也不进审核游标。
    // 物理合并后 card/event/assertion 同落一张 `card`，旧夹具里那三类内容会互为同表邻居、
    // 于是这里必然会开口；这一条测的是“实体被跳过”与“没有邻居就不开口”，
    // 所以夹具收成一条实体 + 一张卡片：实体进 skipped_entity，卡片没有同表邻居。
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, 3, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH).unwrap().unwrap();
    let payload = json!({
        "entities": [{"kind":"org","title":"只有实体","aliases":[],
                      "provenance":[uids[0]]}],
        "cards": [{"kind":"lesson","title":"唯一卡片","body":"独立正文",
                   "valid_to_ms":null,"provenance":[uids[1]]}],
        "events": [], "assertions": [], "worthless_turn_uids": []
    }).to_string();
    let backend = MockBackend::scripted(vec![Ok(payload)]);
    run_extract(root.memory(), root.ledger(), &plan, &backend, 3_000).unwrap();
    // 空脚本：任何一次开口都是 Timeout ⇒ Err。判同在这里一次都不该调用。
    let backend = MockBackend::scripted(vec![]);
    let out = run_dedupe(root.memory(), root.ledger(), &plan, &backend, 4_000).unwrap();
    assert_eq!(out.skipped_entity, 1, "实体行被跳过并计数，不是报错也不会静消失");
    assert_eq!((out.cases_judged, out.plans.len(), backend.calls()), (0, 0, 0),
               "只有一张卡片 ⇒ 它没有同表邻居，一个案例都不成立");
}

#[test]
fn the_freshly_written_rows_are_visible_without_rebuilding_the_shadow() {
    // P-35 的执法点。**故意不跑 rebuild**：跑绿它会把这条测成假绿。
    // 前半段的断言先证明影子确实是空的（于是"判同看见了邻居"不可能是走检索面），
    // 后半段证明它确实看见了刚写的那一行。
    let dir = tempfile::tempdir().unwrap();
    let (root, plan) = extracted(dir.path(), dup_payload);
    assert!(rc_index::shadow_tables(root.memory().conn()).unwrap().is_empty(),
            "影子表还没建 ⇒ 下面那次判同不可能走 rc_search");
    let backend = MockBackend::scripted(vec![
        Ok(r#"{"same":true,"keep":"r1"}"#.to_string()),
        Ok(r#"{"same":false}"#.to_string()),
    ]);
    let before = water::read(root.ledger(), "distill", &plan.water_scope).unwrap();
    let out = run_dedupe(root.memory(), root.ledger(), &plan, &backend, 4_000).unwrap();
    assert_eq!(out.cases_judged, 2, "用 rank 取候选会在这里返回 0 个案例而仍然 Ok");
    assert_eq!(water::read(root.ledger(), "distill", &plan.water_scope).unwrap(), before,
               "判同不许动游标：它一行都不写（`water::Cursor` 因此必须 `PartialEq`，见任务 3）");
}
