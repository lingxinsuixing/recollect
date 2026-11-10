//! S2b-1：fact pass 的 prompt 形状与严格 parser（设计 `2026-10-26-s2-prompt-v0` §2.2、§4）。
//!
//! 这一层只断"模型给什么形状、程序怎么拒"：不选批、不调模型、不写库、不做 CLI。
//! 正反例只用通用占位名（张三/李四/某项目/某打印机/对账单），不含任何语料专名，
//! 也不含 holdout 原句；唯一的例外是一格 prompt 形状断言**逐字**复述生产 prompt 自带的
//! promise 正例（`pass_prompt.rs` 的 system 样例），它跟着 prompt 原文走，不是夹具。
//!
//! 13 条用例覆盖派发档的 1–13：合法 promise/decision/plan、五类元信息拒绝、
//! 未知短引用、闭集、缺 subject/缺值、quote 三坏法、数字 grounding、evidence、
//! 重复 ref 与覆盖、合法空批，以及 prompt 形状（canary 一次、无 uid/毫秒/anchor_id）。

use rc_distill::pass_fact_contract::{
    parse_fact_compact, parse_fact_output, parse_fact_output_lenient, parse_fact_review,
    EvidenceDowngrade, FactPassError, FactReviewReport, LenientFacts, ReviewTarget, ReviewVerdict,
};
use rc_distill::pass_prompt::{
    event_user, fact_review_challenge_shape, fact_review_challenge_system, fact_review_shape,
    fact_review_system, fact_review_user, fact_shape, fact_system, fact_system_compact, fact_user,
    FactCandidate, FactFamily, ReviewInputContext, FACT_CANARY, FACT_REVIEW_CANARY,
    FACT_REVIEW_CHALLENGE_CANARY, MARK_SELF, PASS_FACT_REVIEW, REVIEW_CODES, SPEAKER_LINE_NOTE,
    SPEAKER_NOTE,
};
use rc_distill::{Batch, TurnRow};
use rc_distill::batch::ExtractCtx;
use serde_json::{json, Value};

/// 2026-10-26T14:05+08:00 的毫秒值：prompt 的时间断言要能逐字对上。
const NOW: i64 = 1_792_994_700_000;

fn uid(index: usize) -> String {
    format!("{:064x}", index + 1)
}

fn anchor_id(index: usize) -> String {
    format!("{:064x}", 0xa0 + index)
}

