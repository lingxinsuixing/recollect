//! 波三刀 A：lesson 的契约面——三元组抽取与独立审核的"模型给什么形状、程序怎么拒"。
//!
//! 这一层不选批、不调模型、不写库、不做 CLI。正反例只用通用名与自编句，不抄任何语料原句。
//!
//! 覆盖派发档 §7 的契约三坏（缺格 / 超长 / 闭集外 / 多给 id 键）与审核 parser 的四格
//! （duplicate_ref / uncovered / code 规则 / 越界 ref），外加"空数组是合法产出"。

use rc_distill::pass_lesson_contract::{
    parse_lesson_output, parse_lesson_review, LessonPassError, LESSON_DESCRIPTION_MAX_CHARS,
    LESSON_ERROR_KINDS, LESSON_MAX_PER_BATCH, LESSON_REVIEW_CODES, LESSON_TITLE_MAX_CHARS,
    PASS_LESSON, PASS_LESSON_REVIEW,
};
use rc_distill::{Batch, TurnRow};
use serde_json::{json, Value};

const NOW: i64 = 1_792_994_700_000;

fn uid(index: usize) -> String {
    format!("{:064x}", index + 1)
}

fn batch(bodies: &[&str]) -> Batch {
    Batch {
        session_key: "sess-lesson".into(),
        device_id: "dev-lesson".into(),
        source_agent: "agent-lesson".into(),
        project_hint: None,
        rows: bodies
            .iter()
            .enumerate()
            .map(|(index, body)| TurnRow {
                turn_uid: uid(index),
                role: if index % 2 == 0 { "human".into() } else { "agent".into() },
                body: (*body).to_string(),
                created_at_ms: NOW + index as i64 * 60_000,
                ingested_at_ms: NOW + index as i64 * 60_000,
                speaker: None,
                speaker_anchor_id: None,
            })
            .collect(),
    }
}

fn lesson(title: &str, description: &str, content: &str, error_kind: &str) -> Value {
    json!({
        "title": title, "description": description, "content": content,
        "error_kind": error_kind
    })
}

/// 一条合法三元组。
fn good_lesson() -> Value {
    lesson(
        "引用数字前先回到原文",
        "输出里要出现数量或日期时适用；防止把记忆里的估算写成语料原文",
        "写任何数字前先在引文里找到同一个数字；找不到就写 unknown，并说明原文没有给数",
        "grounding",
    )
}

fn output(lessons: Vec<Value>) -> String {
    json!({"schema_version": 0, "pass": PASS_LESSON, "lessons": lessons}).to_string()
}

fn review(verdicts: Vec<Value>) -> String {
    json!({"schema_version": 0, "pass": PASS_LESSON_REVIEW, "verdicts": verdicts}).to_string()
}

#[test]
fn 合法三元组逐格还原且程序补代表轮与整批证据() {
    let b = batch(&["先把计数报出来", "那个数原文里没有"]);
    let drafts = parse_lesson_output(&output(vec![good_lesson()]), &b, "number_not_grounded")
        .expect("合法输出必须过契约");
    assert_eq!(drafts.len(), 1);
    let d = &drafts[0];
    assert_eq!(d.reference, "l1", "位置编号即契约");
    assert_eq!(d.turn_uid, uid(0), "代表轮取批内第一轮（模型写不进来）");
    assert_eq!(d.error_code, "number_not_grounded", "错因由程序给");
    assert_eq!(
        d.evidence_turn_uids,
        vec![uid(0), uid(1)],
        "证据是整批的轮（顺序即批序）"
    );
    assert_eq!(d.error_kind, "grounding");
}

#[test]
fn 空数组是合法产出不是形状错() {
    let b = batch(&["这一段什么都没发生"]);
    let drafts = parse_lesson_output(&output(vec![]), &b, "review_reject")
        .expect("lessons=[] 是合法空批");
    assert!(drafts.is_empty());
}

#[test]
fn 缺格与空格都按形状拒() {
    let b = batch(&["一句话"]);
    for bad in [
        json!({"title": "标题", "description": "描述", "content": "内容"}),
        lesson("   ", "描述", "内容", "grounding"),
        lesson("标题", "", "内容", "grounding"),
        lesson("标题", "描述", " ", "grounding"),
    ] {
        let err = parse_lesson_output(&output(vec![bad]), &b, "review_reject").unwrap_err();
        assert!(
            matches!(err, LessonPassError::Json | LessonPassError::EmptyField(_)),
            "缺格/空格必须拒：{err:?}"
        );
    }
}

