//! S2c-1：event pass 的 prompt 形状、严格 parser 与独立审核（设计 `2026-10-26-s2-prompt-v0`
//! §2.3、§4、§7）。
//!
//! 这一层只断"模型给什么形状、程序怎么拒"：不选批、不调模型、不写库、不做 CLI。
//! event 与 fact 是两个 pass：自己的 canary、自己的 `pass` 值、自己的审核问题；
//! 正反例只用通用占位名（张三/李四/某项目/某打印机/某机房），不含任何语料专名或 holdout。
//!
//! 覆盖派发档 §5 的 16 条：三类合法事件、顶层坏形状、未知短引用、三处闭集、
//! actor/action、object 与 object_text 互斥、participants 三坏、quote 三坏、
//! 数字 grounding、evidence 三坏、覆盖/空批、prompt 形状、审核 prompt 的独立性，
//! 以及审核 parser 的七格。

use rc_distill::pass_event_contract::{
    parse_event_output, parse_event_output_lenient, parse_event_review, EventDraft, EventPassError,
    LenientEvents,
};
use rc_distill::pass_fact_contract::ReviewVerdict;
use rc_distill::pass_prompt::{
    event_review_shape, event_review_system, event_review_user, event_shape, event_system,
    event_user, FactCandidate, ReviewInputContext, EVENT_CANARY, EVENT_CATEGORIES,
    EVENT_REVIEW_CANARY, EVENT_REVIEW_CODES, EVENT_RESULTS, PASS_EVENT_REVIEW,
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
        session_key: "sess-event".into(),
        device_id: "dev-event".into(),
        source_agent: "agent-event".into(),
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

/// 审核输入的批级信息（C6a）：归属四格 + 与 `batch.rows` 逐位对应的说话人名。
fn review_ctx(b: &Batch) -> ReviewInputContext {
    ReviewInputContext {
        session_key: b.session_key.clone(),
        owner_agent_id: "owner-event".into(),
        visibility: "project".into(),
        scope: "project:sess-event".into(),
        speakers: b
            .rows
            .iter()
            .map(|row| {
                if row.role == "human" { "小明".to_string() } else { b.source_agent.clone() }
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

/// 四个候选：两个人、一台设备、一个地点。place 只能从这里取。
fn sample_candidates() -> Vec<FactCandidate> {
    vec![
        candidate(1, "person", "张三"),
        candidate(2, "person", "李四"),
        candidate(3, "object", "某打印机"),
        candidate(4, "place", "某机房"),
    ]
}

fn doc(events: Vec<Value>, empty_turns: &[&str]) -> String {
    json!({
        "schema_version": 0,
        "pass": "event",
        "events": events,
        "empty_turns": empty_turns,
    })
    .to_string()
}

/// 一条合法的状态变化：t1 是目标轮，t2/t3 是寒暄与过程步骤（进 empty_turns）。
fn state_change_batch() -> Batch {
    batch(&["昨晚我把配置改回去，服务就恢复了", "今天天气不错", "我看了一下日志"])
}

fn state_change() -> Value {
    json!({
        "ref": "e1",
        "turn": "t1",
        "category": "state_change",
        "actor": "a1",
        "trigger": "改回去",
        "action": "restore_config",
        "object": null,
        "object_text": "配置文件",
        "participants": ["a1"],
        "place": null,
        "result": "done",
        "mode": "stated",
        "time_text": "昨晚",
        "quote": "昨晚我把配置改回去，服务就恢复了",
        "evidence": ["t1"],
        "text": "张三昨晚把配置改回旧版本，服务恢复。"
    })
}

fn state_change_reply() -> String {
    doc(vec![state_change()], &["t2", "t3"])
}

fn parse(raw: &str, b: &Batch, c: &[FactCandidate]) -> Result<Vec<EventDraft>, EventPassError> {
    parse_event_output(raw, b, c)
}

/// 1. state_change 合法：短引用换成真实 uid / anchor_id，canonical 只给审核面。
#[test]
fn state_change_round_trips_short_refs_to_real_identity() {
    let b = state_change_batch();
    let c = sample_candidates();
    let got = parse(&state_change_reply(), &b, &c).unwrap();
    assert_eq!(got.len(), 1);
    let e = &got[0];
    assert_eq!(e.reference, "e1");
    assert_eq!(e.turn_uid, b.rows[0].turn_uid);
    assert_eq!(e.category, "state_change");
    assert_eq!(e.actor_anchor_id, anchor_id(1));
    assert_eq!(e.actor_canonical, "张三", "canonical 只用于审核渲染");
    assert_eq!(e.object_anchor_id, None);
    assert_eq!(e.object_text.as_deref(), Some("配置文件"));
    assert_eq!(e.participants_anchor_ids, vec![anchor_id(1)]);
    assert_eq!(e.participant_canonicals, vec!["张三".to_string()]);
    assert_eq!(e.place_anchor_id, None);
    assert_eq!(e.action, "restore_config");
    assert_eq!(e.result, "done");
    assert_eq!(e.mode, "stated");
    assert_eq!(e.time_text, "昨晚");
    assert_eq!(e.quote, "昨晚我把配置改回去，服务就恢复了");
    assert_eq!(e.evidence_turn_uids, vec![b.rows[0].turn_uid.clone()]);
    assert_eq!(e.text, "张三昨晚把配置改回旧版本，服务恢复。");
}

/// 2. event 与 observation 都合法：对象锚、多参与者、地点锚；数字在 quote 内成立。
#[test]
fn event_and_observation_are_legal() {
    let b = batch(&["刚才某打印机卡纸，李四拆开清理后恢复", "早上巡检发现备份目录还占着 12GB"]);
    let c = sample_candidates();
    let raw = doc(
        vec![
            json!({
                "ref": "e1",
                "turn": "t1",
                "category": "event",
                "actor": "a2",
                "trigger": "清理",
                "action": "clear_jam",
                "object": "a3",
                "object_text": null,
                "participants": ["a2", "a3"],
                "place": null,
                "result": "done",
                "mode": "stated",
                "time_text": "刚才",
                "quote": "刚才某打印机卡纸，李四拆开清理后恢复",
                "evidence": ["t1"],
                "text": "李四拆开清理卡纸的某打印机后恢复。"
            }),
            json!({
                "ref": "e2",
                "turn": "t2",
                "category": "observation",
                "actor": "a1",
                "trigger": "巡检",
                "action": "inspect_backup_dir",
                "object": null,
                "object_text": "备份目录",
                "participants": ["a1"],
                "place": "a4",
                "result": "ongoing",
                "mode": "stated",
                "time_text": "早上",
                "quote": "早上巡检发现备份目录还占着 12GB",
                "evidence": ["t2"],
                "text": "张三早上巡检时发现备份目录仍占用 12GB。"
            }),
        ],
        &[],
    );
    let got = parse(&raw, &b, &c).unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].category, "event");
    assert_eq!(got[0].object_anchor_id, Some(anchor_id(3)));
    assert_eq!(got[0].participants_anchor_ids, vec![anchor_id(2), anchor_id(3)]);
    assert_eq!(got[1].category, "observation");
    assert_eq!(got[1].place_anchor_id, Some(anchor_id(4)));
    assert_eq!(got[1].place_canonical.as_deref(), Some("某机房"));
    assert_eq!(got[1].result, "ongoing");
}

/// 3. 坏 JSON / 顶层多键 / 事件多键（confidence、id、occurred_at、cause）/ 类型错 /
///    version / pass / 缺键 ⇒ 各自一格。
#[test]
fn malformed_envelopes_are_rejected() {
    let b = state_change_batch();
    let c = sample_candidates();
    let good = state_change_reply();
    let mut value: Value = serde_json::from_str(&good).unwrap();

    assert_eq!(parse("这不是 json", &b, &c), Err(EventPassError::Json));
    assert_eq!(parse("[]", &b, &c), Err(EventPassError::Json));

    let mut top_extra = value.clone();
    top_extra["db_id"] = json!(7);
    assert_eq!(parse(&top_extra.to_string(), &b, &c), Err(EventPassError::Json));

    for key in ["confidence", "id", "occurred_at", "cause", "relation"] {
        let mut extra = value.clone();
        extra["events"][0][key] = json!("x");
        assert_eq!(parse(&extra.to_string(), &b, &c), Err(EventPassError::Json),
                   "事件级未知键 {key} 必须当场拒");
    }

    let mut wrong_type = value.clone();
    wrong_type["events"] = json!({"not": "a list"});
    assert_eq!(parse(&wrong_type.to_string(), &b, &c), Err(EventPassError::Json));

    let mut participants_type = value.clone();
    participants_type["events"][0]["participants"] = json!("a1");
    assert_eq!(parse(&participants_type.to_string(), &b, &c), Err(EventPassError::Json));

    let mut version = value.clone();
    version["schema_version"] = json!(1);
    assert_eq!(parse(&version.to_string(), &b, &c), Err(EventPassError::Version));

    let mut pass = value.clone();
    pass["pass"] = json!("fact");
    assert_eq!(parse(&pass.to_string(), &b, &c), Err(EventPassError::Pass));

    // 缺键同样归 Json（四键全必填）。
    value.as_object_mut().unwrap().remove("empty_turns");
    assert_eq!(parse(&value.to_string(), &b, &c), Err(EventPassError::Json));
}

/// 4. 批外短引用：turn / actor / object / place / participants 各自硬拒。
#[test]
fn short_refs_outside_the_batch_are_rejected() {
    let b = state_change_batch();
    let c = sample_candidates();

    let mut unknown_turn = state_change();
    unknown_turn["turn"] = json!("t9");
    unknown_turn["evidence"] = json!(["t9"]);
    assert_eq!(parse(&doc(vec![unknown_turn], &["t2", "t3"]), &b, &c),
               Err(EventPassError::UnknownTurn("t9".into())));

    let mut unknown_actor = state_change();
    unknown_actor["actor"] = json!("a9");
    unknown_actor["participants"] = json!(["a9"]);
    assert_eq!(parse(&doc(vec![unknown_actor], &["t2", "t3"]), &b, &c),
               Err(EventPassError::UnknownCandidate("a9".into())));

    let mut unknown_object = state_change();
    unknown_object["object"] = json!("a9");
    unknown_object["object_text"] = json!(null);
    assert_eq!(parse(&doc(vec![unknown_object], &["t2", "t3"]), &b, &c),
               Err(EventPassError::UnknownCandidate("a9".into())));

    let mut unknown_place = state_change();
    unknown_place["place"] = json!("a9");
    assert_eq!(parse(&doc(vec![unknown_place], &["t2", "t3"]), &b, &c),
               Err(EventPassError::UnknownCandidate("a9".into())));

    let mut unknown_participant = state_change();
    unknown_participant["participants"] = json!(["a1", "a9"]);
    assert_eq!(parse(&doc(vec![unknown_participant], &["t2", "t3"]), &b, &c),
               Err(EventPassError::UnknownCandidate("a9".into())));
}

/// 5. category / mode / result 三个闭集各自一格错误。
#[test]
fn closed_sets_for_category_mode_and_result_are_enforced() {
    let b = state_change_batch();
    let c = sample_candidates();

    let mut category = state_change();
    category["category"] = json!("promise");
    assert_eq!(parse(&doc(vec![category], &["t2", "t3"]), &b, &c),
               Err(EventPassError::Category));

    let mut mode = state_change();
    mode["mode"] = json!("inferred");
    assert_eq!(parse(&doc(vec![mode], &["t2", "t3"]), &b, &c), Err(EventPassError::Mode));

    let mut result = state_change();
    result["result"] = json!("大概好了");
    assert_eq!(parse(&doc(vec![result], &["t2", "t3"]), &b, &c),
               Err(EventPassError::BadResult));
}

/// 6. actor 缺失是闭集里的一格；action 空是另一格。
#[test]
fn actor_and_action_are_required() {
    let b = state_change_batch();
    let c = sample_candidates();

    for actor in ["", "   "] {
        let mut missing = state_change();
        missing["actor"] = json!(actor);
        assert_eq!(parse(&doc(vec![missing], &["t2", "t3"]), &b, &c),
                   Err(EventPassError::MissingActor));
    }

    for action in ["", "  "] {
        let mut missing = state_change();
        missing["action"] = json!(action);
        assert_eq!(parse(&doc(vec![missing], &["t2", "t3"]), &b, &c),
                   Err(EventPassError::MissingAction));
    }

    // 空 text 仍按形状问题归 Json（与 fact 同一条纪律）。`time_text` 自 S2e-4 起可空，
    // 它的四种写法在 `time_text_may_be_null_omitted_or_blank` 里单独钉。
    let mut no_text = state_change();
    no_text["text"] = json!("\r\n");
    assert_eq!(parse(&doc(vec![no_text], &["t2", "t3"]), &b, &c), Err(EventPassError::Json));
}

/// 6b. （S2e-4）`time_text` 可空：`null` / 省略 / 空白 ⇒ draft 空串；非空原样保留。
///
/// 与 fact 同一条读法：真实冒烟里 `"time_text": null` 被必填字符串拒掉整批。没有时间短语
/// 的 event 是正常行；空串进写库层后 `time_expr` 一条不落（writer 侧已有测试，不重复）。
#[test]
fn time_text_may_be_null_omitted_or_blank() {
    let b = state_change_batch();
    let c = sample_candidates();

    let mut null_time = state_change();
    null_time["time_text"] = json!(null);
    let got = parse(&doc(vec![null_time], &["t2", "t3"]), &b, &c).unwrap();
    assert_eq!(got[0].time_text, "", "null ⇒ 空串，不再整批拒收");

    let mut omitted = state_change();
    omitted.as_object_mut().unwrap().remove("time_text");
    let got = parse(&doc(vec![omitted], &["t2", "t3"]), &b, &c).unwrap();
    assert_eq!(got[0].time_text, "", "字段省略与 null 同一个读法");

    for blank in ["", "   "] {
        let mut empty_time = state_change();
        empty_time["time_text"] = json!(blank);
        let got = parse(&doc(vec![empty_time], &["t2", "t3"]), &b, &c).unwrap();
        assert_eq!(got[0].time_text, "", "空/全空白 ⇒ 空串");
    }

    let got = parse(&state_change_reply(), &b, &c).unwrap();
    assert_eq!(got[0].time_text, "昨晚", "非空短语原样保留");
}

/// 6c. （§7.5 切片一a）`time_text` 非空时必须是 `quote` 的逐字子串。
///
/// 与 fact 的 `value ⊂ quote` 同一把尺：时间短语是引文的一片，不是从邻句/邻轮补出来的。
/// 改前这条闸不存在，`state_change()` 这个"合法例"本身就带引文外的 `昨晚`——
/// 阴性对照（门被删掉时本用例必须红）见台账 §10.60。
#[test]
fn time_text_must_be_verbatim_inside_the_quote() {
    let b = state_change_batch();
    let c = sample_candidates();

    // 反例：时间只在同轮前一句，不在本条 quote 里（改前 GOOD 例的形状）。
    let mut borrowed = state_change();
    borrowed["quote"] = json!("我把配置改回去，服务就恢复了");
    borrowed["time_text"] = json!("昨晚");
    assert_eq!(
        parse(&doc(vec![borrowed], &["t2", "t3"]), &b, &c),
        Err(EventPassError::TimeNotInQuote("e1".into()))
    );

    // 反例：归一化后的日期不是原文子串。
    let mut normalized = state_change();
    normalized["time_text"] = json!("2026-10-28");
    assert_eq!(
        parse(&doc(vec![normalized], &["t2", "t3"]), &b, &c),
        Err(EventPassError::TimeNotInQuote("e1".into()))
    );

    // 反例：跨子句拼出来的短语不是任何一段连续原文。
    let mut spliced = state_change();
    spliced["time_text"] = json!("昨晚到现在");
    assert_eq!(
        parse(&doc(vec![spliced], &["t2", "t3"]), &b, &c),
        Err(EventPassError::TimeNotInQuote("e1".into()))
    );

    // 正例：时间在引文里就放行；空白 time_text 归上面那格（空串），不归这条。
    let mut inside = state_change();
    inside["time_text"] = json!(" 昨晚 ");
    assert_eq!(parse(&doc(vec![inside], &["t2", "t3"]), &b, &c).unwrap().len(), 1);
}

/// 6d. （§7.5 切片一b）`trigger` 必填、非空、且必须是 `quote` 的逐字子串。
#[test]
fn trigger_must_be_verbatim_inside_the_quote() {
    let b = state_change_batch();
    let c = sample_candidates();

    // 缺键 ⇒ 形状错（必填字段）。
    let mut missing = state_change();
    missing.as_object_mut().unwrap().remove("trigger");
    assert_eq!(parse(&doc(vec![missing], &["t2", "t3"]), &b, &c), Err(EventPassError::Json));

    // 空串/全空白 ⇒ 同一格（与空 ref/text 同一条读法）。
    for blank in ["", "   "] {
        let mut empty = state_change();
        empty["trigger"] = json!(blank);
        assert_eq!(parse(&doc(vec![empty], &["t2", "t3"]), &b, &c), Err(EventPassError::Json));
    }

    // 反例：写的动词不在引文里（同义改写）⇒ 拒；载荷是 ref，不回显模型短语。
    let mut rewritten = state_change();
    rewritten["trigger"] = json!("把配置改回旧版本");
    assert_eq!(
        parse(&doc(vec![rewritten], &["t2", "t3"]), &b, &c),
        Err(EventPassError::TriggerNotInQuote("e1".into()))
    );

    // 正例：引文里的动词短语原样抄下来；首尾空白去干净后落进 draft。
    let mut verbatim = state_change();
    verbatim["trigger"] = json!(" 改回去 ");
    let got = parse(&doc(vec![verbatim], &["t2", "t3"]), &b, &c).unwrap();
    assert_eq!(got[0].trigger, "改回去");
}

/// 7. object 与 object_text 同时给 ⇒ `ObjectBoth`；只给其一合法。
///
/// §7.5 切片一b 改了"都给 null"这一支：`state_change` 必须点名客体，两种都空 ⇒
/// `StateChangeWithoutObject`。同一形状在 `event`/`observation` 上仍然合法（见下）。
#[test]
fn object_and_object_text_are_mutually_exclusive() {
    let b = state_change_batch();
    let c = sample_candidates();

    let mut both = state_change();
    both["object"] = json!("a3");
    both["object_text"] = json!("配置文件");
    assert_eq!(parse(&doc(vec![both], &["t2", "t3"]), &b, &c), Err(EventPassError::ObjectBoth));

    let mut neither = state_change();
    neither["object_text"] = json!(null);
    assert_eq!(
        parse(&doc(vec![neither.clone()], &["t2", "t3"]), &b, &c),
        Err(EventPassError::StateChangeWithoutObject)
    );

    // 同一对空客体在 `event` 上合法：这条闸只针对 state_change。
    let mut as_event = neither.clone();
    as_event["category"] = json!("event");
    assert_eq!(parse(&doc(vec![as_event], &["t2", "t3"]), &b, &c).unwrap().len(), 1);

    // 空串的 object_text 等于没给：不许和 object 一起触发 ObjectBoth。
    let mut blank_text = neither.clone();
    blank_text["object"] = json!("a3");
    blank_text["object_text"] = json!("   ");
    let got = parse(&doc(vec![blank_text], &["t2", "t3"]), &b, &c).unwrap();
    assert_eq!(got[0].object_anchor_id, Some(anchor_id(3)));
    assert_eq!(got[0].object_text, None);
}

/// 8. participants：空 / 缺 actor / 含非候选，三种都硬拒。
#[test]
fn participants_must_be_non_empty_candidates_and_include_actor() {
    let b = state_change_batch();
    let c = sample_candidates();

    let mut empty = state_change();
    empty["participants"] = json!([]);
    assert_eq!(parse(&doc(vec![empty], &["t2", "t3"]), &b, &c),
               Err(EventPassError::MissingActor));

    let mut without_actor = state_change();
    without_actor["participants"] = json!(["a2"]);
    assert_eq!(parse(&doc(vec![without_actor], &["t2", "t3"]), &b, &c),
               Err(EventPassError::MissingActor));

    let mut non_candidate = state_change();
    non_candidate["participants"] = json!(["a1", "c2"]);
    assert_eq!(parse(&doc(vec![non_candidate], &["t2", "t3"]), &b, &c),
               Err(EventPassError::UnknownCandidate("c2".into())));
}

/// 9. quote：不逐字 / 超 120 字 / 含省略号（中文、ASCII 两种）。
#[test]
fn quote_must_be_verbatim_short_and_without_ellipsis() {
    let b = state_change_batch();
    let c = sample_candidates();

    let mut not_verbatim = state_change();
    not_verbatim["quote"] = json!("我把配置改回去，服务就好了");
    assert_eq!(parse(&doc(vec![not_verbatim], &["t2", "t3"]), &b, &c),
               Err(EventPassError::QuoteNotVerbatim("e1".into())));

    let mut from_other_turn = state_change();
    from_other_turn["quote"] = json!("我看了一下日志");
    assert_eq!(parse(&doc(vec![from_other_turn], &["t2", "t3"]), &b, &c),
               Err(EventPassError::QuoteNotVerbatim("e1".into())),
               "quote 只能抄本条 turn");

    let mut too_long = state_change();
    too_long["quote"] = json!("改".repeat(121));
    assert_eq!(parse(&doc(vec![too_long], &["t2", "t3"]), &b, &c),
               Err(EventPassError::QuoteTooLong));

    for quote in ["我把配置…恢复了", "我把配置...恢复了"] {
        let mut ellipsis = state_change();
        ellipsis["quote"] = json!(quote);
        assert_eq!(parse(&doc(vec![ellipsis], &["t2", "t3"]), &b, &c),
                   Err(EventPassError::Ellipsis));
    }
}

/// 9b. 行内标记不算"引文不逐字"：模型抄引文时系统性剥 `**`/反引号（10-29 审计 4/4）。
#[test]
fn inline_marks_are_normalized_before_the_verbatim_check() {
    let b = batch(&["昨晚我把**配置**改回去，`服务`就恢复了", "今天天气不错", "我看了一下日志"]);
    let c = sample_candidates();

    let mut stripped = state_change();
    stripped["quote"] = json!("昨晚我把配置改回去，服务就恢复了");
    assert!(parse(&doc(vec![stripped], &["t2", "t3"]), &b, &c).is_ok(),
            "剥掉 `**`/反引号后的引文仍是同一段原文");

    // 阴性对照：改掉正文字，仍然拒——归一只抹标记，不放过改写。
    let mut paraphrase = state_change();
    paraphrase["quote"] = json!("昨晚我把配置改回旧版本，服务就恢复了");
    assert_eq!(parse(&doc(vec![paraphrase], &["t2", "t3"]), &b, &c),
               Err(EventPassError::QuoteNotVerbatim("e1".into())));
}

/// 10. text 的数字必须逐一出现在 quote 里。
#[test]
fn numbers_in_text_must_be_grounded_in_quote() {    let b = state_change_batch();
    let c = sample_candidates();

    let mut invented = state_change();
    invented["text"] = json!("张三在 3 分钟内把配置改回旧版本，服务恢复。");
    assert_eq!(parse(&doc(vec![invented], &["t2", "t3"]), &b, &c),
               Err(EventPassError::NumberNotGrounded("3".into())));

    // 正例：quote 里是 ISO 日期，text 里的 2026/10/26 各自都能找到。
    let b2 = batch(&["2026-10-26 晚上把配置改回去了，服务恢复"]);
    let raw = doc(
        vec![json!({
            "ref": "e1",
            "turn": "t1",
            "category": "state_change",
            "actor": "a1",
            "trigger": "改回去",
            "action": "restore_config",
            "object": null,
            "object_text": "配置文件",
            "participants": ["a1"],
            "place": null,
            "result": "done",
            "mode": "stated",
            "time_text": "2026-10-26 晚上",
            "quote": "2026-10-26 晚上把配置改回去了，服务恢复",
            "evidence": ["t1"],
            "text": "张三在 2026-10-26 晚上把配置改回去，服务恢复。"
        })],
        &[],
    );
    assert_eq!(parse(&raw, &b2, &c).unwrap().len(), 1);
}

/// 11. evidence：为空 / 含非目标引用 / 不含本条 turn。
#[test]
fn evidence_must_be_non_empty_target_only_and_include_turn() {
    let b = state_change_batch();
    let c = sample_candidates();

    let mut empty = state_change();
    empty["evidence"] = json!([]);
    assert_eq!(parse(&doc(vec![empty], &["t2", "t3"]), &b, &c),
               Err(EventPassError::EmptyEvidence));

    let mut outside = state_change();
    outside["evidence"] = json!(["t1", "t9"]);
    assert_eq!(parse(&doc(vec![outside], &["t2", "t3"]), &b, &c),
               Err(EventPassError::EvidenceNotTarget("t9".into())));

    let b2 = batch(&["今天天气不错", "我把配置改回去，服务就恢复了"]);
    let mut missing_turn = state_change();
    missing_turn["turn"] = json!("t2");
    // 这条轮没有「昨晚」：引文与时间一起收窄到该轮真正有的那段，否则先撞 quote 闸。
    missing_turn["quote"] = json!("我把配置改回去，服务就恢复了");
    missing_turn["time_text"] = json!(null);
    missing_turn["evidence"] = json!(["t1"]);
    assert_eq!(parse(&doc(vec![missing_turn], &[]), &b2, &c),
               Err(EventPassError::EvidenceNotTarget("t2".into())));

    // 锚引用不能进 evidence（a1 只属于 actor/participants）。
    let mut anchor_as_evidence = state_change();
    anchor_as_evidence["evidence"] = json!(["a1"]);
    assert_eq!(parse(&doc(vec![anchor_as_evidence], &["t2", "t3"]), &b, &c),
               Err(EventPassError::EvidenceNotTarget("a1".into())));

    // c* 上下文轮不可引用。
    let mut context_ref = state_change();
    context_ref["evidence"] = json!(["t1", "c1"]);
    assert_eq!(parse(&doc(vec![context_ref], &["t2", "t3"]), &b, &c),
               Err(EventPassError::EvidenceNotTarget("c1".into())));
}

/// 12. 重复 ref；未覆盖目标按 t 序号列出全部缺失 id。
#[test]
fn duplicate_refs_and_uncovered_turns_are_named_exactly() {
    let b = state_change_batch();
    let c = sample_candidates();

    let second = json!({
        "ref": "e1",
        "turn": "t2",
        "category": "observation",
        "actor": "a1",
        "trigger": "不错",
        "action": "notice_weather",
        "object": null,
        "object_text": null,
        "participants": ["a1"],
        "place": null,
        "result": "done",
        "mode": "stated",
        "time_text": "今天",
        "quote": "今天天气不错",
        "evidence": ["t2"],
        "text": "张三注意到今天天气不错。"
    });
    assert_eq!(parse(&doc(vec![state_change(), second], &["t3"]), &b, &c),
               Err(EventPassError::DuplicateRef("e1".into())));

    // t1 有事件，t2 进 empty_turns，t3 谁也没提 ⇒ 只缺 t3。
    assert_eq!(parse(&doc(vec![state_change()], &["t2"]), &b, &c),
               Err(EventPassError::Uncovered(vec!["t3".into()])));

    // 一条都不给 ⇒ 三个目标全部缺失，顺序是 t1、t2、t3。
    assert_eq!(parse(&doc(vec![], &[]), &b, &c),
               Err(EventPassError::Uncovered(vec!["t1".into(), "t2".into(), "t3".into()])));
}

/// 13. 空批合法只在每个目标都有归属时成立；empty_turns 本身也不许重复/批外引用。
#[test]
fn an_empty_batch_is_legal_only_when_every_target_is_covered() {
    let b = batch(&["今天天气不错", "我看了一下日志"]);
    let c: Vec<FactCandidate> = vec![];

    assert_eq!(parse(&doc(vec![], &["t1", "t2"]), &b, &c), Ok(vec![]));
    assert_eq!(parse(&doc(vec![], &["t1"]), &b, &c),
               Err(EventPassError::Uncovered(vec!["t2".into()])));
    assert_eq!(parse(&doc(vec![], &["t1", "t1"]), &b, &c),
               Err(EventPassError::DuplicateRef("t1".into())));
    assert_eq!(parse(&doc(vec![], &["t9"]), &b, &c),
               Err(EventPassError::UnknownTurn("t9".into())));
    assert_eq!(parse(&doc(vec![], &["c1"]), &b, &c),
               Err(EventPassError::UnknownTurn("c1".into())));
}

/// 14. event prompt：canary 一次、单任务、result 闭集逐字、候选只见 a*、无身份/时钟。
#[test]
fn event_prompt_shape_hides_identity_and_pins_the_single_task() {
    let shape = event_shape();
    assert_eq!((shape.method_steps, shape.standards, shape.positives, shape.negatives), (4, 6, 3, 10),
               "方法四步、标准六条（加 ACTOR 点名判据）、正例 3（结果+宣布/诊断+行动/失败，替换两条生活场景正例）、反例 10（删寒暄、假设、无结果过程三条，加宣布 2、诊断 1、指令 1、提问 1、系统产物 1、读数 1、actor 1）");

    let sys = event_system();
    assert_eq!(count(&sys, EVENT_CANARY), 1, "canary 只在 system 首行出现一次");
    assert!(sys.starts_with(EVENT_CANARY), "canary 必须在首行");
    assert!(sys.contains("\"pass\":\"event\""));
    assert!(sys.contains("\"schema_version\":0"));
    // 唯一任务：已发生或已开始 + 值得记；结果只是属性。不做计划/承诺/情绪/关系/因果。
    for needle in ["already happened", "already started", "plan", "promise", "mood", "relation", "cause"] {
        assert!(sys.contains(needle), "system 缺单任务表述：{needle}");
    }
    // 三值 category 与五值 result 逐字在场。
    for category in EVENT_CATEGORIES {
        assert!(sys.contains(category), "缺 category 闭集值：{category}");
    }
    for result in EVENT_RESULTS {
        assert!(sys.contains(result), "缺 result 闭集值：{result}");
    }
    // 新示例逐条在场：结果+宣布、宣布独占、诊断、指令、提问、系统产物、读数、actor。
    for needle in [
        "我把配置改回去，服务就恢复了",
        "实施者把迁移脚本跑完，两条校验都过了",
        "日志里报的是连接池耗尽",
        "上一轮的改动没生效，我回滚了",
        "接着我把开关切到新路径，然后观察两天",
        "你先看一下那三个会话，再决定要不要改",
        "最近还有别的吗？",
        "[Tool call cancelled by host]",
        "工作区现在是干净的，HEAD 在 cafebabe",
        "实施者交回了回滚脚本",
    ] {
        assert!(sys.contains(needle), "模板缺样例：{needle}");
    }
    // 三处冲突裁定 + actor 塌陷的判据必须在正文里（不靠示例反推）。
    for needle in [
        "are ONE result row",
        "this rule constrains different actions only",
        "result=ongoing",
        "not whether it ended",
        "ACTOR IS WHO THE QUOTE NAMES",
        "use the turn speaker",
    ] {
        assert!(sys.contains(needle), "缺裁定/actor 判据：{needle}");
    }
    assert!(sys.contains("Do not write any id"));
    assert!(sys.contains("*_ms"));
    assert!(sys.contains("confidence"));
    // v0.2 的两条语义边界必须**在文本里**：数量对不等于内容对。
    // ① 完成汇报算事件；② 同一轮"先报告结果、再宣布下一步"只取结果、丢掉宣布。
    // 缺了它们，"宣布开始"会被当成事件、"完成 + 接下来"会被整轮判空（10-28 重跑实测的两处缺口）。
    assert!(sys.contains("A completion report"), "缺「完成汇报算事件」那一条：{sys}");
    // §7.5 切片一b：category 必须是 trigger 的类别，trigger 必须逐字在 quote 里；
    // 三条互斥判据 + 一条优先级缺一不可。
    for needle in [
        "CATEGORY IS THE TRIGGER'S CLASS",
        "must be a substring of quote",
        "PRECEDENCE",
        "never write the perceived content as a second row",
        "object or object_text must name the thing whose state changed",
    ] {
        assert!(sys.contains(needle), "缺 category/trigger 判据：{needle}");
    }
    // P-fix：单数 "keep the result" 会误伤"先做 A 再做 B"这类已完成叙述，改成复数。
    assert!(sys.contains("keep every reported result and drop the announcement"),
            "缺「同一轮保留全部已报告结果」那一条：{sys}");
    // 交叉缺席：event 模板不借任何别的作业的 canary。
    for old in [
        rc_distill::prompt::EXTRACT_CANARY,
        rc_distill::prompt::JUDGE_CANARY,
        rc_distill::prompt::AUDIT_CANARY,
        rc_distill::prompt::ANCHOR_CANARY,
        rc_distill::pass_prompt::FACT_CANARY,
        EVENT_REVIEW_CANARY,
    ] {
        assert!(!sys.contains(old), "event 模板借了别人的 canary");
    }
    assert!(!has_hex64(&sys));

    let b = batch(&["好的，收到", "我把配置改回去，服务就恢复了"]);
    let c = sample_candidates();
    // C6b：这一条断的是 TARGET/候选段的字面量，邻轮不在本用例的范围内 ⇒ 空 CTX。
    let user = event_user(&b, &c, &ExtractCtx::default());
    assert!(user.lines().next().unwrap().contains("event"), "user 首行点明 pass");
    assert!(user.contains("TARGET [t1] 2026-10-26T14:05+08:00 human: 好的，收到"));
    assert!(user.contains("TARGET [t2] 2026-10-26T14:06+08:00 agent: 我把配置改回去，服务就恢复了"));
    assert!(!user.contains("_ms"), "user 不渲染毫秒字段名");
    assert!(!user.contains("anchor_id"), "user 不渲染 anchor_id 这个词");
    assert!(!user.contains(&uid(0)) && !user.contains(&uid(1)), "user 不渲染 64 位 turn_uid");
    assert!(!has_hex64(&user), "user 不含任何 64 位 hex 串");
    for cand in &c {
        assert!(user.contains(&format!("- {} | {} | {}", cand.reference, cand.kind, cand.canonical)));
        assert!(!user.contains(&cand.anchor_id), "候选锚 id 一个字都不许进 prompt");
    }
    assert!(user.contains("CANDIDATES"));

    // 空候选表明说 (none)，不让模型在空表上猜。
    let empty_user = event_user(&b, &[], &ExtractCtx::default());
    assert!(empty_user.contains("(none in this batch)"));
}

/// 15. 审核 prompt 是**另一次**调用：自己的 canary、自己的 pass 值、只问原子性/结果/参与者。
#[test]
fn review_prompt_is_a_separate_call_with_its_own_canary() {
    assert_ne!(EVENT_REVIEW_CANARY, EVENT_CANARY);
    let shape = event_review_shape();
    assert_eq!((shape.method_steps, shape.standards, shape.positives, shape.negatives), (5, 6, 1, 5),
               "审核：方法五步（第 5 步是 G1 刀 B 的价值轴）、标准六条（举证责任是第 5 条、末条是价值轴；空邻居池那条已移出，本 pass 无矛盾轴）、正例 1、反例 5（自述无结果 + 价值轴）");

    let sys = event_review_system();
    assert_eq!(count(&sys, EVENT_REVIEW_CANARY), 1, "审核 canary 只在 system 首行一次");
    assert!(sys.starts_with(EVENT_REVIEW_CANARY), "canary 必须在首行");
    assert!(sys.contains(&format!("\"pass\":\"{}\"", PASS_EVENT_REVIEW)));
    assert!(sys.contains("\"schema_version\":0"));
    // 唯一问题：一个原子动作 + 已发生/已开始 + 参与者有引文支持；结果可为 unknown。
    for needle in ["one atomic action", "already happened", "already started", "participants", "quote"] {
        assert!(sys.contains(needle), "审核 system 缺问题要素：{needle}");
    }
    // 三种判决都在契约文本里；JSON 样例只给 pass/reject——可判的失败一律 reject，
    // uncertain 只留给证据本身判不了的输入（H6-fix 的示例口径）。
    for word in ["pass", "reject", "uncertain"] {
        assert!(sys.contains(word), "契约缺 verdict：{word}");
    }
    // 台账 §10.26 缺陷 4：uncertain 可以不带码；自述可以指认 actor，但不能替代结果证据。
    assert!(sys.contains("uncertain may carry null"), "契约没说 uncertain 可空码");
    assert!(sys.contains("SELF-REPORT NEEDS A CHECKABLE RESULT"), "缺举证责任标准");
    for code in EVENT_REVIEW_CODES {
        assert!(sys.contains(code), "缺 code：{code}");
    }
    assert!(sys.contains("Do not add"));
    assert!(sys.contains("identity") || sys.contains("identity field"));
    assert!(!sys.contains(EVENT_CANARY), "审核模板不借抽取的 canary");
    assert!(!sys.contains(rc_distill::pass_prompt::FACT_REVIEW_CANARY));
    // 反向：抽取模板里不许出现审核 canary。
    assert!(!event_system().contains(EVENT_REVIEW_CANARY));
}

/// 16. 审核 user 只渲染该行 + 引文 + 目标轮：无 uid / 毫秒 / anchor_id / 生成理由，
///     也不给抽取侧写下的整句（那是抽取的结论，不是证据）。
#[test]
fn review_prompt_shows_only_the_row_quote_and_target_turn() {
    let b = batch(&["今天天气不错", "我把配置改回去，服务就恢复了，顺手也清了缓存。"]);
    let draft = EventDraft {
        reference: "e1".into(),
        turn_uid: b.rows[1].turn_uid.clone(),
        category: "state_change".into(),
        actor_anchor_id: anchor_id(1),
        actor_canonical: "张三".into(),
        object_anchor_id: None,
        object_text: Some("配置文件".into()),
        object_canonical: None,
        participants_anchor_ids: vec![anchor_id(1)],
        participant_canonicals: vec!["张三".into()],
        place_anchor_id: None,
        place_canonical: None,
        trigger: "改回去".into(),
        action: "restore_config".into(),
        result: "done".into(),
        mode: "stated".into(),
        time_text: "昨晚".into(),
        quote: "我把配置改回去，服务就恢复了".into(),
        evidence_turn_uids: vec![b.rows[1].turn_uid.clone()],
        text: "张三昨晚把配置改回旧版本，服务恢复。".into(),
    };
    let user = event_review_user(&[draft], &b, &review_ctx(&b));

    assert!(user.lines().next().unwrap().contains(PASS_EVENT_REVIEW), "首行点明这是审核调用");
    assert!(user.contains("ITEM e1"));
    assert!(user.contains("actor: 张三"), "actor 给 canonical，不给 anchor_id");
    assert!(user.contains("action: restore_config"));
    assert!(user.contains("object_text: 配置文件"));
    assert!(user.contains("participants: 张三"));
    assert!(user.contains("category: state_change"));
    assert!(user.contains("result: done"));
    assert!(user.contains("quote: 我把配置改回去，服务就恢复了"));
    assert!(user.contains("我把配置改回去，服务就恢复了，顺手也清了缓存。"),
            "目标轮原文完整给出（引文只是它的子串）");
    // C6a：邻轮只进 `CTX (not evidence)` 段，不进 TARGET 段。
    let ctx_at = user.find("CTX (not evidence):").expect("有邻轮时必须出现 CTX 段");
    assert!(!user[..ctx_at].contains("今天天气不错"), "邻轮不进 TARGET 段");
    assert!(user[ctx_at..].contains("今天天气不错"), "前一轮在 CTX 段");
    assert!(!user.contains(&uid(0)) && !user.contains(&uid(1)), "user 不渲染 turn_uid");
    assert!(!has_hex64(&user), "user 不含 64 位 hex");
    assert!(!user.contains(&anchor_id(1)), "user 不渲染 anchor_id");
    assert!(!user.contains("anchor_id"), "连字段名都不出现");
    assert!(!user.contains("_ms"), "user 不渲染毫秒字段");
    assert!(!user.contains("confidence"));
    assert!(!user.contains("reason"));
    // 抽取侧写下的整句不进审核。
    assert!(!user.contains("张三昨晚把配置改回旧版本，服务恢复。"));
    assert!(!user.contains("ITEM e2"));
}

// ── 审核 parser（§4 的第二次调用）───────────────────────────────────────────────

fn review_reply(verdicts: Vec<Value>) -> String {
    json!({
        "schema_version": 0,
        "pass": "event_review",
        "verdicts": verdicts,
    })
    .to_string()
}

fn review(refs: &[&str], raw: &str)
    -> Result<Vec<(String, ReviewVerdict, Option<String>)>, EventPassError> {
    let refs: Vec<String> = refs.iter().map(|r| (*r).to_string()).collect();
    parse_event_review(raw, &refs)
}

/// 17. 三分支合法：pass/pass、reject/not_atomic、uncertain/result_unsupported。
#[test]
fn review_parser_accepts_the_three_verdict_branches() {
    let raw = review_reply(vec![
        json!({"ref": "e1", "verdict": "pass", "code": null}),
        json!({"ref": "e2", "verdict": "reject", "code": "not_atomic"}),
        json!({"ref": "e3", "verdict": "uncertain", "code": "result_unsupported"}),
    ]);
    assert_eq!(review(&["e1", "e2", "e3"], &raw).unwrap(), vec![
        ("e1".to_string(), ReviewVerdict::Pass, None),
        ("e2".to_string(), ReviewVerdict::Reject, Some("not_atomic".to_string())),
        ("e3".to_string(), ReviewVerdict::Uncertain, Some("result_unsupported".to_string())),
    ]);
}

/// 18. 每个 ref 恰好一次：缺 / 多 / 重复 / 空 ref；返回顺序跟 refs 走。
#[test]
fn review_parser_requires_each_ref_exactly_once() {
    let missing = review_reply(vec![json!({"ref": "e1", "verdict": "pass", "code": null})]);
    assert_eq!(review(&["e1", "e2", "e3"], &missing),
               Err(EventPassError::Uncovered(vec!["e2".into(), "e3".into()])));

    let none = review_reply(vec![]);
    assert_eq!(review(&["e1"], &none), Err(EventPassError::Uncovered(vec!["e1".into()])));

    let extra = review_reply(vec![
        json!({"ref": "e1", "verdict": "pass", "code": null}),
        json!({"ref": "e9", "verdict": "pass", "code": null}),
    ]);
    assert_eq!(review(&["e1"], &extra), Err(EventPassError::UnknownTurn("e9".into())));

    let duplicate = review_reply(vec![
        json!({"ref": "e1", "verdict": "pass", "code": null}),
        json!({"ref": "e1", "verdict": "reject", "code": "other"}),
    ]);
    assert_eq!(review(&["e1"], &duplicate), Err(EventPassError::DuplicateRef("e1".into())));

    let blank = review_reply(vec![json!({"ref": "", "verdict": "pass", "code": null})]);
    assert_eq!(review(&["e1"], &blank), Err(EventPassError::Json));

    // 没有待审行 ⇒ 空判决合法；顺序跟 refs 走。
    assert_eq!(review(&[], &review_reply(vec![])), Ok(vec![]));
    let reversed = review_reply(vec![
        json!({"ref": "e2", "verdict": "reject", "code": "missing_actor"}),
        json!({"ref": "e1", "verdict": "reject", "code": "quote_mismatch"}),
    ]);
    let got = review(&["e1", "e2"], &reversed).unwrap();
    assert_eq!((got[0].0.as_str(), got[1].0.as_str()), ("e1", "e2"));
    assert_eq!(got[0].2.as_deref(), Some("quote_mismatch"));
    assert_eq!(got[1].2.as_deref(), Some("missing_actor"));
}

/// 19. verdict / code 闭集：pass 必须 code=null；reject/uncertain 必须带闭集内的 code。
#[test]
fn review_parser_closes_verdict_and_code_sets() {
    let bad_verdict = review_reply(vec![json!({"ref": "e1", "verdict": "maybe", "code": null})]);
    assert_eq!(review(&["e1"], &bad_verdict), Err(EventPassError::Json));

    let pass_with_code = review_reply(vec![
        json!({"ref": "e1", "verdict": "pass", "code": "other"}),
    ]);
    assert_eq!(review(&["e1"], &pass_with_code), Err(EventPassError::Json));

    let reject_without_code = review_reply(vec![
        json!({"ref": "e1", "verdict": "reject", "code": null}),
    ]);
    assert_eq!(review(&["e1"], &reject_without_code), Err(EventPassError::Json));

    let unknown_code = review_reply(vec![
        json!({"ref": "e1", "verdict": "reject", "code": "not_durable"}),
    ]);
    assert_eq!(review(&["e1"], &unknown_code), Err(EventPassError::Json),
               "fact 的 code 不算 event 的 code");

    // 台账 §10.26 缺陷 4：`uncertain` 允许不带码——"判不了"不该被规则逼进 pass 或 reject。
    let uncertain_no_code = review_reply(vec![
        json!({"ref": "e1", "verdict": "uncertain", "code": null}),
    ]);
    assert_eq!(
        review(&["e1"], &uncertain_no_code),
        Ok(vec![("e1".to_string(), ReviewVerdict::Uncertain, None)]),
        "uncertain 带 null 码必须被收下"
    );
    // 带上闭集里的码同样合法：模型能说清"为什么拿不准"。
    let uncertain_with_code = review_reply(vec![
        json!({"ref": "e1", "verdict": "uncertain", "code": "result_unsupported"}),
    ]);
    assert_eq!(
        review(&["e1"], &uncertain_with_code),
        Ok(vec![("e1".to_string(), ReviewVerdict::Uncertain, Some("result_unsupported".into()))])
    );

    // 闭集五值都收。
    for code in EVENT_REVIEW_CODES {
        let raw = review_reply(vec![json!({"ref": "e1", "verdict": "reject", "code": code})]);
        assert_eq!(
            review(&["e1"], &raw),
            Ok(vec![("e1".to_string(), ReviewVerdict::Reject, Some(code.to_string()))]),
            "code {code} 应被接受"
        );
    }
}

/// 20. 信封：多键/少键/类型错 ⇒ Json；version/pass 各自一格；event 的 pass 值不算审核的。
#[test]
fn review_parser_rejects_wrong_envelope_and_unknown_keys() {
    let ok = review_reply(vec![json!({"ref": "e1", "verdict": "pass", "code": null})]);
    let mut doc: Value = serde_json::from_str(&ok).unwrap();

    assert_eq!(review(&["e1"], "这不是 json"), Err(EventPassError::Json));
    assert_eq!(review(&["e1"], "[]"), Err(EventPassError::Json));

    let mut extra_top = doc.clone();
    extra_top["reason"] = json!("模型解释");
    assert_eq!(review(&["e1"], &extra_top.to_string()), Err(EventPassError::Json));

    let mut extra_item = doc.clone();
    extra_item["verdicts"][0]["confidence"] = json!(0.9);
    assert_eq!(review(&["e1"], &extra_item.to_string()), Err(EventPassError::Json));

    let mut missing_code = doc.clone();
    missing_code["verdicts"][0].as_object_mut().unwrap().remove("code");
    assert_eq!(review(&["e1"], &missing_code.to_string()), Err(EventPassError::Json));

    let mut version = doc.clone();
    version["schema_version"] = json!(1);
    assert_eq!(review(&["e1"], &version.to_string()), Err(EventPassError::Version));

    let mut pass = doc.clone();
    pass["pass"] = json!("event");
    assert_eq!(review(&["e1"], &pass.to_string()), Err(EventPassError::Pass));

    // 审核信封没有 family 键：多一个就拒。
    let mut family = doc.clone();
    family["family"] = json!("event");
    assert_eq!(review(&["e1"], &family.to_string()), Err(EventPassError::Json));

    doc.as_object_mut().unwrap().remove("verdicts");
    assert_eq!(review(&["e1"], &doc.to_string()), Err(EventPassError::Json));
}

fn count(hay: &str, needle: &str) -> usize {
    hay.match_indices(needle).count()
}

fn has_hex64(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    chars.windows(64).any(|window| window.iter().all(|c| c.is_ascii_hexdigit()))
}

/// 19 格的 slug 表——这张表就是闭集本身：新增变体不在这里补一行，穷尽 match 会先翻脸，
/// `seen.len()` 会再翻一次。
fn slug_cases() -> Vec<(EventPassError, &'static str)> {
    vec![
        (EventPassError::Json, "json"),
        (EventPassError::Version, "version"),
        (EventPassError::Pass, "pass"),
        (EventPassError::UnknownTurn("t9".into()), "unknown_turn"),
        (EventPassError::UnknownCandidate("a9".into()), "unknown_candidate"),
        (EventPassError::Category, "category"),
        (EventPassError::Mode, "mode"),
        (EventPassError::MissingActor, "missing_actor"),
        (EventPassError::MissingAction, "missing_action"),
        (EventPassError::ObjectBoth, "object_both"),
        (EventPassError::BadResult, "bad_result"),
        (EventPassError::EmptyEvidence, "empty_evidence"),
        (EventPassError::EvidenceNotTarget("t9".into()), "evidence_not_target"),
        (EventPassError::DuplicateRef("e1".into()), "duplicate_ref"),
        (EventPassError::QuoteNotVerbatim("e1".into()), "quote_not_verbatim"),
        (EventPassError::QuoteTooLong, "quote_too_long"),
        (EventPassError::Ellipsis, "ellipsis"),
        (EventPassError::NumberNotGrounded("12".into()), "number_not_grounded"),
        (EventPassError::Uncovered(vec!["t2".into()]), "uncovered"),
    ]
}

/// 21. 每一格一个稳定 slug：只有小写 ASCII 与下划线、无空格、≤32 字符、彼此不重样。
///     event 的 19 格没有审核专属格，审核失败靠 `review_reason()` 的 `review_` 前缀区分。
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

        // 审核失败落库用的词：`review_` + 同一格，且只补一次前缀。
        let reason = error.review_reason();
        assert!(!reason.contains("review_review_"), "{reason}");
        assert_eq!(reason, format!("review_{slug}"), "{error:?}");
        assert!(reason.chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{reason}");
        assert!(reason.len() <= 32, "{reason} 太长");
    }
    assert_eq!(seen.len(), 19, "闭集大小就是这样表：新增变体必须在这里补一行");
}

/// 22. 审核侧沿用同一张表：坏 JSON ⇒ `review_json`，坏引用 ⇒ `review_unknown_turn`，
///     缺判决 ⇒ `review_uncovered`。抽取侧不带前缀。
#[test]
fn review_reasons_carry_the_phase_prefix_on_the_same_cells() {
    assert_eq!(EventPassError::Json.slug(), "json");
    assert_eq!(EventPassError::Json.review_reason(), "review_json");
    assert_eq!(EventPassError::UnknownTurn("e9".into()).review_reason(), "review_unknown_turn");
    assert_eq!(EventPassError::Uncovered(vec!["e1".into()]).review_reason(), "review_uncovered");
    assert_eq!(
        EventPassError::DuplicateRef("e1".into()).review_reason(),
        "review_duplicate_ref"
    );
}

// ── S2e-2：整段 fenced JSON ────────────────────────────────────────────────────
//
// 真实模型把**整段**输出包进 ```` ```json … ``` ```` 是常态（冒烟里 event 的 `rejected(schema)`
// 就是这个形状）。剥围栏的条件只有一个：**整段**恰好是一个围栏块；剥完仍走上面同一把尺子
// （`deny_unknown_fields` 一字不动）。围栏外有说明文字、两个围栏块、围栏标记拼错，一律照旧 `Json`。

/// 23. 整段一个围栏块（带 `json` 标记或不带、单行或换行、前后空白）⇒ 抽取与审核都收。
#[test]
fn a_whole_fenced_block_is_accepted_by_output_and_review() {
    let b = state_change_batch();
    let c = sample_candidates();
    let body = state_change_reply();

    for raw in [
        format!("```json\n{body}\n```"),
        format!("```\n{body}\n```"),
        // 围栏标记与正文之间没有换行也是同一个块。
        format!("```json{body}```"),
        // 围栏块前后的空白不算"杂质"（`trim` 是剥围栏的前提）。
        format!("\n  ```json\n{body}\n```  \n"),
    ] {
        assert_eq!(
            parse(&raw, &b, &c).unwrap().len(),
            1,
            "整段围栏块必须被接受：{raw}"
        );
    }

    let rbody = review_reply(vec![json!({"ref": "e1", "verdict": "pass", "code": null})]);
    for raw in [
        format!("```json\n{rbody}\n```"),
        format!("```\n{rbody}\n```"),
        format!("```json{rbody}```"),
    ] {
        assert_eq!(
            review(&["e1"], &raw),
            Ok(vec![("e1".to_string(), ReviewVerdict::Pass, None)]),
            "审核的整段围栏块必须被接受：{raw}"
        );
    }
}

/// 24. 围栏外有杂质 ⇒ 仍 `Json`；围栏内仍是严格 JSON（多键/坏类型/坏版本各归原错误）。
#[test]
fn fenced_json_outside_prose_still_fails_and_the_inner_grammar_is_unchanged() {
    let b = state_change_batch();
    let c = sample_candidates();
    let body = state_change_reply();
    let short = &body[..body.rfind('}').unwrap()];

    // 整段不是"一个围栏块"的几种形状：围栏前说明、围栏后说明、再来一个块、标记拼错。
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
            parse(&raw, &b, &c),
            Err(EventPassError::Json),
            "带散文/杂质的输出必须仍被拒：{raw}"
        );
    }

    // 围栏内多一个键：`deny_unknown_fields` 仍在，不是被围栏逻辑吞掉。
    let mut extra_item: Value = serde_json::from_str(&body).unwrap();
    extra_item["events"][0]["confidence"] = json!(0.9);
    assert_eq!(
        parse(&format!("```json\n{extra_item}\n```"), &b, &c),
        Err(EventPassError::Json)
    );
    // 围栏内顶层多一个键：同样 Json。
    let mut extra_top: Value = serde_json::from_str(&body).unwrap();
    extra_top["db_id"] = json!(7);
    assert_eq!(
        parse(&format!("```json\n{extra_top}\n```"), &b, &c),
        Err(EventPassError::Json)
    );
    // 围栏内坏类型：Json。
    let mut bad_type: Value = serde_json::from_str(&body).unwrap();
    bad_type["events"] = json!({"not": "a list"});
    assert_eq!(
        parse(&format!("```json\n{bad_type}\n```"), &b, &c),
        Err(EventPassError::Json)
    );
    // 围栏内坏版本：仍是 `Version`，不被折成 `Json`。
    let mut version: Value = serde_json::from_str(&body).unwrap();
    version["schema_version"] = json!(1);
    assert_eq!(
        parse(&format!("```json\n{version}\n```"), &b, &c),
        Err(EventPassError::Version)
    );

    // 审核侧同一条尺子。
    let rbody = review_reply(vec![json!({"ref": "e1", "verdict": "pass", "code": null})]);
    assert_eq!(review(&["e1"], &format!("说明\n{rbody}")), Err(EventPassError::Json));
    assert_eq!(review(&["e1"], &format!("{rbody}\n说明")), Err(EventPassError::Json));
    assert_eq!(
        review(&["e1"], &format!("```json\n{rbody}\n``` 额外文字")),
        Err(EventPassError::Json)
    );
    let mut rextra: Value = serde_json::from_str(&rbody).unwrap();
    rextra["verdicts"][0]["confidence"] = json!(0.9);
    assert_eq!(
        review(&["e1"], &format!("```json\n{rextra}\n```")),
        Err(EventPassError::Json),
        "审核围栏内多键仍归 Json"
    );
}

