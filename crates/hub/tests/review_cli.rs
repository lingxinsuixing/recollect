// crates/hub/tests/review_cli.rs
//! S3a-3d：`hub review` 的真进程面（设计 `2026-10-26-review-queue-s3` §9 与验收 §2）。
//!
//! 队列项用 `rc_memory::review::enqueue` / `record_decision` 直造——本文件不测存储层，
//! 只测 CLI 的边界：**默认不打印载荷**、`--state/--pass` 过滤、人工判定的追加语义与
//! 非法转换的退出码；H7a 起再加 `review prune` 的保留判定与 `stats` 的 7 天告警。
//! 每条用例一个临时 data-dir，跑完即弃。
use rc_memory::review::{
    enqueue, record_decision, Decider, Decision, NewItem, ReviewStage, ReviewVerdict,
};
use rc_store::Root;
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Output};

const DATA: &str = "data";
/// 载荷里的记号：默认输出（list / show 不带 --payload）一个字节都不许出现。
const PAYLOAD_CANARY: &str = "PAYLOAD-CANARY-9931";
/// 造 item 用的入库钟（2023-11-14）；`stats` 的最老 pending 年龄因此是正数。
const CREATED_MS: i64 = 1_700_000_000_000;
/// 一天 = 86 400 000 ms（与 `review prune` 的窗口、7 天告警同口径）。
const DAY_MS: i64 = 86_400_000;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin()).args(args).output().expect("hub binary")
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

fn data_dir(dir: &Path) -> String {
    dir.join(DATA).to_str().unwrap().to_string()
}

fn open(dir: &Path) -> Root {
    Root::open(&dir.join(DATA)).unwrap()
}

fn item(pass_id: &str, family: &str, payload: &str) -> NewItem {
    NewItem {
        source: "distill".into(),
        pass_id: pass_id.into(),
        family: family.into(),
        session_key: "sess-cli".into(),
        device_id: "dev-cli".into(),
        turn_uids_json: "[\"turn-cli\"]".into(),
        owner_json: "{\"owner_agent_id\":\"agent-cli\"}".into(),
        payload_json: payload.into(),
        batch_key: String::new(),
    }
}

/// 入队一条并返回 `(id, payload_sha256)`。
fn enqueue_item(root: &Root, pass_id: &str, family: &str, payload: &str) -> (i64, String) {
    let out = enqueue(root.memory(), &item(pass_id, family, payload), CREATED_MS).unwrap();
    (out.id, out.payload_sha256)
}

fn decision(
    stage: ReviewStage,
    verdict: ReviewVerdict,
    reason: &str,
    decider: Decider,
    sha: &str,
) -> Decision {
    Decision {
        stage,
        verdict,
        reason_code: reason.into(),
        rules_version: "review-gate-v0.1".into(),
        model: String::new(),
        prompt_version: String::new(),
        input_sha256: sha.into(),
        review_input_sha256: String::new(),
        decider,
        detail: "test".into(),
    }
}

