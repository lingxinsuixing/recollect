//! 波二刀 B：`hub review reuse-ingest` 的真进程面（派发档测试钉④⑥⑦⑧）。
//!
//! 四格钉：
//! 1. `failed` 反馈经 CLI 入队，`review list --pass reuse --json` 读到 `source='reuse'`；
//! 2. **水位镜像**：enqueue 之后 `review_watermark` 有 `pass_id='reuse'` 一行，pending 数
//!    与实表一致（`feedback-review-item-ingress-needs-watermark` 的教训）；
//! 3. 重复 ingest 不重复入队（queued=0/deduped=1）；
//! 4. 合成闭环：failed 入队 → `review decide --reject` → 队列状态闭合（pending 归零）。
//!
//! 每条用例一个临时 data-dir，跑完即弃。

use rc_memory::reuse::record_reuse;
use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_store::Root;
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

const DATA: &str = "data";
const NOW: i64 = 1_700_000_700_000;
const AGENT: &str = "agent-hub-reuse";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin()).args(args).output().expect("hub binary")
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap()
}

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON")
}

fn data_dir(dir: &Path) -> String {
    dir.join(DATA).to_str().unwrap().to_string()
}

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-hub-reuse".into() }
}

/// 造一张带 human 出处的卡 + 一条 `failed` 复用反馈。返回卡 id。
fn seed_failed(dir: &Path, title: &str) -> String {
    let root = Root::open(&dir.join(DATA)).unwrap();
    let prov = vec![Provenance { turn_uid: "f".repeat(64), anchor_role: AnchorRole::Human }];
    let key = put_card(
        root.memory(),
        &FactContent {
            kind: "note".into(),
            title: title.into(),
            body: "复核接线合成正文".into(),
            scope: scope(),
            valid_to_ms: None,
        },
        &prov,
        NOW,
    )
    .unwrap();
    record_reuse(root.memory(), &key.id, "failed", Some("闭环测试"), NOW, AGENT).unwrap();
    key.id
}