// ── S2e-8a：宽松解析（逐条丢弃，不拖整批）────────────────────────────────────
//
// 与 fact 侧同一条分界：**内容**错丢一条并记账，**信封/批次级声明**错仍整批拒。
// event 的信封只有四键（没有 `family`），所以这一侧的整批拒比 fact 少一格。

/// t2（"今天天气不错"）上的一条合法 observation：给"其余照常返回"当对照物。
fn greeting_event() -> Value {
    json!({
        "ref": "e2",
        "turn": "t2",
        "category": "observation",
        "actor": "a1",
        "trigger": "不错",
        "action": "notice_weather",
        "object": null,
        "object_text": null,
        "participants": ["a1"],
        "place": null,
        "result": "done",
        "mode": "stated",
        "time_text": null,
        "quote": "今天天气不错",
        "evidence": ["t2"],
        "text": "张三注意到今天天气不错。"
    })
}

fn lenient(raw: &str, b: &Batch, c: &[FactCandidate]) -> Result<LenientEvents, EventPassError> {
    parse_event_output_lenient(raw, b, c)
}

/// 25. 一条坏 quote + 一条合法 ⇒ Ok：只丢坏的那条，原因按原顺序记，uncovered 为空。
#[test]
fn lenient_drops_one_bad_quote_and_keeps_the_rest() {
    let b = state_change_batch();
    let c = sample_candidates();

    let mut mangled = state_change();
    mangled["quote"] = json!("我把配置改回旧版本，服务就恢复了");
    let raw = doc(vec![mangled, greeting_event()], &["t1", "t3"]);

    let got = lenient(&raw, &b, &c).unwrap();
    assert_eq!(got.drafts.len(), 1, "合法那条照常返回");
    assert_eq!(got.drafts[0].reference, "e2");
    assert_eq!(got.drafts[0].turn_uid, b.rows[1].turn_uid, "t2 换成真实 uid");
    assert_eq!(got.dropped, vec![EventPassError::QuoteNotVerbatim("e1".into())]);
    assert_eq!(got.uncovered, Vec::<String>::new());
}

