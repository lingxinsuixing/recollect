//! §7.3b：uncertain 草稿的**单条重跑**（专用 prompt + 单条契约）。
//!
//! 派发档 `2026-10-29-7-3b` §2.2：`fact_review_retry_system` 是同一批 item 的**第三次**
//! 独立调用——第一、二次是 proposer / challenger（§7.2）。它只判**一条**草稿，输入比正常
//! 审核更全：引文边界、整轮正文、前后邻轮、同轮子句清单、候选锚全表。
//!
//! 这一份只断 prompt 形状与单条解析（不选批、不调模型、不写库）；队列选择与三态合成在
//! `crates/hub/tests/review_retry_cli.rs`。

use rc_distill::pass_fact_contract::{
    parse_fact_review_retry, EvidenceDowngrade, FactPassError, ReviewVerdict,
};
use rc_distill::pass_prompt::{
    fact_review_challenge_system, fact_review_retry_shape, fact_review_retry_system,
    fact_review_retry_user, fact_review_system, fact_system, FactCandidate, FactFamily,
    ReviewInputContext, FACT_CANARY, FACT_REVIEW_CANARY, FACT_REVIEW_CHALLENGE_CANARY,
    FACT_REVIEW_RETRY_CANARY, REVIEW_CODES,
};
use rc_distill::{Batch, FactDraft, TurnRow};
use serde_json::{json, Value};

const NOW: i64 = 1_792_994_700_000;

fn uid(index: usize) -> String {
    format!("{:064x}", index + 1)
}

fn anchor_id(index: usize) -> String {
    format!("{:064x}", 0xa0 + index)
}

fn count(hay: &str, needle: &str) -> usize {
    hay.match_indices(needle).count()
}

fn batch(bodies: &[&str]) -> Batch {
    Batch {
        session_key: "sess-retry".into(),
        device_id: "dev-retry".into(),
        source_agent: "agent-retry".into(),
        project_hint: None,
        rows: bodies
            .iter()
            .enumerate()
            .map(|(index, body)| TurnRow {
                turn_uid: uid(index),
                role: if index % 2 == 0 {
                    "human".into()
                } else {
                    "agent".into()
                },
                body: (*body).to_string(),
                created_at_ms: NOW + index as i64 * 60_000,
                ingested_at_ms: NOW + index as i64 * 60_000,
                speaker: None,
                speaker_anchor_id: None,
            })
            .collect(),
    }
}

fn review_ctx(b: &Batch) -> ReviewInputContext {
    ReviewInputContext {
        session_key: b.session_key.clone(),
        owner_agent_id: "owner-retry".into(),
        visibility: "project".into(),
        scope: "project:sess-retry".into(),
        speakers: b
            .rows
            .iter()
            .map(|row| {
                if row.role == "human" {
                    "张三".to_string()
                } else {
                    b.source_agent.clone()
                }
            })
            .collect(),
    }
}

fn candidates() -> Vec<FactCandidate> {
    vec![
        FactCandidate {
            reference: "a1".into(),
            kind: "person".into(),
            canonical: "张三".into(),
            anchor_id: anchor_id(1),
            mark: None,
        },
        FactCandidate {
            reference: "a2".into(),
            kind: "person".into(),
            canonical: "李四".into(),
            anchor_id: anchor_id(2),
            mark: None,
        },
    ]
}

fn review_draft(
    reference: &str,
    turn_uid: String,
    subject: &str,
    object: Option<&str>,
    value: Option<&str>,
    quote: &str,
) -> FactDraft {
    FactDraft {
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
        text: String::new(),
        subject_canonical: subject.into(),
        object_canonical: object.map(str::to_string),
    }
}

fn retry_reply(verdict: &str, code: Value, evidence: Value, note: Value) -> String {
    json!({"verdict": verdict, "code": code, "evidence": evidence, "note": note}).to_string()
}

