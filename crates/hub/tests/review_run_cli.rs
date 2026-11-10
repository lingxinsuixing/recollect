// crates/hub/tests/review_run_cli.rs
//! S3b-4a：`hub review run` 的真进程面（设计 `2026-10-26-review-queue-s3` §9/§11 的 S3b 段）。
//!
//! 照 `distill_fact_cli.rs` 的写法：先让 `distill once --pass fact` 把 item 停在
//! `pending_review`（抽取回放成功、审核回放 `{"fail":...}`），再用 `review run` 的
//! `--replies` 补一次窄审（§7.2 起两档：proposer 判成立、challenger 只找反证）。断的是七条 CLI 边界：
//!
//! 1. **pending 补写**：退出 3/0、fact 0→1 行、item applied、判定链 program/review/apply；
//! 2. **quarantined 复审**：审核坏 JSON ⇒ quarantined；`review run` pass ⇒ applied；
//! 3. **人工放行无 LLM**：`review decide --approve` 之后不带来源也能补写（惰性装配的证明）；
//! 4. **退出 4**：两条 pending + `--limit 1` ⇒ 4、`remaining_pending_review=1`；
//! 5. **quota 停止**：两条 pending + `quota_exhausted` ⇒ 3、`exhausted=true`、`processed=1`；
//! 6. **H2 approved 缺口**：approved + 载荷不可重放 ⇒ program/uncertain 推 quarantined，
//!    退出 4、`remaining_quarantined=1`（以前是静默 Ok(Approved)、退 0）；
//! 7. **H2 `--pass` 域**：本 pass 清空后，另一 pass 的遗留不许把退出码抬到 4。
//!
//! 纪律与 distill 同源：不跑真实 LLM（`--replies` 唯一来源）、stdout 不带正文/路径、
//! 退避不 sleep（本文件不靠时钟推进）。
use rc_contract::{normalize, parse};
use rc_ledger::insert_batch;
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{AnchorRole, Provenance, Scope, ScopeKind};
use rc_store::Root;
use rusqlite::params;
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Output};

const DEV: &str = "dev-review-run";
const SESS: &str = "sess-review-run";
const DATA: &str = "data";
const NOW: i64 = 1_700_000_000_000;
/// 目标轮正文：断它**不出现在** stdout/stderr（守卫 12 的零正文）。
const CANARY: &str = "我下周三之前把对账单发给你";

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

fn stdout_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn data_dir(dir: &Path) -> String {
    dir.join(DATA).to_str().unwrap().to_string()
}

fn open(dir: &Path) -> Root {
    Root::open(&dir.join(DATA)).unwrap()
}