/// 钉①+②：入队 + 读数口 + 水位镜像。
#[test]
fn failed_入队_读数口可见_水位同步() {
    let dir = tempfile::tempdir().unwrap();
    let card = seed_failed(dir.path(), "复核接线甲卡");
    let dd = data_dir(dir.path());

    let out = run(&["review", "reuse-ingest", "--data-dir", &dd, "--json"]);
    assert_eq!(code(&out), 0, "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v = stdout_json(&out);
    assert_eq!(v["candidates"], 1);
    assert_eq!(v["queued"], 1);
    assert_eq!(v["deduped"], 0);
    assert_eq!(v["items"][0]["card_id"], format!("card:{card}"));
    let item_id = v["items"][0]["review_item_id"].as_i64().unwrap();

    // 读数口之一：`review list --pass reuse --json` 带着 source='reuse'。
    let out = run(&["review", "list", "--pass", "reuse", "--data-dir", &dd, "--json"]);
    assert_eq!(code(&out), 0);
    let list = stdout_json(&out);
    let rows = list.as_array().unwrap();
    assert_eq!(rows.len(), 1, "复用待审项不在 list 里：{list}");
    assert_eq!(rows[0]["id"].as_i64().unwrap(), item_id);
    assert_eq!(rows[0]["source"], "reuse");
    assert_eq!(rows[0]["pass_id"], "reuse");
    assert_eq!(rows[0]["state"], "pending_review");

    // 读数口之二：`review stats --json` 的 by_pass 里有 reuse 一格。
    let out = run(&["review", "stats", "--data-dir", &dd, "--json"]);
    let stats = stdout_json(&out);
    let reuse = stats["by_pass"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["pass_id"] == "reuse")
        .unwrap_or_else(|| panic!("by_pass 里没有 reuse 格：{stats}"));
    assert_eq!(reuse["pending_review"], 1);
    // 读数取证（`-- --nocapture` 可见）。
    println!("reuse-ingest json: {v}");
    println!("review list --pass reuse --json: {list}");
    println!("review stats by_pass reuse: {reuse}");

    // 水位镜像：实表 +1 的同时水位 +1（否则 doctor 的「水位和==实表」会误报）。
    let root = Root::open(&dir.path().join(DATA)).unwrap();
    let marks = rc_ledger::review_mark::all(root.ledger()).unwrap();
    let mark = marks
        .iter()
        .find(|m| m.pass_id == "reuse")
        .unwrap_or_else(|| panic!("reuse 水位没写：{marks:?}"));
    assert_eq!(mark.pending_review, 1, "水位没镜像实表");
}

/// 钉③：重复 ingest 不重复入队（在途项查重）。
#[test]
fn 重复_ingest_不重复入队() {
    let dir = tempfile::tempdir().unwrap();
    let _ = seed_failed(dir.path(), "复核接线乙卡");
    let dd = data_dir(dir.path());
    let first = stdout_json(&run(&["review", "reuse-ingest", "--data-dir", &dd, "--json"]));
    assert_eq!(first["queued"], 1);
    let again = stdout_json(&run(&["review", "reuse-ingest", "--data-dir", &dd, "--json"]));
    assert_eq!(again["queued"], 0);
    assert_eq!(again["deduped"], 1);
    let list = stdout_json(&run(&["review", "list", "--pass", "reuse", "--data-dir", &dd, "--json"]));
    assert_eq!(list.as_array().unwrap().len(), 1, "重复 ingest 长出了第二条待审项：{list}");
}

/// 钉④：合成闭环——failed 入队 → 人工判定（`review decide --reject`）→ 队列闭合。
///
/// 本周判据（设计档的"至少一条 failed 走完 attention→复核闭环"）用这条合成闭环代偿：
/// 真实流量等下一周读数。
#[test]
fn 合成闭环_failed_入队到人工判定闭合() {
    let dir = tempfile::tempdir().unwrap();
    let _ = seed_failed(dir.path(), "复核接线丙卡");
    let dd = data_dir(dir.path());
    let ingest = stdout_json(&run(&["review", "reuse-ingest", "--data-dir", &dd, "--json"]));
    let item_id = ingest["items"][0]["review_item_id"].as_i64().unwrap();
    let id = item_id.to_string();

    let out = run(&[
        "review", "decide", &id, "--reject", "--reason", "wrong_card", "--data-dir", &dd, "--json",
    ]);
    assert_eq!(code(&out), 0, "decide 失败：{}", String::from_utf8_lossy(&out.stderr));
    let decided = stdout_json(&out);
    assert_eq!(decided["state"], "rejected");

    let stats = stdout_json(&run(&["review", "stats", "--data-dir", &dd, "--json"]));
    let reuse = stats["by_pass"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["pass_id"] == "reuse")
        .unwrap_or_else(|| panic!("by_pass 里没有 reuse 格：{stats}"));
    assert_eq!(reuse["pending_review"], 0, "闭环之后 pending 必须归零");
    assert_eq!(reuse["rejected"], 1, "人工拒绝要落在 reuse 那一格上");

    // 水位随人工判定再写一次：pending 归零、实表与水位一致。
    let root = Root::open(&dir.path().join(DATA)).unwrap();
    let marks = rc_ledger::review_mark::all(root.ledger()).unwrap();
    let mark = marks.iter().find(|m| m.pass_id == "reuse").unwrap();
    assert_eq!(mark.pending_review, 0, "人工判定之后水位没跟上");
}

/// 没有负向反馈时命令也是干净的零读数（空结果不是故障）。
#[test]
fn 无候选时零读数() {
    let dir = tempfile::tempdir().unwrap();
    // 只有一张卡、一条 worked：程序判定为"不进队列"。
    {
        let root = Root::open(&dir.path().join(DATA)).unwrap();
        let prov = vec![Provenance { turn_uid: "f".repeat(64), anchor_role: AnchorRole::Human }];
        let key = put_card(
            root.memory(),
            &FactContent {
                kind: "note".into(),
                title: "复核接线丁卡".into(),
                body: "复核接线合成正文二".into(),
                scope: scope(),
                valid_to_ms: None,
            },
            &prov,
            NOW,
        )
        .unwrap();
        record_reuse(root.memory(), &key.id, "worked", None, NOW, AGENT).unwrap();
    }
    let dd = data_dir(dir.path());
    let out = run(&["review", "reuse-ingest", "--data-dir", &dd, "--json"]);
    assert_eq!(code(&out), 0);
    let v = stdout_json(&out);
    assert_eq!(v["candidates"], 0);
    assert_eq!(v["queued"], 0);
    assert!(v["items"].as_array().unwrap().is_empty());
    let root = Root::open(&dir.path().join(DATA)).unwrap();
    let marks = rc_ledger::review_mark::all(root.ledger()).unwrap();
    assert!(marks.iter().all(|m| m.pass_id != "reuse"), "没有入队就不该造水位行");
}
