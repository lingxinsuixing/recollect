// crates/hub/tests/dry_run_cli.rs
//! DR：两条**只打印、不调模型、不写状态**的出口（工单 `2026-10-28-dry-run`）。
//!
//! - `hub distill once --pass naming|fact|event --dry-run`：真实选批 + 同一组
//!   `*_system()`/`*_user(...)`，把 `SYSTEM`/`USER` 打到 stdout；
//! - `hub review show <ID> --prompt`：复用 `review_plan` 重建审核输入（含分片与程序闸
//!   skip），打印 `SYSTEM` 与每个 `CHUNK i`。
//!
//! 两条出口的用途只有一个：让"扮演模型"的审计读到**逐字原文**，而不是转述。所以这里的
//! 断言分两类——
//! 1. 原文真的到了 stdout（canary/正文/候选/`ITEM f1`）；
//! 2. **状态真的没动**：不是"没报错"，是 `extract_pass_state` / `review_item` /
//!    `review_decision` 的行数（与 item 状态）跑前跑后一致。
//!
//! 纪律与其余 CLI 用例同源：不跑真实 LLM（这两条出口本来就不开模型口）、夹具走
//! `parse`→`normalize`→`insert_batch` 真链路。注意这里**不**套守卫 12 的"零正文"断言：
//! 这两条出口的产出就是正文，那是它们的职责；零正文规则管的是 `finish_*` 的运维读数面。
use rc_contract::{normalize, parse};
use rc_ledger::insert_batch;
use rc_ledger::pass_state::{mark_terminal, PassKey, PassState};
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{AnchorRole, Provenance, Scope, ScopeKind};
use rc_store::Root;
use rusqlite::params;
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Output};

const DEV: &str = "dev-dry-run";
const SESS: &str = "sess-dry-run";
const DATA: &str = "data";
const NOW: i64 = 1_700_000_000_000;
/// 第一轮正文：它必须原样出现在 `--dry-run` 的 USER 里。
const BODY_1: &str = "张三下周末搬家，老张也来帮忙";
/// 第二轮正文。
const BODY_2: &str = "记下了：张三搬家的时间是下周末";

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

fn stderr_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn data_dir(dir: &Path) -> String {
    dir.join(DATA).to_str().unwrap().to_string()
}

fn open(dir: &Path) -> Root {
    Root::open(&dir.join(DATA)).unwrap()
}

