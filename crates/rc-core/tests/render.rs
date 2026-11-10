//! 20K 常驻简报：分区顺序、相对时间、token 近似、区上限与总预算淘汰（设计 §9.1/§9.3），
//! 以及 `store.brief` 写 `core_injection_log` 的读数。

use rc_core::render::{CAP_PERSONA, SEC_HANDOFF, SEC_PERSONA};
use rc_core::{
    estimate_tokens, render_brief, render_brief_with, render_delta, render_due, Affect,
    CoreError, CorePayload, CoreStore, HandoffItem, LoopItem, Persona, PinnedEpisode,
    Relationship, Subject, DEFAULT_INCLUDE,
};
use rusqlite::Connection;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;

/// `core_injection_log` 最后一行的投影（clippy 的 `type_complexity` 只认有名字的元组）。
type InjectionRow = (String, String, Option<String>, i64, i64, String, Option<i64>, Option<String>);

const NOW: i64 = 1_700_000_000_000;
const MINUTE: i64 = 60_000;
const HOUR: i64 = 3_600_000;
const DAY: i64 = 86_400_000;
const BUDGET: usize = 20_000;

const HEADINGS: [&str; 8] = [
    "[L0 安全与身份]",
    "[L1 人格核心]",
    "[主用户画像]",
    "[关系与交互]",
    "[情绪状态]",
    "[未闭环]",
    "[pinned 共同经历]",
    "[近期上下文]",
];

fn rich_payload() -> CorePayload {
    let mut p = CorePayload::empty("agent-a");
    p.l0 = vec![
        "边界：不冒充人类，不假装有身体".to_string(),
        "诚实：不确定就说不确定".to_string(),
    ];
    p.persona = Persona {
        core_text: "慢热，但记得小事".to_string(),
        traits: [
            ("warmth".to_string(), json!(0.7)),
            ("verbosity".to_string(), json!("low")),
        ]
        .into_iter()
        .collect(),
        style: [("punct".to_string(), json!("…"))].into_iter().collect(),
        quirks: vec!["喜欢用比喻".to_string()],
        boundaries: vec!["不替用户做决定".to_string()],
        voice_examples: vec![
            "你回来了。".to_string(),
            "慢慢说，我在。".to_string(),
            "这条不该被渲染".to_string(),
        ],
    };
    p.subjects = vec![Subject {
        subject_id: "user-main".to_string(),
        is_primary: true,
        profile_text: "夜猫子，做后端".to_string(),
        preferences: [("tea".to_string(), json!("oolong"))].into_iter().collect(),
        triggers: vec!["被忽略".to_string()],
        taboos: vec!["公开隐私".to_string()],
    }];
    p.interaction.last_user_message_at_ms = NOW - 5 * MINUTE;
    p.interaction.last_agent_initiated_at_ms = NOW - 2 * DAY;
    p.interaction.unanswered_user_messages = 2;
    p.interaction.attention_debt_ms = 30 * MINUTE;
    p.interaction.current_topic = "周末露营".to_string();
    p.relationship = Relationship {
        trust: 0.8,
        intimacy: 0.6,
        tension: 0.1,
        status: "normal".to_string(),
        text: "最近有点安静".to_string(),
        confidence: 0.7,
    };
    p.affects = vec![
        Affect {
            subject_kind: "user".to_string(),
            label: "疲惫".to_string(),
            valence: -0.4,
            arousal: 0.2,
            energy: 0.3,
            confidence: 0.8,
            valid_from_ms: NOW - 3 * HOUR,
            cause: "加班".to_string(),
        },
        Affect {
            subject_kind: "agent".to_string(),
            label: "专注".to_string(),
            valence: 0.3,
            arousal: 0.4,
            energy: 0.7,
            confidence: 0.6,
            valid_from_ms: NOW - 30 * MINUTE,
            cause: String::new(),
        },
    ];
    p.loops = vec![
        LoopItem {
            id: "loop-1".to_string(),
            kind: "promise".to_string(),
            text: "下次带那本书".to_string(),
            status: "open".to_string(),
            due_at_ms: NOW + 2 * DAY,
        },
        LoopItem {
            id: "loop-2".to_string(),
            kind: "topic".to_string(),
            text: "还没聊完的搬家".to_string(),
            status: "open".to_string(),
            due_at_ms: 0,
        },
        LoopItem {
            id: "loop-3".to_string(),
            kind: "conflict".to_string(),
            text: "已关闭的事".to_string(),
            status: "closed".to_string(),
            due_at_ms: 0,
        },
    ];
    p.pinned_episodes = vec![PinnedEpisode {
        episode_id: "ep-1".to_string(),
        pin_reason: "第一次一起熬夜".to_string(),
    }];
    p
}

