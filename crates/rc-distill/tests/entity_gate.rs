//! A39/A41/A45：实体 kind 归一/拒收、别名原文回读、三个计数。
//! 全部走 `seed → select_batch → run_extract` 的真路径，不直接调私有闸门。
use rc_distill::extract::{run_extract, select_batch};
use rc_llm::mock::MockBackend;
use rc_store::Root;
mod common;
use common::seed;

fn run_one(root: &mut Root, raw: String) -> rc_distill::extract::ExtractOutcome {
    let plan = select_batch(root.transcript(), root.ledger(), 40).unwrap().unwrap();
    let backend = MockBackend::scripted(vec![Ok(raw)]);
    run_extract(root.memory(), root.ledger(), &plan, &backend, 3_000).unwrap()
}

fn payload(entities: serde_json::Value, card_uid: &str) -> String {
    serde_json::json!({
        "entities": entities,
        "cards": [{"kind":"lesson","title":"保留卡片","body":"保留正文",
                   "valid_to_ms":null,"provenance":[card_uid]}],
        "events": [], "assertions": [], "worthless_turn_uids": []
    }).to_string()
}

#[test]
fn kind_大写归一后过表_未知_kind_单条丢弃不炸批() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, 1, 1_000);
    let raw = payload(serde_json::json!([
        {"kind":"Org","title":"甲","aliases":[],"provenance":[&uids[0]]},
        {"kind":"human","title":"乙","aliases":[],"provenance":[&uids[0]]}
    ]), &uids[0]);
    let out = run_one(&mut root, raw);
    assert_eq!(out.entities_seen, 2, "模型给了两条实体，分母是 2");
    assert_eq!(out.kinds_rejected, 1, "human 不在六词内，只丢这一条");
    assert_eq!(out.aliases_dropped, 0);
    assert_eq!(out.rows_written, 2, "归一后的 Org + 卡片各一行");
    let n: i64 = root.memory().conn()
        .query_row("SELECT count(*) FROM anchor WHERE kind='org'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "大写 Org 必须归一后落成 org");
}

#[test]
fn 坏_kind_带空出处也只丢这一条_不炸批() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, 1, 1_000);
    let raw = payload(serde_json::json!([
        {"kind":"human","title":"坏 kind","aliases":[],"provenance":[]}
    ]), &uids[0]);
    let out = run_one(&mut root, raw);
    assert_eq!(out.entities_seen, 1);
    assert_eq!(out.kinds_rejected, 1);
    assert_eq!(out.rows_written, 1, "只剩卡片那一行");
    let n: i64 = root.memory().conn()
        .query_row("SELECT count(*) FROM card", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "卡片必须照落，整批不许因一条坏 kind 被拒");
}

#[test]
fn 推导保留字_causal_timeline_被拒收() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, 1, 1_000);
    let raw = payload(serde_json::json!([
        {"kind":"causal","title":"甲","aliases":[],"provenance":[&uids[0]]},
        {"kind":"timeline","title":"乙","aliases":[],"provenance":[&uids[0]]}
    ]), &uids[0]);
    let out = run_one(&mut root, raw);
    assert_eq!(out.entities_seen, 2);
    assert_eq!(out.kinds_rejected, 2);
    assert_eq!(out.rows_written, 1, "只剩卡片那一行");
}

#[test]
fn alias_不在出处原文里就丢弃_实体行照落() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    // seed 的正文是"合成第 0 句话，讲的是项目进度"
    let uids = seed(&mut root, 1, 1_000);
    let raw = payload(serde_json::json!([
        {"kind":"org","title":"合成项目",
         "aliases":["项目进度","不存在的别名"],"provenance":[&uids[0]]}
    ]), &uids[0]);
    let out = run_one(&mut root, raw);
    assert_eq!(out.entities_seen, 1);
    assert_eq!(out.kinds_rejected, 0);
    assert_eq!(out.aliases_dropped, 1, "只有不在源文里的那一个别名被丢");
    assert_eq!(out.rows_written, 2, "实体行与卡片都照落");
    let aliases: Vec<String> = root.memory().conn()
        .prepare("SELECT alias FROM alias ORDER BY alias").unwrap()
        .query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(aliases, vec!["项目进度".to_string()]);
}

