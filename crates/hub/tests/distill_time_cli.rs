// crates/hub/tests/distill_time_cli.rs
//! S2d-2：`hub distill once --pass time` 的黑盒进程面。照 `distill_event_cli.rs` 的写法：
//! CLI 是这条 pass 的唯一出口，装配漏了一格（来源、选批、回填、退出码）只有子进程看得见。
//!
//! 纪律：
//! - **不跑真实 LLM**：time pass 是纯程序，**一个 LLM 来源都不给**；给了也不读（用例 4）；
//! - stdout 只许一个 JSON（守卫 12）：用例同时断正文与 data-dir 路径不出现；
//! - time 是单一 pass（空串 family），`--family` 是用法错误（3，且不开库）。
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_fact::{put_fact_rows, FactWriteItem};
use rc_memory::{AnchorRole, BatchCtx, Scope, ScopeKind};
use rc_store::Root;
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

const DATA: &str = "data";
/// 2026-10-26T14:05:00+08:00：`下周三之前` 的参照钟（也是 `time_expr.created_at_ms`）。
const TURN: i64 = 1_792_994_700_000;
const UID: &str = "0000000000000000000000000000000000000000000000000000000000000031";
const ANCHOR: &str = "00000000000000000000000000000000000000000000000000000000000000b1";
/// 夹具的会话坐标：与 `put_fact_rows` 的 `BatchCtx`（source_agent/device）和归属一致。
/// S3a-3c 起 time pass 用 origin 分组并过程序闸，转录里必须有这轮的会话/设备/正文。
const SESSION: &str = "sess-time";
const DEVICE: &str = "dev-time";
const BODY: &str = "我下周三之前把对账单发给你";
/// fact 正文：断它不出现在 stdout/stderr（守卫 12 的零正文）。
const CANARY: &str = "轮一：下周三之前发对账单";

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

fn stderr_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn data_dir(dir: &Path) -> String {
    dir.join(DATA).to_str().unwrap().to_string()
}

fn open(dir: &Path) -> Root {
    Root::open(&dir.join(DATA)).unwrap()
}

/// 播种一条带 `time_text` 的 fact（写库层同时落一条未解析的 `time_expr`），返回数据目录。
/// 同时为夹具里的 `UID` 播转录（session/device/turn）：S3a-3c 起 time pass 按 origin 分组并过
/// 程序闸（`turn_missing` / `turn_session_mismatch` / `turn_tombstoned`），只播记忆库会让每一组
/// 落在 `("","")` 上被闸门拒掉。
fn seed(dir: &Path) -> String {
    let root = Root::open(&dir.join(DATA)).unwrap();
    let m = root.memory();
    m.conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind,
                                scope_key, status, profile_text, created_at_ms, updated_at_ms,
                                norm_version)
             VALUES(?1,'person','张三','unknown',NULL,'project','proj-time','active',NULL,?2,?2,?3)",
            rusqlite::params![ANCHOR, TURN, rc_store::capability::NORM_VERSION as i64],
        )
        .unwrap();
    // 会话归属与 `seed` 里的 `OwnerCtx` / `BatchCtx` 同一组值：time pass 的归属读法从
    // `sessions` 推 owner/scope，推不出会落 `unknown`——夹具不该让正常路径从哨兵开始。
    root.transcript()
        .conn()
        .execute(
            "INSERT INTO sessions(session_key, device_id, source_agent, machine, project_hint,
                                  last_saved_at_ms, owner_principal_id, owner_agent_id,
                                  scope_kind, scope_key, visibility, retention, ownership_src)
             VALUES(?1,?2,'agent-time',NULL,'proj-time',?3,'p-time','agent-owner',
                    'project','proj-time','project','keep','operator')",
            rusqlite::params![SESSION, DEVICE, TURN],
        )
        .unwrap();
    // 正文是 fact 的逐字引文：time 不消费它，但闸门的压缩块判定会读被引轮。
    root.transcript()
        .conn()
        .execute(
            "INSERT INTO turns(turn_uid, session_key, device_id, role, body, body_fp,
                                turn_key_tier, project_hint, created_at_ms, ingested_at_ms,
                                commit_seq)
             VALUES(?1,?2,?3,'human',?4,'00',1,'proj-time',?5,?5,0)",
            rusqlite::params![UID, SESSION, DEVICE, BODY, TURN],
        )
        .unwrap();
    let it = FactWriteItem {
        subject_anchor_id: ANCHOR.into(),
        object_anchor_id: None,
        predicate: "承诺".into(),
        value: Some("把对账单发出去".into()),
        category: "promise".into(),
        mode: "stated".into(),
        time_text: "下周三之前".into(),
        quote: "我下周三之前把对账单发给你".into(),
        evidence: vec![(UID.into(), AnchorRole::Human)],
        text: CANARY.into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-time".into() },
        source_ms: TURN,
    };
    let ctx = BatchCtx { source_agent: "agent-time".into(), device_id: "dev-time".into() };
    let owner = OwnerCtx {
        owner_principal_id: Some("p-time".into()),
        owner_agent_id: "agent-owner".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    };
    put_fact_rows(m, &[it], &ctx, &owner, TURN).unwrap();
    data_dir(dir)
}

