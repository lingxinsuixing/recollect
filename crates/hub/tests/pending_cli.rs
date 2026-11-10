//! W25：`hub pending` 的黑盒面（派发档 §1 的五条用例 + 两条本面自己的闸）。
//!
//! 这一面是**水位闸的仪表**：`drain-if-pending.sh` 每 10 分钟读一次，按"有没有非零格"
//! 决定跑不跑日抽干。所以这里钉的两件事都不是装饰：
//! 1. **空库是一座干净的仪表**：全 0、退出 0（`no_rows` 不是故障，也不能被读成"库里坏了"）；
//! 2. **读不出来不许念成没待办**：缺库 ⇒ 退出 3，而且**不许把库建出来**——只读装配
//!    （`Root::open_readonly`，W24a）是这一面的前提，退化成 `Root::open` 就会在每次
//!    定时器触发时建表/迁移。
use rc_ledger::pass_state::{self, PassKey, PassState};
use rc_ledger::review_mark::{self, ReviewWatermark};
use rc_ledger::water;
use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_store::Root;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const NOW: i64 = 1_700_000_000_000;

fn bin() -> &'static str { env!("CARGO_BIN_EXE_recollect-hub") }

fn data_dir(dir: &Path) -> PathBuf { dir.join("data") }

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .arg("--data-dir")
        .arg(data_dir(dir))
        .output()
        .expect("hub binary")
}

fn code(out: &Output) -> i32 { out.status.code().unwrap() }

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象")
}

fn stderr_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// 铺一轮转录（列全由 DDL 的 NOT NULL 决定，与 `rc-ledger/tests/water.rs` 的夹具同形）。
/// 走真库的 `turns` 表而不是某个写口：这一面读的就是"转录里有哪些可抽取轮"。
fn synth_turn(t: &rc_store::TranscriptHandle, uid: &str, scope_key: &str, role: &str) {
    t.conn().execute(
        "INSERT INTO turns(turn_uid, session_key, device_id, role, body, body_fp,
                           turn_key_tier, project_hint, created_at_ms, ingested_at_ms)
         VALUES(?1, ?2, 'dev-w25', ?3, '合成正文', '合成指纹', 1, NULL, 1000, 1001)",
        rusqlite::params![uid, scope_key, role],
    ).unwrap();
}

fn uid(i: usize) -> String { format!("{:064x}", i + 1) }

fn key<'a>(turn: &'a str, pass: &'a str) -> PassKey<'a> {
    PassKey { turn_uid: turn, pass_id: pass, family: "" }
}

/// 空库（三库齐全、一行都没有）。`pending` 走只读装配，所以库必须先由可写入口建出来——
/// 这一条与"缺库 ⇒ 退出 3"是同一条前提的两面（见本文件头 ②）。
fn empty_store(dir: &Path) -> PathBuf {
    let data = data_dir(dir);
    Root::open(&data).expect("建空库");
    data
}

#[test]
fn an_empty_store_reports_zeros_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    empty_store(dir.path());
    let out = run(dir.path(), &["pending"]);
    assert_eq!(code(&out), 0, "空库是读数不是故障：{}", stderr_text(&out));
    let line = String::from_utf8(out.stdout).unwrap();
    for want in ["pending_passes=0", "running_passes=0", "lag_turns=0", "scopes_with_lag=0",
                 "review_pending=0", "quarantined=0", "derive_lag_rows=0", "cursor_at_ms=0"] {
        assert!(line.contains(want), "空库必须报 {want}，实测：{line}");
    }
    // "从来没有"在文本面写成 none（与 `derive status` 同一个写法，不折成 0）。
    assert!(line.contains("last_derive_success_at_ms=none"), "{line}");
    assert!(line.contains("last_pass_success_at_ms=none"), "{line}");
}

#[test]
fn a_pending_pass_and_one_ungturned_turn_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let data = empty_store(dir.path());
    {
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), &uid(0), "s-w25", "human");
        // `note_attempt` 落的正是 `pending`（退避里的重试）——用它而不是手写 SQL：状态机的
        // 唯一写口在 `pass_state`，这里只是把"有一格在排队"做成事实。
        pass_state::note_attempt(root.ledger(), key(&uid(0), "naming"), 0, NOW).unwrap();
    }
    let out = run(dir.path(), &["pending", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v["extract"]["pending_passes"], json!(1), "{v}");
    assert_eq!(v["extract"]["running_passes"], json!(0), "{v}");
    assert_eq!(v["extract"]["lag_turns"], json!(1), "这一轮还没有终态行：{v}");
    assert_eq!(v["extract"]["scopes_with_lag"], json!(1), "dev-w25/s-w25 是唯一落后的 scope：{v}");
}

