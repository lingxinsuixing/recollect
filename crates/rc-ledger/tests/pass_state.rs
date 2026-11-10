//! S2a-1：`extract_pass_state` 的读写面（设计 §3）。这一层只做按 `(turn, pass, family)`
//! 的状态机，不选批、不调模型、不写记忆行；八条判据逐条钉在这里。
//!
//! H7c1 起同一张文件也钉接管对账的两把只读尺子：`rows_for_turns`（按 turn 批量读）
//! 与 `transcript::turn_uids_in_commit_range`（commit 区间内的轮）。
use rc_contract::{compute_body_fp, compute_turn_uid, NormalizedTurn, SessionId, TurnTier};
use rc_ledger::pass_state::{
    self, count_terminal_by_pass, count_terminal_turns, last_terminal_at_ms, latest_artifact,
    record_artifact, rows_for_turns, settled_turns, PassArtifact, PassKey, PassRecord, PassState,
    PassStateRow, ARTIFACT_MAX_CHARS, REVIEW_PENDING_REASON, TERMINAL_STATES,
};
use rc_ledger::transcript::{insert_batch_committed, turn_uids_in_commit_range};
use rc_store::{Root, StoreError};

fn key<'a>(turn_uid: &'a str, pass_id: &'a str, family: &'a str) -> PassKey<'a> {
    PassKey { turn_uid, pass_id, family }
}

fn record_of(root: &Root, k: PassKey<'_>) -> PassRecord {
    pass_state::get(root.ledger(), k).unwrap().expect("状态行不存在")
}

fn row_count(root: &Root) -> i64 {
    root.ledger()
        .conn()
        .query_row("SELECT count(*) FROM extract_pass_state", [], |r| r.get(0))
        .unwrap()
}

/// 判据 1：空库没有行（`None` 不是 `Pending`），于是这一 pass 必须要跑。
#[test]
fn empty_ledger_reads_none_and_should_run_is_true() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    assert!(pass_state::get(root.ledger(), key("turn-a", "naming", "")).unwrap().is_none());
    assert!(pass_state::should_run(root.ledger(), key("turn-a", "naming", ""), "fp-1", 1_000).unwrap());
}

/// 判据 2：`mark_running` 逐列落值；重复开跑是覆盖（指纹与 run_id 换新），且一行只有一条。
///
/// `attempt` 这一列 W19c1 起分两种：**退避中的 `pending` 重开时保留**（那是"这一格失败了几次"，
/// 上限判定靠它数得下去），**其余入口归零**（首次、`stale`、终态之后的重跑都是新一轮）。
#[test]
fn mark_running_writes_every_column_and_a_rerun_resets_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k = key("turn-a", "naming", "");
    pass_state::mark_running(root.ledger(), k, "fp-1", "run-1", 2_000).unwrap();
    assert_eq!(
        record_of(&root, k),
        PassRecord {
            state: PassState::Running,
            attempt: 0,
            next_retry_at_ms: None,
            fingerprint: Some("fp-1".into()),
            reason: None,
            run_id: Some("run-1".into()),
            updated_at_ms: 2_000,
            prompt_version: None,
        }
    );
    // 失败一次把 attempt 抬到 1：退避中的重开**保留**这个数（W19c1 的隔离上限靠它）。
    pass_state::note_attempt(root.ledger(), k, 60_000, 2_100).unwrap();
    assert_eq!(record_of(&root, k).attempt, 1);
    pass_state::mark_running(root.ledger(), k, "fp-2", "run-2", 3_000).unwrap();
    let r = record_of(&root, k);
    assert_eq!(r.state, PassState::Running);
    assert_eq!(r.attempt, 1, "退避中的重开把失败计数清零了 ⇒ 隔离上限永远到不了");
    assert_eq!(r.next_retry_at_ms, None);
    assert_eq!(r.fingerprint.as_deref(), Some("fp-2"));
    assert_eq!(r.run_id.as_deref(), Some("run-2"));
    assert_eq!(r.updated_at_ms, 3_000);
    assert_eq!(row_count(&root), 1, "同一 (turn, pass, family) 只许一行");

    // 终态之后的重跑是新一轮：`attempt` 归零（`stale` 同一条，见下一条断言）。
    pass_state::mark_terminal(root.ledger(), k, PassState::Done, None, 4_000).unwrap();
    pass_state::mark_running(root.ledger(), k, "fp-3", "run-3", 5_000).unwrap();
    assert_eq!(record_of(&root, k).attempt, 0, "终态之后的重开必须从零数");
    pass_state::note_attempt(root.ledger(), k, 60_000, 5_100).unwrap();
    pass_state::mark_terminal(root.ledger(), k, PassState::Stale, None, 6_000).unwrap();
    pass_state::mark_running(root.ledger(), k, "fp-4", "run-4", 7_000).unwrap();
    assert_eq!(record_of(&root, k).attempt, 0, "stale 的重开必须从零数");
}

/// 判据 3：终态可写可覆盖；Running/Pending 传进 `mark_terminal` 是调用方 bug（响亮拒绝，
/// 且拒写的调用一个字节都不改行）。
#[test]
fn mark_terminal_records_and_loudly_refuses_non_terminal_states() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k = key("turn-a", "event", "");
    pass_state::mark_running(root.ledger(), k, "fp-1", "run-1", 2_000).unwrap();
    pass_state::mark_terminal(root.ledger(), k, PassState::Done, Some("ok"), 3_000).unwrap();
    let r = record_of(&root, k);
    assert_eq!(r.state, PassState::Done);
    assert_eq!(r.reason.as_deref(), Some("ok"));
    assert_eq!(r.updated_at_ms, 3_000);
    assert_eq!(r.next_retry_at_ms, None, "终态行没有下一次重试");
    assert_eq!(r.fingerprint.as_deref(), Some("fp-1"), "终态写不许抹掉指纹");

    let before = r;
    for bad in [PassState::Running, PassState::Pending] {
        let e = pass_state::mark_terminal(root.ledger(), k, bad, None, 4_000).unwrap_err();
        assert!(matches!(e, StoreError::Sql(_)), "{bad:?} 必须以 StoreError::Sql 拒绝：{e}");
    }
    assert_eq!(record_of(&root, k), before, "被拒的 mark_terminal 改动了行");

    // 重复 mark_terminal 是覆盖（不是插入第二条）。
    pass_state::mark_terminal(root.ledger(), k, PassState::Rejected, Some("schema"), 5_000)
        .unwrap();
    let r = record_of(&root, k);
    assert_eq!(r.state, PassState::Rejected);
    assert_eq!(r.reason.as_deref(), Some("schema"));
    assert_eq!(row_count(&root), 1);
}

