//! A9（W34 2026-11-07）：`recollect_report` 的工具面黑盒判据——真子进程 + 真会话库，
//! 一行 JSON-RPC 进、一行出，落库读数从 `memory.db` 白盒查。
//!
//! 钉三件事：
//! ① 已知卡入队：`review_item` 出现新 `pending_review` 行，`source` 列可辨识为 `report`
//!    （队列侧的辨识位，SQL 可直接 `WHERE source='report'`）；
//! ② 重复报同一目标+同载荷：走**既有载荷去重链**（`(pass_id, family, payload_sha256)`
//!    唯一索引），`fresh=false`，不产生第二行；
//! ③ 分型：坏句柄是 Validation、幽灵目标是 Empty（`row_absent` / `turn_absent`），
//!    且被拒的报告**一行都不落**。
//!
//! 轮目标的存在性在这一面查（门面握着转录柄，守卫 6 不许 rc-agent 碰 rc-ledger）：
//! `remember` 先落一轮真轮，再报它的 `turn_uid`，是 ③ 的反面（真轮入队成功）。

mod common;
use common::*;
use serde_json::{json, Value};

/// 报告入队的时间戳（与夹具播种同一档的合成钟）。
const NOW: i64 = 1_700_000_000_000;

/// 与 `tools.rs` 的 `CENTRE_AGENT` 同名同值（不共享：它住在那个用例文件里）。
/// `common::seed` 的行由这个主语署名，读闸才放行——报告的"先认行再写"走的是同一道闸。
const CENTRE_AGENT: &str = "synthetic-agent";

