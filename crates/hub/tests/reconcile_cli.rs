//! H7c1：`hub reconcile memory` 的真进程面（派发档 §4）。
//!
//! 接管现场的造法：两个提交（`commit_seq` 1、2），再把 ledger 的记忆复制水位按到 1——
//! 那一格是 **ledger 内部状态**（`memory_replication_state.last_commit_seq`，没有面向测试的
//! 写入口），所以这里直接 SQL，与 `replicate::apply_memory` 写的是同一行。
//!
//! 四条用例：无缺口全 0；有缺口只读数不动行；`--apply` 只标缺口内 `done` 的行且幂等；
//! 截断在 CLI 面只断言 `truncated=false`（>limit 的造法不现实，limit 由 helper 单测覆盖）。
use rc_contract::{compute_body_fp, compute_turn_uid, NormalizedTurn, SessionId, TurnTier};
use rc_ledger::pass_state::{self, PassKey, PassState};
use rc_ledger::transcript::insert_batch_committed;
use rc_store::Root;
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Output};

const DATA: &str = "data";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(dir: &Path, args: &[&str]) -> Output {
    let data = dir.join(DATA);
    let mut full: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    full.push("--data-dir".into());
    full.push(data.to_str().unwrap().to_string());
    Command::new(bin()).args(&full).output().expect("hub binary")
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap()
}

fn stdout_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON")
}

fn stderr_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn session() -> SessionId {
    SessionId {
        session_key: "s-reconcile".into(),
        device_id: "dev-reconcile".into(),
        source_agent: "qoder".into(),
        machine: None,
        project_hint: None,
        last_saved_at_ms: None,
        owner_principal_id: None,
        principal_name: None,
        principal_source: None,
    }
}

fn turn_of(s: &SessionId, body: &str, created_at_ms: i64) -> NormalizedTurn {
    NormalizedTurn {
        tier: TurnTier::ContentOnly,
        turn_uid: compute_turn_uid(&s.device_id, &s.session_key, "human", body, created_at_ms),
        body: body.into(),
        body_fp: compute_body_fp(body),
        role: "human".into(),
        created_at_ms,
    }
}

/// 两个提交，返回 `(第一轮的 turn_uid, 第二轮的 turn_uid)`。运行时同时核对右端点就是 2。
fn two_commits(root: &mut Root) -> (String, String) {
    let s = session();
    let first = turn_of(&s, "alpha", 100);
    let second = turn_of(&s, "beta", 200);
    let uids = (first.turn_uid.clone(), second.turn_uid.clone());
    insert_batch_committed(root.transcript_mut(), &s, &[first], 100, "import").unwrap();
    insert_batch_committed(root.transcript_mut(), &s, &[second], 200, "import").unwrap();
    assert_eq!(
        hub::replicate::status(root.transcript()).unwrap().last_commit_seq,
        2,
        "缺口读数的右端点必须是第二个提交"
    );
    uids
}

/// 把记忆快照水位按到 `seq`——ledger 内部状态（见文件头），直接 SQL 造接管现场。
fn set_memory_seq(root: &Root, seq: u64) {
    hub::replicate::memory_apply_status(root.ledger()).unwrap();
    root.ledger()
        .conn()
        .execute(
            "UPDATE memory_replication_state SET last_commit_seq=?1, last_snapshot_sha256='sha-test', last_verified_at_ms=1 WHERE id=1",
            rusqlite::params![seq as i64],
        )
        .unwrap();
}

fn key<'a>(turn_uid: &'a str, pass_id: &'a str, family: &'a str) -> PassKey<'a> {
    PassKey { turn_uid, pass_id, family }
}

/// 一条 `pending` 行（抽取跑过、失败退避中）：`mark_running` + `note_attempt`。
fn pending_row(root: &Root, k: PassKey<'_>, now_ms: i64) {
    pass_state::mark_running(root.ledger(), k, "fp-1", "run-1", now_ms).unwrap();
    pass_state::note_attempt(root.ledger(), k, 60_000, now_ms).unwrap();
}

fn record(root: &Root, k: PassKey<'_>) -> pass_state::PassRecord {
    pass_state::get(root.ledger(), k).unwrap().expect("状态行不存在")
}

// ── 1. 无缺口 ────────────────────────────────────────────────────────────────