/// 1. 重跑模板是第三次独立调用：自己的 canary（首行一次），三向缺席。
#[test]
fn the_retry_prompt_has_its_own_canary_and_the_three_review_roles_never_borrow_each_other() {
    assert_ne!(FACT_REVIEW_RETRY_CANARY, FACT_REVIEW_CANARY);
    assert_ne!(FACT_REVIEW_RETRY_CANARY, FACT_REVIEW_CHALLENGE_CANARY);
    assert_ne!(FACT_REVIEW_RETRY_CANARY, FACT_CANARY);

    let shape = fact_review_retry_shape();
    assert_eq!(
        (
            shape.method_steps,
            shape.standards,
            shape.positives,
            shape.negatives
        ),
        (4, 6, 1, 5),
        "重跑：方法四步（只判一条 / 边界 / 全上下文 / 一个判决）、标准六条、正例 1、反例 5"
    );

    let retry = fact_review_retry_system();
    assert!(
        retry.starts_with(FACT_REVIEW_RETRY_CANARY),
        "canary 必须在首行"
    );
    assert_eq!(
        count(&retry, FACT_REVIEW_RETRY_CANARY),
        1,
        "重跑 canary 只在首行一次"
    );
    assert!(
        !retry.contains(FACT_REVIEW_CANARY),
        "重跑不借 proposer 的 canary"
    );
    assert!(
        !retry.contains(FACT_REVIEW_CHALLENGE_CANARY),
        "重跑不借 challenger 的 canary"
    );
    assert!(!retry.contains(FACT_CANARY), "重跑不借抽取的 canary");

    for family in FactFamily::ALL {
        let proposer = fact_review_system(family);
        let challenger = fact_review_challenge_system(family);
        let extract = fact_system(family);
        for (name, sys) in [
            ("proposer", &proposer),
            ("challenger", &challenger),
            ("extract", &extract),
        ] {
            assert!(
                !sys.contains(FACT_REVIEW_RETRY_CANARY),
                "{name} 模板不许出现重跑的 canary"
            );
        }
        // 反向仍成立：两份正常审核的 canary 还在自己那一份里。
        assert!(proposer.contains(FACT_REVIEW_CANARY));
        assert!(challenger.contains(FACT_REVIEW_CHALLENGE_CANARY));
    }

    // 单条契约：只判一条、只依据 quote 取证据、note 说缺口、code 闭集。
    assert!(retry.contains("exactly ONE row"), "必须明写只判一条");
    assert!(retry.contains("ONLY THE QUOTE IS EVIDENCE"));
    assert!(
        retry.contains("never scores it"),
        "note 必须明写程序只存不判分"
    );
    assert!(retry.contains("Never repair") || retry.contains("never repair"));
    for code in REVIEW_CODES {
        assert!(retry.contains(code), "重跑缺 code：{code}");
    }
    // 输出契约四键（单条、无 schema/pass/family），且不是整批契约。
    for key in ["\"verdict\"", "\"code\"", "\"evidence\"", "\"note\""] {
        assert!(retry.contains(key), "输出契约缺键：{key}");
    }
    assert!(
        !retry.contains("\"verdicts\""),
        "重跑不是整批契约：不许出现 verdicts 数组"
    );
}