#[test]
fn a_terminal_row_clears_pending_and_a_running_row_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let data = empty_store(dir.path());
    {
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), &uid(0), "s-w25", "human");
        pass_state::note_attempt(root.ledger(), key(&uid(0), "naming"), 0, NOW).unwrap();
        // 补齐终态：那一格不再排队，那一轮也不再落后（naming 那一格已蒸完）。
        pass_state::mark_terminal(root.ledger(), key(&uid(0), "naming"), PassState::Done, None, NOW)
            .unwrap();
        // 同一轮的另一格在跑：`running` 是"有进程正在跑"的读数，与"卡住"无关，也不进 lag。
        pass_state::mark_running(root.ledger(), key(&uid(0), "event"), "fp", "run-1", NOW).unwrap();
    }
    let out = run(dir.path(), &["pending", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v["extract"]["pending_passes"], json!(0), "终态补齐后 pending 必须归 0：{v}");
    assert_eq!(v["extract"]["running_passes"], json!(1), "{v}");
    assert_eq!(v["extract"]["lag_turns"], json!(0), "这一轮有终态行 ⇒ 不落后：{v}");
    assert_eq!(v["extract"]["scopes_with_lag"], json!(0), "{v}");
    assert!(v["pass"]["last_success_at_ms"].is_number(), "终态落下时刻就是 pass 的心跳：{v}");
}

#[test]
fn review_watermark_two_pending_is_two() {
    let dir = tempfile::tempdir().unwrap();
    let data = empty_store(dir.path());
    {
        let root = Root::open(&data).unwrap();
        // 水位表是每 pass 一行、写口是覆盖：跨 pass 求和就是全库的待审/隔离条数。
        review_mark::put(root.ledger(), &ReviewWatermark {
            pass_id: "fact".into(), max_decision_id: 7, pending_review: 2, quarantined: 1,
            applied: 4, updated_at_ms: NOW,
        }).unwrap();
        review_mark::put(root.ledger(), &ReviewWatermark {
            pass_id: "event".into(), max_decision_id: 3, pending_review: 1, quarantined: 0,
            applied: 1, updated_at_ms: NOW,
        }).unwrap();
    }
    let out = run(dir.path(), &["pending", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v["review"]["pending"], json!(3), "两个 pass 的待审条数相加（2 + 1）：{v}");
    assert_eq!(v["review"]["quarantined"], json!(1), "{v}");
}

#[test]
fn derive_lag_follows_the_cursor_pair() {
    let dir = tempfile::tempdir().unwrap();
    let data = empty_store(dir.path());
    let want = {
        let root = Root::open(&data).unwrap();
        // 推导池的一行 = 有出处的活行（`derive_pool_union` 的判据，这里不复制那份 SQL）。
        put_card(root.memory(), &FactContent {
            kind: "decision".into(),
            title: "水位闸读数".into(),
            body: "这一行用来证明 derive 那一格读的是池子".into(),
            scope: Scope { kind: ScopeKind::Project, key: "proj-w25".into() },
            valid_to_ms: None,
        }, &[Provenance { turn_uid: uid(0), anchor_role: AnchorRole::Human }], NOW).unwrap();
        let want = rc_memory::reader::count_rows_since(root.memory(), None).unwrap();
        assert!(want > 0, "夹具本身要让池子非空，否则下面两条断言在空集上成立");
        // 再验一次"游标之后"那一支：游标压在所有行之后 ⇒ 余量归零，且 cursor_at_ms 报的是它。
        water::advance_settled(root.ledger(), "derive", "",
                               &water::Cursor { at_ms: NOW + 1, turn_uid: String::new() }, NOW)
            .unwrap();
        want
    };
    let out = run(dir.path(), &["pending", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v["derive"]["lag_rows"], json!(0), "游标已在池尾之后：{v}");
    assert_eq!(v["derive"]["cursor_at_ms"], json!(NOW + 1), "{v}");
    assert!(v["derive"]["last_success_at_ms"].is_number(), "推进过就有成功时刻：{v}");
    // 前面那次全量读数不作废：`want` 只用来证明池子非空（空池上"余量 0"会假绿）。
    assert!(want > 0);
}

#[test]
fn json_keys_are_exactly_the_contract_and_the_nulls_are_nulls() {
    let dir = tempfile::tempdir().unwrap();
    empty_store(dir.path());
    let out = run(dir.path(), &["pending", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    // 逐格断言（对象相等 ⇒ 多一个键、少一个键、值错一格都红）。
    // 两个 `null` 是"从来没有"（还没成功推进过、还没有终态行），不是 0 点、更不是读失败。
    assert_eq!(stdout_json(&out), json!({
        "extract": {"pending_passes": 0, "running_passes": 0, "lag_turns": 0, "scopes_with_lag": 0},
        "review": {"pending": 0, "quarantined": 0},
        "derive": {"lag_rows": 0, "cursor_at_ms": 0, "last_success_at_ms": null},
        "pass": {"last_success_at_ms": null},
    }));
    // 机器面是一行 JSON（脚本 `read` 的第一行就是它）。
    let raw = String::from_utf8(out.stdout).unwrap();
    assert_eq!(raw.trim_end().lines().count(), 1, "stdout 必须恰好一行：{raw}");
}

#[test]
fn a_missing_store_is_an_operational_error_and_is_not_created() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    let out = Command::new(bin()).args(["pending", "--json"])
        .arg("--data-dir").arg(&data).output().unwrap();
    assert_eq!(code(&out), 3, "读不出来必须是运维失败，不是一份全 0 的读数：{}", stderr_text(&out));
    assert!(!stderr_text(&out).contains("invalid invocation"), "这不是用法错误：{}", stderr_text(&out));
    assert!(!data.exists(), "只读装配不许把库建出来（`Root::open` 的迁移副作用会在这里现形）");
}