/// 一行计数（库与表名都是本文件自己写的字面量，不是来料）。
fn count(root: &Root, db: &str, table: &str) -> i64 {
    let conn = if db == "ledger" {
        root.ledger().conn()
    } else {
        root.memory().conn()
    };
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// 播种两轮真转录（`parse`→`normalize`→`insert_batch`，与其余 CLI 用例同一条链），
/// 归属改成非默认值（事实侧的闸要按会话 owner 判锚的可见性）。
fn seed(dir: &Path) -> Vec<String> {
    let doc = json!({
        "format_version": 1,
        "sessions": [{
            "session_key": SESS,
            "device_id": DEV,
            "source_agent": "agent-dry-run",
            "turns": [
                {"role": "human", "body": BODY_1, "created_at_ms": NOW},
                {"role": "agent", "body": BODY_2, "created_at_ms": NOW + 1}
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

/// 铺两个活锚（`a1`=张三 较新、`a2`=李四 较早），并把锚的归属改挂会话 owner——
/// 事实侧的候选池/程序闸都按它判可见性。返回锚 id（`[a1, a2]`）。
fn seed_anchors(dir: &Path) -> Vec<String> {
    let root = Root::open(&dir.join(DATA)).unwrap();
    let uid: String = root
        .transcript()
        .conn()
        .query_row("SELECT turn_uid FROM turns ORDER BY created_at_ms LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    let mut ids = Vec::new();
    for (canonical, created_at_ms) in [("李四", NOW - 2_000), ("张三", NOW - 1_000)] {
        let key = put_entity(
            root.memory(),
            &EntityContent {
                kind: "person".into(),
                title: canonical.into(),
                scope: scope.clone(),
                aliases: vec![],
            },
            &[Provenance { turn_uid: uid.clone(), anchor_role: AnchorRole::Human }],
            created_at_ms,
        )
        .unwrap();
        ids.push(key.id);
    }
    let changed = root
        .memory()
        .conn()
        .execute(
            "UPDATE resource_owner SET owner_agent_id='alice', owner_principal_id='p1',
                    visibility='project', scope_kind='project', scope_key='proj-x'
              WHERE mem_table='anchor'",
            [],
        )
        .unwrap();
    assert!(changed >= 2, "两个锚必须真的改到归属：{changed}");
    // 候选池按 `created_at` 新→旧排：a1=张三、a2=李四（与 review_run_cli 的夹具同序）。
    ids.reverse();
    ids
}

/// 一条 fact 草稿（promise 族）：`turn=t1`、主语 `a1`、引文逐字。
fn fact_draft(reference: &str, value: &str, quote: &str) -> Value {
    json!({
        "ref": reference, "turn": "t1", "subject": "a1", "predicate": "promise",
        "object": null, "value": value, "category": "promise",
        "mode": "stated", "time_text": "下周末", "quote": quote,
        "evidence": ["t1"], "text": "张三承诺在期限之前处理"
    })
}

/// 一条 fact pass（promise 族）的抽取输出：`facts` 逐条给，覆盖缺口用 `empty_turns`。
fn extraction(facts: Value, empty: &[&str]) -> Value {
    json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": facts,
        "empty_turns": empty
    })
}

fn reply(text: &Value) -> Value {
    Value::String(text.to_string())
}

/// `distill once --pass fact --family promise --no-review --replies <file>`：
/// 只抽取并入队，item 停在 `pending_review`（审核输入还没来得及送出去）。
fn enqueue_fact(dir: &Path, facts: Value, empty: &[&str]) -> Output {
    let p = dir.join("dry-run-extraction.json");
    std::fs::write(&p, Value::Array(vec![reply(&extraction(facts, empty))]).to_string()).unwrap();
    let dd = data_dir(dir);
    run(&[
        "distill",
        "once",
        "--pass",
        "fact",
        "--family",
        "promise",
        "--no-review",
        "--replies",
        p.to_str().unwrap(),
        "--data-dir",
        &dd,
    ])
}

fn only_item(root: &Root) -> (i64, String) {
    root.memory()
        .conn()
        .query_row("SELECT id, state FROM review_item", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap()
}

/// `review show <id> --prompt`。
fn review_prompt(dir: &Path, id: i64) -> Output {
    let dd = data_dir(dir);
    run(&["review", "show", &id.to_string(), "--prompt", "--data-dir", &dd])
}

// ── 1. 抽取：打印原文、一行不写 ───────────────────────────────────────────────

/// 2 轮 + 1 实体夹具 ⇒ `distill once --pass naming --dry-run` 打印 `SYSTEM`/`USER`，
/// USER 里逐字带着两轮正文与实体候选；ledger `extract_pass_state` 与 memory
/// `review_item`/`review_decision` 的行数与跑前一致（不是"没报错"）。
#[test]
fn extract_dry_run_prints_system_and_user_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let _uids = seed(dir.path());
    let _anchors = seed_anchors(dir.path());

    let root = open(dir.path());
    let pass_state_before = count(&root, "ledger", "extract_pass_state");
    let items_before = count(&root, "memory", "review_item");
    let decisions_before = count(&root, "memory", "review_decision");
    drop(root);

    let dd = data_dir(dir.path());
    let out = run(&["distill", "once", "--pass", "naming", "--dry-run", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let text = stdout_text(&out);
    assert!(text.contains("=== PASS ===\nnaming\n"), "PASS 那一格：{text}");
    assert!(text.contains("=== SYSTEM ==="), "缺段头：{text}");
    assert!(text.contains("=== USER ==="), "缺段头：{text}");
    assert!(text.contains("=== END ==="), "缺收尾：{text}");
    assert!(text.contains(BODY_1), "第一轮正文必须在 USER 里：{text}");
    assert!(text.contains(BODY_2), "第二轮正文必须在 USER 里：{text}");
    // 1 个实体夹具：候选段落里逐字出现那个名字（A1 的候选只给 kind/canonical，不给 id）。
    assert!(text.contains("EXISTING CANDIDATES"), "缺候选段：{text}");
    assert!(text.contains("- person | 李四"), "实体候选必须进 USER：{text}");
    assert!(!text.contains("=== NO BATCH ==="), "有可选批次，不许报空批：{text}");

    // 不写状态：三张表行数一致（选批只读；这批轮没有压缩摘要块，选批也不落标记）。
    let root = open(dir.path());
    assert_eq!(
        count(&root, "ledger", "extract_pass_state"),
        pass_state_before,
        "dry-run 不许写 extract_pass_state"
    );
    assert_eq!(
        count(&root, "memory", "review_item"),
        items_before,
        "dry-run 不许入队"
    );
    assert_eq!(
        count(&root, "memory", "review_decision"),
        decisions_before,
        "dry-run 不许记判定"
    );
}

/// 两轮都标终态 ⇒ 没有可选批次：stdout **只有** `=== NO BATCH ===`，退出 0。
#[test]
fn extract_dry_run_on_an_exhausted_batch_says_no_batch() {
    let dir = tempfile::tempdir().unwrap();
    let uids = seed(dir.path());

    let root = open(dir.path());
    for uid in &uids {
        mark_terminal(
            root.ledger(),
            PassKey { turn_uid: uid.as_str(), pass_id: "naming", family: "" },
            PassState::Done,
            None,
            NOW,
        )
        .unwrap();
    }
    let before = count(&root, "ledger", "extract_pass_state");
    drop(root);

    let dd = data_dir(dir.path());
    let out = run(&["distill", "once", "--pass", "naming", "--dry-run", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    assert_eq!(stdout_text(&out), "=== NO BATCH ===\n");
    assert!(out.stderr.is_empty(), "stderr: {}", stderr_text(&out));

    let root = open(dir.path());
    assert_eq!(
        count(&root, "ledger", "extract_pass_state"),
        before,
        "空批也不许动状态行"
    );
}

/// `--dry-run` 与座位来源同给是用法错误：退出 3，stdout 一个字都不许有
/// （否则脚本会把一份"没跑"读成"跑过了"）。
#[test]
fn extract_dry_run_refuses_a_model_source() {
    let dir = tempfile::tempdir().unwrap();
    let _uids = seed(dir.path());
    let p = dir.path().join("replies.json");
    std::fs::write(&p, Value::Array(vec![reply(&json!({}))]).to_string()).unwrap();

    let dd = data_dir(dir.path());
    let out = run(&[
        "distill",
        "once",
        "--pass",
        "naming",
        "--dry-run",
        "--replies",
        p.to_str().unwrap(),
        "--data-dir",
        &dd,
    ]);
    assert_eq!(code(&out), 3, "stderr: {}", stderr_text(&out));
    assert!(out.stdout.is_empty(), "用法错误不打原文：{}", stdout_text(&out));
    assert!(stderr_text(&out).contains("dry-run"), "stderr: {}", stderr_text(&out));
}

// ── 2. 审核：复用 review_plan，编号与线上一致 ─────────────────────────────────

/// 一条 2 草稿的 fact item ⇒ `review show ID --prompt` 打印 `SYSTEM` + `CHUNK 1`，
/// 片内两条 `ITEM f1`/`ITEM f2`；**不写任何表**（`review_decision` 行数与 item 状态不变）。
#[test]
fn review_prompt_reuses_the_plan_and_numbers_like_the_runner() {
    let dir = tempfile::tempdir().unwrap();
    let _uids = seed(dir.path());
    let _anchors = seed_anchors(dir.path());

    // 两条草稿的引文都逐字落在第一轮正文里（BODY_1），时间短语也接地。
    let facts = json!([
        fact_draft("f1", "下周末搬家", "张三下周末搬家"),
        fact_draft("f2", "来帮忙", "老张也来帮忙")
    ]);
    let out = enqueue_fact(dir.path(), facts, &["t2"]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));

    let root = open(dir.path());
    let (id, state) = only_item(&root);
    assert_eq!(state, "pending_review");
    let decisions_before = count(&root, "memory", "review_decision");
    let items_before = count(&root, "memory", "review_item");
    drop(root);

    let out = review_prompt(dir.path(), id);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let text = stdout_text(&out);
    assert!(text.contains("=== PASS ===\nfact:promise\n"), "PASS 那一格：{text}");
    assert!(text.contains(&format!("=== ITEM ===\n{id}\n")), "ITEM 那一格：{text}");
    assert!(text.contains("=== SYSTEM ==="), "缺段头：{text}");
    assert!(text.contains("=== CHUNK 1 ==="), "单片也必须有分片段头：{text}");
    assert!(!text.contains("=== CHUNK 2 ==="), "两条草稿只该有一片：{text}");
    assert!(text.contains("ITEM f1\n"), "缺 f1：{text}");
    assert!(text.contains("ITEM f2\n"), "缺 f2：{text}");
    assert!(text.contains("value: 下周末搬家"), "f1 的正文：{text}");
    assert!(text.contains("value: 来帮忙"), "f2 的正文：{text}");

    // 不写任何表：判定台账与 item 状态都不动（gate/replay/review_plan 全是只读）。
    let root = open(dir.path());
    assert_eq!(
        count(&root, "memory", "review_decision"),
        decisions_before,
        "--prompt 不许记判定"
    );
    assert_eq!(
        count(&root, "memory", "review_item"),
        items_before,
        "--prompt 不许改队列行"
    );
    assert_eq!(only_item(&root), (id, "pending_review".to_string()), "item 状态不动");
}

/// 一条被程序闸拒收的草稿（引文不在正文里）⇒ 它的 ref 与正文都不进 `--prompt`；
/// 剩下的两条按**保留顺序**重新编号成 `f1`/`f2`（与 runner 的 `skip` 后编号同一套）。
#[test]
fn review_prompt_skips_program_rejected_drafts() {
    let dir = tempfile::tempdir().unwrap();
    let _uids = seed(dir.path());
    let _anchors = seed_anchors(dir.path());

    let facts = json!([
        fact_draft("f1", "下周末搬家", "张三下周末搬家"),
        // 这一条的引文不在任何证据轮正文里 ⇒ 程序闸判 `quote_not_verbatim`。
        fact_draft("f2", "把合同寄给王五", "这句正文里没有"),
        fact_draft("f3", "来帮忙", "老张也来帮忙")
    ]);
    let out = enqueue_fact(dir.path(), facts, &["t2"]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));

    let root = open(dir.path());
    let (id, state) = only_item(&root);
    assert_eq!(state, "pending_review", "混合批不能被整批拒收");
    drop(root);

    let out = review_prompt(dir.path(), id);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let text = stdout_text(&out);
    assert!(!text.contains("把合同寄给王五"), "被闸拒的草稿正文不许进 prompt：{text}");
    assert!(!text.contains("这句正文里没有"), "被闸拒的草稿引文不许进 prompt：{text}");
    assert!(!text.contains("ITEM f3\n"), "编号必须压缩到 f1..f2：{text}");

    // 保留的两条按原顺序占 f1/f2：编号与 runner 的 skip 后位置编号同一套。
    let after_first = text.split("ITEM f1\n").nth(1).expect("缺 f1");
    let first_block = after_first.split("ITEM f2\n").next().unwrap();
    assert!(first_block.contains("value: 下周末搬家"), "f1 应是第一条好草稿");
    let second_block = text.split("ITEM f2\n").nth(1).expect("缺 f2");
    assert!(second_block.contains("value: 来帮忙"), "f2 应是第三条草稿");
}