/// 判据 4：失败退避 attempt 1→2→3，`next_retry_at_ms` 每次刷新；到期前不跑、到期（<= now）跑。
#[test]
fn note_attempt_backs_off_and_should_run_waits_for_the_due_time() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k = key("turn-a", "fact", "promise");
    pass_state::mark_running(root.ledger(), k, "fp-1", "run-1", 10_000).unwrap();

    pass_state::note_attempt(root.ledger(), k, 60_000, 10_000).unwrap();
    let r = record_of(&root, k);
    assert_eq!(r.attempt, 1);
    assert_eq!(r.next_retry_at_ms, Some(70_000));
    assert_eq!(r.state, PassState::Pending, "失败后回到 pending");

    pass_state::note_attempt(root.ledger(), k, 240_000, 70_000).unwrap();
    assert_eq!(record_of(&root, k).attempt, 2);
    assert_eq!(record_of(&root, k).next_retry_at_ms, Some(310_000));

    pass_state::note_attempt(root.ledger(), k, 960_000, 310_000).unwrap();
    assert_eq!(record_of(&root, k).attempt, 3);
    assert_eq!(record_of(&root, k).next_retry_at_ms, Some(1_270_000));

    assert!(!pass_state::should_run(root.ledger(), k, "fp-1", 1_269_999).unwrap(),
            "退避未到期不许跑");
    assert!(pass_state::should_run(root.ledger(), k, "fp-1", 1_270_000).unwrap(),
            "到期即跑（<= now 就算到期）");
}

/// 判据 4c（W19c1）：`note_attempt_or_quarantine` 数满 [`MAX_PASS_ATTEMPTS`] 次之后把这一格
/// 隔离成 `quarantined(retry_exhausted)`，没数满之前一次都不隔离。
///
/// 三次"开跑 → 失败"用真实的两个入口轮流跑（`mark_running` + `note_attempt_or_quarantine`），
/// 因为"计数能跨进程累积"恰恰是这一格的成败所在：谁把 `attempt` 在开跑时清零，
/// 第三条断言就红（上限永远到不了 ⇒ 卡死的对端每轮重烧）。
#[test]
fn the_call_wall_quarantines_a_unit_after_the_last_allowed_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k = key("turn-a", "naming", "");

    for attempt in 1..pass_state::MAX_PASS_ATTEMPTS {
        pass_state::mark_running(root.ledger(), k, "fp-1", "run-1", 1_000 * attempt).unwrap();
        let state = pass_state::note_attempt_or_quarantine(
            root.ledger(), k, 60_000, 1_000 * attempt + 500).unwrap();
        assert_eq!(state, PassState::Pending, "第 {attempt} 次失败还没到上限，必须是 pending");
        assert_eq!(record_of(&root, k).state, PassState::Pending);
        assert_eq!(record_of(&root, k).reason, None, "退避中的行不许挂隔离理由");
    }

    // 最后一次：这一次记完就到了上限 ⇒ 隔离（不是 rejected：产出没被判坏，只是没拿到答复）。
    pass_state::mark_running(root.ledger(), k, "fp-1", "run-last", 9_000).unwrap();
    let state = pass_state::note_attempt_or_quarantine(root.ledger(), k, 60_000, 9_500).unwrap();
    assert_eq!(state, PassState::Quarantined, "到上限的这一次必须交回 Quarantined");
    let r = record_of(&root, k);
    assert_eq!(r.state, PassState::Quarantined);
    assert_eq!(r.reason.as_deref(), Some("retry_exhausted"));
    assert_eq!(r.reason.as_deref(), Some(pass_state::RETRY_EXHAUSTED_REASON));
    assert_eq!(r.attempt, pass_state::MAX_PASS_ATTEMPTS, "隔离那一刻的计数要留住");
    assert_eq!(r.next_retry_at_ms, None, "终态行没有下一次重试");
    assert!(!pass_state::should_run(root.ledger(), k, "fp-1", 10_000_000).unwrap(),
            "隔离之后不再被选批捡起");
}

/// 判据 4d（W19c1）：隔离**只对 `pending` 生效**。已经到终态的行（这里用 `done` 与 `rejected`）
/// 不论 `attempt` 多大都不被改写——它们是别人的结论，不是"这一格卡住了"。
#[test]
fn an_already_settled_row_is_never_quarantined_by_a_late_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k = key("turn-a", "event", "");
    pass_state::mark_running(root.ledger(), k, "fp-1", "run-1", 1_000).unwrap();
    for i in 0..5 {
        pass_state::note_attempt(root.ledger(), k, 60_000, 1_100 + i).unwrap();
    }
    pass_state::mark_terminal(root.ledger(), k, PassState::Done, None, 2_000).unwrap();
    let before = record_of(&root, k);
    assert_eq!(before.state, PassState::Done);
    assert!(before.attempt >= pass_state::MAX_PASS_ATTEMPTS, "夹具里的计数必须已经过线");

    let state = pass_state::note_attempt_or_quarantine(root.ledger(), k, 60_000, 3_000).unwrap();
    assert_eq!(state, PassState::Pending, "终态行不许被这一支改写成隔离");
    let after = record_of(&root, k);
    assert_eq!(after.state, PassState::Done, "终态被改写了");
    assert_eq!(after.reason, before.reason);
}

/// 判据 4b（S3b-4b）：`mark_awaiting_review` 把"抽取完成、窄审还没跑"写成
/// `pending(review_pending)`——指纹/run_id/attempt 保留、退避清空、时刻刷新；
/// 没先 `mark_running` 的格子响亮拒绝（不替它插一行没有开工记录的 pending）。
#[test]
fn mark_awaiting_review_keeps_the_run_identity_and_clears_the_backoff() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k = key("turn-a", "fact", "promise");

    // ① 缺行：调用方跳过了 `mark_running` ⇒ StoreError::Sql，且一行都不留。
    let e = pass_state::mark_awaiting_review(root.ledger(), k, 1_000).unwrap_err();
    assert!(matches!(e, StoreError::Sql(_)), "缺行必须报 StoreError::Sql：{e}");
    assert_eq!(row_count(&root), 0, "被拒的标记留下了一行");

    // ② 正常路径：抽取开过工、失败过一次（attempt=1、退避已记），审核还没跑。
    pass_state::mark_running(root.ledger(), k, "fp-1", "run-1", 2_000).unwrap();
    pass_state::note_attempt(root.ledger(), k, 60_000, 2_100).unwrap();
    pass_state::mark_awaiting_review(root.ledger(), k, 3_000).unwrap();

    let r = record_of(&root, k);
    assert_eq!(r.state, PassState::Pending);
    assert_eq!(r.reason.as_deref(), Some("review_pending"));
    assert_eq!(r.reason.as_deref(), Some(REVIEW_PENDING_REASON));
    assert_eq!(r.next_retry_at_ms, None, "等审核不是等退避");
    assert_eq!(r.fingerprint.as_deref(), Some("fp-1"), "指纹是开工记录，不许抹掉");
    assert_eq!(r.run_id.as_deref(), Some("run-1"), "run_id 保留");
    assert_eq!(r.attempt, 1, "attempt 是历史读数，保留");
    assert_eq!(r.updated_at_ms, 3_000);

    // ③ 重复标记是覆盖：只换时刻，不长第二条。
    pass_state::mark_awaiting_review(root.ledger(), k, 4_000).unwrap();
    assert_eq!(record_of(&root, k).updated_at_ms, 4_000);
    assert_eq!(record_of(&root, k).state, PassState::Pending);
    assert_eq!(row_count(&root), 1);
}