/// 26. 全部坏 ⇒ 仍然 Ok：drafts 空、dropped 数量与顺序正确。
#[test]
fn lenient_keeps_going_when_every_draft_is_bad() {
    let b = state_change_batch();
    let c = sample_candidates();

    let mut mangled = state_change();
    mangled["quote"] = json!("我把配置改回旧版本，服务就恢复了");
    let mut both = state_change();
    both["ref"] = json!("e2");
    both["object"] = json!("a3");
    both["object_text"] = json!("配置文件");
    let raw = doc(vec![mangled, both], &["t1", "t2", "t3"]);

    let got = lenient(&raw, &b, &c).unwrap();
    assert!(got.drafts.is_empty());
    assert_eq!(
        got.dropped,
        vec![
            EventPassError::QuoteNotVerbatim("e1".into()),
            EventPassError::ObjectBoth,
        ],
        "dropped 顺序同原 events"
    );
    assert!(got.uncovered.is_empty());
}

/// 27. 信封仍是整批拒：顶层多键 / 事件多键 / 版本 / pass / `empty_turns` 批外引用。
#[test]
fn lenient_still_rejects_the_envelope_and_batch_level_declarations() {
    let b = state_change_batch();
    let c = sample_candidates();
    let good = state_change_reply();
    let mut value: Value = serde_json::from_str(&good).unwrap();

    assert_eq!(lenient("这不是 json", &b, &c), Err(EventPassError::Json));

    let mut top_extra = value.clone();
    top_extra["db_id"] = json!(7);
    assert_eq!(lenient(&top_extra.to_string(), &b, &c), Err(EventPassError::Json));

    // 单条多键在反序列化里就坏：整批 `Json`，不是丢一条。
    let mut event_extra = value.clone();
    event_extra["events"][0]["cause"] = json!("x");
    assert_eq!(lenient(&event_extra.to_string(), &b, &c), Err(EventPassError::Json));

    let mut version = value.clone();
    version["schema_version"] = json!(1);
    assert_eq!(lenient(&version.to_string(), &b, &c), Err(EventPassError::Version));

    let mut pass = value.clone();
    pass["pass"] = json!("fact");
    assert_eq!(lenient(&pass.to_string(), &b, &c), Err(EventPassError::Pass));

    value["empty_turns"] = json!(["t2", "t9"]);
    assert_eq!(
        lenient(&value.to_string(), &b, &c),
        Err(EventPassError::UnknownTurn("t9".into()))
    );
    value["empty_turns"] = json!(["t2", "t2"]);
    assert_eq!(
        lenient(&value.to_string(), &b, &c),
        Err(EventPassError::DuplicateRef("t2".into()))
    );
}

