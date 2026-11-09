//! 推导契约丢弃的隔离/重跑（工单 `2026-10-28-derive-quarantine`）。
//!
//! 这组用例钉住的是**推导线不再 live-lock**：同一个坏批在前两轮不推游标、attempts 1/2；
//! 第三轮到上限 ⇒ 游标推到本批末尾 + 落一条可重跑的 issue；下一轮选下一批；按 issue 回拨
//! 之后原批会被重新抽。`ZeroOutput` 与 LLM 故障两族各有反面对照（一个推、一个不推）。
//!
//! 这一层是库面用例：`derive retry` 的 CLI 装配在 `crates/hub/tests/derive_cli.rs`，
//! 这里走它调用的同三个零件（`issues::get` 解析 detail、`water::retry_from`、`issues::resolve`）。
use rc_distill::derive::{run_derive, DeriveExit};
use rc_ledger::water::{self, Cursor};
use rc_llm::mock::MockBackend;
use rc_llm::LlmFailure;
use rc_memory::{issues, AnchorRole, FactContent, MemKey, Provenance, Scope, ScopeKind, put_card};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;
/// 夹具正文里的记号：它一个字节都不许出现在 issue.detail 里（守卫 12 的零正文）。
const CANARY: &str = "DERIVE-QUARANTINE-CANARY-7731";
/// 缺键 ⇒ 契约族（整批丢）。
const BAD: &str = r#"{"relationships":[]}"#;
/// 四键齐全、四数组全空 ⇒ `ZeroOutput`（合格的空答复，照推游标）。
const EMPTY_OK: &str = r#"{"relationships":[],"chains":[],"timeline":[],"mentions":[]}"#;

fn seed_card(root: &Root, title: &str, at: i64, turn: &str) -> MemKey {
    put_card(root.memory(), &FactContent {
        kind: "decision".into(),
        title: title.into(),
        body: format!("{CANARY} {title} 的正文"),
        scope: Scope { kind: ScopeKind::Project, key: "proj-derive-quarantine".into() },
        valid_to_ms: None,
    }, &[Provenance { turn_uid: turn.into(), anchor_role: AnchorRole::Human }], at).unwrap()
}

fn issue_count(root: &Root) -> i64 {
    root.memory().conn().query_row(
        "SELECT count(*) FROM extraction_issue", [], |r| r.get(0)).unwrap()
}

/// 最新一条 issue 的 id。隔离轮的读红基准是**库里的记录**，不是 `DeriveReadout` 的新格子：
/// 用例要能在 BASE 的 `derive.rs` 上编译并红在游标/issue 上（工单判据 4 的证红）。
fn latest_issue(root: &Root) -> i64 {
    root.memory().conn().query_row(
        "SELECT max(id) FROM extraction_issue", [], |r| r.get::<_, Option<i64>>(0))
        .unwrap().expect("隔离轮必须落一条 issue")
}

/// 本批的游标落点（批末那一行）。从选批结果里取，而不是从写入次序猜：全序是
/// `(created_at_ms, tbl, id)`，同 ms 时落到内容寻址 id 的字典序。
fn batch_end(root: &Root) -> (i64, String) {
    let batch = rc_derive::select::select_batch(root.memory(), None, 40).unwrap();
    assert!(!batch.is_empty(), "夹具必须先有活行");
    let last = batch.last().unwrap();
    (last.created_at_ms, last.key.key_string())
}

fn cursor(root: &Root) -> Option<Cursor> {
    water::read(root.ledger(), "derive", "").unwrap()
}

/// 两轮契约丢弃：游标不动、attempts 1/2、不落 issue。
fn assert_first_two_attempts_keep_the_cursor(root: &Root) {
    for round in 1..=2 {
        let backend = MockBackend::scripted(vec![Ok(BAD.to_string())]);
        let (readout, exit) = run_derive(root.memory(), root.ledger(), &backend, NOW);
        assert_eq!(exit, DeriveExit::BatchDropped, "第 {round} 轮：契约族仍是整批丢");
        assert!(readout.dropped, "第 {round} 轮：{readout:?}");
        assert_eq!(readout.drop_reason, Some("schema"));
        let cur = cursor(root).expect("note_attempt 建出水位行");
        assert_eq!((cur.at_ms, cur.turn_uid.as_str()), (0, ""),
                   "第 {round} 轮游标不许动——这是「下一轮还会选中同一批」的可观测形状");
        assert_eq!(water::attempts(root.ledger(), "derive", "").unwrap(), round,
                   "第 {round} 轮 attempts");
        assert_eq!(issue_count(root), 0, "第 {round} 轮不许落 issue");
    }
}

