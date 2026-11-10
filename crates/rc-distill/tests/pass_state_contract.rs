//! P2 状态 pass 的契约层（方案 `design/2026-10-31-state-pass-p2.md` §3–§4）。
//!
//! 这一层只断"模型给什么形状、程序怎么判"：不选批、不调模型、不写库、不做 CLI。
//! 三条主轴：
//! 1. **值逐字 = `expressed`**：`value_after ⊂ quote` 才进审核队列；值需要换词的那条进
//!    `inferred`（隔离），既不是草稿也不是丢弃，但**占住**那一轮的覆盖；
//! 2. **陈述性**（方案 §7 的阴性对照）：问句与纯宣布被 `not_assertive` 拒；
//! 3. 信封坏整批拒，单条坏只丢那一条（与 event 同一条宽松纪律）。
//!
//! 正反例只用通用占位名（张三/小海/某网关），不含任何语料专名或 holdout。

use rc_distill::batch::ExtractCtx;
use rc_distill::pass_state_contract::{
    parse_state_output, parse_state_output_lenient, parse_state_review, StatePassError,
};
use rc_distill::pass_fact_contract::ReviewVerdict;
use rc_distill::pass_prompt::{
    state_review_shape, state_review_system, state_review_user, state_shape, state_system,
    state_user, ExistingState, STATE_CANARY, STATE_DIMENSIONS, STATE_REVIEW_CANARY,
    STATE_REVIEW_CODES,
};
use rc_distill::{Batch, TurnRow};
use serde_json::json;

const NOW: i64 = 1_792_994_700_000;
const ANXIOUS_BODY: &str = "我这两天焦虑得很，晚上睡不着。";

fn uid(index: usize) -> String {
    format!("{:064x}", index + 1)
}

fn anchor_id(index: usize) -> String {
    format!("{:064x}", 0xa0 + index)
}

