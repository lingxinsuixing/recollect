//! S3a-1：双审队列存储 API 的行为执法（设计 `2026-10-26-review-queue-s3` §3/§4）。
//!
//! 覆盖：入队字段与载荷指纹、唯一键幂等、键/长度校验、状态转换全矩阵（合法 + 非法）、
//! 判定只追加、list/get 过滤、stats/prune、只读柄拒写。

use rc_memory::review::{
    claim, decisions, enqueue, get, has_stage, list, max_decision_id, note_attempt, prune,
    record_decision, release, runnable, stats, Decider, Decision, EnqueueOutcome, NewItem,
    ReviewStage, ReviewState, ReviewVerdict,
};
use rc_memory::MemError;
use rc_store::{AgentRoot, MemoryHandle, Root};
use sha2::{Digest, Sha256};

const OWNER: &str = "{\"agent\":\"a\"}";

fn item(pass_id: &str, family: &str, payload: &str) -> NewItem {
    NewItem {
        source: "distill".into(),
        pass_id: pass_id.into(),
        family: family.into(),
        session_key: "sess-a".into(),
        device_id: "dev-a".into(),
        turn_uids_json: "[\"turn-1\"]".into(),
        owner_json: OWNER.into(),
        payload_json: payload.into(),
        batch_key: String::new(),
    }
}

fn decision(
    stage: ReviewStage,
    verdict: ReviewVerdict,
    decider: Decider,
    reason_code: &str,
) -> Decision {
    Decision {
        stage,
        verdict,
        reason_code: reason_code.into(),
        rules_version: "review-gate-v0.1".into(),
        model: String::new(),
        prompt_version: String::new(),
        input_sha256: "0".repeat(64),
        review_input_sha256: String::new(),
        decider,
        detail: String::new(),
    }
}

fn expected_sha(pass_id: &str, family: &str, owner_json: &str, payload_json: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"review.v1\0");
    for part in [pass_id, family, owner_json] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    h.update(payload_json.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn count(m: &rc_store::MemoryHandle, sql: &str) -> i64 {
    m.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn pending(m: &rc_store::MemoryHandle, pass: &str, family: &str, payload: &str) -> i64 {
    enqueue(m, &item(pass, family, payload), 100).unwrap().id
}

fn approve(m: &rc_store::MemoryHandle, id: i64, now_ms: i64) -> ReviewState {
    record_decision(
        m,
        id,
        &decision(
            ReviewStage::Review,
            ReviewVerdict::Pass,
            Decider::Center,
            "review_ok",
        ),
        now_ms,
    )
    .unwrap()
}

fn reject(m: &rc_store::MemoryHandle, id: i64, now_ms: i64) -> ReviewState {
    record_decision(
        m,
        id,
        &decision(
            ReviewStage::Review,
            ReviewVerdict::Reject,
            Decider::Center,
            "not_supported",
        ),
        now_ms,
    )
    .unwrap()
}

#[test]
fn enqueue_writes_all_fields_and_dedupes_on_the_payload_key() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let it = item(
        "fact",
        "decision",
        "{\"schema\":\"review.pass.v1\",\"text\":\"x\"}",
    );
    let first = enqueue(m, &it, 1_000).unwrap();
    assert!(first.fresh);
    assert_eq!(first.state, ReviewState::PendingReview);
    assert_eq!(
        first.payload_sha256,
        expected_sha("fact", "decision", OWNER, &it.payload_json)
    );
    assert_eq!(first.payload_sha256.len(), 64);

    let full = get(m, first.id)
        .unwrap()
        .expect("刚入队的 item 必须读得回来");
    assert_eq!(full.meta.id, first.id);
    assert_eq!(full.meta.source, "distill");
    assert_eq!(full.meta.pass_id, "fact");
    assert_eq!(full.meta.family, "decision");
    assert_eq!(full.meta.session_key, "sess-a");
    assert_eq!(full.meta.device_id, "dev-a");
    assert_eq!(full.meta.state, ReviewState::PendingReview);
    assert_eq!(full.meta.program_code, None);
    assert_eq!(full.meta.attempt, 0);
    assert_eq!(full.meta.created_at_ms, 1_000);
    assert_eq!(full.meta.updated_at_ms, 1_000);
    assert_eq!(full.meta.applied_at_ms, None);
    assert_eq!(full.turn_uids_json, "[\"turn-1\"]");
    assert_eq!(full.owner_json, OWNER);
    assert_eq!(full.payload_json, it.payload_json);
    assert_eq!(full.payload_sha256, first.payload_sha256);
    assert_eq!(full.conflict_json, None);

    // 同一载荷 + pass/family/owner 再入队：同 id、fresh=false、不覆盖既有行。
    let again = NewItem {
        session_key: "sess-b".into(),
        device_id: "dev-b".into(),
        ..it.clone()
    };
    let second = enqueue(m, &again, 2_000).unwrap();
    assert_eq!(second.id, first.id);
    assert!(!second.fresh);
    assert_eq!(second.state, ReviewState::PendingReview);
    assert_eq!(second.payload_sha256, first.payload_sha256);
    assert_eq!(count(m, "SELECT count(*) FROM review_item"), 1);
    assert_eq!(
        get(m, first.id).unwrap().unwrap().meta.updated_at_ms,
        1_000,
        "重复入队不得覆盖既有行"
    );

    // 指纹的 preimage 含 owner：换 owner 就是另一条 item。
    let other_owner = NewItem {
        owner_json: "{\"agent\":\"b\"}".into(),
        ..it.clone()
    };
    let third = enqueue(m, &other_owner, 3_000).unwrap();
    assert!(third.fresh);
    assert_ne!(third.id, first.id);
    assert_ne!(third.payload_sha256, first.payload_sha256);
}

#[test]
fn enqueue_rejects_bad_keys_and_oversize_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    let cases: Vec<(&str, NewItem)> = vec![
        (
            "source",
            NewItem {
                source: "bogus".into(),
                ..item("fact", "decision", "{}")
            },
        ),
        (
            "pass_id",
            NewItem {
                pass_id: "bogus".into(),
                ..item("fact", "decision", "{}")
            },
        ),
        (
            "family",
            NewItem {
                pass_id: "naming".into(),
                family: "decision".into(),
                ..item("fact", "decision", "{}")
            },
        ),
        (
            "owner_json",
            NewItem {
                owner_json: "  ".into(),
                ..item("fact", "decision", "{}")
            },
        ),
        (
            "turn_uids_json",
            NewItem {
                turn_uids_json: "".into(),
                ..item("fact", "decision", "{}")
            },
        ),
    ];
    for (what, bad) in cases {
        match enqueue(m, &bad, 1) {
            Err(MemError::Sql(msg)) => {
                assert!(
                    msg.starts_with("review: invalid"),
                    "{what}: 错误短语不对：{msg}"
                )
            }
            other => panic!("{what} 必须被拒，实际：{other:?}"),
        }
    }
    // 边界：64_000 字符收下，64_001 拒绝。
    assert!(enqueue(m, &item("time", "", &"x".repeat(64_000)), 1).is_ok());
    match enqueue(m, &item("time", "", &"x".repeat(64_001)), 1) {
        Err(MemError::Sql(msg)) => assert!(msg.starts_with("review: invalid"), "{msg}"),
        other => panic!("超长载荷必须被拒，实际：{other:?}"),
    }
    // fact 的 family 非空合法（其余 pass 必须空串）。
    assert!(enqueue(m, &item("fact", "promise", "{}"), 1).is_ok());
    assert_eq!(count(m, "SELECT count(*) FROM review_item"), 2);
}