/// 判据 5：指纹是"这一行是谁产的"。同指纹 + done ⇒ 不跑；指纹变了 ⇒ 跑，重跑后读数更新。
#[test]
fn a_changed_fingerprint_makes_the_pass_runnable_again() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k = key("turn-a", "time", "");
    pass_state::mark_running(root.ledger(), k, "fp-1", "run-1", 1_000).unwrap();
    pass_state::mark_terminal(root.ledger(), k, PassState::Done, None, 1_100).unwrap();
    assert!(!pass_state::should_run(root.ledger(), k, "fp-1", 2_000).unwrap(),
            "同指纹 + done ⇒ 不调用、不追加");
    assert!(pass_state::should_run(root.ledger(), k, "fp-2", 2_000).unwrap(),
            "指纹不同 ⇒ 重跑");

    pass_state::mark_running(root.ledger(), k, "fp-2", "run-2", 3_000).unwrap();
    assert_eq!(record_of(&root, k).fingerprint.as_deref(), Some("fp-2"),
               "开跑就把旧指纹/旧状态覆盖成新的");
    pass_state::mark_terminal(root.ledger(), k, PassState::Done, None, 3_100).unwrap();
    assert_eq!(record_of(&root, k).fingerprint.as_deref(), Some("fp-2"));
    assert!(!pass_state::should_run(root.ledger(), k, "fp-2", 4_000).unwrap());
    assert_eq!(row_count(&root), 1);
}

/// 判据 5b（P0-2）：`prompt_version` 只由 system prompt 算（64 位 hex、同输入同值、换 system
/// 换值）；开跑时与指纹同一次写落库，终态写不抹掉它；"改判据 ⇒ 可重选"的判据只认四个终态，
/// 且 **NULL 一律不可重选**（旧库升级后不自动全量重跑）。
#[test]
fn prompt_version_is_system_only_and_only_a_non_null_change_reselects() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let l = root.ledger();
    let k = key("turn-a", "event", "");

    // ① 版本号：64 位小写 hex；同一段 system 稳定；换 system 就换值。
    let v1 = pass_state::prompt_version("你是抽取器：只输出 JSON。");
    let again = pass_state::prompt_version("你是抽取器：只输出 JSON。");
    let v2 = pass_state::prompt_version("你是抽取器：只输出 JSON。（改了一句判据）");
    assert_eq!(v1.len(), 64, "sha256 十六进制");
    assert!(v1.bytes().all(|b| b.is_ascii_hexdigit()), "{v1}");
    assert_eq!(v1, again, "同一段 system 必须稳定");
    assert_ne!(v1, v2, "system 变了版本号必须变");

    // ② 开跑：版本号与指纹同一次写落库。
    pass_state::mark_running_with_prompt_version(l, k, "fp-1", Some(&v1), "run-1", 1_000)
        .unwrap();
    assert_eq!(record_of(&root, k).prompt_version.as_deref(), Some(v1.as_str()));

    // ③ 终态写保留版本号（它是开工记录，不是可被终态清掉的中间值）。
    pass_state::mark_terminal(l, k, PassState::Done, None, 1_100).unwrap();
    assert_eq!(record_of(&root, k).prompt_version.as_deref(), Some(v1.as_str()));

    // ④ 判据：四个终态 + 非空 + 不同 ⇒ 可重选；NULL 与同值都不行；非终态不归它管。
    for state in [PassState::Done, PassState::Empty, PassState::Rejected,
                  PassState::Quarantined] {
        assert!(pass_state::prompt_version_reselectable(state, Some(&v1), &v2),
                "{state:?} + 版本不同必须可重选");
        assert!(!pass_state::prompt_version_reselectable(state, Some(&v1), &v1),
                "{state:?} + 同版本不许重选");
        assert!(!pass_state::prompt_version_reselectable(state, None, &v2),
                "{state:?} + NULL 不许重选（旧库不自动全量重跑）");
    }
    for state in [PassState::Running, PassState::Pending, PassState::Stale] {
        assert!(!pass_state::prompt_version_reselectable(state, Some(&v1), &v2),
                "{state:?} 的重跑判据不归这一列");
    }

    // ⑤ 没有模型面的开跑（`mark_running`）把这格写回 NULL，不继承上一版的版本号。
    pass_state::mark_running(l, k, "fp-2", "run-2", 2_000).unwrap();
    assert_eq!(record_of(&root, k).prompt_version, None);
}

/// 判据 6：`count_by_state` 按 pass 分开，且 slug 字典序（不靠调用方二次排序）。
#[test]
fn count_by_state_is_per_pass_and_in_slug_order() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    for (turn, state) in [("t-1", PassState::Done), ("t-2", PassState::Done),
                          ("t-3", PassState::Pending)] {
        let k = key(turn, "naming", "");
        pass_state::mark_running(root.ledger(), k, "fp-1", "run-1", 1_000).unwrap();
        if state == PassState::Pending {
            // pending 是 `note_attempt` 的产物（退避到期前它也是 pending）。
            pass_state::note_attempt(root.ledger(), k, 0, 1_100).unwrap();
        } else {
            pass_state::mark_terminal(root.ledger(), k, state, None, 1_100).unwrap();
        }
    }
    let k = key("t-1", "fact", "decision");
    pass_state::mark_running(root.ledger(), k, "fp-1", "run-1", 1_000).unwrap();
    pass_state::mark_terminal(root.ledger(), k, PassState::Empty, None, 1_100).unwrap();

    assert_eq!(pass_state::count_by_state(root.ledger(), "naming").unwrap(),
               vec![(PassState::Done, 2), (PassState::Pending, 1)]);
    assert_eq!(pass_state::count_by_state(root.ledger(), "fact").unwrap(),
               vec![(PassState::Empty, 1)]);
    assert_eq!(pass_state::count_by_state(root.ledger(), "event").unwrap(), vec![]);
}

