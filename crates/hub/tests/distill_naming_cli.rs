// crates/hub/tests/distill_naming_cli.rs
//! S2a-3：`hub distill once --pass naming` 的黑盒进程面。照 `cycle_once.rs` 的写法：
//! CLI 是这条 pass 的唯一出口，装配漏了一格（来源、选批、计数器、退出码）只有子进程看得见。
//!
//! 纪律与 `cycle_once.rs` 同源：
//! - **不跑真实 LLM**：`--replies` 是唯一来源，脚本里只有合法的 A1 输出或 `{"fail":...}`；
//! - stdout 只许一个 JSON（守卫 12）：用例同时断正文与 data-dir 路径不出现；
//! - 退避不 sleep：`now_ms` 是参数，到期那一步用 SQL 把 `next_retry_at_ms` 改到过去。
use rc_contract::{normalize, parse};
use rc_ledger::insert_batch;
use rc_store::Root;
use rusqlite::params;
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Output};

const DEV: &str = "dev-naming";
const SESS: &str = "sess-naming";
const DATA: &str = "data";
const NOW: i64 = 1_700_000_000_000;
/// 正文里的记号：断它**不出现在** stdout/stderr（守卫 12 的零正文）。
const CANARY: &str = "张三下周末搬家";

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

/// 一条 naming 状态行：`(state, next_retry_at_ms)`。
fn pass_state(root: &Root, uid: &str) -> (String, Option<i64>) {
    root.ledger()
        .conn()
        .query_row(
            "SELECT state, next_retry_at_ms FROM extract_pass_state
             WHERE turn_uid=?1 AND pass_id='naming' AND family=''",
            [uid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

/// `llm_job:naming` 的 `llm_calls` 总账（naming 走默认族，计数器主语是 pass_id）。
fn naming_calls(root: &Root) -> i64 {
    root.ledger()
        .conn()
        .query_row(
            "SELECT coalesce(sum(count),0) FROM usage_counter
             WHERE subject_kind='llm_job' AND subject_id='naming' AND metric='llm_calls'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

/// S3a：某一主语（`llm_job:<pass>`）的"最近一次运行是否触顶"格。`None` = 这一 pass 从没写过。
/// 读的是**生产侧写下去的那一行**，不是读侧自己推出来的结论。
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

/// 把某个主语的触顶格种成 `value`：模拟"上一次运行留下的值"。写的是 `set_one`（覆盖），
/// 与生产侧同一个入口、同一种语义。
fn seed_quota_stopped(root: &Root, subject: &str, value: i64) {
    rc_ledger::counters::set_one(
        root.ledger(),
        "llm_job",
        subject,
        rc_ledger::counters::QUOTA_STOPPED_METRIC,
        value,
        NOW,
    )
    .unwrap();
}

/// 播种两轮真转录（`parse` → `normalize` → `insert_batch`，与 `rc-distill` 的夹具同一条链），
/// 再把会话归属改成非默认值：naming 批要继承的那一行。
fn seed(dir: &Path) -> Vec<String> {
    let doc = json!({
        "format_version": 1,
        "sessions": [{
            "session_key": SESS,
            "device_id": DEV,
            "source_agent": "agent-naming",
            "turns": [
                {"role": "human", "body": format!("{CANARY}，老张也来帮忙"), "created_at_ms": NOW},
                {"role": "agent", "body": "记下了：张三搬家的时间是下周末，老张也来帮忙", "created_at_ms": NOW + 1},
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
    uids
}

/// 合法 A1 输出：一个 person 锚（evidence 两轮）+ 一条 t2 的提及。
fn a1_json() -> String {
    json!({
        "schema_version": 4, "pass": "a1",
        "anchors": [{"ref": "a1", "kind": "person", "canonical": "张三",
                     "aliases": ["老张"], "reality_status": "real",
                     "evidence": ["t1", "t2"], "reason": "两轮点名"}],
        "mentions": [{"anchor_ref": "a1", "surface": "老张", "turn_uid": "t2",
                      "speaker_ref": null, "discourse_role": "mentioned",
                      "resolution": "resolved", "confidence": 0.9}],
        "issues": [], "unresolved_surfaces": []
    })
    .to_string()
}

fn reply(text: &str) -> Value {
    Value::String(text.to_owned())
}

/// naming 窄审输出（S3b-3c）：逐锚判决，`pass` 的 code 必须是显式 null。
/// runner 在闸 pass 之后必然开口问窄审，所以成功路径的脚本是"A1、A1、naming_review"三段。
fn naming_review_json() -> String {
    json!({
        "schema_version": 0, "pass": "naming_review",
        "verdicts": [{"ref": "a1", "verdict": "pass", "code": null}]
    })
    .to_string()
}

/// 真实冒烟的形状（发现 2）：一个真锚 + 一个反引号代码令牌；两条提及各指向一个。
/// 过滤闸在 runner 里跑，CLI 的 `filtered` 必须把这件事说出来。
fn noise_a1_json() -> String {
    json!({
        "schema_version": 4, "pass": "a1",
        "anchors": [
            {"ref": "a1", "kind": "person", "canonical": "张三",
             "aliases": ["老张"], "reality_status": "real",
             "evidence": ["t1", "t2"], "reason": "两轮点名"},
            {"ref": "a2", "kind": "object", "canonical": "`99c05800…`",
             "aliases": [], "reality_status": "unknown",
             "evidence": ["t1"], "reason": "代码令牌"}
        ],
        "mentions": [
            {"anchor_ref": "a1", "surface": "老张", "turn_uid": "t2",
             "speaker_ref": null, "discourse_role": "mentioned",
             "resolution": "resolved", "confidence": 0.9},
            {"anchor_ref": "a2", "surface": "`99c05800…`", "turn_uid": "t1",
             "speaker_ref": null, "discourse_role": "mentioned",
             "resolution": "resolved", "confidence": 0.8}
        ],
        "issues": [], "unresolved_surfaces": []
    })
    .to_string()
}

fn fail(slug: &str) -> Value {
    json!({"fail": slug})
}

/// `distill once --pass naming --replies <file>`（同一目录下覆盖写同一份脚本）。
fn distill(dir: &Path, replies: &[Value]) -> Output {
    let p = dir.join("naming-replies.json");
    std::fs::write(&p, Value::Array(replies.to_vec()).to_string()).unwrap();
    run(&[
        "distill",
        "once",
        "--pass",
        "naming",
        "--replies",
        p.to_str().unwrap(),
        "--data-dir",
        &data_dir(dir),
    ])
}

/// 守卫 12：stdout 只有读数，没有正文、没有路径。
fn assert_no_body_or_path(out: &Output) {
    let text = stdout_text(out);
    assert!(!text.contains(CANARY), "stdout 带正文：{text}");
    assert!(!text.contains(DATA), "stdout 带路径：{text}");
}

/// 1+2. 成功一批：exit 0、单 JSON、`turns=2`、`anchors>=1`、库里有 owner 行、
///    两轮 `done`；`llm_calls` 记 3（A1 双跑 + 窄审一次）。
#[test]
fn a_naming_pass_writes_rows_owner_and_done_states() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let out = distill(
        dir.path(),
        &[reply(&a1_json()), reply(&a1_json()), reply(&naming_review_json())],
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));

    let doc = stdout_json(&out);
    assert_eq!(doc["schema_version"], 2, "W33-A10: 运维面加了观测两格");
    assert_eq!(doc["pass"], "naming");
    assert_eq!(doc["turns"], 2);
    assert!(
        doc["anchors"].as_u64().unwrap() >= 1,
        "双跑交集必须落出至少一个锚：{doc}"
    );
    assert_eq!(doc["filtered"], 0, "干净的一批不该丢锚");
    assert_eq!(doc["state"], "done");
    // W19c2：正常跑完的一批这一格恒 0（`state=pending` 的"入队待审核"同理）。
    assert_eq!(doc["retry_exhausted"], 0, "成功的批不许带重试耗尽：{doc}");
    assert_no_body_or_path(&out);
    assert!(!stderr_text(&out).contains(CANARY), "stderr 带正文");

    let root = open(dir.path());
    assert!(count(&root, "anchor") >= 1);
    assert_eq!(
        root.memory()
            .conn()
            .query_row(
                "SELECT count(*) FROM resource_owner WHERE mem_table='anchor'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1,
        "锚必须带一行归属"
    );
    for uid in &uids {
        assert_eq!(pass_state(&root, uid).0, "done", "{uid}");
    }
    assert_eq!(naming_calls(&root), 3, "A1 双跑 + 窄审 = 三次调用");
    // S3a：这一 pass 收尾必须落"最近一次运行没触顶"那一格 0（读侧 `status`/`doctor` 靠它
    // 把 null 与 false 分开）。写的是覆盖语义，所以重复跑也只会是这一行。
    assert_eq!(quota_stopped(&root, "naming"), Some(0),
        "跑完而没触顶的一轮必须把这一格写成 0");
}

/// S3a 阳性（评审 I-1 的 record 半边）：这一 pass 触顶时，收尾那一句必须把**自己那一格**
/// 写成 1。变异电池：
/// - 删掉 `run_distill_naming` 收尾的 `record_quota_stopped(...)` ⇒ 这一格停在开头清 0 的 0
///   （或者干脆没有行）⇒ 本用例红；
/// - 主语打错（`NAMING_PASS` → `PASS_FACT`）⇒ `naming` 那一格没有行 ⇒ 红；第二句同时断
///   别人的格子没有被写上，把"写对了值、写错了主语"这一型也钉住。
#[test]
fn a_quota_stop_marks_the_naming_cell_and_no_other() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let out = distill(dir.path(), &[fail("quota_exhausted")]);
    assert_eq!(code(&out), 3, "触顶是 LLM 面故障（退出 3）：{}", stderr_text(&out));
    assert!(
        stderr_text(&out).contains("llm quota_exhausted"),
        "stderr 少了四态那一格：{}",
        stderr_text(&out)
    );

    let root = open(dir.path());
    assert_eq!(quota_stopped(&root, "naming"), Some(1),
        "触顶那一轮必须落在 `llm_job:naming` 自己的格上（主语写错＝这一格没有行）");
    assert_eq!(quota_stopped(&root, "fact"), None,
        "主语写成别的 pass 会在这里多出一行：这一格必须是空的");
}

/// S3a 阴性（评审 I-1 的 clear 半边）：空批是**早退**——它走到 `finish_naming` 就返回，
/// 收尾的 record 一次都不执行，所以这一格只能是开头那一次清 0 写的。
/// 种 1 再跑空批：删掉 `clear_quota_stopped` 那一行，这一格会留在 1 ⇒ 本用例红
/// （这正是"clear 与 record 都写 0，分不开"的那个缺口）。
#[test]
fn an_empty_batch_clears_the_previous_runs_quota_cell() {
    let dir = tempfile::tempdir().unwrap();
    // 空库：没有候选轮 ⇒ `Ok(None)` ⇒ 早退，永远走不到收尾那一句。
    let root = open(dir.path());
    seed_quota_stopped(&root, "naming", 1);
    assert_eq!(quota_stopped(&root, "naming"), Some(1), "夹具必须先真的种上 1");
    drop(root);

    let out = distill(dir.path(), &[]);
    assert_eq!(code(&out), 0, "空批是合法结果：{}", stderr_text(&out));
    assert_eq!(stdout_json(&out)["state"], "none");

    let root = open(dir.path());
    assert_eq!(quota_stopped(&root, "naming"), Some(0),
        "早退的运行也必须把上一次的 1 清掉（否则 status 会一直报上次的触顶）");
}

/// 3. 再跑一次：`turns=0`、`state="none"`、退出 0、行数与计数器都不动。
#[test]
fn a_second_run_is_an_empty_batch_with_no_new_rows() {
    let dir = tempfile::tempdir().unwrap();
    let _uids = seed(dir.path());
    let first = distill(
        dir.path(),
        &[reply(&a1_json()), reply(&a1_json()), reply(&naming_review_json())],
    );
    assert_eq!(code(&first), 0, "stderr: {}", stderr_text(&first));

    let root = open(dir.path());
    let tables = ["anchor", "alias", "mention", "provenance", "resource_owner"];
    let before: Vec<i64> = tables.iter().map(|t| count(&root, t)).collect();
    let calls_before = naming_calls(&root);
    drop(root);

    let again = distill(dir.path(), &[reply(&a1_json()), reply(&a1_json())]);
    assert_eq!(code(&again), 0, "stderr: {}", stderr_text(&again));
    let doc = stdout_json(&again);
    assert_eq!(doc["turns"], 0);
    assert_eq!(doc["anchors"], 0);
    assert_eq!(doc["mentions"], 0);
    assert_eq!(doc["filtered"], 0, "没跑模型就没有过滤计数");
    assert_eq!(doc["state"], "none");

    let root = open(dir.path());
    let after: Vec<i64> = tables.iter().map(|t| count(&root, t)).collect();
    assert_eq!(before, after, "空批不许长行");
    assert_eq!(naming_calls(&root), calls_before, "没开口就不加 llm_calls");
}

/// 4. 坏 replies：exit 2、stderr `error: rejected`、记忆库零行、两轮 `rejected`。
#[test]
fn a_bad_reply_exits_two_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let out = distill(dir.path(), &[reply("{}"), reply("{}")]);
    assert_eq!(code(&out), 2, "stderr: {}", stderr_text(&out));
    assert!(
        stderr_text(&out).contains("error: rejected"),
        "stderr: {}",
        stderr_text(&out)
    );
    let doc = stdout_json(&out);
    assert_eq!(doc["turns"], 2);
    assert_eq!(doc["state"], "rejected");
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(count(&root, "anchor"), 0);
    assert_eq!(count(&root, "mention"), 0);
    assert_eq!(count(&root, "resource_owner"), 0, "失败批不写归属");
    for uid in &uids {
        assert_eq!(pass_state(&root, uid).0, "rejected", "{uid}");
    }
}

/// 5. 来源两格：都不给 ⇒ 3 且点名两个旗标；`--pass fact` ⇒ 非 0 且 stderr 点名只支持 naming。
#[test]
fn a_missing_source_and_an_unknown_pass_are_configuration_faults() {
    let dir = tempfile::tempdir().unwrap();
    let dd = data_dir(dir.path());
    let out = run(&["distill", "once", "--pass", "naming", "--data-dir", &dd]);
    assert_eq!(code(&out), 3);
    assert!(
        stderr_text(&out).contains("no llm reply source"),
        "stderr: {}",
        stderr_text(&out)
    );
    assert!(out.stdout.is_empty(), "配置故障不开库也不打读数");

    let p = dir.path().join("naming-replies.json");
    std::fs::write(&p, Value::Array(vec![reply(&a1_json())]).to_string()).unwrap();
    let out = run(&[
        "distill",
        "once",
        "--pass",
        "fact",
        "--replies",
        p.to_str().unwrap(),
        "--data-dir",
        &dd,
    ]);
    assert_ne!(code(&out), 0);
    assert!(stderr_text(&out).contains("naming"), "stderr: {}", stderr_text(&out));
}

/// 6. 退避重试：单元级 LLM 故障（超时）⇒ **rc=0**、`state=pending` + 两轮 pending +
///    `next_retry_at_ms`（W19c2：坏的是这一格，整族不该因为它在退避里就停）；
///    立刻再跑 ⇒ `turns=0`；把退避改到过去再跑 ⇒ 重新选中（`turns=2`）并真的又开了两次口。
#[test]
fn a_provider_failure_backs_off_and_the_due_batch_is_retried() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let out = distill(dir.path(), &[fail("timeout"), fail("timeout")]);
    assert_eq!(code(&out), 0, "单元级失败不是这一族的失败：{}", stderr_text(&out));
    // rc=0 的那一发不打 `error:` 行（hub 的"两件事同时出去"只发生在失败面）。
    assert!(
        !stderr_text(&out).contains("error:"),
        "成功的读数不该带运维错误行: {}",
        stderr_text(&out)
    );
    let doc = stdout_json(&out);
    assert_eq!(doc["turns"], 2);
    assert_eq!(doc["state"], "pending", "退避中的一批是 pending，不是终态：{doc}");

    let root = open(dir.path());
    for uid in &uids {
        let (state, due) = pass_state(&root, uid);
        assert_eq!(state, "pending", "{uid}");
        assert!(due.is_some(), "{uid} 必须写下退避时刻");
    }
    assert_eq!(count(&root, "anchor"), 0, "LLM 故障不落记忆行");
    assert_eq!(naming_calls(&root), 2, "Rotating 对 Timeout 先原样重试一次");
    drop(root);

    // 退避未到期：同一数据目录立刻再跑 ⇒ 没有可选的轮次。
    let too_soon = distill(dir.path(), &[fail("timeout"), fail("timeout")]);
    assert_eq!(code(&too_soon), 0, "stderr: {}", stderr_text(&too_soon));
    assert_eq!(stdout_json(&too_soon)["turns"], 0);
    assert_eq!(stdout_json(&too_soon)["state"], "none");

    // 时钟不动，把 ledger 的退避改到过去：选批必须重新选中并再跑一轮。
    let root = open(dir.path());
    root.ledger()
        .conn()
        .execute(
            "UPDATE extract_pass_state SET next_retry_at_ms=0
             WHERE pass_id='naming' AND family=''",
            [],
        )
        .unwrap();
    drop(root);

    let retried = distill(dir.path(), &[fail("timeout"), fail("timeout")]);
    assert_eq!(code(&retried), 0, "单元级失败仍是 rc=0：{}", stderr_text(&retried));
    let doc = stdout_json(&retried);
    assert_eq!(doc["turns"], 2, "到期的 pending 必须重新选中");
    assert_eq!(doc["state"], "pending");
    let root = open(dir.path());
    assert_eq!(naming_calls(&root), 4, "第二次尝试真的又开了两次口");
}

/// 7. `stale` 行不靠时钟重选：改状态再跑，批又被选中且重放不长行。
#[test]
fn a_stale_row_is_selected_again_and_the_replay_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let _uids = seed(dir.path());
    let first = distill(
        dir.path(),
        &[reply(&a1_json()), reply(&a1_json()), reply(&naming_review_json())],
    );
    assert_eq!(code(&first), 0, "stderr: {}", stderr_text(&first));

    let root = open(dir.path());
    let tables = ["anchor", "alias", "mention", "provenance", "resource_owner"];
    let before: Vec<i64> = tables.iter().map(|t| count(&root, t)).collect();
    root.ledger()
        .conn()
        .execute(
            "UPDATE extract_pass_state SET state='stale'
             WHERE pass_id='naming' AND family=''",
            [],
        )
        .unwrap();
    drop(root);

    // `stale` 重选时 `load_candidates` 已看得见上一轮落下的锚 ⇒ 指纹（载荷 run_id）变了，
    // 这是 stale 的本义（旧载荷作废）；新载荷同样要过窄审，所以脚本仍是三段。
    let again = distill(
        dir.path(),
        &[reply(&a1_json()), reply(&a1_json()), reply(&naming_review_json())],
    );
    assert_eq!(code(&again), 0, "stderr: {}", stderr_text(&again));
    let doc = stdout_json(&again);
    assert_eq!(doc["turns"], 2, "stale 不看退避时刻");
    assert_eq!(doc["state"], "done");

    let root = open(dir.path());
    let after: Vec<i64> = tables.iter().map(|t| count(&root, t)).collect();
    assert_eq!(before, after, "重放同输入不长行");
}

/// 8. 噪声锚（S2e-5）：模型给出反引号代码令牌，runner 后置过滤；stdout 的 `filtered=1`，
///    库里只剩真锚，令牌那条提及跟着走，退出码仍是 0（过滤不是失败）。
#[test]
fn a_noise_anchor_is_filtered_and_counted_in_the_readout() {
    let dir = tempfile::tempdir().unwrap();
    let _uids = seed(dir.path());
    let out = distill(
        dir.path(),
        &[reply(&noise_a1_json()), reply(&noise_a1_json()), reply(&naming_review_json())],
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));

    let doc = stdout_json(&out);
    assert_eq!(doc["turns"], 2);
    assert_eq!(doc["anchors"], 1, "只剩张三：{doc}");
    assert_eq!(doc["mentions"], 1, "令牌那条提及跟着锚一起走：{doc}");
    assert_eq!(doc["filtered"], 1, "过滤计数必须可读：{doc}");
    assert_eq!(doc["state"], "done", "被过滤不等于整批失败");
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(count(&root, "anchor"), 1);
    assert_eq!(count(&root, "mention"), 1);
    let canonical: String = root
        .memory()
        .conn()
        .query_row("SELECT canonical FROM anchor", [], |r| r.get(0))
        .unwrap();
    assert_eq!(canonical, "张三", "反引号令牌不许落成锚");
}

// ---- W19c1/W19c2：卡死墙的三次隔离，与座位级硬失败的区别 ----

/// 一条 naming 状态行的三格：`(state, reason, attempt)`。
fn pass_row(root: &Root, uid: &str) -> (String, Option<String>, i64) {
    root.ledger()
        .conn()
        .query_row(
            "SELECT state, reason, attempt FROM extract_pass_state
             WHERE turn_uid=?1 AND pass_id='naming' AND family=''",
            [uid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

/// 把退避改到过去（不 sleep：钟是参数，与上面第 6 条同一手法）。
fn rewind_backoff(dir: &Path) {
    let root = open(dir);
    root.ledger()
        .conn()
        .execute(
            "UPDATE extract_pass_state SET next_retry_at_ms=0
             WHERE pass_id='naming' AND family=''",
            [],
        )
        .unwrap();
}

/// 9. W19c2 的单元级臂：对端恒回不合格的答复（`schema_mismatch`，与超时同一类——
///    坏的是这一格），连续三发 `distill once --pass naming`。
///    前两发 **rc=0** 且 `state=pending`（该单元在退避里，日路径继续选下一批，
///    脚本的连失败闸不该数它）；第三发 rc=0 且 `state=quarantined`——试满上限的那一格隔离。
#[test]
fn a_unit_level_failure_is_pending_twice_then_quarantined_all_at_zero() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());

    for round in 1..3 {
        let out = distill(dir.path(), &[fail("schema_mismatch")]);
        println!("round {round}: rc={} out={}", code(&out), stdout_text(&out).trim());
        assert_eq!(code(&out), 0, "单元级失败不是这一族的失败（rc 必须 0）：{}", stderr_text(&out));
        let doc = stdout_json(&out);
        assert_eq!(doc["state"], "pending", "第 {round} 发还没到上限：{doc}");
        assert_eq!(doc["turns"], 2, "选中了两轮：{doc}");
        assert_eq!(doc["anchors"], 0);
        assert_eq!(doc["mentions"], 0);
        // W19c2：到上限之前，这一格必须是 0（"退避中"不是"重试耗尽"）。
        assert_eq!(doc["retry_exhausted"], 0, "第 {round} 发不是重试耗尽：{doc}");
        assert_no_body_or_path(&out);
        let root = open(dir.path());
        for uid in &uids {
            let (state, reason, attempt) = pass_row(&root, uid);
            assert_eq!(state, "pending", "{uid} 第 {round} 发之后应是 pending");
            assert_eq!(reason, None, "{uid} 的 pending 不许挂终态理由");
            assert_eq!(attempt, round, "{uid} 的计数没有跨发累积");
        }
        rewind_backoff(dir.path());
    }

    let last = distill(dir.path(), &[fail("schema_mismatch")]);
    println!("round 3: rc={} out={}", code(&last), stdout_text(&last).trim());
    assert_eq!(code(&last), 0, "第三发必须 rc=0（隔离不是失败）：{}", stderr_text(&last));
    let doc = stdout_json(&last);
    assert_eq!(doc["state"], "quarantined", "第三发的读数：{doc}");
    // W19c2：这一格就是日路径隔离墙唯一认的信号——`state=quarantined` 本身不够
    // （审核不确定也交回同一个 state）。
    assert_eq!(doc["retry_exhausted"], 1, "第三发必须显式说'重试耗尽'：{doc}");
    assert_eq!(doc["turns"], 2, "选中了两轮：{doc}");
    assert_eq!(doc["anchors"], 0);
    assert_eq!(doc["mentions"], 0);
    assert_no_body_or_path(&last);

    let root = open(dir.path());
    for uid in &uids {
        let (state, reason, attempt) = pass_row(&root, uid);
        println!("row {uid}: state={state} reason={reason:?} attempt={attempt}");
        assert_eq!(state, "quarantined", "{uid} 没被隔离");
        assert_eq!(reason.as_deref(), Some("retry_exhausted"), "{uid}");
        assert_eq!(attempt, 3, "{uid} 的计数没有跨发累积");
    }
    assert_eq!(count(&root, "anchor"), 0, "隔离的批不许落记忆行");

    // 隔离之后不再被选批捡起：第四发是空批（rc=0、state=none），不是又一次重烧。
    let after = distill(dir.path(), &[fail("schema_mismatch")]);
    assert_eq!(code(&after), 0, "stderr: {}", stderr_text(&after));
    assert_eq!(stdout_json(&after)["state"], "none", "隔离行又被选中了");
}

/// 10. W19c2 的座位级臂：对端恒 `backend_error`（传输失败，与额度耗尽同一类——
///     坏的是座位）。**始终 rc≠0、行停在 pending、一次都不隔离**：座位死掉时该停的是
///     这一族（脚本的连失败闸），不是把这些单元逐个判成终态。
#[test]
fn a_seat_level_failure_never_quarantines_and_always_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());

    for round in 1..=4 {
        let out = distill(
            dir.path(),
            &[fail("backend_error"), fail("backend_error")],
        );
        println!("round {round}: rc={} out={}", code(&out), stdout_text(&out).trim());
        assert_ne!(code(&out), 0, "第 {round} 发必须非零退出：{}", stderr_text(&out));
        assert!(
            stderr_text(&out).contains("llm backend_error"),
            "第 {round} 发的 stderr 少了四态那一格：{}",
            stderr_text(&out)
        );
        let doc = stdout_json(&out);
        assert_eq!(doc["state"], "pending", "第 {round} 发的读数：{doc}");
        // 座位级失败从不交回隔离，也就永远不带"重试耗尽"那一格。
        assert_eq!(doc["retry_exhausted"], 0, "座位级失败不是重试耗尽：{doc}");
        let root = open(dir.path());
        for uid in &uids {
            let (state, reason, attempt) = pass_row(&root, uid);
            assert_eq!(state, "pending", "{uid} 第 {round} 发之后应停在 pending，不许隔离");
            assert_eq!(reason, None, "{uid} 不该有终态理由");
            assert_eq!(attempt, round, "{uid} 的计数没有跨发累积");
        }
        assert_eq!(count(&root, "anchor"), 0, "失败的发不许落记忆行");
        rewind_backoff(dir.path());
    }
}

/// 11. W19c2 的阴性臂：**一次**单元级失败之后成功 ⇒ 行回到 `done`，没有 `quarantined`。
///     上限是"连续失败数"，不是"曾经失败过"；pending 不是终态。
#[test]
fn one_failed_round_does_not_quarantine_a_batch_that_then_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());
    let first = distill(dir.path(), &[fail("timeout")]);
    assert_eq!(code(&first), 0, "超时是单元级失败（rc=0）：{}", stderr_text(&first));
    assert_eq!(stdout_json(&first)["state"], "pending");
    rewind_backoff(dir.path());

    let second = distill(
        dir.path(),
        &[reply(&a1_json()), reply(&a1_json()), reply(&naming_review_json())],
    );
    assert_eq!(code(&second), 0, "stderr: {}", stderr_text(&second));
    assert_eq!(stdout_json(&second)["state"], "done");

    let root = open(dir.path());
    for uid in &uids {
        let (state, reason, _attempt) = pass_row(&root, uid);
        assert_eq!(state, "done", "{uid} 没有回到 done");
        assert_ne!(state, "quarantined", "{uid} 被误隔离");
        assert_eq!(reason, None, "{uid} 的终态挂了理由");
    }
    assert!(count(&root, "anchor") >= 1, "成功的那一发必须真的落锚");
}