#[test]
fn state_machine_accepts_the_legal_transitions() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    // stage=program：pending + pass ⇒ 留在 pending_review，program_code 为 NULL。
    let program_pass = pending(m, "naming", "", "program-pass");
    assert_eq!(
        record_decision(
            m,
            program_pass,
            &decision(
                ReviewStage::Program,
                ReviewVerdict::Pass,
                Decider::Program,
                "payload_ok"
            ),
            200,
        )
        .unwrap(),
        ReviewState::PendingReview
    );
    assert_eq!(
        get(m, program_pass).unwrap().unwrap().meta.program_code,
        None
    );

    // stage=program：非终态 + reject ⇒ rejected，program_code 记失败码。
    let program_reject = pending(m, "naming", "", "program-reject");
    assert_eq!(
        record_decision(
            m,
            program_reject,
            &decision(
                ReviewStage::Program,
                ReviewVerdict::Reject,
                Decider::Program,
                "payload_shape"
            ),
            210,
        )
        .unwrap(),
        ReviewState::Rejected
    );
    assert_eq!(
        get(m, program_reject)
            .unwrap()
            .unwrap()
            .meta
            .program_code
            .as_deref(),
        Some("payload_shape")
    );

    // stage=review：pending 的三条出路。
    let reviewed_pass = pending(m, "fact", "decision", "review-pass");
    assert_eq!(approve(m, reviewed_pass, 300), ReviewState::Approved);
    let reviewed_reject = pending(m, "fact", "decision", "review-reject");
    assert_eq!(reject(m, reviewed_reject, 310), ReviewState::Rejected);
    let reviewed_uncertain = pending(m, "fact", "decision", "review-uncertain");
    assert_eq!(
        record_decision(
            m,
            reviewed_uncertain,
            &decision(
                ReviewStage::Review,
                ReviewVerdict::Uncertain,
                Decider::Center,
                "review_uncertain"
            ),
            320,
        )
        .unwrap(),
        ReviewState::Quarantined
    );

    // stage=apply：approved + pass ⇒ applied，applied_at_ms 落 now。
    let applied = pending(m, "event", "", "apply-pass");
    approve(m, applied, 400);
    assert_eq!(
        record_decision(
            m,
            applied,
            &decision(
                ReviewStage::Apply,
                ReviewVerdict::Pass,
                Decider::Program,
                "write_ok"
            ),
            500,
        )
        .unwrap(),
        ReviewState::Applied
    );
    let applied_full = get(m, applied).unwrap().unwrap();
    assert_eq!(applied_full.meta.state, ReviewState::Applied);
    assert_eq!(applied_full.meta.applied_at_ms, Some(500));

    // stage=apply：approved + reject ⇒ 留在 approved，attempt+1，退避 60s × 4^n。
    let retried = pending(m, "event", "", "apply-retry");
    approve(m, retried, 600);
    assert_eq!(
        record_decision(
            m,
            retried,
            &decision(
                ReviewStage::Apply,
                ReviewVerdict::Reject,
                Decider::Program,
                "write_failed"
            ),
            1_000,
        )
        .unwrap(),
        ReviewState::Approved
    );
    let after_first = get(m, retried).unwrap().unwrap().meta;
    assert_eq!(after_first.attempt, 1);
    assert_eq!(after_first.applied_at_ms, None);
    let first_next_retry: i64 = m
        .conn()
        .query_row(
            "SELECT next_retry_at_ms FROM review_item WHERE id=?1",
            [retried],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(first_next_retry, 1_000 + 60_000);
    assert_eq!(
        record_decision(
            m,
            retried,
            &decision(
                ReviewStage::Apply,
                ReviewVerdict::Reject,
                Decider::Program,
                "write_failed"
            ),
            61_000,
        )
        .unwrap(),
        ReviewState::Approved
    );
    let after_second = get(m, retried).unwrap().unwrap().meta;
    assert_eq!(after_second.attempt, 2);
    let second_next_retry: i64 = m
        .conn()
        .query_row(
            "SELECT next_retry_at_ms FROM review_item WHERE id=?1",
            [retried],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(second_next_retry, 61_000 + 240_000);

    // stage=human：pending_review|quarantined|approved 三态 + 人工判定。
    let human_pass = pending(m, "rows", "", "human-pass");
    assert_eq!(
        record_decision(
            m,
            human_pass,
            &decision(
                ReviewStage::Human,
                ReviewVerdict::Pass,
                Decider::Human,
                "operator_ok"
            ),
            700,
        )
        .unwrap(),
        ReviewState::Approved
    );
    let human_from_quarantine = pending(m, "rows", "", "human-from-quarantine");
    record_decision(
        m,
        human_from_quarantine,
        &decision(
            ReviewStage::Review,
            ReviewVerdict::Uncertain,
            Decider::Center,
            "review_uncertain",
        ),
        710,
    )
    .unwrap();
    assert_eq!(
        record_decision(
            m,
            human_from_quarantine,
            &decision(
                ReviewStage::Human,
                ReviewVerdict::Pass,
                Decider::Human,
                "operator_ok"
            ),
            720,
        )
        .unwrap(),
        ReviewState::Approved
    );
    let human_reject = pending(m, "rows", "", "human-reject");
    approve(m, human_reject, 730);
    assert_eq!(
        record_decision(
            m,
            human_reject,
            &decision(
                ReviewStage::Human,
                ReviewVerdict::Reject,
                Decider::Human,
                "operator_reject"
            ),
            740,
        )
        .unwrap(),
        ReviewState::Rejected
    );
}

#[test]
fn state_machine_refuses_illegal_transitions_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    // rejected → applied
    let rejected = pending(m, "fact", "decision", "illegal-rejected-apply");
    reject(m, rejected, 100);
    // pending → applied
    let pending_item = pending(m, "fact", "decision", "illegal-pending-apply");
    // approved → apply pass（第一次合法，制造 applied）
    let applied = pending(m, "fact", "decision", "illegal-double-apply");
    approve(m, applied, 110);
    record_decision(
        m,
        applied,
        &decision(
            ReviewStage::Apply,
            ReviewVerdict::Pass,
            Decider::Program,
            "write_ok",
        ),
        120,
    )
    .unwrap();
    // human 用 center decider
    let human_center = pending(m, "fact", "decision", "illegal-human-center");
    // review 只能从 pending_review
    let approved = pending(m, "fact", "decision", "illegal-review-approved");
    approve(m, approved, 130);
    // program pass 只能从 pending_review
    let program_on_approved = approved;

    let cases: Vec<(&str, i64, Decision)> = vec![
        (
            "rejected→applied",
            rejected,
            decision(
                ReviewStage::Apply,
                ReviewVerdict::Pass,
                Decider::Program,
                "write_ok",
            ),
        ),
        (
            "pending→applied",
            pending_item,
            decision(
                ReviewStage::Apply,
                ReviewVerdict::Pass,
                Decider::Program,
                "write_ok",
            ),
        ),
        (
            "double apply",
            applied,
            decision(
                ReviewStage::Apply,
                ReviewVerdict::Pass,
                Decider::Program,
                "write_ok",
            ),
        ),
        (
            "human with center decider",
            human_center,
            decision(
                ReviewStage::Human,
                ReviewVerdict::Pass,
                Decider::Center,
                "operator_ok",
            ),
        ),
        (
            "review on approved",
            approved,
            decision(
                ReviewStage::Review,
                ReviewVerdict::Pass,
                Decider::Center,
                "review_ok",
            ),
        ),
        (
            "program pass on approved",
            program_on_approved,
            decision(
                ReviewStage::Program,
                ReviewVerdict::Pass,
                Decider::Program,
                "payload_ok",
            ),
        ),
        (
            "human uncertain",
            human_center,
            decision(
                ReviewStage::Human,
                ReviewVerdict::Uncertain,
                Decider::Human,
                "operator_ok",
            ),
        ),
    ];

    for (what, id, d) in cases {
        let before_state = get(m, id).unwrap().unwrap().meta.state;
        let before = count(m, "SELECT count(*) FROM review_decision");
        match record_decision(m, id, &d, 200) {
            Err(MemError::Sql(msg)) => assert!(
                msg.starts_with("review: illegal transition"),
                "{what}: 错误短语不对：{msg}"
            ),
            other => panic!("{what} 必须被判为非法转换，实际：{other:?}"),
        }
        assert_eq!(
            get(m, id).unwrap().unwrap().meta.state,
            before_state,
            "{what} 改了状态"
        );
        assert_eq!(
            count(m, "SELECT count(*) FROM review_decision"),
            before,
            "{what} 落了判定行"
        );
    }
}

/// H2：程序侧的 `uncertain` 是 pending_review/approved 上的合法边 ⇒ `quarantined`。
/// 原因码落 `program_code`（与 reject 同格：非 pass 的程序裁定把码留在 item 上）。
#[test]
fn program_uncertain_quarantines_pending_and_approved() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    let from_pending = pending(m, "fact", "decision", "program-uncertain-pending");
    assert_eq!(
        record_decision(
            m,
            from_pending,
            &decision(
                ReviewStage::Program,
                ReviewVerdict::Uncertain,
                Decider::Program,
                "review_payload_gap",
            ),
            300,
        )
        .unwrap(),
        ReviewState::Quarantined
    );
    assert_eq!(
        get(m, from_pending).unwrap().unwrap().meta.program_code.as_deref(),
        Some("review_payload_gap"),
        "隔离的原因码必须留在 item 上（review show/list 不必翻台账就能看到）"
    );

    let from_approved = pending(m, "fact", "decision", "program-uncertain-approved");
    assert_eq!(approve(m, from_approved, 310), ReviewState::Approved);
    assert_eq!(
        record_decision(
            m,
            from_approved,
            &decision(
                ReviewStage::Program,
                ReviewVerdict::Uncertain,
                Decider::Program,
                "review_verdicts_missing",
            ),
            320,
        )
        .unwrap(),
        ReviewState::Quarantined
    );
    let row = get(m, from_approved).unwrap().unwrap().meta;
    assert_eq!(row.state, ReviewState::Quarantined);
    assert_eq!(row.program_code.as_deref(), Some("review_verdicts_missing"));
    assert_eq!(row.applied_at_ms, None, "隔离不是写库");
}