/// 判据 7：非法 `pass_id` / 空 `turn_uid` / 非法 `family` 一律 `StoreError::Sql`（不静默）；
/// 库里躺着读不出的 slug 时 `get` 必须响（同 `water::extract_record` 的纪律）。
#[test]
fn illegal_keys_are_refused_and_a_bad_slug_reads_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    for bad in [key("turn-a", "entity", ""), key("", "naming", ""), key("turn-a", "", ""),
                key("turn-a", "naming", "promise")] {
        assert!(matches!(pass_state::get(root.ledger(), bad).unwrap_err(), StoreError::Sql(_)),
                "get 必须拒绝非法键 {bad:?}");
        assert!(matches!(
            pass_state::mark_running(root.ledger(), bad, "fp-1", "run-1", 1).unwrap_err(),
            StoreError::Sql(_)), "mark_running 必须拒绝非法键 {bad:?}");
        assert!(matches!(
            pass_state::mark_terminal(root.ledger(), bad, PassState::Done, None, 1).unwrap_err(),
            StoreError::Sql(_)), "mark_terminal 必须拒绝非法键 {bad:?}");
        assert!(matches!(
            pass_state::note_attempt(root.ledger(), bad, 60_000, 1).unwrap_err(),
            StoreError::Sql(_)), "note_attempt 必须拒绝非法键 {bad:?}");
        assert!(matches!(
            pass_state::mark_awaiting_review(root.ledger(), bad, 1).unwrap_err(),
            StoreError::Sql(_)), "mark_awaiting_review 必须拒绝非法键 {bad:?}");
        assert!(matches!(
            pass_state::should_run(root.ledger(), bad, "fp-1", 1).unwrap_err(),
            StoreError::Sql(_)), "should_run 必须拒绝非法键 {bad:?}");
    }
    assert!(matches!(
        pass_state::count_by_state(root.ledger(), "entity").unwrap_err(), StoreError::Sql(_)));
    assert_eq!(row_count(&root), 0, "被拒的写入留下了行");

    // DDL 的 CHECK 自己就拦坏 slug（API 之外的第二道闸）。
    let conn = root.ledger().conn();
    let insert = |state: &str| {
        conn.execute(
            "INSERT INTO extract_pass_state(turn_uid, pass_id, family, state, attempt,
                                            next_retry_at_ms, fingerprint, reason, run_id,
                                            updated_at_ms)
             VALUES('turn-bad', 'naming', '', ?1, 0, NULL, 'fp-1', NULL, NULL, 1)",
            rusqlite::params![state],
        )
    };
    assert!(insert("wat").is_err(), "DDL 的 CHECK(state IN …) 必须拦住坏 slug");

    // 绕过 CHECK 写进去（旧库/外部工具的唯一现实路径），读的时候必须响。
    conn.pragma_update(None, "ignore_check_constraints", true).unwrap();
    insert("wat").unwrap();
    conn.pragma_update(None, "ignore_check_constraints", false).unwrap();
    let e = pass_state::get(root.ledger(), key("turn-bad", "naming", "")).unwrap_err();
    assert!(matches!(e, StoreError::Sql(_)), "坏 slug 必须报 StoreError::Sql：{e}");
    assert!(e.to_string().contains("wat"), "错误信息要带上读不出的 slug：{e}");
}

/// 判据 8：`family` 只对 fact 取值（`''` 或 decision|promise|plan）；非 fact 只许空串。
#[test]
fn family_is_only_for_fact_and_only_the_three_variants() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    for family in ["", "decision", "promise", "plan"] {
        let k = key("turn-a", "fact", family);
        pass_state::mark_running(root.ledger(), k, "fp-1", "run-1", 1_000).unwrap();
        assert_eq!(record_of(&root, k).state, PassState::Running);
    }
    let k = key("turn-a", "naming", "");
    pass_state::mark_running(root.ledger(), k, "fp-1", "run-1", 1_000).unwrap();
    assert!(pass_state::get(root.ledger(), k).unwrap().is_some(), "非 fact + 空串可写");
    assert_eq!(row_count(&root), 5, "family 是主键的第三格，四类 fact 变体各一行");

    for bad in ["verdict", "fact", "PROMISE"] {
        let k = key("turn-a", "fact", bad);
        assert!(matches!(
            pass_state::mark_running(root.ledger(), k, "fp-1", "run-1", 1_000).unwrap_err(),
            StoreError::Sql(_)), "fact 的非法 family {bad:?} 必须被拒");
    }
    assert_eq!(row_count(&root), 5, "被拒的写入留下了行");
}

fn artifact_count(root: &Root) -> i64 {
    root.ledger()
        .conn()
        .query_row("SELECT count(*) FROM extract_pass_artifact", [], |r| r.get(0))
        .unwrap()
}

/// 判据 9（S2e-3）：artifact 往返逐列；读只认同一个 `(pass_id, family, stage)` 格子，
/// 没写过的格子给 `None`（不是空串、也不是别的格子的行）。
#[test]
fn artifact_round_trips_and_filters_by_pass_family_and_stage() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let l = root.ledger();
    record_artifact(l, "fact", "promise", "extract", "run-1", "turn-a", "抽取原文", 1_000)
        .unwrap();
    record_artifact(l, "fact", "promise", "review", "run-1", "turn-a", "审核原文", 1_100)
        .unwrap();
    record_artifact(l, "event", "", "extract", "run-2", "turn-b", "事件原文", 1_200).unwrap();

    assert_eq!(
        latest_artifact(l, "fact", "promise", "extract").unwrap(),
        Some(PassArtifact {
            pass_id: "fact".into(),
            family: "promise".into(),
            stage: "extract".into(),
            run_id: "run-1".into(),
            turn_uid: "turn-a".into(),
            raw: "抽取原文".into(),
            created_at_ms: 1_000,
        })
    );
    assert_eq!(
        latest_artifact(l, "fact", "promise", "review").unwrap().unwrap().raw,
        "审核原文"
    );
    assert_eq!(latest_artifact(l, "event", "", "extract").unwrap().unwrap().raw, "事件原文");

    // 零对照：同一条查询对没写过的格子必须命中 0 行。
    assert!(latest_artifact(l, "fact", "decision", "extract").unwrap().is_none());
    assert!(latest_artifact(l, "event", "", "review").unwrap().is_none());
    assert!(latest_artifact(l, "naming", "", "extract").unwrap().is_none());
    assert_eq!(artifact_count(&root), 3, "artifact 是追加表，不覆盖");
}

