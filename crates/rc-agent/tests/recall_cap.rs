//! `recall` 查询臂的上限与那句"还有 N 条未列出"（C3 那把尺子在 agent 面的对偶），
//! 以及 `row_json` 三个键（`form` / `silence_reason` / `verdict`）在 agent 面的**读者**。
//! 本文件的第一条用例是那句"未列出"在**整个阶段 1 里唯一一次被执行**（生产路径上
//! `ExportRequest::all()` 固定 `limit: None`），所以 Step 7 第 6 项的"写死一个数字"变异必须在这里红。
//! 后面三条用例是裁定 R41/R42 补的读者：`row_json` 里的键若没人读，"G7 在消费侧的落点"
//! 那句注释就是假承诺（任务 6 的 `opens_mouth` 就是这么翻过车的）。

use rc_memory::synth;
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

#[test]
fn 查询臂超限时报出未列出的条数而不是静默截断() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    synth::seed(root.memory(), NOW).unwrap();
    for i in 0..25 {
        synth::seed_variants(root.memory(), NOW, i).unwrap();
    }
    rc_index::rebuild(root.memory(), NOW).unwrap();

    let q = "同分合成词甲";
    // 期望值独立取自检索本身（不是从被截断的那份读数反推），这样"两处各减一次"就会当场对不上。
    let ranked = rc_search::rank::rank(root.memory(), None, NOW, q).unwrap();
    assert!(ranked.len() > rc_agent::recall::MAX_RECALL_ROWS, "夹具没越过上限，这条用例就成了空的");

    let json = rc_agent::recall::recall(root.memory(), &rc_agent::recall::RecallRequest::query(q).unwrap(), &rc_agent::recall::RecallFilter::empty(), NOW)
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&json.text()).unwrap();
    assert_eq!(
        v["rows"].as_array().expect("rows 必须是数组").len(),
        rc_agent::recall::MAX_RECALL_ROWS,
        "召回上限没被执行"
    );
    let markers: Vec<String> = v["unresolved_handles"]
        .as_array()
        .expect("unresolved_handles 必须是数组")
        .iter()
        .map(|x| x.as_str().expect("每一项都是字符串").to_owned())
        .filter(|s| s.starts_with('+'))
        .collect();
    assert_eq!(markers.len(), 1, "超限那条读数没进结果：{v}");
    let digits: String = markers[0].trim_start_matches('+').chars().take_while(|c| c.is_ascii_digit()).collect();
    let n: usize = digits.parse().expect("未列出数必须是数字");
    let marker = markers[0].clone();
    assert_eq!(n, ranked.len() - rc_agent::recall::MAX_RECALL_ROWS, "未列出的条数与检索总数对不上：{marker}");
    // 数据点之一（裁定 R37）：**截断 ⇒ `truncated` 为 `true`**。它的另一半在
    // `未截断时_truncated_是_false_而不是跟着数组形状走`——只有一条的话，
    // 生产码里把那个字段写成常量（`true` 或 `false`）都能绿，而契约字段带反真值出厂就是 C-1。
    // 断言消息只带身份值（守卫 12）：那句未列出标记，不 dump 行（行里有正文）。
    assert_eq!(v["truncated"], serde_json::json!(true), "越过了上限却报未截断：未列出标记={marker}");
}

/// `row_json` 的 `form` / `silence_reason` 两个键的**同源读者**：拿这一行自己的 facts 现算一次
/// `decide`，与 JSON 里的读数逐字对撞。这里复刻的是本面的**立场**（裁定 R42 (乙)：候选来自
/// `rank::rank`、分数真实存在，但这一面刻意让分数不参与档位，故合成 `i64::MAX`），不是 prod 的字面。
fn decide_now(m: &rc_store::MemoryHandle, id: &str, now_ms: i64) -> rc_search::gate::Decision {
    use rc_memory::keys::MemKey;
    use rc_search::rank::Scored;
    let key = MemKey::parse(id).expect("recall 输出的 id 必须是合法句柄");
    let facts = rc_memory::reader::lookup(m, &key).unwrap().expect("刚被列出来的行查不回来");
    let s = Scored { key, score: i64::MAX, matched: 0, row_len: 1, facts, reuse: Default::default() };
    rc_search::gate::decide(&s, now_ms)
}

/// 本面四个可达的沉默格（`low_score` 被合成值抹掉了，见 `recall_面数不到_low_score`）。
const REACHABLE_CELLS: [&str; 4] = ["rejected", "stale", "cooldown", "weak_anchor"];

#[test]
fn 未截断时_truncated_是_false_而不是跟着数组形状走() {
    // 数据点之二（裁定 R37）：同一份夹具、行数没越过上限 ⇒ `truncated` 必须 `false`，
    // 且未列出标记不出现。它同时也是那句"契约字段不许带着与名字相反的真值出厂"的反面样本：
    // 旧判据 `!absent.iter().any(|a| a.starts_with('+'))` 在这个数据点上会给出 `true`。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    synth::seed(root.memory(), NOW).unwrap();
    rc_index::rebuild(root.memory(), NOW).unwrap();

    let json = rc_agent::recall::recall(root.memory(), &rc_agent::recall::RecallRequest::query("同分合成词甲").unwrap(), &rc_agent::recall::RecallFilter::empty(), NOW)
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&json.text()).unwrap();
    let rows = v["rows"].as_array().expect("rows 必须是数组");
    assert!(!rows.is_empty(), "夹具里一行都没召回，这条用例就成了空的");
    assert!(rows.len() < rc_agent::recall::MAX_RECALL_ROWS, "夹具越过了上限，换小它");
    assert_eq!(v["truncated"], serde_json::json!(false), "没截断却报 truncated：列出行数={}", rows.len());
    let markers = v["unresolved_handles"].as_array().expect("unresolved_handles 必须是数组");
    assert!(markers.is_empty(), "没截断却推进了未列出标记：{markers:?}");
}