fn batch(bodies: &[&str]) -> Batch {
    Batch {
        session_key: "sess-fact".into(),
        device_id: "dev-fact".into(),
        source_agent: "agent-fact".into(),
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

fn candidate(index: usize, kind: &str, canonical: &str) -> FactCandidate {
    FactCandidate {
        reference: format!("a{index}"),
        kind: kind.into(),
        canonical: canonical.into(),
        anchor_id: anchor_id(index),
        mark: None,
    }
}

/// 审核输入的批级信息（C6a）：归属四格 + 与 `batch.rows` 逐位对应的说话人名。
fn review_ctx(b: &Batch) -> ReviewInputContext {
    ReviewInputContext {
        session_key: b.session_key.clone(),
        owner_agent_id: "owner-fact".into(),
        visibility: "project".into(),
        scope: "project:sess-fact".into(),
        speakers: b
            .rows
            .iter()
            .map(|row| {
                if row.role == "human" { "小明".to_string() } else { b.source_agent.clone() }
            })
            .collect(),
    }
}

fn sample_candidates() -> Vec<FactCandidate> {
    vec![candidate(1, "person", "张三"), candidate(2, "person", "李四")]
}

fn doc(family: &str, facts: Vec<Value>, empty_turns: &[&str]) -> String {
    json!({
        "schema_version": 0,
        "pass": "fact",
        "family": family,
        "facts": facts,
        "empty_turns": empty_turns,
    })
    .to_string()
}

/// 合法 promise：t2 是目标轮，t1/t3 是寒暄（进 empty_turns）。
fn promise_batch() -> Batch {
    batch(&["好的，收到", "我下周三之前把对账单发给你", "谢谢"])
}

fn promise_fact() -> Value {
    json!({
        "ref": "f1",
        "turn": "t2",
        "subject": "a1",
        "predicate": "promise",
        "object": "a2",
        "value": "把对账单发给你",
        "category": "promise",
        "mode": "stated",
        "time_text": "下周三之前",
        "quote": "我下周三之前把对账单发给你",
        "evidence": ["t2"],
        "text": "张三承诺在下周三之前把对账单发给李四。"
    })
}

fn promise_reply() -> String {
    doc("promise", vec![promise_fact()], &["t1", "t3"])
}

fn parse(raw: &str, batch: &Batch, candidates: &[FactCandidate], family: FactFamily) -> Result<Vec<rc_distill::FactDraft>, FactPassError> {
    parse_fact_output(raw, batch, candidates, family)
}

/// 1. promise 合法：subject/object/value/quote/text 映射到真实 uid 与 anchor_id。
#[test]
fn promise_round_trips_short_refs_to_real_identity() {
    let b = promise_batch();
    let got = parse(&promise_reply(), &b, &sample_candidates(), FactFamily::Promise).unwrap();
    assert_eq!(got.len(), 1);
    let f = &got[0];
    assert_eq!(f.reference, "f1");
    assert_eq!(f.turn_uid, b.rows[1].turn_uid);
    assert_eq!(f.subject_anchor_id, anchor_id(1));
    assert_eq!(f.object_anchor_id, Some(anchor_id(2)));
    assert_eq!(f.predicate, "promise");
    assert_eq!(f.category, "promise");
    assert_eq!(f.mode, "stated");
    assert_eq!(f.time_text, "下周三之前");
    assert_eq!(f.quote, "我下周三之前把对账单发给你");
    assert_eq!(f.evidence_turn_uids, vec![b.rows[1].turn_uid.clone()]);
    assert_eq!(f.value.as_deref(), Some("把对账单发给你"), "value 是 quote 的逐字区间");
    assert_eq!(f.text, "张三承诺在下周三之前把对账单发给李四。");
}

/// 2. decision 只带 value、object 为空 ⇒ 合法。
#[test]
fn decision_with_value_only_is_legal() {
    let b = batch(&["我们决定先不上线，等迁移完"]);
    let c = vec![candidate(1, "org", "某项目")];
    let raw = doc(
        "decision",
        vec![json!({
            "ref": "f1",
            "turn": "t1",
            "subject": "a1",
            "predicate": "decide",
            "object": null,
            "value": "先不上线，等迁移完",
            "category": "decision",
            "mode": "stated",
            "time_text": "等迁移完",
            "quote": "我们决定先不上线，等迁移完",
            "evidence": ["t1"],
            "text": "某项目决定先不上线，等迁移完成。"
        })],
        &[],
    );
    let got = parse(&raw, &b, &c, FactFamily::Decision).unwrap();
    assert_eq!(got[0].object_anchor_id, None);
    assert_eq!(got[0].predicate, "decide");
    assert_eq!(got[0].value.as_deref(), Some("先不上线，等迁移完"));
}

/// 3. plan family + 只带 object ⇒ 合法。
#[test]
fn plan_with_object_only_is_legal() {
    let b = batch(&["计划下周把某打印机搬走"]);
    let c = vec![candidate(1, "org", "某项目"), candidate(2, "object", "某打印机")];
    let raw = doc(
        "plan",
        vec![json!({
            "ref": "f1",
            "turn": "t1",
            "subject": "a1",
            "predicate": "plan",
            "object": "a2",
            "value": null,
            "category": "plan",
            "mode": "stated",
            "time_text": "下周",
            "quote": "计划下周把某打印机搬走",
            "evidence": ["t1"],
            "text": "某项目计划下周把某打印机搬走。"
        })],
        &[],
    );
    let got = parse(&raw, &b, &c, FactFamily::Plan).unwrap();
    assert_eq!(got[0].value, None);
    assert_eq!(got[0].object_anchor_id, Some(anchor_id(2)));
    assert_eq!(got[0].predicate, "plan");
}

/// 4. 坏 JSON / 顶层多键 / 字段多键 / 类型错 / version / pass / family 不符。
#[test]
fn malformed_envelopes_are_rejected_by_json_version_pass_and_family() {
    let b = promise_batch();
    let c = sample_candidates();
    let good = promise_reply();
    let mut value: Value = serde_json::from_str(&good).unwrap();

    assert_eq!(parse("这不是 json", &b, &c, FactFamily::Promise), Err(FactPassError::Json));
    assert_eq!(parse("[]", &b, &c, FactFamily::Promise), Err(FactPassError::Json));

    let mut top_extra = value.clone();
    top_extra["db_id"] = json!(7);
    assert_eq!(parse(&top_extra.to_string(), &b, &c, FactFamily::Promise), Err(FactPassError::Json));

    let mut fact_extra = value.clone();
    fact_extra["facts"][0]["confidence"] = json!(0.9);
    assert_eq!(parse(&fact_extra.to_string(), &b, &c, FactFamily::Promise), Err(FactPassError::Json));

    let mut fact_id = value.clone();
    fact_id["facts"][0]["id"] = json!("fact-1");
    assert_eq!(parse(&fact_id.to_string(), &b, &c, FactFamily::Promise), Err(FactPassError::Json));

    let mut wrong_type = value.clone();
    wrong_type["facts"] = json!({"not": "a list"});
    assert_eq!(parse(&wrong_type.to_string(), &b, &c, FactFamily::Promise), Err(FactPassError::Json));

    let mut version = value.clone();
    version["schema_version"] = json!(1);
    assert_eq!(parse(&version.to_string(), &b, &c, FactFamily::Promise), Err(FactPassError::Version));

    let mut pass = value.clone();
    pass["pass"] = json!("event");
    assert_eq!(parse(&pass.to_string(), &b, &c, FactFamily::Promise), Err(FactPassError::Pass));

    let mut family = value.clone();
    family["family"] = json!("plan");
    assert_eq!(parse(&family.to_string(), &b, &c, FactFamily::Promise), Err(FactPassError::Family));

    // 缺键同样归 Json（五键全必填）。
    value.as_object_mut().unwrap().remove("empty_turns");
    assert_eq!(parse(&value.to_string(), &b, &c, FactFamily::Promise), Err(FactPassError::Json));
}

/// 5. 批外短引用：turn / subject / evidence 各自硬拒，且错误带原引用。
#[test]
fn short_refs_outside_the_batch_are_rejected() {
    let b = promise_batch();
    let c = sample_candidates();

    let mut unknown_turn = promise_fact();
    unknown_turn["turn"] = json!("t9");
    unknown_turn["evidence"] = json!(["t9"]);
    let raw = doc("promise", vec![unknown_turn], &["t1", "t3"]);
    assert_eq!(parse(&raw, &b, &c, FactFamily::Promise), Err(FactPassError::UnknownTurn("t9".into())));

    let mut unknown_subject = promise_fact();
    unknown_subject["subject"] = json!("a9");
    let raw = doc("promise", vec![unknown_subject], &["t1", "t3"]);
    assert_eq!(parse(&raw, &b, &c, FactFamily::Promise), Err(FactPassError::UnknownCandidate("a9".into())));

    let mut unknown_object = promise_fact();
    unknown_object["object"] = json!("a9");
    let raw = doc("promise", vec![unknown_object], &["t1", "t3"]);
    assert_eq!(parse(&raw, &b, &c, FactFamily::Promise), Err(FactPassError::UnknownCandidate("a9".into())));
}

/// 6. category / predicate / mode 任一不符 ⇒ 各自一格错误。
#[test]
fn closed_sets_for_category_predicate_and_mode_are_enforced() {
    let b = promise_batch();
    let c = sample_candidates();

    let mut category = promise_fact();
    category["category"] = json!("decision");
    assert_eq!(
        parse(&doc("promise", vec![category], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::Category)
    );

    let mut predicate = promise_fact();
    predicate["predicate"] = json!("decide");
    assert_eq!(
        parse(&doc("promise", vec![predicate], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::Predicate)
    );

    let mut mode = promise_fact();
    mode["mode"] = json!("reported");
    assert_eq!(
        parse(&doc("promise", vec![mode], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::Mode)
    );
}

/// 7. subject 空 / object 与 value 都空 ⇒ 硬拒。
#[test]
fn subject_and_value_are_required() {
    let b = promise_batch();
    let c = sample_candidates();

    let mut empty_subject = promise_fact();
    empty_subject["subject"] = json!("");
    assert_eq!(
        parse(&doc("promise", vec![empty_subject], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::MissingSubject)
    );

    let mut blank_subject = promise_fact();
    blank_subject["subject"] = json!("   ");
    assert_eq!(
        parse(&doc("promise", vec![blank_subject], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::MissingSubject)
    );

    let mut null_pair = promise_fact();
    null_pair["object"] = json!(null);
    null_pair["value"] = json!(null);
    assert_eq!(
        parse(&doc("promise", vec![null_pair], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::MissingValue)
    );

    let mut empty_pair = promise_fact();
    empty_pair["object"] = json!("");
    empty_pair["value"] = json!("  ");
    assert_eq!(
        parse(&doc("promise", vec![empty_pair], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::MissingValue)
    );
}

/// 7b. （S2e-4）`time_text` 可空：`null` / 省略字段 / 空白 ⇒ draft 空串；非空原样保留。
///
/// 真实冒烟里模型对"没有时间短语"的轮次写了 `"time_text": null`，旧契约按必填字符串拒了
/// 整批。没有时间短语是**正常**的一条 fact，不是形状错：这里把它读成空串，写库层的空串
/// 分支本来就不落 `time_expr`。
#[test]
fn time_text_may_be_null_omitted_or_blank() {
    let b = promise_batch();
    let c = sample_candidates();

    let mut null_time = promise_fact();
    null_time["time_text"] = json!(null);
    let got =
        parse(&doc("promise", vec![null_time], &["t1", "t3"]), &b, &c, FactFamily::Promise).unwrap();
    assert_eq!(got[0].time_text, "", "null ⇒ 空串，不再整批拒收");

    let mut omitted = promise_fact();
    omitted.as_object_mut().unwrap().remove("time_text");
    let got =
        parse(&doc("promise", vec![omitted], &["t1", "t3"]), &b, &c, FactFamily::Promise).unwrap();
    assert_eq!(got[0].time_text, "", "字段省略与 null 同一个读法");

    for blank in ["", "   "] {
        let mut empty_time = promise_fact();
        empty_time["time_text"] = json!(blank);
        let got = parse(&doc("promise", vec![empty_time], &["t1", "t3"]), &b, &c,
                         FactFamily::Promise)
            .unwrap();
        assert_eq!(got[0].time_text, "", "空/全空白 ⇒ 空串");
    }

    let got = parse(&promise_reply(), &b, &c, FactFamily::Promise).unwrap();
    assert_eq!(got[0].time_text, "下周三之前", "非空短语原样保留");
}

/// 7c. （§10.47）`text` 可省：省略或空白 ⇒ 程序拼 `主语：value`，数字仍须落在 quote 里。
///
/// 来源是 Qwen3-4B 的第一次真跑：它逐条漏写 `text`，整批被判 `json`（`drafts=0`）。
/// `text` 只是给人看的复述，判同/归属/证据都不用，由程序拼比逼弱模型写更稳。
#[test]
fn text_may_be_omitted_and_is_derived_from_subject_and_value() {
    let b = promise_batch();
    let c = sample_candidates();

    let mut omitted = promise_fact();
    omitted.as_object_mut().unwrap().remove("text");
    let got =
        parse(&doc("promise", vec![omitted], &["t1", "t3"]), &b, &c, FactFamily::Promise).unwrap();
    assert_eq!(got[0].text, "张三：把对账单发给你", "省略 ⇒ 程序拼 `主语：value`（不补猜）");

    let mut blank = promise_fact();
    blank["text"] = json!("  ");
    let got =
        parse(&doc("promise", vec![blank], &["t1", "t3"]), &b, &c, FactFamily::Promise).unwrap();
    assert_eq!(got[0].text, "张三：把对账单发给你", "空白与省略同一个读法");

    // 写了的仍以模型为准，并且数字仍须落在 quote 里。
    let got = parse(&promise_reply(), &b, &c, FactFamily::Promise).unwrap();
    assert_eq!(got[0].text, "张三承诺在下周三之前把对账单发给李四。");
    let mut invented = promise_fact();
    invented["text"] = json!("张三承诺等 10 天后再发对账单。");
    assert_eq!(
        parse(&doc("promise", vec![invented], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::NumberNotGrounded("10".into()))
    );
}

/// 7d. （§10.47）TARGET 行带程序解析出的说话人：human 轮取唯一的 person，agent 轮取 `[self]`；
/// 多个 person 又没有 `[principal]` 时不写这一格（宁可不猜），并在说明里交代怎么读。
#[test]
fn targets_carry_the_resolved_speaker() {
    let b = batch(&["好的，收到", "我下周三之前把对账单发给你"]);
    let mut self_anchor = candidate(1, "agent", "qoder");
    self_anchor.mark = Some(MARK_SELF);
    let person = candidate(2, "person", "张三");
    let c = vec![self_anchor, person];

    let user = fact_user(&b, &c, FactFamily::Promise, &ExtractCtx::default());
    assert!(
        user.contains("TARGET [t1] 2026-10-26T14:05+08:00 human speaker=a2: 好的，收到"),
        "human 轮要带唯一 person 的短 id：{user}"
    );
    assert!(
        user.contains("TARGET [t2] 2026-10-26T14:06+08:00 agent speaker=a1: 我下周三之前把对账单发给你"),
        "agent 轮要带 [self] 的短 id：{user}"
    );
    assert_eq!(count(&user, SPEAKER_LINE_NOTE), 1, "speaker= 读法说明恰好一次");

    // 两个 person、没有 claim ⇒ 不写 speaker=，不把主语猜错。
    let ambiguous = sample_candidates();
    let user = fact_user(&b, &ambiguous, FactFamily::Promise, &ExtractCtx::default());
    assert!(
        user.lines()
            .filter(|line| line.starts_with("TARGET ["))
            .all(|line| !line.contains("speaker=")),
        "多个 person 且无 [principal] 时不许猜：{user}"
    );
}

/// 7e. （§10.47）`clause` 由程序抄 quote：给了编号就不必重打引文；跨轮/不存在的编号要拒。
#[test]
fn a_clause_id_copies_the_quote_and_cross_turn_ids_are_rejected() {
    let b = promise_batch();
    let c = sample_candidates();

    let mut by_clause = promise_fact();
    by_clause.as_object_mut().unwrap().remove("quote");
    by_clause["clause"] = json!("c2");
    let got =
        parse(&doc("promise", vec![by_clause], &["t1", "t3"]), &b, &c, FactFamily::Promise).unwrap();
    assert_eq!(got[0].quote, "我下周三之前把对账单发给你", "clause ⇒ 程序抄该子句原文");

    let mut cross = promise_fact();
    cross.as_object_mut().unwrap().remove("quote");
    cross["clause"] = json!("c1");
    assert_eq!(
        parse(&doc("promise", vec![cross], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::ClauseTurnMismatch("c1".into())),
        "t2 的事实借用 t1 的子句 = 引错证据"
    );

    let mut unknown = promise_fact();
    unknown.as_object_mut().unwrap().remove("quote");
    unknown["clause"] = json!("c99");
    assert_eq!(
        parse(&doc("promise", vec![unknown], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::UnknownClause("c99".into()))
    );
}

/// 7f. （§10.47）紧凑档：`marks` 逐子句 yes/no ⇒ 程序补 turn/subject/quote/value/text。
#[test]
fn compact_marks_become_facts_with_program_filled_fields() {
    // 承诺放在 human 轮（下标 0）：agent 轮要先有 `[self]` 候选才解析得出说话人。
    let b = batch(&["我下周三之前把对账单发给你", "好的", "谢谢"]);
    let c = vec![candidate(1, "person", "张三")];
    let raw = r#"{"marks":[{"c":"c1","keep":true},{"c":"c2","keep":false}]}"#;
    let got = parse_fact_compact(raw, &b, &c, FactFamily::Promise).unwrap();
    assert_eq!(got.drafts.len(), 1);
    assert_eq!(got.drafts[0].turn_uid, b.rows[0].turn_uid);
    assert_eq!(got.drafts[0].subject_canonical, "张三");
    assert_eq!(got.drafts[0].quote, "我下周三之前把对账单发给你");
    assert_eq!(
        got.drafts[0].value.as_deref(),
        Some("我下周三之前把对账单发给你"),
        "value 缺省 = 子句原文"
    );
    assert_eq!(got.drafts[0].text, "张三：我下周三之前把对账单发给你");
    assert_eq!(got.drafts[0].evidence_turn_uids, vec![b.rows[0].turn_uid.clone()]);
    assert!(got.uncovered.is_empty(), "没被 keep 的轮由程序写进 empty_turns");
}

/// 8. quote：不逐字 / 超 120 字 / 含省略号。
#[test]
fn quote_must_be_verbatim_short_and_without_ellipsis() {
    let b = promise_batch();
    let c = sample_candidates();

    let mut not_verbatim = promise_fact();
    not_verbatim["quote"] = json!("我下周三把对账单发给你");
    assert_eq!(
        parse(&doc("promise", vec![not_verbatim], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::QuoteNotVerbatim("f1".into()))
    );

    let mut too_long = promise_fact();
    too_long["quote"] = json!("下".repeat(121));
    assert_eq!(
        parse(&doc("promise", vec![too_long], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::QuoteTooLong)
    );

    let mut chinese_ellipsis = promise_fact();
    chinese_ellipsis["quote"] = json!("我下周三…发给你");
    assert_eq!(
        parse(&doc("promise", vec![chinese_ellipsis], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::Ellipsis)
    );

    let mut ascii_ellipsis = promise_fact();
    ascii_ellipsis["quote"] = json!("我下周三...发给你");
    assert_eq!(
        parse(&doc("promise", vec![ascii_ellipsis], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::Ellipsis)
    );
}

/// 9. text 的数字必须逐一出现在 quote 里；日期拆成数字串后同样成立。
#[test]
fn numbers_in_text_must_be_grounded_in_quote() {
    let b = promise_batch();
    let c = sample_candidates();

    let mut invented_number = promise_fact();
    invented_number["text"] = json!("张三承诺等 10 天后再把对账单发给李四。");
    assert_eq!(
        parse(&doc("promise", vec![invented_number], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::NumberNotGrounded("10".into()))
    );

    // 正例：quote 里是 ISO 日期，text 里是同一组数字 ⇒ 2026/10/26 各自都能找到。
    let b = batch(&["2026-10-26 之前交对账单"]);
    let raw = doc(
        "promise",
        vec![json!({
            "ref": "f1",
            "turn": "t1",
            "subject": "a1",
            "predicate": "promise",
            "object": "a2",
            "value": "交对账单",
            "category": "promise",
            "mode": "stated",
            "time_text": "2026-10-26 之前",
            "quote": "2026-10-26 之前交对账单",
            "evidence": ["t1"],
            "text": "张三承诺在 2026-10-26 之前交对账单。"
        })],
        &[],
    );
    let got = parse(&raw, &b, &c, FactFamily::Promise).unwrap();
    assert_eq!(got.len(), 1);
}

/// 10. evidence：为空 / 含非目标 id / 不含本条 turn。
#[test]
fn evidence_must_be_non_empty_target_only_and_include_turn() {
    let b = promise_batch();
    let c = sample_candidates();

    let mut empty = promise_fact();
    empty["evidence"] = json!([]);
    assert_eq!(
        parse(&doc("promise", vec![empty], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::EmptyEvidence)
    );

    let mut outside = promise_fact();
    outside["evidence"] = json!(["t2", "t9"]);
    assert_eq!(
        parse(&doc("promise", vec![outside], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::EvidenceNotTarget("t9".into()))
    );

    // 目标集合里有 t1，但它不是这一条的 turn ⇒ 证据没有覆盖自己声明的轮。
    let b2 = batch(&["好的，收到", "我下周三之前把对账单发给你"]);
    let mut missing_turn = promise_fact();
    missing_turn["turn"] = json!("t2");
    missing_turn["evidence"] = json!(["t1"]);
    assert_eq!(
        parse(&doc("promise", vec![missing_turn], &["t1", "t2"]), &b2, &c, FactFamily::Promise),
        Err(FactPassError::EvidenceNotTarget("t2".into()))
    );

    // 锚引用不能进 evidence（a1 只属于 subject/object）。
    let mut anchor_as_evidence = promise_fact();
    anchor_as_evidence["evidence"] = json!(["a1"]);
    assert_eq!(
        parse(&doc("promise", vec![anchor_as_evidence], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::EvidenceNotTarget("a1".into()))
    );
}

/// 11. 重复 ref；未覆盖目标 ⇒ `Uncovered` 按序号列出全部缺失 id。
#[test]
fn duplicate_refs_and_uncovered_turns_are_named_exactly() {
    let b = promise_batch();
    let c = sample_candidates();

    let second = json!({
        "ref": "f1",
        "turn": "t1",
        "subject": "a1",
        "predicate": "promise",
        "object": null,
        "value": "占位",
        "category": "promise",
        "mode": "stated",
        "time_text": "占位",
        "quote": "好的，收到",
        "evidence": ["t1"],
        "text": "占位。"
    });
    let raw = doc("promise", vec![promise_fact(), second], &["t3"]);
    assert_eq!(parse(&raw, &b, &c, FactFamily::Promise), Err(FactPassError::DuplicateRef("f1".into())));

    // t2 有 fact，t1 进 empty_turns，t3 谁也没提 ⇒ 只缺 t3。
    let raw = doc("promise", vec![promise_fact()], &["t1"]);
    assert_eq!(parse(&raw, &b, &c, FactFamily::Promise), Err(FactPassError::Uncovered(vec!["t3".into()])));

    // 一条都不给 ⇒ 三个目标全部缺失，顺序是 t1、t2、t3。
    let raw = doc("promise", vec![], &[]);
    assert_eq!(
        parse(&raw, &b, &c, FactFamily::Promise),
        Err(FactPassError::Uncovered(vec!["t1".into(), "t2".into(), "t3".into()]))
    );
}

/// 12. `facts=[]` + `empty_turns` 覆盖全部 ⇒ 合法空批；缺一项 ⇒ Uncovered。
#[test]
fn an_empty_batch_is_legal_only_when_every_target_is_covered() {
    let b = batch(&["好的", "谢谢"]);
    let c: Vec<FactCandidate> = vec![];

    let raw = doc("promise", vec![], &["t1", "t2"]);
    assert_eq!(parse(&raw, &b, &c, FactFamily::Promise), Ok(vec![]));

    let raw = doc("promise", vec![], &["t1"]);
    assert_eq!(parse(&raw, &b, &c, FactFamily::Promise), Err(FactPassError::Uncovered(vec!["t2".into()])));

    // empty_turns 也不许重复、不许批外引用。
    let raw = doc("promise", vec![], &["t1", "t1"]);
    assert_eq!(parse(&raw, &b, &c, FactFamily::Promise), Err(FactPassError::DuplicateRef("t1".into())));

    let raw = doc("promise", vec![], &["t9"]);
    assert_eq!(parse(&raw, &b, &c, FactFamily::Promise), Err(FactPassError::UnknownTurn("t9".into())));

    let raw = doc("promise", vec![], &["c1"]);
    assert_eq!(parse(&raw, &b, &c, FactFamily::Promise), Err(FactPassError::UnknownTurn("c1".into())));
}

/// 13. prompt 形状：canary 一次、family 首行、无 uid/毫秒/anchor_id、候选只见 a*。
#[test]
fn fact_prompt_shape_hides_identity_and_pins_one_family() {
    // 交接档 §3.2 粒度四格（规则 1–4）：一轮可出多条且一条一子句、出了 fact 的轮不进
    // empty_turns；quote 连续、不跨子句拼接、不丢值；object 只挂引文点名的人/组织；
    // time_text 逐字抄本条 quote 里的时间表达。逐字断言渲染结果，不测常量存在。
    // 这组措辞断言放在形状断言之前：删掉任一条规则时，失败落在缺哪句原文上。
    let sys = fact_system(FactFamily::Promise);
    for needle in [
        "ONE CLAUSE, ONE FACT",
        "never put a turn that produced a fact into empty_turns",
        "must not splice two clauses",
        "cover every character of value",
        "A project, an event, a work item, a tool or a data source is not an object",
        "never borrow a time from another clause",
        "If one clause is longer than 120 characters",
    ] {
        assert!(sys.contains(needle), "粒度四格缺原文：{needle}");
    }

    // 台账 §10.26 重放的三条判据（`f637921` 后重放：必须猜 6/8/9 格，新增格全在这三处）。
    // 每条都能单独红：删掉对应那句，只有本用例失败。

    // 1. 子句边界：句末四个标点或换行才算结束；冒号、逗号、破折号都不算。
    for needle in [
        "CLAUSE BOUNDARY",
        "a clause ends at",
        "Neither ： nor ， ends a clause",
    ] {
        assert!(sys.contains(needle), "子句边界判据缺原文：{needle}");
    }
    assert!(
        sys.contains("a clause ends at 。 ！ ？ ； or a line break"),
        "子句边界必须逐字列出四个句末标点与换行"
    );
    assert!(
        sys.contains("a dash (——) does not either"),
        "子句边界必须排掉破折号（`盯四件事——…` 不许被切开）"
    );

    // 2. 一个子句一条 fact + 同一子句内的列举不拆。
    for needle in [
        "emit one fact per clause",
        "never split a list inside one clause",
    ] {
        assert!(sys.contains(needle), "粒度判据缺原文：{needle}");
    }

    // 3. time_text 逐字抄（含属于它的动词/前缀），不缩成裸数字、不加词、无时间写 null。
    for needle in [
        "copies the time expression exactly as it is written",
        "including a verb or prefix",
        "never shorten it to a bare number",
        "never add words that are not in the quote",
        "when the quote has no time expression, write null",
        // 派发档给的四个例子逐字在场：`5 天` / `24 小时` / `本轮` 三种歧义都点了名。
        "接下来 5 天, 每天, 跑满 24 小时, 本轮",
        "接下来 5 天",
        "跑满 24 小时",
        "本轮",
    ] {
        assert!(sys.contains(needle), "time_text 逐字判据缺原文：{needle}");
    }
    // 旧口径必须消失：`shortest` 一在，"5 天 / 接下来 5 天"就仍有两种合法答案。
    assert!(
        !sys.contains("shortest time phrase"),
        "time_text 还留着「最短时间短语」的旧口径"
    );
    assert!(
        sys.contains("never write later, soon or a guessed date"),
        "time_text 仍必须禁止编时间"
    );

    // 4. 会话内的工作步骤不是承诺：与 DURABLE AFTER THE SESSION 的分界写在同一条里。
    for needle in [
        "WORK LOG IS NOT A COMMITMENT",
        "narrates the step it is about to take in this same session",
        "keep that turn in empty_turns",
        "Only a rule, promise, plan or decision that outlives this conversation counts",
    ] {
        assert!(sys.contains(needle), "工作日志判据缺原文：{needle}");
    }
    assert!(
        sys.contains("A plan for the next few days, a recurring schedule and a standing rule all count"),
        "DURABLE AFTER THE SESSION 仍须说未来几天的计划与周期性日程都算（与工作日志条不冲突）"
    );

    // 5. 台账 §10.44：`CLAUSE BOUNDARY` 只判死"在哪切"，没判"哪块算承诺"。
    //    三族重放残余的 G1/G2/G6/G7 全落在这一条上：身份陈述、仓库状态、文档指针、
    //    中心主机/路径各被记成 fact。判据给正面定义 + 四类"不承载承诺"的闭集 + 常设禁令的例外。
    for needle in [
        "COMMITMENT-BEARING CLAUSE",
        "a clause carries a commitment only when it says what someone will do",
        "still binds after this conversation",
        "saying who someone is",
        "reporting the current state of the work",
        "a pointer that only says where information lives",
        "narrating infrastructure",
        "Emit no fact for them",
        "A standing prohibition (do not reopen what was deferred) still counts as a commitment",
    ] {
        assert!(sys.contains(needle), "承诺判据缺原文：{needle}");
    }
    // t17 类"产物会留下来、但动作是本会话的步骤"必须被判成工作日志。
    assert!(
        sys.contains("A step whose only product is this session's artifact"),
        "工作日志条须覆盖 t17：产物留存不等于承诺（台账 §10.44）"
    );

    // 6. 台账 §10.46：play6 残余的四类各补一句可判据的措辞（形状不变：并入 STEP 1/2 与承诺条）。
    for needle in [
        "A turn that pastes or quotes a block speaks in its own voice",
        "is not being acted upon; leave object null there too",
        "value is the clause with only its leading subject and commitment verb removed",
        "A clause that sets the scope or the operating rule of a named task or period",
        "Only a bare procedural instruction about the current tooling",
        "This rule wins over the scope rule below when a clause narrates a step this session is executing",
        "A statement of what is being verified right now is not scope",
    ] {
        assert!(sys.contains(needle), "§10.46 判据缺原文：{needle}");
    }

    // 形状变更（相对 §10.42 的 7 条 / 4 条）：标准 7 → 8，反例 4 → 7。
    // 理由：承诺判据是独立一条（塞进 `CLAUSE BOUNDARY` 会让"在哪切"与"哪块算"混读，
    // 重放里正是这两件事被混着猜）；反例补三条状态/指针/基础设施，对应 §10.44 的三类过度抽取。
    let shape = fact_shape();
    assert_eq!(
        (shape.method_steps, shape.standards, shape.positives, shape.negatives),
        (4, 8, 2, 7),
        "方法四步、标准八条（耐久判据、工作日志不是承诺、单一 family、主语必需、\
         主语优先级与种类、子句边界与粒度、承诺判据、引文）、正例 2、反例 7"
    );

    for family in FactFamily::ALL {
        let sys = fact_system(family);
        assert_eq!(count(&sys, FACT_CANARY), 1, "canary 只在 system 首行出现一次");
        assert!(sys.starts_with(FACT_CANARY), "canary 必须在首行");
        assert!(sys.contains(&format!("\"family\":\"{}\"", family.slug())));
        assert!(sys.contains(&format!(
            "family={}, category={}, predicate={}",
            family.slug(),
            family.category(),
            family.predicate()
        )));
        // 三道输出闸的字面约束必须在场：id、毫秒、confidence/occurred_at 都不许写。
        assert!(sys.contains("Do not write any id"));
        assert!(sys.contains("*_ms"));
        assert!(sys.contains("confidence"));
        assert!(sys.contains("occurred_at"));
        // 交叉缺席：fact 模板不偷别的作业的 canary。
        assert!(!sys.contains(rc_distill::prompt::EXTRACT_CANARY));
        assert!(!sys.contains(rc_distill::prompt::JUDGE_CANARY));
        assert!(!sys.contains(rc_distill::prompt::AUDIT_CANARY));
        assert!(!sys.contains(rc_distill::prompt::ANCHOR_CANARY));
    }

    // 正反例只用通用占位名，且确实在模板里；正例**按族分开**（台账 §10.26 缺陷 1），
    // 所以别的族的例子在这里一条都不许出现。
    //
    // 第一条 needle 是例外：它逐字是**生产 prompt 自带**的 promise 正例（`pass_prompt.rs`
    // 的 system 样例），不是本文件造的夹具。夹具已统一改成 `对账单`，这条 needle 跟着
    // prompt 原文走——prompt 不改，它就不能改，否则这一格就从"样例在场"变成"样例缺席"。
    for needle in [
        "我下周五之前把对账单发给你",
        "我会在周五下班前把验收清单发到群里",
        "好的",
        "他好像不太高兴",
        "我下周可能看看吧",
        "今天先帮我看一眼日志",
    ] {
        assert!(sys.contains(needle), "模板缺样例：{needle}");
    }
    assert!(!sys.contains("我们决定先不上线，等迁移完"), "promise 模板里混进了 decision 的正例");
    assert!(!sys.contains("我们计划下周先跑一轮小流量"), "promise 模板里混进了 plan 的正例");
    // 台账 §10.26 缺陷 2 / 3：耐久判据与主语优先级各一条标准，plan 族要示范短周期。
    assert!(sys.contains("DURABLE AFTER THE SESSION"), "耐久判据没改成「会话结束后仍成立」");
    assert!(sys.contains("SUBJECT PRIORITY"), "缺主语优先级标准");
    // 交接档 §3.1 后半：主语只能是人/组织/agent，event/work/object 永不作主语；
    // 没有可挂的主语就把该轮放进 empty_turns，不许拿 event 顶上。
    assert!(sys.contains("SUBJECT KIND"), "缺主语种类约束");
    assert!(
        sys.contains("subject must be a person, an organization or an agent"),
        "主语种类约束必须逐字点名 person / organization / agent"
    );
    assert!(
        sys.contains("An event, a work item or an object is never a subject"),
        "主语种类约束必须逐字禁止 event / work / object 作主语"
    );
    assert!(
        sys.contains("put that turn in empty_turns instead of using an event anchor"),
        "没有可挂的主语时必须退回 empty_turns"
    );
    assert!(
        fact_system(FactFamily::Plan).contains("接下来 5 天每天跑一次迁移校验"),
        "plan 族缺短周期/周期性的正例"
    );

    let b = batch(&["好的，收到", "我下周三之前把对账单发给你"]);
    let c = sample_candidates();
    // C6b：这一条断的是 TARGET/候选段的字面量，邻轮不在本用例的范围内 ⇒ 空 CTX。
    let user = fact_user(&b, &c, FactFamily::Promise, &ExtractCtx::default());

    assert!(user.lines().next().unwrap().contains("family promise"), "family 在 user 首行");
    assert!(user.contains("TARGET [t1] 2026-10-26T14:05+08:00 human: 好的，收到"));
    assert!(user.contains("TARGET [t2] 2026-10-26T14:06+08:00 agent: 我下周三之前把对账单发给你"));
    assert!(!user.contains("_ms"), "user 不渲染毫秒字段名");
    assert!(!user.contains("anchor_id"), "user 不渲染 anchor_id 这个词");
    assert!(!user.contains(&uid(0)) && !user.contains(&uid(1)), "user 不渲染 64 位 turn_uid");
    assert!(!has_hex64(&user), "user 不含任何 64 位 hex 串");
    for cand in &c {
        assert!(user.contains(&format!("- {} | {} | {}", cand.reference, cand.kind, cand.canonical)));
        assert!(!user.contains(&cand.anchor_id), "候选锚 id 一个字都不许进 prompt");
    }
    assert!(user.contains("CANDIDATES (subject/object 只能引用这些 a*):"));

    // SPEAKER_NOTE 同时进 fact_user 与 event_user，措辞必须与 SUBJECT PRIORITY 一致：
    // 说话人只是默认主语，不是"事实主体以引文为准"。两个 pass 里各恰好一次。
    let event = event_user(&b, &c, &ExtractCtx::default());
    for (name, prompt) in [("fact_user", &user), ("event_user", &event)] {
        assert_eq!(count(prompt, SPEAKER_NOTE), 1, "{name} 里的说话人说明必须恰好一次");
        assert!(
            prompt.contains("说话人只是默认主语"),
            "{name} 缺「说话人只是默认主语」"
        );
        assert!(
            !prompt.contains("事实主体仍以引文为准"),
            "{name} 还留着与 SUBJECT PRIORITY 打架的旧口径"
        );
    }
}

fn count(hay: &str, needle: &str) -> usize {
    hay.match_indices(needle).count()
}

/// 用例 13b — 紧凑档（§10.47 弱模型档）的形状闸（P0-2）。
///
/// `fact_system_compact` 此前在全部 tests 里 0 命中（`RECOLLECT_FACT_PROMPT=compact` 同理）：
/// 改紧凑档的措辞不会红。这一条按 A/B/C 三面把它钉住，与用例 13 的完整档形状一一对照。
/// 断言只用现有原文，prompt 一个字都不改。
#[test]
fn compact_prompt_pins_canary_family_and_output_contract() {
    for family in FactFamily::ALL {
        let sys = fact_system_compact(family);
        let slug = family.slug();

        // A. canary 方向：与完整档同一条纪律——首行、恰好一次、不偷别的作业的 canary。
        assert_eq!(
            count(&sys, FACT_CANARY),
            1,
            "紧凑档 {} 的 canary 必须恰好一次",
            slug
        );
        assert!(
            sys.starts_with(FACT_CANARY),
            "紧凑档 {} 的 canary 必须在首行",
            slug
        );
        for (name, other) in [
            ("extract", rc_distill::prompt::EXTRACT_CANARY),
            ("judge", rc_distill::prompt::JUDGE_CANARY),
            ("audit", rc_distill::prompt::AUDIT_CANARY),
            ("anchor", rc_distill::prompt::ANCHOR_CANARY),
            ("fact_review", FACT_REVIEW_CANARY),
        ] {
            assert!(
                !sys.contains(other),
                "紧凑档 {} 里混进了 {} 的 canary",
                slug,
                name
            );
        }

        // B. 本族闭集：一次只问一族，别的族的 slug 一条都不许出现。
        assert!(
            sys.contains(&format!("family = {}", slug)),
            "紧凑档 {} 缺 `family = <slug>`",
            slug
        );
        for other in FactFamily::ALL {
            assert!(
                other == family || !sys.contains(other.slug()),
                "紧凑档 {} 里出现了 {} 族的 slug（越出本族闭集）",
                slug,
                other.slug()
            );
        }
        // B. 输出契约只有 marks/c/keep。没有 value / quote / schema_version / facts 这些字段，
        //    就是"紧凑档不可能改写 value"的机械证据：value 与 quote 由程序从子句与说话人抄。
        assert!(
            sys.contains("\"marks\""),
            "紧凑档 {} 的输出契约缺 \"marks\"",
            slug
        );
        assert!(
            sys.contains("\"keep\""),
            "紧凑档 {} 的输出契约缺 \"keep\"",
            slug
        );
        for absent in ["schema_version", "\"facts\"", "\"value\"", "\"quote\""] {
            assert!(
                !sys.contains(absent),
                "紧凑档 {} 的输出契约里出现了 `{}`：它不写这些字段",
                slug,
                absent
            );
        }

        // C. 逐子句覆盖：每个编号子句都要有一条 keeping 判断（`pass_prompt.rs:297`）。
        assert!(
            sys.contains("每个 cN 都要有一条"),
            "紧凑档 {} 缺逐子句覆盖要求（pass_prompt.rs:297）",
            slug
        );
    }

    // C（完整档一侧）：同一轴在完整档里用自己的措辞在场；两档不比较相等，只断"这一轴在场"。
    assert!(
        fact_system(FactFamily::Promise).contains("ONE CLAUSE, ONE FACT"),
        "完整档缺逐子句覆盖标准 `ONE CLAUSE, ONE FACT`"
    );
}

/// 用例 13c — 两档判据一致性（P0-2）：完整档（英文 8 条标准）与紧凑档（中文清单）各写一份判据，
/// 此前没有交叉检查，改一族只改一边也全绿。下表一行一轴，两档的锚点都必须在场；
/// 锚点是**各自现有的措辞**，断的是"这一轴还在不在"，不是两串相等。
///
/// 轴名照交接档 §7.7-3：耐久性、工作日志不算承诺、状态/指针/基础设施不算、禁令与周期期限算承诺。
/// 第六轴「逐字不改写」两档不对称，不进表，见下面的交叉引用。
#[test]
fn both_fact_prompts_keep_the_same_criteria() {
    // [轴名, 完整档锚点, 紧凑档锚点]
    const PARITY: &[(&str, &[&str], &[&str])] = &[
        ("耐久性", &["DURABLE AFTER THE SESSION"], &["在这段对话结束后仍然有用"]),
        (
            "工作日志不算承诺",
            &[
                "WORK LOG IS NOT A COMMITMENT",
                "narrates the step it is about to take in this same session",
            ],
            &["agent 这一轮马上要执行的步骤"],
        ),
        (
            "状态/指针/基础设施不算",
            &[
                "only reports the state",
                "where information lives",
                "narrates infrastructure",
            ],
            &["现在的工作状态", "只说文档或报告在哪", "只说主机、路径、端口"],
        ),
        (
            "禁令算承诺",
            &["A standing prohibition (do not reopen what was deferred) still counts as a commitment"],
            &["带禁令"],
        ),
        (
            "周期与期限算承诺",
            &["A plan for the next few days, a recurring schedule and a standing rule all count"],
            &["带周期性或期限"],
        ),
    ];

    // 标准段与紧凑档正文都不随族变（`STANDARDS_FACT` 一支、`fact_system_compact` 只填 family），
    // 取一族即可。
    let full = fact_system(FactFamily::Promise);
    let compact = fact_system_compact(FactFamily::Promise);
    for &(axis, full_anchors, compact_anchors) in PARITY {
        for &anchor in full_anchors {
            assert!(
                full.contains(anchor),
                "两档判据漂了：轴「{}」的完整档锚点缺原文 `{}`",
                axis,
                anchor
            );
        }
        for &anchor in compact_anchors {
            assert!(
                compact.contains(anchor),
                "两档判据漂了：轴「{}」的紧凑档锚点缺原文 `{}`",
                axis,
                anchor
            );
        }
    }

    // 第六轴「逐字不改写」：两档不对称，故不进上表。完整档靠输出契约里的
    // `quote is the verbatim substring`；紧凑档根本没有 value/quote 字段可写——输出契约只有
    // c/keep，引文由程序从子句抄（这一侧的形状由 13b 的 B 段逐字断过，这里只留交叉引用）。
    assert!(
        full.contains("quote is the verbatim substring"),
        "两档判据漂了：轴「逐字不改写」的完整档锚点缺原文"
    );
}

/// 用例 14 — 示例必须落在本族闭集里（台账 §10.26 缺陷 1）：尾段写死 `Any other value is rejected`，
/// 而三族此前共用一份正例（一条 promise + 一条 decision）⇒ plan 一族两条示例全违反自己的闭集。
/// 这一条逐行解析 GOOD 示例的 JSON，核对 `predicate` / `category` 与本次调用一致，
/// 并且示例里**不出现**另外两族的 slug。示例取值再被写成手写常量 ⇒ 红。
#[test]
fn examples_stay_inside_their_own_family() {
    for family in FactFamily::ALL {
        let sys = fact_system(family);
        let goods: Vec<&str> = sys.lines().filter(|l| l.starts_with("GOOD:")).collect();
        assert_eq!(goods.len(), 2, "{} 的正例必须两条", family.slug());
        for line in goods {
            let at = line.find("-> {").expect("正例必须带一个 JSON 对象") + 3;
            let obj: Value = serde_json::from_str(&line[at..]).expect("正例 JSON 必须可解析");
            assert_eq!(obj["predicate"], family.predicate(), "示例谓词越出本族闭集：{line}");
            assert_eq!(obj["category"], family.slug(), "示例类别越出本族闭集：{line}");
            for other in FactFamily::ALL {
                assert!(
                    other == family || !line.contains(&format!("\"{}\"", other.slug())),
                    "{} 的示例里出现了别的族的取值 {}：{line}",
                    family.slug(),
                    other.slug()
                );
            }
        }
    }
}

fn has_hex64(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    chars.windows(64).any(|window| window.iter().all(|c| c.is_ascii_hexdigit()))
}

// ── S2b-2a：fact 的**独立**审核（设计 §4 的 LLM 窄审）─────────────────────────
//
// 审核是第二次调用：自己的 canary、自己的输出契约，只读「行 + 引文 + 目标轮」。
// 它不改写、不重抽、不造 id，判决只落 pass|reject|uncertain 加一个闭集 code。

/// 一条待审的行（抽取 parser 的产物形状）。`subject_canonical` 只给审核渲染用——
/// prompt 里不许出现 anchor_id，所以名字必须由抽取侧带过来。
fn review_draft(
    reference: &str, turn_uid: String, subject: &str, object: Option<&str>,
    value: Option<&str>, quote: &str, text: &str,
) -> rc_distill::FactDraft {
    rc_distill::FactDraft {
        reference: reference.into(),
        turn_uid: turn_uid.clone(),
        subject_anchor_id: anchor_id(1),
        object_anchor_id: object.map(|_| anchor_id(2)),
        predicate: "promise".into(),
        value: value.map(str::to_string),
        category: "promise".into(),
        mode: "stated".into(),
        time_text: "下周三之前".into(),
        quote: quote.into(),
        evidence_turn_uids: vec![turn_uid],
        text: text.into(),
        subject_canonical: subject.into(),
        object_canonical: object.map(str::to_string),
    }
}

fn review_reply(family: &str, verdicts: Vec<Value>) -> String {
    json!({
        "schema_version": 0,
        "pass": "fact_review",
        "family": family,
        "verdicts": verdicts,
    })
    .to_string()
}

/// 审核 parser 的通用引用面：一条真实引文覆盖夹具里用到的所有 evidence 片段。
/// 判据只断"证据是否落在引文里"，引文本身由调用方（片内草稿）给。
const REVIEW_QUOTE: &str = "我下周三之前把对账单发给你，收到请回复";

fn review(refs: &[&str], raw: &str) -> Result<Vec<(String, ReviewVerdict, Option<String>)>, FactPassError> {
    let targets: Vec<ReviewTarget> =
        refs.iter().map(|r| ReviewTarget::new(*r, REVIEW_QUOTE)).collect();
    parse_fact_review(raw, &targets, FactFamily::Promise).map(|report| report.verdicts)
}

/// 证据闸的用例要自己定 quote（越界/缺失两种），所以单独一条入口。
fn review_report(
    targets: &[(&str, &str)],
    raw: &str,
) -> Result<FactReviewReport, FactPassError> {
    let targets: Vec<ReviewTarget> =
        targets.iter().map(|(r, quote)| ReviewTarget::new(*r, *quote)).collect();
    parse_fact_review(raw, &targets, FactFamily::Promise)
}

/// 14. 审核 prompt 是**另一次**调用：自己的 canary（首行一次）、自己的输出契约，与抽取的 canary 交叉缺席。
#[test]
fn review_prompt_is_a_separate_call_with_its_own_canary() {
    assert_ne!(FACT_REVIEW_CANARY, FACT_CANARY);
    assert_ne!(FACT_REVIEW_CHALLENGE_CANARY, FACT_REVIEW_CANARY, "反判必须是自己的 canary");
    let shape = fact_review_shape();
    assert_eq!((shape.method_steps, shape.standards, shape.positives, shape.negatives), (5, 8, 1, 7),
               "审核：方法五步（第 5 步是 G1 刀 B 的价值轴）、标准八条（第 2 条是证据闸、末条是价值轴）、正例 1、反例 7（自述无结果 + 工作日志 + 两条价值轴）；空邻居池那条只属于有矛盾轴的 pass");
    // 反判是六段同形：方法四步、标准七条（末条是 R2-1 的空邻居池）、正例 1、反例 5。
    let challenge_shape = fact_review_challenge_shape();
    assert_eq!(
        (challenge_shape.method_steps, challenge_shape.standards,
         challenge_shape.positives, challenge_shape.negatives),
        (4, 7, 1, 5),
        "反判：只依据 quote 找反证"
    );

    for family in FactFamily::ALL {
        let sys = fact_review_system(family);
        assert_eq!(count(&sys, FACT_REVIEW_CANARY), 1, "审核 canary 只在 system 首行一次");
        assert!(sys.starts_with(FACT_REVIEW_CANARY), "canary 必须在首行");
        assert!(!sys.contains(FACT_CANARY), "审核模板不借抽取的 canary");
        for old in [
            rc_distill::prompt::EXTRACT_CANARY,
            rc_distill::prompt::JUDGE_CANARY,
            rc_distill::prompt::AUDIT_CANARY,
            rc_distill::prompt::ANCHOR_CANARY,
        ] {
            assert!(!sys.contains(old), "审核模板不借别的作业的 canary");
        }
        assert!(sys.contains(&format!("\"pass\":\"{}\"", PASS_FACT_REVIEW)));
        assert!(sys.contains(&format!("\"family\":\"{}\"", family.slug())));
        // 唯一问题在场：引文支持 + 耐久承诺。
        assert!(sys.contains("durable"));
        assert!(sys.contains("one-off"));
        // 三种判决都在契约文本里；JSON 样例只给 pass/reject——可判的失败一律 reject，
        // uncertain 只留给证据本身判不了的输入（H6-fix 的示例口径）。
        for word in ["pass", "reject", "uncertain"] {
            assert!(sys.contains(word), "契约缺 verdict：{word}");
        }
        // 台账 §10.26 缺陷 4：uncertain 可以不带码；自述句要过举证责任。
        assert!(sys.contains("uncertain may carry null"), "契约没说 uncertain 可空码");
        assert!(sys.contains("SELF-REPORT IS NOT A SECOND WITNESS"), "缺举证责任标准");
        for code in REVIEW_CODES {
            assert!(sys.contains(code), "缺 code：{code}");
        }
        // 不改写、不重抽、不造 id。
        assert!(sys.contains("Do not rewrite") || sys.contains("do not rewrite"));
        assert!(sys.contains("Do not add"));
        assert!(sys.contains("identity"));
    }

    // 反向：抽取模板里不许出现审核 canary。
    for family in FactFamily::ALL {
        assert!(!fact_system(family).contains(FACT_REVIEW_CANARY));
    }
}

/// 14b. 反判的 canary 双向缺席：自己的首行恰好一次；challenger 里没有 proposer 的 canary，
///      proposer 里也没有 challenger 的；抽取模板两个都不借（工单 §2.2 / §5 第 5 条）。
#[test]
fn the_challenger_has_its_own_canary_and_the_two_roles_never_borrow_each_other() {
    for family in FactFamily::ALL {
        let challenge = fact_review_challenge_system(family);
        assert_eq!(
            count(&challenge, FACT_REVIEW_CHALLENGE_CANARY),
            1,
            "反判 canary 只在 challenger system 首行一次"
        );
        assert!(challenge.starts_with(FACT_REVIEW_CHALLENGE_CANARY), "canary 必须在首行");
        assert!(!challenge.contains(FACT_REVIEW_CANARY), "challenger 不许出现 proposer 的 canary");
        assert!(!challenge.contains(FACT_CANARY), "challenger 不借抽取的 canary");

        let proposer = fact_review_system(family);
        assert_eq!(count(&proposer, FACT_REVIEW_CANARY), 1, "proposer canary 仍是一次");
        assert!(
            !proposer.contains(FACT_REVIEW_CHALLENGE_CANARY),
            "proposer 不许出现 challenger 的 canary"
        );
        assert!(
            !fact_system(family).contains(FACT_REVIEW_CHALLENGE_CANARY),
            "抽取模板不许出现 challenger 的 canary"
        );

        // 两档同形：code 闭集、四键与证据契约都在场；角色句在 challenger 里。
        for code in REVIEW_CODES {
            assert!(challenge.contains(code), "反判缺 code：{code}");
        }
        assert!(challenge.contains("evidence"), "反判契约必须写 evidence");
        assert!(challenge.contains("ONLY THE QUOTE IS EVIDENCE"));
        assert!(challenge.contains("never repair") || challenge.contains("Never repair"));
        assert!(challenge.contains(&format!("\"pass\":\"{}\"", PASS_FACT_REVIEW)));
        assert!(challenge.contains(&format!("\"family\":\"{}\"", family.slug())));
    }
}

/// 15. 审核 user 只渲染该行 + 引文 + 目标轮：无 uid / 毫秒 / anchor_id / 生成理由，也不给抽取侧写下的整句（那是抽取的结论，不是证据）。
#[test]
fn review_prompt_shows_only_the_row_quote_and_target_turn() {
    let b = batch(&["好的，收到", "我下周三之前把对账单发给你，顺手抄送财务。"]);
    let d = review_draft(
        "f1", b.rows[1].turn_uid.clone(), "张三", Some("李四"), Some("把对账单发给李四"),
        "我下周三之前把对账单发给你", "张三承诺在下周三之前把对账单发给李四。");
    let user = fact_review_user(FactFamily::Promise, &[d], &b, &review_ctx(&b));

    assert!(user.lines().next().unwrap().contains("fact_review"), "首行点明这是审核调用");
    assert!(user.lines().next().unwrap().contains("family promise"));
    assert!(user.contains("ITEM f1"));
    assert!(user.contains("subject: 张三"), "主语给 canonical，不给 anchor_id");
    assert!(user.contains("predicate: promise"));
    assert!(user.contains("object: 李四"));
    assert!(user.contains("value: 把对账单发给李四"));
    assert!(user.contains("category: promise"));
    assert!(user.contains("quote: 我下周三之前把对账单发给你"));
    assert!(user.contains("我下周三之前把对账单发给你，顺手抄送财务。"),
            "目标轮原文完整给出（引文只是它的子串）");
    // C6a：邻轮只进 `CTX (not evidence)` 段，不进 TARGET 段。
    let ctx_at = user.find("CTX (not evidence):").expect("有邻轮时必须出现 CTX 段");
    assert!(user[..ctx_at].contains("TARGET TURN"));
    assert!(!user[..ctx_at].contains("好的，收到"), "邻轮不进 TARGET 段");
    assert!(user[ctx_at..].contains("好的，收到"), "前一轮在 CTX 段");

    // 身份与时钟一个字都不给。
    assert!(!user.contains(&uid(0)) && !user.contains(&uid(1)), "user 不渲染 turn_uid");
    assert!(!has_hex64(&user), "user 不含 64 位 hex");
    assert!(!user.contains(&anchor_id(1)) && !user.contains(&anchor_id(2)), "user 不渲染 anchor_id");
    assert!(!user.contains("anchor_id"), "连字段名都不出现");
    assert!(!user.contains("_ms"), "user 不渲染毫秒字段");
    assert!(!user.contains("confidence"), "不给 confidence");
    assert!(!user.contains("reason"), "不给生成理由");
    // 抽取侧写下的整句不进审核：审的是「引文支持不支持」，不是「抽取写的顺不顺」。
    assert!(!user.contains("张三承诺在下周三之前把对账单发给李四。"));

    // 没传进来的草稿一个字都不出现（每批只审给定的行）。
    assert!(!user.contains("ITEM f2"));
}

/// 16. 三分支合法：pass/pass、reject/not_durable、uncertain/quote_mismatch。
#[test]
fn review_parser_accepts_the_three_verdict_branches() {
    let raw = review_reply("promise", vec![
        json!({"ref": "f1", "verdict": "pass", "code": null, "evidence": "把对账单发给你"}),
        json!({"ref": "f2", "verdict": "reject", "code": "not_durable", "evidence": "收到"}),
        json!({"ref": "f3", "verdict": "uncertain", "code": "quote_mismatch", "evidence": "收到"}),
    ]);
    let got = review(&["f1", "f2", "f3"], &raw).unwrap();
    assert_eq!(got, vec![
        ("f1".to_string(), ReviewVerdict::Pass, None),
        ("f2".to_string(), ReviewVerdict::Reject, Some("not_durable".to_string())),
        ("f3".to_string(), ReviewVerdict::Uncertain, Some("quote_mismatch".to_string())),
    ]);
}

/// 17. 返回顺序跟 `refs` 走，不跟回复顺序走；`other`/`missing_subject` 也合法。
#[test]
fn review_parser_returns_in_refs_order_and_accepts_every_code() {
    let raw = review_reply("promise", vec![
        json!({"ref": "f2", "verdict": "reject", "code": "missing_subject", "evidence": "收到"}),
        json!({"ref": "f1", "verdict": "reject", "code": "other", "evidence": "收到"}),
    ]);
    let got = review(&["f1", "f2"], &raw).unwrap();
    assert_eq!(got[0].0, "f1");
    assert_eq!(got[1].0, "f2");
    assert_eq!(got[0].2.as_deref(), Some("other"));
    assert_eq!(got[1].2.as_deref(), Some("missing_subject"));
}

/// 18. 每个 ref 恰好一次：缺 / 多 / 重复。
#[test]
fn review_parser_requires_each_ref_exactly_once() {
    // 缺：f1 给了，f2/f3 没给。
    let raw = review_reply("promise", vec![
        json!({"ref": "f1", "verdict": "pass", "code": null, "evidence": "把对账单发给你"}),
    ]);
    assert_eq!(
        review(&["f1", "f2", "f3"], &raw),
        Err(FactPassError::MissingReviewRef(vec!["f2".into(), "f3".into()]))
    );

    // 一条都没有 ⇒ 每个 ref 都缺。
    let raw = review_reply("promise", vec![]);
    assert_eq!(
        review(&["f1"], &raw),
        Err(FactPassError::MissingReviewRef(vec!["f1".into()]))
    );

    // 多：批外引用。
    let raw = review_reply("promise", vec![
        json!({"ref": "f1", "verdict": "pass", "code": null, "evidence": "把对账单发给你"}),
        json!({"ref": "f9", "verdict": "pass", "code": null, "evidence": "把对账单发给你"}),
    ]);
    assert_eq!(review(&["f1"], &raw), Err(FactPassError::UnknownReviewRef("f9".into())));

    // 重复。
    let raw = review_reply("promise", vec![
        json!({"ref": "f1", "verdict": "pass", "code": null, "evidence": "把对账单发给你"}),
        json!({"ref": "f1", "verdict": "reject", "code": "other", "evidence": "收到"}),
    ]);
    assert_eq!(review(&["f1"], &raw), Err(FactPassError::DuplicateReviewRef("f1".into())));

    // 空 ref / 空 ref 表都按形状处理。
    let raw = review_reply("promise", vec![
        json!({"ref": "", "verdict": "pass", "code": null}),
    ]);
    assert_eq!(review(&["f1"], &raw), Err(FactPassError::Json));
    let raw = review_reply("promise", vec![]);
    assert_eq!(review(&[], &raw), Ok(vec![]), "没有待审行 ⇒ 空判决合法");
}

/// 19. verdict 闭集；pass 的 code 必须是 null；reject/uncertain 必须有闭集内的 code。
#[test]
fn review_parser_closes_verdict_and_code_sets() {
    let bad_verdict = review_reply("promise", vec![
        json!({"ref": "f1", "verdict": "maybe", "code": null}),
    ]);
    assert_eq!(review(&["f1"], &bad_verdict), Err(FactPassError::BadReviewVerdict("maybe".into())));

    let pass_with_code = review_reply("promise", vec![
        json!({"ref": "f1", "verdict": "pass", "code": "other"}),
    ]);
    assert_eq!(review(&["f1"], &pass_with_code), Err(FactPassError::BadReviewCode("f1".into())));

    let reject_without_code = review_reply("promise", vec![
        json!({"ref": "f1", "verdict": "reject", "code": null}),
    ]);
    assert_eq!(review(&["f1"], &reject_without_code), Err(FactPassError::BadReviewCode("f1".into())));

    let reject_unknown_code = review_reply("promise", vec![
        json!({"ref": "f1", "verdict": "reject", "code": "unsupported"}),
    ]);
    assert_eq!(review(&["f1"], &reject_unknown_code), Err(FactPassError::BadReviewCode("f1".into())));

    // 台账 §10.26 缺陷 4：`uncertain` 允许不带码。
    let uncertain_no_code = review_reply("promise", vec![
        json!({"ref": "f1", "verdict": "uncertain", "code": null}),
    ]);
    assert_eq!(
        review(&["f1"], &uncertain_no_code),
        Ok(vec![("f1".to_string(), ReviewVerdict::Uncertain, None)]),
        "uncertain 带 null 码必须被收下"
    );

    let uncertain_ok = review_reply("promise", vec![
        json!({"ref": "f1", "verdict": "uncertain", "code": "other"}),
    ]);
    assert_eq!(
        review(&["f1"], &uncertain_ok),
        Ok(vec![("f1".to_string(), ReviewVerdict::Uncertain, Some("other".to_string()))])
    );
}

/// 20. 信封：多键/少键/类型错 ⇒ Json；version/pass/family 各自一格。
#[test]
fn review_parser_rejects_wrong_envelope_and_unknown_keys() {
    let ok = review_reply("promise", vec![
        json!({"ref": "f1", "verdict": "pass", "code": null, "evidence": "把对账单发给你"}),
    ]);
    let mut doc: Value = serde_json::from_str(&ok).unwrap();

    assert_eq!(review(&["f1"], "这不是 json"), Err(FactPassError::Json));
    assert_eq!(review(&["f1"], "[]"), Err(FactPassError::Json));

    let mut extra_top = doc.clone();
    extra_top["reason"] = json!("模型解释");
    assert_eq!(review(&["f1"], &extra_top.to_string()), Err(FactPassError::Json));

    let mut extra_item = doc.clone();
    extra_item["verdicts"][0]["confidence"] = json!(0.9);
    assert_eq!(review(&["f1"], &extra_item.to_string()), Err(FactPassError::Json));

    // 三个键都必填：`code: null` 才是"没有 code"，缺键是形状错。
    let mut missing_code = doc.clone();
    missing_code["verdicts"][0].as_object_mut().unwrap().remove("code");
    assert_eq!(review(&["f1"], &missing_code.to_string()), Err(FactPassError::Json));

    let mut version = doc.clone();
    version["schema_version"] = json!(1);
    assert_eq!(review(&["f1"], &version.to_string()), Err(FactPassError::Version));

    let mut pass = doc.clone();
    pass["pass"] = json!("fact");
    assert_eq!(review(&["f1"], &pass.to_string()), Err(FactPassError::Pass));

    let mut family = doc.clone();
    family["family"] = json!("plan");
    assert_eq!(review(&["f1"], &family.to_string()), Err(FactPassError::Family));

    // 顶层缺 family 也是 Json。
    doc.as_object_mut().unwrap().remove("family");
    assert_eq!(review(&["f1"], &doc.to_string()), Err(FactPassError::Json));
}

/// 25. 工单 §7.2 证据闸（先红）：`pass`/`reject` 都必须带 quote 内的一段连续原文。
///     两条都没有 `evidence`：一条 pass、一条 reject —— 一律降 `uncertain`，
///     "无证据的 reject" 绝不许继续当 reject 用，"无证据的 pass" 也绝不默认放行。
#[test]
fn evidence_missing_never_stays_pass_or_reject() {
    let raw = review_reply("promise", vec![
        json!({"ref": "f1", "verdict": "pass", "code": null}),
        json!({"ref": "f2", "verdict": "reject", "code": "not_durable"}),
    ]);
    let quote = "我下周三之前把对账单发给你";
    let got = review_report(&[("f1", quote), ("f2", quote)], &raw).unwrap();
    assert_eq!(
        got.verdicts,
        vec![
            ("f1".to_string(), ReviewVerdict::Uncertain, None),
            ("f2".to_string(), ReviewVerdict::Uncertain, None),
        ],
        "没有 evidence 的 pass/reject 必须逐条降 uncertain"
    );
    assert_eq!(
        got.downgrades,
        vec![
            ("f1".to_string(), EvidenceDowngrade::Missing),
            ("f2".to_string(), EvidenceDowngrade::Missing),
        ],
        "降级原因是程序侧的 evidence_missing，不进 REVIEW_CODES"
    );
    for reason in [EvidenceDowngrade::Missing, EvidenceDowngrade::NotInQuote] {
        assert!(!REVIEW_CODES.contains(&reason.as_str()), "程序原因不许混进模型 code 闭集");
    }
}

/// 26. evidence 不在 quote 内 ⇒ 同样降级并记 `evidence_not_in_quote`；uncertain 带越界证据
///     也照样降级（它本来就是 uncertain，但原因必须被记下来）。
#[test]
fn evidence_outside_the_quote_downgrades_every_verdict() {
    let raw = review_reply("promise", vec![
        json!({"ref": "f1", "verdict": "pass", "code": null, "evidence": "李四"}),
        json!({"ref": "f2", "verdict": "reject", "code": "not_durable", "evidence": "下周再聊"}),
        json!({"ref": "f3", "verdict": "uncertain", "code": "quote_mismatch", "evidence": "李四"}),
    ]);
    let quote = "我下周三之前把对账单发给你";
    let got = review_report(&[("f1", quote), ("f2", quote), ("f3", quote)], &raw).unwrap();
    assert_eq!(
        got.verdicts,
        vec![
            ("f1".to_string(), ReviewVerdict::Uncertain, None),
            ("f2".to_string(), ReviewVerdict::Uncertain, None),
            ("f3".to_string(), ReviewVerdict::Uncertain, None),
        ],
        "越界证据的 pass/reject 降 uncertain；uncertain 的 code 也清空"
    );
    assert_eq!(
        got.downgrades,
        vec![
            ("f1".to_string(), EvidenceDowngrade::NotInQuote),
            ("f2".to_string(), EvidenceDowngrade::NotInQuote),
            ("f3".to_string(), EvidenceDowngrade::NotInQuote),
        ]
    );
}

/// 27. 证据真的落在 quote 里 ⇒ 判决照旧；uncertain 可以不带证据，也可以带块内证据。
///     这条是 25/26 的对照物：闸不是把所有判决都降级。
#[test]
fn evidence_inside_the_quote_keeps_the_verdict() {
    let quote = "我下周三之前把对账单发给你，收到请回复";
    let raw = review_reply("promise", vec![
        json!({"ref": "f1", "verdict": "pass", "code": null, "evidence": "把对账单发给你"}),
        json!({"ref": "f2", "verdict": "reject", "code": "not_durable", "evidence": "收到"}),
        json!({"ref": "f3", "verdict": "uncertain", "code": null, "evidence": null}),
        json!({"ref": "f4", "verdict": "uncertain", "code": "other", "evidence": "下周三"}),
    ]);
    let got = review_report(
        &[("f1", quote), ("f2", quote), ("f3", quote), ("f4", quote)],
        &raw,
    )
    .unwrap();
    assert_eq!(
        got.verdicts,
        vec![
            ("f1".to_string(), ReviewVerdict::Pass, None),
            ("f2".to_string(), ReviewVerdict::Reject, Some("not_durable".to_string())),
            ("f3".to_string(), ReviewVerdict::Uncertain, None),
            ("f4".to_string(), ReviewVerdict::Uncertain, Some("other".to_string())),
        ]
    );
    assert_eq!(got.downgrades, vec![], "证据合格的条目一条都不许降级");
    assert!(
        got.verdicts
            .iter()
            .all(|(_, verdict, _)| matches!(verdict, ReviewVerdict::Pass
                | ReviewVerdict::Reject
                | ReviewVerdict::Uncertain)),
        "降级之后逐条仍有判决（不是整批拒）"
    );
}

/// 24 个变体的 slug 表——这张表就是闭集本身：新增变体不在这里补一行，穷尽 match 会先翻脸，
/// `seen.len()` 会再翻一次。
fn slug_cases() -> Vec<(FactPassError, &'static str)> {
    vec![
        (FactPassError::Json, "json"),
        (FactPassError::Version, "version"),
        (FactPassError::Pass, "pass"),
        (FactPassError::Family, "family"),
        (FactPassError::UnknownTurn("t9".into()), "unknown_turn"),
        (FactPassError::UnknownCandidate("a9".into()), "unknown_candidate"),
        (FactPassError::Category, "category"),
        (FactPassError::Predicate, "predicate"),
        (FactPassError::Mode, "mode"),
        (FactPassError::MissingSubject, "missing_subject"),
        (FactPassError::MissingValue, "missing_value"),
        (FactPassError::EmptyEvidence, "empty_evidence"),
        (FactPassError::EvidenceNotTarget("t9".into()), "evidence_not_target"),
        (FactPassError::DuplicateRef("f1".into()), "duplicate_ref"),
        (FactPassError::QuoteNotVerbatim("f1".into()), "quote_not_verbatim"),
        (FactPassError::QuoteTooLong, "quote_too_long"),
        (FactPassError::Ellipsis, "ellipsis"),
        (FactPassError::NumberNotGrounded("12".into()), "number_not_grounded"),
        (FactPassError::Uncovered(vec!["t2".into()]), "uncovered"),
        (FactPassError::UnknownReviewRef("f9".into()), "review_unknown_ref"),
        (FactPassError::DuplicateReviewRef("f1".into()), "review_duplicate_ref"),
        (FactPassError::MissingReviewRef(vec!["f2".into()]), "review_missing_ref"),
        (FactPassError::BadReviewVerdict("maybe".into()), "review_verdict"),
        (FactPassError::BadReviewCode("f1".into()), "review_code"),
    ]
}

/// 21. 每一格一个稳定 slug：只有小写 ASCII 与下划线、无空格、≤32 字符、彼此不重样。
///     `review_reason()` 对自带前缀的格原样返回，对共用的信封格只补一次 `review_`。
#[test]
fn every_error_variant_has_a_stable_slug_and_a_review_reason() {
    let cases = slug_cases();
    let mut seen: Vec<&'static str> = Vec::new();
    for (error, want) in &cases {
        let slug = error.slug();
        assert_eq!(slug, *want, "{error:?}");
        assert!(
            slug.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "slug 只许小写 ASCII 与下划线：{slug}"
        );
        assert!(!slug.starts_with('_') && !slug.ends_with('_'), "{slug}");
        assert!(slug.len() <= 32, "{slug} 太长");
        assert!(!seen.contains(&slug), "两个变体共用一个 slug：{slug}");
        seen.push(slug);

        // 审核失败落库用的词：自带前缀的原样，其余补一次 `review_`。
        let reason = error.review_reason();
        assert!(!reason.contains("review_review_"), "{reason}");
        if slug.starts_with("review_") {
            assert_eq!(reason, slug, "{error:?}");
        } else {
            assert_eq!(reason, format!("review_{slug}"), "{error:?}");
        }
        assert!(reason.chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{reason}");
        assert!(reason.len() <= 32, "{reason} 太长");
    }
    assert_eq!(seen.len(), 24, "闭集大小就是这样表：新增变体必须在这里补一行");
}

/// 22. `review_reason()` 的四个共用信封格：审核坏 JSON 是 `review_json`，不是 `review_schema`。
#[test]
fn shared_envelope_errors_get_exactly_one_review_prefix() {
    assert_eq!(FactPassError::Json.slug(), "json");
    assert_eq!(FactPassError::Json.review_reason(), "review_json");
    assert_eq!(FactPassError::Version.review_reason(), "review_version");
    assert_eq!(FactPassError::Pass.review_reason(), "review_pass");
    assert_eq!(FactPassError::Family.review_reason(), "review_family");
    // 抽取侧不会带 review_ 前缀，两边的词表靠前缀分开。
    assert!(!FactPassError::QuoteNotVerbatim("f1".into()).slug().starts_with("review_"));
}

// ── S2e-2：整段 fenced JSON ────────────────────────────────────────────────────
//
// 真实模型把**整段**输出包进 ```` ```json … ``` ```` 是常态（冒烟里 event 的拒收就是这个形状）。
// 剥围栏的条件只有一个：**整段**恰好是一个围栏块；剥完仍走上面同一把尺子（`deny_unknown_fields`
// 一字不动）。围栏外有说明文字、两个围栏块、围栏标记拼错，一律照旧 `Json`。

/// 23. 整段一个围栏块（带 `json` 标记或不带、单行或换行、前后空白）⇒ 抽取与审核都收。
#[test]
fn a_whole_fenced_block_is_accepted_by_output_and_review() {
    let b = promise_batch();
    let c = sample_candidates();
    let body = promise_reply();

    for raw in [
        format!("```json\n{body}\n```"),
        format!("```\n{body}\n```"),
        // 围栏标记与正文之间没有换行也是同一个块。
        format!("```json{body}```"),
        // 围栏块前后的空白不算"杂质"（`trim` 是剥围栏的前提）。
        format!("\n  ```json\n{body}\n```  \n"),
    ] {
        assert_eq!(
            parse(&raw, &b, &c, FactFamily::Promise).unwrap().len(),
            1,
            "整段围栏块必须被接受：{raw}"
        );
    }

    let rbody = review_reply("promise", vec![json!({"ref": "f1", "verdict": "pass", "code": null, "evidence": "把对账单发给你"})]);
    for raw in [
        format!("```json\n{rbody}\n```"),
        format!("```\n{rbody}\n```"),
        format!("```json{rbody}```"),
    ] {
        assert_eq!(
            review(&["f1"], &raw),
            Ok(vec![("f1".to_string(), ReviewVerdict::Pass, None)]),
            "审核的整段围栏块必须被接受：{raw}"
        );
    }
}

/// 24. 围栏外有杂质 ⇒ 仍 `Json`；围栏内仍是严格 JSON（多键/坏类型/坏版本各归原错误）。
#[test]
fn fenced_json_outside_prose_still_fails_and_the_inner_grammar_is_unchanged() {
    let b = promise_batch();
    let c = sample_candidates();
    let body = promise_reply();
    let short = &body[..body.rfind('}').unwrap()];

    // 整段不是"一个围栏块"的四种形状：围栏前说明、围栏后说明、围栏后再来一个块、标记拼错。
    for raw in [
        format!("说明文字\n{body}"),
        format!("{body}\n说明文字"),
        format!("```json\n{body}\n```\n说明文字"),
        format!("```json\n{body}\n```\n```json\n{body}\n```"),
        format!("```JSON\n{body}\n```"),
        format!("~~~json\n{body}\n~~~"),
        // 围栏里被截断的正文：围栏逻辑不替模型补齐。
        format!("```json\n{short}\n```"),
    ] {
        assert_eq!(
            parse(&raw, &b, &c, FactFamily::Promise),
            Err(FactPassError::Json),
            "带散文/杂质的输出必须仍被拒：{raw}"
        );
    }

    // 围栏内多一个键：`deny_unknown_fields` 仍在，不是被围栏逻辑吞掉。
    let mut extra_item: Value = serde_json::from_str(&body).unwrap();
    extra_item["facts"][0]["confidence"] = json!(0.9);
    assert_eq!(
        parse(&format!("```json\n{extra_item}\n```"), &b, &c, FactFamily::Promise),
        Err(FactPassError::Json)
    );
    // 围栏内顶层多一个键：同样 Json。
    let mut extra_top: Value = serde_json::from_str(&body).unwrap();
    extra_top["db_id"] = json!(7);
    assert_eq!(
        parse(&format!("```json\n{extra_top}\n```"), &b, &c, FactFamily::Promise),
        Err(FactPassError::Json)
    );
    // 围栏内坏类型：Json。
    let mut bad_type: Value = serde_json::from_str(&body).unwrap();
    bad_type["facts"] = json!({"not": "a list"});
    assert_eq!(
        parse(&format!("```json\n{bad_type}\n```"), &b, &c, FactFamily::Promise),
        Err(FactPassError::Json)
    );
    // 围栏内坏版本：仍是 `Version`，不被折成 `Json`。
    let mut version: Value = serde_json::from_str(&body).unwrap();
    version["schema_version"] = json!(1);
    assert_eq!(
        parse(&format!("```json\n{version}\n```"), &b, &c, FactFamily::Promise),
        Err(FactPassError::Version)
    );

    // 审核侧同一条尺子。
    let rbody = review_reply("promise", vec![json!({"ref": "f1", "verdict": "pass", "code": null, "evidence": "把对账单发给你"})]);
    assert_eq!(review(&["f1"], &format!("说明\n{rbody}")), Err(FactPassError::Json));
    assert_eq!(review(&["f1"], &format!("{rbody}\n说明")), Err(FactPassError::Json));
    assert_eq!(
        review(&["f1"], &format!("```json\n{rbody}\n``` 额外文字")),
        Err(FactPassError::Json)
    );
    let mut rextra: Value = serde_json::from_str(&rbody).unwrap();
    rextra["verdicts"][0]["confidence"] = json!(0.9);
    assert_eq!(
        review(&["f1"], &format!("```json\n{rextra}\n```")),
        Err(FactPassError::Json),
        "审核围栏内多键仍归 Json"
    );
}

// ── S2e-8a：宽松解析（逐条丢弃，不拖整批）────────────────────────────────────
//
// 真实冒烟：3 条草稿里 1 条 quote 挪了一个反引号位置，严格 parser 把整批判死。
// 设计 `s2-prompt-v0` §2.2/§3 要的是"单条坏 ⇒ 丢该条 + 记住原因，其余照常返回"。
// 这里断的正是这条分界：**内容**错丢一条，**信封/批次级声明**错仍整批拒。

/// t1（寒暄）上的一条合法 promise fact：给"其余照常返回"当对照物。
fn greeting_fact() -> Value {
    json!({
        "ref": "f2",
        "turn": "t1",
        "subject": "a1",
        "predicate": "promise",
        "object": "a2",
        "value": "收到",
        "category": "promise",
        "mode": "stated",
        "time_text": null,
        "quote": "好的，收到",
        "evidence": ["t1"],
        "text": "张三确认收到。"
    })
}

fn lenient(
    raw: &str, b: &Batch, c: &[FactCandidate], family: FactFamily,
) -> Result<LenientFacts, FactPassError> {
    parse_fact_output_lenient(raw, b, c, family)
}

/// 25. 一条坏 quote + 一条合法 ⇒ Ok：只丢坏的那条，原因按原顺序记，uncovered 为空。
///     坏 quote 的形状是**改了一个字**（你→他）：行内标记已由 `evidence_view` 归一，
///     不逐字必须来自内容差异，不能靠 `**`/反引号。
#[test]
fn lenient_drops_one_bad_quote_and_keeps_the_rest() {
    let b = promise_batch();
    let c = sample_candidates();

    let mut mangled = promise_fact();
    mangled["quote"] = json!("我下周三之前把对账单发给他");
    let raw = doc("promise", vec![mangled, greeting_fact()], &["t2", "t3"]);

    let got = lenient(&raw, &b, &c, FactFamily::Promise).unwrap();
    assert_eq!(got.drafts.len(), 1, "合法那条照常返回");
    assert_eq!(got.drafts[0].reference, "f2");
    assert_eq!(got.drafts[0].turn_uid, b.rows[0].turn_uid, "t1 换成真实 uid");
    assert_eq!(got.dropped, vec![FactPassError::QuoteNotVerbatim("f1".into())]);
    assert_eq!(got.uncovered, Vec::<String>::new(), "t2 由 empty_turns 认领，t3 同");
}

/// 26. 全部坏 ⇒ 仍然 Ok：drafts 空、dropped 数量与顺序正确。宽松不是"至少留一条"，
///     也不是把整批折成 Err——它只是把每一条的坏法分别记账。
#[test]
fn lenient_keeps_going_when_every_draft_is_bad() {
    let b = promise_batch();
    let c = sample_candidates();

    let mut mangled = promise_fact();
    mangled["quote"] = json!("我下周三之前把对账单发给他");
    let mut no_value = promise_fact();
    no_value["ref"] = json!("f2");
    no_value["object"] = json!(null);
    no_value["value"] = json!(null);
    let raw = doc("promise", vec![mangled, no_value], &["t1", "t2", "t3"]);

    let got = lenient(&raw, &b, &c, FactFamily::Promise).unwrap();
    assert!(got.drafts.is_empty());
    assert_eq!(
        got.dropped,
        vec![
            FactPassError::QuoteNotVerbatim("f1".into()),
            FactPassError::MissingValue,
        ],
        "dropped 顺序同原 facts"
    );
    assert!(got.uncovered.is_empty());
}

/// 27. 信封四格仍是整批拒：顶层多键 / 版本 / family / `empty_turns` 批外引用。
///     这几格坏掉时"其余草稿"没有意义，绝不降级成 dropped。
#[test]
fn lenient_still_rejects_the_envelope_and_batch_level_declarations() {
    let b = promise_batch();
    let c = sample_candidates();
    let good = promise_reply();
    let mut value: Value = serde_json::from_str(&good).unwrap();

    assert_eq!(
        lenient("这不是 json", &b, &c, FactFamily::Promise),
        Err(FactPassError::Json)
    );

    let mut top_extra = value.clone();
    top_extra["db_id"] = json!(7);
    assert_eq!(
        lenient(&top_extra.to_string(), &b, &c, FactFamily::Promise),
        Err(FactPassError::Json)
    );

    // 单条多键也是形状错：它在反序列化里就坏了，整批 `Json`（不是丢一条）。
    let mut fact_extra = value.clone();
    fact_extra["facts"][0]["confidence"] = json!(0.9);
    assert_eq!(
        lenient(&fact_extra.to_string(), &b, &c, FactFamily::Promise),
        Err(FactPassError::Json)
    );

    let mut version = value.clone();
    version["schema_version"] = json!(1);
    assert_eq!(
        lenient(&version.to_string(), &b, &c, FactFamily::Promise),
        Err(FactPassError::Version)
    );

    let mut pass = value.clone();
    pass["pass"] = json!("event");
    assert_eq!(
        lenient(&pass.to_string(), &b, &c, FactFamily::Promise),
        Err(FactPassError::Pass)
    );

    let mut family = value.clone();
    family["family"] = json!("plan");
    assert_eq!(
        lenient(&family.to_string(), &b, &c, FactFamily::Promise),
        Err(FactPassError::Family)
    );

    // empty_turns 是批次级声明：批外引用与重复都整批拒。
    value["empty_turns"] = json!(["t1", "t9"]);
    assert_eq!(
        lenient(&value.to_string(), &b, &c, FactFamily::Promise),
        Err(FactPassError::UnknownTurn("t9".into()))
    );
    value["empty_turns"] = json!(["t1", "t1"]);
    assert_eq!(
        lenient(&value.to_string(), &b, &c, FactFamily::Promise),
        Err(FactPassError::DuplicateRef("t1".into()))
    );
}

/// 28. 未覆盖目标进 `uncovered`（有序、去重），不因此整批失败；`empty_turns` 认领后不再列。
#[test]
fn lenient_lists_uncovered_targets_and_keeps_the_drafts() {
    let b = promise_batch();
    let c = sample_candidates();

    // 只有 t2 有合法草稿，t1/t3 谁也没提。
    let raw = doc("promise", vec![promise_fact()], &[]);
    let got = lenient(&raw, &b, &c, FactFamily::Promise).unwrap();
    assert_eq!(got.drafts.len(), 1);
    assert_eq!(got.uncovered, vec!["t1".to_string(), "t3".to_string()]);
    assert!(got.dropped.is_empty(), "覆盖缺口不是草稿的坏法");

    // empty_turns 认领 t1/t3 ⇒ 缺口消失。
    let raw = doc("promise", vec![promise_fact()], &["t1", "t3"]);
    let got = lenient(&raw, &b, &c, FactFamily::Promise).unwrap();
    assert!(got.uncovered.is_empty());

    // 被丢掉的那条不算覆盖：它的 turn 若没人认领，仍进 uncovered。
    let mut mangled = promise_fact();
    mangled["quote"] = json!("我下周三之前把对账单发给他");
    let raw = doc("promise", vec![mangled], &["t1", "t3"]);
    let got = lenient(&raw, &b, &c, FactFamily::Promise).unwrap();
    assert_eq!(got.uncovered, vec!["t2".to_string()], "坏草稿不覆盖它的 turn");
}

/// 29. 重复 ref：先出现的那条保留，第二条进 dropped；返回的 drafts 里 ref 仍唯一。
#[test]
fn lenient_drops_the_second_duplicate_ref_and_keeps_the_first() {
    let b = promise_batch();
    let c = sample_candidates();

    let mut second = greeting_fact();
    second["ref"] = json!("f1");
    let raw = doc("promise", vec![promise_fact(), second], &["t1", "t3"]);

    let got = lenient(&raw, &b, &c, FactFamily::Promise).unwrap();
    assert_eq!(got.drafts.len(), 1);
    assert_eq!(got.drafts[0].reference, "f1");
    assert_eq!(got.drafts[0].turn_uid, b.rows[1].turn_uid, "保留的是第一条（t2）");
    assert_eq!(got.dropped, vec![FactPassError::DuplicateRef("f1".into())]);
    assert!(got.uncovered.is_empty());
}

/// 30. 其余逐条格子也各丢一条、不连坐：批外 turn / 批外候选 / 数字不落地 / 空 ref。
///     注意 `turn` 与 `empty_turns` 的层级不同：单条 fact 的 turn 错是丢一条，
///     `empty_turns` 的短 id 错是整批拒（上一组已断）。
#[test]
fn lenient_drops_each_content_error_kind_per_entry() {
    let b = promise_batch();
    let c = sample_candidates();

    let mut unknown_turn = promise_fact();
    unknown_turn["turn"] = json!("t9");
    unknown_turn["evidence"] = json!(["t9"]);

    let mut unknown_subject = promise_fact();
    unknown_subject["ref"] = json!("f2");
    unknown_subject["subject"] = json!("a9");

    let mut ungrounded = promise_fact();
    ungrounded["ref"] = json!("f3");
    ungrounded["text"] = json!("张三承诺等 10 天后再发对账单。");

    let mut blank_ref = promise_fact();
    blank_ref["ref"] = json!("  ");

    let raw = doc(
        "promise",
        vec![unknown_turn, unknown_subject, ungrounded, blank_ref],
        &["t1", "t2", "t3"],
    );
    let got = lenient(&raw, &b, &c, FactFamily::Promise).unwrap();
    assert!(got.drafts.is_empty());
    assert_eq!(
        got.dropped,
        vec![
            FactPassError::UnknownTurn("t9".into()),
            FactPassError::UnknownCandidate("a9".into()),
            FactPassError::NumberNotGrounded("10".into()),
            FactPassError::Json,
        ]
    );
    assert!(got.uncovered.is_empty());
}

/// 31. 回归：同一批输入喂严格 parser，仍在**第一条**错处停下（不因为有了宽松 parser 而变软）。
#[test]
fn strict_parser_still_returns_the_first_error_on_the_same_input() {
    let b = promise_batch();
    let c = sample_candidates();

    let mut mangled = promise_fact();
    mangled["quote"] = json!("我下周三之前把对账单发给他");
    let raw = doc("promise", vec![mangled, greeting_fact()], &["t2", "t3"]);
    assert_eq!(
        parse(&raw, &b, &c, FactFamily::Promise),
        Err(FactPassError::QuoteNotVerbatim("f1".into())),
        "严格侧仍整批判死，且是第一条错"
    );

    let mut second = greeting_fact();
    second["ref"] = json!("f1");
    let raw = doc("promise", vec![promise_fact(), second], &["t1", "t3"]);
    assert_eq!(
        parse(&raw, &b, &c, FactFamily::Promise),
        Err(FactPassError::DuplicateRef("f1".into()))
    );

    // 覆盖缺口在严格侧仍是 `Uncovered`；宽松侧只列进 uncovered。
    let raw = doc("promise", vec![promise_fact()], &[]);
    assert_eq!(
        parse(&raw, &b, &c, FactFamily::Promise),
        Err(FactPassError::Uncovered(vec!["t1".into(), "t3".into()]))
    );
    assert_eq!(
        lenient(&raw, &b, &c, FactFamily::Promise).unwrap().uncovered,
        vec!["t1".to_string(), "t3".to_string()]
    );
}

/// 32. （P0-1）`value` 必须逐字落在 `quote` 内：模型显式给的值不是引文区间 ⇒ `ValueNotInQuote`。
///
/// `VERBATIM` 标准要求 quote 覆盖 value 的每一个字符；此前程序只核对 text 里的数字，
/// 模型改写 value 也能过闸，判同键与溯源证据就建在不可信的值上。这条闸只对**模型显式给了
/// value** 的条目生效：`clause` 那一支的 value 由程序从子句抄（与 quote 同源），天然满足；
/// 空/全空白 value 仍归既有的 `MissingValue`。闸追加在全部既有校验之后 ⇒ 下面的
/// `NumberNotGrounded` 仍先出，错误优先级一个字不变。
#[test]
fn value_must_be_a_verbatim_span_of_quote() {
    let b = promise_batch();
    let c = sample_candidates();

    // 显式 value 把 quote 里的「你」改写成了「李四」：引文里没有这三个字。
    let mut paraphrased = promise_fact();
    paraphrased["value"] = json!("把对账单发给李四");
    assert_eq!(
        parse(&doc("promise", vec![paraphrased], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::ValueNotInQuote("f1".into()))
    );

    // 与既有数字闸相遇时仍由数字闸先出：新闸不抢码序。
    let mut both_bad = promise_fact();
    both_bad["value"] = json!("把对账单发给李四");
    both_bad["text"] = json!("张三承诺等 10 天后再把对账单发给李四。");
    assert_eq!(
        parse(&doc("promise", vec![both_bad], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::NumberNotGrounded("10".into())),
        "既有校验先出，新闸只兜最后一道"
    );

    // 弱模型档（§10.47）：value/quote 都由程序从子句抄，逐字关系天然成立 ⇒ 不许误杀。
    let mut by_clause = promise_fact();
    by_clause.as_object_mut().unwrap().remove("object");
    by_clause.as_object_mut().unwrap().remove("value");
    by_clause.as_object_mut().unwrap().remove("quote");
    by_clause["clause"] = json!("c2");
    let got =
        parse(&doc("promise", vec![by_clause], &["t1", "t3"]), &b, &c, FactFamily::Promise).unwrap();
    assert_eq!(
        got[0].value.as_deref(),
        Some("我下周三之前把对账单发给你"),
        "子句推导的 value 就是 quote 原文，照常通过"
    );

    // 空 value 不归这条：object 也缺席时仍是既有的 `MissingValue`。
    let mut empty_value = promise_fact();
    empty_value["object"] = json!(null);
    empty_value["value"] = json!("  ");
    assert_eq!(
        parse(&doc("promise", vec![empty_value], &["t1", "t3"]), &b, &c, FactFamily::Promise),
        Err(FactPassError::MissingValue)
    );

    // 宽松档把它当"丢一条"：原因进 dropped，其余草稿照常返回（不加特例）。
    let mut bad = promise_fact();
    bad["value"] = json!("把对账单发给李四");
    let raw = doc("promise", vec![bad, greeting_fact()], &["t2", "t3"]);
    let got = lenient(&raw, &b, &c, FactFamily::Promise).unwrap();
    assert_eq!(got.drafts.len(), 1);
    assert_eq!(got.drafts[0].reference, "f2");
    assert_eq!(got.dropped, vec![FactPassError::ValueNotInQuote("f1".into())]);
}