/// 判据 10：同一个格子的 latest 取最近一条（`created_at_ms` 大的赢；同刻按插入序，后写的赢）。
#[test]
fn latest_artifact_is_the_newest_of_the_same_cell() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let l = root.ledger();
    record_artifact(l, "fact", "plan", "extract", "run-1", "turn-a", "第一", 1_000).unwrap();
    record_artifact(l, "fact", "plan", "extract", "run-3", "turn-a", "第二", 3_000).unwrap();
    record_artifact(l, "fact", "plan", "extract", "run-2", "turn-a", "第三", 2_000).unwrap();
    // 同刻两条：后写的那条是 latest（id 是唯一能拆平的量）。
    record_artifact(l, "fact", "plan", "extract", "run-4", "turn-a", "先写", 4_000).unwrap();
    record_artifact(l, "fact", "plan", "extract", "run-5", "turn-a", "后写", 4_000).unwrap();

    let latest = latest_artifact(l, "fact", "plan", "extract").unwrap().unwrap();
    assert_eq!(latest.raw, "后写");
    assert_eq!(latest.run_id, "run-5");
    assert_eq!(latest.created_at_ms, 4_000);
    assert_eq!(artifact_count(&root), 5, "latest 读不许删旧行");
}

/// 判据 11：超长原文按**字符**截断到 64,000（不切坏 UTF-8、不 panic）；短的与恰好等于上限的
/// 原样落库。
#[test]
fn artifact_truncates_overlong_raw_at_a_char_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let l = root.ledger();

    // ① 70,000 个三字节汉字（210 KB 的原文）⇒ 64,000 个汉字。
    let long: String = "记".repeat(70_000);
    record_artifact(l, "fact", "promise", "extract", "run-1", "turn-a", &long, 1_000).unwrap();
    let got = latest_artifact(l, "fact", "promise", "extract").unwrap().unwrap().raw;
    assert_eq!(got.chars().count(), ARTIFACT_MAX_CHARS);
    assert_eq!(got, "记".repeat(ARTIFACT_MAX_CHARS));

    // ② 截断点正好落在一个三字节字符上：前 63,999 个 ASCII + 一个汉字 + 尾巴。
    //    按字节切会切坏这个汉字（panic 或 U+FFFD），按字符切只留到它为止。
    let mixed = format!("{}记{}", "a".repeat(63_999), "tail");
    record_artifact(l, "fact", "promise", "extract", "run-2", "turn-a", &mixed, 2_000).unwrap();
    let got = latest_artifact(l, "fact", "promise", "extract").unwrap().unwrap().raw;
    assert_eq!(got.chars().count(), ARTIFACT_MAX_CHARS);
    assert_eq!(got, format!("{}记", "a".repeat(63_999)));

    // ③ 恰好 64,000 字符：不截（边界不是 63,999）。
    let exact: String = "b".repeat(ARTIFACT_MAX_CHARS);
    record_artifact(l, "fact", "promise", "extract", "run-3", "turn-a", &exact, 3_000).unwrap();
    assert_eq!(
        latest_artifact(l, "fact", "promise", "extract").unwrap().unwrap().raw,
        exact
    );

    // ④ 短原文原样落库。
    record_artifact(l, "fact", "promise", "extract", "run-4", "turn-a", "短文", 4_000).unwrap();
    assert_eq!(
        latest_artifact(l, "fact", "promise", "extract").unwrap().unwrap().raw,
        "短文"
    );
}

/// 判据 12：`stage` 只许 `extract|review`、`pass_id` 非空；非法一律 `StoreError::Sql`
/// 且一个字节都不写；DDL 的 CHECK 是第二道闸。
#[test]
fn artifact_refuses_an_unknown_stage_and_an_empty_pass_id() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let l = root.ledger();
    for bad in ["", "EXTRACT", "extract ", "review_extra", "schema"] {
        let e = record_artifact(l, "fact", "promise", bad, "run-1", "turn-a", "原文", 1)
            .unwrap_err();
        assert!(matches!(e, StoreError::Sql(_)), "stage {bad:?} 必须以 StoreError::Sql 拒绝：{e}");
        let e = latest_artifact(l, "fact", "promise", bad).unwrap_err();
        assert!(matches!(e, StoreError::Sql(_)), "读侧 stage {bad:?} 也要响：{e}");
    }
    let e = record_artifact(l, "", "", "extract", "run-1", "turn-a", "原文", 1).unwrap_err();
    assert!(matches!(e, StoreError::Sql(_)), "空 pass_id 必须被拒：{e}");
    assert_eq!(artifact_count(&root), 0, "被拒的写入留下了行");

    // 绕过 API 直接插：DDL 的 CHECK(stage IN …) 自己就拦坏 stage。
    let insert = |stage: &str| {
        l.conn().execute(
            "INSERT INTO extract_pass_artifact(pass_id, family, stage, run_id, turn_uid, raw,
                                               created_at_ms)
             VALUES('fact', 'promise', ?1, 'run-1', 'turn-a', '原文', 1)",
            rusqlite::params![stage],
        )
    };
    assert!(insert("other").is_err(), "DDL 的 CHECK(stage IN …) 必须拦住坏 stage");
    assert!(insert("extract").is_ok());
}

// ── H7c1：按 turn 批量读（接管对账的只读尺子之一） ────────────────────────────────
//
// `hub reconcile memory` 要问"缺口内这些轮各 pass 格是什么状态"。一轮一轮 `get` 是 N 次
// prepare；`rows_for_turns` 是一条 `IN (...)` 的只读查询（500/块绕开变量上限）。

fn uids(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// 判据 13：只返回入参里的轮，按 `(turn_uid, pass_id, family)` 升序，带 reason；
/// 空入参 ⇒ 空表；查不到的轮不出现（不是错误）。
#[test]
fn rows_for_turns_reads_only_requested_turns_in_key_order() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let l = root.ledger();
    // 写入序故意与返回序相反：先 turn-b，再 turn-a，另有一条**不该被读到**的 turn-z。
    pass_state::mark_terminal(l, key("turn-b", "naming", ""), PassState::Done, Some("ok"), 10)
        .unwrap();
    pass_state::mark_terminal(
        l,
        key("turn-b", "fact", "promise"),
        PassState::Stale,
        Some("fp"),
        11,
    )
    .unwrap();
    pass_state::mark_terminal(l, key("turn-a", "event", ""), PassState::Done, None, 12).unwrap();
    pass_state::mark_terminal(l, key("turn-z", "naming", ""), PassState::Done, Some("ok"), 13)
        .unwrap();

    let got = rows_for_turns(l, &uids(&["turn-b", "turn-a"])).unwrap();
    assert_eq!(
        got,
        vec![
            PassStateRow {
                turn_uid: "turn-a".into(),
                pass_id: "event".into(),
                family: String::new(),
                state: PassState::Done,
                reason: None,
            },
            PassStateRow {
                turn_uid: "turn-b".into(),
                pass_id: "fact".into(),
                family: "promise".into(),
                state: PassState::Stale,
                reason: Some("fp".into()),
            },
            PassStateRow {
                turn_uid: "turn-b".into(),
                pass_id: "naming".into(),
                family: String::new(),
                state: PassState::Done,
                reason: Some("ok".into()),
            },
        ],
        "只返回入参里的轮，按 (turn_uid, pass_id, family) 升序"
    );

    // 空入参 ⇒ 空表（不查库、不报错）。
    assert!(rows_for_turns(l, &[]).unwrap().is_empty());
    // 一个都不存在 ⇒ 空表，而不是"某一行读不出来"。
    assert!(rows_for_turns(l, &uids(&["turn-missing"])).unwrap().is_empty());
}