#[test]
fn 超长整批拒收不截断() {
    let b = batch(&["一句话"]);
    let long_title = "字".repeat(LESSON_TITLE_MAX_CHARS + 1);
    let long_description = "字".repeat(LESSON_DESCRIPTION_MAX_CHARS + 1);
    for bad in [
        lesson(&long_title, "描述", "内容", "grounding"),
        lesson("标题", &long_description, "内容", "grounding"),
    ] {
        let err = parse_lesson_output(&output(vec![bad]), &b, "review_reject").unwrap_err();
        assert!(matches!(err, LessonPassError::TooLong(_)), "超长是拒收不是截断：{err:?}");
    }
    // 上界本身合法（60 / 400 是上界，不是建议）。
    let exact = lesson(
        &"a".repeat(LESSON_TITLE_MAX_CHARS),
        &"b".repeat(LESSON_DESCRIPTION_MAX_CHARS),
        &"c".repeat(400),
        "quoting",
    );
    assert!(parse_lesson_output(&output(vec![exact]), &b, "review_reject").is_ok());
}

#[test]
fn 闭集外的错误轴与模型自己写的_id_都整批拒() {
    let b = batch(&["一句话"]);
    // 合法输出先过一遍：下面的坏例子必须因为"闭集/多键"被拒，不是别的原因。
    assert!(parse_lesson_output(&output(vec![good_lesson()]), &b, "review_reject").is_ok());

    let bad_axis = lesson("标题", "描述", "内容", "quality");
    assert_eq!(
        parse_lesson_output(&output(vec![bad_axis]), &b, "review_reject").unwrap_err(),
        LessonPassError::BadErrorKind
    );

    // 多给一个 id 键：`deny_unknown_fields` 当场拒（模型写不进任何 id）。
    let mut with_id = good_lesson();
    with_id["id"] = json!("l1");
    assert_eq!(
        parse_lesson_output(&output(vec![with_id]), &b, "review_reject").unwrap_err(),
        LessonPassError::Json
    );

    let mut with_turn = good_lesson();
    with_turn["turn_uid"] = json!(uid(0));
    assert_eq!(
        parse_lesson_output(&output(vec![with_turn]), &b, "review_reject").unwrap_err(),
        LessonPassError::Json
    );
}

#[test]
fn 一次超过三条整批拒() {
    let b = batch(&["一句话"]);
    let many: Vec<Value> = (0..=LESSON_MAX_PER_BATCH).map(|_| good_lesson()).collect();
    assert_eq!(
        parse_lesson_output(&output(many), &b, "review_reject").unwrap_err(),
        LessonPassError::TooManyLessons
    );
}

#[test]
fn 版本与_pass_值都是硬门() {
    let b = batch(&["一句话"]);
    let wrong_version = json!({"schema_version": 1, "pass": PASS_LESSON,
                               "lessons": [good_lesson()]}).to_string();
    assert_eq!(
        parse_lesson_output(&wrong_version, &b, "review_reject").unwrap_err(),
        LessonPassError::Version
    );
    let wrong_pass = json!({"schema_version": 0, "pass": "event",
                            "lessons": [good_lesson()]}).to_string();
    assert_eq!(
        parse_lesson_output(&wrong_pass, &b, "review_reject").unwrap_err(),
        LessonPassError::Pass
    );
}

#[test]
fn 审核判决按_pass_与_reject_必带闭集码() {
    // pass 的 code 必须是 null。
    let ok = review(vec![json!({"ref": "l1", "verdict": "pass", "code": null})]);
    let got = parse_lesson_review(&ok, &["l1".to_string()]).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1, rc_distill::pass_fact_contract::ReviewVerdict::Pass);

    // reject 必须带闭集码。
    let missing = review(vec![json!({"ref": "l1", "verdict": "reject", "code": null})]);
    assert_eq!(
        parse_lesson_review(&missing, &["l1".to_string()]).unwrap_err(),
        LessonPassError::Json
    );
    let outside = review(vec![json!({"ref": "l1", "verdict": "reject", "code": "nope"})]);
    assert!(LESSON_REVIEW_CODES.iter().all(|code| *code != "nope"));
    assert_eq!(
        parse_lesson_review(&outside, &["l1".to_string()]).unwrap_err(),
        LessonPassError::Json
    );
    let good = review(vec![json!({"ref": "l1", "verdict": "reject", "code": "restates"})]);
    assert!(parse_lesson_review(&good, &["l1".to_string()]).is_ok());
}