/// 28. 未覆盖目标进 `uncovered`（有序），不因此整批失败；`empty_turns` 认领后不再列。
#[test]
fn lenient_lists_uncovered_targets_and_keeps_the_drafts() {
    let b = state_change_batch();
    let c = sample_candidates();

    let raw = doc(vec![state_change()], &[]);
    let got = lenient(&raw, &b, &c).unwrap();
    assert_eq!(got.drafts.len(), 1);
    assert_eq!(got.uncovered, vec!["t2".to_string(), "t3".to_string()]);
    assert!(got.dropped.is_empty(), "覆盖缺口不是草稿的坏法");

    let raw = doc(vec![state_change()], &["t2", "t3"]);
    assert!(lenient(&raw, &b, &c).unwrap().uncovered.is_empty());

    // 坏草稿不覆盖它的 turn。
    let mut mangled = state_change();
    mangled["quote"] = json!("我把配置改回旧版本，服务就恢复了");
    let raw = doc(vec![mangled], &["t2", "t3"]);
    let got = lenient(&raw, &b, &c).unwrap();
    assert_eq!(got.uncovered, vec!["t1".to_string()]);
}

/// 29. 重复 ref：先出现的那条保留，第二条进 dropped。
#[test]
fn lenient_drops_the_second_duplicate_ref_and_keeps_the_first() {
    let b = state_change_batch();
    let c = sample_candidates();

    let mut second = greeting_event();
    second["ref"] = json!("e1");
    // t1 由保留下来的草稿覆盖；被丢的 t2 交给 empty_turns 认领，缺口因此为空。
    let raw = doc(vec![state_change(), second], &["t2", "t3"]);

    let got = lenient(&raw, &b, &c).unwrap();
    assert_eq!(got.drafts.len(), 1);
    assert_eq!(got.drafts[0].reference, "e1");
    assert_eq!(got.drafts[0].turn_uid, b.rows[0].turn_uid, "保留的是第一条（t1）");
    assert_eq!(got.dropped, vec![EventPassError::DuplicateRef("e1".into())]);
    assert!(got.uncovered.is_empty());
}