/// 判据 14：分块查询（500/块）不许吞行——501 轮的跨块边界那一条必须还在，
/// 且结果序与分块方式无关。
#[test]
fn rows_for_turns_splits_queries_at_the_variable_limit() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let l = root.ledger();
    let turns: Vec<String> = (0..501).map(|i| format!("turn-{i:04}")).collect();
    for (i, uid) in turns.iter().enumerate() {
        pass_state::mark_terminal(l, key(uid, "naming", ""), PassState::Done, None, i as i64)
            .unwrap();
    }

    let got = rows_for_turns(l, &turns).unwrap();
    assert_eq!(got.len(), 501, "分块查询丢了行");
    assert_eq!(got.first().unwrap().turn_uid, "turn-0000");
    assert_eq!(got.last().unwrap().turn_uid, "turn-0500");
    // 第 500 条（第一块的末条）与第 501 条（第二块的首条）都要在。
    assert!(got.iter().any(|r| r.turn_uid == "turn-0499"));
    assert!(got.iter().any(|r| r.turn_uid == "turn-0500"));
}

/// 判据 15b（波三修 1）：`settled_turns` 只认 `done/empty/rejected/quarantined`——
/// `stale`（判据变了、要重跑）与 `running`/`pending`（还没有结论）都不算；
/// 别的 pass 的行不串进来；空入参 ⇒ 空集；`lesson` 已是合法 pass（闭集加宽）。
#[test]
fn settled_turns_are_terminal_but_never_stale_or_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let l = root.ledger();
    // 四种终态都算"已盖棺"。
    pass_state::mark_terminal(l, key("turn-done", "lesson", ""), PassState::Done, None, 1)
        .unwrap();
    pass_state::mark_terminal(l, key("turn-empty", "lesson", ""), PassState::Empty, None, 1)
        .unwrap();
    pass_state::mark_terminal(
        l,
        key("turn-rejected", "lesson", ""),
        PassState::Rejected,
        Some("json"),
        1,
    )
    .unwrap();
    pass_state::mark_terminal(
        l,
        key("turn-quarantined", "lesson", ""),
        PassState::Quarantined,
        Some("review_uncertain"),
        1,
    )
    .unwrap();
    // 三支"没结论"的：`stale` 要重跑，`running`/`pending` 还在路上。
    pass_state::mark_terminal(l, key("turn-stale", "lesson", ""), PassState::Stale, Some("fp"), 1)
        .unwrap();
    pass_state::mark_running(l, key("turn-running", "lesson", ""), "fp", "run-1", 1).unwrap();
    pass_state::note_attempt(l, key("turn-pending", "lesson", ""), 60_000, 1).unwrap();
    // 同一轮在别的 pass 上的终态行不串成 lesson 的结论。
    pass_state::mark_terminal(l, key("turn-other", "event", ""), PassState::Done, None, 1)
        .unwrap();

    let all = uids(&[
        "turn-done",
        "turn-empty",
        "turn-rejected",
        "turn-quarantined",
        "turn-stale",
        "turn-running",
        "turn-pending",
        "turn-other",
        "turn-missing",
    ]);
    let got = settled_turns(l, "lesson", &all).unwrap();
    let mut names: Vec<&str> = got.iter().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["turn-done", "turn-empty", "turn-quarantined", "turn-rejected"],
        "只有 done/empty/rejected/quarantined 算已落终态"
    );
    // 空入参 ⇒ 空集（不开查询、不报错）。
    assert!(settled_turns(l, "lesson", &[]).unwrap().is_empty());
}

// ── H7c1：commit 区间内的轮（另一把只读尺子） ─────────────────────────────────────

fn session(key: &str) -> SessionId {
    SessionId {
        session_key: key.into(),
        device_id: "dev-range".into(),
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

/// 判据 15：区间**左开右闭**、按 `(commit_seq, turn_uid)` 升序、`limit` 生效、
/// 空区间给空表。三个提交：1 两条轮、2 一条、3 一条。
#[test]
fn turn_uids_in_commit_range_is_left_open_right_closed_and_ordered() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let s = session("s-range");
    let a = turn_of(&s, "c1-a", 100);
    let b = turn_of(&s, "c1-b", 200);
    let c = turn_of(&s, "c2-a", 300);
    let d = turn_of(&s, "c3-a", 400);
    insert_batch_committed(root.transcript_mut(), &s, &[a.clone(), b.clone()], 10, "import")
        .unwrap();
    insert_batch_committed(root.transcript_mut(), &s, std::slice::from_ref(&c), 20, "import")
        .unwrap();
    insert_batch_committed(root.transcript_mut(), &s, std::slice::from_ref(&d), 30, "import")
        .unwrap();
    let t = root.transcript();

    // 全区间：提交 1 的两条按 turn_uid 排（同一 commit_seq 内的次序），再到 2、3。
    let mut first = vec![a.turn_uid.clone(), b.turn_uid.clone()];
    first.sort();
    let all = turn_uids_in_commit_range(t, 0, 3, 0).unwrap();
    assert_eq!(
        all,
        [first.clone(), vec![c.turn_uid.clone()], vec![d.turn_uid.clone()]].concat(),
        "按 (commit_seq, turn_uid) 升序"
    );

    // 左开：from=1 不含提交 1（提交 1 的轮已经覆盖）；右闭：to=2 含提交 2。
    assert_eq!(
        turn_uids_in_commit_range(t, 1, 2, 0).unwrap(),
        vec![c.turn_uid.clone()]
    );
    assert_eq!(
        turn_uids_in_commit_range(t, 2, 3, 0).unwrap(),
        vec![d.turn_uid.clone()]
    );

    // 空区间（to <= from）：空表，不报错。
    assert!(turn_uids_in_commit_range(t, 2, 2, 0).unwrap().is_empty());
    assert!(turn_uids_in_commit_range(t, 3, 2, 0).unwrap().is_empty());
    assert!(turn_uids_in_commit_range(t, 0, 0, 0).unwrap().is_empty());

    // limit：0 = 不限；非 0 只取前 N 条（截断提示归调用方——问 limit+1 条）。
    assert_eq!(turn_uids_in_commit_range(t, 0, 3, 1).unwrap(), vec![first[0].clone()]);
    assert_eq!(turn_uids_in_commit_range(t, 0, 3, 3).unwrap().len(), 3);
    assert_eq!(turn_uids_in_commit_range(t, 0, 3, 4).unwrap().len(), 4);
    assert_eq!(turn_uids_in_commit_range(t, 0, 3, 99).unwrap().len(), 4);
}

// ── S3b：lag 换源到 pass 表的三个读法 ──────────────────────────────────────────

/// `count_terminal_turns` 按**轮**去重：同一轮的两条终态 pass 只算一轮（lag 的分母是轮，
/// 不是格）；`running`/`pending` 一行都不算。`last_terminal_at_ms` 取终态的
/// `updated_at_ms` 最大者，不看非终态行的更晚时间。
#[test]
fn terminal_turn_readers_count_only_terminal_rows_and_dedup_by_turn() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let l = root.ledger();
    // turn-a：两个 pass 都到终态 ⇒ 仍只算 1 轮（DISTINCT turn_uid）。
    pass_state::mark_terminal(l, key("turn-a", "naming", ""), PassState::Done, None, 1_000)
        .unwrap();
    pass_state::mark_terminal(l, key("turn-a", "fact", "decision"), PassState::Empty, None, 1_500)
        .unwrap();
    // turn-b：quarantined 也是终态。
    pass_state::mark_terminal(
        l, key("turn-b", "event", ""), PassState::Quarantined, Some("schema"), 2_000,
    )
    .unwrap();
    // turn-c/d：running 与 pending 都不是终态 ⇒ 不进计数，也不进最后时刻（3_100 > 2_000）。
    pass_state::mark_running(l, key("turn-c", "naming", ""), "fp-c", "run-c", 3_000).unwrap();
    pass_state::note_attempt(l, key("turn-d", "naming", ""), 60_000, 3_100).unwrap();

    assert_eq!(count_terminal_turns(l).unwrap(), 2, "两条轮有终态行（四条终态格去重成两条轮）");
    assert_eq!(last_terminal_at_ms(l).unwrap(), Some(2_000),
        "最后终态时刻是 turn-b 的 2_000，不是非终态行更晚的 3_100");
    assert_eq!(
        count_terminal_by_pass(l).unwrap(),
        vec![
            ("event".to_string(), 1),
            ("fact".to_string(), 1),
            ("naming".to_string(), 1),
        ],
        "按 pass 数**轮**：naming 的 running 行不算，turn-a 的两个 pass 各记 1"
    );
}

