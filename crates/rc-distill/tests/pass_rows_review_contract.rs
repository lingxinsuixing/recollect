//! S3c-2：rows 窄审的 prompt 形状、渲染与解析器（设计 `2026-10-26-review-queue-s3c` 的
//! rows 行；拍板 C1：行 `text` 对证据轮，不做逐字引文）。
//!
//! 这一层只断"模型看到什么、程序怎么拒"：不选批、不调模型、不写库、不入队、不碰引擎。
//! 夹具只用通用占位名（张三/李四/某项目），不含任何真实语料专名或 holdout。
//!
//! 覆盖派发档 §5 的五条：
//! 1. 形状 4/7/1/5；canary 在 system 恰一次、在 user 零次；五码全在 system 且逐码有判决样例，
//!    码间优先级与 `reject`/`uncertain` 边界在文本里；
//! 2. `rows_review_user` 的 golden：两条（一条有 conflict、一条无）、subject 缺失写 `-`、
//!    空行分隔，且不含 64 位 hex 与 uid/记账字段；
//! 3. `prepare`：零宽字符与注入块围栏在渲染里被剪掉；
//! 4. 解析 golden：三种判决、code null 与非 null、返回顺序 = refs 顺序、```json 围栏可解；
//! 5. 解析负例逐条：每条断言 `slug()`。

use rc_distill::pass_fact_contract::ReviewVerdict;
use rc_distill::pass_prompt::{
    rows_review_shape, rows_review_system, rows_review_user, RowReviewItem, ROWS_REVIEW_CANARY,
    ROWS_REVIEW_CODES, PASS_ROWS_REVIEW,
};
use rc_distill::pass_rows_review_contract::{parse_rows_review, RowsReviewError};
use rc_distill::prepare::prepare;
use serde_json::{json, Value};

fn item(
    index: usize,
    text: &str,
    subject: Option<&str>,
    category: &str,
    predicate: &str,
    evidence_body: &str,
    conflict: Option<&str>,
) -> RowReviewItem {
    RowReviewItem {
        reference: format!("r{index}"),
        text: text.to_string(),
        subject_canonical: subject.map(|value| value.to_string()),
        category: category.to_string(),
        predicate: predicate.to_string(),
        evidence_body: evidence_body.to_string(),
        conflict: conflict.map(|value| value.to_string()),
    }
}

