// crates/hub/tests/distill_fact_cli.rs
//! S2b-2b：`hub distill once --pass fact --family <decision|promise|plan>` 的黑盒进程面。
//! 照 `distill_naming_cli.rs` 的写法：CLI 是这条 pass 的唯一出口，装配漏了一格（来源、选批、
//! 审核、计数器、退出码）只有子进程看得见。
//!
//! 纪律与 naming 同源：
//! - **不跑真实 LLM**：`--replies` 是唯一来源，脚本里只有抽取 + 审核两档（proposer、challenger）
//!   三条合法回复或 `{"fail":...}`（§7.2 起每片两次审核调用）；
//! - stdout 只许一个 JSON（守卫 12）：用例同时断正文与 data-dir 路径不出现；
//! - `--family` 对 fact 必填，其它 pass 不接受它（用法错误 = 3，且不开库）。
use rc_contract::{normalize, parse};
use rc_ledger::insert_batch;
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{AnchorRole, Provenance, Scope, ScopeKind};
use rc_store::Root;
use rusqlite::params;
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Output};

const DEV: &str = "dev-fact";
const SESS: &str = "sess-fact";
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

/// 一条 fact 状态行：`(state, reason, next_retry_at_ms)`。
fn pass_state(root: &Root, uid: &str, family: &str) -> (String, Option<String>, Option<i64>) {
    root.ledger()
        .conn()
        .query_row(
            "SELECT state, reason, next_retry_at_ms FROM extract_pass_state
             WHERE turn_uid=?1 AND pass_id='fact' AND family=?2",
            params![uid, family],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

/// `llm_job:<pass>` 的 `llm_calls` 总账（fact 的计数器主语是 pass_id）。
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

/// 播种三轮真转录（`parse` → `normalize` → `insert_batch`，与 `rc-distill` 的夹具同一条链），
/// 归属改成非默认值，再铺两个活锚：张三较新（`a1`）、李四较早（`a2`）。
fn seed(dir: &Path) -> Vec<String> {
    let doc = json!({
        "format_version": 1,
        "sessions": [{
            "session_key": SESS,
            "device_id": DEV,
            "source_agent": "agent-fact",
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

fn promise_reply() -> Value {
    json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a1", "predicate": "promise",
            "object": "a2", "value": "把对账单发给你", "category": "promise",
            "mode": "stated", "time_text": "下周三之前", "quote": CANARY,
            "evidence": ["t1"], "text": "张三承诺在下周三之前把对账单发给李四。"
        }],
        "empty_turns": ["t2", "t3"]
    })
}

/// 审核证据：§7.2 的证据闸要求每条 verdict 的 `evidence` 是**该条 `quote` 的子串**。
/// 夹具里每条 quote 都是 `CANARY`（「我下周三之前把对账单发给你」），这段是它的子串；
/// 缺证据或越界会被程序侧逐条降 `uncertain`（`evidence_missing` / `evidence_not_in_quote`）。
const EVIDENCE: &str = "把对账单发给你";

fn review_pass() -> Value {
    json!({
        "schema_version": 0, "pass": "fact_review", "family": "promise",
        "verdicts": [{"ref": "f1", "verdict": "pass", "code": null, "evidence": EVIDENCE}]
    })
}

fn review_reject() -> Value {
    json!({
        "schema_version": 0, "pass": "fact_review", "family": "promise",
        "verdicts": [{
            "ref": "f1", "verdict": "reject", "code": "not_durable", "evidence": EVIDENCE
        }]
    })
}

/// 一片的三条应答（§7.2）：抽取 → proposer（判是否成立）→ challenger（只找反证）。
///
/// 本文件的用例断的是"这一片最终判什么"，所以两档回放**同一份判决**：proposer pass +
/// challenger pass ⇒ 裁决表的 pass 格；两侧都 reject ⇒ reject 格（challenger 的 code 胜出，
/// 与 proposer 同码，裁决无歧义）。
fn reviewed(extraction: &Value, verdict: &Value) -> Vec<Value> {
    vec![reply(extraction), reply(verdict), reply(verdict)]
}

/// 另一个 family 对**同一条证据轮**落**同一个 value**：S2e-6 的跨 family 去重用例。
fn plan_reply() -> Value {
    json!({
        "schema_version": 0, "pass": "fact", "family": "plan",
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a1", "predicate": "plan",
            "object": "a2", "value": "把对账单发给你", "category": "plan",
            "mode": "stated", "time_text": "下周三之前", "quote": CANARY,
            "evidence": ["t1"], "text": "张三计划在下周三之前把对账单发给李四。"
        }],
        "empty_turns": ["t2", "t3"]
    })
}

fn plan_review_pass() -> Value {
    json!({
        "schema_version": 0, "pass": "fact_review", "family": "plan",
        "verdicts": [{"ref": "f1", "verdict": "pass", "code": null, "evidence": EVIDENCE}]
    })
}

fn reply(text: &Value) -> Value {
    Value::String(text.to_string())
}

/// `distill once --pass fact --family <family> --replies <file>`；`extra` 用来覆盖/补充旗标
/// （例如刻意不给 `--family` 的那条用例）。
fn distill(dir: &Path, family: Option<&str>, replies: &[Value]) -> Output {
    let p = dir.join("fact-replies.json");
    std::fs::write(&p, Value::Array(replies.to_vec()).to_string()).unwrap();
    let dd = data_dir(dir);
    let mut args: Vec<&str> = vec!["distill", "once", "--pass", "fact"];
    if let Some(family) = family {
        args.push("--family");
        args.push(family);
    }
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

/// 1. 成功一批：exit 0、单 JSON（十一格）、库里有 fact/owner、每轮状态、`llm_job:fact` 记 3。
#[test]
fn a_fact_pass_writes_rows_owner_and_done_states() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let out = distill(
        dir.path(),
        Some("promise"),
        &reviewed(&promise_reply(), &review_pass()),
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));

    let doc = stdout_json(&out);
    assert_eq!(doc["schema_version"], 2, "W33-A10: 运维面加了观测两格");
    assert_eq!(doc["pass"], "fact");
    assert_eq!(doc["family"], "promise");
    assert_eq!(doc["turns"], 3);
    assert_eq!(doc["drafts"], 1);
    assert_eq!(doc["dropped"], 0, "正常批没有草稿被丢");
    assert_eq!(doc["uncovered"], 0, "正常批覆盖完整");
    assert_eq!(doc["written"], 1);
    assert_eq!(doc["duplicate"], 0, "第一族没有可去重的行");
    assert_eq!(doc["rejected"], 0);
    assert_eq!(doc["uncertain"], 0);
    assert_eq!(doc["facts"], 1);
    assert_eq!(doc["cards"], 1);
    assert_eq!(doc["mentions"], 2);
    // 转换器配套：读数多一格 `system_turns_skipped`（additive；干净批读 0）。
    assert_eq!(doc["system_turns_skipped"], 0, "干净的一批没有 system 轮：{doc}");
    assert_eq!(doc["state"], "done");
    // 观测面（W33-A10）：回放座位没有真实出网 ⇒ max_call_ms 恒 0（stub 不发明耗时）；
    // 脚本没有显式 timeout ⇒ timeouts 恒 0。键必须在（旧二进制缺键由 daily-drain 聚合器兜底）。
    assert_eq!(doc["max_call_ms"], 0, "stub 不发明传输耗时：{doc}");
    assert_eq!(doc["timeouts"], 0, "没有显式超时 ⇒ 计数 0：{doc}");
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(count(&root, "fact"), 1, "库里必须真有 fact 行");
    let (owner, created_by): (String, String) = root
        .memory()
        .conn()
        .query_row(
            "SELECT owner_agent_id, created_by_agent FROM resource_owner WHERE mem_table='fact'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(owner, "alice", "归属继承会话的 owner_agent_id");
    assert_eq!(created_by, "agent-fact");
    assert_eq!(pass_state(&root, &uids[0], "promise").0, "done");
    assert_eq!(pass_state(&root, &uids[1], "promise").0, "empty");
    assert_eq!(pass_state(&root, &uids[2], "promise").0, "empty");
    assert_eq!(llm_calls(&root, "fact"), 3, "抽取 + proposer + challenger 三次调用");
    assert_eq!(llm_calls(&root, "naming"), 0, "计数器按 pass 分账");
}

/// S3a 阳性（评审 I-1 的 record 半边）：fact 触顶时收尾那一句必须把 `llm_job:fact` 写成 1。
/// 变异电池：删掉 `run_distill_fact` 收尾的 `record_quota_stopped(...)` ⇒ 停在清 0 的 0 ⇒ 红；
/// 主语打错（`PASS_FACT` → `NAMING_PASS`）⇒ `fact` 那一格没有行 ⇒ 红，第二句同时断
/// `naming` 那一格没被写上。
#[test]
fn a_quota_stop_marks_the_fact_cell_and_no_other() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let out = distill(dir.path(), Some("promise"), &[json!({"fail": "quota_exhausted"})]);
    assert_eq!(code(&out), 3, "触顶是 LLM 面故障（退出 3）：{}", stderr_text(&out));
    assert!(
        stderr_text(&out).contains("llm quota_exhausted"),
        "stderr 少了四态那一格：{}",
        stderr_text(&out)
    );

    let root = open(dir.path());
    assert_eq!(quota_stopped(&root, "fact"), Some(1),
        "触顶那一轮必须落在 `llm_job:fact` 自己的格上（主语写错＝这一格没有行）");
    assert_eq!(quota_stopped(&root, "naming"), None,
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
        root.ledger(), "llm_job", "fact", rc_ledger::counters::QUOTA_STOPPED_METRIC, 1, NOW,
    )
    .unwrap();
    assert_eq!(quota_stopped(&root, "fact"), Some(1), "夹具必须先真的种上 1");
    drop(root);

    let out = distill(dir.path(), Some("promise"), &[]);
    assert_eq!(code(&out), 0, "空批是合法结果：{}", stderr_text(&out));
    assert_eq!(stdout_json(&out)["state"], "none");
    assert_eq!(stdout_json(&out)["family"], "promise");

    let root = open(dir.path());
    assert_eq!(quota_stopped(&root, "fact"), Some(0),
        "早退的运行也必须把上一次的 1 清掉（否则 status 会一直报上次的触顶）");
}

