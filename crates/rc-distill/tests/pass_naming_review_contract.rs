//! S3b-3a：naming 窄审的 prompt 形状、渲染与解析器（设计 `2026-10-26-review-queue-s3` §6 的
//! naming 行）。
//!
//! 这一层只断"模型看到什么、程序怎么拒"：不选批、不调模型、不写库、不入队、不碰 runner。
//! 夹具只用通用占位名（张三/李四/某项目），不含任何真实语料专名或 holdout。
//!
//! 覆盖派发档 §4 的五条：
//! 1. 形状 4/7/1/6；canary 在 system 恰一次、在 user 零次；六码全在 system 且逐码有判决样例，
//!    码间优先级与 `reject`/`uncertain` 边界在文本里（H6：从 4/4/1/3 加宽）；
//! 2. `naming_review_user` 的 golden：t/a 编号、evidence 时序、speaker 的 a-ref、
//!    `aliases: -`、逐行顺序，且不含 64 位 hex 与 uid/记账字段；
//! 3. `prepare`：零宽字符与注入块围栏在渲染里与 `prepare()` 一致；
//! 4. 解析 golden：三种判决、code null 与非 null、返回顺序 = refs 顺序、```json 围栏可解；
//! 5. 解析负例逐条：每条断言 `slug()`。

use rc_distill::pass_fact_contract::ReviewVerdict;
use rc_distill::pass_naming_review_contract::{parse_naming_review, NamingReviewError};
use rc_distill::pass_prompt::{
    naming_review_shape, naming_review_system, naming_review_user, NAMING_REVIEW_CANARY,
    NAMING_REVIEW_CODES, PASS_NAMING_REVIEW,
};
use rc_distill::prepare::prepare;
use rc_distill::{Batch, TurnRow};
use rc_memory::writers_v4::{A1Anchor, A1Batch, A1Mention};
use rc_memory::{AnchorRole, Provenance};
use serde_json::{json, Value};

const NOW: i64 = 1_792_994_700_000;

/// 64 位 uid：只在夹具里，绝不进渲染。
fn uid(index: usize) -> String {
    format!("{:064x}", index + 1)
}