#[test]
fn 三数各有生产者且进计数器() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, 1, 1_000);
    let raw = payload(serde_json::json!([
        {"kind":"human","title":"甲","aliases":["不存在"],"provenance":[&uids[0]]},
        {"kind":"concept","title":"乙","aliases":[],"provenance":[&uids[0]]}
    ]), &uids[0]);
    let _ = run_one(&mut root, raw);
    let rows = rc_ledger::counters::read_all(root.ledger()).unwrap();
    let get = |m: &str| rows.iter()
        .find(|r| r.subject_kind == "llm_job" && r.subject_id == "extract" && r.metric == m)
        .map(|r| r.count)
        .unwrap_or_else(|| panic!("metric {m} 没有生产者"));
    assert_eq!(get("entities_seen"), 2);
    assert_eq!(get("kinds_rejected"), 1);
    // 被拒收的那条实体不进别名校验，所以这里只有 concept 的 0 个坏别名
    assert_eq!(get("aliases_dropped"), 0);
}

#[test]
fn 六词到_canonical_anchor_kind_的映射是确定性的() {
    // M1 的兼容映射：抽取契约六词里只有 `project`/`tool` 与 `anchor.kind` 的 CHECK 对不上。
    // 纯函数这一格只钉映射表本身；真写入面由下一条钉。
    use rc_distill::extract::canonical_anchor_kind;
    assert_eq!(canonical_anchor_kind("project"), "org");
    assert_eq!(canonical_anchor_kind("tool"), "object");
    for k in ["person", "org", "place", "concept"] {
        assert_eq!(canonical_anchor_kind(k), k, "这四个词在两张表里同名，必须原样");
    }
    // 六词全表逐个过：映射后的结果只能是 canonical `anchor.kind` 的词表成员
    // （那张 CHECK 的词表写在 rc-store 的 ddl 里，这里按它的成员手抄一份）。
    const ANCHOR_KINDS: [&str; 9] = ["person", "agent", "org", "place", "object",
                                     "animal", "work", "event", "concept"];
    for k in ["person", "project", "org", "tool", "place", "concept"] {
        assert!(ANCHOR_KINDS.contains(&canonical_anchor_kind(k)),
                "抽取六词里的 {k} 映射后仍撞 anchor.kind 的 CHECK");
    }
}

#[test]
fn project_与_tool_走真写入路径落成_org_与_object() {
    // 这一条是映射的**落库面**：`project`/`tool` 原样写会撞 anchor.kind 的 CHECK、
    // 整批回滚；走 seed → select_batch → run_extract 的真链路，必须写成功且库里是映射后的词。
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed(&mut root, 1, 1_000);
    let raw = payload(serde_json::json!([
        {"kind":"project","title":"项目甲","aliases":[],"provenance":[&uids[0]]},
        {"kind":"tool","title":"工具乙","aliases":[],"provenance":[&uids[0]]},
        {"kind":"person","title":"人丙","aliases":[],"provenance":[&uids[0]]},
        {"kind":"org","title":"组织丁","aliases":[],"provenance":[&uids[0]]},
        {"kind":"place","title":"地点戊","aliases":[],"provenance":[&uids[0]]},
        {"kind":"concept","title":"概念己","aliases":[],"provenance":[&uids[0]]}
    ]), &uids[0]);
    let out = run_one(&mut root, raw);
    assert_eq!(out.entities_seen, 6, "六条都在契约的六词内，一条都不许被 kind 闸门丢掉");
    assert_eq!(out.kinds_rejected, 0);
    assert_eq!(out.rows_written, 7, "六条实体 + 卡片各一行：整批没有因 CHECK 回滚");
    let kinds: Vec<String> = root.memory().conn()
        .prepare("SELECT kind FROM anchor ORDER BY kind").unwrap()
        .query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(kinds,
        vec!["concept", "object", "org", "org", "person", "place"],
        "project ⇒ org、tool ⇒ object，其余原样；库里不许出现 project/tool");
    let n_project: i64 = root.memory().conn().query_row(
        "SELECT COUNT(*) FROM anchor WHERE kind IN ('project','tool')", [], |r| r.get(0)).unwrap();
    assert_eq!(n_project, 0, "撞 CHECK 的两个旧词一个都不许落进 anchor");
}
