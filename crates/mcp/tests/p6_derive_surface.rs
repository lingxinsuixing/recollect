//! P6 两格的线上判据。生产者分别是 `rc_memory::reader::count_rows_since`（T2 的函数，
//! 调用点在 `rc_agent::tools::status`）与 `rc_ledger::water::last_derive_at_ms`（本任务）
//! ——两条都不许是占位。测试面不动：它测的是 `recollect_status` 吐回来的 JSON，
//! 谁现算这一格不归它管（这正是这一版路线能沿用同一份用例的原因）。
mod common;
use common::*;
use serde_json::{json, Value};

// **本文件不声明 `NOW` 常量**（E27 ② 已改实）：三条用例的时刻分别是 `1000+i`、`9000`、`4321`、
// `7777`，没有一个用得上 1_700_000_000_000 —— 留着它是一条 `dead_code` 告警，而本计划的门禁是
// `cargo check --workspace --all-targets` 的 `^(error|warning)` = **0**（P5 那个文件里的 `NOW`
// 有真读者，这一格没有）。`put_cards` 的 `1000 + i` 与 `common::TOK` 一样是本文件自造的，
// 不引 `common` 的任何时刻常量。

fn booted(dir: &std::path::Path) -> Session {
    let mut s = Session::start(dir);
    let _ = s.ask(&init(1, "qoder-test"));
    s
}

fn status_json(s: &mut Session, id: i64) -> Value {
    serde_json::from_str(&tool_text(&s.ask(&call(id, "recollect_status", json!({}))))).unwrap()
}

/// 白盒写 n 条**互不相同**的活卡（内容寻址 ⇒ 正文重复会撞同一个 id），按写入序交回 key_string。
/// 时刻固定为 1000+i——`count_rows_since` 的游标就是 (ms, key)，用例要能点名"压在第几条上"。
fn put_cards(data: &std::path::Path, n: usize) -> Vec<String> {
    use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
    use rc_store::Root;
    let root = Root::open(data).unwrap();
    let scope = Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() };
    let mut out = Vec::new();
    for i in 0..n {
        let k = put_card(root.memory(), &FactContent {
            kind: "decision".into(),
            title: format!("{TOK} 的口径 {i}"),
            body: "余量判据的合成正文。".into(),
            scope: scope.clone(),
            valid_to_ms: None,
        }, &[Provenance { turn_uid: "a".repeat(64), anchor_role: AnchorRole::Human }],
           1000 + i as i64).unwrap();
        out.push(k.key_string());
    }
    out
}

#[test]
fn 三条活卡未推导_status_报余量三且时间_null() {
    let dir = tempfile::tempdir().unwrap();
    let keys = put_cards(dir.path(), 4);        // ms = 1000 / 1001 / 1002 / 1003
    {
        // 第 4 条写下去就退场：余量只数活行（与 `rows_since` 同口径，`derive_pool_union` 一处执法）
        let root = rc_store::Root::open(dir.path()).unwrap();
        rc_memory::retire(root.memory(),
                          &rc_memory::keys::MemKey::parse(&keys[3]).unwrap(),
                          "本条为余量判据而写", 9000).unwrap();
    }
    let out = status_json(&mut booted(dir.path()), 2);
    let obj = out.as_object().unwrap();
    assert!(obj.contains_key("derive_lag_rows"), "P6 的格必须恒在场：{out}");
    assert!(obj.contains_key("last_derive_at_ms"), "P6 的格必须恒在场：{out}");
    assert_eq!(out["derive_lag_rows"], 3, "3 活 + 1 退场 ⇒ 余量 3（死行不占数）：{out}");
    assert_eq!(out["last_derive_at_ms"], Value::Null, "derive 水位从没推进 ⇒ null，不是 0：{out}");
}

#[test]
fn 游标压在第二条_status_余量减到一且时间在场() {
    let dir = tempfile::tempdir().unwrap();
    let keys = put_cards(dir.path(), 3);
    {
        let root = rc_store::Root::open(dir.path()).unwrap();
        // 与 T4 第 4 步同形：turn_uid 位置存的是批末行的 key_string，不是真的轮 uid
        rc_ledger::water::advance(root.ledger(), "derive", "",
            &rc_ledger::water::Cursor { at_ms: 1001, turn_uid: keys[1].clone() }, 4321)
            .unwrap();
    }
    let out = status_json(&mut booted(dir.path()), 2);
    assert_eq!(out["derive_lag_rows"], 1, "游标压在 ms=1001 ⇒ 只剩 1002 一条：{out}");
    assert_eq!(out["last_derive_at_ms"], 4321, "时间读水位行的 last_success_at_ms：{out}");
}

#[test]
fn 游标推到末条_status_余量归零() {
    // 这一格的"0"必须来自真实追平，不是占位——所以它在上一条之后才有意义。
    let dir = tempfile::tempdir().unwrap();
    let keys = put_cards(dir.path(), 3);
    {
        let root = rc_store::Root::open(dir.path()).unwrap();
        rc_ledger::water::advance(root.ledger(), "derive", "",
            &rc_ledger::water::Cursor { at_ms: 1002, turn_uid: keys[2].clone() }, 7777)
            .unwrap();
    }
    let out = status_json(&mut booted(dir.path()), 2);
    assert_eq!(out["derive_lag_rows"], 0, "追平后的零要来自真实的追平：{out}");
    assert_eq!(out["last_derive_at_ms"], 7777, "{out}");
}