/// 2. 再跑一次：`turns=0`、`state="none"`、退出 0、行数与计数器都不动。
#[test]
fn a_second_run_is_an_empty_batch_with_no_new_rows() {
    let dir = tempfile::tempdir().unwrap();
    let _uids = seed(dir.path());
    let first = distill(
        dir.path(),
        Some("promise"),
        &reviewed(&promise_reply(), &review_pass()),
    );
    assert_eq!(code(&first), 0, "stderr: {}", stderr_text(&first));

    let root = open(dir.path());
    let tables = ["fact", "card", "provenance", "memory_mention", "resource_owner"];
    let before: Vec<i64> = tables.iter().map(|t| count(&root, t)).collect();
    let calls_before = llm_calls(&root, "fact");
    drop(root);

    let again = distill(
        dir.path(),
        Some("promise"),
        &reviewed(&promise_reply(), &review_pass()),
    );
    assert_eq!(code(&again), 0, "stderr: {}", stderr_text(&again));
    let doc = stdout_json(&again);
    assert_eq!(doc["turns"], 0);
    assert_eq!(doc["family"], "promise");
    assert_eq!(doc["written"], 0);
    assert_eq!(doc["dropped"], 0);
    assert_eq!(doc["uncovered"], 0);
    assert_eq!(doc["state"], "none");

    let root = open(dir.path());
    let after: Vec<i64> = tables.iter().map(|t| count(&root, t)).collect();
    assert_eq!(before, after, "空批不许长行");
    assert_eq!(llm_calls(&root, "fact"), calls_before, "没开口就不加 llm_calls");
}