/// 程序闸拒绝：pending_review → rejected，`program_code` 落原因码。
fn program_reject(root: &Root, id: i64, sha: &str, reason: &str) {
    record_decision(
        root.memory(),
        id,
        &decision(
            ReviewStage::Program,
            ReviewVerdict::Reject,
            reason,
            Decider::Program,
            sha,
        ),
        CREATED_MS,
    )
    .unwrap();
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// 三闸全过 ⇒ `applied`（终态）。判定台账因此有三行，prune 之后必须原样还在。
fn to_applied(root: &Root, id: i64, sha: &str) {
    for (stage, verdict, reason) in [
        (ReviewStage::Program, ReviewVerdict::Pass, "program_pass"),
        (ReviewStage::Review, ReviewVerdict::Pass, "review_pass"),
        (ReviewStage::Apply, ReviewVerdict::Pass, "apply_pass"),
    ] {
        let decider = match stage {
            ReviewStage::Review => Decider::Center,
            _ => Decider::Program,
        };
        record_decision(
            root.memory(),
            id,
            &decision(stage, verdict, reason, decider, sha),
            CREATED_MS,
        )
        .unwrap();
    }
}

/// 绕开队列 API 把一条 item 改成"库里的旧行"：CLI 的窗口判定读的就是这两列。
/// `state` 也在这里落：测试要的是"终态且很旧"，不是状态机的一次真转换。
fn backdate(root: &Root, id: i64, state: &str, updated_at_ms: i64) {
    let n = root
        .memory()
        .conn()
        .execute(
            "UPDATE review_item SET state=?2, updated_at_ms=?3 WHERE id=?1",
            rusqlite::params![id, state, updated_at_ms],
        )
        .unwrap();
    assert_eq!(n, 1, "backdate 必须命中一条");
}

/// item 的 `(state, payload_json, turn_uids_json, owner_json)`。
fn stored(root: &Root, id: i64) -> (String, String, String, String) {
    root.memory()
        .conn()
        .query_row(
            "SELECT state, payload_json, turn_uids_json, owner_json
               FROM review_item WHERE id=?1",
            rusqlite::params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
}

fn decision_count(root: &Root, id: i64) -> i64 {
    root.memory()
        .conn()
        .query_row(
            "SELECT count(*) FROM review_decision WHERE item_id=?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap()
}

/// 1. 空库：`list --json` 是 `[]`，`stats --json` 六态全 0、无最老 pending。
#[test]
fn an_empty_queue_lists_as_an_array_and_counts_zero() {
    let dir = tempfile::tempdir().unwrap();
    let dd = data_dir(dir.path());
    drop(open(dir.path()));

    let out = run(&["review", "list", "--json", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    assert_eq!(stdout_json(&out), json!([]));

    let out = run(&["review", "stats", "--json", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    for key in ["pending_review", "approved", "applied", "rejected", "quarantined", "stale"] {
        assert_eq!(doc[key], 0, "{key} 应为 0：{doc}");
    }
    assert!(doc["oldest_pending_age_ms"].is_null(), "空队列没有最老 pending：{doc}");
    assert_eq!(doc["oldest_pending_over_7d"], false, "没有 pending ⇒ 不告警：{doc}");
    assert_eq!(doc["by_pass"], json!([]));
}

/// 2. `--state` 过滤；`show` 默认只给 meta/判定历史/程序码，`--payload` 才出载荷。
#[test]
fn list_filters_and_show_hides_the_payload_until_asked() {
    let dir = tempfile::tempdir().unwrap();
    let dd = data_dir(dir.path());
    let (pending_id, rejected_id);
    {
        let root = open(dir.path());
        let (id, _sha) = enqueue_item(
            &root,
            "fact",
            "promise",
            &format!("{{\"schema\":\"review.pass.v1\",\"note\":\"{PAYLOAD_CANARY}\"}}"),
        );
        pending_id = id;
        let (id, sha) = enqueue_item(
            &root,
            "time",
            "",
            &format!("{{\"schema\":\"review.pass.v1\",\"note\":\"{PAYLOAD_CANARY}-time\"}}"),
        );
        rejected_id = id;
        program_reject(&root, id, &sha, "turn_missing");
    }

    let out = run(&["review", "list", "--json", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let all = stdout_json(&out);
    assert_eq!(all.as_array().unwrap().len(), 2);
    assert!(!stdout_text(&out).contains(PAYLOAD_CANARY), "list 不许打印载荷");

    let out = run(&["review", "list", "--json", "--state", "pending_review", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let pending = stdout_json(&out);
    let arr = pending.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], pending_id);
    assert_eq!(arr[0]["state"], "pending_review");

    let out = run(&["review", "list", "--json", "--state", "rejected", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let rejected = stdout_json(&out);
    let arr = rejected.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], rejected_id);
    assert_eq!(arr[0]["program_code"], "turn_missing");

    // show 拒绝项：判定历史与原因码在，载荷不在。
    let id = rejected_id.to_string();
    let out = run(&["review", "show", &id, "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let text = stdout_text(&out);
    assert!(text.contains("stage=program"), "show 要打印判定历史：{text}");
    assert!(text.contains("verdict=reject"), "show 要打印判定：{text}");
    assert!(text.contains("reason_code=turn_missing"), "show 要打印原因码：{text}");
    assert!(text.contains("program_code=turn_missing"), "show 要打印程序码：{text}");
    assert!(!text.contains(PAYLOAD_CANARY), "show 默认不许打印载荷：{text}");
    assert!(!text.contains("payload_json"), "show 默认不许出现载荷行：{text}");

    // show pending 默认无载荷；--payload 才出现。
    let id = pending_id.to_string();
    let out = run(&["review", "show", &id, "--json", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["id"], pending_id);
    assert_eq!(doc["state"], "pending_review");
    assert!(doc.get("payload_json").is_none(), "无 --payload 的结构化输出不带载荷");
    assert!(!stdout_text(&out).contains(PAYLOAD_CANARY));

    let out = run(&["review", "show", &id, "--payload", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    assert!(stdout_text(&out).contains(PAYLOAD_CANARY), "--payload 才打印载荷");
}

/// 3. `decide --reject` 追加一条 `decider=human` 判定并推进到 rejected。
#[test]
fn decide_reject_appends_a_human_decision() {
    let dir = tempfile::tempdir().unwrap();
    let dd = data_dir(dir.path());
    let id;
    {
        let root = open(dir.path());
        let (queued, _sha) = enqueue_item(
            &root,
            "fact",
            "decision",
            &format!("{{\"schema\":\"review.pass.v1\",\"note\":\"{PAYLOAD_CANARY}\"}}"),
        );
        id = queued;
    }
    let id_s = id.to_string();

    let out = run(&["review", "decide", &id_s, "--reject", "--reason", "operator_reject", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    assert!(stdout_text(&out).contains("state=rejected"), "decide 打印新状态：{}", stdout_text(&out));

    let out = run(&["review", "show", &id_s, "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let text = stdout_text(&out);
    assert!(text.contains("decider=human"), "第二条判定是人工：{text}");
    assert!(text.contains("reason_code=operator_reject"), "人工原因码：{text}");

    let out = run(&["review", "list", "--state", "rejected", "--json", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let arr = stdout_json(&out);
    let arr = arr.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], id);
}

/// 4. 对 `applied` item 执行人工改判是非法转换：退出 3，stderr 是一句 review 自己的
///    静态话（H2/C5：不再指向 doctor、不再报成存储故障）。
#[test]
fn decide_on_an_applied_item_is_an_operational_error() {
    let dir = tempfile::tempdir().unwrap();
    let dd = data_dir(dir.path());
    let id;
    {
        let root = open(dir.path());
        let (queued, sha) = enqueue_item(&root, "time", "", "{\"schema\":\"review.pass.v1\"}");
        id = queued;
        for (stage, verdict, reason) in [
            (ReviewStage::Program, ReviewVerdict::Pass, "program_pass"),
            (ReviewStage::Review, ReviewVerdict::Pass, "review_pass"),
            (ReviewStage::Apply, ReviewVerdict::Pass, "apply_pass"),
        ] {
            let decider = match stage {
                ReviewStage::Program | ReviewStage::Apply => Decider::Program,
                _ => Decider::Center,
            };
            record_decision(
                root.memory(),
                id,
                &decision(stage, verdict, reason, decider, &sha),
                CREATED_MS,
            )
            .unwrap();
        }
    }
    let id_s = id.to_string();

    let out = run(&["review", "decide", &id_s, "--approve", "--reason", "operator_approve", "--data-dir", &dd]);
    assert_eq!(code(&out), 3, "非法转换必须退出 3，stderr: {}", stderr_text(&out));
    assert!(out.stdout.is_empty(), "失败面只在 stderr：{}", stdout_text(&out));
    assert_eq!(
        stderr_text(&out),
        "error: review decide: item is not decidable in its current state\n",
        "必须是那一句静态话（零插值），不是存储故障话"
    );
    assert!(
        !stderr_text(&out).contains("doctor"),
        "非法转换不指向 doctor：{}",
        stderr_text(&out)
    );
    assert!(!stderr_text(&out).contains(PAYLOAD_CANARY));

    let out = run(&["review", "show", &id_s, "--json", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    assert_eq!(stdout_json(&out)["state"], "applied", "非法转换不改状态");
}

/// 6. H4b：`show --json` 的 decisions 带两枚指纹列——program 行的送审指纹为空串，
///    review 行是 64 位 hex 的送审指纹；两行的 `input_sha256` 都是载荷指纹。
#[test]
fn show_json_carries_the_payload_and_review_input_fingerprints() {
    let dir = tempfile::tempdir().unwrap();
    let dd = data_dir(dir.path());
    let id;
    let payload_sha;
    let review_input = "ab".repeat(32);
    {
        let root = open(dir.path());
        let (queued, sha) = enqueue_item(&root, "fact", "promise", "{\"schema\":\"review.pass.v1\"}");
        id = queued;
        payload_sha = sha.clone();
        record_decision(
            root.memory(),
            id,
            &decision(ReviewStage::Program, ReviewVerdict::Pass, "program_pass", Decider::Program, &sha),
            CREATED_MS,
        )
        .unwrap();
        let mut review = decision(
            ReviewStage::Review,
            ReviewVerdict::Pass,
            "review_pass",
            Decider::Center,
            &sha,
        );
        review.model = "fake-review-model".into();
        review.prompt_version = "pass-fact-review-v0.4".into();
        review.review_input_sha256 = review_input.clone();
        record_decision(root.memory(), id, &review, CREATED_MS + 1).unwrap();
    }

    let id_s = id.to_string();
    let out = run(&["review", "show", &id_s, "--json", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    let decisions = doc["decisions"].as_array().expect("decisions 必须是数组");
    assert_eq!(decisions.len(), 2, "{doc}");
    assert_eq!(decisions[0]["stage"], "program");
    assert_eq!(decisions[0]["input_sha256"], payload_sha, "program 行是载荷指纹");
    assert_eq!(decisions[0]["review_input_sha256"], "", "program 行没有送审指纹");
    assert_eq!(decisions[1]["stage"], "review");
    assert_eq!(decisions[1]["input_sha256"], payload_sha, "review 行也是载荷指纹");
    assert_eq!(decisions[1]["review_input_sha256"], review_input, "review 行是送审指纹");
    assert_eq!(
        decisions[1]["review_input_sha256"].as_str().unwrap().len(),
        64,
        "送审指纹是 sha256 十六进制"
    );

    // 文本形不打印指纹列（两枚同类指纹只进 JSON，避免一显一隐）。
    let out = run(&["review", "show", &id_s, "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let text = stdout_text(&out);
    assert!(text.contains("stage=review"), "{text}");
    assert!(!text.contains("review_input="), "文本形不打印送审指纹：{text}");
}

/// 5. `stats` 的计数与 `oldest_pending_age_ms`。
#[test]
fn stats_counts_and_oldest_pending_age() {
    let dir = tempfile::tempdir().unwrap();
    let dd = data_dir(dir.path());
    {
        let root = open(dir.path());
        let _ = enqueue_item(&root, "fact", "promise", "{\"schema\":\"review.pass.v1\",\"n\":1}");
        let (id, sha) = enqueue_item(
            &root,
            "time",
            "",
            "{\"schema\":\"review.pass.v1\",\"n\":2}",
        );
        program_reject(&root, id, &sha, "turn_tombstoned");
    }

    let out = run(&["review", "stats", "--json", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["pending_review"], 1, "{doc}");
    assert_eq!(doc["rejected"], 1, "{doc}");
    assert_eq!(doc["approved"], 0);
    assert_eq!(doc["applied"], 0);
    assert_eq!(doc["quarantined"], 0);
    assert_eq!(doc["stale"], 0);
    assert!(doc["oldest_pending_age_ms"].is_number(), "最老 pending 年龄非空：{doc}");
    assert!(doc["oldest_pending_age_ms"].as_i64().unwrap() >= 0);
    let by_pass = doc["by_pass"].as_array().unwrap();
    assert_eq!(by_pass.len(), 2, "两条 pass 各一组：{doc}");
    assert!(by_pass.iter().any(|p| p["pass_id"] == "fact" && p["pending_review"] == 1));
    assert!(by_pass.iter().any(|p| p["pass_id"] == "time" && p["rejected"] == 1));

    let out = run(&["review", "stats", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let text = stdout_text(&out);
    assert!(text.contains("pending_review=1"), "{text}");
    assert!(text.contains("rejected=1"), "{text}");
    assert!(text.contains("oldest_pending_age_ms="), "{text}");
}

// ── H7a：`review prune` 与 7 天告警 ────────────────────────────────────────────

/// 7. prune 清终态载荷、留判定台账：30 天前的 applied item ⇒ 三列置空、state 不变、
///    `review_decision` 行数不变；已经清空的行不重复计。
#[test]
fn prune_clears_terminal_payload_but_keeps_the_decision_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let dd = data_dir(dir.path());
    let id;
    let decisions_before;
    {
        let root = open(dir.path());
        let (queued, sha) = enqueue_item(
            &root,
            "fact",
            "promise",
            &format!("{{\"schema\":\"review.pass.v1\",\"note\":\"{PAYLOAD_CANARY}\"}}"),
        );
        id = queued;
        to_applied(&root, id, &sha);
        backdate(&root, id, "applied", now_ms() - 30 * DAY_MS);
        decisions_before = decision_count(&root, id);
        assert!(decisions_before >= 3, "三闸各一行判定：{decisions_before}");
    }

    let out = run(&[
        "review",
        "prune",
        "--older-than-days",
        "7",
        "--json",
        "--data-dir",
        &dd,
    ]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    assert_eq!(
        stdout_json(&out),
        json!({"pruned": 1, "older_than_days": 7}),
        "窗口原样回显、只清一条"
    );
    assert!(!stdout_text(&out).contains(PAYLOAD_CANARY));

    {
        let root = open(dir.path());
        let (state, payload, turns, owner) = stored(&root, id);
        assert_eq!(state, "applied", "prune 不改状态");
        assert_eq!(payload, "", "载荷被清空");
        assert_eq!(turns, "", "turn_uids 被清空");
        assert_eq!(owner, "", "占用列被清空");
        assert_eq!(
            decision_count(&root, id),
            decisions_before,
            "判定台账一行不动"
        );
    }

    // 第二次：载荷已经空了 ⇒ 不重复计（存储层按"三列非空"圈定）。
    let out = run(&["review", "prune", "--older-than-days", "7", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    assert_eq!(stdout_text(&out).trim(), "pruned=0 older_than_days=7");
}

/// 8. 不够老不动：1 天前的 applied item 在 7 天窗口外 ⇒ `pruned=0`、载荷仍在。
#[test]
fn prune_leaves_terminal_items_inside_the_window_alone() {
    let dir = tempfile::tempdir().unwrap();
    let dd = data_dir(dir.path());
    let id;
    {
        let root = open(dir.path());
        let (queued, sha) = enqueue_item(
            &root,
            "fact",
            "decision",
            &format!("{{\"schema\":\"review.pass.v1\",\"note\":\"{PAYLOAD_CANARY}\"}}"),
        );
        id = queued;
        to_applied(&root, id, &sha);
        backdate(&root, id, "applied", now_ms() - DAY_MS);
    }

    let out = run(&["review", "prune", "--older-than-days", "7", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    assert_eq!(stdout_text(&out).trim(), "pruned=0 older_than_days=7");

    let root = open(dir.path());
    let (state, payload, turns, owner) = stored(&root, id);
    assert_eq!(state, "applied");
    assert!(payload.contains(PAYLOAD_CANARY), "窗口内的载荷不许动：{payload}");
    assert!(!turns.is_empty(), "turn_uids 不许动：{turns}");
    assert!(!owner.is_empty(), "占用列不许动：{owner}");
}

/// 9. 非终态不动：pending 项即使 `updated_at_ms` 很旧，也不在 prune 的圈里。
#[test]
fn prune_leaves_non_terminal_items_alone() {
    let dir = tempfile::tempdir().unwrap();
    let dd = data_dir(dir.path());
    let id;
    {
        let root = open(dir.path());
        let (queued, _sha) = enqueue_item(
            &root,
            "fact",
            "plan",
            &format!("{{\"schema\":\"review.pass.v1\",\"note\":\"{PAYLOAD_CANARY}\"}}"),
        );
        id = queued;
        backdate(&root, id, "pending_review", now_ms() - 30 * DAY_MS);
    }

    let out = run(&["review", "prune", "--older-than-days", "7", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    assert_eq!(stdout_text(&out).trim(), "pruned=0 older_than_days=7");

    let root = open(dir.path());
    let (state, payload, turns, owner) = stored(&root, id);
    assert_eq!(state, "pending_review", "非终态不改状态");
    assert!(payload.contains(PAYLOAD_CANARY), "pending 的载荷要留给审：{payload}");
    assert!(!turns.is_empty(), "turn_uids 不许动：{turns}");
    assert!(!owner.is_empty(), "占用列不许动：{owner}");
}

/// 10. `--older-than-days 0` 是用法错误：退出 3、stdout 空、纯静态话、**不建库**。
///     0 天等于"清掉全部终态载荷"，不是清理——挡在开库之前。
#[test]
fn zero_days_is_a_usage_error_and_opens_no_store() {
    let dir = tempfile::tempdir().unwrap();
    let dd = dir.path().join(DATA);
    let out = run(&[
        "review",
        "prune",
        "--older-than-days",
        "0",
        "--data-dir",
        dd.to_str().unwrap(),
    ]);
    assert_eq!(code(&out), 3, "stderr: {}", stderr_text(&out));
    assert!(out.stdout.is_empty(), "用法错误不打读数：{}", stdout_text(&out));
    assert_eq!(
        stderr_text(&out),
        "error: review prune --older-than-days must be greater than zero\n",
        "必须是那一句静态话（零插值）"
    );
    assert!(!dd.exists(), "用法错误不建库");
}

/// 11. `stats` 的 7 天告警：8 天前入队的 pending ⇒ true（文本与 JSON）；
///     刚入队的新项 ⇒ false。
#[test]
fn stats_flags_a_pending_item_older_than_seven_days() {
    // 老项：8 天前入队。
    let old_dir = tempfile::tempdir().unwrap();
    let old_dd = data_dir(old_dir.path());
    {
        let root = open(old_dir.path());
        let payload = "{\"schema\":\"review.pass.v1\",\"n\":1}";
        let out = enqueue(
            root.memory(),
            &item("fact", "promise", payload),
            now_ms() - 8 * DAY_MS,
        )
        .unwrap();
        assert!(out.id > 0);
    }

    let out = run(&["review", "stats", "--json", "--data-dir", &old_dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["pending_review"], 1, "{doc}");
    assert!(
        doc["oldest_pending_age_ms"].as_i64().unwrap() >= 8 * DAY_MS,
        "最老 pending 至少 8 天：{doc}"
    );
    assert_eq!(doc["oldest_pending_over_7d"], true, "超过 7 天必须告警：{doc}");

    let out = run(&["review", "stats", "--data-dir", &old_dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let text = stdout_text(&out);
    assert!(text.contains("oldest_pending_over_7d=true"), "文本 counts 行要带告警：{text}");
    assert!(text.contains("oldest_pending_age_ms="), "{text}");

    // 新项：刚入队 ⇒ 不告警。
    let new_dir = tempfile::tempdir().unwrap();
    let new_dd = data_dir(new_dir.path());
    {
        let root = open(new_dir.path());
        let payload = "{\"schema\":\"review.pass.v1\",\"n\":2}";
        enqueue(root.memory(), &item("fact", "promise", payload), now_ms()).unwrap();
    }

    let out = run(&["review", "stats", "--json", "--data-dir", &new_dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["pending_review"], 1, "{doc}");
    assert_eq!(doc["oldest_pending_over_7d"], false, "新项不告警：{doc}");

    let out = run(&["review", "stats", "--data-dir", &new_dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let text = stdout_text(&out);
    assert!(text.contains("oldest_pending_over_7d=false"), "{text}");
}

// ── H7c2：decide 写审查轴水位 ────────────────────────────────────────────────
//
// 两个 pass 两条 item：第一次 `decide` 只写该 item 的 pass 那一行；第二次换 pass 再加
// 一行，且 `max_decision_id` 跟着判定台账前进（旧行保留它写那一刻的值）。

#[test]
fn decide_writes_the_items_pass_review_watermark() {
    let dir = tempfile::tempdir().unwrap();
    let dd = data_dir(dir.path());
    let (fact, event);
    {
        let root = open(dir.path());
        fact = enqueue_item(
            &root,
            "fact",
            "decision",
            "{\"schema\":\"review.pass.v1\",\"n\":1}",
        )
        .0;
        event = enqueue_item(&root, "event", "", "{\"schema\":\"review.pass.v1\",\"n\":2}").0;
        assert!(
            rc_ledger::review_mark::all(root.ledger()).unwrap().is_empty(),
            "入队本身不写水位（没有判定）"
        );
    }

    let out = run(&[
        "review", "decide", &fact.to_string(), "--reject", "--reason", "operator_reject",
        "--data-dir", &dd,
    ]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));

    let root = open(dir.path());
    let marks = rc_ledger::review_mark::all(root.ledger()).unwrap();
    assert_eq!(marks.len(), 1, "只有被 decide 的那个 pass 一行：{marks:?}");
    assert_eq!(marks[0].pass_id, "fact");
    let after_first = rc_memory::review::max_decision_id(root.memory()).unwrap();
    assert!(after_first > 0, "人工判定已落，最大 id 必须是正数");
    assert_eq!(marks[0].max_decision_id, after_first, "水位记的是判定台账的最大 id");
    assert_eq!(marks[0].pending_review, 0, "fact 那一行只数 fact 的 pending（这条已 rejected）");
    assert_eq!(marks[0].applied, 0);
    drop(root);

    // 第二条换个 pass：event 行出现，最大 id 前进；fact 行保留它写那一刻的旧值。
    let out = run(&[
        "review", "decide", &event.to_string(), "--reject", "--reason", "operator_reject",
        "--data-dir", &dd,
    ]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));

    let root = open(dir.path());
    let marks = rc_ledger::review_mark::all(root.ledger()).unwrap();
    assert_eq!(
        marks.iter().map(|m| m.pass_id.as_str()).collect::<Vec<_>>(),
        vec!["event", "fact"],
        "按 pass_id 升序"
    );
    let after_second = rc_memory::review::max_decision_id(root.memory()).unwrap();
    assert!(after_second > after_first, "第二条例判定必须让水位前进");
    assert_eq!(marks[0].max_decision_id, after_second, "event 行是新的");
    assert_eq!(marks[1].max_decision_id, after_first, "fact 行没被重写");
}