/// 2. 重跑 user 给足五项上下文：整轮 + 引文边界 + 前后邻轮 + 同轮子句清单 + 候选锚全表。
#[test]
fn retry_user_shows_the_quote_boundary_the_whole_turn_the_clauses_and_the_anchor_table() {
    let b = batch(&[
        "今天是2026年10月28日。",
        "好，我下周三之前把对账单发给你。记得查收。",
        "好的，收到",
    ]);
    let d = review_draft(
        "f1",
        b.rows[1].turn_uid.clone(),
        "张三",
        Some("李四"),
        Some("把对账单发给你"),
        "我下周三之前把对账单发给你",
    );
    let user = fact_review_retry_user(FactFamily::Promise, &d, &b, &review_ctx(&b), &candidates());

    assert!(
        user.lines().next().unwrap().contains("fact_review_retry"),
        "首行点明这是重跑调用"
    );
    assert!(user.lines().next().unwrap().contains("family promise"));
    assert!(
        user.contains("the only row in this call"),
        "必须明写只判这一条"
    );
    // ① 该草稿的六格
    assert!(
        user.contains("subject: 张三"),
        "主语给 canonical，不给 anchor_id"
    );
    assert!(user.contains("predicate: promise"));
    assert!(user.contains("object: 李四"));
    assert!(user.contains("value: 把对账单发给你"));
    assert!(user.contains("category: promise"));
    assert!(user.contains("quote: 我下周三之前把对账单发给你"));
    // ② 引文边界：同一轮里 quote 前后的原文，quote 带标记
    assert!(user.contains("QUOTE BOUNDARY"));
    assert!(
        user.contains("[[我下周三之前把对账单发给你]]"),
        "quote 必须被标记出来"
    );
    assert!(user.contains("before: 好，"), "引文前文：{user}");
    assert!(user.contains("after: 。记得查收。"), "引文后文：{user}");
    // ③ 该轮完整正文 + 前后各一轮
    assert!(
        user.contains("TARGET TURN [role: agent"),
        "整轮正文的段头（夹具里第 2 轮是 agent）"
    );
    assert!(
        user.contains("好，我下周三之前把对账单发给你。记得查收。"),
        "整轮正文完整给出"
    );
    let ctx_at = user
        .find("CTX (not evidence):")
        .expect("有邻轮时必须出现 CTX 段");
    assert!(
        user[ctx_at..].contains("今天是2026年10月28日"),
        "前一轮在 CTX 段"
    );
    assert!(user[ctx_at..].contains("好的，收到"), "后一轮在 CTX 段");
    // ④ 同一轮被切出的全部子句（编号 + 原文）
    assert!(user.contains("CLAUSES OF THIS TURN"));
    assert!(
        user.contains("[c2] 好，我下周三之前把对账单发给你。"),
        "同轮子句 c2：{user}"
    );
    assert!(user.contains("[c3] 记得查收。"), "同轮子句 c3");
    assert!(!user.contains("[c1] 今天是"), "别的轮的子句不进这一格");
    // ⑤ 候选锚全表
    assert!(user.contains("CANDIDATES"));
    assert!(user.contains("- a1 | person | 张三"));
    assert!(user.contains("- a2 | person | 李四"));
    // 身份与时钟一个字都不给（与 normal 审核同一把尺子）
    assert!(!user.contains(&uid(1)), "user 不渲染 turn_uid");
    assert!(
        !user.contains(&anchor_id(1)) && !user.contains(&anchor_id(2)),
        "不渲染 anchor_id"
    );
    assert!(!user.contains("_ms"), "不渲染毫秒字段");
    assert!(!user.contains("confidence"), "不给 confidence");
}