fn batch(bodies: &[&str]) -> Batch {
    Batch {
        session_key: "sess-state".into(),
        device_id: "dev-state".into(),
        source_agent: "agent-state".into(),
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

fn candidate(index: usize, kind: &str, canonical: &str) -> rc_distill::FactCandidate {
    rc_distill::FactCandidate {
        reference: format!("a{index}"),
        kind: kind.into(),
        canonical: canonical.into(),
        anchor_id: anchor_id(index),
        mark: None,
    }
}

fn candidates() -> Vec<rc_distill::FactCandidate> {
    vec![
        candidate(1, "person", "张三"),
        candidate(2, "person", "小海"),
        candidate(3, "animal", "猫"),
        candidate(4, "system", "某网关"),
    ]
}

/// 一条合法的状态行 JSON（键顺序与契约一致）。
fn row(reference: &str, turn: &str, subject: &str, dimension: &str, value: &str, quote: &str)
    -> serde_json::Value {
    json!({
        "ref": reference, "turn": turn, "subject": subject, "dimension": dimension,
        "value_after": value, "quote": quote, "evidence": [turn]
    })
}

fn output(states: Vec<serde_json::Value>, empty: Vec<&str>) -> String {
    json!({
        "schema_version": 0, "pass": "state", "states": states, "empty_turns": empty
    })
    .to_string()
}

fn slug_of(error: &StatePassError) -> &'static str {
    error.slug()
}

// ── 合法形状 ────────────────────────────────────────────────────────────────────

#[test]
fn one_direct_statement_is_an_expressed_draft() {
    let b = batch(&[ANXIOUS_BODY]);
    let raw = output(
        vec![row("s1", "t1", "a1", "emotion", "焦虑得很", ANXIOUS_BODY)],
        Vec::new(),
    );
    let got = parse_state_output_lenient(&raw, &b, &candidates()).unwrap();
    assert_eq!(got.drafts.len(), 1);
    assert_eq!(got.inferred.len(), 0);
    assert_eq!(got.dropped.len(), 0);
    assert_eq!(got.uncovered.len(), 0);
    let draft = &got.drafts[0];
    assert_eq!(draft.turn_uid, uid(0));
    assert_eq!(draft.subject_anchor_id, anchor_id(1));
    assert_eq!(draft.subject_canonical, "张三");
    assert_eq!(draft.dimension, "emotion");
    assert_eq!(draft.value_after, "焦虑得很");
    assert_eq!(draft.mode, "expressed");
    assert_eq!(draft.evidence_turn_uids, vec![uid(0)]);
}

#[test]
fn several_dimensions_of_one_subject_are_several_rows() {
    let b = batch(&[ANXIOUS_BODY]);
    let raw = output(
        vec![
            row("s1", "t1", "a1", "emotion", "焦虑得很", ANXIOUS_BODY),
            row("s2", "t1", "a1", "health", "睡不着", ANXIOUS_BODY),
        ],
        Vec::new(),
    );
    let got = parse_state_output_lenient(&raw, &b, &candidates()).unwrap();
    assert_eq!(got.drafts.len(), 2);
    assert_eq!(got.dropped.len(), 0);
}

#[test]
fn a_third_party_or_an_animal_can_be_the_subject() {
    // 探针结论的回归位：主体锁死在"我"时 DSH 陪伴轮出 0 条。
    let bodies = ["小海不喜欢别人碰他的键盘。", "我家猫这两天不吃东西，蔫得很。"];
    let b = batch(&bodies);
    let raw = output(
        vec![
            row("s1", "t1", "a2", "sentiment", "不喜欢", bodies[0]),
            row("s2", "t2", "a3", "health", "不吃东西", bodies[1]),
            row("s3", "t2", "a3", "mood", "蔫得很", bodies[1]),
        ],
        Vec::new(),
    );
    let got = parse_state_output_lenient(&raw, &b, &candidates()).unwrap();
    assert_eq!(got.drafts.len(), 3);
    assert_eq!(got.drafts[1].subject_anchor_id, anchor_id(3));
}

#[test]
fn empty_turns_are_a_legal_answer_and_cover_the_batch() {
    let b = batch(&[ANXIOUS_BODY, "你今天有没有好一点？"]);
    let raw = output(Vec::new(), vec!["t1", "t2"]);
    let got = parse_state_output_lenient(&raw, &b, &candidates()).unwrap();
    assert_eq!(got.drafts.len(), 0);
    assert_eq!(got.uncovered.len(), 0);
    assert_eq!(got.dropped.len(), 0);
}

// ── 阴性对照：闸必须拒掉不陈述状态的引文 ───────────────────────────────────────

#[test]
fn a_question_is_rejected_as_not_assertive() {
    let question = "你是不是生气了？";
    let b = batch(&[question]);
    let raw = output(vec![row("s1", "t1", "a1", "emotion", "生气", question)], Vec::new());
    let got = parse_state_output_lenient(&raw, &b, &candidates()).unwrap();
    assert_eq!(got.drafts.len(), 0, "问句不许落库");
    assert_eq!(got.inferred.len(), 0);
    assert_eq!(got.dropped.len(), 1);
    assert_eq!(slug_of(&got.dropped[0]), "not_assertive");
}

#[test]
fn an_instruction_style_question_is_rejected_too() {
    let question = "我们能不能先看一下那三个会话？";
    let b = batch(&[question]);
    let raw = output(vec![row("s1", "t1", "a1", "progress", "看一下", question)], Vec::new());
    let got = parse_state_output_lenient(&raw, &b, &candidates()).unwrap();
    assert_eq!(slug_of(&got.dropped[0]), "not_assertive");
}

#[test]
fn a_pure_announcement_is_rejected_as_not_assertive() {
    let announcement = "现在我去跑一遍全量测试";
    let b = batch(&[announcement]);
    let raw = output(
        vec![row("s1", "t1", "a1", "progress", "跑一遍全量测试", announcement)],
        Vec::new(),
    );
    let got = parse_state_output_lenient(&raw, &b, &candidates()).unwrap();
    assert_eq!(got.drafts.len(), 0);
    assert_eq!(slug_of(&got.dropped[0]), "not_assertive");
}

/// 陈述性判据不能砍掉真陈述：带"现在/已经"的直陈仍然算陈述（阴性对照的阳性对照）。
#[test]
fn a_statement_with_a_time_word_is_still_assertive() {
    let body = "现在服务是好的，我心情也缓过来了。";
    let b = batch(&[body]);
    let raw = output(
        vec![
            row("s1", "t1", "a4", "config", "是好的", body),
            row("s2", "t1", "a1", "mood", "缓过来了", body),
        ],
        Vec::new(),
    );
    let got = parse_state_output_lenient(&raw, &b, &candidates()).unwrap();
    assert_eq!(got.drafts.len(), 2, "两条直陈都不许被陈述性闸误杀");
}

// ── inferred 的分岔 ────────────────────────────────────────────────────────────

#[test]
fn a_reworded_value_is_isolated_not_dropped() {
    let body = "我最近压力挺大的";
    let b = batch(&[body]);
    let raw = output(vec![row("s1", "t1", "a1", "emotion", "焦虑", body)], Vec::new());
    let got = parse_state_output_lenient(&raw, &b, &candidates()).unwrap();
    assert_eq!(got.drafts.len(), 0, "换过词的值不是直陈");
    assert_eq!(got.dropped.len(), 0, "它也不是拒收");
    assert_eq!(got.inferred.len(), 1);
    assert_eq!(got.inferred_turns, vec!["t1".to_string()]);
    // 隔离的那一轮仍然算"被回答过"，不许同时出现在覆盖缺口里。
    assert_eq!(got.uncovered.len(), 0);
}

#[test]
fn an_expressed_row_wins_over_an_inferred_row_of_the_same_turn() {
    let body = "我最近压力挺大的";
    let b = batch(&[body]);
    let raw = output(
        vec![
            row("s1", "t1", "a1", "emotion", "焦虑", body),
            row("s2", "t1", "a1", "emotion", "压力挺大", body),
        ],
        Vec::new(),
    );
    let got = parse_state_output_lenient(&raw, &b, &candidates()).unwrap();
    assert_eq!(got.drafts.len(), 1);
    assert_eq!(got.inferred_turns.len(), 1);
    assert_eq!(got.inferred.len(), 1);
}

/// 严格 parser 没有"隔离"这一格：推导值按最贴近的引文码整批拒。
#[test]
fn strict_parser_rejects_a_reworded_value_for_the_whole_batch() {
    let body = "我最近压力挺大的";
    let b = batch(&[body]);
    let raw = output(vec![row("s1", "t1", "a1", "emotion", "焦虑", body)], Vec::new());
    let error = parse_state_output(&raw, &b, &candidates()).unwrap_err();
    assert_eq!(slug_of(&error), "quote_not_verbatim");
}

// ── 单条拒收的每一格 ───────────────────────────────────────────────────────────

fn dropped_slug_for(state: serde_json::Value, bodies: &[&str]) -> &'static str {
    let b = batch(bodies);
    let raw = output(vec![state], Vec::new());
    let got = parse_state_output_lenient(&raw, &b, &candidates()).unwrap();
    assert_eq!(got.dropped.len(), 1, "只该丢这一条：{got:?}");
    slug_of(&got.dropped[0])
}

