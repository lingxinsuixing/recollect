//! T10 的**接线**收据：`recall()` 查询臂上那一条 `.filter(recall_filter_match …)` 真的挂在那里。
//! `recall.rs` 里的 `filter_tests` 只测谓词本身（喂的是 `RowFacts` 字面量，不经 `recall()`），
//! 所以把整条 `.filter(...)` 从调用点删掉，它们一条都不红（E55 实测：556 全绿）。
//! 本文件全部用**真库里的行**：播种走 `put_batch`，`device_id` 由 `BatchCtx` 写进列，
//! 期望值独立取自 `rank::rank` 的读数，不从被过滤的那份输出反推。

use rc_memory::keys::{AnchorRole, MemTable, Scope, ScopeKind};
use rc_memory::reader::RecallFilter;
use rc_memory::writers_x::{put_batch, BatchCtx, WriteItem};
use rc_memory::Provenance;
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;
const QUERY: &str = "设备过滤合成词";
/// 命中同样四个查询词，只靠行长假把分数拉开：短句分高、长句分低。
const HIT: &str = "设备过滤合成词";
const FILLER: &str = "一句用来压低归一化分数的合成正文它不含任何生产文本也不来自真实会话";

fn ctx(device: &str) -> BatchCtx {
    BatchCtx { source_agent: "agent-wiring".into(), device_id: device.into() }
}

fn card(i: usize, device: &str, long: bool) -> WriteItem {
    WriteItem {
        table: MemTable::Card,
        kind: "note".into(),
        title: format!("设备过滤卡{device}{i}"),
        body: if long { format!("{HIT}{FILLER}") } else { HIT.into() },
        scope: Scope { kind: ScopeKind::Project, key: "proj-wiring".into() },
        valid_to_ms: None,
        aliases: vec![],
        prov: vec![Provenance { turn_uid: format!("{i:02}{}", "b".repeat(62)), anchor_role: AnchorRole::Human }],
    }
}

/// 三行他设备（高分）+ 二十五行本设备（低分）。高分刻意留给他设备那一族：
/// 一旦有人把顺序颠倒成"先取前 20 再过滤"，那三行会占掉名额 ⇒ 本用例当场数出 17。
fn seed(root: &Root) {
    let foreign: Vec<WriteItem> = (0..3).map(|i| card(i, "other", false)).collect();
    let home: Vec<WriteItem> = (0..25).map(|i| card(i, "home", true)).collect();
    put_batch(root.memory(), &foreign, &ctx("dev-other"), NOW).unwrap();
    put_batch(root.memory(), &home, &ctx("dev-home"), NOW).unwrap();
    rc_index::rebuild(root.memory(), NOW).unwrap();
}

fn rows(v: &serde_json::Value) -> Vec<String> {
    v["rows"].as_array().expect("rows 必须是数组")
        .iter().map(|r| r["id"].as_str().expect("每行都带句柄").to_owned()).collect()
}

#[test]
fn 设备过滤在查询臂上真的滤掉了行_正反两侧() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed(&root);

    // 前置：夹具不许空过。28 行全部命中，他设备那三行必须排在**前 20 名之内**，
    // 否则本用例对"先过滤还是先取名额"毫无判别力（空过的测试不是测试）。
    let ranked = rc_search::rank::rank(root.memory(), None, NOW, QUERY).unwrap();
    assert_eq!(ranked.len(), 28, "夹具的候选集不是 28 行");
    let foreign_ids: Vec<String> = rc_memory::reader::facts(root.memory()).unwrap()
        .into_iter().filter(|f| f.device_id == "dev-other").map(|f| f.key.key_string()).collect();
    assert_eq!(foreign_ids.len(), 3, "他设备那三行没写进 device_id 列");
    let order: Vec<String> = ranked.iter().map(|s| s.key.key_string()).collect();
    let in_top = foreign_ids.iter().filter(|id| order.iter().take(20).any(|o| o.as_str() == id.as_str())).count();
    assert_eq!(in_top, 3, "他设备那三行不在前 20 名之内 ⇒ 本用例数不出名额被谁占掉");

    let req = rc_agent::recall::RecallRequest::query(QUERY).unwrap();
    // 负一侧：带过滤器 ⇒ 他设备的行一条都不许出现，且本设备要填满 20 个名额。
    let filtered = RecallFilter { device_id: Some("dev-home".into()), ..Default::default() };
    let got = rows(&serde_json::from_str(
        &rc_agent::recall::recall(root.memory(), &req, &filtered, NOW).unwrap().text()).unwrap());
    assert_eq!(got.len(), rc_agent::recall::MAX_RECALL_ROWS, "过滤后没填满名额 ⇒ 像是先取名额再过滤");
    for id in &foreign_ids {
        assert!(!got.contains(id), "他设备的行穿透了过滤器：{id}");
    }
    // 期望值独立取自检索：过滤后的 rank 前 20，逐位对撞（顺序也在判据里）。
    let want: Vec<String> = ranked.iter()
        .filter(|s| s.facts.device_id == "dev-home")
        .take(rc_agent::recall::MAX_RECALL_ROWS)
        .map(|s| s.key.key_string()).collect();
    assert_eq!(got, want, "过滤后前 20 的 id 序列与检索读数不一致");

    // 正一侧：同一份夹具、空过滤器 ⇒ 那三行必须出现。两条合起来才排除"恒过滤/恒不过滤"。
    let none = rows(&serde_json::from_str(
        &rc_agent::recall::recall(root.memory(), &req, &RecallFilter::empty(), NOW).unwrap().text()).unwrap());
    for id in &foreign_ids {
        assert!(none.contains(id), "空过滤器把该行滤掉了：{id}");
    }
    assert_eq!(none, order.iter().take(20).cloned().collect::<Vec<_>>(), "空过滤器动了 rank 的前 20");
}

