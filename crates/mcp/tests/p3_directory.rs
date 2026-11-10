//! P3 的线上半边：`entity` 参数真的走别名解析、`turn`/`entity` 互斥在 JSON-RPC 上是
//! `validation`，以及 P-53 的 mcp 那半——子进程面与**当场**调进程内 `tools::status`
//! 的结果逐字节对撞（期望值不是两边各抄一份字面量，那抄的就是第二份期望）。
mod common;
use common::*;
use rc_agent::tools::{status, StatusOptions};
use serde_json::json;

/// mcp 的 `common` 只搬了 `tools.rs` 第 24–143 行，那里没有 `NOW`（它只有 `TOK` / `BODY`），
/// 而 `Session::start` 的 `--device-id` 与播种时刻是本文件自己要钉住的一件事。
/// 与 `p4_similar.rs`、`p5_lag_quota.rs` 同形：三个新文件各自带一份自己的时间基准，
/// 这不是第二个家——`common` 里没有那个值，家在这里就是"谁用它谁定它"。
const NOW: i64 = 1_700_000_000_000;

#[test]
fn 线上的_entity_参数走别名解析并数出六格() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, "qoder-test"));
    // `seed_person` 返回 `(title, alias_hit)` 二元组（它没有结构体：两个串、没有第三种读数，
    // 给夹具造一个只住在这个文件里的类型就是给下一次漂移留位置）。
    let (title, alias) = seed_person(dir.path(), NOW);    // common 里的新夹具（实体 + 共享出处的 card）

    let got = call_json(&mut s, 2, "recollect_status", json!({"entity": alias}));
    let arr = got["entities"].as_array().unwrap();
    assert_eq!(arr.len(), 1, "{got}");
    assert_eq!(arr[0]["name"], title);
    let mut cells: Vec<String> = arr[0].as_object().unwrap().keys().cloned().collect();
    cells.sort();
    assert_eq!(cells, ["aliases", "id", "kind", "latest_as_of_ms", "name", "refs"]);
    assert_eq!(got["entities_total"], 1);
    assert_eq!(got["entities_unlisted"], 0);
}

#[test]
fn 线上_turn_与_entity_同给是_validation_turn_and_entity() {
    // 这一条**必须**走线上（而不是只写进程内那条）：`tools/list` 的 schema 声明两个参数都可选，
    // 而"同时给"的拒绝在参数解析之后。症状差半边就会变成"进程内红、线上绿"。
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, "qoder-test"));
    seed(dir.path(), NOW);

    let text = call_error_text(&mut s, 2, "recollect_status",
        json!({"turn": "f".repeat(64), "entity": "老王"}));
    assert!(text.contains("turn_and_entity"), "{text}");
    assert!(text.contains("not both") || text.contains("either"), "错误消息要能自解释：{text}");
}

#[test]
fn 子进程面与进程内面对目录逐字节相同() {
    // P-53 的 mcp 半边 + P-64 的双比（先比 `Value`，再比 `to_string`）。
    // 期望值 = **当场**在同一份播种好的库里调 `rc_agent::tools::status(...)`（rc-agent 是 mcp 的正式依赖）。
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, "qoder-test"));
    seed_person(dir.path(), NOW);

    let root = rc_store::Root::open(dir.path()).unwrap();
    // 这里**不再** `rc_index::rebuild`：`seed_person` 已经在同一个 `memory.db` 上重建过一次
    // （影子表住在这个文件里，不在测试进程里），第二次重建改不了任何断言，只会让读这条用例的人
    // 以为"影子是测试自己养的"。
    let in_proc = status(root.memory(), None, &StatusOptions::default(), NOW).unwrap();
    let want: serde_json::Value = serde_json::from_str(&in_proc.text()).unwrap();

    let got = call_json(&mut s, 2, "recollect_status", json!({}));
    assert_eq!(got["entities"], want["entities"], "两面的目录内容漂开（守卫 3）");
    // "逐字节"这一半强度由 to_string 比较提供。P-64：这里不许被"顺手"简化成只比 Value。
    assert_eq!(got["entities"].to_string(), want["entities"].to_string());
    assert_eq!(got["entities_total"], want["entities_total"]);
    assert_eq!(got["entities_unlisted"], want["entities_unlisted"]);
}