/// `distill once --pass time --data-dir <dd>`（可选地在前面塞一个旗标对）。
fn distill(dd: &str, extra: &[&str]) -> Output {
    let mut args: Vec<&str> = vec!["distill", "once", "--pass", "time"];
    args.extend_from_slice(extra);
    args.push("--data-dir");
    args.push(dd);
    run(&args)
}

/// 守卫 12：stdout 只有读数，没有正文、没有路径。
fn assert_no_body_or_path(out: &Output) {
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(!text.contains(CANARY), "stdout 带正文：{text}");
    assert!(!text.contains(DATA), "stdout 带路径：{text}");
    assert!(!stderr_text(out).contains(CANARY), "stderr 带正文");
}

/// 1. 不给任何 LLM 来源：exit 0、单 JSON（七格）、fact 的 due 被填、pass state done。
#[test]
fn a_time_pass_backfills_without_any_llm_source() {
    let dir = tempfile::tempdir().unwrap();
    let dd = seed(dir.path());
    let out = distill(&dd, &[]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));

    let doc = stdout_json(&out);
    assert_eq!(doc["schema_version"], 2, "W33-A10: 运维面加了观测两格");
    assert_eq!(doc["pass"], "time");
    assert!(doc.get("family").is_none(), "time 的读数没有 family 格");
    assert_eq!(doc["turns"], 1);
    assert_eq!(doc["expressions"], 1);
    assert_eq!(doc["resolved"], 1);
    assert_eq!(doc["facts"], 1);
    assert_eq!(doc["state"], "done");
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    assert_eq!(
        root.memory()
            .conn()
            .query_row("SELECT due_at_ms FROM fact WHERE text=?1", [CANARY], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .unwrap(),
        Some(1_793_807_999_999),
        "deadline 回填 due_at_ms = 2026-11-04 当日末",
    );
    let (precision, basis, kind): (String, String, String) = root
        .memory()
        .conn()
        .query_row(
            "SELECT precision, basis, kind FROM time_expr WHERE turn_uid=?1 AND text='下周三之前'",
            [UID],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((precision.as_str(), basis.as_str(), kind.as_str()),
               ("day", "inferred", "deadline"));
    let (state, reason): (String, Option<String>) = root
        .ledger()
        .conn()
        .query_row(
            "SELECT state, reason FROM extract_pass_state
             WHERE turn_uid=?1 AND pass_id='time' AND family=''",
            [UID],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((state.as_str(), reason), ("done", None));
}

/// 2. 再跑一次：`turns=0`、`state="empty"`、退出 0、库里的时间列不动。
#[test]
fn a_second_run_is_an_empty_batch() {
    let dir = tempfile::tempdir().unwrap();
    let dd = seed(dir.path());
    let first = distill(&dd, &[]);
    assert_eq!(code(&first), 0, "stderr: {}", stderr_text(&first));

    let again = distill(&dd, &[]);
    assert_eq!(code(&again), 0, "stderr: {}", stderr_text(&again));
    let doc = stdout_json(&again);
    assert_eq!(doc["turns"], 0);
    assert_eq!(doc["expressions"], 0);
    assert_eq!(doc["resolved"], 0);
    assert_eq!(doc["facts"], 0);
    assert_eq!(doc["state"], "empty");

    let root = open(dir.path());
    assert_eq!(
        root.memory()
            .conn()
            .query_row("SELECT due_at_ms FROM fact WHERE text=?1", [CANARY], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .unwrap(),
        Some(1_793_807_999_999),
        "空批不许覆盖已填的列",
    );
}

/// 3. `--pass time --family <f>` 是用法错误：exit 3、stderr 点名 time 与 --family、不开库。
#[test]
fn the_family_flag_is_a_usage_error_for_time() {
    let dir = tempfile::tempdir().unwrap();
    let dd = data_dir(dir.path());

    for family in ["promise", "decision", "meeting"] {
        let out =
            run(&["distill", "once", "--pass", "time", "--family", family, "--data-dir", &dd]);
        assert_eq!(code(&out), 3, "stderr: {}", stderr_text(&out));
        assert!(out.stdout.is_empty(), "配置故障不开库也不打读数");
        let err = stderr_text(&out);
        assert!(err.contains("time"), "stderr: {err}");
        assert!(err.contains("--family"), "stderr: {err}");
        assert!(!err.contains(family), "stderr 不回显未知值：{err}");
    }
    assert!(!dir.path().join(DATA).exists(), "用法错误不建库");
}

/// 4. 给了 `--replies` 也不读：路径不存在也 exit 0，读数照打（time 没有 LLM 契约）。
#[test]
fn llm_sources_are_accepted_and_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let dd = seed(dir.path());
    let missing = dir.path().join("no-such-replies.json");
    let out = distill(&dd, &["--replies", missing.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "不给 LLM 场景不读来源，stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["pass"], "time");
    assert_eq!(doc["turns"], 1);
    assert_eq!(doc["state"], "done");
}

/// 5. 真进程 E2E（验收 §2 "未过审不入库"）：同一夹具先插 session 作用域 `tombstone`，
///    再跑 `distill once --pass time`。按现有 CLI 合约这一批是 `state=rejected`、退出 0；
///    逐格读原始读数：resolved/facts 都是 0，`time_expr.start_ms` 与 `fact.due_at_ms` 仍是
///    NULL，队列里留下一条 `rejected` item（`program_code=turn_tombstoned`）。
#[test]
fn a_tombstoned_session_never_writes_time_rows() {
    let dir = tempfile::tempdir().unwrap();
    let dd = seed(dir.path());
    {
        let root = open(dir.path());
        let hash = rc_ledger::transcript::tombstone_sha256("session", SESSION, 1);
        root.transcript()
            .conn()
            .execute(
                "INSERT INTO tombstone(commit_seq, epoch, scope_kind, scope_key, turns_removed,
                                       content_sha256, created_at_ms)
                 VALUES(1,1,'session',?1,1,?2,?3)",
                rusqlite::params![SESSION, hash, TURN],
            )
            .unwrap();
    }
    let out = distill(&dd, &[]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let doc = stdout_json(&out);
    assert_eq!(doc["turns"], 1);
    assert_eq!(doc["expressions"], 1);
    assert_eq!(doc["resolved"], 0);
    assert_eq!(doc["facts"], 0);
    assert_eq!(doc["state"], "rejected");
    assert_no_body_or_path(&out);

    let root = open(dir.path());
    // 未过审不入库：解析列与 fact 的 due 都还是 NULL。
    let start: Option<i64> = root
        .memory()
        .conn()
        .query_row(
            "SELECT start_ms FROM time_expr WHERE turn_uid=?1 AND text='下周三之前'",
            [UID],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(start, None, "闸门拒绝后不许回填 time_expr.start_ms");
    let due: Option<i64> = root
        .memory()
        .conn()
        .query_row("SELECT due_at_ms FROM fact WHERE text=?1", [CANARY], |r| r.get(0))
        .unwrap();
    assert_eq!(due, None, "闸门拒绝后不许回填 fact.due_at_ms");

    let list = run(&["review", "list", "--json", "--data-dir", &dd]);
    assert_eq!(code(&list), 0, "stderr: {}", stderr_text(&list));
    let items = stdout_json(&list);
    let arr = items.as_array().expect("review list --json 是数组");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["pass_id"], "time");
    assert_eq!(arr[0]["state"], "rejected");
    assert_eq!(arr[0]["program_code"], "turn_tombstoned");
}

// ── H7c2：time runner 收尾写审查轴水位 ────────────────────────────────────────

/// H7c2：`distill once --pass time` 收尾在 ledger 留下一行 `pass_id='time'` 的水位——
/// `max_decision_id` 是本地判定台账的最大 id，三态与 `stats` 的 by_pass 同一格。
/// 这条钉的是"第三个写点"（引擎、`review decide` 之外的那一个）。
#[test]
fn a_time_run_writes_the_time_review_watermark() {
    let dir = tempfile::tempdir().unwrap();
    let dd = seed(dir.path());
    {
        let root = open(dir.path());
        assert!(
            rc_ledger::review_mark::all(root.ledger()).unwrap().is_empty(),
            "跑之前不该有水位行"
        );
    }

    let out = distill(&dd, &[]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));

    let root = open(dir.path());
    let marks = rc_ledger::review_mark::all(root.ledger()).unwrap();
    assert_eq!(marks.len(), 1, "只应有一条 time 水位：{marks:?}");
    assert_eq!(marks[0].pass_id, "time");
    let max = rc_memory::review::max_decision_id(root.memory()).unwrap();
    assert!(max > 0, "time 的程序闸判定已落，最大 id 必须是正数");
    assert_eq!(marks[0].max_decision_id, max, "水位记的是判定台账的最大 id");

    let stats = rc_memory::review::stats(root.memory(), TURN + 10_000).unwrap();
    let time = stats
        .by_pass
        .iter()
        .find(|(pass, _)| pass == "time")
        .expect("time 的 by_pass 行不见了")
        .1;
    assert_eq!(
        (marks[0].pending_review, marks[0].quarantined, marks[0].applied),
        (time[0], time[4], time[2]),
        "三态必须与 stats 当前值一致"
    );
    assert_eq!(marks[0].applied, 1, "夹具那一批已 apply");
}
