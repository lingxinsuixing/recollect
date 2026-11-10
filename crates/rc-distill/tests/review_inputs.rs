//! C6a：审核输入的**信息段**（不改指令文案）。
//!
//! 五条用例对应派发档的 a–e：批头归属、目标轮 role+说话人、±1 邻轮只进 CTX、
//! 压缩块只进 CTX、条件词提示，以及"渲染里没有 turn_uid / anchor_id / 毫秒"。
//! 另加一条 `review_context` 的名字解析用例：human 轮复用 C1 的主人锚解析，
//! 解析不出就是 `unknown`（不拿 id 冒充名字）。
//!
//! 夹具只用通用占位名（张三/小明/某项目），不选批、不调模型、不写库。

use rc_distill::pass_event_contract::EventDraft;
use rc_distill::pass_fact_contract::FactDraft;
use rc_distill::pass_prompt::{
    condition_markers, event_review_user, fact_review_user, FactFamily, ReviewInputContext,
};
use rc_distill::review::run::review_context;
use rc_distill::{Batch, TurnRow};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_v4::ensure_principal_anchor;
use rc_memory::{Scope, ScopeKind};
use rc_store::Root;

const NOW: i64 = 1_792_994_700_000;

fn uid(index: usize) -> String {
    format!("{:064x}", index + 1)
}