/// 空表的三个读法是 0 / `None` / 空表——"一行都还没跑过"是读数，不是错误。
#[test]
fn terminal_turn_readers_on_empty_table_are_zero_none_and_empty() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    assert_eq!(count_terminal_turns(root.ledger()).unwrap(), 0);
    assert_eq!(last_terminal_at_ms(root.ledger()).unwrap(), None);
    assert!(count_terminal_by_pass(root.ledger()).unwrap().is_empty());
}

/// SQL 里的 slug 清单必须与 `PassState::is_terminal` 的五个值逐字一致。
///
/// 断言从 `TERMINAL_STATES` 里拆出 slug，再拿 `mark_terminal` 逐条试写：这个入口只接受
/// `is_terminal` 为真的状态，所以"SQL 里写的每个 slug 都能写终态"正是那条镜像关系。
/// 另外两句堵住两边：数目必须恰是五个，`pending`/`running` 一个都不许混进来；最后一遍
/// 让**除 `stale` 外**的每个 slug 过一遍 `note_attempt`，钉住"`CASE` 只从这一个常量派生"。
#[test]
fn terminal_states_sql_slugs_are_exactly_the_five_terminal_states() {
    let slugs: Vec<&str> = TERMINAL_STATES
        .trim_start_matches('(')
        .trim_end_matches(')')
        .split(',')
        .map(|s| s.trim().trim_matches('\''))
        .collect();
    assert_eq!(slugs.len(), 5, "终态恰五个：{TERMINAL_STATES}");
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    for slug in &slugs {
        let state = PassState::parse(slug)
            .unwrap_or_else(|| panic!("SQL 里的 {slug} 不在 PassState::parse 的词表里"));
        pass_state::mark_terminal(
            root.ledger(), key("turn-terminal-slug", "naming", ""), state, None, 1,
        )
        .unwrap_or_else(|e| panic!("SQL 里的 {slug} 不是 is_terminal 认的终态：{e}"));
    }
    for not_terminal in [PassState::Pending, PassState::Running] {
        assert!(!slugs.contains(&not_terminal.slug()),
            "非终态 {} 混进了终态 SQL 清单", not_terminal.slug());
    }
    // `note_attempt` 的 `CASE` 读的是「这份清单**减 `stale`**」：迟到的退避登记对除 `stale`
    // 外的终态一个都不许改回 `pending`；`stale` 的回归是唯一例外，由下一条用例单独钉住。
    for slug in slugs.iter().filter(|s| **s != PassState::Stale.slug()) {
        let state = PassState::parse(slug).unwrap();
        let k = key("turn-terminal-note", "naming", "");
        pass_state::mark_terminal(root.ledger(), k, state, None, 2).unwrap();
        pass_state::note_attempt(root.ledger(), k, 60_000, 3).unwrap();
        assert_eq!(
            pass_state::get(root.ledger(), k).unwrap().unwrap().state, state,
            "终态 {slug} 被一次迟到的退避登记改回 pending 了"
        );
    }
}

/// `stale` 是唯一退回 `Pending` 的终态：退避登记必须让 `next_retry_at_ms` 接手重跑，
/// 否则 `should_run` 对 `stale` 的无条件点头会让持续失败的轮每轮重选、退避形同虚设。
///
/// 夹具先 `mark_running` 落指纹、再 `mark_terminal(stale)`（终态写保留指纹），于是
/// `should_run` 走的是退避分支而不是"指纹不同 ⇒ 跑"那一支。
/// 变异：把 `note_attempt` 的 `CASE` 退回五格终态 ⇒ 状态仍停在 `stale`，第一条断言红；
/// `should_run` 同时无条件放行，窗口内那条断言也红。
#[test]
fn stale_经过一次退避登记回到_pending_让退避生效() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let l = root.ledger();
    let k = key("turn-stale-note", "naming", "");
    pass_state::mark_running(l, k, "fp-1", "run-1", 1_000).unwrap();
    pass_state::mark_terminal(l, k, PassState::Stale, Some("fp"), 2_000).unwrap();
    pass_state::note_attempt(l, k, 60_000, 10_000).unwrap();
    let rec = record_of(&root, k);
    assert_eq!(rec.state, PassState::Pending,
        "stale 必须退回 pending：留在 stale 就没有退避可言");
    assert_eq!(rec.next_retry_at_ms, Some(70_000), "退避时刻没有落下");
    assert_eq!(rec.attempt, 1, "退避登记仍记一次尝试");
    assert!(!pass_state::should_run(l, k, "fp-1", 69_999).unwrap(),
        "退避窗口内不许再跑（stale 的无条件点头必须已经收回）");
    assert!(pass_state::should_run(l, k, "fp-1", 70_000).unwrap(), "到期必须放行");
}