#[test]
fn chinese_expression_switch_is_off_by_default_and_rendered_when_on() {
    let mut p = CorePayload::empty("agent-a");
    let off = render_brief(&p, "user-1", "daily", 0, NOW);
    assert!(!off.text.contains("[中文表达优化]"), "默认关闭却渲染了：{}", off.text);

    p.chinese_expression.enabled = true;
    let on = render_brief(&p, "user-1", "daily", 0, NOW);
    assert!(on.text.contains("[中文表达优化]"), "{}", on.text);
    assert!(on.text.contains("该用“就”的地方要写出来"), "{}", on.text);
    assert!(on.text.contains("不要为了压缩把它删掉"), "{}", on.text);
    assert!(on.text.contains("尽量在短句里也把主语写出来"), "{}", on.text);
    assert!(on.text.contains("不要用说明书式的书面语言"), "{}", on.text);
    assert!(on.text.contains("建议用英文和英文符号"), "{}", on.text);
    assert!(on.text.contains("路径本来就是中文"), "{}", on.text);
    assert!(!on.text.contains("一律用英文"), "路径可能是中文，不能写死“一律”：{}", on.text);
    assert!(!on.text.contains("先给结论"), "判断/推理纪律不该混进中文表达规则：{}", on.text);
    assert!(on.section_tokens.contains_key(rc_core::SEC_CHINESE_EXPRESSION));
}

#[test]
fn an_empty_payload_renders_no_sections_and_nonempty_ones_keep_the_fixed_order() {
    // 空载荷：只有头部，一个区都不输出。
    let empty = render_brief(&CorePayload::empty("agent-a"), "user-1", "daily", 0, NOW);
    assert!(empty.text.starts_with("[常驻简报]"));
    for heading in HEADINGS {
        assert!(!empty.text.contains(heading), "empty payload rendered {heading}");
    }
    assert!(empty.section_tokens.is_empty());
    assert!(empty.truncated.is_empty());
    assert!(empty.token_estimate >= 1);

    // 只有一个非空区：只出现那一个。
    let mut l0_only = CorePayload::empty("agent-a");
    l0_only.l0 = vec!["边界：不冒充人类".to_string()];
    let brief = render_brief(&l0_only, "user-1", "daily", 0, NOW);
    assert!(brief.text.contains(HEADINGS[0]));
    for heading in &HEADINGS[1..] {
        assert!(!brief.text.contains(heading), "unexpected {heading}");
    }

    // 全非空：8 个区按固定顺序出现。
    let brief = render_brief(&rich_payload(), "user-1", "daily", 0, NOW);
    let mut last = 0usize;
    for heading in HEADINGS {
        let at = brief
            .text
            .find(heading)
            .unwrap_or_else(|| panic!("missing {heading} in:\n{}", brief.text));
        assert!(at >= last, "{heading} is out of order");
        last = at;
    }
    // 语气样例只放前 2 条；已关闭的 loop 不渲染。
    assert!(brief.text.contains("语气样例2"));
    assert!(!brief.text.contains("语气样例3"));
    assert!(!brief.text.contains("已关闭的事"));
    // section_tokens 与实际渲染的区一一对应（BTreeMap 按键排序，不是渲染顺序）。
    let mut keys: Vec<&str> = brief.section_tokens.keys().copied().collect();
    keys.sort_unstable();
    let mut expected = vec![
        "affect",
        "l0",
        "loops",
        "persona",
        "pinned",
        "recent",
        "relationship",
        "subject",
    ];
    expected.sort_unstable();
    assert_eq!(keys, expected);
    assert!(brief.token_estimate <= BUDGET);
}

#[test]
fn relative_time_covers_just_now_minutes_hours_days_and_ahead() {
    assert_eq!(render_delta(NOW, NOW - 10_000), "刚刚");
    assert_eq!(render_delta(NOW, NOW - 5 * MINUTE), "5 分钟前");
    assert_eq!(render_delta(NOW, NOW - 3 * HOUR), "3 小时前");
    assert_eq!(render_delta(NOW, NOW - 2 * DAY), "2 天前");
    assert_eq!(render_delta(NOW, NOW + 2 * DAY), "还有 2 天");
    assert_eq!(render_delta(NOW, NOW + 3 * HOUR), "还有 3 小时");

    // 截止时间：未到期"还有"，已到期"已过期"。
    assert_eq!(render_due(NOW, NOW + 2 * DAY), "还有 2 天");
    assert_eq!(render_due(NOW, NOW - 3 * DAY), "已过期 3 天");

    // 未填时间戳说"未知"，绝不甩 epoch。
    assert_eq!(render_delta(NOW, 0), "未知");
    assert_eq!(render_due(NOW, 0), "未知");
}

