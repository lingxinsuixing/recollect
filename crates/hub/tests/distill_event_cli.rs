// crates/hub/tests/distill_event_cli.rs
//! S2c-2：`hub distill once --pass event` 的黑盒进程面。照 `distill_fact_cli.rs` 的写法：
//! CLI 是这条 pass 的唯一出口，装配漏了一格（来源、选批、审核、计数器、退出码）只有子进程看得见。
//!
//! 纪律与 naming/fact 同源：
//! - **不跑真实 LLM**：`--replies` 是唯一来源，脚本里只有抽取 + 审核两条合法回复或 `{"fail":...}`；
//! - stdout 只许一个 JSON（守卫 12）：用例同时断正文与 data-dir 路径不出现；
//! - event **不接受** `--family`（它是单一 pass，没有调用变体）；给了就是用法错误（3，且不开库）。
use rc_contract::{normalize, parse};
use rc_ledger::insert_batch;
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{AnchorRole, Provenance, Scope, ScopeKind};
use rc_store::Root;
use rusqlite::params;
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Output};

const DEV: &str = "dev-event";
const SESS: &str = "sess-event";
const DATA: &str = "data";
const NOW: i64 = 1_700_000_000_000;
/// 目标轮正文：断它**不出现在** stdout/stderr（守卫 12 的零正文）。
/// C2 起 `time_text` 必须接地，正文带上「昨晚」（与 `event_reply` 的 `time_text` 同字）。
const CANARY: &str = "昨晚我把配置改回去，服务就恢复了";

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
    serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象")
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

