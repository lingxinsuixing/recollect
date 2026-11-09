//! P2 的**线上**那一半。单元侧（`tools_shape.rs`）证的是映射与形状，这里证的是
//! "缝的实现方真的在查那两个库"：`Tap` 给得出 `extracted`，`Server` 给不出就是没查。
//! 夹具全在 `common`（P-65）⇒ 本文件零 `Command::new`、零 `read_line`、零 `spawn`。
//! 白盒播种走 `rc_ledger`（mcp 的正式依赖，本机已核 `crates/mcp/Cargo.toml`），不引入 `rc-distill`（§2.3）。
mod common;
use common::{call_error_text, call_json, init, Session};
use rc_ledger::water::{self, ExtractState};
use rc_store::Root;
use serde_json::{json, Value};
use std::path::Path;

const P2_BODY: &str = "P2 缝面合成词-7c13 的一句正文";

/// 白盒写一态：绕开 mcp 自己（"抽取会发生"不是本文件要证的事）。
fn stamp(data: &Path, uid: &str, state: ExtractState, reason: Option<&str>) {
    let root = Root::open(data).unwrap();
    water::set_extract_state(root.ledger(), uid, state, reason, 1_700_000_000_000).unwrap();
}

#[test]
fn a_fresh_remember_reports_pending_on_the_wire() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, "qoder-test"));
    let out = call_json(&mut s, 2, "recollect_remember",
                        json!({"body": P2_BODY, "session_key": "sess-p2"}));
    assert_eq!(out["extract_state"], "pending", "刚写下去 ⇒ 水位库无行 ⇒ 默认读数（§2.22 连带）");
    assert_eq!(out["eta_turns"], 0, "该会话还没有游标 ⇒ 从最早的算起，而本轮自己不计（左开右开）");
    assert_eq!(out["reason"], Value::Null, "无理由时那一格是 null 不是空串");
    // P-45 的线上半边：`remember` 的读数 ≠ `capabilities` 的声明，**在这一刻同时可见**。
    let caps = call_json(&mut s, 3, "recollect_status", json!({}));
    assert_eq!(caps["extract_state"], "queryable", "声明已经改判（P-26 的 :198 那一处的对撞面）");
    assert_ne!(out["extract_state"], caps["extract_state"],
               "`mcp/tests/tools.rs:198` 那句断言的注释改写后就靠这一条活着");
}

#[test]
fn the_four_states_read_back_verbatim_through_status_turn() {
    // 四态合一条（§1 那一行按此改写）：分四条写的是同一个 `match` 的四次，测不到更多东西。
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, "qoder-test"));
    for (i, state) in [ExtractState::Pending, ExtractState::Extracted,
                       ExtractState::Rejected, ExtractState::Worthless].into_iter().enumerate() {
        let uid = call_json(&mut s, 10 + i as i64, "recollect_remember",
                            json!({"body": format!("{P2_BODY} 第{i}条"), "session_key": "sess-p2"}))
            ["turn_uid"].as_str().unwrap().to_string();
        stamp(dir.path(), &uid, state, None);
        let got = call_json(&mut s, 20 + i as i64, "recollect_status", json!({"turn": uid}));
        assert_eq!(got["extraction"]["extract_state"], state.slug(),
                   "{state:?} 穿线后被改了说法 ⇒ mcp 侧有一张翻译表");
    }
}

#[test]
fn reason_is_passed_through_while_no_body_escapes() {
    // P-46② 的线上半边：`reason` 是透传（写什么读什么），而**正文一条都不出**。
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, "qoder-test"));
    let uid = call_json(&mut s, 1, "recollect_remember",
                        json!({"body": P2_BODY, "session_key": "sess-p2"}))
        ["turn_uid"].as_str().unwrap().to_string();
    stamp(dir.path(), &uid, ExtractState::Rejected, Some("合成理由短语-甲"));
    let st = call_json(&mut s, 2, "recollect_status", json!({"turn": uid}));
    assert_eq!(st["extraction"]["reason"], "合成理由短语-甲");
    assert!(!st.to_string().contains(P2_BODY), "status 面上出现了转录正文（守卫 12）");
}

#[test]
fn a_ghost_turn_on_the_wire_is_empty_not_protocol_error() {
    // P-62 的线上半边 + 任务 10 的协议纪律：**工具级**失败走 `isError`，不走 JSON-RPC error 对象
    // （`call_error_text` 里那两句 assert 就是这条，用例只断 `kind:code` 前缀，与 `tools.rs` 同一断法）。
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, "qoder-test"));
    let t = call_error_text(&mut s, 2, "recollect_status", json!({"turn": "f".repeat(64)}));
    assert!(t.contains("empty:turn_absent"), "{t}");
    let bad = call_error_text(&mut s, 3, "recollect_status", json!({"turn": "zz"}));
    assert!(bad.contains("validation:bad_turn_handle"), "{bad}");
}