#[test]
fn a_bad_batch_is_quarantined_on_the_third_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let turn_a = "a".repeat(64);
    let turn_b = "b".repeat(64);
    seed_card(&root, "甲决策", NOW, &turn_a);
    seed_card(&root, "乙决策", NOW, &turn_b);
    let end = batch_end(&root);

    assert_first_two_attempts_keep_the_cursor(&root);

    // 第 3 轮：attempts 到 MAX_ATTEMPTS ⇒ 隔离 + 越过这一批。
    let backend = MockBackend::scripted(vec![Ok(BAD.to_string())]);
    let (readout, exit) = run_derive(root.memory(), root.ledger(), &backend, NOW);
    assert_eq!(exit, DeriveExit::BatchDropped, "退出码契约不变：仍是整批拒收那一档");
    assert!(readout.dropped && readout.drop_reason == Some("schema"));
    assert_eq!(issue_count(&root), 1, "第三轮隔离这一批：恰好落一条 issue");
    let issue_id = latest_issue(&root);
    let cur = cursor(&root).unwrap();
    assert_eq!((cur.at_ms, cur.turn_uid), end,
               "隔离轮把游标推到本批末尾（与成功路径同一个 Cursor）");
    assert_eq!(water::attempts(root.ledger(), "derive", "").unwrap(), 0,
               "结算写口把 attempts 归零：它不许跨批累计到误伤好批");
    assert_eq!(issue_count(&root), 1, "隔离轮只落一条 issue");

    let issue = issues::get(root.memory(), issue_id).unwrap().expect("issue 行必须可读");
    assert_eq!((issue.pass.as_str(), issue.code.as_str()), ("derive", "schema"));
    assert_eq!(issue.run_id, issues::derive_run_id(NOW));
    assert_eq!(issue.resolved_at_ms, None, "刚落下的那一行还没解决");
    let detail: serde_json::Value = serde_json::from_str(&issue.detail).unwrap();
    assert_eq!(detail["attempts"], 3, "detail 记的是到上限那一刻的 attempts：{detail}");
    assert_eq!(detail["prev_at_ms"], 0);
    assert_eq!(detail["prev_uid"], "");
    assert_eq!(detail["batch_last_at_ms"], end.0);
    assert_eq!(detail["batch_last_uid"], end.1);
    let turns: Vec<&str> = detail["turns"].as_array().unwrap().iter()
        .map(|v| v.as_str().unwrap()).collect();
    assert!(turns.contains(&turn_a.as_str()) && turns.contains(&turn_b.as_str()),
           "turns 是本批的出处轮：{turns:?}");
    assert!(turns.len() <= 40, "turns 最多 40 个：{}", turns.len());
    // 守卫 12：detail 只有元数据，夹具正文一个字节都不许进来。
    assert!(!issue.detail.contains(CANARY), "issue.detail 含正文：{}", issue.detail);
    assert!(!issue.detail.contains("甲决策"), "issue.detail 含标题：{}", issue.detail);
}

#[test]
fn the_round_after_a_quarantine_takes_the_next_batch() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed_card(&root, "甲决策", NOW, &"a".repeat(64));
    seed_card(&root, "乙决策", NOW, &"b".repeat(64));
    assert_first_two_attempts_keep_the_cursor(&root);
    let backend = MockBackend::scripted(vec![Ok(BAD.to_string())]);
    let (readout, _) = run_derive(root.memory(), root.ledger(), &backend, NOW);
    assert!(readout.dropped);
    assert_eq!(issue_count(&root), 1, "隔离轮落一条 issue（下一批不许把它再落一遍）");

    // 隔离之后才出现的新行 = 下一批。它必须被选中——原批不许再重抽。
    let next = seed_card(&root, "丙决策", NOW + 10, &"c".repeat(64));
    let backend = MockBackend::scripted(vec![Ok(EMPTY_OK.to_string())]);
    let (_, exit) = run_derive(root.memory(), root.ledger(), &backend, NOW + 20);
    assert_eq!(exit, DeriveExit::Ok, "下一批是合格的空答复：{exit:?}");
    let cur = cursor(&root).unwrap();
    assert_eq!((cur.at_ms, cur.turn_uid), (NOW + 10, next.key_string()),
               "第 4 轮必须落在下一批的末行；停在 NOW 就说明原批又被抽了一遍");
    assert_eq!(issue_count(&root), 1, "隔离记录不随下一批翻倍");
}