/// 报告成功路径 + 去重 + 库侧三格。读者身份用 `CENTRE_AGENT`：
/// `common::seed` 的行由它署名，读闸才放行（与 `tools.rs` 的成功读数用例同一处置）。
#[test]
fn report把已知卡入队_source可辨识_重复报去重() {
    let dir = tempfile::tempdir().unwrap();
    let handle = seed(dir.path(), NOW);
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, CENTRE_AGENT));

    let args = |target: &str, category: &str| {
        json!({"target": target, "category": category, "note": "口径说反了"})
    };
    let v: Value = call_json(&mut s, 2, "recollect_report", args(&handle, "wrong_fact"));
    assert_eq!(v["fresh"], json!(true), "{v}");
    assert_eq!(v["state"], "pending_review", "{v}");
    let first_id = v["review_item_id"].as_i64().expect("回执带队列 id：{v}");
    assert_eq!(v["target"], handle, "{v}");
    assert_eq!(v["category"], "wrong_fact", "{v}");

    // 库侧三格：source / pass_id / state。这一条就是验收判据 1 的钉子。
    let root = rc_store::Root::open(dir.path()).unwrap();
    let (src, pass, state): (String, String, String) = root
        .memory()
        .conn()
        .query_row(
            "SELECT source, pass_id, state FROM review_item WHERE id=?1",
            [first_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((src.as_str(), pass.as_str(), state.as_str()),
               ("report", "report", "pending_review"),
               "报告项的辨识位不在 source 列上：({src},{pass},{state})");
    // W34 评审缺陷②的钉子：report 臂在 enqueue 之后必须补写 "report" 的审查轴水位
    // （唯一写入口 `rc_distill::review::run::write_watermark`）。不写的话 report 项
    // pending 期间实表 +1、水位 +0 ⇒ doctor 的「水位和==实表」必误报、hub pending 漏数。
    let wm = rc_ledger::review_mark::all(root.ledger()).unwrap();
    let w = wm.iter().find(|w| w.pass_id == "report")
        .expect("report 臂必须写 'report' 的审查轴水位（doctor 对账 / hub pending 的镜像源）");
    assert_eq!(w.pending_review, 1, "刚入队的报告项在水位镜像里可见：{w:?}");
    assert_eq!(w.max_decision_id, 0, "报告项没有判定：判定最大 id 仍是 0：{w:?}");
    drop(root);

    // 重复报同一目标+同载荷：既有载荷去重链生效，不出第二行（验收判据 2）。
    let again: Value = call_json(&mut s, 3, "recollect_report", args(&handle, "wrong_fact"));
    assert_eq!(again["fresh"], json!(false), "{again}");
    assert_eq!(again["review_item_id"], json!(first_id), "{again}");
    let root = rc_store::Root::open(dir.path()).unwrap();
    assert_eq!(
        root.memory().conn()
            .query_row::<i64, _, _>("SELECT count(*) FROM review_item", [], |r| r.get(0))
            .unwrap(),
        1,
        "重复报告不得产生第二行"
    );
    drop(root);

    // 类别进了载荷指纹：换类别就是另一条 item（同一目标的两类问题该分开待审）。
    let other: Value = call_json(&mut s, 4, "recollect_report", args(&handle, "stale_time"));
    assert_eq!(other["fresh"], json!(true), "{other}");
    let root = rc_store::Root::open(dir.path()).unwrap();
    assert_eq!(
        root.memory().conn()
            .query_row::<i64, _, _>("SELECT count(*) FROM review_item", [], |r| r.get(0))
            .unwrap(),
        2,
    );
}

#[test]
fn report的分型_坏句柄_幽灵目标_缺类别_被拒不落行() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), NOW);
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, CENTRE_AGENT));

    // Validation：句柄形状不合法（`MemKey::parse` 的同一道门，与 `vote` 同一族）。
    let bad = call_error_text(&mut s, 2, "recollect_report",
        json!({"target": "not-a-handle", "category": "wrong_fact"}));
    assert!(bad.contains("validation:bad_handle"), "{bad}");
    // Empty：形状合法但指不到行——被拒与"行不存在"同形（与 `vote` 的 `row_absent` 同一版）。
    let ghost = call_error_text(&mut s, 3, "recollect_report",
        json!({"target": format!("card:{}", "f".repeat(64)), "category": "wrong_fact"}));
    assert!(ghost.contains("empty:row_absent"), "{ghost}");
    // Validation：缺类别（必填格空白按"没给"，与 `required_str` 全家同一形状）。
    let no_cat = call_error_text(&mut s, 4, "recollect_report",
        json!({"target": "not-a-handle", "category": "  "}));
    assert!(no_cat.contains("missing_argument"), "{no_cat}");
    // Empty：裸 turn_uid 指不到轮（存在性在门面的转录柄上查）。
    let ghost_turn = call_error_text(&mut s, 5, "recollect_report",
        json!({"target": "f".repeat(64), "category": "wrong_time"}));
    assert!(ghost_turn.contains("empty:turn_absent"), "{ghost_turn}");

    // 四条被拒的报告一行都不落（悬空报告比没有报告更坏）。
    let root = rc_store::Root::open(dir.path()).unwrap();
    assert_eq!(
        root.memory().conn()
            .query_row::<i64, _, _>("SELECT count(*) FROM review_item", [], |r| r.get(0))
            .unwrap(),
        0,
        "被拒的报告在库里留下了行"
    );
}

/// 轮目标的成功路径：先 `remember` 落一条真轮，再报它的 `turn_uid`。
/// 这一半钉住门面的存在性检查是"认得出真轮"，不是"一律拒"。
#[test]
fn report认得出remember落的真轮并把它入队() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, "qoder-test"));
    let remembered: Value = call_json(&mut s, 1, "recollect_remember", json!({"body": BODY}));
    let uid = remembered["turn_uid"].as_str().expect("remember 回执带 turn_uid").to_string();

    let v: Value = call_json(&mut s, 2, "recollect_report",
        json!({"target": uid, "category": "wrong_time"}));
    assert_eq!(v["fresh"], json!(true), "{v}");
    let id = v["review_item_id"].as_i64().unwrap();

    let root = rc_store::Root::open(dir.path()).unwrap();
    let (src, uids): (String, String) = root
        .memory()
        .conn()
        .query_row(
            "SELECT source, turn_uids_json FROM review_item WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(src, "report");
    assert!(uids.contains(&uid), "目标轮没进可追溯列：{uids}");
}