/// 30. 其余逐条格子也各丢一条、不连坐：批外 turn / 批外候选 / participants 空 / 坏 result / 空 ref。
#[test]
fn lenient_drops_each_content_error_kind_per_entry() {
    let b = state_change_batch();
    let c = sample_candidates();

    let mut unknown_turn = state_change();
    unknown_turn["turn"] = json!("t9");
    unknown_turn["evidence"] = json!(["t9"]);

    let mut unknown_actor = state_change();
    unknown_actor["ref"] = json!("e2");
    unknown_actor["actor"] = json!("a9");
    unknown_actor["participants"] = json!(["a9"]);

    let mut empty_participants = state_change();
    empty_participants["ref"] = json!("e3");
    empty_participants["participants"] = json!([]);

    let mut bad_result = state_change();
    bad_result["ref"] = json!("e4");
    bad_result["result"] = json!("大概好了");

    let mut blank_ref = state_change();
    blank_ref["ref"] = json!("");

    let raw = doc(
        vec![unknown_turn, unknown_actor, empty_participants, bad_result, blank_ref],
        &["t1", "t2", "t3"],
    );
    let got = lenient(&raw, &b, &c).unwrap();
    assert!(got.drafts.is_empty());
    assert_eq!(
        got.dropped,
        vec![
            EventPassError::UnknownTurn("t9".into()),
            EventPassError::UnknownCandidate("a9".into()),
            EventPassError::MissingActor,
            EventPassError::BadResult,
            EventPassError::Json,
        ]
    );
    assert!(got.uncovered.is_empty());
}