/// H2：这条新边**只**从 pending_review/approved 出发。applied/stale（终态）与
/// quarantined（复审走 review 阶段）上仍是非法转换：不改状态、不落判定行。
#[test]
fn program_uncertain_is_illegal_on_applied_stale_and_quarantined() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    // applied：approve + apply pass 之后。
    let applied = pending(m, "event", "", "program-uncertain-applied");
    approve(m, applied, 400);
    record_decision(
        m,
        applied,
        &decision(
            ReviewStage::Apply,
            ReviewVerdict::Pass,
            Decider::Program,
            "write_ok",
        ),
        410,
    )
    .unwrap();

    // stale：同 batch_key 换载荷由顶替推进（H1c 的真实路径）。
    let key = rc_memory::review::batch_key("fact", "decision", "sess-a", "dev-a", &["turn-1"]);
    let stale = enqueue(m, &batch_item("fact", "decision", "stale-v1", &key), 420)
        .unwrap()
        .id;
    let _ = enqueue(m, &batch_item("fact", "decision", "stale-v2", &key), 430).unwrap();

    // quarantined：review/uncertain 已经隔离过的那条。
    let quarantined = pending(m, "fact", "decision", "program-uncertain-quarantined");
    record_decision(
        m,
        quarantined,
        &decision(
            ReviewStage::Review,
            ReviewVerdict::Uncertain,
            Decider::Center,
            "review_uncertain",
        ),
        440,
    )
    .unwrap();

    for (what, id) in [
        ("applied", applied),
        ("stale", stale),
        ("quarantined", quarantined),
    ] {
        let before_state = get(m, id).unwrap().unwrap().meta.state;
        assert_eq!(before_state, ReviewState::parse(what).unwrap(), "{what}: 夹具状态");
        let before = count(m, "SELECT count(*) FROM review_decision");
        match record_decision(
            m,
            id,
            &decision(
                ReviewStage::Program,
                ReviewVerdict::Uncertain,
                Decider::Program,
                "review_payload_gap",
            ),
            450,
        ) {
            Err(MemError::Sql(msg)) => assert!(
                msg.starts_with("review: illegal transition"),
                "{what}: 错误短语不对：{msg}"
            ),
            other => panic!("{what} 上的 program/uncertain 必须非法，实际：{other:?}"),
        }
        assert_eq!(
            get(m, id).unwrap().unwrap().meta.state,
            before_state,
            "{what}: 改了状态"
        );
        assert_eq!(
            count(m, "SELECT count(*) FROM review_decision"),
            before,
            "{what}: 落了判定行"
        );
    }
}

#[test]
fn decisions_are_append_only_and_validated() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let id = pending(m, "fact", "decision", "append-only");

    record_decision(
        m,
        id,
        &decision(
            ReviewStage::Program,
            ReviewVerdict::Pass,
            Decider::Program,
            "payload_ok",
        ),
        100,
    )
    .unwrap();
    approve(m, id, 200);

    let rows: Vec<(String, String, String)> = m
        .conn()
        .prepare("SELECT stage, verdict, decider FROM review_decision WHERE item_id=?1 ORDER BY id")
        .unwrap()
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            (
                "program".to_string(),
                "pass".to_string(),
                "program".to_string()
            ),
            (
                "review".to_string(),
                "pass".to_string(),
                "center".to_string()
            ),
        ],
        "判定台账必须两条都在、顺序稳定"
    );

    let long_detail = {
        let mut d = decision(
            ReviewStage::Review,
            ReviewVerdict::Pass,
            Decider::Center,
            "review_ok",
        );
        d.detail = "x".repeat(513);
        d
    };
    let bad_code = decision(
        ReviewStage::Review,
        ReviewVerdict::Pass,
        Decider::Center,
        "Bad Code",
    );
    let empty_code = decision(
        ReviewStage::Review,
        ReviewVerdict::Pass,
        Decider::Center,
        "",
    );
    let empty_rules = {
        let mut d = decision(
            ReviewStage::Review,
            ReviewVerdict::Pass,
            Decider::Center,
            "review_ok",
        );
        d.rules_version = "  ".into();
        d
    };
    for (what, d) in [
        ("detail > 512", long_detail),
        ("bad reason_code", bad_code),
        ("empty reason_code", empty_code),
        ("empty rules_version", empty_rules),
    ] {
        match record_decision(m, id, &d, 300) {
            Err(MemError::Sql(msg)) => assert!(msg.starts_with("review: invalid"), "{what}: {msg}"),
            other => panic!("{what} 必须被拒，实际：{other:?}"),
        }
    }
    assert_eq!(
        count(m, "SELECT count(*) FROM review_decision"),
        2,
        "被拒的判定不许落行"
    );
    assert_eq!(
        get(m, id).unwrap().unwrap().meta.state,
        ReviewState::Approved
    );
}

#[test]
fn list_filters_by_state_and_pass_and_get_returns_the_payload() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    let a = pending(m, "fact", "decision", "list-a");
    let b = pending(m, "fact", "promise", "list-b");
    let c = pending(m, "time", "", "list-c");
    approve(m, a, 200);
    reject(m, b, 210);

    let ids = |items: Vec<rc_memory::review::ItemMeta>| -> Vec<i64> {
        items.into_iter().map(|i| i.id).collect()
    };
    assert_eq!(ids(list(m, None, None, 0).unwrap()), vec![a, b, c]);
    assert_eq!(
        ids(list(m, Some(ReviewState::Approved), None, 10).unwrap()),
        vec![a]
    );
    assert_eq!(ids(list(m, None, Some("time"), 10).unwrap()), vec![c]);
    assert_eq!(ids(list(m, None, None, 2).unwrap()), vec![a, b]);
    assert!(list(m, Some(ReviewState::Stale), None, 10)
        .unwrap()
        .is_empty());

    let full = get(m, c)
        .unwrap()
        .expect("list 里的 item 必须能按 id 取全量");
    assert_eq!(full.meta.id, c);
    assert_eq!(full.payload_json, "list-c");
    assert_eq!(full.turn_uids_json, "[\"turn-1\"]");
    assert_eq!(full.owner_json, OWNER);
    assert!(
        get(m, 9_999).unwrap().is_none(),
        "不存在的 id 必须返回 None"
    );
}

#[test]
fn stats_counts_and_prune_clear_only_terminal_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    let pending_item = pending(m, "fact", "decision", "stats-pending");
    let approved = pending(m, "fact", "decision", "stats-approved");
    approve(m, approved, 1_100);
    let applied = pending(m, "time", "", "stats-applied");
    approve(m, applied, 1_200);
    record_decision(
        m,
        applied,
        &decision(
            ReviewStage::Apply,
            ReviewVerdict::Pass,
            Decider::Program,
            "write_ok",
        ),
        1_300,
    )
    .unwrap();
    let rejected = pending(m, "naming", "", "stats-rejected");
    reject(m, rejected, 1_400);
    let quarantined = pending(m, "event", "", "stats-quarantined");
    record_decision(
        m,
        quarantined,
        &decision(
            ReviewStage::Review,
            ReviewVerdict::Uncertain,
            Decider::Center,
            "review_uncertain",
        ),
        1_500,
    )
    .unwrap();
    let stale = pending(m, "rows", "", "stats-stale");
    m.conn()
        .execute(
            "UPDATE review_item SET state='stale', updated_at_ms=1_700 WHERE id=?1",
            [stale],
        )
        .unwrap();

    let st = stats(m, 10_000).unwrap();
    assert_eq!(st.pending_review, 1);
    assert_eq!(st.approved, 1);
    assert_eq!(st.applied, 1);
    assert_eq!(st.rejected, 1);
    assert_eq!(st.quarantined, 1);
    assert_eq!(st.stale, 1);
    assert_eq!(st.oldest_pending_age_ms, Some(10_000 - 100));
    let by: std::collections::BTreeMap<String, [i64; 6]> = st.by_pass.into_iter().collect();
    assert_eq!(by["event"], [0, 0, 0, 0, 1, 0]);
    assert_eq!(by["fact"], [1, 1, 0, 0, 0, 0]);
    assert_eq!(by["naming"], [0, 0, 0, 1, 0, 0]);
    assert_eq!(by["rows"], [0, 0, 0, 0, 0, 1]);
    assert_eq!(by["time"], [0, 0, 1, 0, 0, 0]);

    let decisions_before = count(m, "SELECT count(*) FROM review_decision");
    assert_eq!(
        prune(m, 2_000, 20_000).unwrap(),
        3,
        "只有 applied/rejected/stale 三行该清载荷"
    );
    for id in [applied, rejected, stale] {
        let full = get(m, id).unwrap().unwrap();
        assert_eq!(full.payload_json, "", "终态载荷没清空");
        assert_eq!(full.turn_uids_json, "", "终态轮引用没清空");
        assert_eq!(full.owner_json, "", "终态归属没清空");
        assert_eq!(full.meta.updated_at_ms, 20_000);
    }
    // 判定不动：applied 有 review + apply 两条，rejected 有一条，stale 没有。
    assert_eq!(
        count(
            m,
            &format!("SELECT count(*) FROM review_decision WHERE item_id={applied}")
        ),
        2
    );
    assert_eq!(
        count(
            m,
            &format!("SELECT count(*) FROM review_decision WHERE item_id={rejected}")
        ),
        1
    );
    assert_eq!(
        count(
            m,
            &format!("SELECT count(*) FROM review_decision WHERE item_id={stale}")
        ),
        0
    );
    assert_eq!(
        get(m, applied).unwrap().unwrap().meta.state,
        ReviewState::Applied
    );
    assert_eq!(
        get(m, rejected).unwrap().unwrap().meta.state,
        ReviewState::Rejected
    );
    assert_eq!(
        get(m, stale).unwrap().unwrap().meta.state,
        ReviewState::Stale
    );
    assert_eq!(
        count(m, "SELECT count(*) FROM review_decision"),
        decisions_before
    );
    // 非终态即使 updated_at 更老也不清。
    for id in [pending_item, approved, quarantined] {
        assert_ne!(
            get(m, id).unwrap().unwrap().payload_json,
            "",
            "非终态载荷被清了"
        );
        assert_ne!(
            get(m, id).unwrap().unwrap().owner_json,
            "",
            "非终态归属被清了"
        );
    }
    assert_eq!(
        prune(m, 2_000, 21_000).unwrap(),
        0,
        "已清空的 item 不重复计"
    );
}