#[test]
fn dimension_outside_the_closed_set_is_dropped() {
    assert_eq!(
        dropped_slug_for(
            row("s1", "t1", "a1", "feeling", "焦虑得很", ANXIOUS_BODY),
            &[ANXIOUS_BODY]
        ),
        "dimension_invalid"
    );
    // 九个词逐条都能过（值都在引文里）。
    for dimension in STATE_DIMENSIONS {
        let got = parse_state_output_lenient(
            &output(vec![row("s1", "t1", "a1", dimension, "焦虑得很", ANXIOUS_BODY)], Vec::new()),
            &batch(&[ANXIOUS_BODY]),
            &candidates(),
        )
        .unwrap();
        assert_eq!(got.drafts.len(), 1, "{dimension} 是闭集里的词");
    }
}

#[test]
fn blank_value_is_dropped() {
    assert_eq!(
        dropped_slug_for(row("s1", "t1", "a1", "emotion", "   ", ANXIOUS_BODY), &[ANXIOUS_BODY]),
        "value_empty"
    );
}

#[test]
fn quote_not_from_the_target_turn_is_dropped() {
    assert_eq!(
        dropped_slug_for(
            row("s1", "t1", "a1", "emotion", "焦虑得很", "他今天心情很好"),
            &[ANXIOUS_BODY]
        ),
        "quote_not_verbatim"
    );
}

#[test]
fn ellipsis_in_the_quote_is_dropped() {
    assert_eq!(
        dropped_slug_for(
            row("s1", "t1", "a1", "emotion", "焦虑", "我这两天焦虑…"),
            &[ANXIOUS_BODY]
        ),
        "ellipsis"
    );
}

#[test]
fn overlong_quote_is_dropped() {
    let long = format!("{}{}", "焦虑得很", "啊".repeat(120));
    let slug = dropped_slug_for(row("s1", "t1", "a1", "emotion", "焦虑得很", &long), &[&long]);
    assert_eq!(slug, "quote_too_long");
}