#[test]
fn 沉默行在_recall_面带出原因且与_decide_同源() {
    // 裁定 R41：`form` / `silence_reason` 这两个键得有读者。夹具用**生产 API** 压出来的冷却格
    // （`bump_cooldown` + 生产常数 `COOLDOWN_MS`），所以被测分支是真的被走到，不是构造出来的形状。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let keys = synth::seed(root.memory(), NOW).unwrap();
    for i in 0..3 {
        synth::seed_variants(root.memory(), NOW, i).unwrap();
    }
    rc_index::rebuild(root.memory(), NOW).unwrap();
    let cooled = keys[0].clone();
    let touched = rc_memory::bump_cooldown(
        root.memory(),
        std::slice::from_ref(&cooled),
        NOW,
        rc_memory::COOLDOWN_MS,
    )
    .unwrap();
    assert_eq!(touched, 1, "冷却没写进行：夹具是空的");

    let json = rc_agent::recall::recall(root.memory(), &rc_agent::recall::RecallRequest::query("同分合成词甲").unwrap(), &rc_agent::recall::RecallFilter::empty(), NOW)
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&json.text()).unwrap();
    let rows = v["rows"].as_array().expect("rows 必须是数组");
    let mut silent_reasons: Vec<String> = Vec::new();
    for r in rows {
        let id = r["id"].as_str().expect("每行都带句柄");
        let d = decide_now(root.memory(), id, NOW);
        // `form` 缺键 ⇒ 这里的 `expect` 当场炸（那就是 C2 面读不出形态，G7 的消费侧落点成空话）。
        let form = r["form"].as_str().expect("form 必须是字符串（缺键 = agent 面读不出形态）");
        assert_eq!(form, serde_json::to_value(d.form).unwrap().as_str().expect("Form 渲染成字符串"),
            "形态读数与 decide 不同源：{id}");
        // `verdict` 同理（R41 的第三个键）：四串之一，缺键就红。
        let verdict = r["verdict"].as_str().expect("verdict 必须是字符串（缺键 = 复核结论读不出来）");
        assert_eq!(verdict, d.facts.verdict.as_str(), "verdict 与库里那一列不同源：{id}");
        match d.cell {
            Some(c) => {
                // 这里也只点名 `silence_reason` 这一个键的读数，不 dump 整行（守卫 12）。
                let reason = r["silence_reason"].as_str().unwrap_or_else(|| {
                    panic!("沉默行的 silence_reason 必须是字符串：{id} ⇒ {}", r["silence_reason"])
                });
                assert_eq!(reason, c.slug(), "沉默原因与 decide 的格子不同源：{id}");
                assert!(REACHABLE_CELLS.contains(&reason), "recall 面报出了不可达的格子：{reason} ⇒ {id}");
                silent_reasons.push(reason.to_owned());
            }
            None => assert!(
                r["silence_reason"].is_null(),
                "开口的行却带了沉默原因：{id} ⇒ {}",
                r["silence_reason"]
            ),
        }
    }
    assert_eq!(silent_reasons, vec!["cooldown".to_string()], "冷却那条没被逐行带出来：{silent_reasons:?}");
}

#[test]
fn recall_面数不到_low_score_那一格只由_probe_view_报() {
    // 裁定 R42 (乙) 的 recall 面断言，与 `export_doc.rs` 那句 `- low_score: 0` 对称。
    // 五格夹具的第五格（超长行/低分）在这一面**开不了口也不算低分**：本面刻意让分数不参与档位，
    // 所以它落到"没有格子"那一档。低分只在注入面（`probe_view`，分数是真的）报出去。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    synth::seed_cells(root.memory(), NOW).unwrap();
    rc_index::rebuild(root.memory(), NOW).unwrap();

    let json = rc_agent::recall::recall(root.memory(), &rc_agent::recall::RecallRequest::query("格子测试合成词").unwrap(), &rc_agent::recall::RecallFilter::empty(), NOW)
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&json.text()).unwrap();
    let rows = v["rows"].as_array().expect("rows 必须是数组");
    assert_eq!(rows.len(), 5, "五格夹具没被逐行召回（沉默的行被滤掉了）：列出行数={}", rows.len());
    let mut reasons: Vec<String> = Vec::new();
    for r in rows {
        // 断言消息只带身份值（守卫 12）：句柄 + 这一格那一键，不 dump 整行（行里有正文）。
        let id = r["id"].as_str().expect("每行都带句柄");
        let reason = &r["silence_reason"];
        assert!(reason.is_null() || reason.is_string(), "silence_reason 只能是字符串或缺省：{id} ⇒ {reason}");
        if let Some(s) = reason.as_str() {
            assert_ne!(s, "low_score", "recall 面报出了低分格（本面分数不参与判定）：{id}");
            assert!(REACHABLE_CELLS.contains(&s), "未知格子：{s} ⇒ {id}");
            reasons.push(s.to_owned());
        }
    }
    reasons.sort();
    let mut expected = REACHABLE_CELLS.to_vec();
    expected.sort();
    assert_eq!(reasons, expected, "四个可达格没逐一点名：{reasons:?}");
}