#[test]
fn empty_interaction_fields_say_unknown() {
    let mut p = CorePayload::empty("agent-a");
    p.relationship.text = "熟悉".to_string();
    let brief = render_brief(&p, "user-1", "daily", 0, NOW);
    assert!(brief.text.contains("[关系与交互]"), "{}", brief.text);
    assert!(brief.text.contains("上次消息：未知"), "{}", brief.text);
    assert!(brief.text.contains("上次主动：未知"), "{}", brief.text);
    assert!(brief.text.contains("当前话题：未知"), "{}", brief.text);
    assert!(!brief.text.contains("1970"), "{}", brief.text);
    assert!(!brief.text.contains("[近期上下文]"), "{}", brief.text);
}

#[test]
fn token_estimate_is_ascii_quarters_plus_one_per_wide_char() {
    assert_eq!(estimate_tokens(""), 0);
    // 非空至少 1：3 个 ASCII 字符凑不满 1 token 也记 1。
    assert_eq!(estimate_tokens("abc"), 1);
    assert_eq!(estimate_tokens(&"a".repeat(400)), 100);
    assert_eq!(estimate_tokens(&"中".repeat(100)), 100);
    assert_eq!(estimate_tokens("abcd中文"), 3);
    assert_eq!(estimate_tokens("abcdefgh中"), 3);
}

#[test]
fn a_section_over_its_cap_is_truncated_and_the_total_stays_within_budget() {
    let mut p = CorePayload::empty("agent-a");
    p.persona.core_text = "慢热".to_string();
    p.persona.quirks = (0..120)
        .map(|i| format!("癖好 {i}：{}", "细节".repeat(30)))
        .collect();

    let brief = render_brief(&p, "user-1", "daily", BUDGET, NOW);
    assert!(
        brief.truncated.contains(&SEC_PERSONA),
        "truncated: {:?}",
        brief.truncated
    );
    let persona_tokens = brief.section_tokens[SEC_PERSONA];
    assert!(
        persona_tokens <= CAP_PERSONA,
        "persona {persona_tokens} > cap {CAP_PERSONA}"
    );
    let kept = brief.text.matches("- 癖好").count();
    assert!(kept < 120, "tail entries should be dropped, kept {kept}");
    assert!(brief.token_estimate <= BUDGET, "{}", brief.token_estimate);

    // 单条超长行：不是丢行，而是截断并留省略号。
    let mut p = CorePayload::empty("agent-a");
    p.persona.quirks = vec!["很长的癖好".repeat(1_000)];
    let brief = render_brief(&p, "user-1", "daily", BUDGET, NOW);
    assert!(brief.text.contains('…'), "a long line should be cut");
    assert!(brief.truncated.contains(&SEC_PERSONA));
    assert!(brief.section_tokens[SEC_PERSONA] <= CAP_PERSONA);
    assert!(brief.token_estimate <= BUDGET);
}

#[test]
fn shrinking_the_budget_evicts_pinned_first_and_keeps_l0() {
    let p = rich_payload();
    let full = render_brief(&p, "user-1", "daily", BUDGET, NOW);
    assert!(full.text.contains("[pinned 共同经历]"), "{}", full.text);
    assert!(full.truncated.is_empty(), "{:?}", full.truncated);

    let tight = render_brief(&p, "user-1", "daily", full.token_estimate - 1, NOW);
    assert!(
        !tight.text.contains("[pinned 共同经历]"),
        "pinned must be evicted first:\n{}",
        tight.text
    );
    assert!(
        tight.text.contains("[L0 安全与身份]"),
        "L0 must survive:\n{}",
        tight.text
    );
    assert!(tight.truncated.contains(&"pinned"), "{:?}", tight.truncated);
    assert!(tight.token_estimate < full.token_estimate);
    // 与上一句同一命题的反向写法：clippy 的 `int_plus_one` 要求 `x <= y - 1` 写成 `x < y`
    // （断言强度一字不变，只是把"严格小于"换个方向再说一次）。
    assert!(full.token_estimate > tight.token_estimate);
}