#[test]
fn subject_outside_the_candidate_table_is_dropped() {
    assert_eq!(
        dropped_slug_for(row("s1", "t1", "a9", "emotion", "焦虑得很", ANXIOUS_BODY), &[ANXIOUS_BODY]),
        "unknown_candidate"
    );
}

#[test]
fn turn_outside_the_batch_is_dropped() {
    assert_eq!(
        dropped_slug_for(row("s1", "t9", "a1", "emotion", "焦虑得很", ANXIOUS_BODY), &[ANXIOUS_BODY]),
        "unknown_turn"
    );
}

#[test]
fn evidence_that_omits_its_own_turn_is_dropped() {
    let b = batch(&[ANXIOUS_BODY, "另一轮"]);
    let raw = json!({
        "schema_version": 0, "pass": "state",
        "states": [{
            "ref": "s1", "turn": "t1", "subject": "a1", "dimension": "emotion",
            "value_after": "焦虑得很", "quote": ANXIOUS_BODY, "evidence": ["t2"]
        }],
        "empty_turns": ["t2"]
    })
    .to_string();
    let got = parse_state_output_lenient(&raw, &b, &candidates()).unwrap();
    assert_eq!(slug_of(&got.dropped[0]), "evidence_not_target");
}

#[test]
fn empty_evidence_is_dropped() {
    let b = batch(&[ANXIOUS_BODY]);
    let raw = json!({
        "schema_version": 0, "pass": "state",
        "states": [{
            "ref": "s1", "turn": "t1", "subject": "a1", "dimension": "emotion",
            "value_after": "焦虑得很", "quote": ANXIOUS_BODY, "evidence": []
        }],
        "empty_turns": []
    })
    .to_string();
    let got = parse_state_output_lenient(&raw, &b, &candidates()).unwrap();
    assert_eq!(slug_of(&got.dropped[0]), "empty_evidence");
}

#[test]
fn duplicate_ref_drops_the_second_row_only() {
    let b = batch(&[ANXIOUS_BODY]);
    let raw = output(
        vec![
            row("s1", "t1", "a1", "emotion", "焦虑得很", ANXIOUS_BODY),
            row("s1", "t1", "a1", "health", "睡不着", ANXIOUS_BODY),
        ],
        Vec::new(),
    );
    let got = parse_state_output_lenient(&raw, &b, &candidates()).unwrap();
    assert_eq!(got.drafts.len(), 1);
    assert_eq!(slug_of(&got.dropped[0]), "duplicate_ref");
}

#[test]
fn a_missed_turn_is_reported_as_uncovered_not_a_batch_failure() {
    let b = batch(&[ANXIOUS_BODY, "小海不喜欢别人碰他的键盘。"]);
    let raw = output(vec![row("s1", "t1", "a1", "emotion", "焦虑得很", ANXIOUS_BODY)], Vec::new());
    let got = parse_state_output_lenient(&raw, &b, &candidates()).unwrap();
    assert_eq!(got.uncovered, vec!["t2".to_string()]);
    assert_eq!(got.drafts.len(), 1, "覆盖缺口不折整批");
    // 严格侧仍是整批错。
    assert_eq!(
        slug_of(&parse_state_output(&raw, &b, &candidates()).unwrap_err()),
        "uncovered"
    );
}

// ── 信封：整批拒 ───────────────────────────────────────────────────────────────

#[test]
fn envelope_failures_reject_the_whole_batch() {
    let b = batch(&[ANXIOUS_BODY]);
    let c = candidates();
    let good = row("s1", "t1", "a1", "emotion", "焦虑得很", ANXIOUS_BODY);
    let cases: Vec<(String, &str)> = vec![
        ("不是 json".to_string(), "json"),
        (json!({"schema_version": 1, "pass": "state", "states": [good], "empty_turns": []})
            .to_string(), "version"),
        (json!({"schema_version": 0, "pass": "event", "states": [good], "empty_turns": []})
            .to_string(), "pass"),
        // 多一个键（模型自己写 mode/confidence 是这一片明令禁止的形状）。
        (json!({"schema_version": 0, "pass": "state", "states": [
            { "ref": "s1", "turn": "t1", "subject": "a1", "dimension": "emotion",
              "value_after": "焦虑得很", "quote": ANXIOUS_BODY, "evidence": ["t1"],
              "mode": "expressed" }], "empty_turns": []})
            .to_string(), "json"),
        // empty_turns 引用批外轮：批次级声明坏 ⇒ 整批拒。
        (json!({"schema_version": 0, "pass": "state", "states": [good], "empty_turns": ["t7"]})
            .to_string(), "unknown_turn"),
        (json!({"schema_version": 0, "pass": "state", "states": [good], "empty_turns": ["t1", "t1"]})
            .to_string(), "duplicate_ref"),
    ];
    for (raw, slug) in cases {
        let error = parse_state_output_lenient(&raw, &b, &c).unwrap_err();
        assert_eq!(slug_of(&error), slug, "{raw} 的落点不对");
    }
}