/// 3. `--family` 的三条用法错误：缺它、家族值不认识、其它 pass 带它。全部 exit 3 且不开库。
#[test]
fn family_flag_usage_errors_exit_three_without_opening_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let dd = data_dir(dir.path());

    // 缺 --family：stderr 点名旗标与闭集，stdout 为空。
    let out = run(&["distill", "once", "--pass", "fact", "--data-dir", &dd]);
    assert_eq!(code(&out), 3, "stderr: {}", stderr_text(&out));
    assert!(out.stdout.is_empty(), "配置故障不开库也不打读数");
    assert!(stderr_text(&out).contains("--family"), "stderr: {}", stderr_text(&out));
    assert!(stderr_text(&out).contains("decision"), "stderr: {}", stderr_text(&out));

    // 家族值不认识：不回显操作员递进来的那个词，只列闭集。
    let out = run(&["distill", "once", "--pass", "fact", "--family", "meeting", "--data-dir", &dd]);
    assert_eq!(code(&out), 3);
    assert!(stderr_text(&out).contains("promise"), "stderr: {}", stderr_text(&out));
    assert!(!stderr_text(&out).contains("meeting"), "stderr 回显了未知值：{}", stderr_text(&out));

    // naming 不接受 --family。
    let out =
        run(&["distill", "once", "--pass", "naming", "--family", "promise", "--data-dir", &dd]);
    assert_eq!(code(&out), 3);
    assert!(stderr_text(&out).contains("naming"), "stderr: {}", stderr_text(&out));

    // 不认识的 pass 仍点名只支持哪几条（S2c-2 起 event、S2d-2 起 time 已是合法 pass，
    // 本格用 design §6 里尚未实现的 relation 当未知值）。
    let out = run(&["distill", "once", "--pass", "relation", "--data-dir", &dd]);
    assert_eq!(code(&out), 3);
    assert!(stderr_text(&out).contains("--pass"), "stderr: {}", stderr_text(&out));
}