/// 一条 event 状态行：`(state, reason, next_retry_at_ms)`。`family` 恒空串。
fn pass_state(root: &Root, uid: &str) -> (String, Option<String>, Option<i64>) {
    root.ledger()
        .conn()
        .query_row(
            "SELECT state, reason, next_retry_at_ms FROM extract_pass_state
             WHERE turn_uid=?1 AND pass_id='event' AND family=''",
            [uid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

/// `llm_job:<pass>` 的 `llm_calls` 总账。
fn llm_calls(root: &Root, pass: &str) -> i64 {
    root.ledger()
        .conn()
        .query_row(
            "SELECT coalesce(sum(count),0) FROM usage_counter
             WHERE subject_kind='llm_job' AND subject_id=?1 AND metric='llm_calls'",
            [pass],
            |r| r.get(0),
        )
        .unwrap()
}

/// S3a：某一主语（`llm_job:<pass>`）的"最近一次运行是否触顶"格。`None` = 这一 pass 从没写过。
fn quota_stopped(root: &Root, subject: &str) -> Option<i64> {
    root.ledger()
        .conn()
        .query_row(
            "SELECT count FROM usage_counter
             WHERE subject_kind='llm_job' AND subject_id=?1 AND metric='quota_stopped'",
            [subject],
            |r| r.get(0),
        )
        .ok()
}

/// 播种两轮真转录（`parse` → `normalize` → `insert_batch`，与 `rc-distill` 的夹具同一条链），
/// 归属改成非默认值，再铺两个活锚：张三较新（`a1`）、某机房较旧（`a2`，place）。
fn seed(dir: &Path) -> Vec<String> {
    let doc = json!({
        "format_version": 1,
        "sessions": [{
            "session_key": SESS,
            "device_id": DEV,
            "source_agent": "agent-event",
            "turns": [
                {"role": "human", "body": CANARY, "created_at_ms": NOW},
                {"role": "agent", "body": "好的，收到", "created_at_ms": NOW + 1},
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
    assert_eq!(uids.len(), 2, "夹具的条数必须真是 2");
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
    for (kind, canonical, created_at_ms) in
        [("place", "某机房", NOW - 2_000), ("person", "张三", NOW - 1_000)]
    {
        put_entity(
            root.memory(),
            &EntityContent {
                kind: kind.into(),
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

/// t1 的一条 state_change：actor a1=张三、object_text 配置文件、place a2=某机房、result done。
fn event_reply() -> Value {
    json!({
        "schema_version": 0, "pass": "event",
        "events": [{
            "ref": "e1", "turn": "t1", "category": "state_change", "actor": "a1",
            "trigger": "改回去", "action": "restore_config", "object": null, "object_text": "配置文件",
            "participants": ["a1"], "place": "a2", "result": "done", "mode": "stated",
            "time_text": "昨晚", "quote": CANARY, "evidence": ["t1"],
            "text": "张三昨晚把配置改回去，服务恢复。"
        }],
        "empty_turns": ["t2"]
    })
}

fn review_pass() -> Value {
    json!({
        "schema_version": 0, "pass": "event_review",
        "verdicts": [{"ref": "e1", "verdict": "pass", "code": null}]
    })
}

fn review_reject() -> Value {
    json!({
        "schema_version": 0, "pass": "event_review",
        "verdicts": [{"ref": "e1", "verdict": "reject", "code": "not_atomic"}]
    })
}

fn reply(text: &Value) -> Value {
    Value::String(text.to_string())
}

/// `distill once --pass event [--family <family>] --replies <file>`。
fn distill(dir: &Path, family: Option<&str>, replies: &[Value]) -> Output {
    let mut extra: Vec<&str> = Vec::new();
    if let Some(family) = family {
        extra.push("--family");
        extra.push(family);
    }
    distill_with(dir, &extra, replies)
}

/// 同一个装配，外加任意旗标（`--retry-empty N` 与既有调用走同一条命令行，不另开一条路）。
fn distill_with(dir: &Path, extra: &[&str], replies: &[Value]) -> Output {
    let p = dir.join("event-replies.json");
    std::fs::write(&p, Value::Array(replies.to_vec()).to_string()).unwrap();
    let dd = data_dir(dir);
    let mut args: Vec<&str> = vec!["distill", "once", "--pass", "event"];
    args.extend_from_slice(extra);
    args.push("--replies");
    args.push(p.to_str().unwrap());
    args.push("--data-dir");
    args.push(&dd);
    run(&args)
}

/// 守卫 12：stdout 只有读数，没有正文、没有路径。
fn assert_no_body_or_path(out: &Output) {
    let text = stdout_text(out);
    assert!(!text.contains(CANARY), "stdout 带正文：{text}");
    assert!(!text.contains(DATA), "stdout 带路径：{text}");
    assert!(!stderr_text(out).contains(CANARY), "stderr 带正文");
}

/// 1. 成功一批：exit 0、单 JSON（十一格）、库里有 fact/owner/place、每轮状态、`llm_job:event` 记 2。
#[test]
fn an_event_pass_writes_rows_owner_place_and_done_states() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let out = distill(
        dir.path(),
        None,
        &[reply(&event_reply()), reply(&review_pass())],
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));

    let doc = stdout_json(&out);
    assert_eq!(doc["schema_version"], 2, "W33-A10: 运维面加了观测两格");
    assert_eq!(doc["pass"], "event");
    assert!(doc.get("family").is_none(), "event 的读数没有 family 格");
    assert_eq!(doc["turns"], 2);
    assert_eq!(doc["drafts"], 1);
    assert_eq!(doc["dropped"], 0, "正常批没有草稿被丢");
    assert_eq!(doc["uncovered"], 0, "正常批覆盖完整");
    assert_eq!(doc["written"], 1);
    assert_eq!(doc["duplicate"], 0, "正常批没有被去重的草稿");
    assert_eq!(doc["rejected"], 0);
    assert_eq!(doc["uncertain"], 0);
    assert_eq!(doc["facts"], 1);
    assert_eq!(doc["cards"], 1);
    assert_eq!(doc["mentions"], 1);
    assert_eq!(doc["state"], "done");
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(count(&root, "fact"), 1, "库里必须真有 fact 行");
    let (owner, created_by, place) = root
        .memory()
        .conn()
        .query_row(
            "SELECT o.owner_agent_id, o.created_by_agent, f.place_anchor_id
             FROM resource_owner o JOIN fact f ON o.mem_id=f.id
             WHERE o.mem_table='fact'",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?)),
        )
        .unwrap();
    assert_eq!(owner, "alice", "归属继承会话的 owner_agent_id");
    assert_eq!(created_by, "agent-event");
    let room: String = root
        .memory()
        .conn()
        .query_row("SELECT id FROM anchor WHERE canonical='某机房'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(place.as_deref(), Some(room.as_str()), "place_anchor_id 落列");
    assert_eq!(pass_state(&root, &uids[0]).0, "done");
    assert_eq!(pass_state(&root, &uids[1]).0, "empty");
    assert_eq!(llm_calls(&root, "event"), 2, "抽取 + 审核两次调用");
    assert_eq!(llm_calls(&root, "fact"), 0, "计数器按 pass 分账");
    assert_eq!(llm_calls(&root, "naming"), 0);
    // S3a：跑完而没触顶的一轮收尾落 0（读侧靠它把 null 与 false 分开）。
    assert_eq!(quota_stopped(&root, "event"), Some(0), "没触顶的一轮必须把这一格写成 0");
}

/// S3a 阳性（评审 I-1 的 record 半边）：event 触顶时收尾那一句必须把 `llm_job:event` 写成 1。
/// 变异电池：删掉 `run_distill_event` 收尾的 `record_quota_stopped(...)` ⇒ 停在清 0 的 0 ⇒ 红；
/// 主语打错（`PASS_EVENT` → `PASS_FACT`）⇒ `event` 那一格没有行 ⇒ 红，第二句同时断
/// `fact` 那一格没被写上。
#[test]
fn a_quota_stop_marks_the_event_cell_and_no_other() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let out = distill(dir.path(), None, &[json!({"fail": "quota_exhausted"})]);
    assert_eq!(code(&out), 3, "触顶是 LLM 面故障（退出 3）：{}", stderr_text(&out));
    assert!(
        stderr_text(&out).contains("llm quota_exhausted"),
        "stderr 少了四态那一格：{}",
        stderr_text(&out)
    );

    let root = open(dir.path());
    assert_eq!(quota_stopped(&root, "event"), Some(1),
        "触顶那一轮必须落在 `llm_job:event` 自己的格上（主语写错＝这一格没有行）");
    assert_eq!(quota_stopped(&root, "fact"), None,
        "主语写成别的 pass 会在这里多出一行：这一格必须是空的");
}

/// S3a 阴性（评审 I-1 的 clear 半边）：空批是早退，走不到收尾那一句 ⇒ 这一格只能由开头
/// 那一次清 0 写。种 1 再跑空批：删掉 `clear_quota_stopped` 那一行，这一格留在 1 ⇒ 红。
#[test]
fn an_empty_batch_clears_the_previous_runs_quota_cell() {
    let dir = tempfile::tempdir().unwrap();
    // 空库：没有候选轮 ⇒ `Ok(None)` ⇒ 早退。
    let root = open(dir.path());
    rc_ledger::counters::set_one(
        root.ledger(), "llm_job", "event", rc_ledger::counters::QUOTA_STOPPED_METRIC, 1, NOW,
    )
    .unwrap();
    assert_eq!(quota_stopped(&root, "event"), Some(1), "夹具必须先真的种上 1");
    drop(root);

    let out = distill(dir.path(), None, &[]);
    assert_eq!(code(&out), 0, "空批是合法结果：{}", stderr_text(&out));
    assert_eq!(stdout_json(&out)["state"], "none");

    let root = open(dir.path());
    assert_eq!(quota_stopped(&root, "event"), Some(0),
        "早退的运行也必须把上一次的 1 清掉（否则 status 会一直报上次的触顶）");
}

/// 2. 再跑一次：`turns=0`、`state="none"`、退出 0、行数与计数器都不动。
#[test]
fn a_second_run_is_an_empty_batch_with_no_new_rows() {
    let dir = tempfile::tempdir().unwrap();
    let _uids = seed(dir.path());
    let first = distill(
        dir.path(),
        None,
        &[reply(&event_reply()), reply(&review_pass())],
    );
    assert_eq!(code(&first), 0, "stderr: {}", stderr_text(&first));

    let root = open(dir.path());
    let tables = ["fact", "card", "provenance", "memory_mention", "resource_owner"];
    let before: Vec<i64> = tables.iter().map(|t| count(&root, t)).collect();
    let calls_before = llm_calls(&root, "event");
    drop(root);

    let again = distill(
        dir.path(),
        None,
        &[reply(&event_reply()), reply(&review_pass())],
    );
    assert_eq!(code(&again), 0, "stderr: {}", stderr_text(&again));
    let doc = stdout_json(&again);
    assert_eq!(doc["turns"], 0);
    assert_eq!(doc["written"], 0);
    assert_eq!(doc["dropped"], 0);
    assert_eq!(doc["uncovered"], 0);
    assert_eq!(doc["state"], "none");
    assert!(doc.get("family").is_none());

    let root = open(dir.path());
    let after: Vec<i64> = tables.iter().map(|t| count(&root, t)).collect();
    assert_eq!(before, after, "空批不许长行");
    assert_eq!(llm_calls(&root, "event"), calls_before, "没开口就不加 llm_calls");
}

/// 3. `--pass event --family <f>` 是用法错误：exit 3、stderr 点名 event 与 --family、不开库。
#[test]
fn the_family_flag_is_a_usage_error_for_event() {
    let dir = tempfile::tempdir().unwrap();
    let dd = data_dir(dir.path());

    for family in ["promise", "decision", "meeting"] {
        let out =
            run(&["distill", "once", "--pass", "event", "--family", family, "--data-dir", &dd]);
        assert_eq!(code(&out), 3, "stderr: {}", stderr_text(&out));
        assert!(out.stdout.is_empty(), "配置故障不开库也不打读数");
        let err = stderr_text(&out);
        assert!(err.contains("event"), "stderr: {err}");
        assert!(err.contains("--family"), "stderr: {err}");
        assert!(!err.contains(family) || family == "event", "stderr 不回显未知值：{err}");
    }
    assert!(!dir.path().join(DATA).exists(), "用法错误不建库");
}

/// 4. 审核 reject：exit 0（正常拒收，不是故障）、`written=0`、`state=rejected`、库里无 fact。
#[test]
fn a_review_rejection_exits_zero_with_no_fact_rows() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let out = distill(
        dir.path(),
        None,
        &[reply(&event_reply()), reply(&review_reject())],
    );
    assert_eq!(code(&out), 0, "审核 reject 是正常结果，stderr: {}", stderr_text(&out));

    let doc = stdout_json(&out);
    assert_eq!(doc["turns"], 2);
    assert_eq!(doc["drafts"], 1);
    assert_eq!(doc["written"], 0);
    assert_eq!(doc["rejected"], 1);
    assert_eq!(doc["uncertain"], 0);
    assert_eq!(doc["facts"], 0);
    assert_eq!(doc["state"], "rejected");
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(count(&root, "fact"), 0);
    assert_eq!(
        root.memory()
            .conn()
            .query_row(
                "SELECT count(*) FROM resource_owner WHERE mem_table IN ('fact','card')",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    let (state, reason, _) = pass_state(&root, &uids[0]);
    assert_eq!(state, "rejected");
    assert_eq!(reason.as_deref(), Some("review_reject"));
    assert_eq!(pass_state(&root, &uids[1]).0, "empty");
}

// ── 2026-10-30-retry-empty：空轮重试（聚合层开关，默认关闭）──────────────────────
//
// 三条用例各钉一头（派发档 §验收 2）：
// 1. `--retry-empty 0` 与不传参**逐字节**相同（读数、调用次数、落库结果）；
// 2. 第一次判空、第二次有事件 ⇒ 最终落一条草稿，`retries=1`、`recovered=1`；
// 3. 两次都判空 ⇒ 仍落 `empty`，`retries=1`、`recovered=0`。
//
// 脚本长度同时钉住调用次数：回放座位耗尽即 `Timeout`（退出 3），所以"多发一次调用"这
// 一侧不是靠断言读数，而是靠**多给的那一次会让用例当场变红**。

/// 一次合法空产出：`events=[]` + 两个目标轮都进 `empty_turns`（判空的封闭形状）。
fn event_empty_reply() -> Value {
    json!({
        "schema_version": 0, "pass": "event",
        "events": [], "empty_turns": ["t1", "t2"]
    })
}

/// `fact` 全部行的可比较快照（行按 id、列按定序、值走 Debug）：两张库"落库结果相同"的判据。
/// 不读归属表的时刻列——那是墙上钟（`now_ms`）盖的章，两次进程本来就不等。
fn fact_dump(root: &Root) -> Vec<String> {
    let mut st = root
        .memory()
        .conn()
        .prepare(
            "SELECT id, subject_anchor_id, coalesce(object_anchor_id,''), predicate,
                    coalesce(value_json,''), coalesce(value_text,''), category, mode,
                    coalesce(place_anchor_id,''), participants_json, evidence_turn_uid,
                    coalesce(occurred_at_ms,-1), coalesce(status,''), scope_kind, scope_key,
                    source_agent, device_id, created_at_ms
               FROM fact ORDER BY id",
        )
        .unwrap();
    let rows = st
        .query_map([], |r| {
            (0..18)
                .map(|i| r.get::<_, rusqlite::types::Value>(i).map(|v| format!("{v:?}")))
                .collect::<Result<Vec<_>, _>>()
                .map(|cols| cols.join("|"))
        })
        .unwrap();
    rows.collect::<Result<Vec<_>, _>>().unwrap()
}

/// 5. 硬约束：`--retry-empty 0` 与**不传这一格**逐字节相同。
///
/// 判据三层：① stdout 字节相等（读数不多一格、没有 `retries`/`recovered` 键）；② `llm_calls`
/// 相等（不多发一次调用）；③ `fact` 每一行的每一列相等（落库结果同一份）。
#[test]
fn retry_empty_zero_is_byte_identical_to_leaving_the_flag_out() {
    let replies = [reply(&event_reply()), reply(&review_pass())];

    let without = tempfile::tempdir().unwrap();
    let uids_without = seed(without.path());
    let default_run = distill(without.path(), None, &replies);
    assert_eq!(code(&default_run), 0, "stderr: {}", stderr_text(&default_run));

    let explicit = tempfile::tempdir().unwrap();
    let uids_explicit = seed(explicit.path());
    let zero_run = distill_with(explicit.path(), &["--retry-empty", "0"], &replies);
    assert_eq!(code(&zero_run), 0, "stderr: {}", stderr_text(&zero_run));
    assert_eq!(uids_without, uids_explicit, "两次夹具必须播种出同一批轮");

    // ① 读数逐字节相同，且没有多出来的两格。
    assert_eq!(default_run.stdout, zero_run.stdout, "0 与不传参的读数必须逐字节相同");
    assert_eq!(stderr_text(&default_run), stderr_text(&zero_run), "stderr 也必须相同");
    let doc = stdout_json(&zero_run);
    assert!(doc.get("retries").is_none(), "0 时不许多写 retries 一格：{doc}");
    assert!(doc.get("recovered").is_none(), "0 时不许多写 recovered 一格：{doc}");
    assert_eq!(doc["written"], 1);

    // ② 调用次数同一本账（抽取 + 审核两次；重试一次都不许多）。
    let root_without = open(without.path());
    let root_explicit = open(explicit.path());
    assert_eq!(llm_calls(&root_without, "event"), 2, "0 时就是两次调用");
    assert_eq!(
        llm_calls(&root_without, "event"),
        llm_calls(&root_explicit, "event"),
        "0 与不传参的调用次数必须相同"
    );

    // ③ 落库结果逐行逐列相同（含每轮状态）。
    assert_eq!(fact_dump(&root_without), fact_dump(&root_explicit), "落库行必须逐列相同");
    for uid in &uids_without {
        assert_eq!(
            pass_state(&root_without, uid),
            pass_state(&root_explicit, uid),
            "同一轮的状态行必须相同：{uid}"
        );
    }
    assert_eq!(count(&root_without, "fact"), 1);
    assert_eq!(count(&root_explicit, "fact"), 1);
}

/// 6. 第一次判空、第二次有事件 ⇒ 最终落一条草稿（走同一条闸/落库/审核），`retries=1`、`recovered=1`。
///
/// 脚本只有三条：空产出 → 真产出 → 审核 pass。因此这里同时证明了三件事：
/// ① 重试真的发生过（否则第二条回复会成为审核的输入、审核那条永远轮不到，读数也不对）；
/// ② 重试拿到的草稿确实进了审核队列（第三次调用存在，`review_item` 只有一行）；
/// ③ 重试计入 `llm_calls`（3 = 抽取 + 重试 + 审核），且没有重复写 `running`（该轮终态、`attempt=0`）。
#[test]
fn an_empty_first_sample_is_resampled_and_the_draft_lands_through_the_review_queue() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let out = distill_with(
        dir.path(),
        &["--retry-empty", "1"],
        &[reply(&event_empty_reply()), reply(&event_reply()), reply(&review_pass())],
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));

    let doc = stdout_json(&out);
    assert_eq!(doc["turns"], 2);
    assert_eq!(doc["drafts"], 1, "第二次采样的事件才是这一批的草稿");
    assert_eq!(doc["written"], 1, "重试拿到的草稿走同一条闸/落库路径");
    assert_eq!(doc["facts"], 1);
    assert_eq!(doc["rejected"], 0);
    assert_eq!(doc["uncertain"], 0);
    assert_eq!(doc["state"], "done");
    assert_eq!(doc["retries"], 1, "实际发起了一次重试");
    assert_eq!(doc["recovered"], 1, "重试命中的轮数");
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(count(&root, "fact"), 1);
    assert_eq!(count(&root, "review_item"), 1, "重试的草稿只入队一次（没有第二条旁路）");
    assert_eq!(llm_calls(&root, "event"), 3, "抽取 + 重试 + 审核：重试计入 llm_calls");
    assert_eq!(pass_state(&root, &uids[0]).0, "done", "重试命中那一轮落 done");
    assert_eq!(pass_state(&root, &uids[1]).0, "empty", "另一个轮仍是空");

    // 没有重复写 `running`：终态行仍是 `mark_running` 那一次留下的开工记录（attempt=0，
    // run_id 非空、不是 `running`）——重试只是同一轮里的又一次开口，不是第二轮 pass。
    let attempt: i64 = root
        .ledger()
        .conn()
        .query_row(
            "SELECT attempt FROM extract_pass_state
             WHERE turn_uid=?1 AND pass_id='event' AND family=''",
            [&uids[0]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(attempt, 0, "重试不重写 running，也不重置 attempt");
}

/// 7. 两次都判空 ⇒ 仍落 `empty`（合法空批），`retries=1`、`recovered=0`。
///
/// 脚本只有两条空产出：若实现多发了第三次调用，回放座位耗尽 ⇒ 退出 3，本用例当场红。
#[test]
fn two_empty_samples_still_land_empty_with_one_retry() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let out = distill_with(
        dir.path(),
        &["--retry-empty", "1"],
        &[reply(&event_empty_reply()), reply(&event_empty_reply())],
    );
    assert_eq!(code(&out), 0, "空批仍是合法结果、退出 0，stderr: {}", stderr_text(&out));

    let doc = stdout_json(&out);
    assert_eq!(doc["turns"], 2);
    assert_eq!(doc["drafts"], 0);
    assert_eq!(doc["written"], 0);
    assert_eq!(doc["state"], "empty");
    assert_eq!(doc["retries"], 1);
    assert_eq!(doc["recovered"], 0);

    let root = open(dir.path());
    assert_eq!(count(&root, "fact"), 0, "两次都判空 ⇒ 一行都不落");
    assert_eq!(count(&root, "review_item"), 0, "零草稿不入队");
    assert_eq!(llm_calls(&root, "event"), 2, "空产出不调审核；重试是一次真调用");
    for uid in &uids {
        let (state, reason, _) = pass_state(&root, uid);
        assert_eq!(state, "empty", "{uid}");
        assert!(reason.is_none(), "{uid}");
    }
}