#[test]
fn retry_rewinds_the_cursor_to_the_issue_prev_and_the_batch_is_taken_again() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed_card(&root, "甲决策", NOW, &"a".repeat(64));
    seed_card(&root, "乙决策", NOW, &"b".repeat(64));
    let end = batch_end(&root);
    assert_first_two_attempts_keep_the_cursor(&root);
    let backend = MockBackend::scripted(vec![Ok(BAD.to_string())]);
    let _ = run_derive(root.memory(), root.ledger(), &backend, NOW);
    let issue_id = latest_issue(&root);

    // `hub derive retry` 的库面三步：读 issue → 解析 detail → retry_from + resolve。
    let issue = issues::get(root.memory(), issue_id).unwrap().unwrap();
    let detail: serde_json::Value = serde_json::from_str(&issue.detail).unwrap();
    let prev = Cursor {
        at_ms: detail["prev_at_ms"].as_i64().expect("prev_at_ms"),
        turn_uid: detail["prev_uid"].as_str().expect("prev_uid").to_owned(),
    };
    water::retry_from(root.ledger(), "derive", "", &prev, NOW + 1).unwrap();
    assert!(issues::resolve(root.memory(), issue_id, NOW + 1).unwrap());

    let cur = cursor(&root).unwrap();
    assert_eq!((cur.at_ms, cur.turn_uid.as_str()), (prev.at_ms, prev.turn_uid.as_str()),
               "回拨到隔离前的位置");
    assert_eq!(water::attempts(root.ledger(), "derive", "").unwrap(), 0, "重跑从 0 计数");
    assert!(issues::get(root.memory(), issue_id).unwrap().unwrap().resolved_at_ms.is_some(),
            "重跑入口把这一行解决掉");
    assert_eq!(issues::open_count(root.memory(), "derive").unwrap(), 0);

    // 下一轮重抽原批（游标 0 ⇒ 那一批又被选中），并且这次成功。
    let backend = MockBackend::scripted(vec![Ok(EMPTY_OK.to_string())]);
    let (_, exit) = run_derive(root.memory(), root.ledger(), &backend, NOW + 2);
    assert_eq!(exit, DeriveExit::Ok);
    let cur = cursor(&root).unwrap();
    assert_eq!((cur.at_ms, cur.turn_uid), end, "原批的批末就是 NOW 那一行");
}

#[test]
fn zero_output_still_advances_and_writes_no_issue() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed_card(&root, "甲决策", NOW, &"a".repeat(64));
    seed_card(&root, "乙决策", NOW, &"b".repeat(64));
    let end = batch_end(&root);
    let backend = MockBackend::scripted(vec![Ok(EMPTY_OK.to_string())]);
    let (readout, exit) = run_derive(root.memory(), root.ledger(), &backend, NOW);
    assert_eq!(exit, DeriveExit::Ok, "合格的空答复不是丢批：{exit:?}");
    assert!(!readout.dropped, "{readout:?}");
    let cur = cursor(&root).unwrap();
    assert_eq!((cur.at_ms, cur.turn_uid), end);
    assert_eq!(issue_count(&root), 0);
    assert_eq!(water::attempts(root.ledger(), "derive", "").unwrap(), 0);
}

#[test]
fn an_llm_fault_neither_notes_an_attempt_nor_quarantines() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed_card(&root, "甲决策", NOW, &"a".repeat(64));
    seed_card(&root, "乙决策", NOW, &"b".repeat(64));
    let backend = MockBackend::scripted(vec![
        Err(LlmFailure::Timeout),
        Err(LlmFailure::BackendError),
    ]);
    for (round, expect) in ["llm timeout", "llm backend unavailable"].iter().enumerate() {
        let (readout, exit) = run_derive(root.memory(), root.ledger(), &backend, NOW);
        let DeriveExit::Fault(cat) = exit else {
            panic!("第 {} 轮 LLM 故障必须是 Fault：{exit:?}", round + 1);
        };
        assert!(cat.contains(expect), "第 {} 轮类别：{cat}", round + 1);
        assert!(!readout.dropped, "LLM 故障不是契约丢批：{readout:?}");
        assert_eq!(cursor(&root), None, "故障期不许建水位行、更不许推游标");
        assert_eq!(water::attempts(root.ledger(), "derive", "").unwrap(), 0);
        assert_eq!(issue_count(&root), 0, "LLM 故障不写隔离记录");
    }
}