#[test]
fn a_fresh_store_reports_no_gap() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["reconcile", "memory", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    assert_eq!(
        stdout_json(&out),
        json!({
            "memory_commit_seq": 0,
            "transcript_commit_seq": 0,
            "gap_turns": 0,
            "truncated": false,
            "by_state": [],
            "done_turns": 0,
            "marked_stale": 0,
            "ledger_review_max_decision_id": 0,
            "local_review_max_decision_id": 0,
            "not_a_replica": true,
        })
    );

    // 文本形：`by_state` 空写 `-`。
    let out = run(dir.path(), &["reconcile", "memory"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    assert_eq!(
        stdout_text(&out).trim(),
        "memory_seq=0 transcript_seq=0 gap_turns=0 truncated=false done_turns=0 \
         marked_stale=0 by_state=- ledger_review_max_decision_id=0 \
         local_review_max_decision_id=0 not_a_replica=true"
    );
}

// ── 2. 有缺口 + 只读 ─────────────────────────────────────────────────────────

#[test]
fn a_readonly_run_reports_the_gap_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(&dir.path().join(DATA)).unwrap();
    let (_first, second) = two_commits(&mut root);
    set_memory_seq(&root, 1);
    // 缺口（第二轮）内：一条 fact/promise done、一条 naming pending。
    pass_state::mark_terminal(
        root.ledger(),
        key(&second, "fact", "promise"),
        PassState::Done,
        Some("ok"),
        500,
    )
    .unwrap();
    pending_row(&root, key(&second, "naming", ""), 510);
    drop(root);

    let out = run(dir.path(), &["reconcile", "memory", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    assert_eq!(
        stdout_json(&out),
        json!({
            "memory_commit_seq": 1,
            "transcript_commit_seq": 2,
            "gap_turns": 1,
            "truncated": false,
            "by_state": [["done", 1], ["pending", 1]],
            "done_turns": 1,
            "marked_stale": 0,
            "ledger_review_max_decision_id": 0,
            "local_review_max_decision_id": 0,
            "not_a_replica": false,
        })
    );

    // 文本形同字段（by_state 按 slug 字典序）。
    let out = run(dir.path(), &["reconcile", "memory"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    assert_eq!(
        stdout_text(&out).trim(),
        "memory_seq=1 transcript_seq=2 gap_turns=1 truncated=false done_turns=1 \
         marked_stale=0 by_state=done=1,pending=1 ledger_review_max_decision_id=0 \
         local_review_max_decision_id=0 not_a_replica=false"
    );

    // 只读：状态行一个字节都不许变。
    let root = Root::open(&dir.path().join(DATA)).unwrap();
    let done = record(&root, key(&second, "fact", "promise"));
    assert_eq!(done.state, PassState::Done);
    assert_eq!(done.reason.as_deref(), Some("ok"));
    assert_eq!(record(&root, key(&second, "naming", "")).state, PassState::Pending);
}

// ── 3. --apply ───────────────────────────────────────────────────────────────

#[test]
fn apply_marks_only_done_rows_in_the_gap_stale_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(&dir.path().join(DATA)).unwrap();
    let (first, second) = two_commits(&mut root);
    set_memory_seq(&root, 1);
    // 缺口**外**（第一轮）一条 done：--apply 不许碰它（缺口区间的左端点是开的）。
    pass_state::mark_terminal(
        root.ledger(),
        key(&first, "fact", "plan"),
        PassState::Done,
        Some("ok"),
        400,
    )
    .unwrap();
    // 缺口内：两条 done（fact 的两个 family 各一条）+ 一条 pending。
    for family in ["decision", "promise"] {
        pass_state::mark_terminal(
            root.ledger(),
            key(&second, "fact", family),
            PassState::Done,
            Some("ok"),
            500,
        )
        .unwrap();
    }
    pending_row(&root, key(&second, "naming", ""), 510);
    drop(root);

    // 第一轮 --apply：两条 done 行 → stale(memory_gap)；pending 不动。
    let out = run(dir.path(), &["reconcile", "memory", "--apply", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    assert_eq!(
        stdout_json(&out),
        json!({
            "memory_commit_seq": 1,
            "transcript_commit_seq": 2,
            "gap_turns": 1,
            "truncated": false,
            "by_state": [["done", 2], ["pending", 1]],
            "done_turns": 1,
            "marked_stale": 2,
            "ledger_review_max_decision_id": 0,
            "local_review_max_decision_id": 0,
            "not_a_replica": false,
        })
    );

    let root = Root::open(&dir.path().join(DATA)).unwrap();
    for family in ["decision", "promise"] {
        let r = record(&root, key(&second, "fact", family));
        assert_eq!(r.state, PassState::Stale, "fact/{family} 应已标 stale");
        assert_eq!(r.reason.as_deref(), Some("memory_gap"));
    }
    let naming = record(&root, key(&second, "naming", ""));
    assert_eq!(naming.state, PassState::Pending, "pending 行没有可缺的行，不许动");
    let outside = record(&root, key(&first, "fact", "plan"));
    assert_eq!(outside.state, PassState::Done, "缺口外的 done 行不许碰");
    drop(root);

    // 再跑一次：状态已是 stale，`marked_stale=0`；读数变 stale 计数。
    let out = run(dir.path(), &["reconcile", "memory", "--apply", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    assert_eq!(
        stdout_json(&out),
        json!({
            "memory_commit_seq": 1,
            "transcript_commit_seq": 2,
            "gap_turns": 1,
            "truncated": false,
            "by_state": [["pending", 1], ["stale", 2]],
            "done_turns": 0,
            "marked_stale": 0,
            "ledger_review_max_decision_id": 0,
            "local_review_max_decision_id": 0,
            "not_a_replica": false,
        })
    );
}

// ── 4. 存储失败 ⇒ 退出 3、静态话 ─────────────────────────────────────────────

#[test]
fn a_store_failure_exits_three_with_a_static_line() {
    let dir = tempfile::tempdir().unwrap();
    // `--data-dir` 指向一个**文件**：开库必失败（create_dir_all 拒）。
    let file = dir.path().join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    let out = Command::new(bin())
        .args([
            "reconcile",
            "memory",
            "--data-dir",
            file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(code(&out), 3);
    assert!(stdout_text(&out).is_empty(), "失败时 stdout 必须为空");
    assert_eq!(stderr_text(&out).trim(), "error: cannot open data store");
}

// ── 5. H7c2：审查轴的两条读数与差值提示 ──────────────────────────────────────
//
// 造法：本地两条判定（最大 id = 2），ledger 水位人为置到 5；报告必须读出 (5, 2) 并提示
// `review_gap=3`。再把水位置 0 ⇒ 差值是 -2（本地比水位多）。两次都不改任何状态：
// 判定行数不变、水位行原样。

fn two_local_decisions(root: &Root) -> i64 {
    use rc_memory::review::{
        enqueue, record_decision, Decider, Decision, NewItem, ReviewStage, ReviewVerdict,
    };
    for (pass, family, payload) in [("fact", "decision", "p-1"), ("event", "", "p-2")] {
        let out = enqueue(
            root.memory(),
            &NewItem {
                source: "distill".into(),
                pass_id: pass.into(),
                family: family.into(),
                session_key: "sess-rec".into(),
                device_id: "dev-rec".into(),
                turn_uids_json: "[\"turn-rec\"]".into(),
                owner_json: "{\"owner_agent_id\":\"agent-rec\"}".into(),
                payload_json: payload.into(),
                batch_key: String::new(),
            },
            400,
        )
        .unwrap();
        record_decision(
            root.memory(),
            out.id,
            &Decision {
                stage: ReviewStage::Human,
                verdict: ReviewVerdict::Reject,
                reason_code: "operator_reject".into(),
                rules_version: "review-gate-v0.1".into(),
                model: String::new(),
                prompt_version: String::new(),
                input_sha256: String::new(),
                review_input_sha256: String::new(),
                decider: Decider::Human,
                detail: "operator".into(),
            },
            410,
        )
        .unwrap();
    }
    rc_memory::review::max_decision_id(root.memory()).unwrap()
}

fn put_mark(root: &Root, pass: &str, max_decision_id: i64) {
    rc_ledger::review_mark::put(
        root.ledger(),
        &rc_ledger::review_mark::ReviewWatermark {
            pass_id: pass.into(),
            max_decision_id,
            pending_review: 0,
            quarantined: 0,
            applied: 0,
            updated_at_ms: 420,
        },
    )
    .unwrap();
}

#[test]
fn the_report_compares_the_ledger_watermark_with_the_local_decision_max() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(&dir.path().join(DATA)).unwrap();
    two_commits(&mut root);
    set_memory_seq(&root, 1);
    let local_max = two_local_decisions(&root);
    assert_eq!(local_max, 2, "两条判定 ⇒ 最大 id 2");
    // 水位按 pass 分行，报告取全 pass 的最大值：5。
    put_mark(&root, "fact", 5);
    put_mark(&root, "naming", 3);
    drop(root);

    let out = run(dir.path(), &["reconcile", "memory", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["ledger_review_max_decision_id"], 5, "取全 pass 的最大值：{doc}");
    assert_eq!(doc["local_review_max_decision_id"], 2, "{doc}");

    let out = run(dir.path(), &["reconcile", "memory"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let text = stdout_text(&out);
    assert!(text.contains("ledger_review_max_decision_id=5"), "{text}");
    assert!(text.contains("local_review_max_decision_id=2"), "{text}");
    assert!(text.trim_end().ends_with("review_gap=3"), "5 - 2 的差值必须提示：{text}");

    // 水位置 0：差值是负数（本地比水位多），提示照出现。
    {
        let root = Root::open(&dir.path().join(DATA)).unwrap();
        put_mark(&root, "fact", 0);
        put_mark(&root, "naming", 0);
    }
    let out = run(dir.path(), &["reconcile", "memory"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let text = stdout_text(&out);
    assert!(text.contains("ledger_review_max_decision_id=0"), "{text}");
    assert!(text.trim_end().ends_with("review_gap=-2"), "0 - 2 的差值必须提示：{text}");

    // 只读：判定行数不变、水位行原样（对账不改任何状态）。
    let root = Root::open(&dir.path().join(DATA)).unwrap();
    let n: i64 = root
        .memory()
        .conn()
        .query_row("SELECT count(*) FROM review_decision", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2, "对账不许删/改判定行");
    let marks = rc_ledger::review_mark::all(root.ledger()).unwrap();
    assert_eq!(marks.len(), 2, "两条水位行都不许动：{marks:?}");
    assert!(marks.iter().all(|m| m.max_decision_id == 0));
}