#[test]
fn read_only_handles_refuse_writes_but_still_read() {
    let dir = tempfile::tempdir().unwrap();
    let id = {
        let root = Root::open(dir.path()).unwrap();
        let id = pending(root.memory(), "fact", "decision", "read-only");
        approve(root.memory(), id, 200);
        id
    };
    let replica = AgentRoot::open_replica(dir.path(), &dir.path().join("memory.db")).unwrap();
    let m = replica.memory();
    assert!(m.is_read_only());

    match enqueue(m, &item("fact", "decision", "another"), 300) {
        Err(MemError::ReadOnly) => {}
        other => panic!("只读柄必须拒绝入队，实际：{other:?}"),
    }
    match record_decision(
        m,
        id,
        &decision(
            ReviewStage::Human,
            ReviewVerdict::Reject,
            Decider::Human,
            "operator_reject",
        ),
        300,
    ) {
        Err(MemError::ReadOnly) => {}
        other => panic!("只读柄必须拒绝判定，实际：{other:?}"),
    }
    match prune(m, i64::MAX, 300) {
        Err(MemError::ReadOnly) => {}
        other => panic!("只读柄必须拒绝清理，实际：{other:?}"),
    }

    assert!(get(m, id).unwrap().is_some(), "只读柄仍应能读单个 item");
    assert_eq!(list(m, None, None, 0).unwrap().len(), 1);
    assert_eq!(stats(m, 300).unwrap().approved, 1);
}

/// S3a-3a：`has_stage` 是只读查询，按 (item_id, stage) 判"这一阶段是否已表态"。
/// 它不改状态机，也不受同 item 其它阶段的行影响。
#[test]
fn has_stage_reads_only_the_matching_stage() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let id = pending(m, "fact", "promise", "has-stage");
    assert!(!has_stage(m, id, ReviewStage::Program).unwrap());
    assert!(!has_stage(m, id, ReviewStage::Review).unwrap());

    record_decision(
        m,
        id,
        &decision(
            ReviewStage::Program,
            ReviewVerdict::Pass,
            Decider::Program,
            "program_pass",
        ),
        200,
    )
    .unwrap();
    assert!(has_stage(m, id, ReviewStage::Program).unwrap());
    assert!(!has_stage(m, id, ReviewStage::Review).unwrap(), "别的阶段不受影响");

    approve(m, id, 300);
    assert!(has_stage(m, id, ReviewStage::Review).unwrap());
    assert!(!has_stage(m, id, ReviewStage::Apply).unwrap());
    // 不存在的 item 返回 false（不是错误）。
    assert!(!has_stage(m, id + 1_000, ReviewStage::Program).unwrap());
}

/// S3a-3a-fix：`decisions` 按 id 升序返回整份台账，字段逐字（detail 原样带回）。
/// 这是重试/重放重建逐条判决的唯一读入口。
#[test]
fn decisions_returns_rows_in_id_order_with_verbatim_fields() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let id = pending(m, "fact", "decision", "decisions-order");

    record_decision(
        m,
        id,
        &decision(
            ReviewStage::Program,
            ReviewVerdict::Pass,
            Decider::Program,
            "program_pass",
        ),
        100,
    )
    .unwrap();
    let mut review = decision(
        ReviewStage::Review,
        ReviewVerdict::Pass,
        Decider::Center,
        "review_mixed",
    );
    review.detail = "p=1;r=1;u=0;pass=f1;uncertain=-".into();
    record_decision(m, id, &review, 200).unwrap();
    let mut apply = decision(
        ReviewStage::Apply,
        ReviewVerdict::Pass,
        Decider::Center,
        "applied",
    );
    apply.detail = "facts=1;cards=1;mentions=2".into();
    record_decision(m, id, &apply, 300).unwrap();

    let rows = decisions(m, id).unwrap();
    assert_eq!(rows.len(), 3, "三条判定都要读回");
    assert!(
        rows[0].id < rows[1].id && rows[1].id < rows[2].id,
        "必须按 id 升序"
    );
    for row in &rows {
        assert_eq!(row.item_id, id);
    }
    assert_eq!(
        (rows[0].stage, rows[0].verdict, rows[0].reason_code.as_str(), rows[0].detail.as_str()),
        (
            ReviewStage::Program,
            ReviewVerdict::Pass,
            "program_pass",
            ""
        )
    );
    assert_eq!(
        (rows[1].stage, rows[1].verdict, rows[1].reason_code.as_str(), rows[1].detail.as_str()),
        (
            ReviewStage::Review,
            ReviewVerdict::Pass,
            "review_mixed",
            "p=1;r=1;u=0;pass=f1;uncertain=-"
        )
    );
    assert_eq!(
        (rows[2].stage, rows[2].verdict, rows[2].reason_code.as_str(), rows[2].detail.as_str()),
        (
            ReviewStage::Apply,
            ReviewVerdict::Pass,
            "applied",
            "facts=1;cards=1;mentions=2"
        )
    );
    assert_eq!(
        (rows[0].created_at_ms, rows[1].created_at_ms, rows[2].created_at_ms),
        (100, 200, 300)
    );
    assert!(
        decisions(m, id + 1_000).unwrap().is_empty(),
        "没有判定的 item 返回空表而不是错误"
    );
}

/// S3b-4a：`review` 阶段的起点从"只有 pending_review"扩到"pending_review | quarantined"。
/// 隔离项可以被复审（pass/reject/uncertain 三条出路都合法），三个终态仍拒。
#[test]
fn quarantined_items_can_be_reviewed_again_but_terminal_ones_cannot() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    let quarantine = |id: i64, now_ms: i64| {
        record_decision(
            m,
            id,
            &decision(
                ReviewStage::Review,
                ReviewVerdict::Uncertain,
                Decider::Center,
                "review_uncertain",
            ),
            now_ms,
        )
        .unwrap()
    };

    // quarantined + pass ⇒ approved
    let passed = pending(m, "fact", "decision", "re-review-pass");
    assert_eq!(quarantine(passed, 100), ReviewState::Quarantined);
    assert_eq!(approve(m, passed, 200), ReviewState::Approved);

    // quarantined + reject ⇒ rejected
    let rejected = pending(m, "fact", "decision", "re-review-reject");
    assert_eq!(quarantine(rejected, 110), ReviewState::Quarantined);
    assert_eq!(reject(m, rejected, 210), ReviewState::Rejected);

    // quarantined + uncertain ⇒ 仍 quarantined（幂等）
    let still = pending(m, "fact", "decision", "re-review-uncertain");
    assert_eq!(quarantine(still, 120), ReviewState::Quarantined);
    assert_eq!(quarantine(still, 220), ReviewState::Quarantined);
    assert_eq!(get(m, still).unwrap().unwrap().meta.state, ReviewState::Quarantined);

    // 终态仍非法：rejected / applied / stale。
    let applied = pending(m, "fact", "decision", "re-review-applied");
    approve(m, applied, 300);
    record_decision(
        m,
        applied,
        &decision(
            ReviewStage::Apply,
            ReviewVerdict::Pass,
            Decider::Program,
            "write_ok",
        ),
        310,
    )
    .unwrap();
    let stale = pending(m, "fact", "decision", "re-review-stale");
    m.conn()
        .execute("UPDATE review_item SET state='stale' WHERE id=?1", [stale])
        .unwrap();

    let decisions_before = count(m, "SELECT count(*) FROM review_decision");
    for (what, id) in [("rejected", rejected), ("applied", applied), ("stale", stale)] {
        let d = decision(ReviewStage::Review, ReviewVerdict::Pass, Decider::Center, "review_ok");
        match record_decision(m, id, &d, 400) {
            Err(MemError::Sql(msg)) => assert!(
                msg.starts_with("review: illegal transition"),
                "{what}: 错误短语不对：{msg}"
            ),
            other => panic!("{what} 必须被判为非法转换，实际：{other:?}"),
        }
    }
    assert_eq!(
        count(m, "SELECT count(*) FROM review_decision"),
        decisions_before,
        "被拒的复审判定不许落行"
    );
    assert_eq!(get(m, rejected).unwrap().unwrap().meta.state, ReviewState::Rejected);
    assert_eq!(get(m, applied).unwrap().unwrap().meta.state, ReviewState::Applied);
    assert_eq!(get(m, stale).unwrap().unwrap().meta.state, ReviewState::Stale);
}

