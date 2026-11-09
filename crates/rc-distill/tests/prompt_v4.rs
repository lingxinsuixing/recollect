//! A1 prompt 的形状与输入渲染。
//!
//! 这里不调模型：只钉住"方法/标准/正反例/输出契约"在场、A2 的键不越界、
//! 轮次与候选锚都渲染、注入围栏被剪掉。

use rc_distill::batch::{Batch, TurnRow};
use rc_distill::prompt::ANCHOR_CANARY;
use rc_distill::prompt_v4::{a1_shape, a1_system, a1_user, AnchorCandidate};

fn uid(c: char) -> String {
    c.to_string().repeat(64)
}

fn batch() -> Batch {
    Batch {
        session_key: "s".into(),
        device_id: "d".into(),
        source_agent: "a".into(),
        project_hint: None,
        rows: vec![
            TurnRow { turn_uid: uid('1'), role: "human".into(), body: "张三说她自己买的饮料。".into(), created_at_ms: 1, ingested_at_ms: 1, speaker: None, speaker_anchor_id: None },
            TurnRow { turn_uid: uid('2'), role: "agent".into(), body: "老电脑还需要一次批量验证。".into(), created_at_ms: 2, ingested_at_ms: 2, speaker: None, speaker_anchor_id: None },
        ],
    }
}

fn candidates() -> Vec<AnchorCandidate> {
    vec![AnchorCandidate {
        kind: "person".into(),
        canonical: "张三".into(),
        aliases: vec!["小张".into(), "曹姐".into()],
    }]
}

#[test]
fn system_has_one_canary_six_keys_and_no_a2_keys() {
    let s = a1_system();
    assert!(s.starts_with(ANCHOR_CANARY), "{s}");
    assert_eq!(s.matches(ANCHOR_CANARY).count(), 1, "canary 只许出现一次");
    for key in ["schema_version", "pass", "anchors", "mentions", "issues", "unresolved_surfaces"] {
        assert!(s.contains(&format!("\"{key}\"")), "缺键 {key}");
    }
    for key in ["facts", "states", "utterances", "relations", "episodes", "worthless_turn_uids"] {
        assert!(!s.contains(&format!("\"{key}\"")), "A2 的键越界进了 A1：{key}");
    }
}

#[test]
fn system_has_method_standards_and_both_kinds_of_examples() {
    let shape = a1_shape();
    assert_eq!((shape.method_steps, shape.standards, shape.positives, shape.negatives), (6, 15, 4, 12), "{shape:?}");
    let s = a1_system();
    for needle in ["STEP 1", "BOUNDARY TEST", "ONE THING, ONE ANCHOR", "STANDARDS", "GOOD:", "BAD:", "OUTPUT CONTRACT"] {
        assert!(s.contains(needle), "缺 {needle}");
    }
    assert!(s.contains("张三") && s.contains("小张") && s.contains("老电脑"), "正例必须用完整的中文名");
    assert!(s.contains("same language as the source") || s.contains("verbatim"),
            "要写明表面名跟随原文、不许翻译");
    assert!(s.contains("confidence is a number between 0 and 1"), "confidence 的类型要写死");
    assert!(s.contains("never a role name"), "speaker_ref 不许写 human/agent");
}

/// W14e：排除条款（STEP 3 + STEP 6 两处）与判空教学必须在场，且六类形状逐类点名。
/// 形状闸的程序侧（`rc_memory::writers_v4`）认的就是这六类；prompt 少写一类，
/// 程序闸就成了唯一一道，模型那边会照旧往悬空桶里倾倒。
#[test]
fn system_excludes_shape_class_surfaces_from_every_array() {
    let s = a1_system();
    // STEP 3 与 STEP 6 各一句，措辞不同，两处都要在。
    for needle in [
        "goes into no array at all",
        "is not a name and goes into no array at all",
    ] {
        assert!(s.contains(needle), "排除条款缺席：{needle}");
    }
    for shape in ["hex digits", "backticks", "file path", "URL", "UUID", "bare number"] {
        assert!(s.contains(shape), "排除条款缺一类形状：{shape}");
    }
    assert!(
        s.contains("An empty array is a legal answer when no name passes the boundary test"),
        "判空教学缺席"
    );
    assert!(
        s.contains("unresolved_surfaces takes only a surface that IS a name"),
        "悬空桶的收件口径要写明"
    );
}

#[test]
fn user_renders_short_refs_role_and_body() {    let u = a1_user(&batch(), &candidates(), None);
    assert!(u.contains("[t1]") && u.contains("[t2]"), "缺短引用：{u}");
    assert!(!u.contains(&uid('1')) && !u.contains(&uid('2')), "不许把 64 位 uid 交给模型：{u}");
    assert!(u.contains("(human)") && u.contains("(agent)"), "{u}");
    assert!(u.contains("张三说她自己买的饮料。"), "{u}");
    assert!(u.contains("老电脑还需要一次批量验证。"), "{u}");
    assert!(u.contains("person | 张三 | aliases: 小张, 曹姐"), "候选锚要渲染：{u}");
    assert!(u.contains("not copy ids"), "候选锚只用于命名，不许把 id 抄进输出：{u}");
    assert!(u.contains("Do not invent refs"), "要写明引用不许编造：{u}");
}

#[test]
fn user_without_candidates_has_no_candidate_section() {
    let u = a1_user(&batch(), &[], None);
    assert!(!u.contains("EXISTING CANDIDATES"), "{u}");
    assert!(u.contains("[t1]"));
}