#[test]
fn a_tiny_budget_still_keeps_l0_intact() {
    let mut p = CorePayload::empty("agent-a");
    p.l0 = vec![
        "边界：不冒充人类".to_string(),
        "诚实：不确定就说不确定".to_string(),
    ];
    p.persona.core_text = "这句应该被淘汰".to_string();

    let brief = render_brief(&p, "user-1", "daily", 1, NOW);
    assert!(brief.text.contains("[L0 安全与身份]"), "{}", brief.text);
    assert!(!brief.truncated.contains(&"l0"), "{:?}", brief.truncated);
    assert!(brief.text.contains("不冒充人类"), "{}", brief.text);
    assert!(brief.text.contains("不确定就说不确定"), "{}", brief.text);
    assert!(!brief.text.contains("这句应该被淘汰"), "{}", brief.text);
}

fn db(dir: &Path) -> Connection {
    Connection::open(dir.join("core.db")).unwrap()
}

fn injection_rows(dir: &Path) -> i64 {
    db(dir)
        .query_row("SELECT COUNT(*) FROM core_injection_log", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn brief_writes_one_injection_log_row_with_parseable_readings() {
    let dir = tempfile::tempdir().unwrap();
    let store = CoreStore::open(dir.path()).unwrap();
    store.register_agent("agent-a", 0).unwrap();
    let mut payload = rich_payload();
    payload.agent_id = "agent-a".to_string();
    store.save_doc("agent-a", &payload, "dev-1", 1).unwrap();

    let brief = store
        .brief("agent-a", "user-1", "daily", BUDGET, Some("sess-1"), NOW)
        .unwrap();
    assert_eq!(injection_rows(dir.path()), 1);

    let (agent_id, user_id, session_key, assembled_at_ms, token_estimate, sections, cache_hit, truncated):
        InjectionRow = db(dir.path())
        .query_row(
            "SELECT agent_id, user_id, session_key, assembled_at_ms, token_estimate, \
             section_tokens_json, cache_hit, truncated_json FROM core_injection_log ORDER BY id \
             DESC LIMIT 1",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            },
        )
        .unwrap();

    assert_eq!(agent_id, "agent-a");
    assert_eq!(user_id, "user-1");
    assert_eq!(session_key.as_deref(), Some("sess-1"));
    assert_eq!(assembled_at_ms, NOW);
    assert_eq!(token_estimate as usize, brief.token_estimate);
    assert_eq!(cache_hit, None);

    let logged: BTreeMap<String, usize> = serde_json::from_str(&sections).unwrap();
    let expected: BTreeMap<String, usize> = brief
        .section_tokens
        .iter()
        .map(|(k, v)| ((*k).to_string(), *v))
        .collect();
    assert_eq!(logged, expected);

    let truncated: Vec<String> = serde_json::from_str(truncated.as_deref().unwrap()).unwrap();
    let expected: Vec<String> = brief.truncated.iter().map(|k| (*k).to_string()).collect();
    assert_eq!(truncated, expected);
}

#[test]
fn a_registered_agent_without_a_document_gets_an_empty_brief() {
    let dir = tempfile::tempdir().unwrap();
    let store = CoreStore::open(dir.path()).unwrap();
    store.register_agent("agent-b", 0).unwrap();

    let brief = store
        .brief("agent-b", "user-1", "daily", BUDGET, None, NOW)
        .unwrap();
    assert!(brief.text.starts_with("[常驻简报]"));
    assert!(brief.section_tokens.is_empty());
    assert!(brief.truncated.is_empty());
    assert_eq!(injection_rows(dir.path()), 1);

    let (session_key, token_estimate): (Option<String>, i64) = db(dir.path())
        .query_row(
            "SELECT session_key, token_estimate FROM core_injection_log ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(session_key, None);
    assert_eq!(token_estimate as usize, brief.token_estimate);
}

/// 一张交接条。`id` 只要唯一（渲染不查唯一性，`validate` 才查），正文用调用方给的。
fn handoff(kind: &str, text: &str, created_at_ms: i64, expires_at_ms: i64) -> HandoffItem {
    HandoffItem {
        id: format!("h-{kind}-{created_at_ms}"),
        kind: kind.to_string(),
        text: text.to_string(),
        created_at_ms,
        expires_at_ms,
        ..HandoffItem::default()
    }
}

#[test]
fn handoff_section_shows_continue_by_default() {
    assert_eq!(DEFAULT_INCLUDE, ["continue", "due"]);

    let mut p = rich_payload();
    p.handoffs = vec![handoff(
        "continue",
        "从 C4 开始：先跑 ops 用例",
        NOW - 2 * HOUR,
        NOW + 7 * DAY,
    )];
    let brief = render_brief(&p, "user-1", "daily", 0, NOW);
    assert!(brief.text.contains("[交接（给下个会话）]"), "{}", brief.text);
    assert!(brief.text.contains("从 C4 开始：先跑 ops 用例"), "{}", brief.text);
    assert!(brief.text.contains("- 接着做（2 小时前写下"), "{}", brief.text);
    assert!(brief.text.contains("还有 7 天）"), "{}", brief.text);
    assert!(
        brief.section_tokens.contains_key(SEC_HANDOFF),
        "{:?}",
        brief.section_tokens
    );

    // 交接段在「未闭环」之后、「pinned」之前：易变状态靠后，淘汰先走 recent → pinned → handoff。
    let loops = brief.text.find("[未闭环]").expect("loops section");
    let handoffs = brief.text.find("[交接（给下个会话）]").expect("handoff section");
    let pinned = brief.text.find("[pinned 共同经历]").expect("pinned section");
    assert!(loops < handoffs && handoffs < pinned, "{}", brief.text);
}

#[test]
fn a_note_never_shows_without_include() {
    let mut p = CorePayload::empty("agent-a");
    p.handoffs = vec![handoff("note", "钥匙箱在第二格", NOW - HOUR, NOW + 7 * DAY)];

    let by_default = render_brief(&p, "user-1", "daily", 0, NOW);
    assert!(!by_default.text.contains("钥匙箱在第二格"), "{}", by_default.text);
    assert!(
        !by_default.section_tokens.contains_key(SEC_HANDOFF),
        "{:?}",
        by_default.section_tokens
    );

    let with_note = render_brief_with(&p, "user-1", "daily", 0, &["continue", "due", "note"], NOW);
    assert!(with_note.text.contains("钥匙箱在第二格"), "{}", with_note.text);
    assert!(with_note.text.contains("[交接（给下个会话）]"), "{}", with_note.text);
    assert!(with_note.text.contains("- 备注：钥匙箱在第二格"), "{}", with_note.text);
}

#[test]
fn a_due_item_appears_only_within_a_day() {
    let mut p = CorePayload::empty("agent-a");
    p.handoffs = vec![handoff("due", "交付验收脚本", NOW - DAY, NOW + 3 * DAY)];

    let far = render_brief(&p, "user-1", "daily", 0, NOW);
    assert!(!far.text.contains("交付验收脚本"), "三天后到期不该出现：{}", far.text);

    p.handoffs[0].expires_at_ms = NOW + 2 * HOUR;
    let near = render_brief(&p, "user-1", "daily", 0, NOW);
    assert!(near.text.contains("交付验收脚本"), "两小时后到期要出现：{}", near.text);
    assert!(near.text.contains("- 到期（还有 2 小时）"), "{}", near.text);

    // 已过期即消失（可见性要求 `expires_at_ms > now_ms`）。
    p.handoffs[0].expires_at_ms = NOW - HOUR;
    let gone = render_brief(&p, "user-1", "daily", 0, NOW);
    assert!(!gone.text.contains("交付验收脚本"), "过期条不该出现：{}", gone.text);
    assert!(!gone.section_tokens.contains_key(SEC_HANDOFF), "{:?}", gone.section_tokens);
}

#[test]
fn closed_and_expired_handoffs_never_render() {
    let mut closed = handoff("continue", "已关掉的接着做", NOW - DAY, NOW + 7 * DAY);
    closed.closed_at_ms = Some(NOW - HOUR);
    closed.closed_reason = Some("done".to_string());
    let expired = handoff("continue", "已过期的接着做", NOW - 8 * DAY, NOW - DAY);

    let mut p = CorePayload::empty("agent-a");
    p.handoffs = vec![closed, expired];
    let brief = render_brief(&p, "user-1", "daily", 0, NOW);
    assert!(!brief.text.contains("已关掉的接着做"), "{}", brief.text);
    assert!(!brief.text.contains("已过期的接着做"), "{}", brief.text);
    assert!(
        !brief.section_tokens.contains_key(SEC_HANDOFF),
        "{:?}",
        brief.section_tokens
    );
}

#[test]
fn only_handoffs_still_render_a_brief() {
    // `CorePayload::empty` 加一条 continue：载荷不算空，简报要有头部与交接段。
    let mut p = CorePayload::empty("agent-a");
    p.handoffs = vec![handoff("continue", "先读工单再动手", NOW - 2 * HOUR, NOW + 7 * DAY)];
    let brief = render_brief(&p, "user-1", "daily", 0, NOW);
    assert!(brief.text.starts_with("[常驻简报]"), "{}", brief.text);
    assert!(brief.text.contains("[交接（给下个会话）]"), "{}", brief.text);
    assert!(brief.text.contains("先读工单再动手"), "{}", brief.text);
    assert!(brief.section_tokens.contains_key(SEC_HANDOFF), "{:?}", brief.section_tokens);
}

#[test]
fn include_filters_kinds() {
    let mut p = CorePayload::empty("agent-a");
    p.handoffs = vec![
        handoff("continue", "接着做的那条", NOW - HOUR, NOW + 7 * DAY),
        handoff("note", "备注的那条", NOW - HOUR, NOW + 7 * DAY),
    ];
    let only_note = render_brief_with(&p, "user-1", "daily", 0, &["note"], NOW);
    assert!(only_note.text.contains("备注的那条"), "{}", only_note.text);
    assert!(!only_note.text.contains("接着做的那条"), "{}", only_note.text);
    assert!(!only_note.text.contains("- 接着做"), "{}", only_note.text);
}

#[test]
fn unregistered_or_unsafe_agents_are_refused_before_any_log_row() {
    let dir = tempfile::tempdir().unwrap();
    let store = CoreStore::open(dir.path()).unwrap();
    assert!(matches!(
        store.brief("agent-a", "user-1", "daily", BUDGET, None, NOW),
        Err(CoreError::Unregistered)
    ));
    assert!(matches!(
        store.brief("bad/agent", "user-1", "daily", BUDGET, None, NOW),
        Err(CoreError::BadAgentId)
    ));
    assert_eq!(injection_rows(dir.path()), 0);
}

// ── 「上一棒交接」段（W34 续接包）：hook brief 的固定段 ──────────────────────

use rc_core::render::{render_handoff_prev, CAP_HANDOFF_PREV, SEC_HANDOFF_PREV};

#[test]
fn handoff_prev_有未闭条目_段出现且含条目() {
    let items = vec![
        handoff("continue", "把水位闸的读数补进台账", NOW - DAY, NOW + DAY),
        handoff("note", "这周先不动白名单", NOW - 2 * HOUR, NOW + DAY),
    ];
    let got = render_handoff_prev(&items, &[], NOW);
    assert!(got.text.starts_with("[上一棒交接]"), "{}", got.text);
    assert!(got.text.contains("把水位闸的读数补进台账"), "{}", got.text);
    assert!(got.text.contains("- 接着做"), "{}", got.text);
    assert!(got.text.contains("1 天前写下"), "{}", got.text);
    assert!(got.text.contains("- 备注：这周先不动白名单"), "{}", got.text);
    assert!(!got.truncated);
}

#[test]
fn handoff_prev_无条目无读数_段整体缺席() {
    // 阴性臂：沉默匹配视图内容——缺席的是整段，不是空标题。
    let got = render_handoff_prev(&[], &[], NOW);
    assert!(got.text.is_empty());
    assert!(!got.truncated);
}

#[test]
fn handoff_prev_只有读数_段只有读数行() {
    let got = render_handoff_prev(
        &[],
        &["- 项目状态：审核队列待审 3 条".to_string()],
        NOW,
    );
    assert_eq!(got.text, "[上一棒交接]\n- 项目状态：审核队列待审 3 条");
}

#[test]
fn handoff_prev_空白读数行被丢_全空仍缺席() {
    // payload_is_blank 类空判据的新段覆盖面：空白行不把段"撑出场"。
    let got = render_handoff_prev(&[], &["   ".to_string(), "\t".to_string()], NOW);
    assert!(got.text.is_empty());
}

#[test]
fn handoff_prev_已过期未闭条目照出_并标已过期() {
    // `handoff_list` 含已过期（过期与否交给工具面标）：这一段的标记就是 render_due 的
    // "已过期 N 天"，不是把行滤掉。
    let items = vec![handoff("due", "到期没关的那条", NOW - DAY, NOW - HOUR)];
    let got = render_handoff_prev(&items, &[], NOW);
    assert!(got.text.contains("已过期"), "{}", got.text);
    assert!(got.text.contains("到期没关的那条"), "{}", got.text);
}

#[test]
fn handoff_prev_超区上限_尾部截断并记truncated() {
    // 20 条 200 字符的 continue ≈ 远超 CAP_HANDOFF_PREV(600)：截断走 Section::push 的
    // 既有机制（从尾部丢、末行截短加省略号），truncated 原样交回。
    let items: Vec<_> = (0..20)
        .map(|i| {
            handoff(
                "continue",
                &format!("第 {i} 条：{}", "待办事项正文".repeat(40)),
                NOW - i64::from(i) * MINUTE,
                NOW + DAY,
            )
        })
        .collect();
    let got = render_handoff_prev(&items, &[], NOW);
    assert!(got.truncated, "20 条长文必然触顶");
    assert!(got.text.starts_with("[上一棒交接]"), "{}", got.text.chars().take(30).collect::<String>());
    assert!(got.text.chars().count() < items.len() * 210, "截断后不该装下全部条目");
}

#[test]
fn handoff_prev_区名与上限常量() {
    // 命名避让的钉子：这一段的区名不是 SEC_HANDOFF，也不是 W36 的 notebook。
    assert_eq!(SEC_HANDOFF_PREV, "handoff_prev");
    assert_ne!(SEC_HANDOFF_PREV, SEC_HANDOFF);
    assert_eq!(CAP_HANDOFF_PREV, 600);
}

// ── 小本本索引栏（W36 细案 §4）：core_brief 的固定段 ─────────────────────────

use rc_core::ops::NotebookEntry;
use rc_core::render::{render_brief_with_notebook, CAP_NOTEBOOK};

/// 一条索引行。`id` 只要形状像句柄；渲染不查存在性。
fn entry(id: &str, title: &str, tags: &[&str], created_at_ms: i64) -> NotebookEntry {
    NotebookEntry {
        id: id.to_string(),
        title: title.to_string(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        created_at_ms,
    }
}

#[test]
fn notebook_有行_段出现且含_id_标题_标签_相对时间() {
    let entries = vec![
        entry("n-0123456789ab", "炉头清单", &["露营", "装备"], NOW - HOUR),
        entry("n-ffffffffffff", "", &[], NOW - 2 * DAY),
    ];
    let brief = render_brief_with_notebook(
        &CorePayload::empty("agent-a"),
        "user-1",
        "daily",
        BUDGET,
        &DEFAULT_INCLUDE,
        &entries,
        None,
        NOW,
    );
    assert!(brief.text.contains("[小本本]"), "{}", brief.text);
    // 行形：`- {id}｜{title}｜{tags 逗号}｜{相对时间}`，新的在前。
    assert!(brief.text.contains("- n-0123456789ab｜炉头清单｜露营,装备｜1 小时前"), "{}", brief.text);
    // 空标题不消失：占位说法，行仍在。
    assert!(brief.text.contains("- n-ffffffffffff｜（无标题）｜｜2 天前"), "{}", brief.text);
    assert!(brief.section_tokens.contains_key(rc_core::render::SEC_NOTEBOOK));
    assert!(brief.truncated.is_empty(), "{:?}", brief.truncated);

    // 区序：交接段之后、pinned 之前。
    let mut p = rich_payload();
    p.handoffs = vec![handoff("continue", "接着做的那条", NOW - HOUR, NOW + 7 * DAY)];
    let brief = render_brief_with_notebook(&p, "user-1", "daily", BUDGET, &DEFAULT_INCLUDE, &entries, None, NOW);
    let handoffs = brief.text.find("[交接（给下个会话）]").expect("handoff");
    let notebook = brief.text.find("[小本本]").expect("notebook");
    let pinned = brief.text.find("[pinned 共同经历]").expect("pinned");
    assert!(handoffs < notebook && notebook < pinned, "{}", brief.text);
}

#[test]
fn notebook_空_整段缺席_旧签名也不受影响() {
    // 沉默匹配视图内容：无条目 ⇒ 区零行 ⇒ 段整体缺席（不是空标题）。
    let empty_payload = CorePayload::empty("agent-a");
    let brief = render_brief_with_notebook(
        &empty_payload, "user-1", "daily", BUDGET, &DEFAULT_INCLUDE, &[], None, NOW,
    );
    assert!(!brief.text.contains("[小本本]"), "{}", brief.text);
    assert!(!brief.section_tokens.contains_key(rc_core::render::SEC_NOTEBOOK));
    assert!(brief.truncated.is_empty());

    // 旧签名（不带 notebook 参数）以空 vec 委托：输出不含小本本段。
    let old = render_brief_with(&empty_payload, "user-1", "daily", BUDGET, &DEFAULT_INCLUDE, NOW);
    assert!(!old.text.contains("[小本本]"), "{}", old.text);

    // 有 notebook 行 ⇒ 载荷不算空：只写小本本的新 agent 不再只有头部。
    let entries = vec![entry("n-0123456789ab", "炉头清单", &["露营"], NOW - HOUR)];
    let only_notes = render_brief_with_notebook(
        &CorePayload::empty("agent-a"),
        "user-1",
        "daily",
        BUDGET,
        &DEFAULT_INCLUDE,
        &entries,
        None,
        NOW,
    );
    assert!(only_notes.text.contains("[小本本]"), "{}", only_notes.text);
    assert!(only_notes.text.contains("炉头清单"), "{}", only_notes.text);
}

#[test]
fn notebook_超区上限_truncated_记_notebook() {
    // 40 条 60 字符标题 ≈ 远超 CAP_NOTEBOOK(600)：截断走 Section::push 的既有机制，
    // truncated 记区名 notebook，且不越尺。
    let entries: Vec<_> = (0..40)
        .map(|i| {
            entry(
                &format!("n-{i:012x}"),
                &format!("第 {i} 条：{}", "待办事项标题".repeat(20)),
                &[],
                NOW - i64::from(i),
            )
        })
        .collect();
    let brief = render_brief_with_notebook(
        &CorePayload::empty("agent-a"),
        "user-1",
        "daily",
        BUDGET,
        &DEFAULT_INCLUDE,
        &entries,
        None,
        NOW,
    );
    assert!(
        brief.truncated.contains(&rc_core::render::SEC_NOTEBOOK),
        "truncated: {:?}",
        brief.truncated
    );
    let tokens = brief.section_tokens[rc_core::render::SEC_NOTEBOOK];
    assert!(tokens <= CAP_NOTEBOOK, "notebook {tokens} > cap {CAP_NOTEBOOK}");
    // 尾部的（旧的）条目被丢：首条新的在场，最后几条不在。
    assert!(brief.text.contains("第 0 条"), "{}", brief.text);
    assert!(!brief.text.contains(&format!("第 {} 条", entries.len() - 1)), "{}", brief.text);
    // 常量钉（已拍板：CAP=600）。
    assert_eq!(CAP_NOTEBOOK, 600);
}

// ── 核心记忆"上次更新"自我提醒（core.db 治理小刀②）───────────────────────────
//
// brief 组装时若核心文档有"上次更新"时间，头部多一行"核心记忆上次更新：X 天前"——
// 让 agent 自己看见自己多久没写，不是替它写。`None`（无文档/从未更新）⇒ 一行不加，
// 不造假数据。相对时间用 `render_delta`（唯一实现住 rc-contract：两张面同一句话）。

#[test]
fn the_brief_reminds_how_long_since_the_core_doc_was_last_written() {
    let p = rich_payload();
    let brief = render_brief_with_notebook(
        &p, "user-1", "daily", BUDGET, &DEFAULT_INCLUDE, &[], Some(NOW - 5 * DAY), NOW,
    );
    assert!(brief.text.contains("核心记忆上次更新：5 天前"), "{}", brief.text);
    // 位置在头部块内（第一个分区标题之前），不挤占任何内容分区。
    let header_end = brief.text.find("[L0 安全与身份]").expect("l0 heading");
    let reminder = brief.text.find("核心记忆上次更新").expect("reminder");
    assert!(reminder < header_end, "{}", brief.text);
    // 提醒行计入头部 token 读数（它占的是常驻预算）。
    let with_reminder = render_brief_with_notebook(
        &p, "user-1", "daily", BUDGET, &DEFAULT_INCLUDE, &[], Some(NOW - 5 * DAY), NOW,
    );
    let without = render_brief_with_notebook(
        &p, "user-1", "daily", BUDGET, &DEFAULT_INCLUDE, &[], None, NOW,
    );
    assert!(with_reminder.token_estimate > without.token_estimate);
}

#[test]
fn a_brief_without_a_last_write_time_carries_no_reminder() {
    let p = rich_payload();
    let brief = render_brief_with_notebook(
        &p, "user-1", "daily", BUDGET, &DEFAULT_INCLUDE, &[], None, NOW,
    );
    assert!(!brief.text.contains("核心记忆上次更新"), "{}", brief.text);
}

#[test]
fn brief_carries_the_last_write_reminder_only_after_a_document_exists() {
    // store 级闭环：注册但从未写过文档 ⇒ 无文档，不造假数据；写下 v1 ⇒ 有提醒，
    // 天数取注册表行的 updated_at_ms（与 doc 单行同一事务里落的同一个钟）。
    let dir = tempfile::tempdir().unwrap();
    let store = CoreStore::open(dir.path()).unwrap();
    store.register_agent("agent-a", 0).unwrap();
    let brief = store
        .brief("agent-a", "user-1", "daily", BUDGET, None, NOW)
        .unwrap();
    assert!(!brief.text.contains("核心记忆上次更新"), "{}", brief.text);

    store
        .save_doc("agent-a", &CorePayload::empty("agent-a"), "dev-1", NOW - DAY)
        .unwrap();
    let brief = store
        .brief("agent-a", "user-1", "daily", BUDGET, None, NOW)
        .unwrap();
    assert!(brief.text.contains("核心记忆上次更新：1 天前"), "{}", brief.text);
}