/// S3b-4a：`note_attempt` 只做退避算术（attempt+1、next_retry、updated），不写判定；
/// 终态/不存在 ⇒ 0 行命中 ⇒ `MemError::Sql`，不复活终态。
#[test]
fn note_attempt_backs_off_live_items_and_refuses_terminal_ones() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    let id = pending(m, "fact", "decision", "backoff-pending");
    let decisions_before = count(m, "SELECT count(*) FROM review_decision");
    note_attempt(m, id, 60_000, 1_000).unwrap();
    let after_first = get(m, id).unwrap().unwrap();
    assert_eq!(after_first.meta.attempt, 1);
    assert_eq!(after_first.meta.updated_at_ms, 1_000);
    assert_eq!(after_first.meta.state, ReviewState::PendingReview, "退避不改状态");
    let next: i64 = m
        .conn()
        .query_row("SELECT next_retry_at_ms FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap();
    assert_eq!(next, 1_000 + 60_000);
    assert_eq!(
        count(m, "SELECT count(*) FROM review_decision"),
        decisions_before,
        "退避不是判决：一行判定都不许写"
    );

    note_attempt(m, id, 240_000, 61_000).unwrap();
    let after_second = get(m, id).unwrap().unwrap();
    assert_eq!(after_second.meta.attempt, 2, "attempt 逐次 +1");
    assert_eq!(after_second.meta.updated_at_ms, 61_000);
    let next: i64 = m
        .conn()
        .query_row("SELECT next_retry_at_ms FROM review_item WHERE id=?1", [id], |r| r.get(0))
        .unwrap();
    assert_eq!(next, 61_000 + 240_000);
    assert_eq!(count(m, "SELECT count(*) FROM review_decision"), decisions_before);

    // quarantined / approved 也是"可再试"状态：退避照记。
    let quarantined = pending(m, "fact", "decision", "backoff-quarantined");
    record_decision(
        m,
        quarantined,
        &decision(
            ReviewStage::Review,
            ReviewVerdict::Uncertain,
            Decider::Center,
            "review_uncertain",
        ),
        2_000,
    )
    .unwrap();
    note_attempt(m, quarantined, 60_000, 2_100).unwrap();
    assert_eq!(get(m, quarantined).unwrap().unwrap().meta.attempt, 1);
    let approved = pending(m, "event", "", "backoff-approved");
    approve(m, approved, 2_200);
    note_attempt(m, approved, 60_000, 2_300).unwrap();
    assert_eq!(get(m, approved).unwrap().unwrap().meta.attempt, 1);

    // 终态与不存在的 id：响亮拒绝，状态/attempt 都不动。
    let rejected = pending(m, "naming", "", "backoff-rejected");
    reject(m, rejected, 3_000);
    let stale = pending(m, "time", "", "backoff-stale");
    m.conn()
        .execute("UPDATE review_item SET state='stale' WHERE id=?1", [stale])
        .unwrap();
    for (what, id) in [("rejected", rejected), ("stale", stale), ("missing", 9_999)] {
        match note_attempt(m, id, 60_000, 3_100) {
            Err(MemError::Sql(msg)) => assert!(msg.starts_with("review:"), "{what}: {msg}"),
            other => panic!("{what} 必须被拒，实际：{other:?}"),
        }
    }
    let rejected_row = get(m, rejected).unwrap().unwrap();
    assert_eq!(rejected_row.meta.state, ReviewState::Rejected);
    assert_eq!(rejected_row.meta.attempt, 0, "终态的 attempt 不动");
    assert_eq!(get(m, stale).unwrap().unwrap().meta.state, ReviewState::Stale);
}

/// S3b-4a：`runnable` = 三种非终态 + 退避到期，按 id 升序；`pass` 过滤与 `limit` 同 list。
#[test]
fn runnable_lists_live_due_items_in_id_order_and_honours_backoff() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    let pending_item = pending(m, "fact", "decision", "runnable-pending");
    let approved = pending(m, "fact", "promise", "runnable-approved");
    approve(m, approved, 200);
    let quarantined = pending(m, "event", "", "runnable-quarantined");
    record_decision(
        m,
        quarantined,
        &decision(
            ReviewStage::Review,
            ReviewVerdict::Uncertain,
            Decider::Center,
            "review_uncertain",
        ),
        210,
    )
    .unwrap();
    let rejected = pending(m, "naming", "", "runnable-rejected");
    reject(m, rejected, 220);
    let applied = pending(m, "time", "", "runnable-applied");
    approve(m, applied, 230);
    record_decision(
        m,
        applied,
        &decision(
            ReviewStage::Apply,
            ReviewVerdict::Pass,
            Decider::Program,
            "write_ok",
        ),
        240,
    )
    .unwrap();
    let stale = pending(m, "rows", "", "runnable-stale");
    m.conn()
        .execute("UPDATE review_item SET state='stale' WHERE id=?1", [stale])
        .unwrap();

    let ids = |items: Vec<rc_memory::review::ItemMeta>| -> Vec<i64> {
        items.into_iter().map(|i| i.id).collect()
    };
    assert_eq!(
        ids(runnable(m, None, 1_000, 0).unwrap()),
        vec![pending_item, approved, quarantined],
        "三种可运行态、按 id 升序、终态不出现"
    );
    assert_eq!(
        ids(runnable(m, Some("fact"), 1_000, 0).unwrap()),
        vec![pending_item, approved],
        "pass 过滤与 list 同口径"
    );
    assert_eq!(
        ids(runnable(m, None, 1_000, 2).unwrap()),
        vec![pending_item, approved],
        "limit 截断在排序之后"
    );

    // 退避未到期 ⇒ 跳过；到期（<= now）⇒ 重新纳入。
    note_attempt(m, pending_item, 60_000, 1_100).unwrap();
    assert_eq!(
        ids(runnable(m, None, 1_100, 0).unwrap()),
        vec![approved, quarantined],
        "退避未到期的不许选中"
    );
    assert_eq!(
        ids(runnable(m, None, 1_100 + 60_000, 0).unwrap()),
        vec![pending_item, approved, quarantined],
        "到期（含等号）必须重新选中"
    );
    // 同一时刻 quarantine 复审路径也靠这条闸：给 quarantined 记退避 ⇒ 同样被跳过。
    note_attempt(m, quarantined, 60_000, 1_100).unwrap();
    assert_eq!(
        ids(runnable(m, None, 1_100, 0).unwrap()),
        vec![approved],
        "隔离项的退避同样生效"
    );
    assert!(
        runnable(m, None, 1_100, 0)
            .unwrap()
            .iter()
            .all(|i| i.id != rejected && i.id != applied && i.id != stale),
        "终态一条都不许出现"
    );
}

// ── H1a：claim / release / 冲突分类 ──────────────────────────────────────────