/// 4. 审核 reject：exit 0（正常拒收，不是故障）、`written=0`、`state=rejected`、库里无 fact。
#[test]
fn a_review_rejection_exits_zero_with_no_fact_rows() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    // 两档都 reject(not_durable)：proposer 判不成立，challenger 也找到了同一处反证，
    // 裁决表落 reject 且 code 两侧一致（challenger 的 code 胜出，但不改变判决语义）。
    let out = distill(
        dir.path(),
        Some("promise"),
        &reviewed(&promise_reply(), &review_reject()),
    );
    assert_eq!(code(&out), 0, "审核 reject 是正常结果，stderr: {}", stderr_text(&out));

    let doc = stdout_json(&out);
    assert_eq!(doc["turns"], 3);
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
    let (state, reason, _) = pass_state(&root, &uids[0], "promise");
    assert_eq!(state, "rejected");
    assert_eq!(reason.as_deref(), Some("review_reject"));
    assert_eq!(pass_state(&root, &uids[1], "promise").0, "empty");
}

/// 5. 审核单元级故障（超时）：**exit 0**、`state=pending`、每轮退避、零行（W19c2：
///    坏的是这一格，整族不该因为它在退避里就停；座位级故障那条仍 exit 3）。
#[test]
fn a_review_llm_failure_stays_pending_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    // 故障落在**第一次**审核调用（proposer）上：整 item 立刻上抛，challenger 那一档不再发，
    // 所以队列只需要抽取 + 一次 `{"fail":...}`。
    let out = distill(
        dir.path(),
        Some("promise"),
        &[reply(&promise_reply()), json!({"fail": "timeout"})],
    );
    assert_eq!(code(&out), 0, "单元级故障不是这一族的失败：{}", stderr_text(&out));
    // rc=0 的那一发不打 `error:` 行（hub 的"两件事同时出去"只发生在失败面）。
    assert!(
        !stderr_text(&out).contains("error:"),
        "成功的读数不该带运维错误行: {}",
        stderr_text(&out)
    );
    let doc = stdout_json(&out);
    assert_eq!(doc["turns"], 3);
    assert_eq!(doc["written"], 0, "审核没答 ⇒ 一行都不许写");
    assert_eq!(doc["facts"], 0);
    assert_eq!(doc["state"], "pending");
    // 观测面（W33-A10）：显式声明的 timeout 透传进读数——审核档的失败也是这一发的账。
    assert_eq!(doc["timeouts"], 1, "显式 timeout 要进观测面：{doc}");
    assert_eq!(doc["max_call_ms"], 0, "stub 不发明传输耗时：{doc}");
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(count(&root, "fact"), 0, "审核故障不落记忆行");
    for uid in &uids {
        let (state, reason, due) = pass_state(&root, uid, "promise");
        assert_eq!(state, "pending", "{uid}");
        assert!(reason.is_none(), "{uid}");
        assert!(due.is_some(), "{uid} 必须写下退避时刻");
    }
}

/// 6. 审核契约拒收（坏 JSON）：exit 2、`error: rejected`、stdout 的 `state` 与 ledger 一致
///    （`quarantined`，不是 `rejected`）、零行。reason 是 S2e-1 的精确格：`review_json`。
#[test]
fn a_malformed_review_exits_two_and_reports_the_quarantined_state() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    // 两档都回坏 JSON：两侧都 `Failed(Json)` ⇒ 隔离，reason 取 proposer 的 `review_json`。
    let out = distill(
        dir.path(),
        Some("promise"),
        &reviewed(&promise_reply(), &json!({})),
    );
    assert_eq!(code(&out), 2, "stderr: {}", stderr_text(&out));
    assert!(
        stderr_text(&out).contains("error: rejected"),
        "stderr: {}",
        stderr_text(&out)
    );
    let doc = stdout_json(&out);
    assert_eq!(doc["turns"], 3);
    assert_eq!(doc["written"], 0);
    assert_eq!(doc["facts"], 0);
    assert_eq!(doc["state"], "quarantined", "stdout 的 state 必须与 ledger 一致");
    // W19c2：契约拒收的隔离是**质量**隔离，不是"重试耗尽"——日路径的连续隔离墙不许数它。
    assert_eq!(doc["retry_exhausted"], 0, "契约隔离不许带重试耗尽：{doc}");
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(count(&root, "fact"), 0);
    for uid in &uids {
        let (state, reason, _) = pass_state(&root, uid, "promise");
        assert_eq!(state, "quarantined", "{uid}");
        assert_eq!(reason.as_deref(), Some("review_json"), "{uid}");
    }
}