/// 3. 单条重跑契约：三分支 + 证据闸（与 §7.2 同一条：不合格逐条降 uncertain）+ note 原样带回。
#[test]
fn retry_parser_accepts_one_ruling_and_gates_evidence_against_the_quote() {
    let quote = "我下周三之前把对账单发给你";

    let pass = parse_fact_review_retry(
        &retry_reply("pass", json!(null), json!("把对账单发给你"), json!(null)),
        "f1",
        quote,
    )
    .unwrap();
    assert_eq!(
        (
            pass.verdict,
            pass.code.as_deref(),
            pass.evidence.as_deref(),
            pass.note
        ),
        (ReviewVerdict::Pass, None, Some("把对账单发给你"), None)
    );
    assert_eq!(pass.downgrade, None);

    let reject = parse_fact_review_retry(
        &retry_reply(
            "reject",
            json!("not_durable"),
            json!("下周三之前"),
            json!(null),
        ),
        "f1",
        quote,
    )
    .unwrap();
    assert_eq!(
        (reject.verdict, reject.code.as_deref(), reject.downgrade),
        (ReviewVerdict::Reject, Some("not_durable"), None)
    );

    // uncertain 允许不带证据；note 原样带回（程序只存不判分）。
    let uncertain = parse_fact_review_retry(
        &retry_reply(
            "uncertain",
            json!(null),
            json!(null),
            json!("缺主语锚：候选表里没有张三"),
        ),
        "f1",
        quote,
    )
    .unwrap();
    assert_eq!(uncertain.verdict, ReviewVerdict::Uncertain);
    assert_eq!(uncertain.downgrade, None);
    assert_eq!(
        uncertain.note.as_deref(),
        Some("缺主语锚：候选表里没有张三")
    );

    // 证据越界 ⇒ 逐条降 uncertain 并记原因（§7.2 的同一条闸）。
    let outside = parse_fact_review_retry(
        &retry_reply("pass", json!(null), json!("李四"), json!(null)),
        "f1",
        quote,
    )
    .unwrap();
    assert_eq!(outside.verdict, ReviewVerdict::Uncertain);
    assert_eq!(outside.code, None, "降级后 code 清空");
    assert_eq!(
        outside.evidence.as_deref(),
        Some("李四"),
        "模型给的原文仍原样带回"
    );
    assert_eq!(outside.downgrade, Some(EvidenceDowngrade::NotInQuote));

    // 缺证据的 pass ⇒ evidence_missing。
    let missing = parse_fact_review_retry(
        &retry_reply("pass", json!(null), json!(null), json!(null)),
        "f1",
        quote,
    )
    .unwrap();
    assert_eq!(missing.verdict, ReviewVerdict::Uncertain);
    assert_eq!(missing.downgrade, Some(EvidenceDowngrade::Missing));
}

/// 4. 单条重跑契约的形状闸：多键 / 列表 / 坏判决 / code 与判决不配一律整条拒。
#[test]
fn retry_parser_rejects_anything_that_is_not_one_well_formed_ruling() {
    let quote = "我下周三之前把对账单发给你";

    // 把整批审核契约照搬过来（多一个 verdicts 键）⇒ 形状错。
    let batchy = json!({
        "verdict": "pass", "code": null, "evidence": "把对账单发给你", "note": null,
        "verdicts": [],
    })
    .to_string();
    assert_eq!(
        parse_fact_review_retry(&batchy, "f1", quote),
        Err(FactPassError::Json)
    );

    // 一条列表 ⇒ 形状错（只判一条，不收数组）。
    let listed =
        json!([{"verdict": "pass", "code": null, "evidence": "把对账单发给你", "note": null}])
            .to_string();
    assert_eq!(
        parse_fact_review_retry(&listed, "f1", quote),
        Err(FactPassError::Json)
    );

    // 判决闭集
    assert_eq!(
        parse_fact_review_retry(
            &retry_reply("maybe", json!(null), json!("把对账单发给你"), json!(null)),
            "f1",
            quote
        ),
        Err(FactPassError::BadReviewVerdict("maybe".into()))
    );
    // pass 带 code / reject 缺 code ⇒ 同一个码（载荷是 ref，不回显模型写的词）
    assert_eq!(
        parse_fact_review_retry(
            &retry_reply("pass", json!("other"), json!("把对账单发给你"), json!(null)),
            "f1",
            quote
        ),
        Err(FactPassError::BadReviewCode("f1".into()))
    );
    assert_eq!(
        parse_fact_review_retry(
            &retry_reply("reject", json!(null), json!("下周三之前"), json!(null)),
            "f1",
            quote
        ),
        Err(FactPassError::BadReviewCode("f1".into()))
    );
    // 整段围栏照剥（与抽取/审核同一把剪刀）。
    let fenced = format!(
        "```json\n{}\n```",
        retry_reply("pass", json!(null), json!("把对账单发给你"), json!(null))
    );
    assert!(
        parse_fact_review_retry(&fenced, "f1", quote).is_ok(),
        "整段围栏仍是合法的运输层"
    );
}