fn count(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// 任意 64 个连续 hex 字符即"看起来像 uid"：渲染里一个都不许有。
fn has_hex64(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    chars
        .windows(64)
        .any(|window| window.iter().all(|c| c.is_ascii_hexdigit()))
}

/// 1. 形状 4/7/1/5；canary 只在 system 首行一次、在 user 里零次；五码全在 system，
///    且码间优先级与 `reject`/`uncertain` 的边界写在标准里。
#[test]
fn rows_review_prompt_has_the_dispatched_shape_and_closed_codes() {
    let shape = rows_review_shape();
    assert_eq!(
        (shape.method_steps, shape.standards, shape.positives, shape.negatives),
        (4, 7, 1, 5),
        "方法四步、标准七条、正例 1、反例 5（五码逐码一条反例）"
    );

    let system = rows_review_system();
    assert_eq!(count(&system, ROWS_REVIEW_CANARY), 1, "canary 只在 system 首行出现一次");
    assert!(system.starts_with(ROWS_REVIEW_CANARY), "canary 必须在首行");
    assert!(system.contains(&format!("\"pass\":\"{}\"", PASS_ROWS_REVIEW)));
    assert!(system.contains("\"schema_version\":0"));
    for code in ROWS_REVIEW_CODES {
        assert!(system.contains(code), "system 缺五码闭集值：{code}");
    }
    // 码间优先级（同一个输入命中多条时报哪一个）与 `reject`/`uncertain` 的边界。
    assert!(
        system.contains("ONE CODE, FIRST THAT APPLIES")
            && system.contains(
                "report the first in this order: unsupported, wrong_subject, invented_number, contradiction, not_atomic"
            ),
        "system 缺码间优先级句"
    );
    assert!(
        system.contains("REJECT VS UNCERTAIN")
            && system.contains("use uncertain only when the TURN text is truncated or missing"),
        "system 缺 reject 与 uncertain 的边界"
    );
    // S3c-fix：uncertain 不再"无码可填"——必须填这条轮子无法裁定的规则的码。
    assert!(
        system.contains("when uncertain, use the code of the rule the TURN cannot settle"),
        "system 缺 uncertain 的取码句"
    );
    // 单任务：只判行，不改写。
    for needle in ["row support review pass", "one question only", "never rewrite one"] {
        assert!(system.contains(needle), "system 缺单任务表述：{needle}");
    }

    // 空 user：canary 一次都不许进 user。
    let user = rows_review_user(&[]);
    assert_eq!(count(&user, ROWS_REVIEW_CANARY), 0, "canary 只在 system，不进 user");
    assert!(!has_hex64(&system));
}

/// 1b. 五码逐码有判决样例：每条反例都写完整 JSON，且恰有一条 reject + 该码。
#[test]
fn every_rows_code_has_a_negative_with_a_reject_verdict() {
    let system = rows_review_system();
    let negatives: Vec<&str> = system.lines().filter(|line| line.starts_with("BAD:")).collect();
    assert_eq!(negatives.len(), 5, "五码逐码一条反例");
    for code in ROWS_REVIEW_CODES {
        let sample = negatives
            .iter()
            .find(|line| line.contains(&format!("\"code\":\"{code}\"")))
            .unwrap_or_else(|| panic!("{code} 没有带判决 JSON 的反例"));
        assert!(
            sample.contains("\"verdict\":\"reject\""),
            "{code} 的反例必须给出 reject 判决：{sample}"
        );
        assert!(sample.contains("\"ref\":\"r"), "{code} 的反例必须点名行：{sample}");
    }
}

/// 2. golden：两行（一行有 conflict、一行无且 subject 缺失）；空行分隔、逐行顺序、
///    `subject: -`、`predicate:` 必在场、conflict 整行省略，以及渲染里没有任何 identity。
#[test]
fn rows_review_user_renders_rows_without_conflict_line_when_absent() {
    let items = vec![
        item(
            1,
            "发布推迟到 12 月 3 日",
            Some("张三"),
            "promise",
            "postpone-release",
            "我们决定把发布推迟到 12 月 3 日。",
            Some("12 月 5 日"),
        ),
        item(
            2,
            "旧版本回滚完成",
            None,
            "state_change",
            "rollback-old-version",
            "旧版本已经回滚。",
            None,
        ),
    ];

    let user = rows_review_user(&items);
    let expected = "\
PASS rows_review v0
ROW r1
subject: 张三
category: promise
predicate: postpone-release
text: 发布推迟到 12 月 3 日
conflict: 12 月 5 日
TURN:
我们决定把发布推迟到 12 月 3 日。

ROW r2
subject: -
category: state_change
predicate: rollback-old-version
text: 旧版本回滚完成
TURN:
旧版本已经回滚。
";
    assert_eq!(user, expected, "golden：逐行顺序、空行分隔、subject 缺失写 -、conflict 可省");

    // S3c-fix（复审 P1-b）：谓词必须逐行进 prompt——`contradiction` 的判据是"同一主语同一谓词"。
    assert_eq!(count(&user, "predicate:"), 2, "每行恰一条 predicate 行：{user}");
    assert!(
        user.contains("predicate: postpone-release\n"),
        "行谓词没有渲染：{user}"
    );

    // 无 conflict 的那一行连 `conflict:` 字样都不出现（不是写 null、不是留空行）。
    let second = user.split("\n\n").nth(1).expect("两行之间有一个空行");
    assert!(!second.contains("conflict:"), "conflict 缺失时整行省略：{second}");
    assert_eq!(count(&user, "conflict:"), 1, "只有第一行带 conflict");

    // identity 一个字都不许有。
    assert!(!has_hex64(&user), "user 不含任何 64 位 hex 串");
    for needle in ["turn_uid", "anchor_id", "millisecond", "_ms", "confidence", "owner"] {
        assert!(!user.contains(needle), "user 不许出现记账字段：{needle}");
    }
}

/// 3. `prepare`：零宽字符与注入块围栏在渲染里与 `prepare()` 逐字一致（含入的正是剪过的）。
#[test]
fn rows_review_user_passes_every_text_cell_through_prepare() {
    let raw_text = "发布推迟到\u{200b}12 月 3 日";
    let raw_body = "证据轮正文\n[recollect] 注入块不是用户说的话\n后面还在";
    let clean_text = prepare(raw_text);
    let clean_body = prepare(raw_body);
    assert_ne!(clean_text, raw_text, "夹具本身要过剪刀");
    assert_ne!(clean_body, raw_body, "夹具本身要过剪刀");

    let user = rows_review_user(&[item(
        1,
        raw_text,
        Some("张\u{200b}三"),
        "promise",
        "post\u{200b}pone-release",
        raw_body,
        Some("12 月\u{200b}5 日"),
    )]);

    assert!(user.contains(&format!("text: {clean_text}\n")), "text 过 prepare");
    assert!(user.contains("subject: 张三\n"), "subject 过同一把剪刀");
    assert!(user.contains("predicate: postpone-release\n"), "predicate 过同一把剪刀");
    assert!(user.contains("conflict: 12 月5 日\n"), "conflict 过同一把剪刀");
    assert!(user.contains(&format!("TURN:\n{clean_body}\n")), "证据轮正文过同一把剪刀");
    assert!(!user.contains('\u{200b}'), "零宽字符不许原样进渲染");
    assert!(!user.contains("[recollect]"), "注入块在渲染里不存在（不是标为不可信）");
    assert!(!user.contains("注入块不是用户说的话"));
}

// ── 解析器 ────────────────────────────────────────────────────────────────────

fn refs(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

fn reply(verdicts: Vec<Value>) -> String {
    json!({
        "schema_version": 0,
        "pass": PASS_ROWS_REVIEW,
        "verdicts": verdicts,
    })
    .to_string()
}

fn verdict(reference: &str, label: &str, code: Value) -> Value {
    json!({"ref": reference, "verdict": label, "code": code})
}

/// 4. golden：三种判决、code null 与非 null、返回顺序 = refs 顺序（回复顺序被消掉）、
///    整段 ```json 围栏可解。
#[test]
fn parse_rows_review_returns_verdicts_in_refs_order() {
    // 回复顺序 r3,r1,r2 ≠ refs 顺序。
    let raw = reply(vec![
        verdict("r3", "uncertain", json!("contradiction")),
        verdict("r1", "pass", Value::Null),
        verdict("r2", "reject", json!("invented_number")),
    ]);
    let got = parse_rows_review(&raw, &refs(&["r1", "r2", "r3"])).expect("合法回复必须能解");
    assert_eq!(
        got,
        vec![
            ("r1".to_string(), ReviewVerdict::Pass, None),
            ("r2".to_string(), ReviewVerdict::Reject, Some("invented_number".to_string())),
            ("r3".to_string(), ReviewVerdict::Uncertain, Some("contradiction".to_string())),
        ],
        "返回顺序跟 refs 走，code 原样带出"
    );

    // 围栏只是运输层：整段一个 ```json 块与裸 JSON 同解。
    let fenced = format!("```json\n{raw}\n```");
    assert_eq!(parse_rows_review(&fenced, &refs(&["r1", "r2", "r3"])), Ok(got));

    // 空 refs + 空判决是合法空批，不是错误。
    assert_eq!(parse_rows_review(&reply(Vec::new()), &[]), Ok(Vec::new()));
}

/// 5. 负例逐条：每条只钉一格，断言的是 `slug()`（封闭词，不带模型正文）。
#[test]
fn parse_rows_review_rejects_every_broken_shape_by_closed_slug() {
    let known = refs(&["r1", "r2"]);
    let cases: Vec<(&str, String, &str)> = vec![
        // 未知 ref / 重复 ref / 缺 ref。
        (
            "未知 ref",
            reply(vec![verdict("r1", "pass", Value::Null), verdict("r9", "pass", Value::Null)]),
            "ref_unknown",
        ),
        (
            "重复 ref",
            reply(vec![
                verdict("r1", "pass", Value::Null),
                verdict("r1", "pass", Value::Null),
                verdict("r2", "pass", Value::Null),
            ]),
            "ref_duplicate",
        ),
        ("缺 ref", reply(vec![verdict("r1", "pass", Value::Null)]), "ref_missing"),
        // verdict 闭集。
        ("未知 verdict", reply(vec![verdict("r1", "approve", Value::Null)]), "verdict"),
        // code 与 verdict 相配。
        ("reject 缺 code（null）", reply(vec![verdict("r1", "reject", Value::Null)]), "verdict_code"),
        ("reject 缺 code（空串）", reply(vec![verdict("r1", "reject", json!(""))]), "verdict_code"),
        (
            "reject 缺 code（键整个漏掉）",
            json!({"schema_version":0,"pass":PASS_ROWS_REVIEW,
                   "verdicts":[{"ref":"r1","verdict":"reject"}]})
            .to_string(),
            "verdict_code",
        ),
        ("pass 带 code", reply(vec![verdict("r1", "pass", json!("unsupported"))]), "code_on_pass"),
        (
            "pass 缺 code（键整个漏掉）",
            json!({"schema_version":0,"pass":PASS_ROWS_REVIEW,
                   "verdicts":[{"ref":"r1","verdict":"pass"}]})
            .to_string(),
            "code_on_pass",
        ),
        // 五码闭集：naming 的码在这里不是码。
        ("未知 code", reply(vec![verdict("r1", "reject", json!("not_verbatim"))]), "verdict_code"),
        // 信封。
        (
            "schema_version≠0",
            json!({"schema_version":1,"pass":PASS_ROWS_REVIEW,"verdicts":[]}).to_string(),
            "schema_version",
        ),
        (
            "pass≠rows_review",
            json!({"schema_version":0,"pass":"naming_review","verdicts":[]}).to_string(),
            "pass",
        ),
        (
            "verdicts 不是数组",
            json!({"schema_version":0,"pass":PASS_ROWS_REVIEW,"verdicts":{}}).to_string(),
            "verdicts",
        ),
        (
            "顶层多一个键",
            json!({"schema_version":0,"pass":PASS_ROWS_REVIEW,"verdicts":[],"family":"fact"})
                .to_string(),
            "extra_key",
        ),
        ("顶层缺一个键", json!({"schema_version":0,"verdicts":[]}).to_string(), "json"),
        ("不是 JSON", "not json at all".to_string(), "json"),
        // 条目形状。
        (
            "条目多一个键",
            reply(vec![json!({"ref":"r1","verdict":"pass","code":null,"reason":"看起来没问题"})]),
            "extra_key",
        ),
        ("条目缺 verdict", reply(vec![json!({"ref":"r1","code":null})]), "json"),
        ("条目 code 类型错", reply(vec![verdict("r1", "reject", json!(1))]), "json"),
        ("条目 ref 为空串", reply(vec![verdict("", "pass", Value::Null)]), "json"),
    ];

    for (label, raw, expected_slug) in cases {
        let error =
            parse_rows_review(&raw, &known).expect_err(&format!("负例必须被拒：{label}"));
        assert_eq!(error.slug(), expected_slug, "负例 slug 不对：{label}");
    }
}

/// 6. 错误面封闭：十二个变体的 slug 恰是十一个词，且没有变体落在自由文本上。
#[test]
fn rows_review_error_slugs_are_a_closed_set_of_words() {
    let expected = [
        (RowsReviewError::Json, "json"),
        (RowsReviewError::SchemaVersion, "schema_version"),
        (RowsReviewError::Pass, "pass"),
        (RowsReviewError::VerdictsShape, "verdicts"),
        (RowsReviewError::RefUnknown, "ref_unknown"),
        (RowsReviewError::RefDuplicate, "ref_duplicate"),
        (RowsReviewError::RefMissing, "ref_missing"),
        (RowsReviewError::VerdictUnknown, "verdict"),
        (RowsReviewError::CodeMissing, "verdict_code"),
        (RowsReviewError::CodeUnknown, "verdict_code"),
        (RowsReviewError::CodeOnPass, "code_on_pass"),
        (RowsReviewError::ExtraKey, "extra_key"),
    ];
    let slugs: Vec<&str> = expected.iter().map(|(_, slug)| *slug).collect();
    for (error, slug) in expected {
        assert_eq!(error.slug(), slug, "slug 表与枚举不同步");
        assert!(!slug.chars().any(|c| c.is_whitespace()), "slug 是词，不是句子");
    }
    // 十一个词：`verdict_code` 由 CodeMissing/CodeUnknown 共用。
    let mut unique = slugs.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), 11, "封闭词恰十一格");
}