#[test]
fn a_fenced_reply_is_parsed_like_a_bare_one() {
    let b = batch(&[ANXIOUS_BODY]);
    let inner = output(vec![row("s1", "t1", "a1", "emotion", "焦虑得很", ANXIOUS_BODY)], Vec::new());
    let fenced = format!("```json\n{inner}\n```");
    assert_eq!(parse_state_output_lenient(&fenced, &b, &candidates()).unwrap().drafts.len(), 1);
}

// ── 窄审 parser ────────────────────────────────────────────────────────────────

fn review_raw(verdicts: serde_json::Value) -> String {
    json!({"schema_version": 0, "pass": "state_review", "verdicts": verdicts}).to_string()
}

#[test]
fn review_verdicts_follow_the_requested_refs_order() {
    let refs = vec!["s1".to_string(), "s2".to_string()];
    let raw = review_raw(json!([
        {"ref": "s2", "verdict": "reject", "code": "value_drift"},
        {"ref": "s1", "verdict": "pass", "code": null}
    ]));
    let got = parse_state_review(&raw, &refs).unwrap();
    assert_eq!(got[0].0, "s1");
    assert_eq!(got[0].1, ReviewVerdict::Pass);
    assert_eq!(got[0].2, None);
    assert_eq!(got[1].0, "s2");
    assert_eq!(got[1].1, ReviewVerdict::Reject);
    assert_eq!(got[1].2.as_deref(), Some("value_drift"));
}

#[test]
fn review_missing_or_extra_refs_are_caught() {
    let raw = review_raw(json!([{"ref": "s1", "verdict": "pass", "code": null}]));
    assert_eq!(
        slug_of(&parse_state_review(&raw, &["s1".to_string(), "s2".to_string()]).unwrap_err()),
        "uncovered"
    );
    let extra = review_raw(json!([
        {"ref": "s1", "verdict": "pass", "code": null},
        {"ref": "s9", "verdict": "pass", "code": null}
    ]));
    assert_eq!(
        slug_of(&parse_state_review(&extra, &["s1".to_string()]).unwrap_err()),
        "unknown_turn"
    );
}

#[test]
fn review_code_must_be_in_the_closed_set_and_absent_on_pass() {
    for (verdict, code) in [
        ("pass", Some("\"value_drift\"")),
        ("reject", None),
        ("reject", Some("\"not_a_real_code\"")),
        ("uncertain", Some("0")),
        ("maybe", Some("null")),
    ] {
        let code_value: serde_json::Value = match code {
            Some(text) => serde_json::from_str(text).unwrap(),
            None => serde_json::Value::Null,
        };
        let raw = json!({
            "schema_version": 0, "pass": "state_review",
            "verdicts": [{"ref": "s1", "verdict": verdict, "code": code_value}]
        })
        .to_string();
        let error = parse_state_review(&raw, &["s1".to_string()]).unwrap_err();
        assert_eq!(slug_of(&error), "json", "{verdict}/{code_value} 该被形状面挡下");
    }
    // uncertain 允许不带码（判不了本身就是一个答案）。
    let raw = review_raw(json!([{"ref": "s1", "verdict": "uncertain", "code": null}]));
    assert_eq!(parse_state_review(&raw, &["s1".to_string()]).unwrap()[0].1, ReviewVerdict::Uncertain);
    // 六码闭集逐条能过（reject 带码）。
    for code in STATE_REVIEW_CODES {
        let raw = review_raw(json!([{"ref": "s1", "verdict": "reject", "code": code}]));
        assert_eq!(
            parse_state_review(&raw, &["s1".to_string()]).unwrap()[0].2.as_deref(),
            Some(code)
        );
    }
}