fn batch(bodies: &[&str]) -> Batch {
    Batch {
        session_key: "sess-review".into(),
        device_id: "dev-review".into(),
        source_agent: "qoder".into(),
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

fn ctx(b: &Batch) -> ReviewInputContext {
    ReviewInputContext {
        session_key: b.session_key.clone(),
        owner_agent_id: "alice".into(),
        visibility: "project".into(),
        scope: "project:proj-x".into(),
        speakers: b
            .rows
            .iter()
            .map(|row| {
                if row.role == "human" { "小明".to_string() } else { b.source_agent.clone() }
            })
            .collect(),
    }
}

fn fact_draft(reference: &str, b: &Batch, index: usize, quote: &str) -> FactDraft {
    FactDraft {
        reference: reference.into(),
        turn_uid: b.rows[index].turn_uid.clone(),
        subject_anchor_id: format!("{:064x}", 0xa0),
        object_anchor_id: None,
        subject_canonical: "张三".into(),
        object_canonical: None,
        predicate: "promise".into(),
        value: Some("把对账单发给你".into()),
        category: "promise".into(),
        mode: "stated".into(),
        time_text: "下周三之前".into(),
        quote: quote.into(),
        evidence_turn_uids: vec![b.rows[index].turn_uid.clone()],
        text: "张三承诺发出对账单。".into(),
    }
}

fn event_draft(reference: &str, b: &Batch, index: usize, quote: &str) -> EventDraft {
    EventDraft {
        reference: reference.into(),
        turn_uid: b.rows[index].turn_uid.clone(),
        category: "state_change".into(),
        actor_anchor_id: format!("{:064x}", 0xa0),
        actor_canonical: "张三".into(),
        object_anchor_id: None,
        object_text: Some("配置文件".into()),
        object_canonical: None,
        participants_anchor_ids: vec![format!("{:064x}", 0xa0)],
        participant_canonicals: vec!["张三".into()],
        place_anchor_id: None,
        place_canonical: None,
        trigger: "改回去".into(),
        action: "restore_config".into(),
        result: "done".into(),
        mode: "stated".into(),
        time_text: "昨晚".into(),
        quote: quote.into(),
        evidence_turn_uids: vec![b.rows[index].turn_uid.clone()],
        text: "张三把配置改回去。".into(),
    }
}

/// 连续 13 位 ASCII 数字（毫秒时间戳的形状）。
fn has_13_digit_run(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut run = 0usize;
    for byte in bytes {
        if byte.is_ascii_digit() {
            run += 1;
            if run >= 13 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

// ── a. 批头与每轮 role + 说话人 ──────────────────────────────────────────────

#[test]
fn a_fact_review_renders_session_owner_visibility_role_and_speaker() {
    let b = batch(&["我下周三之前把对账单发给你", "张三已经把对账单收下了"]);
    let draft = fact_draft("f1", &b, 0, "我下周三之前把对账单发给你");
    let user = fact_review_user(FactFamily::Promise, &[draft], &b, &ctx(&b));

    assert!(user.contains("session_key=sess-review"), "批头带会话键");
    assert!(user.contains("owner_agent_id=alice"), "批头带归属 agent");
    assert!(user.contains("visibility=project"), "批头带可见性");
    assert!(user.contains("scope=project:proj-x"), "批头带 scope");
    assert!(
        user.contains("TARGET TURN [role: human | speaker: 小明]:"),
        "目标轮标 role 与 human 说话人名\n{user}"
    );
    // event 审核同形：同一份信息段，不是 fact 独有。
    let event = event_draft("e1", &b, 0, "我下周三之前把对账单发给你");
    let event_user = event_review_user(&[event], &b, &ctx(&b));
    assert!(event_user.contains("owner_agent_id=alice"));
    assert!(event_user.contains("visibility=project"));
    assert!(event_user.contains("TARGET TURN [role: human | speaker: 小明]:"));
}

// ── b. ±1 邻轮只在 CTX 段 ────────────────────────────────────────────────────

#[test]
fn b_neighbor_turns_go_to_ctx_only_and_never_into_target() {
    let b = batch(&["上一轮说的话", "我下周三之前把对账单发给你", "下一轮说的话"]);
    let draft = fact_draft("f1", &b, 1, "我下周三之前把对账单发给你");
    let user = fact_review_user(FactFamily::Promise, &[draft], &b, &ctx(&b));

    let ctx_at = user.find("CTX (not evidence):").expect("有邻轮时必须有 CTX 段");
    let (before, after) = user.split_at(ctx_at);
    assert!(!before.contains("上一轮说的话"), "前一轮不进 TARGET 段");
    assert!(!before.contains("下一轮说的话"), "后一轮不进 TARGET 段");
    assert!(after.contains("上一轮说的话"), "前一轮在 CTX 段");
    assert!(after.contains("下一轮说的话"), "后一轮在 CTX 段");

    // 没有邻轮时不静默省略。
    let solo = batch(&["我下周三之前把对账单发给你"]);
    let solo_draft = fact_draft("f1", &solo, 0, "我下周三之前把对账单发给你");
    let solo_user = fact_review_user(FactFamily::Promise, &[solo_draft], &solo, &ctx(&solo));
    assert!(solo_user.contains("CTX: (none)"), "没有邻轮要明写 (none)\n{solo_user}");
}

// ── c. 压缩块只进 CTX 段 ─────────────────────────────────────────────────────

#[test]
fn c_compaction_block_is_only_rendered_in_ctx() {
    let compact = "[[compact_summary]] 这一段是会话摘要，不是某人说过的话";
    let b = batch(&[compact, "我下周三之前把对账单发给你"]);
    let draft = fact_draft("f1", &b, 1, "我下周三之前把对账单发给你");
    let user = fact_review_user(FactFamily::Promise, &[draft], &b, &ctx(&b));

    let ctx_at = user.find("CTX (not evidence):").expect("压缩块邻居必须进 CTX 段");
    assert!(!user[..ctx_at].contains("compact_summary"), "压缩块不进 TARGET 段");
    assert!(user[ctx_at..].contains("compact_summary"), "压缩块只在 CTX 段");
    assert!(
        user.contains("prev [compact summary]:"),
        "CTX 段要标明这是摘要，不是证据\n{user}"
    );
}

// ── d. 条件标记只提示 ────────────────────────────────────────────────────────

#[test]
fn d_condition_markers_appear_only_when_the_evidence_turn_has_one() {
    let with = batch(&["如果明天有空，我就把对账单发给你"]);
    let draft = fact_draft("f1", &with, 0, "如果明天有空，我就把对账单发给你");
    let user = fact_review_user(FactFamily::Promise, &[draft], &with, &ctx(&with));
    assert!(user.contains("condition_markers: 如果"), "命中条件词要提示\n{user}");

    let without = batch(&["今天天气不错"]);
    let draft = fact_draft("f1", &without, 0, "今天天气不错");
    let user = fact_review_user(FactFamily::Promise, &[draft], &without, &ctx(&without));
    assert!(!user.contains("condition_markers"), "没有条件词时不出现这一行");

    // 词表就是派发档给定的那一组；重叠按最长优先（之后 不额外记 后）。
    assert_eq!(condition_markers("确认后再说"), vec!["确认后"]);
    assert_eq!(condition_markers("三天之后再说"), vec!["之后"]);
    assert_eq!(condition_markers("没有任何条件"), Vec::<&str>::new());
}

// ── e. 不渲染身份/时钟 ───────────────────────────────────────────────────────

#[test]
fn e_render_has_no_turn_uid_anchor_id_or_millisecond_timestamp() {
    let b = batch(&["如果明天有空，我就把对账单发给你", "下一轮"]);
    let draft = fact_draft("f1", &b, 0, "如果明天有空，我就把对账单发给你");
    let user = fact_review_user(FactFamily::Promise, &[draft], &b, &ctx(&b));

    assert!(!user.contains(&uid(0)) && !user.contains(&uid(1)), "不渲染 turn_uid");
    assert!(!user.contains("turn_uid"), "连字段名都不出现");
    assert!(!user.contains("anchor_id"), "不渲染 anchor_id（owner_agent_id 不算）");
    assert!(!has_13_digit_run(&user), "不出现 13 位毫秒时间戳\n{user}");

    let event = event_draft("e1", &b, 0, "如果明天有空，我就把对账单发给你");
    let event_user = event_review_user(&[event], &b, &ctx(&b));
    assert!(!event_user.contains(&uid(0)));
    assert!(!event_user.contains("anchor_id"));
    assert!(!has_13_digit_run(&event_user));
}

// ── f. 名字解析复用 C1（主人锚 / source_agent / unknown）────────────────────

#[test]
fn f_review_context_reuses_c1_speaker_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    let owner = OwnerCtx {
        owner_principal_id: Some("p1".into()),
        owner_agent_id: "alice".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    };
    // 主人锚：canonical = claim 的可读名（C1 的 human 说话人锚）。
    ensure_principal_anchor(root.memory(), "p1", Some("小明"), &owner, &scope, NOW).unwrap();

    let b = batch(&["human 说的", "agent 说的"]);
    let got = review_context(root.memory(), "sess-review", &b, &owner, &scope).unwrap();
    assert_eq!(got.session_key, "sess-review");
    assert_eq!(got.owner_agent_id, "alice");
    assert_eq!(got.visibility, "project");
    assert_eq!(got.scope, "project:proj-x");
    assert_eq!(got.speakers, vec!["小明".to_string(), "qoder".to_string()]);

    // §10.50 期望变更（原值 ["unknown","qoder"]）：没有 claim 时不再一律 unknown——
    // scope 内**唯一的 person 锚**就是说话人，与抽取侧 `speaker_ref` 同一判据。
    // 起因是抽查里审核模型把"示例主人"判成 missing_subject，而抽取侧明明给出了 speaker=a5。
    let anonymous = OwnerCtx { owner_principal_id: None, ..owner.clone() };
    let got = review_context(root.memory(), "sess-review", &b, &anonymous, &scope).unwrap();
    assert_eq!(
        got.speakers,
        vec!["小明".to_string(), "qoder".to_string()],
        "唯一 person 锚回退成说话人"
    );

    // 两个 person 锚 ⇒ 判不出来，仍是 unknown（不猜）。
    let other = OwnerCtx { owner_principal_id: Some("p2".into()), ..owner.clone() };
    ensure_principal_anchor(root.memory(), "p2", Some("小红"), &other, &scope, NOW).unwrap();
    let got = review_context(root.memory(), "sess-review", &b, &anonymous, &scope).unwrap();
    assert_eq!(
        got.speakers,
        vec!["unknown".to_string(), "qoder".to_string()],
        "多个 person 锚时不许猜"
    );
}

// ── g. A7：轮级说话人优先于 role 推导；空白 = 没给 ─────────────────────────────

/// 轮级说话人（mention 表回填，`load_turn_speakers`）非空 ⇒ 覆盖 role 推导的名字；
/// 空串/纯空白 = 没给（写入面 `record_turn_speakers` 的口径），回落 C1 解析，
/// 不出空前缀。`None` 的旧轮走同一条回落（上面 f 用例的夹具行全是 None）。
/// mention→TurnRow 的真实回填链路钉在 `tests/review_replay.rs` 的 runner 用例里。
/// 名字用「示例主人」是 A7 派发档验收判据明写的（账号主人名，非语料专名）。
#[test]
fn g_review_context_prefers_turn_level_speaker_and_falls_back_on_blank() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    let owner = OwnerCtx {
        owner_principal_id: Some("p1".into()),
        owner_agent_id: "alice".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    };
    ensure_principal_anchor(root.memory(), "p1", Some("小明"), &owner, &scope, NOW).unwrap();

    let mut b = batch(&["human 说的", "agent 说的"]);
    b.rows[0].speaker = Some("  ".into());
    b.rows[1].speaker = Some("示例主人".into());
    let got = review_context(root.memory(), "sess-review", &b, &owner, &scope).unwrap();
    assert_eq!(
        got.speakers,
        vec!["小明".to_string(), "示例主人".to_string()],
        "轮级说话人覆盖 role 推导；空白按没给回落 principal，不出空前缀"
    );

    // 渲染面同一形状：说话人进 TARGET TURN 行（`render_target_head` 吃 `speakers`）。
    let draft = fact_draft("f1", &b, 1, "human 说的");
    let user = fact_review_user(FactFamily::Promise, &[draft], &b, &got);
    assert!(
        user.contains("TARGET TURN [role: agent | speaker: 示例主人]:"),
        "送审 prompt 带轮级说话人\n{user}"
    );
    assert!(!user.contains("speaker: ],"), "不许出现空说话人前缀");
    assert!(!user.contains("speaker: None"), "不许出现 None 前缀");
}