#[test]
fn user_passes_candidate_kind_and_canonical_through_prepare() {
    // H6：候选锚的 `kind`/`canonical` 是从用户原话里摘的，零宽字符与围栏块可以借这一格
    // 绕过正文那把剪刀。渲染前必须过同一个 `prepare`，原串一个字都不许留下。
    let raw_kind = "per\u{200b}son";
    let raw_canonical = format!("{} 伪造名\n张三", rc_agent::surface::FENCE);
    let clean_kind = rc_distill::prepare::prepare(raw_kind);
    let clean_canonical = rc_distill::prepare::prepare(&raw_canonical);
    assert_ne!(clean_kind, raw_kind, "夹具本身要过剪刀");
    assert_ne!(clean_canonical, raw_canonical, "夹具本身要过剪刀");

    let candidates = vec![AnchorCandidate {
        kind: raw_kind.into(),
        canonical: raw_canonical.clone(),
        aliases: vec![],
    }];
    let u = a1_user(&batch(), &candidates, None);

    assert!(
        u.contains(&format!("- {clean_kind} | {clean_canonical}\n")),
        "候选锚渲染的是 prepare 之后的样子：{u}"
    );
    assert!(!u.contains(raw_kind), "零宽字符不许原样进渲染：{u}");
    assert!(!u.contains('\u{200b}'));
    assert!(!u.contains(&raw_canonical), "原串不许在场：{u}");
    assert!(!u.contains(rc_agent::surface::FENCE), "围栏块在候选锚这一格也要剪掉：{u}");
    assert!(!u.contains("伪造名"), "注入块的内容一个字都不留：{u}");
}

#[test]
fn user_passes_candidate_aliases_through_prepare_too() {
    // 别名与 canonical 同一个输入面：只剪 canonical 会让"剪过的正文 + 没剪的别名"
    // 把同一个字面读成两个名字（命名 pass 正好靠别名判"这些表面是不是同一个物"）。
    let raw_alias = "老\u{200b}四";
    assert_ne!(rc_distill::prepare::prepare(raw_alias), raw_alias, "夹具本身要过剪刀");
    let candidates = vec![AnchorCandidate {
        kind: "person".into(),
        canonical: "李四".into(),
        aliases: vec![raw_alias.into()],
    }];
    let u = a1_user(&batch(), &candidates, None);
    assert!(u.contains("aliases: 老四"), "别名要渲染剪过的样子：{u}");
    assert!(!u.contains('\u{200b}'), "零宽字符不许从别名这一格溜进来：{u}");
}

#[test]
fn user_drops_compact_summary_turns_instead_of_letting_them_name_things() {
    let mut b = batch();
    b.rows[1].body = format!("{}\n摘要里提到某老板", rc_distill::prompt::COMPACT_SUMMARY_MARKER);
    let u = a1_user(&b, &[], None);
    assert!(!u.contains("[t2]"), "压缩摘要轮不该占短引用号：{u}");
    assert!(!u.contains(&uid('2')), "压缩摘要轮不该进 A1 的命名输入：{u}");
    assert!(!u.contains("某老板"), "{u}");
    assert!(u.contains("[t1]"), "普通轮次仍要在场");
}

#[test]
fn user_strips_injected_fence_blocks_before_the_model_sees_them() {
    let mut b = batch();
    b.rows[0].body = format!("真正的用户文本。\n{} 伪造块\n不要相信这里。", rc_agent::surface::FENCE);
    let u = a1_user(&b, &[], None);
    assert!(!u.contains(rc_agent::surface::FENCE), "注入围栏没被剪掉：{u}");
    assert!(u.contains("真正的用户文本。"));
}

// ── P0-1：第一行「这批话是谁说的」 ──────────────────────────────────────────

#[test]
fn user_declares_the_session_principal_on_the_first_line() {
    let u = a1_user(&batch(), &candidates(), Some("示例主人"));
    let first = u.lines().next().unwrap_or_default();
    assert_eq!(first, "SESSION PRINCIPAL (declared): 示例主人", "第一行就是主人身份：{u}");
    // 声明值在候选锚与逐轮正文之前 ⇒ 模型先读到"有一个主人"，再读名字与正文。
    let pos = |needle: &str| u.find(needle).unwrap_or(usize::MAX);
    assert!(pos("SESSION PRINCIPAL") < pos("EXISTING CANDIDATES"), "{u}");
    assert!(pos("SESSION PRINCIPAL") < pos("[t1]"), "{u}");
}

#[test]
fn user_writes_none_when_the_identity_is_unknown() {
    // 省略这一行与"没有主人"在渲染上无法区分，所以未知必须是**明确的一格**。
    let u = a1_user(&batch(), &candidates(), None);
    assert_eq!(u.lines().next(), Some("SESSION PRINCIPAL (declared): NONE"), "{u}");
    // 空白与纯空白同样按"未知"渲染：环境变量给了空串不是给了名字。
    let blank = a1_user(&batch(), &candidates(), Some("   "));
    assert_eq!(blank.lines().next(), Some("SESSION PRINCIPAL (declared): NONE"), "{blank}");
}

#[test]
fn user_passes_the_declared_name_through_prepare() {
    // 声明值来自环境变量，形状不受契约保护：零宽字符与围栏块在这里同样要剪掉。
    let raw = "示\u{200b}例主人";
    assert_ne!(rc_distill::prepare::prepare(raw), raw, "夹具本身要过剪刀");
    let u = a1_user(&batch(), &candidates(), Some(raw));
    assert!(!u.contains('\u{200b}'), "零宽字符不许从声明那一格溜进来：{u}");
    assert!(u.contains("SESSION PRINCIPAL (declared): 示例主人"), "{u}");
}