#[test]
fn review_envelope_is_independent_of_extraction() {
    let raw = json!({"schema_version": 0, "pass": "state", "verdicts": []}).to_string();
    assert_eq!(slug_of(&parse_state_review(&raw, &[]).unwrap_err()), "pass");
}

// ── prompt 面 ──────────────────────────────────────────────────────────────────

#[test]
fn system_prompts_carry_their_own_canary_once_and_stay_apart() {
    let extract = state_system();
    let review = state_review_system();
    assert_eq!(extract.matches(STATE_CANARY).count(), 1);
    assert_eq!(review.matches(STATE_REVIEW_CANARY).count(), 1);
    assert!(!extract.contains(STATE_REVIEW_CANARY));
    assert!(!review.contains(STATE_CANARY));
    // 抽取与审核的 canary 也不能与 event/fact 的四份撞（canary 唯一家是 pass_prompt）。
    for other in [
        rc_distill::pass_prompt::EVENT_CANARY,
        rc_distill::pass_prompt::EVENT_REVIEW_CANARY,
        rc_distill::pass_prompt::FACT_CANARY,
        rc_distill::pass_prompt::FACT_REVIEW_CANARY,
    ] {
        assert!(!extract.contains(other) && !review.contains(other));
    }
}

#[test]
fn the_dimension_closed_set_appears_verbatim_in_the_contract() {
    let text = state_system();
    for dimension in STATE_DIMENSIONS {
        assert!(text.contains(dimension), "{dimension} 没进输出契约 ⇒ 模型只能猜");
    }
    // 判据的四段形状与方法/标准/正反例的配比（与 event 同一把尺子）。
    let shape = state_shape();
    assert_eq!(shape.method_steps, 4);
    assert_eq!(shape.standards, 6);
    assert!(shape.positives >= 3 && shape.negatives >= 6);
    let review_shape = state_review_shape();
    assert_eq!(review_shape.method_steps, 4);
}

#[test]
fn examples_use_generic_names_only() {
    // 专名泄漏检查：这一片的例子只用通用占位名，不含本地语料里的角色名/原句。
    let text = state_system();
    for forbidden in ["小鲸鱼", "jingyu", "大肥鱼", "构建好了"] {
        assert!(!text.contains(forbidden), "prompt 例子里出现了语料专名：{forbidden}");
    }
}

#[test]
fn user_prompt_renders_targets_candidates_and_live_states() {
    let b = batch(&[ANXIOUS_BODY, "另一轮正文"]);
    let existing = vec![ExistingState {
        subject_canonical: "张三".into(),
        dimension: "emotion".into(),
        value: "心情不好".into(),
    }];
    let user = state_user(&b, &candidates(), &ExtractCtx::default(), &existing);
    assert!(user.starts_with("PASS state v0\n"));
    assert!(user.contains(&format!(" human: {ANXIOUS_BODY}")));
    assert!(user.contains("- a1 | person | 张三"));
    assert!(user.contains("CURRENT STATES"));
    assert!(user.contains("- 张三 | emotion | 心情不好"));
    assert!(!user.contains(&anchor_id(1)), "anchor_id 不许进 prompt");
}

#[test]
fn review_user_gives_names_and_quote_but_no_identity_columns() {
    let b = batch(&[ANXIOUS_BODY]);
    let got = parse_state_output_lenient(
        &output(vec![row("s1", "t1", "a1", "emotion", "焦虑得很", ANXIOUS_BODY)], Vec::new()),
        &b,
        &candidates(),
    )
    .unwrap();
    let ctx = rc_distill::pass_prompt::ReviewInputContext {
        session_key: b.session_key.clone(),
        owner_agent_id: "owner-state".into(),
        visibility: "project".into(),
        scope: "project:sess-state".into(),
        speakers: vec!["小明".to_string()],
    };
    let text = state_review_user(&got.drafts, &b, &ctx);
    assert!(text.starts_with("PASS state_review v0\n"));
    assert!(text.contains("ITEM s1"));
    assert!(text.contains("subject: 张三"));
    assert!(text.contains("dimension: emotion"));
    assert!(text.contains("value: 焦虑得很"));
    assert!(text.contains(ANXIOUS_BODY));
    assert!(!text.contains(&anchor_id(1)), "审核面只给名字，不给 anchor_id");
    assert!(!text.contains(&uid(0)), "审核面不给 turn_uid");
    assert!(!text.contains("expressed"), "程序判出的 mode 不进审核输入（否则是自证）");
}