fn claim_row(m: &MemoryHandle, id: i64) -> (Option<String>, Option<i64>) {
    m.conn()
        .query_row(
            "SELECT claimed_by, claim_expires_at_ms FROM review_item WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

fn has_claim_columns(m: &MemoryHandle) -> bool {
    let n: i64 = m
        .conn()
        .query_row(
            "SELECT count(*) FROM pragma_table_info('review_item')
              WHERE name IN ('claimed_by','claim_expires_at_ms')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    n == 2
}

/// 只有三种非终态能被占；退避未到期不许占；租约到期（含等号）可以被接管；
/// 重复 claim 只有一个成功；release 只清 owner 匹配的那一格。
#[test]
fn claim_takes_only_live_items_and_honours_backoff_and_lease() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    // pending_review 可占：写 owner 与租约到期时刻（now + lease）。
    let pending_id = pending(m, "fact", "decision", "claim-pending");
    assert!(claim(m, pending_id, "run-a", 1_000, 200).unwrap());
    assert_eq!(
        claim_row(m, pending_id),
        (Some("run-a".into()), Some(1_200)),
        "占位必须写 owner 与 now+lease"
    );

    // 同一条 item 的上一个租约还没到期：第二个 owner 抢不到。
    assert!(!claim(m, pending_id, "run-b", 1_000, 200).unwrap());
    assert!(!claim(m, pending_id, "run-b", 1_000, 1_199).unwrap());
    assert_eq!(claim_row(m, pending_id).0.as_deref(), Some("run-a"));
    // 到期（<= now，含等号）⇒ 可以接管。
    assert!(claim(m, pending_id, "run-b", 1_000, 1_200).unwrap());
    assert_eq!(claim_row(m, pending_id).0.as_deref(), Some("run-b"));

    // release 只清自己的位：owner 不匹配 ⇒ 0 行、不动别人的占位。
    release(m, pending_id, "run-a").unwrap();
    assert_eq!(claim_row(m, pending_id).0.as_deref(), Some("run-b"));
    release(m, pending_id, "run-b").unwrap();
    assert_eq!(claim_row(m, pending_id), (None, None));

    // quarantined / approved 也可占。
    let quarantined = pending(m, "fact", "decision", "claim-quarantined");
    record_decision(
        m,
        quarantined,
        &decision(
            ReviewStage::Review,
            ReviewVerdict::Uncertain,
            Decider::Center,
            "review_uncertain",
        ),
        300,
    )
    .unwrap();
    assert!(claim(m, quarantined, "run-a", 1_000, 400).unwrap());
    let approved = pending(m, "fact", "decision", "claim-approved");
    approve(m, approved, 410);
    assert!(claim(m, approved, "run-a", 1_000, 420).unwrap());

    // 三种终态与不存在的 id：false，且不留下占位。
    let rejected = pending(m, "fact", "decision", "claim-rejected");
    reject(m, rejected, 500);
    let applied = pending(m, "fact", "decision", "claim-applied");
    approve(m, applied, 510);
    record_decision(
        m,
        applied,
        &decision(
            ReviewStage::Apply,
            ReviewVerdict::Pass,
            Decider::Program,
            "write_ok",
        ),
        520,
    )
    .unwrap();
    let stale = pending(m, "fact", "decision", "claim-stale");
    m.conn()
        .execute("UPDATE review_item SET state='stale' WHERE id=?1", [stale])
        .unwrap();
    for id in [rejected, applied, stale, 9_999] {
        assert!(!claim(m, id, "run-a", 1_000, 600).unwrap(), "id={id} 不该被占");
    }
    assert_eq!(claim_row(m, rejected), (None, None));
    release(m, 9_999, "run-a").unwrap(); // 不存在/未占用：0 行不是错误
}

/// 退避未到期 ⇒ claim 失败；到期（<= now）⇒ 成功。
#[test]
fn claim_waits_for_the_backoff_due_time() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let id = pending(m, "fact", "decision", "claim-backoff");
    note_attempt(m, id, 60_000, 1_000).unwrap(); // next_retry_at_ms = 61_000
    assert!(!claim(m, id, "run-a", 1_000, 60_999).unwrap(), "退避未到期不许占");
    assert!(claim(m, id, "run-a", 1_000, 61_000).unwrap(), "到期含等号必须能占");
}

/// 只读柄上 claim/release 都拒写（与其他写 API 同一条纪律）。
#[test]
fn read_only_handles_refuse_claim_and_release() {
    let dir = tempfile::tempdir().unwrap();
    let id = {
        let root = Root::open(dir.path()).unwrap();
        pending(root.memory(), "fact", "decision", "claim-read-only")
    };
    let replica = AgentRoot::open_replica(dir.path(), &dir.path().join("memory.db")).unwrap();
    let m = replica.memory();
    match claim(m, id, "run-a", 1_000, 100) {
        Err(MemError::ReadOnly) => {}
        other => panic!("只读柄必须拒绝 claim，实际：{other:?}"),
    }
    match release(m, id, "run-a") {
        Err(MemError::ReadOnly) => {}
        other => panic!("只读柄必须拒绝 release，实际：{other:?}"),
    }
}

/// 只有 `illegal()` 生产的短语是冲突；其余 `MemError` 一律不是。`RunError::Conflict`
/// 的判定完全靠这一格，放宽它就会把真存储故障读成竞态。
#[test]
fn only_the_illegal_transition_phrase_is_a_review_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let id = pending(m, "fact", "decision", "conflict-phrase");
    approve(m, id, 200);
    // approved 上再来一条 review pass 是非法转换（另一写者已推进的典型形状）。
    let err = record_decision(
        m,
        id,
        &decision(
            ReviewStage::Review,
            ReviewVerdict::Pass,
            Decider::Center,
            "review_ok",
        ),
        300,
    )
    .unwrap_err();
    assert!(err.is_review_conflict(), "非法转换必须被判为冲突：{err}");
    assert!(!MemError::ReadOnly.is_review_conflict());
    assert!(!MemError::Sql("review: no such review item".into()).is_review_conflict());
    assert!(!MemError::Sql(
        "review: note_attempt hit a terminal or missing item".into()
    )
    .is_review_conflict());
    assert!(!MemError::Sql("sqlite: database is locked".into()).is_review_conflict());
}

/// H1a：旧形状库（缺两列）开库自动补列；已有行两列为 NULL；重复开库幂等；新库列已在。
#[test]
fn opening_an_old_shape_memory_db_backfills_claim_columns_idempotently() {
    let dir = tempfile::tempdir().unwrap();
    let id = {
        let root = Root::open(dir.path()).unwrap();
        let m = root.memory();
        let id = pending(m, "fact", "decision", "claim-migrate");
        assert!(has_claim_columns(m), "新库建表必须带两列");
        // 模拟 H1a 之前的形状：两列不存在，版本号仍是 6。
        m.conn()
            .execute_batch(
                "ALTER TABLE review_item DROP COLUMN claimed_by;
                 ALTER TABLE review_item DROP COLUMN claim_expires_at_ms",
            )
            .unwrap();
        assert!(!has_claim_columns(m));
        id
    };

    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    assert!(has_claim_columns(m), "旧形状库开库必须自动补列");
    assert_eq!(claim_row(m, id), (None, None), "已有行两列必须是 NULL");
    assert_eq!(count(m, "SELECT count(*) FROM review_item"), 1);

    // 重复开库幂等：列还在、行不变。
    drop(root);
    let root = Root::open(dir.path()).unwrap();
    assert!(has_claim_columns(root.memory()));
    assert_eq!(claim_row(root.memory(), id), (None, None));
    assert_eq!(count(root.memory(), "SELECT count(*) FROM review_item"), 1);
}

// ── H1c：batch_key 与同事务顶替 ──────────────────────────────────────────────

fn batch_item(pass: &str, family: &str, payload: &str, key: &str) -> NewItem {
    NewItem { batch_key: key.into(), ..item(pass, family, payload) }
}

fn in_flight(m: &MemoryHandle) -> i64 {
    count(
        m,
        "SELECT count(*) FROM review_item
          WHERE state IN ('pending_review','quarantined','approved')",
    )
}