/// 31. 回归：同一批输入喂严格 parser，仍在**第一条**错处停下。
#[test]
fn strict_parser_still_returns_the_first_error_on_the_same_input() {
    let b = state_change_batch();
    let c = sample_candidates();

    let mut mangled = state_change();
    mangled["quote"] = json!("我把配置改回旧版本，服务就恢复了");
    let raw = doc(vec![mangled, greeting_event()], &["t1", "t3"]);
    assert_eq!(
        parse(&raw, &b, &c),
        Err(EventPassError::QuoteNotVerbatim("e1".into())),
        "严格侧仍整批判死，且是第一条错"
    );

    let mut second = greeting_event();
    second["ref"] = json!("e1");
    let raw = doc(vec![state_change(), second], &["t1", "t3"]);
    assert_eq!(
        parse(&raw, &b, &c),
        Err(EventPassError::DuplicateRef("e1".into()))
    );

    let raw = doc(vec![state_change()], &[]);
    assert_eq!(
        parse(&raw, &b, &c),
        Err(EventPassError::Uncovered(vec!["t2".into(), "t3".into()]))
    );
    assert_eq!(
        lenient(&raw, &b, &c).unwrap().uncovered,
        vec!["t2".to_string(), "t3".to_string()]
    );
}

/// P-fix 之一：模板里不许出现 gold 语料原句。「构建好了」逐字来自 gold 的 `624b9d2` 轮正文
/// （`grep -c` 两边都是 1）——写进 few-shot 就是把评测语料抄进 prompt。替换例子必须是语料里
/// 0 次出现的说法（「部署成功」），否则泄漏只是换了个位置。
#[test]
fn the_prompt_never_quotes_a_corpus_line() {
    let sys = event_system();
    assert!(!sys.contains("构建好了"), "模板抄了 gold 原句「构建好了」：{sys}");
    assert!(sys.contains("部署成功"), "模板缺替换后的新例子「部署成功」：{sys}");
}