/// 判据 9：`reclaim_running` 只回收过期的 running 行；终态与新鲜 running 不动；
/// 第二次调用幂等；attempt 递增、退避可配、指纹/run_id 保留。
#[test]
fn reclaim_running_moves_only_stale_running_to_pending() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let stale = key("turn-stale", "state", "");
    let fresh = key("turn-fresh", "state", "");
    let done = key("turn-done", "state", "");

    pass_state::mark_running(root.ledger(), stale, "fp-1", "run-1", 1_000).unwrap();
    // cutoff 是闭区间（`updated_at_ms <= now - older_than_ms`，now=6_000、older=1_000 ⇒ 5_000）：
    // fresh 必须**晚于** cutoff，正好等于 5_000 的那一行按判据会被回收。
    pass_state::mark_running(root.ledger(), fresh, "fp-1", "run-1", 5_001).unwrap();
    pass_state::mark_terminal(root.ledger(), done, PassState::Done, Some("ok"), 1_000).unwrap();

    let n = pass_state::reclaim_running(root.ledger(), "state", 1_000, 0, 6_000).unwrap();
    assert_eq!(n, 1, "只回收 stale，fresh 与终态不动");

    let r = record_of(&root, stale);
    assert_eq!(r.state, PassState::Pending);
    assert_eq!(r.attempt, 1);
    assert_eq!(r.next_retry_at_ms, Some(6_000));
    assert_eq!(r.fingerprint.as_deref(), Some("fp-1"), "回收不许抹掉指纹");
    assert_eq!(r.run_id.as_deref(), Some("run-1"), "回收不许抹掉 run_id");

    assert_eq!(record_of(&root, fresh).state, PassState::Running);
    assert_eq!(record_of(&root, done).state, PassState::Done);
    assert_eq!(
        pass_state::reclaim_running(root.ledger(), "state", 1_000, 0, 6_000).unwrap(),
        0,
        "第二次调用幂等（同一窗口）"
    );
}

/// W7b：`reset_terminal_to_stale` 只回拨终态行，三项计数各归各的格子。
///
/// 夹具里一轮 `state` 有两格：一族终态（`done`，带指纹/run_id/attempt）与一格 `pending`
/// （退避中）。回拨后终态格变 `stale(理由)`、退避清空、指纹与 run_id 原样；`pending` 格
/// 一个字节都不动（状态与 `next_retry_at_ms` 逐字保留）。未知轮号进 `not_found` 而不是
/// 错误；事实族的一轮两族两行各自成一行计数。
#[test]
fn reset_terminal_to_stale_moves_only_terminal_rows_and_counts_every_cell() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let l = root.ledger();
    // 一轮 `state`：done 行（真终态）+ pending 行（退避中，不许动）。
    let done = key("turn-reset-done", "state", "");
    let pending = key("turn-reset-pending", "state", "");
    pass_state::mark_running(l, done, "fp-1", "run-1", 1_000).unwrap();
    pass_state::mark_terminal(l, done, PassState::Done, Some("ok"), 2_000).unwrap();
    pass_state::mark_running(l, pending, "fp-2", "run-2", 3_000).unwrap();
    pass_state::note_attempt(l, pending, 60_000, 4_000).unwrap();
    let pending_before = record_of(&root, pending);
    // fact 的一轮两族：两行都是 empty，回拨要按**行**计。
    for family in ["decision", "promise"] {
        pass_state::mark_terminal(
            l, key("turn-reset-fact", "fact", family), PassState::Empty, Some("none"), 5_000,
        )
        .unwrap();
    }

    let turns: Vec<String> = ["turn-reset-done", "turn-reset-pending", "turn-reset-fact",
                             "turn-reset-absent"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let counts = pass_state::reset_terminal_to_stale(l, "state", &turns, "operator_reset", 6_000)
        .unwrap();
    assert_eq!(counts, pass_state::ResetCounts {
        reset: 1,
        skipped_non_terminal: 1,
        not_found: 2,
    }, "state 只有 done 那一格可回拨；fact 的两行不在这条 pass 上，缺的那一轮也记 not_found");

    let r = record_of(&root, done);
    assert_eq!(r.state, PassState::Stale);
    assert_eq!(r.reason.as_deref(), Some("operator_reset"));
    assert_eq!(r.next_retry_at_ms, None, "stale 行没有下一次重试");
    assert_eq!(r.updated_at_ms, 6_000);
    assert_eq!(r.fingerprint.as_deref(), Some("fp-1"), "指纹是选批走 stale 那一支的前提");
    assert_eq!(r.run_id.as_deref(), Some("run-1"), "回拨不许抹掉 run_id");
    // 指纹没换 ⇒ `should_run` 走的是 `Stale => true`，不是"指纹不同 ⇒ true"。
    assert!(pass_state::should_run(l, done, "fp-1", 6_000).unwrap(), "回拨后必须重新被选中");
    assert_eq!(record_of(&root, pending), pending_before,
        "pending 行的状态与退避时刻必须逐字不变");

    // fact 的两族各一行：按行计 2。
    let fact = pass_state::reset_terminal_to_stale(
        l, "fact", &["turn-reset-fact".to_string()], "operator_reset", 7_000,
    )
    .unwrap();
    assert_eq!(fact, pass_state::ResetCounts { reset: 2, ..Default::default() },
        "与 state 无关：fact 的两族在这一条 pass 上各一行");
    // 再跑一遍：两行都已是 stale，原样不动（`reset=0`、跳过计 2），读数不随次数变。
    let again = pass_state::reset_terminal_to_stale(
        l, "fact", &["turn-reset-fact".to_string()], "operator_reset", 8_000,
    )
    .unwrap();
    assert_eq!(again, pass_state::ResetCounts { skipped_non_terminal: 2, ..Default::default() });
    assert_eq!(row_count(&root), 4, "回拨不增删行");

    // 形状闸：空理由响亮；未知 pass 响亮；空清单是空计数（不是错误）。
    let e = pass_state::reset_terminal_to_stale(l, "state", &turns, "", 9_000).unwrap_err();
    assert!(matches!(e, StoreError::Sql(_)), "{e}");
    let e = pass_state::reset_terminal_to_stale(l, "rows", &turns, "r", 9_000).unwrap_err();
    assert!(matches!(e, StoreError::Sql(_)), "{e}");
    assert_eq!(
        pass_state::reset_terminal_to_stale(l, "state", &[], "r", 9_000).unwrap(),
        pass_state::ResetCounts::default()
    );
}