#[test]
fn 审核的重复引用覆盖缺口与越界引用都由契约拒() {
    let refs = vec!["l1".to_string(), "l2".to_string()];
    let dup = review(vec![
        json!({"ref": "l1", "verdict": "pass", "code": null}),
        json!({"ref": "l1", "verdict": "pass", "code": null}),
    ]);
    assert_eq!(
        parse_lesson_review(&dup, &refs).unwrap_err(),
        LessonPassError::DuplicateRef("l1".to_string())
    );

    let missing = review(vec![json!({"ref": "l1", "verdict": "pass", "code": null})]);
    assert_eq!(
        parse_lesson_review(&missing, &refs).unwrap_err(),
        LessonPassError::Uncovered(vec!["l2".to_string()])
    );

    let outside = review(vec![json!({"ref": "l9", "verdict": "pass", "code": null})]);
    assert_eq!(parse_lesson_review(&outside, &refs).unwrap_err(), LessonPassError::Json);
}

#[test]
fn 错误轴的闭集与审核码闭集各是五格() {
    assert_eq!(LESSON_ERROR_KINDS.len(), 5);
    assert_eq!(LESSON_REVIEW_CODES.len(), 5);
    assert!(LESSON_ERROR_KINDS.contains(&"judgement"));
    assert!(!LESSON_ERROR_KINDS.contains(&"quality"));
}

// ── prompt 形状：一次一个语义任务、闭集在案、例子不串门 ───────────────────────

use rc_distill::pass_lesson_contract::LessonDraft;
use rc_distill::pass_prompt::{
    lesson_review_system, lesson_review_user, lesson_system, lesson_user, ReviewInputContext,
    LESSON_CANARY, LESSON_PROGRAM_ERROR_NOTE, LESSON_REVIEW_CANARY,
};

fn ctx(b: &Batch) -> ReviewInputContext {
    ReviewInputContext {
        session_key: b.session_key.clone(),
        owner_agent_id: "owner-lesson".into(),
        visibility: "private_agent".into(),
        scope: "project:sess-lesson".into(),
        speakers: b.rows.iter().map(|row| format!("{} 甲", row.role)).collect(),
    }
}

#[test]
fn 抽取_prompt_首行_canary_且闭集与上界在案() {
    let system = lesson_system();
    assert!(system.starts_with(LESSON_CANARY), "system 首行必须是自己的 canary");
    assert!(!system.contains("__EVENT_CANARY"), "两次独立调用不许互相冒充");
    for kind in LESSON_ERROR_KINDS {
        assert!(system.contains(kind), "错误轴 {kind} 必须在 prompt 里点名");
    }
    assert!(system.contains("\"pass\":\"lesson\""), "输出契约的 pass 值必须在案");
    assert!(!system.contains("\"pass\":\"event\""), "lesson 的契约不许抄 event 的 pass 值");
    // 长度上界与条数上界：模型要看到数字，不是"尽量短"。
    assert!(system.contains("60"), "标题上界必须在案");
    assert!(system.contains("400"), "描述/内容上界必须在案");
    assert!(system.contains("three"), "一次最多三条必须在案");
}

#[test]
fn 抽取_user_只给程序错因与这一段的目标轮() {
    let b = batch(&["先把计数报出来", "那个数原文里没有"]);
    let user = lesson_user(&b, "number_not_grounded");
    assert!(user.starts_with("PASS lesson v0"));
    assert!(user.contains("ERROR FAMILY: number_not_grounded"));
    assert!(user.contains(LESSON_PROGRAM_ERROR_NOTE));
    assert!(user.contains("TARGET [t1]"), "第一轮必须有位置编号");
    assert!(user.contains("那个数原文里没有"), "正文原样进 prompt");
    assert!(!user.contains("CANDIDATES"), "lesson 不写锚，候选表不该出现");
}

#[test]
fn 审核_prompt_独立_canary_且只问这一段对不对() {
    let system = lesson_review_system();
    assert!(system.starts_with(LESSON_REVIEW_CANARY));
    assert!(!system.contains(LESSON_CANARY), "抽取与审核的 canary 不许同串");
    for code in LESSON_REVIEW_CODES {
        assert!(system.contains(code), "审核码 {code} 必须在 prompt 里点名");
    }
    assert!(system.contains("\"pass\":\"lesson_review\""));

    let b = batch(&["先把计数报出来"]);
    let draft = LessonDraft {
        reference: "l1".into(),
        turn_uid: uid(0),
        title: "引用数字前先回到原文".into(),
        description: "要出现数字时适用".into(),
        content: "先在原文里找到它".into(),
        error_kind: "grounding".into(),
        error_code: "number_not_grounded".into(),
        evidence_turn_uids: vec![uid(0)],
    };
    let user = lesson_review_user(&[draft], &b, &ctx(&b), "number_not_grounded");
    assert!(user.starts_with("PASS lesson_review v0"));
    assert!(user.contains("ITEM l1"));
    assert!(user.contains("ERROR FAMILY: number_not_grounded"));
    assert!(user.contains("TARGET [t1]"), "审核必须看得到这一段");
    assert!(!user.contains(&uid(0)), "审核输入不带 turn_uid（位置即契约）");
}