/// P-fix 之二：result 闭集五值都要有触发条件。`ongoing`/`unknown` 原先是"列在闭集里、
/// 文案里给不出位置"的两个空位；"还在跑 / 结果未出"必须由标准说出口。
#[test]
fn the_standard_names_every_result_word() {
    let sys = event_system();
    assert!(count(&sys, "ongoing") >= 1, "标准里没有 ongoing 的位置：{sys}");
    assert!(count(&sys, "unknown") >= 1, "标准里没有 unknown 的位置：{sys}");
    assert!(
        sys.contains("still running") || sys.contains("not in yet"),
        "标准里缺「还在跑 / 结果未出」那一句：{sys}"
    );
}

/// P-fix 之三：抽取说"完成汇报算事件"，审核却要求 quote 点名 actor ⇒ 中文无主语的完成汇报
/// 被 missing_actor 拒（item 143：22 条草稿拒 20 条）。C6a 已把每轮的 role 与 speaker 渲染
/// 进审核输入，所以"quote 或目标轮说话人任一能指认"就够。
#[test]
fn the_review_lets_the_turn_speaker_name_the_actor() {
    let sys = event_review_system();
    assert!(
        sys.lines().any(|line| line.contains("missing_actor") && line.contains("speaker")),
        "missing_actor 与 speaker 不在同一条标准里：{sys}"
    );
    assert!(
        sys.contains("counts as naming"),
        "缺「agent 自己的完成汇报就是点名 agent」这层意思：{sys}"
    );
}