fn count(root: &Root, table: &str) -> i64 {
    root.memory()
        .conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// 唯一一条 item 的 `(id, state)`。
fn only_item(root: &Root) -> (i64, String) {
    root.memory()
        .conn()
        .query_row("SELECT id, state FROM review_item", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap()
}

fn item_state(root: &Root, id: i64) -> String {
    root.memory()
        .conn()
        .query_row("SELECT state FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap()
}

fn decision_stages(root: &Root, id: i64) -> Vec<String> {
    let mut st = root
        .memory()
        .conn()
        .prepare("SELECT stage FROM review_decision WHERE item_id=?1 ORDER BY id")
        .unwrap();
    st.query_map([id], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// 一条 fact 状态行（promise 族）：`(state, reason)`。
fn fact_turn(root: &Root, uid: &str, family: &str) -> (String, Option<String>) {
    root.ledger()
        .conn()
        .query_row(
            "SELECT state, reason FROM extract_pass_state
             WHERE turn_uid=?1 AND pass_id='fact' AND family=?2",
            params![uid, family],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

/// 播种三轮真转录（`parse` → `normalize` → `insert_batch`），归属改成非默认值，
/// 再铺两个活锚：张三较新（`a1`）、李四较早（`a2`）。
fn seed(dir: &Path) -> Vec<String> {
    let doc = json!({
        "format_version": 1,
        "sessions": [{
            "session_key": SESS,
            "device_id": DEV,
            "source_agent": "agent-review-run",
            "turns": [
                {"role": "human", "body": CANARY, "created_at_ms": NOW},
                {"role": "agent", "body": "好的，收到", "created_at_ms": NOW + 1},
                {"role": "human", "body": "谢谢", "created_at_ms": NOW + 2},
            ]
        }]
    });
    let file = parse(doc.to_string().as_bytes()).expect("夹具必须过契约");
    let rep = normalize(file, NOW + 100_000);
    let mut root = Root::open(&dir.join(DATA)).unwrap();
    let mut uids = Vec::new();
    for (i, a) in rep.accepted.iter().enumerate() {
        insert_batch(
            root.transcript_mut(),
            &a.session,
            std::slice::from_ref(&a.turn),
            NOW + i as i64,
        )
        .unwrap();
        uids.push(a.turn.turn_uid.clone());
    }
    assert_eq!(uids.len(), 3, "夹具的条数必须真是 3");
    let changed = root
        .transcript_mut()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id='p1', owner_agent_id='alice',
                    visibility='project', retention='keep',
                    scope_kind='project', scope_key='proj-x', ownership_src='operator'
             WHERE session_key=?1 AND device_id=?2",
            params![SESS, DEV],
        )
        .unwrap();
    assert_eq!(changed, 1, "归属那一行必须真的被改到");

    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    for (canonical, created_at_ms) in [("李四", NOW - 2_000), ("张三", NOW - 1_000)] {
        put_entity(
            root.memory(),
            &EntityContent {
                kind: "person".into(),
                title: canonical.into(),
                scope: scope.clone(),
                aliases: vec![],
            },
            &[Provenance { turn_uid: uids[0].clone(), anchor_role: AnchorRole::Human }],
            created_at_ms,
        )
        .unwrap();
    }

    // 候选池/程序闸按会话 owner 判锚的可见性（隐私修复）：夹具的锚改挂会话归属。
    root.memory()
        .conn()
        .execute(
            "UPDATE resource_owner SET owner_agent_id='alice', owner_principal_id='p1',
                    visibility='project', scope_kind='project', scope_key='proj-x'
              WHERE mem_table='anchor'",
            [],
        )
        .unwrap();
    uids
}

fn extraction(family: &str) -> Value {
    let sorted = if family == "plan" { "plan" } else { "promise" };
    json!({
        "schema_version": 0, "pass": "fact", "family": family,
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a1", "predicate": sorted,
            "object": "a2", "value": "把对账单发给你", "category": sorted,
            "mode": "stated", "time_text": "下周三之前", "quote": CANARY,
            "evidence": ["t1"], "text": "张三承诺在下周三之前把对账单发给李四。"
        }],
        "empty_turns": ["t2", "t3"]
    })
}

/// 审核证据：§7.2 的证据闸要求每条 verdict 的 `evidence` 是该条 `quote` 的子串。
/// 夹具里唯一的 quote 是 `CANARY`（「我下周三之前把对账单发给你」），这段是它的子串。
const EVIDENCE: &str = "把对账单发给你";

fn review_pass(family: &str) -> Value {
    json!({
        "schema_version": 0, "pass": "fact_review", "family": family,
        "verdicts": [{"ref": "f1", "verdict": "pass", "code": null, "evidence": EVIDENCE}]
    })
}

fn reply(text: &Value) -> Value {
    Value::String(text.to_string())
}

/// 一片的三条应答（§7.2）：抽取 → proposer（判是否成立）→ challenger（只找反证）。
/// 本文件的用例断的是"这一片最终判什么"，所以两档回放同一份判决（两侧 pass ⇒ pass）。
fn reviewed(extraction: &Value, verdict: &Value) -> Vec<Value> {
    vec![reply(extraction), reply(verdict), reply(verdict)]
}

/// 一次窄审的两条应答：proposer + challenger 同判 pass。
fn review_pair(family: &str) -> Vec<Value> {
    let pass = review_pass(family);
    vec![reply(&pass), reply(&pass)]
}

fn fail(slug: &str) -> Value {
    json!({"fail": slug})
}

/// `distill once --pass fact --family <family> --replies <file>`。
fn distill(dir: &Path, family: &str, replies: &[Value]) -> Output {
    let p = dir.join("fact-replies.json");
    std::fs::write(&p, Value::Array(replies.to_vec()).to_string()).unwrap();
    let dd = data_dir(dir);
    run(&[
        "distill",
        "once",
        "--pass",
        "fact",
        "--family",
        family,
        "--replies",
        p.to_str().unwrap(),
        "--data-dir",
        &dd,
    ])
}

/// 同一条命令 + `--no-review`（S3b-4b：只抽取并入队）。
fn distill_no_review(dir: &Path, family: &str, replies: &[Value]) -> Output {
    let p = dir.join("fact-no-review-replies.json");
    std::fs::write(&p, Value::Array(replies.to_vec()).to_string()).unwrap();
    let dd = data_dir(dir);
    run(&[
        "distill",
        "once",
        "--pass",
        "fact",
        "--family",
        family,
        "--no-review",
        "--replies",
        p.to_str().unwrap(),
        "--data-dir",
        &dd,
    ])
}

/// `review run [--limit N] --replies <file> --data-dir <dd>`（`replies=None` 时不给来源）。
fn review_run(dir: &Path, limit: Option<usize>, replies: Option<&[Value]>) -> Output {
    let dd = data_dir(dir);
    let mut args: Vec<String> = vec!["review".into(), "run".into()];
    if let Some(limit) = limit {
        args.push("--limit".into());
        args.push(limit.to_string());
    }
    if let Some(replies) = replies {
        let p = dir.join("review-replies.json");
        std::fs::write(&p, Value::Array(replies.to_vec()).to_string()).unwrap();
        args.push("--replies".into());
        args.push(p.to_str().unwrap().to_string());
    }
    args.push("--data-dir".into());
    args.push(dd);
    args.push("--json".into());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run(&refs)
}

fn assert_no_body_or_path(out: &Output) {
    let text = stdout_text(out);
    assert!(!text.contains(CANARY), "stdout 带正文：{text}");
    assert!(!text.contains(DATA), "stdout 带路径：{text}");
    assert!(!stderr_text(out).contains(CANARY), "stderr 带正文");
}

/// 起一个 `review run --replies <file> --json` 子进程（**不等待**：并发用例要两个同时在跑）。
/// 回放文件按子进程各给一份，避免两边抢同一个路径。
fn spawn_review_run(dir: &Path, replies_path: &Path) -> std::process::Child {
    let dd = data_dir(dir);
    Command::new(bin())
        .args([
            "review",
            "run",
            "--replies",
            replies_path.to_str().unwrap(),
            "--data-dir",
            &dd,
            "--json",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("hub binary")
}

/// `extract_pass_state` 的完整读数快照（H1a：Store 失败不许改 ledger 的任何一格）。
fn ledger_snapshot(root: &Root) -> Vec<(String, String, String, String, Option<String>)> {
    let mut st = root
        .ledger()
        .conn()
        .prepare(
            "SELECT turn_uid, pass_id, family, state, reason FROM extract_pass_state
              ORDER BY turn_uid, pass_id, family",
        )
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn memory_scalar(root: &Root, sql: &str) -> i64 {
    root.memory().conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn ledger_scalar(root: &Root, sql: &str) -> i64 {
    root.ledger().conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

/// 造一条停在 `pending_review` 的 fact item（抽取成功、审核回放 `BackendError`）。
/// 故障落在 proposer 那次调用上：challenger 不再发，所以队列只需抽取 + 两次 `fail`
/// （`Rotating` 一次开口会重访同一条座位两次：只喂一条的话第二次落到"脚本耗尽 = 超时"，
/// 而超时在 W19c2 里是**单元级**失败、rc=0 —— 这条 helper 要的是座位级那条）。
fn pending(dir: &Path, family: &str) -> i64 {
    let out = distill(
        dir,
        family,
        &[
            reply(&extraction(family)),
            fail("backend_error"),
            fail("backend_error"),
        ],
    );
    assert_eq!(code(&out), 3, "stderr: {}", stderr_text(&out));
    let root = open(dir);
    assert_eq!(count(&root, "fact"), 0, "审核失败不许写行");
    let (id, state) = only_item(&root);
    assert_eq!(state, "pending_review");
    id
}

// ── 1. pending → run 补写 ────────────────────────────────────────────────────

/// 抽取 + 审核失败 ⇒ 退出 3、item pending、fact 0 行；`review run` 用 pass 回放 ⇒ 退出 0、
/// fact 1 行、item applied、判定链 program/review/apply。
///
/// 这同时是设计 §11（S3b）的"LLM 不可用"验收：不可用时一行不写、item 停在队列里；
/// **恢复后** `review run` 补写成功（S3b-4a 建了这一格，S3b-4b 补上"恢复后补写"的那组断言：
/// 未审的轮仍 `pending` 且退避已记，补写只改有草稿的那一轮）。
#[test]
fn a_pending_item_is_written_by_review_run() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let id = pending(dir.path(), "promise");

    let root = open(dir.path());
    assert_eq!(fact_turn(&root, &uids[0], "promise").0, "pending", "退避后轮仍是 pending");
    drop(root);

    let replies = review_pair("promise");
    let out = review_run(dir.path(), None, Some(&replies));
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["processed"], 1);
    assert_eq!(doc["applied"], 1);
    assert_eq!((doc["rejected"].as_u64(), doc["quarantined"].as_u64()), (Some(0), Some(0)));
    assert_eq!(doc["errors"], 0);
    assert_eq!(doc["exhausted"], false);
    // 观测面两格（W33-A10 · 评审缺陷②补线）：键必须在，回放座位没有真实出网 ⇒ 恒 0
    // （stub 不发明耗时；脚本侧聚合口径见 daily-drain.sh 的 parse_call_obs）。
    assert_eq!(doc["max_call_ms"], 0, "{doc}");
    assert_eq!(doc["timeouts"], 0, "{doc}");
    assert_eq!(doc["remaining_pending_review"], 0);
    assert_eq!(doc["remaining_quarantined"], 0);
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(count(&root, "fact"), 1, "补写必须落一行 fact");
    assert_eq!(item_state(&root, id), "applied");
    assert_eq!(
        decision_stages(&root, id),
        vec!["program".to_string(), "review".to_string(), "apply".to_string()],
        "判定链必须是 program/review/apply"
    );
    assert_eq!(fact_turn(&root, &uids[0], "promise"), ("done".to_string(), None));
    // 批内没有草稿的轮不被 `mark_turns` 改写（仍停在 runner 的退避态）。
    assert_eq!(fact_turn(&root, &uids[1], "promise").0, "pending");
    assert_eq!(fact_turn(&root, &uids[2], "promise").0, "pending");
    // 恢复后的补写只改有草稿的轮；未审的轮仍带着 distill 那一次写下的退避。
    let retry: Option<i64> = root
        .ledger()
        .conn()
        .query_row(
            "SELECT next_retry_at_ms FROM extract_pass_state
             WHERE turn_uid=?1 AND pass_id='fact' AND family='promise'",
            params![uids[1]],
            |r| r.get(0),
        )
        .unwrap();
    assert!(retry.is_some_and(|ms| ms > NOW), "未审的轮必须留着退避：{retry:?}");
}

// ── 1b. `distill --no-review` 两段式（S3b-4b）────────────────────────────────

/// `distill once --pass fact --no-review --replies <抽取>` ⇒ 退出 0、0 行、读数 `state=pending`；
/// `review list` 一条 pending；再 `review run --replies <审核 pass>` ⇒ 退出 0、1 行、item applied、
/// 轮 done、判定链 program/review/apply。
///
/// 同一条 item 的逐轮状态把两种终态都钉住：有草稿的轮 `pending(review_pending)`（从 distill
/// 带下来），被 `empty_turns` 覆盖的轮在 distill 那次就落 `empty`，`review run` 不改它们。
#[test]
fn no_review_distill_enqueues_and_review_run_writes_later() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());

    let out = distill_no_review(dir.path(), "promise", &[reply(&extraction("promise"))]);
    assert_eq!(code(&out), 0, "待审不是失败：stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["state"], "pending", "读数必须说这一批在等审核");
    assert_eq!(doc["turns"], 3);
    assert_eq!(doc["drafts"], 1);
    assert_eq!(doc["written"], 0);
    assert_eq!((doc["facts"].as_u64(), doc["cards"].as_u64(), doc["mentions"].as_u64()),
               (Some(0), Some(0), Some(0)));
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(count(&root, "fact"), 0, "--no-review 不许写记忆行");
    let (id, state) = only_item(&root);
    assert_eq!(state, "pending_review");
    assert_eq!(
        decision_stages(&root, id),
        vec!["program".to_string()],
        "只跑了程序闸，窄审还没开口"
    );
    assert_eq!(
        fact_turn(&root, &uids[0], "promise"),
        ("pending".to_string(), Some("review_pending".to_string())),
        "有草稿的轮等审核"
    );
    let retry: Option<i64> = root
        .ledger()
        .conn()
        .query_row(
            "SELECT next_retry_at_ms FROM extract_pass_state
             WHERE turn_uid=?1 AND pass_id='fact' AND family='promise'",
            params![uids[0]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(retry, None, "等审核不是等退避");
    for uid in &uids[1..] {
        assert_eq!(
            fact_turn(&root, uid, "promise"),
            ("empty".to_string(), None),
            "被 empty_turns 覆盖的轮沿用既有终态：{uid}"
        );
    }
    drop(root);

    // `review list` 只列元数据：一条 pending，载荷与正文一个字都不打。
    let dd = data_dir(dir.path());
    let list = run(&["review", "list", "--json", "--data-dir", &dd]);
    assert_eq!(code(&list), 0, "stderr: {}", stderr_text(&list));
    let items: Vec<Value> = serde_json::from_slice(&list.stdout).expect("list --json 是数组");
    assert_eq!(items.len(), 1, "一条 item");
    assert_eq!(items[0]["state"], "pending_review");
    assert_eq!(items[0]["pass_id"], "fact");
    assert_no_body_or_path(&list);

    // 第二段：补审 + 补写。
    let replies = review_pair("promise");
    let out = review_run(dir.path(), None, Some(&replies));
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["processed"], 1);
    assert_eq!(doc["applied"], 1);
    assert_eq!(doc["errors"], 0);
    assert_eq!(doc["remaining_pending_review"], 0);
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(count(&root, "fact"), 1, "补写必须落一行 fact");
    assert_eq!(item_state(&root, id), "applied");
    assert_eq!(
        decision_stages(&root, id),
        vec!["program".to_string(), "review".to_string(), "apply".to_string()],
        "判定链必须是 program/review/apply"
    );
    assert_eq!(fact_turn(&root, &uids[0], "promise"), ("done".to_string(), None));
    for uid in &uids[1..] {
        assert_eq!(fact_turn(&root, uid, "promise").0, "empty", "空覆盖轮不许被改写：{uid}");
    }
}

// ── 1c. time 没有窄审：`--no-review` 是用法错误（S3b-4b）────────────────────

/// `distill once --pass time --no-review` ⇒ 退出 3、一句静态用法错误、stdout 零读数。
/// time 一次模型都不调、也没有队列可进——静默忽略会让操作员以为"它在等审核"。
#[test]
fn no_review_on_time_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let dd = data_dir(dir.path());
    let out = run(&["distill", "once", "--pass", "time", "--no-review", "--data-dir", &dd]);
    assert_eq!(code(&out), 3, "stderr: {}", stderr_text(&out));
    assert!(stdout_text(&out).is_empty(), "用法错误不许打读数：{}", stdout_text(&out));
    assert_eq!(
        stderr_text(&out),
        "error: distill --pass time takes no --no-review (time has no review step)\n",
        "必须是那一句静态话（零插值），不是 clap 的原串"
    );
    assert!(!stderr_text(&out).contains(DATA), "stderr 带路径");
}

// ── 2. quarantined 复审 ──────────────────────────────────────────────────────
/// 审核回放坏 JSON ⇒ item quarantined；`review run` 用 pass 回放 ⇒ applied、fact 1 行。
#[test]
fn a_quarantined_item_is_re_reviewed_by_review_run() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    // 两档都回坏 JSON：两侧 `Failed(Json)` ⇒ 整 item 隔离（reason 取 proposer 的 `review_json`）。
    let bad = reviewed(&extraction("promise"), &json!({}));
    let out = distill(dir.path(), "promise", &bad);
    assert_ne!(code(&out), 0, "审核坏 JSON 不是成功退出");
    let root = open(dir.path());
    let (id, state) = only_item(&root);
    assert_eq!(state, "quarantined");
    assert_eq!(count(&root, "fact"), 0);
    assert_eq!(fact_turn(&root, &uids[0], "promise").0, "quarantined");
    drop(root);

    let replies = review_pair("promise");
    let out = review_run(dir.path(), None, Some(&replies));
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["applied"], 1);
    assert_eq!(doc["quarantined"], 0);
    assert_eq!(doc["remaining_quarantined"], 0);
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(item_state(&root, id), "applied");
    assert_eq!(count(&root, "fact"), 1);
    let stages = decision_stages(&root, id);
    assert_eq!(
        stages,
        vec![
            "program".to_string(),
            "review".to_string(),
            "review".to_string(),
            "apply".to_string()
        ],
        "复审判定是追加的一条 review"
    );
}

// ── 3. 人工放行无 LLM ───────────────────────────────────────────────────────

/// pending 时 `review decide <id> --approve --reason operator` ⇒ approved；
/// `review run` **不带** LLM 来源 ⇒ applied、写出（推进性装配的证明点）。
#[test]
fn a_human_approved_item_is_written_without_an_llm_source() {
    let dir = tempfile::tempdir().unwrap();
    let _uids = seed(dir.path());
    let id = pending(dir.path(), "promise");

    let dd = data_dir(dir.path());
    let out = run(&[
        "review",
        "decide",
        &id.to_string(),
        "--approve",
        "--reason",
        "operator",
        "--data-dir",
        &dd,
    ]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    assert!(stdout_text(&out).contains("state=approved"), "decide 必须回报 approved");
    let root = open(dir.path());
    assert_eq!(item_state(&root, id), "approved");
    drop(root);

    // 不给 --replies/--llm-config：可运行项全是 approved ⇒ 一次都不装配来源。
    let out = review_run(dir.path(), None, None);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["processed"], 1);
    assert_eq!(doc["applied"], 1);
    assert_eq!(doc["errors"], 0);
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(item_state(&root, id), "applied");
    assert_eq!(count(&root, "fact"), 1, "人工放行后 review run 补写");
}

// ── 4. 退出 4 ───────────────────────────────────────────────────────────────

/// 两条 pending，`review run --limit 1 --replies <pass>` ⇒ 退出 4、`remaining_pending_review=1`。
#[test]
fn a_limit_that_leaves_work_behind_exits_four() {
    let dir = tempfile::tempdir().unwrap();
    let _uids = seed(dir.path());
    pending(dir.path(), "promise");
    pending(dir.path(), "plan");

    let replies = review_pair("promise");
    let out = review_run(dir.path(), Some(1), Some(&replies));
    assert_eq!(code(&out), 4, "stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["processed"], 1);
    assert_eq!(doc["applied"], 1);
    assert_eq!(doc["errors"], 0);
    assert_eq!(doc["remaining_pending_review"], 1, "剩下那条必须被说出来");
    assert_eq!(doc["remaining_quarantined"], 0);
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(count(&root, "fact"), 1, "--limit 只跑了一条");
    let pending_left: i64 = root
        .memory()
        .conn()
        .query_row(
            "SELECT count(*) FROM review_item WHERE state='pending_review'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending_left, 1);
}

// ── 4b. H2：approved 缺口可见化 + remaining 的 --pass 域 ─────────────────────

/// P0-2 的复现路径：pending → `review decide --approve` → 改载荷删一个重放必需键 →
/// `review run`。以前这里是 `Ok(Approved)`：hub 不计数、退出 0，而 ledger 已逐轮
/// `quarantined(PAYLOAD_GAP)`。现在程序侧记一条 `program/uncertain` 把 item 推到
/// `quarantined` ⇒ 退出 4、`remaining_quarantined=1`、0 行。
#[test]
fn an_approved_item_with_a_payload_gap_is_quarantined_and_exits_four() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let id = pending(dir.path(), "promise");
    let dd = data_dir(dir.path());

    let out = run(&[
        "review",
        "decide",
        &id.to_string(),
        "--approve",
        "--reason",
        "operator",
        "--data-dir",
        &dd,
    ]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    {
        let root = open(dir.path());
        assert_eq!(item_state(&root, id), "approved");
        // 删 `items[0].turn_uid` 并**重新落章**：H4a 起"只改 payload 不改列 sha"是篡改，
        // 会先被 `payload_tampered` 拦下。这条用例测的是 approved 的**重放 gap**（H2），
        // 所以按"合法重新入队"的路径同步重算指纹。
        let (payload_json, owner_json, pass_id, family): (String, String, String, String) = root
            .memory()
            .conn()
            .query_row(
                "SELECT payload_json, owner_json, pass_id, family FROM review_item WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        let mut payload: Value = serde_json::from_str(&payload_json).unwrap();
        payload["items"][0].as_object_mut().unwrap().remove("turn_uid");
        let payload_json = payload.to_string();
        let sha = rc_memory::review::payload_sha256(&pass_id, &family, &owner_json, &payload_json);
        root.memory()
            .conn()
            .execute(
                "UPDATE review_item SET payload_json=?2, payload_sha256=?3 WHERE id=?1",
                params![id, payload_json, sha],
            )
            .unwrap();
    }

    // approved 不需要 LLM 来源：不给 --replies 也能走到 gap 那一格。
    let out = review_run(dir.path(), None, None);
    assert_eq!(code(&out), 4, "approved 判了缺口必须退出 4；stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["processed"], 1);
    assert_eq!(doc["applied"], 0);
    assert_eq!(doc["quarantined"], 1, "缺口只走隔离这一格：{doc}");
    assert_eq!(doc["errors"], 0);
    assert_eq!(doc["remaining_pending_review"], 0);
    assert_eq!(doc["remaining_quarantined"], 1, "隔离必须被 remaining 看见：{doc}");
    assert_eq!(doc["remaining_approved"], 0);
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(count(&root, "fact"), 0, "缺口不许写记忆行");
    assert_eq!(item_state(&root, id), "quarantined");
    assert_eq!(
        decision_stages(&root, id),
        vec!["program".to_string(), "human".to_string(), "program".to_string()],
        "程序侧追加一条 program 判定"
    );
    let (stage, verdict, reason, detail, decider, model, prompt_version): (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
    ) = root
        .memory()
        .conn()
        .query_row(
            "SELECT stage, verdict, reason_code, detail, decider, model, prompt_version
               FROM review_decision WHERE item_id=?1 ORDER BY id DESC LIMIT 1",
            [id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .unwrap();
    assert_eq!((stage.as_str(), verdict.as_str()), ("program", "uncertain"));
    assert_eq!(reason, "review_payload_gap");
    assert_eq!(detail, "approved_gap");
    assert_eq!(
        (decider.as_str(), model.as_str(), prompt_version.as_str()),
        ("program", "", ""),
        "程序侧裁定：零模型、零 prompt 版本"
    );
    // ledger 逐轮隔离（H2 之前这里已经写了，队列却没跟上）。
    for uid in &uids {
        assert_eq!(
            fact_turn(&root, uid, "promise"),
            ("quarantined".to_string(), Some("review_payload_gap".to_string())),
            "ledger 逐轮 quarantined(PAYLOAD_GAP)：{uid}"
        );
    }
}

/// remaining 的 `--pass` 域：fact 清空后，另一 pass（time）的 pending 不许把本 pass
/// 抬到退出 4。全库仍有 1 条 pending，但这条命令只对 `--pass fact` 负责。
#[test]
fn pass_scoped_remaining_ignores_other_passes() {
    use rc_memory::review::{enqueue, NewItem};

    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let _fact = pending(dir.path(), "promise");
    {
        let root = open(dir.path());
        let queued = enqueue(
            root.memory(),
            &NewItem {
                source: "distill".into(),
                pass_id: "time".into(),
                family: String::new(),
                session_key: SESS.into(),
                device_id: DEV.into(),
                turn_uids_json: json!([uids[1].clone()]).to_string(),
                owner_json: "{\"owner_agent_id\":\"alice\"}".into(),
                payload_json: "{\"schema\":\"review.pass.v1\"}".into(),
                batch_key: String::new(),
            },
            NOW,
        )
        .unwrap();
        assert!(queued.fresh);
    }

    let dd = data_dir(dir.path());
    let p = dir.path().join("pass-scoped-replies.json");
    std::fs::write(&p, Value::Array(review_pair("promise")).to_string()).unwrap();
    let out = run(&[
        "review",
        "run",
        "--pass",
        "fact",
        "--replies",
        p.to_str().unwrap(),
        "--data-dir",
        &dd,
        "--json",
    ]);
    assert_eq!(
        code(&out),
        0,
        "本 pass 已清空 ⇒ 0（time 的遗留不归这条命令）；stderr: {}",
        stderr_text(&out)
    );
    let doc = stdout_json(&out);
    assert_eq!(doc["applied"], 1);
    assert_eq!(doc["remaining_pending_review"], 0, "fact 的剩余只看 fact 域：{doc}");
    assert_eq!(doc["remaining_quarantined"], 0);
    assert_eq!(doc["remaining_approved"], 0);
    assert_no_body_or_path(&out);

    // 全库仍有 1 条 pending：退出 0 只可能来自 `--pass` 过滤，不是"真没有活"。
    let stats = run(&["review", "stats", "--json", "--data-dir", &dd]);
    assert_eq!(code(&stats), 0, "stderr: {}", stderr_text(&stats));
    assert_eq!(stdout_json(&stats)["pending_review"], 1, "全库遗留仍在");

    let root = open(dir.path());
    assert_eq!(count(&root, "fact"), 1, "fact 那条已补写");
    let (time_state, fact_state): (String, String) = root
        .memory()
        .conn()
        .query_row(
            "SELECT (SELECT state FROM review_item WHERE pass_id='time'),
                    (SELECT state FROM review_item WHERE pass_id='fact')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(time_state, "pending_review", "别的 pass 没被这条命令碰");
    assert_eq!(fact_state, "applied");
}

// ── 5. quota 停止 ───────────────────────────────────────────────────────────

/// 两条 pending，回放 `QuotaExhausted` ⇒ 退出 3、`exhausted=true`、`processed` 只 1 条。
#[test]
fn a_quota_exhausted_reply_stops_the_run_after_one_item() {
    let dir = tempfile::tempdir().unwrap();
    let _uids = seed(dir.path());
    pending(dir.path(), "promise");
    pending(dir.path(), "plan");

    let out = review_run(dir.path(), None, Some(&[fail("quota_exhausted")]));
    assert_eq!(code(&out), 3, "stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["processed"], 1, "额度耗尽必须立刻停，不许再碰第二条");
    assert_eq!(doc["applied"], 0);
    assert_eq!(doc["errors"], 1);
    assert_eq!(doc["exhausted"], true);
    assert_eq!(doc["remaining_pending_review"], 2, "两条都还 pending");
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(count(&root, "fact"), 0);
    // 退避已记：attempt+1 且 next_retry_at_ms 非空（与 runner 同一组数：60s × 4^n）。
    let (attempt, next): (i64, Option<i64>) = root
        .memory()
        .conn()
        .query_row(
            "SELECT attempt, next_retry_at_ms FROM review_item ORDER BY id LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(attempt, 1);
    // `review run` 的 now 是墙钟（`unix_now_ms`），不是夹具的 `NOW`：只断"退避被写下来"。
    assert!(
        next.is_some_and(|ms| ms > NOW),
        "第一次失败必须写下 60s 退避，实际：{next:?}"
    );
}

// ── 6. H1a：两个 review run 并发同一 item ─────────────────────────────────────

/// 两条 `review run` 同时起，争同一个 item 的 apply：claim 只允许一个写者进入。
///
/// item 先经 `review decide --approve` 到 `approved`（人工放行 → apply 补写这条既有路径）：
/// 两个进程争的是**同一格 apply**，这正是 P0-1 的双写面。H2 起 `approved` 也计入
/// remaining，但胜者把它写进 `applied` 之后三格全 0，所以被跳过的那一个仍是退出 0。
///
/// 断言：总 apply 判定 == 1、记忆行 == 1、ledger 最终 done、没有任何 rejected(store)、
/// 两个进程都退出 0。
#[test]
fn two_concurrent_review_runs_claim_once_and_keep_the_ledger_done() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let id = pending(dir.path(), "promise");
    let dd = data_dir(dir.path());
    let decide = run(&[
        "review",
        "decide",
        &id.to_string(),
        "--approve",
        "--reason",
        "operator",
        "--data-dir",
        &dd,
    ]);
    assert_eq!(code(&decide), 0, "stderr: {}", stderr_text(&decide));
    assert_eq!(item_state(&open(dir.path()), id), "approved");

    let replies = Value::Array(review_pair("promise")).to_string();
    let r1 = dir.path().join("race-replies-1.json");
    let r2 = dir.path().join("race-replies-2.json");
    std::fs::write(&r1, &replies).unwrap();
    std::fs::write(&r2, &replies).unwrap();

    // 用 memory.db 的写锁做**确定性栅栏**：两个子进程都已在 claim 的 UPDATE 上等锁时
    // 才放行。于是两边的 `runnable` 都已把这条 item 看进来、又都还没占位——claim 的
    // 竞态被真正摆到同一格上，而不是"先跑完的那个让后到的看不见 item"。
    let root = open(dir.path());
    let hold = root.memory().conn();
    hold.execute_batch("BEGIN IMMEDIATE").unwrap();
    // 两个真进程同时起：先 spawn 两个，再一起等。
    let a = spawn_review_run(dir.path(), &r1);
    let b = spawn_review_run(dir.path(), &r2);
    // 进程启动 + 开库 + runnable 的余量（本机 ~50ms，给 2s；持有锁期间它们只能等）。
    std::thread::sleep(std::time::Duration::from_millis(2_000));
    hold.execute_batch("COMMIT").unwrap();
    let out_a = a.wait_with_output().expect("hub binary a");
    let out_b = b.wait_with_output().expect("hub binary b");
    drop(root);
    assert_eq!(code(&out_a), 0, "a stderr: {}", stderr_text(&out_a));
    assert_eq!(code(&out_b), 0, "b stderr: {}", stderr_text(&out_b));

    let doc_a = stdout_json(&out_a);
    let doc_b = stdout_json(&out_b);
    let processed = doc_a["processed"].as_u64().unwrap() + doc_b["processed"].as_u64().unwrap();
    let applied = doc_a["applied"].as_u64().unwrap() + doc_b["applied"].as_u64().unwrap();
    assert_eq!(processed, 1, "同一条 item 只许一个进程处理：{doc_a} / {doc_b}");
    assert_eq!(applied, 1, "同一条 item 只许 apply 一次：{doc_a} / {doc_b}");
    assert_eq!(
        doc_a["skipped"].as_u64().unwrap() + doc_b["skipped"].as_u64().unwrap(),
        1,
        "另一个进程必须是 claim 失败而跳过：{doc_a} / {doc_b}"
    );

    let root = open(dir.path());
    assert_eq!(
        memory_scalar(&root, "SELECT count(*) FROM review_decision WHERE stage='apply'"),
        1,
        "apply 判定只许一条"
    );
    assert_eq!(count(&root, "fact"), 1, "记忆行只许一行");
    assert_eq!(item_state(&root, id), "applied");
    assert_eq!(
        fact_turn(&root, &uids[0], "promise"),
        ("done".to_string(), None),
        "ledger 最终必须是 done"
    );
    assert_eq!(
        ledger_scalar(
            &root,
            "SELECT count(*) FROM extract_pass_state WHERE state='rejected' AND reason='store'"
        ),
        0,
        "不许出现 rejected(store)"
    );
}

/// H1a：Store 失败不改 ledger。
///
/// 先跑一条成功（promise 轮 ledger=done），再造一条 pending（plan），然后**持住 memory.db
/// 的写锁**跑第二次 `review run`：子进程的 claim 忙等 5s 后撞 `SQLITE_BUSY`（或在开库时
/// 就撞锁），退出 3。断言 item 仍 pending、ledger 快照逐行不变、没有 rejected(store)、
/// 记忆行不增。
#[test]
fn a_store_failure_leaves_the_item_and_ledger_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    // 两条 pending：第一条先补写成功（promise 轮 ledger=done），第二条留给失败的那次跑。
    // `--limit 1` 只处理 id 更小的 promise 那条，plan 留在队列里。
    let _first = pending(dir.path(), "promise");
    let _ = pending(dir.path(), "plan");
    // `pending()` 的 `only_item` 只适合单条夹具；两条并存时按 pass 取 plan 那条的 id。
    let second: i64 = open(dir.path())
        .memory()
        .conn()
        .query_row(
            "SELECT id FROM review_item WHERE pass_id='fact' AND family='plan'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let replies = review_pair("promise");
    let out = review_run(dir.path(), Some(1), Some(&replies));
    assert_eq!(code(&out), 4, "另一条仍在队列 ⇒ 4；stderr: {}", stderr_text(&out));
    let root = open(dir.path());
    assert_eq!(fact_turn(&root, &uids[0], "promise"), ("done".to_string(), None));

    let ledger_before = ledger_snapshot(&root);
    let decisions_before = memory_scalar(&root, "SELECT count(*) FROM review_decision");
    let facts_before = count(&root, "fact");

    // 持住 memory.db 的写锁：子进程的 claim 会等满 busy_timeout 再以 SQLITE_BUSY 失败。
    let hold = root.memory().conn();
    hold.execute_batch("BEGIN IMMEDIATE").unwrap();
    {
        let probe = rusqlite::Connection::open(dir.path().join(DATA).join("memory.db")).unwrap();
        probe
            .busy_timeout(std::time::Duration::from_millis(200))
            .unwrap();
        let attempted = probe.execute("UPDATE review_item SET updated_at_ms=updated_at_ms", []);
        assert!(attempted.is_err(), "写锁没有生效：另一个连接写成功了");
    }
    let plan_replies = review_pair("plan");
    let out = review_run(dir.path(), None, Some(&plan_replies));
    hold.execute_batch("ROLLBACK").unwrap();

    assert_eq!(code(&out), 3, "存储失败必须退出 3；stderr: {}", stderr_text(&out));
    assert_eq!(item_state(&root, second), "pending_review", "失败不许改 item 状态");
    assert_eq!(ledger_snapshot(&root), ledger_before, "Store 失败不许改 ledger 任何一格");
    assert_eq!(
        memory_scalar(&root, "SELECT count(*) FROM review_decision"),
        decisions_before,
        "失败那条 item 不许落判定"
    );
    assert_eq!(count(&root, "fact"), facts_before, "记忆行不增");
    assert_eq!(
        fact_turn(&root, &uids[0], "promise"),
        ("done".to_string(), None),
        "已写好的 done 必须保持 done"
    );
    assert_eq!(
        ledger_scalar(
            &root,
            "SELECT count(*) FROM extract_pass_state WHERE state='rejected' AND reason='store'"
        ),
        0,
        "没有 rejected(store)"
    );
    assert_no_body_or_path(&out);
}

// ── 8. S4：review apply 之后 CAS 提升 card.verdict ────────────────────────────

/// `card` 表里某个句柄的判决（白盒读数）。
fn card_verdict(root: &Root, id: &str) -> String {
    root.memory()
        .conn()
        .query_row("SELECT verdict FROM card WHERE id=?1", [id], |r| r.get(0))
        .unwrap()
}

/// `card` 表里某个句柄的分层标记（`stated`/`inferred`）。
fn card_mode(root: &Root, id: &str) -> String {
    root.memory()
        .conn()
        .query_row("SELECT mode FROM card WHERE id=?1", [id], |r| r.get(0))
        .unwrap()
}

/// 唯一一张**由 fact 写出**的镜像卡 `(id, verdict)`：它的指针是 `["fact:<id>"]`。
/// 命中数不是 1 就红——多一张或少一张都说明夹具/写库路径变了，读数不可信。
fn fact_mirror_card(root: &Root) -> (String, String) {
    let mut st = root
        .memory()
        .conn()
        .prepare("SELECT id, verdict FROM card WHERE pointers_json LIKE '[\"fact:%'")
        .unwrap();
    let got: Vec<(String, String)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(got.len(), 1, "由 fact 写出的镜像卡必须恰好一张：{got:?}");
    got.into_iter().next().unwrap()
}

/// 预埋一张卡（`put_card` 的默认判决是 `unreviewed`），返回句柄。`mode='inferred'` 由
/// `derive` 两族显式写；这里白盒改一格，等价于"推导池写过这张卡"。
fn seed_card(root: &Root, content: &rc_memory::FactContent, turn: &str, inferred: bool) -> String {
    let k = rc_memory::put_card(
        root.memory(),
        content,
        &[Provenance { turn_uid: turn.to_string(), anchor_role: AnchorRole::Human }],
        NOW,
    )
    .unwrap();
    if inferred {
        root.memory()
            .conn()
            .execute("UPDATE card SET mode='inferred' WHERE id=?1", [&k.id])
            .unwrap();
    }
    k.id
}

fn scope_proj_x() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-x".into() }
}

/// 被 applied 的 item 在写库层插下的**那张**镜像卡必须被提升为 `passed`；同 scope 预埋的
/// 普通未审卡与推导卡（`mode='inferred'`）一个都不许动——S4 的提升口径是"这次写下的卡"。
#[test]
fn review_applied_把这次写下的卡提升为_passed() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let id = pending(dir.path(), "promise");

    let plain;
    let derived;
    {
        let root = open(dir.path());
        plain = seed_card(
            &root,
            &rc_memory::FactContent {
                kind: "decision".into(),
                title: "旁观卡".into(),
                body: "同 scope 的另一张未审卡，review 不该替它背书。".into(),
                scope: scope_proj_x(),
                valid_to_ms: None,
            },
            &"b".repeat(64),
            false,
        );
        derived = seed_card(
            &root,
            &rc_memory::FactContent {
                kind: "timeline".into(),
                title: "推导卡".into(),
                body: "推导池写出的卡，不是 review apply 的产物。".into(),
                scope: scope_proj_x(),
                valid_to_ms: None,
            },
            &"c".repeat(64),
            true,
        );
        assert_eq!(count(&root, "card"), 2, "两张预埋卡");
    }

    let out = review_run(dir.path(), None, Some(&review_pair("promise")));
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!((doc["applied"].as_u64(), doc["rejected"].as_u64()), (Some(1), Some(0)));
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(item_state(&root, id), "applied");
    let (mirror_id, mirror_verdict) = fact_mirror_card(&root);
    assert_eq!(mirror_verdict, "passed", "apply 插下的卡必须被提升（S4）：{mirror_id}");
    assert_eq!(card_verdict(&root, &plain), "unreviewed", "别人的卡不许被顺手提升");
    assert_eq!(card_verdict(&root, &derived), "unreviewed", "推导卡不是这次 apply 的产物");
    assert_eq!(card_mode(&root, &derived), "inferred", "夹具前提：推导卡确实写着 inferred");
    assert_eq!(count(&root, "card"), 3, "镜像卡 + 两张预埋卡，不多不少");
}

/// rejected：草稿不写库 ⇒ 一张卡都不落、一张卡都不提升；预埋的未审卡原地不动。
#[test]
fn review_rejected_不提升任何卡() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let id = pending(dir.path(), "promise");

    let bystander;
    {
        let root = open(dir.path());
        bystander = seed_card(
            &root,
            &rc_memory::FactContent {
                kind: "decision".into(),
                title: "旁观卡".into(),
                body: "驳回这条 item 之后它必须还是 unreviewed。".into(),
                scope: scope_proj_x(),
                valid_to_ms: None,
            },
            &"b".repeat(64),
            false,
        );
    }

    // 两档都判 reject ⇒ 聚合 reject ⇒ item rejected（证据是 quote 的子串，过 §7.2 的证据闸）。
    let reject = json!({
        "schema_version": 0, "pass": "fact_review", "family": "promise",
        "verdicts": [{"ref": "f1", "verdict": "reject", "code": "not_durable",
                      "evidence": "下周三之前"}]
    });
    let out = review_run(dir.path(), None, Some(&[reply(&reject), reply(&reject)]));
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!((doc["rejected"].as_u64(), doc["applied"].as_u64()), (Some(1), Some(0)));

    let root = open(dir.path());
    assert_eq!(item_state(&root, id), "rejected");
    assert_eq!(count(&root, "fact"), 0, "驳回不写事实行");
    assert_eq!(count(&root, "card"), 1, "驳回不落卡：库里只有预埋那一张");
    assert_eq!(card_verdict(&root, &bystander), "unreviewed", "被拒的 item 不许替任何卡背书");
}

/// uncertain（M2）：审核判不了 ⇒ 不写库 ⇒ 一张卡都不落、一张卡都不提升；预埋的未审卡原地
/// 不动。与 rejected 同一条结构（`write_rows` 没被调用），只差判决那格是 `quarantined`——
/// S4 的提升边界只认 applied，两档都进不来，所以这里钉的是"边界没放宽"。
#[test]
fn review_uncertain_不提升任何卡() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let id = pending(dir.path(), "promise");

    let bystander;
    {
        let root = open(dir.path());
        bystander = seed_card(
            &root,
            &rc_memory::FactContent {
                kind: "decision".into(),
                title: "旁观卡".into(),
                body: "判不了这条 item 之后它必须还是 unreviewed。".into(),
                scope: scope_proj_x(),
                valid_to_ms: None,
            },
            &"b".repeat(64),
            false,
        );
    }

    // 两档都判 uncertain（`uncertain` 允许不带码/不带证据）⇒ 聚合 uncertain ⇒ item quarantined。
    let uncertain = json!({
        "schema_version": 0, "pass": "fact_review", "family": "promise",
        "verdicts": [{"ref": "f1", "verdict": "uncertain", "code": null, "evidence": null}]
    });
    let out = review_run(dir.path(), None, Some(&[reply(&uncertain), reply(&uncertain)]));
    // 隔离项仍在队列里 ⇒ 退出 4（"还没走完"），不是错误退出。
    assert_eq!(code(&out), 4, "stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(
        (doc["quarantined"].as_u64(), doc["applied"].as_u64()),
        (Some(1), Some(0)),
        "判不了只隔离，不算 applied：{doc}"
    );
    assert_eq!(doc["remaining_quarantined"], 1, "隔离必须被 remaining 看见：{doc}");

    let root = open(dir.path());
    assert_eq!(item_state(&root, id), "quarantined");
    assert_eq!(count(&root, "fact"), 0, "不写库 ⇒ 不落事实行");
    assert_eq!(count(&root, "card"), 1, "不确定不落卡：库里只有预埋那一张");
    assert_eq!(card_verdict(&root, &bystander), "unreviewed", "没有 applied 就不许替任何卡背书");
}

/// 去重路径的裁定（**提升只在本次真的插进去时发生**）：预埋一张与 apply 会写出的镜像卡
/// **同址**的卡（同 category/title/text/scope ⇒ 同一个内容寻址 id），`INSERT OR IGNORE`
/// 返回 0 ⇒ 它不进 `card_ids` ⇒ 不提升。一句理由：这张卡不是这次 apply 写的，它可能早已
/// 被 audit 判过（`low`/`failed` 也走这条 CAS 的 `unreviewed` 前提），另一条 fact 的通过
/// 不该替它背书；它仍留在审核游标里，audit 会照常处置。
#[test]
fn review_applied_命中既有同址卡时不提升() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let id = pending(dir.path(), "promise");

    // 镜像卡的形状与 `writers_fact.rs` 的 CARD_INSERT 前一模一样：kind=category、
    // title=text 前 30 字、body=text、scope 取会话归属（project/proj-x）。
    let text = "张三承诺在下周三之前把对账单发给李四。";
    let existing;
    {
        let root = open(dir.path());
        existing = seed_card(
            &root,
            &rc_memory::FactContent {
                kind: "promise".into(),
                title: text.chars().take(30).collect(),
                body: text.into(),
                scope: scope_proj_x(),
                valid_to_ms: None,
            },
            &"b".repeat(64),
            false,
        );
    }

    let out = review_run(dir.path(), None, Some(&review_pair("promise")));
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    assert_eq!(stdout_json(&out)["applied"], 1);

    let root = open(dir.path());
    assert_eq!(item_state(&root, id), "applied");
    assert_eq!(count(&root, "fact"), 1, "事实行照写");
    assert_eq!(count(&root, "card"), 1, "卡被内容寻址去重，没有新卡");
    assert_eq!(
        card_verdict(&root, &existing),
        "unreviewed",
        "去重命中的既有卡不是这次写的 ⇒ 不提升（方案 A 的 payload 重算会在这里提升，这条钉住两案的分界）"
    );
}

/// 波三修 2：`reuse` 项（与 `report` 同族，引擎不认）不被 `review run` 选中——
/// 不占位、不记 `errors`、状态不动、退出码不被抬高；跳过的条数进 `skipped_unsupported`。
///
/// 不给任何 LLM 来源：修前 `needs_llm` 会把这条项当"要跑"（随后 NoLlm 记 `Step::Error`）；
/// 修后零可选 ⇒ 直接跑空、0 处理、0 错误。
#[test]
fn a_reuse_item_is_skipped_and_reported_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let root = open(dir.path());
    let item = rc_memory::review::NewItem {
        source: "reuse".into(),
        pass_id: "reuse".into(),
        family: String::new(),
        session_key: String::new(),
        device_id: DEV.into(),
        turn_uids_json: "[\"t1\"]".into(),
        owner_json: json!({"reporter": "qoder"}).to_string(),
        payload_json: json!({"items": []}).to_string(),
        batch_key: String::new(),
    };
    let id = rc_memory::review::enqueue(root.memory(), &item, NOW)
        .expect("reuse 项入队")
        .id;
    drop(root);

    let out = review_run(dir.path(), None, None);
    assert_eq!(code(&out), 0, "人工来源不进引擎：{}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["processed"], 0, "{doc}");
    assert_eq!(doc["errors"], 0, "{doc}");
    assert!(
        doc["skipped_unsupported"].as_u64().unwrap_or(0) >= 1,
        "跳过的条数必须在读数里：{doc}"
    );
    let root = open(dir.path());
    assert_eq!(item_state(&root, id), "pending_review", "人工项留在队列等 decide");
}
