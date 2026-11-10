//! 波三刀 A：lesson pass 的库面——选批（记错的候选源）、窄 prompt、独立审核、落卡。
//!
//! 这一层断七件事：① 空候选 = 空批零成本；② 重跑幂等（同一组只入队一次）；
//! ③ 抽取契约拒 ⇒ 不入队不落卡；④ 审核 reject/uncertain ⇒ **不落卡**（EDV default-reject）；
//! ⑤ 审核 pass ⇒ 落 `kind='lesson'` 卡且可经句柄 recall、可被 `recollect_reuse` 反馈；
//! ⑥ lesson 不写 fact/event 既有面；⑦ 选批按（会话, 错误族）分组。
//!
//! 候选源是 `memory.db.review_item(state='rejected')`（审核驳回或程序闸整批驳回），
//! 夹具直接落那条"成熟的事实"。正反例只用通用名与自编句，不抄语料原句。

mod common;
use common::seed_bodies;

use rc_distill::pass_lesson::{
    run_lesson, select_lesson_batch, LessonRunError, REVIEW_REJECT_CODE,
};
use rc_distill::pass_prompt::PASS_LESSON;
use rc_distill::review::ReviewMode;
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_store::Root;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

const NOW: i64 = 1_700_000_000_000;
const SESSION: &str = "sess-batch-A";
const DEVICE: &str = "dev-batch-A";

/// 测试自带的后端：按序回放脚本（与 event 测试同一份形状）。
struct ScriptBackend {
    replies: RefCell<VecDeque<Result<String, LlmFailure>>>,
    calls: Cell<usize>,
}

impl ScriptBackend {
    fn new(replies: Vec<Result<String, LlmFailure>>) -> Self {
        ScriptBackend { replies: RefCell::new(replies.into()), calls: Cell::new(0) }
    }
    fn two(extraction: String, review: String) -> Self {
        ScriptBackend::new(vec![Ok(extraction), Ok(review)])
    }
}

impl LlmBackend for ScriptBackend {
    fn slug(&self) -> Option<&str> {
        Some("scripted")
    }
    fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
        self.calls.set(self.calls.get() + 1);
        self.replies
            .borrow_mut()
            .pop_front()
            .unwrap_or(Err(LlmFailure::Timeout))
    }
}

/// 一段两轮的会话（human + agent）。
fn seeded() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &["先把计数报出来，一共 37 条", "那个数原文里没有"], 1_000);
    (dir, root, uids)
}

/// 落一条 `state='rejected'` 的 review_item（候选源的"成熟事实"）。
///
/// `sha` 只需在 `(pass_id, family)` 内唯一；`code` 空串按 NULL 落（审核驳回那一支）。
fn seed_rejected(
    root: &Root,
    pass_id: &str,
    code: Option<&str>,
    uids: &[String],
    sha: u8,
    session: &str,
    device: &str,
) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO review_item(source, pass_id, family, session_key, device_id,
                                     turn_uids_json, owner_json, payload_json, payload_sha256,
                                     state, program_code, created_at_ms, updated_at_ms)
             VALUES('distill', ?1, '', ?2, ?3, ?4, '{\"owner_agent_id\":\"alice\"}',
                    '{\"schema\":\"review.pass.v1\"}', ?5, 'rejected', ?6, ?7, ?7)",
            rusqlite::params![
                pass_id,
                session,
                device,
                json!(uids).to_string(),
                format!("{sha:064x}"),
                code,
                NOW
            ],
        )
        .unwrap();
}

fn lesson_reply(lessons: Vec<Value>) -> String {
    json!({"schema_version": 0, "pass": PASS_LESSON, "lessons": lessons}).to_string()
}

fn one_lesson() -> Value {
    json!({
        "title": "引用数字前先回到原文",
        "description": "输出里要出现数量或日期时适用；防止把记忆里的估算写成语料原文",
        "content": "写任何数字前先在引文里找到同一个数字；找不到就写 unknown",
        "error_kind": "grounding"
    })
}

fn review_reply(verdicts: &[(&str, &str, Option<&str>)]) -> String {
    let verdicts: Vec<Value> = verdicts
        .iter()
        .map(|(reference, verdict, code)| {
            json!({"ref": reference, "verdict": verdict, "code": code})
        })
        .collect();
    json!({"schema_version": 0, "pass": "lesson_review", "verdicts": verdicts}).to_string()
}