/// §1 的手工重算（与生产实现分开写一遍，用来对拍）。
fn manual_batch_key(pass: &str, family: &str, session: &str, device: &str, uids: &[&str]) -> String {
    let mut u = uids.to_vec();
    u.sort_unstable();
    u.dedup();
    let mut h = Sha256::new();
    for part in [pass, family, session, device] {
        h.update(part.as_bytes());
        h.update([0x1f]);
    }
    h.update(u.join("\x1f").as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// 1. 键的稳定性：轮集合顺序/重复不影响键，session/device/family/pass 各是一个分量；
///    空 key 不参与顶替（两条都在途，谁也不 stale）。
#[test]
fn batch_key_is_stable_across_turn_order_and_separates_its_components() {
    let base = rc_memory::review::batch_key("fact", "decision", "sess-a", "dev-a", &["t2", "t1"]);
    assert_eq!(base.len(), 64);
    assert!(base.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_eq!(
        base,
        rc_memory::review::batch_key("fact", "decision", "sess-a", "dev-a", &["t1", "t2", "t1"]),
        "轮顺序与重复不影响键"
    );
    assert_eq!(
        base,
        manual_batch_key("fact", "decision", "sess-a", "dev-a", &["t1", "t2"]),
        "生产实现必须与 §1 的手工重算逐位一致"
    );
    for (what, other) in [
        ("session", rc_memory::review::batch_key("fact", "decision", "sess-b", "dev-a", &["t1", "t2"])),
        ("device", rc_memory::review::batch_key("fact", "decision", "sess-a", "dev-b", &["t1", "t2"])),
        ("family", rc_memory::review::batch_key("fact", "promise", "sess-a", "dev-a", &["t1", "t2"])),
        ("pass", rc_memory::review::batch_key("event", "decision", "sess-a", "dev-a", &["t1", "t2"])),
        ("uids", rc_memory::review::batch_key("fact", "decision", "sess-a", "dev-a", &["t1", "t3"])),
    ] {
        assert_ne!(base, other, "换 {what} 必须换键");
    }

    // 空 key：同 pass/family/session/device/轮集合、不同载荷 ⇒ 两条都在途，不顶替。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let a = enqueue(m, &item("fact", "decision", "empty-key-a"), 10).unwrap();
    let b = enqueue(m, &item("fact", "decision", "empty-key-b"), 20).unwrap();
    assert!(a.fresh && b.fresh);
    assert_ne!(a.id, b.id);
    assert_eq!(count(m, "SELECT count(*) FROM review_item"), 2);
    assert_eq!(in_flight(m), 2, "空 key 不参与顶替");
    assert_eq!(count(m, "SELECT count(*) FROM review_item WHERE state='stale'"), 0);
    let stored: Option<String> = m
        .conn()
        .query_row("SELECT batch_key FROM review_item WHERE id=?1", [a.id], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, None, "空 key 落库必须是 NULL（旧行形状），不进部分索引");
}

// ── S3c-fix：rows 专用键（行集合而不是轮集合）────────────────────────────────

fn rows_ids(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| (*id).to_string()).collect()
}

/// P0 回归钉（队列层）：同一 session/device/**同一轮集合**、不同**行集合**的两个分片
/// 必须得到不同的键——这正是 S3c 复审里"12 条同轮 fact 切两片、后片把前片顶成 stale"
/// 的形状。同一行集合重试（顺序颠倒也算）必须同键。
#[test]
fn rows_batch_key_uses_the_row_set_not_the_turn_set() {
    use rc_memory::review::rows_batch_key;

    let first = rows_batch_key("sess-a", "dev-a", &rows_ids(&["row-1", "row-2"]));
    let second = rows_batch_key("sess-a", "dev-a", &rows_ids(&["row-3", "row-4"]));
    assert_eq!(first.len(), 64);
    assert!(first.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_ne!(first, second, "轮集合相同、行集合不同 ⇒ 键必须不同（P0 的顶替面）");
    // 重试稳定：同一行集合、顺序颠倒/重复 ⇒ 同键（重试命中同一条 item）。
    assert_eq!(
        first,
        rows_batch_key("sess-a", "dev-a", &rows_ids(&["row-2", "row-1", "row-1"])),
        "行顺序与重复不影响键"
    );
    // 分量：session / device 各是一个分量，且 rows 键不吃 fact/event 的名字空间。
    assert_ne!(first, rows_batch_key("sess-b", "dev-a", &rows_ids(&["row-1", "row-2"])));
    assert_ne!(first, rows_batch_key("sess-a", "dev-b", &rows_ids(&["row-1", "row-2"])));
    assert_ne!(
        first,
        rc_memory::review::batch_key("rows", "", "sess-a", "dev-a", &["row-1", "row-2"]),
        "rows 键与 fact/event 的轮集合键不是同一条公式"
    );
}

/// 两个"同轮、不同行"的 rows item 入队 ⇒ 两条都在途，谁也不被顶成 `stale`
/// （P0 的队列层执法：`rows_batch_key` 之外还要 `enqueue` 的顶替真的不命中）。
#[test]
fn two_rows_slices_of_the_same_turn_set_stay_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let key_a = rc_memory::review::rows_batch_key("sess-a", "dev-a", &rows_ids(&["row-1"]));
    let key_b = rc_memory::review::rows_batch_key("sess-a", "dev-a", &rows_ids(&["row-2"]));
    assert_ne!(key_a, key_b);
    let a = enqueue(m, &batch_item("rows", "", "rows-slice-a", &key_a), 100).unwrap();
    let b = enqueue(m, &batch_item("rows", "", "rows-slice-b", &key_b), 200).unwrap();
    assert!(a.fresh && b.fresh);
    assert_eq!(in_flight(m), 2, "两片都得在途");
    assert_eq!(count(m, "SELECT count(*) FROM review_item WHERE state='stale'"), 0);
    assert_eq!(
        count(m, "SELECT count(*) FROM review_decision WHERE reason_code='superseded'"),
        0,
        "不许有顶替判定"
    );
}

/// 2. 同键同载荷：第二次命中载荷去重，`fresh=false`，行数仍 1。
#[test]
fn same_batch_key_and_same_payload_reuses_the_existing_row() {    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let key = rc_memory::review::batch_key("fact", "decision", "sess-a", "dev-a", &["turn-1"]);
    let it = batch_item("fact", "decision", "same-payload", &key);
    let first = enqueue(m, &it, 100).unwrap();
    assert!(first.fresh);
    let second = enqueue(m, &it, 200).unwrap();
    assert!(!second.fresh, "同键同载荷 ⇒ 复用");
    assert_eq!(second.id, first.id);
    assert_eq!(second.state, ReviewState::PendingReview);
    assert_eq!(second.payload_sha256, first.payload_sha256);
    assert_eq!(count(m, "SELECT count(*) FROM review_item"), 1);
    assert_eq!(count(m, "SELECT count(*) FROM review_decision"), 0);
}

/// 3. 同键不同载荷：旧行 `stale` + 一条 `superseded` 判定，新行 `pending_review`；
///    全表在途行恰 1 条。占位/退避一并作废。
#[test]
fn same_batch_key_different_payload_supersedes_the_old_row_in_one_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let key = rc_memory::review::batch_key("fact", "decision", "sess-a", "dev-a", &["turn-1"]);
    let old = enqueue(m, &batch_item("fact", "decision", "payload-v1", &key), 100).unwrap();
    assert!(old.fresh);
    // 旧行被占着、还在退避里：顶替必须把这两样也清掉（载荷已不作数）。
    assert!(claim(m, old.id, "run-a", 60_000, 100).unwrap());
    note_attempt(m, old.id, 60_000, 100).unwrap();

    let new = enqueue(m, &batch_item("fact", "decision", "payload-v2", &key), 200).unwrap();
    assert!(new.fresh, "顶替场景 fresh=true");
    assert_ne!(new.id, old.id);
    assert_eq!(new.state, ReviewState::PendingReview);

    let old_row = get(m, old.id).unwrap().unwrap();
    assert_eq!(old_row.meta.state, ReviewState::Stale);
    assert_eq!(old_row.meta.program_code.as_deref(), Some("superseded"));
    assert_eq!(old_row.meta.updated_at_ms, 200);
    assert_eq!(claim_row(m, old.id), (None, None), "顶替必须清占位");
    let retry: Option<i64> = m
        .conn()
        .query_row("SELECT next_retry_at_ms FROM review_item WHERE id=?1", [old.id], |r| r.get(0))
        .unwrap();
    assert_eq!(retry, None, "顶替必须清退避");

    let rows = decisions(m, old.id).unwrap();
    assert_eq!(rows.len(), 1, "顶替恰好追加一条判定");
    assert_eq!(rows[0].stage, ReviewStage::Program);
    assert_eq!(rows[0].verdict, ReviewVerdict::Reject);
    assert_eq!(rows[0].reason_code, "superseded");
    assert_eq!(rows[0].detail, "batch_superseded");
    assert_eq!(rows[0].created_at_ms, 200);
    let (rules, model, prompt, input, decider): (String, String, String, String, String) = m
        .conn()
        .query_row(
            "SELECT rules_version, model, prompt_version, input_sha256, decider
               FROM review_decision WHERE item_id=?1",
            [old.id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(rules, "review-gate-v0.1");
    assert_eq!(model, "", "顶替没有模型参与");
    assert_eq!(prompt, "", "顶替没有 prompt 参与");
    assert_eq!(input, old.payload_sha256, "input_sha256 = 旧行的载荷指纹");
    assert_eq!(decider, "program");

    assert_eq!(count(m, "SELECT count(*) FROM review_item"), 2);
    assert_eq!(in_flight(m), 1, "全表在途行恰 1 条");
    assert_eq!(get(m, new.id).unwrap().unwrap().meta.state, ReviewState::PendingReview);
}

/// 4. 终态不挡：旧行 applied 后同键新载荷 ⇒ 新行 pending，两行并存（历史保留）。
#[test]
fn terminal_rows_with_the_same_batch_key_do_not_block_a_new_item() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let key = rc_memory::review::batch_key("fact", "decision", "sess-a", "dev-a", &["turn-1"]);
    let old = enqueue(m, &batch_item("fact", "decision", "applied-v1", &key), 100).unwrap();
    approve(m, old.id, 200);
    record_decision(
        m,
        old.id,
        &decision(ReviewStage::Apply, ReviewVerdict::Pass, Decider::Program, "write_ok"),
        300,
    )
    .unwrap();
    assert_eq!(get(m, old.id).unwrap().unwrap().meta.state, ReviewState::Applied);

    let new = enqueue(m, &batch_item("fact", "decision", "applied-v2", &key), 400).unwrap();
    assert!(new.fresh);
    assert_ne!(new.id, old.id);
    assert_eq!(new.state, ReviewState::PendingReview);
    assert_eq!(get(m, old.id).unwrap().unwrap().meta.state, ReviewState::Applied, "终态不被改写");
    assert_eq!(count(m, "SELECT count(*) FROM review_item"), 2);
    assert_eq!(
        count(m, "SELECT count(*) FROM review_decision WHERE reason_code='superseded'"),
        0,
        "终态不产生顶替判定"
    );
}

/// 5. 在途部分唯一索引：绕过 enqueue 手工插第二条同键在途行 ⇒ `SQLITE_CONSTRAINT`；
///    终态不占位（同键 stale 行可以并存）。
#[test]
fn the_partial_unique_index_refuses_a_second_in_flight_row_with_the_same_key() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let key = rc_memory::review::batch_key("fact", "decision", "sess-a", "dev-a", &["turn-1"]);
    let first = enqueue(m, &batch_item("fact", "decision", "index-first", &key), 100).unwrap();
    assert!(first.fresh);

    let insert = |id_suffix: &str, state: &str| {
        m.conn().execute(
            "INSERT INTO review_item(
                source, pass_id, family, session_key, device_id, turn_uids_json, owner_json,
                payload_json, payload_sha256, state, created_at_ms, updated_at_ms, batch_key)
             VALUES('distill','fact','decision','sess-a','dev-a','[\"turn-1\"]','{}',?1,?2,?3,1,1,?4)",
            rusqlite::params![format!("manual-{id_suffix}"), "f".repeat(64), state, key],
        )
    };
    match insert("pending", "pending_review") {
        Err(rusqlite::Error::SqliteFailure(e, _)) => {
            assert_eq!(e.code, rusqlite::ErrorCode::ConstraintViolation, "必须是唯一约束失败");
        }
        other => panic!("第二条同键在途行必须被索引拒绝，实际：{other:?}"),
    }
    // 终态不占位：同键 stale 行（以及 approved/quarantined 之外的三态）可以并存。
    assert!(insert("stale", "stale").is_ok(), "终态同键必须允许并存");
    assert_eq!(count(m, "SELECT count(*) FROM review_item"), 2);
    assert_eq!(in_flight(m), 1);
}