/// 7. 跨 family 去重（S2e-6）：promise 已落 (t1, 把对账单发给李四)，plan 落同 evidence+value
///    ⇒ stdout 的 `duplicate=1`、`written=0`、`facts=0`、`state=done`，库里仍是一行。
#[test]
fn a_second_family_with_the_same_evidence_and_value_reports_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let _uids = seed(dir.path());
    let first = distill(
        dir.path(),
        Some("promise"),
        &reviewed(&promise_reply(), &review_pass()),
    );
    assert_eq!(code(&first), 0, "stderr: {}", stderr_text(&first));
    let doc = stdout_json(&first);
    assert_eq!(doc["facts"], 1);

    let second = distill(
        dir.path(),
        Some("plan"),
        &reviewed(&plan_reply(), &plan_review_pass()),
    );
    assert_eq!(code(&second), 0, "去重是正常结果，stderr: {}", stderr_text(&second));
    let doc = stdout_json(&second);
    assert_eq!(doc["family"], "plan");
    assert_eq!(doc["drafts"], 1);
    assert_eq!(doc["written"], 0, "同 evidence+value 已在库 ⇒ 不交给写库层");
    assert_eq!(doc["duplicate"], 1, "重复必须计数可见");
    assert_eq!(doc["rejected"], 0, "去重不算拒收");
    assert_eq!(doc["uncertain"], 0, "去重不算不确定");
    assert_eq!(doc["facts"], 0);
    assert_eq!(doc["state"], "done", "全被去重 ⇒ 已处理");
    assert_no_body_or_path(&second);

    let root = open(dir.path());
    assert_eq!(count(&root, "fact"), 1, "库里不许出现第二行");
}

/// 8. 一条坏引文只丢那一条（S2e-8b，真实冒烟的形态）：exit 0、`drafts=1`、`dropped=1`、
///    `written=1`、该轮 done——整批不再被一条坏草稿拖死。
#[test]
fn one_bad_quote_is_dropped_without_losing_the_rest_of_the_batch() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let mut extraction = promise_reply();
    let good = extraction["facts"][0].clone();
    let mut bad = good.clone();
    bad["ref"] = json!("f2");
    bad["quote"] = json!("我下周三之前把对账单发给你了");
    extraction["facts"] = json!([good, bad]);

    let out = distill(
        dir.path(),
        Some("promise"),
        &reviewed(&extraction, &review_pass()),
    );
    assert_eq!(code(&out), 0, "单条丢弃是正常结果，stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["turns"], 3);
    assert_eq!(doc["drafts"], 1, "只数保留下来的合法草稿");
    assert_eq!(doc["dropped"], 1);
    assert_eq!(doc["uncovered"], 0);
    assert_eq!(doc["written"], 1);
    assert_eq!(doc["rejected"], 0);
    assert_eq!(doc["facts"], 1);
    assert_eq!(doc["state"], "done");
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(count(&root, "fact"), 1);
    assert_eq!(pass_state(&root, &uids[0], "promise").0, "done");
    assert_eq!(llm_calls(&root, "fact"), 3, "抽取 + proposer + challenger（审核只喂保留的草稿）");
}

/// 8. 抽取档的显式超时（W33-A10 观测面）：单元级失败落 `state=pending`、exit 0，
///    且 `timeouts` 带着真实计数——`Rotating` 对 Timeout 原样重试一次，但脚本已耗尽
///    那一支（`Seat` 的 `None`）不是传输超时，不计进观测面（`replay.rs` 里那条注释）。
///    max_call_ms 在 stub 下恒 0（stub 不发明耗时）。
#[test]
fn an_extraction_timeout_shows_up_in_the_observation_fields() {
    let dir = tempfile::tempdir().unwrap();
    let _ = seed(dir.path());
    let out = distill(dir.path(), Some("promise"), &[json!({"fail": "timeout"})]);
    assert_eq!(code(&out), 0, "单元级故障不是这一族的失败：{}", stderr_text(&out));
    assert!(
        !stderr_text(&out).contains("error:"),
        "成功的读数不该带运维错误行：{}",
        stderr_text(&out)
    );
    let doc = stdout_json(&out);
    assert_eq!(doc["state"], "pending");
    assert_eq!(doc["timeouts"], 1, "显式 timeout 计数透传：{doc}");
    assert_eq!(doc["max_call_ms"], 0, "stub 不发明传输耗时：{doc}");
    assert_no_body_or_path(&out);
}