/// §10.70：`text` 里的批内引用 `aN` 由程序还原成候选正式名（程序只解指针），
/// 还原后数字接地检查不再把 `a1` 里的 1 读成凭空数字。
#[test]
fn a_batch_ref_in_text_is_resolved_to_the_candidate_name() {
    let b = state_change_batch();
    let c = sample_candidates();
    let mut draft = state_change();
    draft["text"] = json!("a1 昨晚把配置改回去，服务恢复。");
    let parsed = parse(&doc(vec![draft], &["t2", "t3"]), &b, &c).expect("引用还原后必须通过");
    assert_eq!(parsed[0].text, "张三 昨晚把配置改回去，服务恢复。");
}

/// §10.70：源文本身含 `…` 时，照抄它的 quote 是合法逐字引文；只有凭空加省略号才拒。
#[test]
fn an_ellipsis_that_the_source_itself_has_is_verbatim() {
    let body = "日志里有 199aa91d…d6ce 这样的前缀";
    let b = batch(&[body, "今天天气不错"]);
    let c = sample_candidates();
    let mut draft = state_change();
    draft["category"] = json!("observation");
    draft["trigger"] = json!("有");
    draft["quote"] = json!(body);
    draft["object_text"] = json!(null);
    draft["time_text"] = json!(null);
    draft["text"] = json!("日志里有 199aa91d…d6ce 这样的前缀。");
    let parsed = parse(&doc(vec![draft], &["t2"]), &b, &c).expect("源文自带省略号必须可引");
    assert_eq!(parsed[0].quote, body);
}

/// 一条最小 event，只换 quote/result；其余槽位固定成可过形状。
fn one_event(quote: &str, result: &str) -> Value {
    json!({
        "ref": "e1",
        "turn": "t1",
        "category": "event",
        "actor": "a1",
        "trigger": "写",
        "action": "write",
        "object": null,
        "object_text": null,
        "participants": ["a1"],
        "place": null,
        "result": result,
        "mode": "stated",
        "time_text": null,
        "quote": quote,
        "evidence": ["t1"],
        "text": "写续派档"
    })
}

/// 台账 §10.111：纯宣布句不进 event（程序闸，不靠 prompt 自觉）。
#[test]
fn announcement_only_event_is_rejected() {
    let b = batch(&["现在写续派档"]);
    let c = sample_candidates();
    let raw = doc(vec![one_event("现在写续派档", "unknown")], &[]);
    assert_eq!(parse(&raw, &b, &c), Err(EventPassError::AnnouncementOnly("e1".into())));

    let b = batch(&["现在跑全量测试与 clippy"]);
    let mut ev = one_event("现在跑全量测试与 clippy", "unknown");
    ev["trigger"] = json!("跑");
    ev["action"] = json!("run_tests");
    ev["text"] = json!("跑测试");
    let raw = doc(vec![ev], &[]);
    assert_eq!(parse(&raw, &b, &c), Err(EventPassError::AnnouncementOnly("e1".into())));

    // 句中计划动作也要拦：句首是"并"，但"核一下"说明还没做。
    let b = batch(&["并核一下现有 hub 的快照/备份能力"]);
    let mut ev = one_event("并核一下现有 hub 的快照/备份能力", "unknown");
    ev["trigger"] = json!("核");
    ev["action"] = json!("check_backup");
    ev["text"] = json!("核备份能力");
    let raw = doc(vec![ev], &[]);
    assert_eq!(parse(&raw, &b, &c), Err(EventPassError::AnnouncementOnly("e1".into())));
}

/// 已开始 / 已完成 / 有发现的引文必须能过闸：闸只减误收，不把真事件一起挡掉。
#[test]
fn started_and_completed_quotes_survive_the_announcement_gate() {
    let c = sample_candidates();

    let b = batch(&["我开始安装依赖，还没跑完"]);
    let mut ev = one_event("我开始安装依赖，还没跑完", "ongoing");
    ev["trigger"] = json!("开始安装");
    ev["action"] = json!("start_install");
    ev["text"] = json!("开始安装依赖");
    assert!(parse(&doc(vec![ev], &[]), &b, &c).is_ok(), "已开始的动作必须能过闸");

    let b = batch(&["基线已核：HEAD 3311510、工作树干净、752 passed"]);
    let mut ev = one_event("基线已核：HEAD 3311510、工作树干净、752 passed", "done");
    ev["trigger"] = json!("已核");
    ev["action"] = json!("check_baseline");
    ev["text"] = json!("基线已核");
    assert!(parse(&doc(vec![ev], &[]), &b, &c).is_ok(), "完成读数必须能过闸");

    let b = batch(&["变异精确打红：唯一红那条新用例"]);
    let mut ev = one_event("变异精确打红：唯一红那条新用例", "done");
    ev["trigger"] = json!("打红");
    ev["action"] = json!("make_red");
    ev["text"] = json!("精确打红");
    assert!(parse(&doc(vec![ev], &[]), &b, &c).is_ok(), "完成态读数必须能过闸");
}