fn cards(root: &Root) -> Vec<(String, String, String)> {
    let mut st = root
        .memory()
        .conn()
        .prepare("SELECT id, kind, title FROM card ORDER BY id")
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

/// 一轮在 `extract_pass_state(pass_id='lesson')` 上的状态行 `(state, reason)`；没有 ⇒ `None`。
fn lesson_state(root: &Root, uid: &str) -> Option<(String, Option<String>)> {
    root.ledger()
        .conn()
        .query_row(
            "SELECT state, reason FROM extract_pass_state
              WHERE turn_uid=?1 AND pass_id='lesson' AND family=''",
            [uid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()
}

#[test]
fn 空候选是空批且零成本() {
    let (_dir, root, _uids) = seeded();
    let plan = select_lesson_batch(
        root.transcript(),
        root.ledger(),
        root.memory(),
        0,
        NOW,
    )
    .unwrap();
    assert!(plan.is_none(), "没有 rejected item 就没有候选（空批合法）");
    assert!(cards(&root).is_empty(), "选批不写库");
}

#[test]
fn 选批按会话与错误族分组并取这一段的目标轮() {
    let (_dir, root, uids) = seeded();
    seed_rejected(&root, "fact", Some("number_not_grounded"), &uids, 1, SESSION, DEVICE);
    let plan = select_lesson_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("有候选必须选出来");
    assert_eq!(plan.error_code, "number_not_grounded");
    assert_eq!(plan.batch.session_key, SESSION);
    assert_eq!(plan.batch.device_id, DEVICE);
    assert_eq!(plan.batch.rows.len(), 2, "这一段的两轮都进批");
    assert_eq!(plan.batch.rows[0].body, "先把计数报出来，一共 37 条");
    assert_eq!(plan.source_items, 1);
    // 审核驳回（program_code 为 NULL）的族词是 `review_reject`。
    let (_dir2, root2, uids2) = seeded();
    seed_rejected(&root2, "naming", None, &uids2[..1], 2, SESSION, DEVICE);
    let plan2 = select_lesson_batch(root2.transcript(), root2.ledger(), root2.memory(), 0, NOW)
        .unwrap()
        .unwrap();
    assert_eq!(plan2.error_code, REVIEW_REJECT_CODE);
}

#[test]
fn 重跑幂等_同一组只入队一次也不再选出来() {
    let (_dir, root, uids) = seeded();
    seed_rejected(&root, "fact", Some("number_not_grounded"), &uids, 3, SESSION, DEVICE);
    let plan = select_lesson_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .unwrap();
    let backend = ScriptBackend::two(
        lesson_reply(vec![one_lesson()]),
        review_reply(&[("l1", "pass", None)]),
    );
    let out = run_lesson(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
        ReviewMode::Inline,
    )
    .unwrap();
    assert_eq!(out.cards, 1);
    assert_eq!(cards(&root).len(), 1);
    // 重跑：同一组的 batch_key 已在 review_item(pass_id='lesson') 里 ⇒ 不再选出来。
    let again = select_lesson_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap();
    assert!(again.is_none(), "重跑不重复：这一组已经产过 lesson 项");
    let n: i64 = root
        .memory()
        .conn()
        .query_row("SELECT count(*) FROM review_item WHERE pass_id='lesson'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "lesson 项只入队一次");
}

#[test]
fn 抽取契约拒_不入队不落卡() {
    let (_dir, root, uids) = seeded();
    seed_rejected(&root, "fact", Some("number_not_grounded"), &uids, 4, SESSION, DEVICE);
    let plan = select_lesson_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .unwrap();
    // 多给一个 id 键：契约当场拒。
    let mut bad = one_lesson();
    bad["id"] = json!("l1");
    let backend = ScriptBackend::new(vec![Ok(lesson_reply(vec![bad]))]);
    let err = run_lesson(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
        ReviewMode::Inline,
    )
    .unwrap_err();
    assert!(matches!(err, LessonRunError::Contract(_)), "契约拒收要能分辨");
    assert!(cards(&root).is_empty(), "契约拒收不落卡");
    let n: i64 = root
        .memory()
        .conn()
        .query_row("SELECT count(*) FROM review_item WHERE pass_id='lesson'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "契约拒收不入队（审核没有可审的东西）");
}

#[test]
fn 审核_reject_与_uncertain_都不落卡() {
    for (verdict, code) in [("reject", Some("restates")), ("uncertain", None)] {
        let (_dir, root, uids) = seeded();
        seed_rejected(&root, "fact", Some("number_not_grounded"), &uids, 5, SESSION, DEVICE);
        let plan = select_lesson_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
            .unwrap()
            .unwrap();
        let backend = ScriptBackend::two(
            lesson_reply(vec![one_lesson()]),
            review_reply(&[("l1", verdict, code)]),
        );
        let out = run_lesson(
            root.memory(),
            root.transcript(),
            root.ledger(),
            &plan,
            &backend,
            NOW,
            ReviewMode::Inline,
        )
        .unwrap();
        assert_eq!(out.cards, 0, "{verdict} 的草稿不许落卡（EDV default-reject）");
        assert!(cards(&root).is_empty(), "{verdict} 之后卡表必须还是空的");
    }
}

#[test]
fn 审核_pass_落_kind_lesson_卡且可_recall_可复用反馈() {
    let (_dir, root, uids) = seeded();
    seed_rejected(&root, "state", Some("subject_not_in_evidence"), &uids, 6, SESSION, DEVICE);
    let plan = select_lesson_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .unwrap();
    let backend = ScriptBackend::two(
        lesson_reply(vec![one_lesson()]),
        review_reply(&[("l1", "pass", None)]),
    );
    let out = run_lesson(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
        ReviewMode::Inline,
    )
    .unwrap();
    assert_eq!(out.cards, 1);
    assert_eq!(out.state, rc_ledger::pass_state::PassState::Done);
    let rows = cards(&root);
    assert_eq!(rows.len(), 1);
    let (id, kind, title) = &rows[0];
    assert_eq!(kind, "lesson");
    assert_eq!(title, "引用数字前先回到原文");
    // 卡面 = 标题 + 描述 + 内容两段（读卡的人不用回查判定台账）。
    let text: String = root
        .memory()
        .conn()
        .query_row("SELECT text FROM card WHERE id=?1", [id], |r| r.get(0))
        .unwrap();
    assert!(text.contains("防止把记忆里的估算写成语料原文"));
    assert!(text.contains("找不到就写 unknown"));
    // 出处：整批的两轮。
    let prov: i64 = root
        .memory()
        .conn()
        .query_row(
            "SELECT count(*) FROM provenance WHERE mem_table='card' AND mem_id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(prov, 2, "整批两轮都要有出处");
    // 归属：不是合成默认（rc-distill 私有），而是会话的 source_agent。
    let (owner_agent, visibility): (String, String) = root
        .memory()
        .conn()
        .query_row(
            "SELECT owner_agent_id, visibility FROM resource_owner
              WHERE mem_table='card' AND mem_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(owner_agent, "agent-batch-A", "归属取会话的 source_agent");
    assert_eq!(visibility, "private_agent", "旧会话 = 最保守的默认");
    // recall：句柄臂（显式寻址）读得到这一张卡，kind 就是 lesson。
    let key = rc_memory::MemKey {
        table: rc_memory::MemTable::Card,
        id: id.clone(),
    };
    let got = rc_agent::recall::recall(
        root.memory(),
        &rc_agent::recall::RecallRequest::Handles(vec![key]),
        &rc_memory::reader::RecallFilter::empty(),
        NOW,
    )
    .unwrap();
    let v: Value = serde_json::from_str(&got.text()).unwrap();
    assert_eq!(v["rows"][0]["kind"], "lesson", "recall 的卡面必须读出 lesson");
    assert!(
        v["rows"][0]["title"].as_str().unwrap_or("").contains("引用数字"),
        "recall 读的是这张卡的标题：{}",
        v["rows"][0]
    );
    // 波二的复用反馈对 lesson 卡同样可用（卡句柄天然支持）。
    let n = rc_memory::reuse::record_reuse(
        root.memory(),
        id,
        "worked",
        Some("下一次真的先找了原文"),
        NOW,
        "agent-batch-A",
    )
    .unwrap();
    assert!(n > 0, "lesson 卡必须能收到复用反馈");
    let counts = rc_memory::reuse::reuse_counts(root.memory(), id).unwrap();
    assert_eq!(counts.worked, 1);
}

#[test]
fn 多草稿只有_pass_的那条落卡() {
    let (_dir, root, uids) = seeded();
    seed_rejected(&root, "fact", Some("number_not_grounded"), &uids, 7, SESSION, DEVICE);
    let plan = select_lesson_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .unwrap();
    let second = json!({
        "title": "回滚前先留一份旧配置",
        "description": "改配置前适用；防止回滚时没有基线",
        "content": "先复制当前配置再改，回滚时用这一份",
        "error_kind": "judgement"
    });
    let backend = ScriptBackend::two(
        lesson_reply(vec![one_lesson(), second]),
        review_reply(&[("l1", "pass", None), ("l2", "reject", Some("not_actionable"))]),
    );
    let out = run_lesson(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
        ReviewMode::Inline,
    )
    .unwrap();
    assert_eq!(out.rejected, 1);
    assert_eq!(out.cards, 1, "只有 pass 的那一条落卡");
    let rows = cards(&root);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2, "引用数字前先回到原文");
}

#[test]
fn lesson_卡不改_fact_与_event_既有面() {
    let (_dir, root, uids) = seeded();
    // 先铺一条既有事件卡（event 面的代表）：lesson 跑完之后它必须一字不动。
    rc_memory::writers_x::put_card(
        root.memory(),
        &rc_memory::FactContent {
            kind: "event".into(),
            title: "上次部署完成".into(),
            body: "部署脚本跑完，环境就绪".into(),
            scope: rc_memory::Scope {
                kind: rc_memory::ScopeKind::Project,
                key: SESSION.into(),
            },
            valid_to_ms: None,
        },
        &[rc_memory::Provenance {
            turn_uid: uids[0].clone(),
            anchor_role: rc_memory::AnchorRole::Human,
        }],
        NOW,
    )
    .unwrap();
    let before = cards(&root);
    let facts_before: i64 = root
        .memory()
        .conn()
        .query_row("SELECT count(*) FROM fact", [], |r| r.get(0))
        .unwrap();
    seed_rejected(&root, "fact", Some("number_not_grounded"), &uids, 8, SESSION, DEVICE);
    let plan = select_lesson_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .unwrap();
    let backend = ScriptBackend::two(
        lesson_reply(vec![one_lesson()]),
        review_reply(&[("l1", "pass", None)]),
    );
    run_lesson(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
        ReviewMode::Inline,
    )
    .unwrap();
    let after = cards(&root);
    assert_eq!(after.len(), before.len() + 1, "只多一张 lesson 卡");
    assert!(
        before.iter().all(|row| after.contains(row)),
        "既有卡（event）不许被改写或替换"
    );
    assert!(
        after.iter().filter(|row| row.1 != "lesson").count() == before.iter().filter(|row| row.1 != "lesson").count(),
        "非 lesson 的卡一张不多"
    );
    let facts_after: i64 = root
        .memory()
        .conn()
        .query_row("SELECT count(*) FROM fact", [], |r| r.get(0))
        .unwrap();
    assert_eq!(facts_after, facts_before, "lesson 不写 fact 行（没有镜像事实）");
}

#[test]
fn 入队模式只建队不落卡() {
    let (_dir, root, uids) = seeded();
    seed_rejected(&root, "fact", Some("number_not_grounded"), &uids, 9, SESSION, DEVICE);
    let plan = select_lesson_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .unwrap();
    // 只回抽取那一条：入队模式下审核模型一次都不该被问。
    let backend = ScriptBackend::new(vec![Ok(lesson_reply(vec![one_lesson()]))]);
    let out = run_lesson(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
        ReviewMode::EnqueueOnly,
    )
    .unwrap();
    assert_eq!(out.cards, 0);
    assert_eq!(backend.calls.get(), 1, "入队模式只调抽取一次");
    assert!(cards(&root).is_empty());
    let state: String = root
        .memory()
        .conn()
        .query_row(
            "SELECT state FROM review_item WHERE pass_id='lesson'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "pending_review");
}

// ── 波三修 1：终态行（空产出 / 契约拒收 / 完工都不许下一夜重选同一组） ─────────────

/// ① 空产出：这一批落 `empty` 终态；第二次选批整组被挡（零模型调用、零新卡）。
#[test]
fn 空产出落_empty_终态_第二次不再选中() {
    let (_dir, root, uids) = seeded();
    seed_rejected(&root, "fact", Some("number_not_grounded"), &uids, 10, SESSION, DEVICE);
    let plan = select_lesson_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .unwrap();
    // 只给一条抽取回复：第二次若还被选中，后端会交回 Timeout 并把这发记成失败。
    let backend = ScriptBackend::new(vec![Ok(lesson_reply(vec![]))]);
    let out = run_lesson(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
        ReviewMode::Inline,
    )
    .unwrap();
    assert_eq!(out.state, rc_ledger::pass_state::PassState::Empty);
    assert_eq!(backend.calls.get(), 1, "空产出只问一次模型");
    for uid in &uids {
        assert_eq!(
            lesson_state(&root, uid).unwrap().0,
            "empty",
            "空产出必须落终态：{uid}"
        );
    }
    let again = select_lesson_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap();
    assert!(again.is_none(), "同一组第二轮不许再被选中（此前每夜重烧一次）");
    assert!(cards(&root).is_empty());
}

/// ② 抽取契约拒收：这一批落 `rejected(<slug>)` 终态；第二次选批不再选中。
#[test]
fn 抽取契约拒收落_rejected_终态_第二次不再选中() {
    let (_dir, root, uids) = seeded();
    seed_rejected(&root, "fact", Some("number_not_grounded"), &uids, 11, SESSION, DEVICE);
    let plan = select_lesson_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .unwrap();
    let mut bad = one_lesson();
    bad["id"] = json!("l1"); // 多键 ⇒ 契约当场拒
    let backend = ScriptBackend::new(vec![Ok(lesson_reply(vec![bad]))]);
    let err = run_lesson(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
        ReviewMode::Inline,
    )
    .unwrap_err();
    assert!(matches!(err, LessonRunError::Contract(_)), "契约拒收要能分辨");
    for uid in &uids {
        let (state, reason) = lesson_state(&root, uid).expect("契约拒收必须落终态行");
        assert_eq!(state, "rejected", "契约拒收的终态是 rejected：{uid}");
        assert_eq!(reason.as_deref(), Some("json"), "reason 是 parser 的 slug：{uid}");
    }
    let again = select_lesson_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap();
    assert!(again.is_none(), "契约拒收之后同一组不许再被选中");
}

/// ③ 审核完成（落卡）：这一批落 `done` 终态；第二次选批不再选中。
#[test]
fn 正常完成落_done_终态_第二次不再选中() {
    let (_dir, root, uids) = seeded();
    seed_rejected(&root, "fact", Some("number_not_grounded"), &uids, 12, SESSION, DEVICE);
    let plan = select_lesson_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .unwrap();
    let backend = ScriptBackend::two(
        lesson_reply(vec![one_lesson()]),
        review_reply(&[("l1", "pass", None)]),
    );
    let out = run_lesson(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
        ReviewMode::Inline,
    )
    .unwrap();
    assert_eq!(out.cards, 1);
    for uid in &uids {
        assert_eq!(
            lesson_state(&root, uid).unwrap().0,
            "done",
            "正常完成必须落 done 终态：{uid}"
        );
    }
    let again = select_lesson_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap();
    assert!(again.is_none(), "完工之后同一组不许再被选中");
}

/// ④ 同会话同错误族后到的新 rejected 行：旧终态只封住旧轮，新轮照样开新批。
#[test]
fn 同组后到的新_rejected_行能开新组() {
    let (_dir, root, uids) = seeded();
    assert_eq!(uids.len(), 2);
    // 第一组只覆盖第 1 轮；跑一次空产出，第 1 轮落 empty 终态。
    seed_rejected(&root, "fact", Some("number_not_grounded"), &uids[..1], 13, SESSION, DEVICE);
    let plan = select_lesson_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .unwrap();
    assert_eq!(plan.batch.rows.len(), 1);
    let backend = ScriptBackend::new(vec![Ok(lesson_reply(vec![]))]);
    run_lesson(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
        ReviewMode::Inline,
    )
    .unwrap();
    assert_eq!(lesson_state(&root, &uids[0]).unwrap().0, "empty");
    // 后到一条同会话同族、指向第 2 轮的新 rejected 行。
    seed_rejected(&root, "fact", Some("number_not_grounded"), &uids[1..2], 14, SESSION, DEVICE);
    let next = select_lesson_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("新 rejected 行必须能开新组（旧终态不许把整组钉死）");
    assert_eq!(next.error_code, "number_not_grounded");
    assert_eq!(next.source_items, 2, "这一组现在覆盖两条被驳回的 item");
    assert_eq!(next.batch.rows.len(), 1, "只把没有终态的那一轮组进新批");
    assert_eq!(next.batch.rows[0].turn_uid, uids[1]);
}