#[test]
fn 句柄臂不吃过滤_指认了的行不许被丢弃() {
    // §10 的裁定：句柄已指认行，过滤会在这一臂引入"指认了又被丢弃"的歧义。
    // 牙：把 `.filter(...)` 从查询臂挪进句柄臂 ⇒ 这里数到 `no_rows` 或空 rows。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed(&root);

    let foreign: Vec<rc_memory::keys::MemKey> = rc_memory::reader::facts(root.memory()).unwrap()
        .into_iter().filter(|f| f.device_id == "dev-other")
        .map(|f| f.key).collect();
    assert_eq!(foreign.len(), 3, "前置：他设备行得先存在");
    let target = foreign[0].key_string();
    let (req, rejected) = rc_agent::recall::RecallRequest::handles(std::slice::from_ref(&target)).unwrap();
    assert_eq!(rejected, 0, "前置：句柄现算自库里那一行，不该有条被拒");
    let filtered = RecallFilter { device_id: Some("dev-home".into()), ..Default::default() };
    let got = rows(&serde_json::from_str(
        &rc_agent::recall::recall(root.memory(), &req, &filtered, NOW).unwrap().text()).unwrap());
    assert_eq!(got, vec![target.clone()], "句柄臂上出现了过滤的读数");
}

// ── C6：`time_basis=event` 在查询臂上的接线 ──────────────────────────────────────

const EVENT_TOKEN: &str = "事件窗合成词";
const DAY: i64 = 86_400_000;

/// 三张同词卡，**记录钟都在 `NOW`**，只有 `time_json` 的事件时间分岔：
/// - A：事件时间在 `NOW` 之前一天；
/// - B：没有事件时间；
/// - C：事件时间在 `NOW` 之后一天。
fn seed_event_window(root: &Root) {
    let items: Vec<WriteItem> = ["甲", "乙", "丙"].iter().enumerate()
        .map(|(i, tag)| WriteItem {
            table: MemTable::Card,
            kind: "note".into(),
            title: format!("{EVENT_TOKEN}{tag}"),
            body: format!("{EVENT_TOKEN}的正文{tag}"),
            scope: Scope { kind: ScopeKind::Project, key: "proj-event-window".into() },
            valid_to_ms: None,
            aliases: vec![],
            prov: vec![Provenance {
                turn_uid: format!("{i:02}{}", "e".repeat(62)), anchor_role: AnchorRole::Human,
            }],
        })
        .collect();
    let keys = put_batch(root.memory(), &items, &ctx("dev-event"), NOW).unwrap();
    // `keys` 与 `items` 逐格对齐（写入口的承诺）；甲/丙各写一个事件时间。
    for (i, event) in [(0usize, Some(NOW - DAY)), (2usize, Some(NOW + DAY))] {
        let Some(k) = keys[i].as_ref() else { panic!("第 {i} 张卡被抑制了，夹具不成立") };
        root.memory().conn().execute_batch(&format!(
            "UPDATE card SET time_json='{{\"kind\":\"event_time\",\"field\":\"occurred\",\
             \"start_ms\":{event},\"end_ms\":null,\"precision\":\"minute\",\"basis\":\"explicit\"}}' \
             WHERE id='{id}'", event = event.unwrap(), id = k.id)).unwrap();
    }
    rc_index::rebuild(root.memory(), NOW).unwrap();
}

/// 同一份夹具、同一个窗，只换 `time_basis`：`event` 只留甲（事件时间在窗内），
/// `as_of`（缺省）三张都留（记录钟都是 `NOW`）——两支的分野在查询臂上真的读得出来。
#[test]
fn time_basis_event_在查询臂上按事件窗筛而缺省支不动() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed_event_window(&root);
    let req = rc_agent::recall::RecallRequest::query(EVENT_TOKEN).unwrap();
    let window = Some((NOW - 2 * DAY, NOW - DAY / 2));

    // 阴性对照的前提：三张卡都真有事件时间/没事件时间的差别，不是"检索根本没命中"。
    let probe = rc_agent::recall::recall(root.memory(), &req, &RecallFilter::empty(), NOW).unwrap();
    assert_eq!(rows(&serde_json::from_str(&probe.text()).unwrap()).len(), 3,
               "夹具不成立：三张卡没全部命中");

    let ev = RecallFilter {
        time_window: window, time_basis: Some(rc_agent::recall::TimeBasis::Event),
        ..Default::default()
    };
    let got = rows(&serde_json::from_str(
        &rc_agent::recall::recall(root.memory(), &req, &ev, NOW).unwrap().text()).unwrap());
    assert_eq!(got.len(), 1, "事件窗里只该有甲那一张：{got:?}");

    let as_of = RecallFilter { time_window: window, ..Default::default() };
    match rc_agent::recall::recall(root.memory(), &req, &as_of, NOW) {
        // 三张全出窗 ⇒ 查询臂的空读数是 `no_rows`（正常状态，不是故障）。
        Err(e) => assert_eq!(e.code, "no_rows", "缺省支在事件窗上应报 no_rows，实际：{e:?}"),
        Ok(r) => panic!("记录钟都在 NOW、窗在一天前 ⇒ 缺省支不该命中任何行：{}", r.text()),
    }
}