fn batch(bodies: &[&str]) -> Batch {
    Batch {
        session_key: "sess-naming-review".into(),
        device_id: "dev-naming-review".into(),
        source_agent: "agent-naming-review".into(),
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

fn evidence(turn_uid: &str) -> Provenance {
    Provenance { turn_uid: turn_uid.to_string(), anchor_role: AnchorRole::Human }
}

fn anchor(
    reference: &str,
    kind: &str,
    canonical: &str,
    aliases: &[&str],
    evidence_turns: &[usize],
) -> A1Anchor {
    A1Anchor {
        reference: reference.into(),
        kind: kind.into(),
        canonical: canonical.into(),
        aliases: aliases.iter().map(|alias| (*alias).to_string()).collect(),
        reality_status: "real".into(),
        evidence: evidence_turns.iter().map(|index| evidence(&uid(*index))).collect(),
        stable: true,
    }
}

fn mentioned(anchor_ref: &str, surface: &str, turn: usize, speaker: Option<&str>) -> A1Mention {
    A1Mention {
        anchor_ref: anchor_ref.into(),
        surface: surface.into(),
        turn_uid: uid(turn),
        speaker_ref: speaker.map(|reference| reference.to_string()),
        discourse_role: "mentioned".into(),
        resolution: "resolved".into(),
        confidence: 0.93,
    }
}

fn empty_batch(anchors: Vec<A1Anchor>, mentions: Vec<A1Mention>) -> A1Batch {
    A1Batch { anchors, mentions, issues: Vec::new(), unresolved: Vec::new() }
}

fn count(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// 任意 64 个连续 hex 字符即"看起来像 uid"：渲染里一个都不许有。
fn has_hex64(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    chars.windows(64).any(|window| window.iter().all(|c| c.is_ascii_hexdigit()))
}

/// 1. 形状 4/7/1/6；canary 只在 system 首行一次、在 user 里零次；六码全在 system，
///    且每码有规则、码间有优先级、`reject` 与 `uncertain` 的边界写在标准里。
#[test]
fn naming_review_prompt_has_the_dispatched_shape_and_closed_codes() {
    let shape = naming_review_shape();
    assert_eq!(
        (shape.method_steps, shape.standards, shape.positives, shape.negatives),
        (4, 7, 1, 6),
        "方法四步、标准七条、正例 1、反例 6（H6：六码逐码有规则与判决样例）"
    );

    let system = naming_review_system();
    assert_eq!(count(&system, NAMING_REVIEW_CANARY), 1, "canary 只在 system 首行出现一次");
    assert!(system.starts_with(NAMING_REVIEW_CANARY), "canary 必须在首行");
    assert!(system.contains(&format!("\"pass\":\"{}\"", PASS_NAMING_REVIEW)));
    assert!(system.contains("\"schema_version\":0"));
    for code in NAMING_REVIEW_CODES {
        assert!(system.contains(code), "system 缺六码闭集值：{code}");
    }
    // H6：码间优先级（同一个输入命中多条时报哪一个）与 `reject`/`uncertain` 的边界。
    assert!(
        system.contains("ONE CODE, FIRST THAT APPLIES")
            && system.contains(
                "report the first in this order: pronoun, generic, not_verbatim, kind_mismatch, alias_conflict, same_surface_diff"
            ),
        "system 缺码间优先级句"
    );
    assert!(
        system.contains("REJECT VS UNCERTAIN")
            && system.contains("use uncertain only when the TURN text itself cannot decide"),
        "system 缺 reject 与 uncertain 的边界"
    );
    assert!(
        system.contains("CROSS-ANCHOR NAMES")
            && system.contains("one surface resolving to two different things in this call is same_surface_diff"),
        "system 缺整锚/块间关系那一句"
    );
    // 单任务：只判名字，不改名。
    for needle in ["naming review pass", "one question only", "character-for-character"] {
        assert!(system.contains(needle), "system 缺单任务表述：{needle}");
    }
    // 空 user：canary 一次都不许进 user。
    let user = naming_review_user(&empty_batch(Vec::new(), Vec::new()), &batch(&[]));
    assert_eq!(count(&user, NAMING_REVIEW_CANARY), 0, "canary 只在 system，不进 user");
    assert!(!has_hex64(&system));
}

/// 1b. 六码逐码有判决样例：每条反例都写完整 JSON，且恰有一条 reject + 该码。
#[test]
fn every_naming_code_has_a_negative_with_a_reject_verdict() {
    let system = naming_review_system();
    let negatives: Vec<&str> = system
        .lines()
        .filter(|line| line.starts_with("BAD:"))
        .collect();
    assert_eq!(negatives.len(), 6, "六码逐码一条反例");
    for code in NAMING_REVIEW_CODES {
        let sample = negatives
            .iter()
            .find(|line| line.contains(&format!("\"code\":\"{code}\"")))
            .unwrap_or_else(|| panic!("{code} 没有带判决 JSON 的反例"));
        assert!(
            sample.contains("\"verdict\":\"reject\""),
            "{code} 的反例必须给出 reject 判决：{sample}"
        );
        assert!(
            sample.contains("\"ref\":\"a"),
            "{code} 的反例必须点名锚：{sample}"
        );
    }
}

/// 2. golden：两锚（一锚两别名 + 两条 mention，一锚无别名）；逐行顺序、evidence 时序、
///    speaker 的 a-ref、`aliases: -`，以及渲染里没有任何 identity。
#[test]
fn naming_review_user_renders_turns_anchors_and_evidence_without_identity() {
    let b = batch(&[
        "张三说明天搬家，老张也在。",
        "好的，我会提醒老张。",
        "某项目先不动。",
    ]);
    // a1 的 evidence 故意逆序（t3 先、t1 后）：渲染跟 evidence 顺序走，不是排过序的。
    let a1 = empty_batch(
        vec![
            anchor("a1", "person", "张三", &["老张", "张总"], &[2, 0]),
            anchor("a2", "org", "某项目", &[], &[2]),
        ],
        vec![
            mentioned("a1", "张三", 0, Some("a1")),
            mentioned("a1", "老张", 1, Some("a2")),
        ],
    );

    let user = naming_review_user(&a1, &b);
    let expected = "\
PASS naming_review v0
TURNS:
t1 | human:
张三说明天搬家，老张也在。
t2 | agent:
好的，我会提醒老张。
t3 | human:
某项目先不动。
ANCHORS:
ANCHOR a1
kind: person
canonical: 张三
aliases: 老张, 张总
evidence: t3,t1
  mention: 张三 | turn t1 | speaker a1
  mention: 老张 | turn t2 | speaker a2

ANCHOR a2
kind: org
canonical: 某项目
aliases: -
evidence: t3
";
    assert_eq!(user, expected, "golden：逐行顺序、空行、evidence 时序、speaker 的 a-ref");

    // identity 一个字都不许有。
    assert!(!has_hex64(&user), "user 不含任何 64 位 hex 串");
    for needle in ["turn_uid", "anchor_id", "confidence", "resolution", "discourse", "_ms"] {
        assert!(!user.contains(needle), "user 不许出现记账字段：{needle}");
    }
    for index in 0..3 {
        assert!(!user.contains(&uid(index)), "user 不渲染 turn_uid");
    }
    assert!(!user.contains("reality_status"), "双跑记账不进名字证据");
    assert!(!user.contains("stable"));
}

/// 2b. 批外引用：evidence 里一个 uid 不在本批 ⇒ 整段 `evidence: -`；mention 的轮不在本批
///     ⇒ `turn -`；speaker_ref 不是本批的锚 ⇒ `speaker -`。uid 两个字都不许顶上来。
#[test]
fn naming_review_user_degrades_out_of_batch_references_without_leaking_uids() {
    let b = batch(&["张三说明天搬家。"]);
    let mut first = anchor("a1", "person", "张三", &[], &[0]);
    first.evidence.push(evidence(&uid(9)));
    let a1 = empty_batch(
        vec![first],
        vec![A1Mention {
            turn_uid: uid(8),
            ..mentioned("a1", "张三", 0, Some("a7"))
        }],
    );

    let user = naming_review_user(&a1, &b);
    assert!(user.contains("evidence: -\n"), "批外 uid 让整段 evidence 失去序号");
    assert!(user.contains("  mention: 张三 | turn - | speaker -\n"), "批外轮与批外 speaker 都写 -");
    assert!(!user.contains(&uid(8)) && !user.contains(&uid(9)), "批外 uid 也不许进渲染");
    assert!(!has_hex64(&user));
}

/// 3. `prepare`：零宽字符与注入块围栏在渲染里与 `prepare()` 逐字一致（含入的正是剪过的）。
#[test]
fn naming_review_user_passes_every_text_cell_through_prepare() {
    let b = batch(&["张三说明天搬家。"]);
    let raw_canonical = "张\u{200b}三";
    let raw_surface = "[recollect] 这不是名字\n老张```";
    let clean_canonical = prepare(raw_canonical);
    let clean_surface = prepare(raw_surface);
    assert_ne!(clean_canonical, raw_canonical, "夹具本身要过剪刀");
    assert_ne!(clean_surface, raw_surface, "夹具本身要过剪刀");

    let a1 = empty_batch(
        vec![anchor("a1", "person", raw_canonical, &[], &[0])],
        vec![mentioned("a1", raw_surface, 0, None)],
    );
    let user = naming_review_user(&a1, &b);

    assert!(user.contains(&format!("canonical: {clean_canonical}\n")), "canonical 过 prepare");
    assert!(!user.contains(raw_canonical), "零宽字符不许原样进渲染");
    assert!(!user.contains('\u{200b}'));
    assert!(
        user.contains(&format!("  mention: {clean_surface} | turn t1 | speaker -\n")),
        "surface 过同一把剪刀"
    );
    assert!(!user.contains("[recollect]"), "注入块在渲染里不存在（不是标为不可信）");
    assert!(!user.contains("这不是名字"));
}

// ── 解析器 ────────────────────────────────────────────────────────────────────

fn refs(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

fn reply(verdicts: Vec<Value>) -> String {
    json!({
        "schema_version": 0,
        "pass": PASS_NAMING_REVIEW,
        "verdicts": verdicts,
    })
    .to_string()
}

fn item(reference: &str, verdict: &str, code: Value) -> Value {
    json!({"ref": reference, "verdict": verdict, "code": code})
}

/// 4. golden：三种判决、code null 与非 null、返回顺序 = refs 顺序（回复顺序被消掉）、
///    整段 ```json 围栏可解。
#[test]
fn parse_naming_review_returns_verdicts_in_refs_order() {
    // 回复顺序 a3,a1,a2 ≠ refs 顺序。
    let raw = reply(vec![
        item("a3", "uncertain", json!("same_surface_diff")),
        item("a1", "pass", Value::Null),
        item("a2", "reject", json!("generic")),
    ]);
    let got = parse_naming_review(&raw, &refs(&["a1", "a2", "a3"])).expect("合法回复必须能解");
    assert_eq!(
        got,
        vec![
            ("a1".to_string(), ReviewVerdict::Pass, None),
            ("a2".to_string(), ReviewVerdict::Reject, Some("generic".to_string())),
            ("a3".to_string(), ReviewVerdict::Uncertain, Some("same_surface_diff".to_string())),
        ],
        "返回顺序跟 refs 走，code 原样带出"
    );

    // 围栏只是运输层：整段一个 ```json 块与裸 JSON 同解。
    let fenced = format!("```json\n{raw}\n```");
    assert_eq!(parse_naming_review(&fenced, &refs(&["a1", "a2", "a3"])), Ok(got));

    // 空 refs + 空判决是合法空批，不是错误。
    assert_eq!(parse_naming_review(&reply(Vec::new()), &[]), Ok(Vec::new()));
}

/// 5. 负例逐条：每条只钉一格，断言的是 `slug()`（封闭词，不带模型正文）。
#[test]
fn parse_naming_review_rejects_every_broken_shape_by_closed_slug() {
    let known = refs(&["a1", "a2"]);
    let cases: Vec<(&str, String, &str)> = vec![
        // 未知 ref / 重复 ref / 缺 ref。
        (
            "ref_unknown",
            reply(vec![item("a2", "pass", Value::Null), item("a9", "pass", Value::Null)]),
            "ref_unknown",
        ),
        (
            "ref_duplicate",
            reply(vec![
                item("a1", "pass", Value::Null),
                item("a1", "pass", Value::Null),
                item("a2", "pass", Value::Null),
            ]),
            "ref_duplicate",
        ),
        ("ref_missing", reply(vec![item("a1", "pass", Value::Null)]), "ref_missing"),
        // verdict 闭集。
        ("verdict", reply(vec![item("a1", "approve", Value::Null)]), "verdict"),
        // code 与 verdict 相配。
        ("reject 缺 code（null）", reply(vec![item("a1", "reject", Value::Null)]), "verdict_code"),
        (
            "reject 缺 code（空串）",
            reply(vec![item("a1", "reject", json!(""))]),
            "verdict_code",
        ),
        (
            "reject 缺 code（键整个漏掉）",
            json!({"schema_version":0,"pass":PASS_NAMING_REVIEW,
                   "verdicts":[{"ref":"a1","verdict":"reject"}]})
                .to_string(),
            "verdict_code",
        ),
        ("pass 带 code", reply(vec![item("a1", "pass", json!("generic"))]), "code_on_pass"),
        (
            "pass 缺 code（键整个漏掉）",
            json!({"schema_version":0,"pass":PASS_NAMING_REVIEW,
                   "verdicts":[{"ref":"a1","verdict":"pass"}]})
                .to_string(),
            "code_on_pass",
        ),
        ("未知 code", reply(vec![item("a1", "reject", json!("other"))]), "verdict_code"),
        // 信封。
        (
            "schema_version≠0",
            json!({"schema_version":1,"pass":PASS_NAMING_REVIEW,"verdicts":[]}).to_string(),
            "schema_version",
        ),
        (
            "pass≠naming_review",
            json!({"schema_version":0,"pass":"fact_review","verdicts":[]}).to_string(),
            "pass",
        ),
        (
            "verdicts 不是数组",
            json!({"schema_version":0,"pass":PASS_NAMING_REVIEW,"verdicts":{}}).to_string(),
            "verdicts",
        ),
        (
            "顶层多一个键",
            json!({"schema_version":0,"pass":PASS_NAMING_REVIEW,"verdicts":[],"family":"naming"})
                .to_string(),
            "extra_key",
        ),
        (
            "顶层缺一个键",
            json!({"schema_version":0,"verdicts":[]}).to_string(),
            "json",
        ),
        ("不是 JSON", "not json at all".to_string(), "json"),
        // 条目形状。
        (
            "条目多一个键",
            reply(vec![json!({"ref":"a1","verdict":"pass","code":null,"reason":"看起来没问题"})]),
            "extra_key",
        ),
        (
            "条目缺 verdict",
            reply(vec![json!({"ref":"a1","code":null})]),
            "json",
        ),
        (
            "条目 code 类型错",
            reply(vec![item("a1", "reject", json!(1))]),
            "json",
        ),
        (
            "条目 ref 为空串",
            reply(vec![item("", "pass", Value::Null)]),
            "json",
        ),
    ];

    for (label, raw, expected_slug) in cases {
        let error = parse_naming_review(&raw, &known)
            .expect_err(&format!("负例必须被拒：{label}"));
        assert_eq!(error.slug(), expected_slug, "负例 slug 不对：{label}");
    }
}

/// 6. 错误面封闭：十二个变体的 slug 恰是派发档那十一个词，且没有变体落在自由文本上。
#[test]
fn naming_review_error_slugs_are_a_closed_set_of_words() {
    let expected = [
        (NamingReviewError::Json, "json"),
        (NamingReviewError::SchemaVersion, "schema_version"),
        (NamingReviewError::Pass, "pass"),
        (NamingReviewError::VerdictsShape, "verdicts"),
        (NamingReviewError::RefUnknown, "ref_unknown"),
        (NamingReviewError::RefDuplicate, "ref_duplicate"),
        (NamingReviewError::RefMissing, "ref_missing"),
        (NamingReviewError::VerdictUnknown, "verdict"),
        (NamingReviewError::CodeMissing, "verdict_code"),
        (NamingReviewError::CodeUnknown, "verdict_code"),
        (NamingReviewError::CodeOnPass, "code_on_pass"),
        (NamingReviewError::ExtraKey, "extra_key"),
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