/// 6. 并发：两个连接同时入队同键不同载荷 ⇒ 一个 stale、一个 pending，无未处理错误。
#[test]
fn concurrent_enqueues_of_the_same_key_end_with_one_stale_and_one_pending() {
    let dir = tempfile::tempdir().unwrap();
    // 预热：先建库+迁移，两个线程只做 enqueue（首开迁移不是本用例的靶子）。
    drop(Root::open(dir.path()).unwrap());
    let key = rc_memory::review::batch_key("fact", "decision", "sess-a", "dev-a", &["turn-1"]);

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut handles = Vec::new();
    for payload in ["concurrent-a", "concurrent-b"] {
        let path = dir.path().to_path_buf();
        let barrier = barrier.clone();
        let key = key.clone();
        handles.push(std::thread::spawn(move || {
            let root = Root::open(&path).unwrap();
            barrier.wait();
            enqueue(root.memory(), &batch_item("fact", "decision", payload, &key), 100)
        }));
    }
    let outcomes: Vec<EnqueueOutcome> = handles
        .into_iter()
        .map(|h| h.join().unwrap().expect("并发入队不许有未处理错误"))
        .collect();
    for out in &outcomes {
        assert!(out.fresh, "一次插入、一次顶替：两次都是新行");
        assert_eq!(out.state, ReviewState::PendingReview);
    }
    assert_ne!(outcomes[0].id, outcomes[1].id, "后到者必须是一条新行");

    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    assert_eq!(count(m, "SELECT count(*) FROM review_item"), 2);
    assert_eq!(in_flight(m), 1, "在途恰 1 条");
    assert_eq!(count(m, "SELECT count(*) FROM review_item WHERE state='stale'"), 1);
    assert_eq!(
        count(
            m,
            "SELECT count(*) FROM review_decision \
             WHERE stage='program' AND verdict='reject' AND reason_code='superseded' \
               AND detail='batch_superseded'"
        ),
        1,
        "顶替判定恰一条"
    );
}

// ── H4b：审计指纹拆列（载荷指纹 / 送审指纹）────────────────────────────────────

fn has_review_input_column(conn: &rusqlite::Connection) -> bool {
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM pragma_table_info('review_decision')
              WHERE name='review_input_sha256'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    n == 1
}

fn review_input_of(m: &MemoryHandle, item_id: i64) -> String {
    m.conn()
        .query_row(
            "SELECT review_input_sha256 FROM review_decision
              WHERE item_id=?1 AND stage='review'",
            [item_id],
            |r| r.get(0),
        )
        .unwrap()
}

/// H4b：`input_sha256` 恒为载荷指纹（三个 stage 同格），`review_input_sha256` 只在
/// review 行放送审指纹；program/apply 行该列为空串。`decisions()` 两列都要读回。
#[test]
fn both_fingerprint_columns_round_trip_with_stage_specific_values() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let id = pending(m, "fact", "decision", "h4b-roundtrip");
    let payload_sha = get(m, id).unwrap().unwrap().payload_sha256;
    let review_input = "a".repeat(64);

    let mut program = decision(
        ReviewStage::Program,
        ReviewVerdict::Pass,
        Decider::Program,
        "program_pass",
    );
    program.input_sha256 = payload_sha.clone();
    record_decision(m, id, &program, 100).unwrap();

    let mut review = decision(
        ReviewStage::Review,
        ReviewVerdict::Pass,
        Decider::Center,
        "review_pass",
    );
    review.input_sha256 = payload_sha.clone();
    review.review_input_sha256 = review_input.clone();
    record_decision(m, id, &review, 200).unwrap();

    let mut apply = decision(
        ReviewStage::Apply,
        ReviewVerdict::Pass,
        Decider::Program,
        "applied",
    );
    apply.input_sha256 = payload_sha.clone();
    record_decision(m, id, &apply, 300).unwrap();

    let rows = decisions(m, id).unwrap();
    assert_eq!(rows.len(), 3);
    for row in &rows {
        assert_eq!(row.input_sha256, payload_sha, "三个 stage 的 input_sha256 都是载荷指纹");
    }
    assert_eq!(rows[0].review_input_sha256, "", "program 行没有送审指纹");
    assert_eq!(rows[1].review_input_sha256, review_input, "review 行带送审指纹");
    assert_eq!(rows[2].review_input_sha256, "", "apply 行没有送审指纹");
    assert_eq!(review_input_of(m, id), review_input);
}

/// H4b：旧形状库（`review_decision` 缺 `review_input_sha256`）开库自动补列；已有判定行
/// 该列是空串；重复开库幂等；新库建表就带这一列。
#[test]
fn opening_an_old_shape_decision_table_backfills_the_review_input_column() {
    let dir = tempfile::tempdir().unwrap();
    let id = {
        let root = Root::open(dir.path()).unwrap();
        let m = root.memory();
        let id = pending(m, "fact", "decision", "h4b-migrate");
        assert!(has_review_input_column(m.conn()), "新库建表必须带 review_input_sha256");
        approve(m, id, 200);
        // 模拟 H4b 之前的形状：列不存在，版本号仍是 6。
        m.conn()
            .execute_batch("ALTER TABLE review_decision DROP COLUMN review_input_sha256")
            .unwrap();
        assert!(!has_review_input_column(m.conn()));
        id
    };

    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    assert!(has_review_input_column(m.conn()), "旧形状库开库必须自动补列");
    assert_eq!(review_input_of(m, id), "", "旧行补列后必须是空串");
    assert_eq!(decisions(m, id).unwrap()[0].review_input_sha256, "");
    assert_eq!(count(m, "SELECT count(*) FROM review_decision"), 1);

    // 重复开库幂等：列还在、值不变。
    drop(root);
    let root = Root::open(dir.path()).unwrap();
    assert!(has_review_input_column(root.memory().conn()));
    assert_eq!(review_input_of(root.memory(), id), "");
    assert_eq!(count(root.memory(), "SELECT count(*) FROM review_decision"), 1);
}

/// H7c2：`max_decision_id` —— 审查轴水位的本地分量。
/// 空库读回 0（不是 NULL/错误）；有判定后等于 `review_decision.id` 的最大值，且随判定
/// 追加单调增。它是"审查差多少条"的右端点，读错一格会让对账报告说谎。
#[test]
fn max_decision_id_reads_zero_on_an_empty_store_then_the_largest_decision_id() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    assert_eq!(max_decision_id(m).unwrap(), 0, "空库必须读回 0");

    let first = pending(m, "fact", "decision", "h7c2-max-1");
    approve(m, first, 200);
    let after_first = max_decision_id(m).unwrap();
    assert!(after_first > 0, "记过判定后不许还是 0：{after_first}");
    assert_eq!(
        after_first,
        count(m, "SELECT MAX(id) FROM review_decision"),
        "必须是判定台账的最大 id"
    );

    // 第二个 item 的判定：单调增，且仍与 `MAX(id)` 同一格。
    let second = pending(m, "event", "", "h7c2-max-2");
    assert_eq!(reject(m, second, 300), ReviewState::Rejected);
    let after_second = max_decision_id(m).unwrap();
    assert!(after_second > after_first, "追加判定必须让水位前进：{after_first} → {after_second}");
    assert_eq!(after_second, count(m, "SELECT MAX(id) FROM review_decision"));
    assert_ne!(
        after_second,
        count(m, "SELECT MIN(id) FROM review_decision"),
        "读的不能是最小 id"
    );
}

/// A9（W34 2026-11-07）：`source='report'` 是消费者报错的最小入口。入队走的是
/// **既有链**（enqueue + 载荷去重），本用例只钉三格：闭集放行、`pending_review` 落行、
/// 同载荷去重 `fresh=false` 不出第二行。报告人换人（owner_json 变）算另一条 item——
/// 指纹的 preimage 含 owner，这与既有 fact 用例的"换 owner 就是另一条"同一条规则。
#[test]
fn report_source_enqueues_pending_and_dedupes_on_the_payload() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let it = NewItem {
        source: "report".into(),
        pass_id: "report".into(),
        family: String::new(),
        session_key: String::new(),
        device_id: "dev-a".into(),
        turn_uids_json: format!("[\"card:{}\"]", "f".repeat(64)),
        owner_json: "{\"reporter\":\"agent-a\"}".into(),
        payload_json: "{\"category\":\"wrong_fact\",\"note\":\"口径反了\",\"schema\":\"report.v1\",\"target\":\"x\"}".into(),
        batch_key: String::new(),
    };
    let first = enqueue(m, &it, 1_000).unwrap();
    assert!(first.fresh);
    assert_eq!(first.state, ReviewState::PendingReview);

    let full = get(m, first.id).unwrap().expect("刚入队的报告必须读得回来");
    assert_eq!(full.meta.source, "report", "source 列就是辨识位");
    assert_eq!(full.meta.pass_id, "report");
    assert_eq!(full.meta.state, ReviewState::PendingReview);
    assert_eq!(full.meta.device_id, "dev-a");

    // 同报告人 + 同载荷重报：去重生效，不出第二行。
    let second = enqueue(m, &it, 2_000).unwrap();
    assert_eq!(second.id, first.id);
    assert!(!second.fresh, "重复报同一目标+同载荷必须命中载荷去重");
    assert_eq!(count(m, "SELECT count(*) FROM review_item"), 1);

    // 换报告人：指纹含 owner ⇒ 新 item（与既有"换 owner 就是另一条"同一格）。
    let other = NewItem {
        owner_json: "{\"reporter\":\"agent-b\"}".into(),
        ..it.clone()
    };
    let third = enqueue(m, &other, 3_000).unwrap();
    assert!(third.fresh);
    assert_ne!(third.id, first.id);
    assert_eq!(count(m, "SELECT count(*) FROM review_item"), 2);
}
