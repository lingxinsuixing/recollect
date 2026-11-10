//! core 工具操作层（设计 §9.4/§10.3，工具契约 §6）：人格提案/审批/回滚，以及
//! 主用户、交互、情绪、未闭环与 pinned 经历的 load → mutate → save。

use rc_core::ops::HANDOFF_EXPIRED_GRACE_MS;
use rc_core::{
    proposal_id, table_names, Affect, CoreError, CorePayload, CoreStore, HandoffItem,
    InteractionTouch, LoopItem, Persona, PersonaPatch, Relationship, Subject, HANDOFF_OPEN_LIMIT,
    HANDOFF_TEXT_MAX, HANDOFF_TTL_MS, HANDOFF_TOTAL_LIMIT,
};
use rusqlite::Connection;
use serde_json::json;
use std::path::Path;
use std::sync::mpsc;

fn new_store() -> (tempfile::TempDir, CoreStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = CoreStore::open(dir.path()).unwrap();
    store.register_agent("agent-a", 0).unwrap();
    (dir, store)
}

/// v1：一份有人格、有 L0 的起点文档。
fn seed(store: &CoreStore) -> u32 {
    let mut payload = CorePayload::empty("agent-a");
    payload.l0 = vec!["不冒充人类".to_string(), "不确定就说".to_string()];
    payload.persona = Persona {
        core_text: "v1: 慢热，记得小事".to_string(),
        traits: [("warmth".to_string(), json!(0.4))].into_iter().collect(),
        style: [("punct".to_string(), json!("…"))].into_iter().collect(),
        quirks: vec!["喜欢留白".to_string()],
        boundaries: vec!["不替用户做决定".to_string()],
        voice_examples: vec!["你回来了。".to_string()],
    };
    store.save_doc("agent-a", &payload, "dev-seed", 1_000).unwrap()
}

fn doc(store: &CoreStore) -> CorePayload {
    store.load_doc("agent-a").unwrap().unwrap()
}

fn version(store: &CoreStore) -> u32 {
    u32::try_from(store.list_agents().unwrap()[0].doc_version).unwrap()
}

/// 载荷里当前未关闭的交接条数（上限判据读的就是这个数）。
fn open_count(store: &CoreStore) -> usize {
    doc(store)
        .handoffs
        .iter()
        .filter(|h| h.closed_at_ms.is_none())
        .count()
}

fn count_rows(dir: &Path, table: &str) -> i64 {
    Connection::open(dir.join("core.db"))
        .unwrap()
        .query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |r| {
            r.get(0)
        })
        .unwrap()
}

fn subject(id: &str, primary: bool) -> Subject {
    Subject {
        subject_id: id.to_string(),
        is_primary: primary,
        profile_text: format!("{id} 的画像"),
        preferences: [("tea".to_string(), json!("oolong"))].into_iter().collect(),
        triggers: vec!["被忽略".to_string()],
        taboos: vec![],
    }
}

fn affect(kind: &str, label: &str, confidence: f64) -> Affect {
    Affect {
        subject_kind: kind.to_string(),
        label: label.to_string(),
        valence: -0.3,
        arousal: 0.2,
        energy: 0.5,
        confidence,
        valid_from_ms: 2_000,
        cause: "加班".to_string(),
    }
}

/// 一段合法的关系读数（§9.2 单段：不是列表，整段替换）。
fn relationship() -> Relationship {
    Relationship {
        trust: 0.8,
        intimacy: 0.6,
        tension: 0.1,
        status: "normal".to_string(),
        text: "最近有点安静".to_string(),
        confidence: 0.7,
    }
}

#[test]
fn propose_then_apply_changes_persona_but_never_l0() {
    let (_dir, store) = new_store();
    let v1 = seed(&store);
    let before = doc(&store);

    let patch = PersonaPatch {
        core_text: Some("v2: 更直接".to_string()),
        traits: Some(json!({"warmth": 0.8, "directness": 0.6})),
        quirks: Some(vec!["爱用比喻".to_string()]),
        ..PersonaPatch::default()
    };
    let reason = "用户说希望我别绕弯";
    let (v2, id) = store
        .persona_propose(
            "agent-a",
            patch.clone(),
            reason,
            vec!["turn-1".to_string()],
            "dev-1",
            2_000,
        )
        .unwrap();
    assert_eq!(v2, v1 + 1);
    assert_eq!(id, proposal_id("agent-a", 2_000, reason, 0));
    assert_eq!(id.len(), 64, "{id}");
    assert!(id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));

    // propose 只追加提案，persona 一个字不动。
    let proposed = doc(&store);
    assert_eq!(proposed, {
        let mut expected = before.clone();
        expected.persona_proposals = proposed.persona_proposals.clone();
        expected
    });
    assert_eq!(proposed.persona, before.persona);
    assert_eq!(proposed.persona_proposals.len(), 1);
    assert_eq!(proposed.persona_proposals[0].status, "proposed");
    assert_eq!(proposed.persona_proposals[0].patch, patch);
    assert_eq!(proposed.persona_proposals[0].reason, reason);
    assert_eq!(proposed.persona_proposals[0].evidence, vec!["turn-1"]);
    assert_eq!(proposed.persona_proposals[0].created_at_ms, 2_000);
    assert_eq!(proposed.persona_proposals[0].applied_at_ms, None);

    // apply 才改 persona。
    let v3 = store.persona_apply("agent-a", &id, "dev-1", 3_000).unwrap();
    assert_eq!(v3, v2 + 1);
    let applied = doc(&store);
    assert_eq!(applied.persona.core_text, "v2: 更直接");
    assert_eq!(applied.persona.traits.get("warmth"), Some(&json!(0.8)));
    assert_eq!(applied.persona.traits.get("directness"), Some(&json!(0.6)));
    assert_eq!(applied.persona.quirks, vec!["爱用比喻"]);
    // patch 没提到的字段保持原值，L0 整段原样。
    assert_eq!(applied.persona.style, before.persona.style);
    assert_eq!(applied.persona.boundaries, before.persona.boundaries);
    assert_eq!(applied.persona.voice_examples, before.persona.voice_examples);
    assert_eq!(applied.l0, before.l0, "L0 must never be touched by a patch");
    assert_eq!(applied.agent_id, "agent-a");
    // 出队：终态提案不再留在载荷里；它还在的那一版（v2）在 rev 表里找得回。
    assert!(
        applied.persona_proposals.iter().all(|p| p.id != id),
        "applied 提案必须出队：{:?}",
        applied.persona_proposals
    );
    let rev_v2 = store.load_revision("agent-a", v2).unwrap();
    assert!(
        rev_v2
            .persona_proposals
            .iter()
            .any(|p| p.id == id && p.status == "proposed"),
        "提案的完整记录必须留在 rev 表的旧版本里"
    );

    // 幂等性边界：同一提案再 apply 一次是 NotFound（已出队），且不落新版本。
    assert!(matches!(
        store.persona_apply("agent-a", &id, "dev-1", 4_000),
        Err(CoreError::NotFound)
    ));
    assert_eq!(version(&store), v3);
    assert!(matches!(
        store.persona_apply("agent-a", "no-such-proposal", "dev-1", 4_000),
        Err(CoreError::NotFound)
    ));
}

#[test]
fn an_empty_reason_or_a_bad_patch_shape_is_refused_without_saving() {
    let (_dir, store) = new_store();
    let v1 = seed(&store);

    assert!(matches!(
        store.persona_propose(
            "agent-a",
            PersonaPatch::default(),
            "   ",
            vec![],
            "dev-1",
            2_000
        ),
        Err(CoreError::BadPayload(_))
    ));
    assert!(matches!(
        store.persona_propose(
            "agent-a",
            PersonaPatch {
                traits: Some(json!("not-an-object")),
                ..PersonaPatch::default()
            },
            "理由",
            vec![],
            "dev-1",
            2_000
        ),
        Err(CoreError::BadPayload(_))
    ));
    assert_eq!(version(&store), v1, "refused proposals must not save");
    assert!(doc(&store).persona_proposals.is_empty());

    // 理由/证据为空是允许的（只是"没有证据"），reason 非空即可。
    let (v2, _) = store
        .persona_propose(
            "agent-a",
            PersonaPatch { quirks: Some(vec!["爱看云".to_string()]), ..Default::default() },
            "用户观察",
            vec![],
            "dev-1",
            2_100,
        )
        .unwrap();
    assert_eq!(v2, v1 + 1);
    assert!(doc(&store).persona_proposals[0].evidence.is_empty());
}

#[test]
fn persona_patch_cannot_carry_l0() {
    for raw in [
        r#"{"l0":["x"]}"#,
        r#"{"core_text":"t","l0":["x"]}"#,
        r#"{"traits":{},"l0":[]}"#,
    ] {
        let err = serde_json::from_str::<PersonaPatch>(raw).unwrap_err();
        assert!(err.to_string().contains("l0"), "{raw} => {err}");
    }
    // 合法的 L1 字段照常解析；缺省字段即 None。
    let parsed: PersonaPatch =
        serde_json::from_str(r#"{"core_text":"t","quirks":["q"]}"#).unwrap();
    assert_eq!(parsed.core_text.as_deref(), Some("t"));
    assert_eq!(parsed.quirks.unwrap(), vec!["q"]);
    assert_eq!(parsed.boundaries, None);
}

#[test]
fn a_rejected_proposal_cannot_be_applied() {
    let (_dir, store) = new_store();
    seed(&store);
    let (_, id) = store
        .persona_propose(
            "agent-a",
            PersonaPatch { core_text: Some("不许变".to_string()), ..Default::default() },
            "试试",
            vec![],
            "dev-1",
            2_000,
        )
        .unwrap();
    let before_reject = doc(&store).persona.clone();
    let v = store.persona_reject("agent-a", &id, "dev-1", 3_000).unwrap();
    let rejected = doc(&store);
    assert!(
        rejected.persona_proposals.iter().all(|p| p.id != id),
        "rejected 提案必须出队：{:?}",
        rejected.persona_proposals
    );
    assert_eq!(rejected.persona, before_reject, "reject must not change persona");
    // 被拒提案的账在历史里：前一版载荷里它还完整在（还是 proposed）。
    let rev_before_reject = store.load_revision("agent-a", v - 1).unwrap();
    assert!(
        rev_before_reject
            .persona_proposals
            .iter()
            .any(|p| p.id == id && p.status == "proposed"),
        "被拒提案的完整记录必须留在 rev 表的旧版本里"
    );

    // 已出队的提案再 apply/reject 都是 NotFound（proposal_index 语义不变），且不落新版本。
    assert!(matches!(
        store.persona_apply("agent-a", &id, "dev-1", 4_000),
        Err(CoreError::NotFound)
    ));
    assert_eq!(version(&store), v);
    assert!(matches!(
        store.persona_reject("agent-a", &id, "dev-1", 4_000),
        Err(CoreError::NotFound)
    ));
}

#[test]
fn revert_restores_an_older_persona_as_a_new_version() {
    let (dir, store) = new_store();
    let v1 = seed(&store);

    let mut payload = doc(&store);
    payload.persona.core_text = "v2: 话变多了".to_string();
    payload.persona.quirks = vec!["爱接梗".to_string()];
    let v2 = store.save_doc("agent-a", &payload, "dev-1", 2_000).unwrap();
    assert_eq!(v2, v1 + 1);

    let v3 = store.persona_revert("agent-a", v1, "dev-1", 3_000).unwrap();
    assert_eq!(v3, v2 + 1);
    let reverted = doc(&store);
    assert_eq!(reverted.persona.core_text, "v1: 慢热，记得小事");
    assert_eq!(reverted.persona.quirks, vec!["喜欢留白"]);
    assert_eq!(reverted.persona.traits.get("warmth"), Some(&json!(0.4)));

    // 回滚是"再存一版"，不是删历史：三行 revision 都在，旧版仍读得回。
    let (_, rev_table) = table_names("agent-a");
    assert_eq!(count_rows(dir.path(), &rev_table), 3);
    assert_eq!(
        store.load_revision("agent-a", v1).unwrap().persona.core_text,
        "v1: 慢热，记得小事"
    );
    assert_eq!(
        store.load_revision("agent-a", v2).unwrap().persona.core_text,
        "v2: 话变多了"
    );

    assert!(matches!(
        store.persona_revert("agent-a", 99, "dev-1", 4_000),
        Err(CoreError::NotFound)
    ));
    assert_eq!(version(&store), v3);
}

#[test]
fn load_revision_reads_old_payloads_and_returns_none_for_missing_versions() {
    let (_dir, store) = new_store();
    let v1 = seed(&store);

    let mut payload = doc(&store);
    payload.interaction.unanswered_user_messages = 3;
    payload.relationship.trust = 0.9;
    let v2 = store.save_doc("agent-a", &payload, "dev-1", 2_000).unwrap();

    let rev1 = store.load_revision("agent-a", v1).unwrap();
    assert_eq!(rev1.agent_id, "agent-a");
    assert_eq!(rev1.interaction.unanswered_user_messages, 0);
    assert_eq!(rev1.relationship.trust, 0.5);
    assert_eq!(rev1.persona.core_text, "v1: 慢热，记得小事");
    assert_eq!(rev1.l0.len(), 2);

    let rev2 = store.load_revision("agent-a", v2).unwrap();
    assert_eq!(rev2.interaction.unanswered_user_messages, 3);
    assert_eq!(rev2.relationship.trust, 0.9);

    assert!(store.load_revision("agent-a", 0).is_none());
    assert!(store.load_revision("agent-a", 99).is_none());
    assert!(store.load_revision("agent-a", v2 + 1).is_none());
    assert!(store.try_load_revision("agent-a", 99).unwrap().is_none());

    // 从没写过文档的 agent 也没有 revision。
    store.register_agent("agent-b", 0).unwrap();
    assert!(store.load_revision("agent-b", 1).is_none());
}

#[test]
fn subject_update_switches_the_primary_and_caps_the_list_at_four() {
    let (_dir, store) = new_store();
    seed(&store);

    let v1 = store
        .subject_update("agent-a", subject("user-main", true), "dev-1", 2_000)
        .unwrap();
    let one = doc(&store);
    assert_eq!(one.subjects.len(), 1);
    assert!(one.subjects[0].is_primary);

    // 新 primary 上位，旧 primary 降级。
    let v2 = store
        .subject_update("agent-a", subject("user-alt", true), "dev-1", 2_100)
        .unwrap();
    assert!(v2 > v1);
    let switched = doc(&store);
    assert_eq!(switched.subjects.len(), 2);
    let main = switched.subjects.iter().find(|s| s.subject_id == "user-main").unwrap();
    let alt = switched.subjects.iter().find(|s| s.subject_id == "user-alt").unwrap();
    assert!(!main.is_primary, "old primary must be demoted");
    assert!(alt.is_primary);
    assert_eq!(alt.profile_text, "user-alt 的画像");

    // 再加两个非 primary：正好 4 个（1 primary + 3 核心关系）。
    store
        .subject_update("agent-a", subject("friend-1", false), "dev-1", 2_200)
        .unwrap();
    let v4 = store
        .subject_update("agent-a", subject("friend-2", false), "dev-1", 2_300)
        .unwrap();
    let four = doc(&store);
    assert_eq!(four.subjects.len(), 4);
    assert_eq!(four.subjects.iter().filter(|s| s.is_primary).count(), 1);

    // 第 5 个 ⇒ BadPayload，且整份不存。
    assert!(matches!(
        store.subject_update("agent-a", subject("friend-3", false), "dev-1", 2_400),
        Err(CoreError::BadPayload(_))
    ));
    assert_eq!(version(&store), v4);
    assert_eq!(doc(&store).subjects.len(), 4);

    // 已有 id 是 upsert 不是追加；空 id 直接拒绝。
    store
        .subject_update(
            "agent-a",
            Subject { profile_text: "改过的画像".to_string(), ..subject("friend-1", false) },
            "dev-1",
            2_500,
        )
        .unwrap();
    let updated = doc(&store);
    assert_eq!(updated.subjects.len(), 4);
    assert_eq!(
        updated
            .subjects
            .iter()
            .find(|s| s.subject_id == "friend-1")
            .unwrap()
            .profile_text,
        "改过的画像"
    );
    assert!(matches!(
        store.subject_update("agent-a", subject("  ", false), "dev-1", 2_600),
        Err(CoreError::BadPayload(_))
    ));
}

#[test]
fn interaction_touch_only_writes_the_fields_that_were_given() {
    let (_dir, store) = new_store();
    seed(&store);

    let v1 = store
        .interaction_touch(
            "agent-a",
            InteractionTouch {
                last_user_message_at_ms: Some(5_000),
                current_topic: Some("周末露营".to_string()),
                ..InteractionTouch::default()
            },
            "dev-1",
            5_000,
        )
        .unwrap();
    let touched = doc(&store);
    assert_eq!(touched.interaction.last_user_message_at_ms, 5_000);
    assert_eq!(touched.interaction.current_topic, "周末露营");
    assert_eq!(touched.interaction.last_agent_initiated_at_ms, 0);
    assert_eq!(touched.interaction.unanswered_user_messages, 0);
    assert_eq!(touched.interaction.attention_debt_ms, 0);

    // 只给未回计数 + 空白话题：话题清空，时间戳保持上一轮的值。
    let v2 = store
        .interaction_touch(
            "agent-a",
            InteractionTouch {
                unanswered_user_messages: Some(3),
                current_topic: Some(String::new()),
                ..InteractionTouch::default()
            },
            "dev-1",
            6_000,
        )
        .unwrap();
    assert!(v2 > v1);
    let second = doc(&store);
    assert_eq!(second.interaction.unanswered_user_messages, 3);
    assert!(second.interaction.current_topic.is_empty());
    assert_eq!(second.interaction.last_user_message_at_ms, 5_000);

    // `attention_debt_ms` 也只在给值时改；其余字段不动。
    store
        .interaction_touch(
            "agent-a",
            InteractionTouch {
                attention_debt_ms: Some(60_000),
                ..InteractionTouch::default()
            },
            "dev-1",
            7_000,
        )
        .unwrap();
    let third = doc(&store);
    assert_eq!(third.interaction.attention_debt_ms, 60_000);
    assert_eq!(third.interaction.unanswered_user_messages, 3);
    assert_eq!(third.interaction.last_user_message_at_ms, 5_000);
}

#[test]
fn affect_update_replaces_per_subject_kind_and_validates_input() {
    let (_dir, store) = new_store();
    seed(&store);

    store
        .affect_update("agent-a", affect("user", "疲惫", 0.4), "dev-1", 2_000)
        .unwrap();
    let one = doc(&store);
    assert_eq!(one.affects.len(), 1);
    assert_eq!(one.affects[0].label, "疲惫");

    // 同 kind 再写一条 ⇒ 替换而不是追加。
    let v2 = store
        .affect_update("agent-a", affect("user", "平静", 0.7), "dev-1", 2_100)
        .unwrap();
    let replaced = doc(&store);
    assert_eq!(replaced.affects.len(), 1);
    assert_eq!(replaced.affects[0].label, "平静");
    assert_eq!(replaced.affects[0].confidence, 0.7);

    // agent 是另一条。
    store
        .affect_update("agent-a", affect("agent", "专注", 0.6), "dev-1", 2_200)
        .unwrap();
    let both = doc(&store);
    assert_eq!(both.affects.len(), 2);
    assert_eq!(
        both.affects
            .iter()
            .find(|a| a.subject_kind == "agent")
            .unwrap()
            .label,
        "专注"
    );

    // 非法 kind / confidence 拒绝，且不落版本。
    for (kind, confidence) in [("robot", 0.5), ("", 0.5), ("user", 1.5), ("user", -0.1), ("user", f64::NAN)]
    {
        assert!(
            matches!(
                store.affect_update("agent-a", affect(kind, "非法", confidence), "dev-1", 2_300),
                Err(CoreError::BadPayload(_))
            ),
            "kind={kind:?} confidence={confidence}"
        );
    }
    assert_eq!(version(&store), v2 + 1);
    assert_eq!(doc(&store).affects.len(), 2);
}

// ── core.db 治理小刀③：relationship 写口（payload 11 段里唯一零写者的那一段）────

/// 写口与回执语义照 affect：整段替换（不是列表 upsert）、比值越界整份不存、版本只在
/// 保存成功时前进。`status`/`text` 是自由文本，不设词表（渲染照抄，不猜业务语义）。
#[test]
fn relationship_update_replaces_the_segment_and_validates_the_ratios() {
    let (_dir, store) = new_store();
    seed(&store);

    let v1 = store
        .relationship_update("agent-a", relationship(), "dev-1", 2_000)
        .unwrap();
    let one = doc(&store);
    assert_eq!(one.relationship.trust, 0.8);
    assert_eq!(one.relationship.intimacy, 0.6);
    assert_eq!(one.relationship.text, "最近有点安静");

    // 再写一条 ⇒ 整段替换；没给的字段跟着新值走（upsert 的不是字段是整段）。
    let v2 = store
        .relationship_update(
            "agent-a",
            Relationship { trust: 0.4, text: "吵了一架".to_string(), ..relationship() },
            "dev-1",
            2_100,
        )
        .unwrap();
    assert!(v2 > v1);
    let replaced = doc(&store);
    assert_eq!(replaced.relationship.trust, 0.4);
    assert_eq!(replaced.relationship.text, "吵了一架");

    // 越界值拒绝（照 affect 的 0..=1 尺，NaN 不在区间内同样被拒），且不落版本。
    for (name, bad) in [
        ("trust", Relationship { trust: 1.5, ..relationship() }),
        ("trust", Relationship { trust: -0.1, ..relationship() }),
        ("intimacy", Relationship { intimacy: 1.5, ..relationship() }),
        ("tension", Relationship { tension: 2.0, ..relationship() }),
        ("confidence", Relationship { confidence: f64::NAN, ..relationship() }),
    ] {
        assert!(
            matches!(
                store.relationship_update("agent-a", bad, "dev-1", 2_200),
                Err(CoreError::BadPayload(msg)) if msg.contains(name)
            ),
            "{name} 越界没被拒"
        );
    }
    assert_eq!(version(&store), v2, "被拒的写入不落版本");
    assert_eq!(doc(&store).relationship.trust, 0.4);
}

/// CAS 波的同形钉（语义面）：写者 A 拿着过期视图，写者 B 在中间完成一整个写；
/// A 的 relationship 落地后，两边的变更都要活着——写口走 [`CoreStore::update_doc`]
/// 的原子读改写（读发生在写锁之后），不许造"锁外读改写再 save"的旁路。
#[test]
fn a_relationship_write_lands_on_top_of_the_newest_committed_payload() {
    let (_dir, store) = new_store();
    seed(&store);

    // 写者 A：通过公开 API 拿到一份载荷（此刻它手里的视图停在 v1）。
    let stale = doc(&store);
    assert_eq!(stale.relationship.trust, 0.5, "前置：还没写过 relationship");

    // 写者 B：完成一整个写（主用户落库 ⇒ v2）。
    store
        .subject_update("agent-a", subject("user-main", true), "dev-1", 2_000)
        .unwrap();

    // A 的变更落地：必须叠在 B 之后，而不是把 B 整份盖掉。
    store
        .relationship_update("agent-a", relationship(), "dev-1", 3_000)
        .unwrap();

    let final_doc = doc(&store);
    assert_eq!(final_doc.relationship.trust, 0.8, "A 的 relationship 必须活着");
    assert_eq!(final_doc.subjects.len(), 1, "B 的主用户不许被 A 盖掉");
}

/// 锁纪律与 [`save_doc_takes_the_write_lock_before_reading_the_version`] 同一把尺：
/// 持锁的旁路连接没放锁时，relationship 写口拿到的是 SQLite 的 busy（SQL 族）错误，
/// 不是静默成功——写口自己开 `BEGIN IMMEDIATE`，不借读连接改完再存。
#[test]
fn a_relationship_write_takes_the_write_lock_before_touching_the_doc() {
    let (dir, store) = new_store();
    let holder = Connection::open(dir.path().join("core.db")).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();

    let blocked = store.relationship_update("agent-a", relationship(), "dev-1", 1_000);
    assert!(
        matches!(blocked, Err(CoreError::Sql(_))),
        "拿不到写锁必须是 SQL 族错误，不是静默成功：{blocked:?}"
    );
    assert_eq!(version(&store), 0, "被挡下的写入不许落版本");

    holder.execute_batch("ROLLBACK").unwrap();
    let ok = store
        .relationship_update("agent-a", relationship(), "dev-1", 1_000)
        .unwrap();
    assert_eq!(ok, 1, "锁释放后写口照常写进去");
    assert_eq!(doc(&store).relationship.trust, 0.8);
}

#[test]
fn loop_upsert_and_close_enforce_the_status_vocabulary() {
    let (_dir, store) = new_store();
    seed(&store);

    let item = LoopItem {
        id: "loop-1".to_string(),
        kind: "promise".to_string(),
        text: "下次带那本书".to_string(),
        status: "open".to_string(),
        due_at_ms: 9_000,
    };
    store.loop_upsert("agent-a", item.clone(), "dev-1", 2_000).unwrap();

    // 同 id 覆盖；不同 id 追加。
    store
        .loop_upsert(
            "agent-a",
            LoopItem { text: "下次带那本画册".to_string(), ..item.clone() },
            "dev-1",
            2_100,
        )
        .unwrap();
    store
        .loop_upsert(
            "agent-a",
            LoopItem { id: "loop-2".to_string(), kind: "conflict".to_string(), ..item.clone() },
            "dev-1",
            2_200,
        )
        .unwrap();
    let loops = doc(&store).loops;
    assert_eq!(loops.len(), 2);
    assert_eq!(
        loops.iter().find(|l| l.id == "loop-1").unwrap().text,
        "下次带那本画册"
    );
    assert_eq!(
        loops.iter().find(|l| l.id == "loop-2").unwrap().status,
        "open"
    );

    // 非法状态与空 id 拒绝。
    assert!(matches!(
        store.loop_upsert(
            "agent-a",
            LoopItem { status: "waiting".to_string(), ..item.clone() },
            "dev-1",
            2_300
        ),
        Err(CoreError::BadPayload(_))
    ));
    assert!(matches!(
        store.loop_upsert(
            "agent-a",
            LoopItem { id: "  ".to_string(), ..item.clone() },
            "dev-1",
            2_300
        ),
        Err(CoreError::BadPayload(_))
    ));

    let v = store.loop_close("agent-a", "loop-1", "done", "dev-1", 2_400).unwrap();
    let closed = doc(&store);
    assert_eq!(
        closed.loops.iter().find(|l| l.id == "loop-1").unwrap().status,
        "done"
    );

    assert!(matches!(
        store.loop_close("agent-a", "loop-1", "open", "dev-1", 2_500),
        Err(CoreError::BadPayload(_))
    ));
    assert!(matches!(
        store.loop_close("agent-a", "loop-404", "done", "dev-1", 2_500),
        Err(CoreError::NotFound)
    ));
    assert_eq!(version(&store), v);
    assert_eq!(
        doc(&store)
            .loops
            .iter()
            .find(|l| l.id == "loop-2")
            .unwrap()
            .status,
        "open"
    );
}

#[test]
fn episode_pin_is_an_upsert_and_unpin_is_idempotent() {
    let (_dir, store) = new_store();
    seed(&store);

    store
        .episode_pin("agent-a", "ep-1", "第一次一起熬夜", "dev-1", 2_000)
        .unwrap();
    store
        .episode_pin("agent-a", "ep-1", "第一次熬夜（补记）", "dev-1", 2_100)
        .unwrap();
    store
        .episode_pin("agent-a", "ep-2", "雨天的车站", "dev-1", 2_200)
        .unwrap();
    let pinned = doc(&store).pinned_episodes;
    assert_eq!(pinned.len(), 2, "same episode_id must upsert, not append");
    assert_eq!(
        pinned.iter().find(|p| p.episode_id == "ep-1").unwrap().pin_reason,
        "第一次熬夜（补记）"
    );

    assert!(matches!(
        store.episode_pin("agent-a", "ep-3", "  ", "dev-1", 2_300),
        Err(CoreError::BadPayload(_))
    ));
    assert!(matches!(
        store.episode_pin("agent-a", "", "理由", "dev-1", 2_300),
        Err(CoreError::BadPayload(_))
    ));

    let v = store.episode_unpin("agent-a", "ep-1", "dev-1", 2_400).unwrap();
    let left = doc(&store).pinned_episodes;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].episode_id, "ep-2");

    // 幂等：不存在的 id 也不报错（只落一个新版本）。
    let again = store.episode_unpin("agent-a", "ep-1", "dev-1", 2_500).unwrap();
    assert!(again > v);
    assert_eq!(doc(&store).pinned_episodes.len(), 1);
}

#[test]
fn handoff_write_list_close_round_trips() {
    let (_dir, store) = new_store();
    let now = 2_000;
    let text = "下次从 C4 开始：先把 handoff 段写进载荷";
    let written = store
        .handoff_write("agent-a", "continue", text, None, "dev-1", now)
        .unwrap();
    assert!(written.superseded.is_empty(), "{:?}", written.superseded);
    assert!(written.id.starts_with("h-"), "{}", written.id);
    assert_eq!(written.id.len(), 14, "h- + 12 hex：{}", written.id);
    assert_eq!(written.version, 1);

    let listed = store.handoff_list("agent-a", now).unwrap();
    assert_eq!(listed.len(), 1);
    let item = &listed[0];
    assert_eq!(item.id, written.id);
    assert_eq!(item.kind, "continue");
    assert_eq!(item.text, text);
    assert_eq!(item.created_at_ms, now);
    assert_eq!(item.expires_at_ms, now + HANDOFF_TTL_MS);
    assert_eq!(item.seen_count, 0);
    assert_eq!(item.last_seen_at_ms, None);
    assert_eq!(item.closed_at_ms, None);
    assert_eq!(item.closed_reason, None);

    let closed = store
        .handoff_close("agent-a", &written.id, Some("done"), "dev-1", now + 10)
        .unwrap();
    assert_eq!(closed.id, written.id);
    assert_eq!(closed.closed_at_ms, now + 10);
    assert_eq!(closed.reason, "done");
    assert!(!closed.already_closed);
    assert_eq!(closed.version, written.version + 1);

    assert!(store.handoff_list("agent-a", now + 20).unwrap().is_empty());
    let stored = doc(&store);
    let row = stored.handoffs.iter().find(|h| h.id == written.id).unwrap();
    assert_eq!(row.closed_at_ms, Some(now + 10));
    assert_eq!(row.closed_reason.as_deref(), Some("done"));
}

#[test]
fn a_new_continue_supersedes_the_previous_one() {
    let (_dir, store) = new_store();
    let now = 2_000;
    let a = store
        .handoff_write("agent-a", "continue", "先做 A", None, "dev-1", now)
        .unwrap();
    let b = store
        .handoff_write("agent-a", "continue", "再做 B", None, "dev-1", now + 10)
        .unwrap();
    assert_eq!(b.superseded, vec![a.id.clone()]);

    let listed = store.handoff_list("agent-a", now + 20).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, b.id);

    let stored = doc(&store);
    assert_eq!(stored.handoffs.len(), 2, "旧条是关闭而不是删除");
    let old = stored.handoffs.iter().find(|h| h.id == a.id).unwrap();
    assert_eq!(old.closed_reason.as_deref(), Some("superseded"));
    assert_eq!(old.closed_at_ms, Some(now + 10));
}

#[test]
fn handoff_refuses_the_fifty_first_open_note_without_saving_half() {
    let (_dir, store) = new_store();
    let now = 2_000;
    let expires = now + 86_400_000;
    for i in 0..50 {
        store
            .handoff_write(
                "agent-a",
                "note",
                &format!("第 {i} 条便签"),
                Some(expires),
                "dev-1",
                now,
            )
            .unwrap();
    }
    let v50 = version(&store);
    let full = store.handoff_write("agent-a", "note", "第五条一", Some(expires), "dev-1", now);
    assert!(matches!(full, Err(CoreError::HandoffFull(50))), "{full:?}");
    assert_eq!(version(&store), v50, "超限的写入不许落一半");
    assert_eq!(doc(&store).handoffs.len(), 50);
}

#[test]
fn handoff_text_is_capped_by_characters_not_bytes() {
    let (_dir, store) = new_store();
    let now = 2_000;
    let hanzis = "字".repeat(HANDOFF_TEXT_MAX);
    assert_eq!(hanzis.len(), HANDOFF_TEXT_MAX * 3, "汉字是 3 字节");
    let ok = store
        .handoff_write("agent-a", "note", &hanzis, None, "dev-1", now)
        .unwrap();
    let stored = doc(&store);
    let row = stored.handoffs.iter().find(|h| h.id == ok.id).unwrap();
    assert_eq!(row.text.chars().count(), HANDOFF_TEXT_MAX);

    let too_long = "字".repeat(HANDOFF_TEXT_MAX + 1);
    assert!(matches!(
        store.handoff_write("agent-a", "note", &too_long, None, "dev-1", now + 10),
        Err(CoreError::BadPayload(_))
    ));
    for blank in ["", "   ", "\n\t "] {
        assert!(
            matches!(
                store.handoff_write("agent-a", "note", blank, None, "dev-1", now + 20),
                Err(CoreError::BadPayload(_))
            ),
            "{blank:?}"
        );
    }
    assert_eq!(version(&store), ok.version, "被拒的写入不落版本");
}

#[test]
fn handoff_rejects_a_past_expiry_and_an_unknown_kind() {
    let (_dir, store) = new_store();
    let now = 2_000;
    assert!(matches!(
        store.handoff_write("agent-a", "note", "到期点就是现在", Some(now), "dev-1", now),
        Err(CoreError::BadPayload(_))
    ));
    assert!(matches!(
        store.handoff_write("agent-a", "note", "已经过期", Some(now - 1), "dev-1", now),
        Err(CoreError::BadPayload(_))
    ));
    assert!(matches!(
        store.handoff_write("agent-a", "later", "未知种类", None, "dev-1", now),
        Err(CoreError::BadPayload(_))
    ));
    assert_eq!(version(&store), 0, "被拒的写入不许留下半份");
}

/// §2.2：一条"到点但没人关"的提醒不许在下次写时无声消失。
/// `list` 照常返回它（过期与否交给工具面标），写时清理给它留宽限期。
#[test]
fn an_expired_unclosed_note_is_still_listed_and_survives_the_next_write() {
    let (_dir, store) = new_store();
    let now = 2_000;
    let first = store
        .handoff_write("agent-a", "note", "到点了但没人关", Some(now + 100), "dev-1", now)
        .unwrap();

    // 过期之后仍然列得出来：过期不是删除，交给工具面标 `expired`。
    let listed = store.handoff_list("agent-a", now + 200).unwrap();
    assert_eq!(listed.len(), 1, "过期条不许从 list 里消失");
    assert_eq!(listed[0].id, first.id);
    assert!(listed[0].expires_at_ms <= now + 200, "这一条确实已经过期");
    // `list` 是纯读：行还在文档里，版本也不动。
    assert_eq!(doc(&store).handoffs.len(), 1);
    assert_eq!(version(&store), first.version, "list 不许落版本");

    // 宽限期内再写一条：那条过期条不会被写时清理真删。
    store
        .handoff_write("agent-a", "note", "另一条", None, "dev-1", now + 200)
        .unwrap();
    let stored = doc(&store);
    assert!(
        stored.handoffs.iter().any(|h| h.id == first.id),
        "未关闭且过期：宽限期内不许真删"
    );
    assert_eq!(store.handoff_list("agent-a", now + 200).unwrap().len(), 2);
}

/// §2.3：过期超过宽限期（`HANDOFF_EXPIRED_GRACE_MS`）才在写时真删。
#[test]
fn an_expired_unclosed_note_is_pruned_after_the_grace() {
    let (_dir, store) = new_store();
    let now = 2_000;
    let first = store
        .handoff_write("agent-a", "note", "宽限期也过了", Some(now + 100), "dev-1", now)
        .unwrap();

    // 差一天到宽限：还在。
    let before_grace = now + 100 + HANDOFF_EXPIRED_GRACE_MS - 1;
    store
        .handoff_write("agent-a", "note", "宽限期内", None, "dev-1", before_grace)
        .unwrap();
    assert!(
        doc(&store).handoffs.iter().any(|h| h.id == first.id),
        "宽限期内不许真删：{before_grace}"
    );

    // 过了宽限期一格：真删。
    let after_grace = now + 100 + HANDOFF_EXPIRED_GRACE_MS + 1;
    store
        .handoff_write("agent-a", "note", "宽限也过了", None, "dev-1", after_grace)
        .unwrap();
    assert!(
        doc(&store).handoffs.iter().all(|h| h.id != first.id),
        "过期超过宽限期就该被写时清理真删"
    );
}

/// §2.4：总量裁剪在 push **之后**做，落盘后不会出现 201。
#[test]
fn the_payload_never_holds_more_than_the_total_limit() {
    let (_dir, store) = new_store();
    let now = 2_000;
    let mut payload = CorePayload::empty("agent-a");
    payload.handoffs = (0..HANDOFF_TOTAL_LIMIT)
        .map(|i| HandoffItem {
            id: format!("h-{i:012}"),
            kind: "note".to_string(),
            text: format!("旧条 {i}"),
            created_at_ms: now,
            expires_at_ms: now + 86_400_000,
            seen_count: 0,
            last_seen_at_ms: None,
            // 全部关闭：未关闭条数另有 50 条上限，这里只压总量。
            closed_at_ms: Some(now + 1_000 + i as i64),
            closed_reason: Some("done".to_string()),
        })
        .collect();
    let v1 = store.save_doc("agent-a", &payload, "dev-1", now).unwrap();
    assert_eq!(v1, 1);
    assert_eq!(doc(&store).handoffs.len(), HANDOFF_TOTAL_LIMIT);

    store
        .handoff_write("agent-a", "note", "第 201 条", None, "dev-1", now + 10)
        .unwrap();
    let stored = doc(&store);
    assert_eq!(stored.handoffs.len(), HANDOFF_TOTAL_LIMIT, "落盘后不许出现 201");
    assert!(
        stored.handoffs.iter().any(|h| h.text == "第 201 条"),
        "新条必须留下"
    );
    assert!(
        stored.handoffs.iter().all(|h| h.id != "h-000000000000"),
        "裁剪按 closed_at_ms 从最旧的已关闭行删起"
    );
}

/// §2.5：写锁在**读版本之前**就拿住。持锁的写者没放锁时，第二个写者拿到的是
/// SQLite 的 busy 错误（SQL 族），不是"两个人都读到 vN、一个盖掉另一个"。
#[test]
fn save_doc_takes_the_write_lock_before_reading_the_version() {
    let (dir, store) = new_store();
    // 第二个连接持写锁：`BEGIN IMMEDIATE` 就是 `save_doc` 自己开事务的那一句。
    let holder = Connection::open(dir.path().join("core.db")).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();

    let blocked = store.handoff_write("agent-a", "note", "抢不到锁", None, "dev-1", 1_000);
    assert!(
        matches!(blocked, Err(CoreError::Sql(_))),
        "拿不到写锁必须是 SQL 族错误，不是静默成功：{blocked:?}"
    );
    assert_eq!(version(&store), 0, "被挡下的写入不许落版本");
    assert!(
        store.load_doc("agent-a").unwrap().is_none(),
        "被挡下的写入不许留下文档"
    );

    holder.execute_batch("ROLLBACK").unwrap();
    let ok = store
        .handoff_write("agent-a", "note", "锁放了再写", None, "dev-1", 1_000)
        .unwrap();
    assert_eq!(ok.version, 1, "锁释放后第二个写者照常写进去");
    assert_eq!(store.handoff_list("agent-a", 1_000).unwrap().len(), 1);
}

/// §2.1：满额时写 `continue` 不许被 `handoff_full` 拒——它取代旧 continue，未关闭总数不增。
///
/// 工单 §2.1 的原文是"写 50 条 note → 再写 continue ⇒ 成功，未关闭数仍 50，superseded 为空"，
/// 但 50 条 note + 一条新 continue = 51 条未关闭，与"未关闭数仍 50"在算术上不可能同时成立。
/// 这里按 §1.2 的判据落地（上限按**取代之后**的未关闭数判）：满额的那一条就是被取代的 continue，
/// 并额外钉住"满额且无可取代时仍然 handoff_full"这条边界。
#[test]
fn a_full_slate_still_accepts_a_new_continue() {
    let (_dir, store) = new_store();
    let now = 2_000;
    let notes = HANDOFF_OPEN_LIMIT - 1;
    for i in 0..notes {
        store
            .handoff_write("agent-a", "note", &format!("第 {i} 条便签"), None, "dev-1", now)
            .unwrap();
    }
    let old = store
        .handoff_write("agent-a", "continue", "先做 A", None, "dev-1", now)
        .unwrap();
    assert!(old.superseded.is_empty(), "第一条 continue 没有旧条可取代");
    assert_eq!(open_count(&store), HANDOFF_OPEN_LIMIT, "此刻恰好满额");

    // 满额时再写 continue：取代旧条，未关闭总数不增 ⇒ 成功。
    let new = store
        .handoff_write("agent-a", "continue", "接着做 B", None, "dev-1", now + 10)
        .unwrap();
    assert_eq!(new.superseded, vec![old.id.clone()]);
    assert_eq!(open_count(&store), HANDOFF_OPEN_LIMIT, "取代之后仍是 50，不增");

    // 边界：满额且没有可取代的旧条时，新条确实会多一条未关闭 ⇒ 仍然拒。
    let full = store.handoff_write("agent-a", "note", "第五条一", None, "dev-1", now + 20);
    assert!(
        matches!(full, Err(CoreError::HandoffFull(HANDOFF_OPEN_LIMIT))),
        "{full:?}"
    );
    assert_eq!(open_count(&store), HANDOFF_OPEN_LIMIT, "被拒的写入不许落一半");
}

#[test]
fn closing_twice_is_a_no_op_and_does_not_bump_the_version() {
    let (_dir, store) = new_store();
    let now = 2_000;
    let written = store
        .handoff_write("agent-a", "reminder", "记得回消息", None, "dev-1", now)
        .unwrap();
    let first = store
        .handoff_close("agent-a", &written.id, None, "dev-1", now + 10)
        .unwrap();
    assert!(!first.already_closed);
    assert_eq!(first.reason, "manual", "缺省理由是 manual");

    let second = store
        .handoff_close("agent-a", &written.id, Some("done"), "dev-1", now + 20)
        .unwrap();
    assert!(second.already_closed);
    assert_eq!(second.closed_at_ms, first.closed_at_ms);
    assert_eq!(second.reason, "manual", "第二次关闭不许改理由");
    assert_eq!(second.version, first.version, "幂等分支不新增版本");
    assert_eq!(version(&store), first.version);

    assert!(matches!(
        store.handoff_close("agent-a", "h-000000000000", None, "dev-1", now + 30),
        Err(CoreError::NotFound)
    ));
}

/// H3：`handoff_prune` 是写路径之外的唯一物理删除口（`list` 是纯读、只在写时惰性清）。
/// 两条判据：没删到东西 ⇒ 不落版本；删到了 ⇒ 只删过期行，没过期的一条不少。
#[test]
fn prune_removes_only_expired_handoffs_and_skips_the_version_when_there_is_nothing() {
    let (_dir, store) = new_store();
    let now = 1_000;
    let soon = store
        .handoff_write("agent-a", "note", "很快就过期", Some(now + 500), "dev-1", now)
        .unwrap();
    let later = store
        .handoff_write("agent-a", "note", "很久以后才过期", Some(now + 10_000), "dev-1", now)
        .unwrap();
    let version_after_writes = later.version;

    // 一条都还没过期 ⇒ 回执 0，且**不新增版本**（一次空巡检不该在人格历史里留痕）。
    assert_eq!(store.handoff_prune("agent-a", now + 200, "hub").unwrap(), 0);
    assert_eq!(version(&store), version_after_writes, "空巡检不许落版本");

    // 过了 soon 的过期点 ⇒ 只删那一条。
    assert_eq!(store.handoff_prune("agent-a", now + 600, "hub").unwrap(), 1);
    let payload = doc(&store);
    assert_eq!(payload.handoffs.len(), 1);
    assert_eq!(payload.handoffs[0].id, later.id, "没过期的那条必须留下");
    assert!(payload.handoffs.iter().all(|h| h.id != soon.id));
    assert!(version(&store) > version_after_writes, "真删了就要落一个版本");

    // 再跑一次：没有可删的，回执 0。
    assert_eq!(store.handoff_prune("agent-a", now + 600, "hub").unwrap(), 0);
}

#[test]
fn every_operation_persists_a_readable_document_and_versions_never_go_backwards() {
    let (_dir, store) = new_store();
    let mut versions = vec![seed(&store)];

    let (v, proposal) = store
        .persona_propose(
            "agent-a",
            PersonaPatch { core_text: Some("v2".to_string()), ..Default::default() },
            "理由",
            vec!["turn-1".to_string()],
            "dev-1",
            2_000,
        )
        .unwrap();
    versions.push(v);
    versions.push(store.persona_apply("agent-a", &proposal, "dev-1", 2_100).unwrap());
    versions.push(
        store
            .subject_update("agent-a", subject("user-main", true), "dev-1", 2_200)
            .unwrap(),
    );
    versions.push(
        store
            .interaction_touch(
                "agent-a",
                InteractionTouch {
                    last_user_message_at_ms: Some(7_000),
                    current_topic: Some("搬家".to_string()),
                    ..Default::default()
                },
                "dev-1",
                2_300,
            )
            .unwrap(),
    );
    versions.push(
        store
            .affect_update("agent-a", affect("user", "兴奋", 0.55), "dev-1", 2_400)
            .unwrap(),
    );
    versions.push(
        store
            .loop_upsert(
                "agent-a",
                LoopItem {
                    id: "loop-1".to_string(),
                    kind: "promise".to_string(),
                    text: "发搬家清单".to_string(),
                    status: "open".to_string(),
                    due_at_ms: 0,
                },
                "dev-1",
                2_500,
            )
            .unwrap(),
    );
    versions.push(store.loop_close("agent-a", "loop-1", "done", "dev-1", 2_600).unwrap());
    versions.push(
        store
            .episode_pin("agent-a", "ep-1", "一起看房", "dev-1", 2_700)
            .unwrap(),
    );
    versions.push(store.episode_unpin("agent-a", "ep-1", "dev-1", 2_800).unwrap());
    versions.push(
        store
            .persona_reject("agent-a", &proposal, "dev-1", 2_900)
            .unwrap_or_else(|_| {
                // 提案已经 applied，换一条新的来覆盖 reject 路径。
                let (_, id) = store
                    .persona_propose(
                        "agent-a",
                        PersonaPatch::default(),
                        "第二个提案",
                        vec![],
                        "dev-1",
                        2_850,
                    )
                    .unwrap();
                store.persona_reject("agent-a", &id, "dev-1", 2_900).unwrap()
            }),
    );

    for pair in versions.windows(2) {
        assert!(pair[1] > pair[0], "versions must strictly increase: {versions:?}");
    }
    let final_doc = doc(&store);
    assert_eq!(final_doc.agent_id, "agent-a");
    assert_eq!(final_doc.interaction.last_user_message_at_ms, 7_000);
    assert_eq!(final_doc.subjects.len(), 1);
    assert_eq!(
        final_doc.loops.iter().find(|l| l.id == "loop-1").unwrap().status,
        "done"
    );
    assert!(final_doc.pinned_episodes.is_empty());
    // 终态提案都出了队；被拒那条的完整记录在它 propose 的那一版 rev 里（versions[1]）。
    assert!(
        final_doc.persona_proposals.is_empty(),
        "终态提案不许留在载荷里：{:?}",
        final_doc.persona_proposals
    );
    assert!(store
        .load_revision("agent-a", versions[1])
        .unwrap()
        .persona_proposals
        .iter()
        .any(|p| p.id == proposal));
    assert_eq!(version(&store), *versions.last().unwrap());
}

// ── core.db CAS 修复波的测试钉 ────────────────────────────────────────────────

/// 丢更新红线（语义面）：写者 A 通过公开 API 拿到一份载荷之后，写者 B 完成一整个写；
/// A 的变更落地时必须作用在 **B 之后** 的载荷上——闭包内断言看到的载荷含 B 的变更，
/// 落库后 B 的变更与 A 的变更同场活着。
#[test]
fn a_change_lands_on_top_of_the_newest_committed_payload() {
    let (_dir, store) = new_store();
    let v1 = seed(&store);

    // 写者 A：通过公开 API 拿到一份载荷（此刻它手里的视图停在 v1）。
    let stale = doc(&store);
    assert!(stale.persona_proposals.is_empty());

    // 写者 B：完成一整个写（提案落库 → v2）。
    let (v2, b_id) = store
        .persona_propose(
            "agent-a",
            PersonaPatch { core_text: Some("B 想更直接".to_string()), ..Default::default() },
            "B 的理由",
            vec![],
            "dev-2",
            2_000,
        )
        .unwrap();
    assert_eq!(v2, v1 + 1);

    // A 的变更落地：原子原语的闭包必须在写锁内看到 B 的变更，并把 A 的修改叠在其上。
    let mut saw_b = false;
    let v3 = store
        .update_doc("agent-a", "dev-1", 3_000, |payload| {
            saw_b = payload.persona_proposals.iter().any(|p| p.id == b_id);
            payload.persona.core_text = "A 在 B 之后改的".to_string();
            Ok(true)
        })
        .unwrap();
    assert_eq!(v3, v2 + 1);
    assert!(saw_b, "闭包内必须看到 B 已落的提案，否则 A 落盘就会把它整份盖掉");

    let final_doc = doc(&store);
    assert_eq!(final_doc.persona.core_text, "A 在 B 之后改的");
    assert!(
        final_doc.persona_proposals.iter().any(|p| p.id == b_id),
        "B 的提案必须在 A 落盘后仍然活着"
    );
}

/// 丢更新红线（交错面，变异证明的红线）：A 的变更进入读改写窗口时，旁路写者 B 完成
/// 一整个写——要么 B 被 A 的写锁干净挡下（CAS 生效，`BEGIN IMMEDIATE` 先到手），要么
/// B 落了库就必须在 A 落盘后仍然活着。把原子原语临时改回"锁外读 → 改 → `save_doc`"
/// 的旧形状，恰好这一条变红（B 报成功却被 A 的整份落库盖掉）。
#[test]
fn a_concurrent_full_write_is_never_silently_clobbered() {
    let (dir, store) = new_store();
    seed(&store);

    // B 的第二个 store 连接先建好（建连接不参与锁竞争），等 A 的闭包放行。
    let store_b = CoreStore::open(dir.path()).unwrap();

    /// B 旁路写者的整份写出结果。
    enum BOutcome {
        /// 被 A 的写锁挡下（busy），干净失败、不留半份。
        Blocked,
        /// 完成一整个写（带落库的提案 id）。
        Landed(String),
        Other(String),
    }

    let (gate_tx, gate_rx) = mpsc::channel::<()>();
    let (back_tx, back_rx) = mpsc::channel::<BOutcome>();
    let b = std::thread::spawn(move || {
        gate_rx.recv().unwrap();
        let outcome = match store_b.persona_propose(
            "agent-a",
            PersonaPatch { core_text: Some("B 的整份写".to_string()), ..Default::default() },
            "B 的理由",
            vec![],
            "dev-2",
            2_000,
        ) {
            Ok((_, id)) => BOutcome::Landed(id),
            Err(CoreError::Sql(_)) => BOutcome::Blocked,
            Err(e) => BOutcome::Other(e.to_string()),
        };
        back_tx.send(outcome).unwrap();
    });

    // 写者 A 的变更落地：闭包里放行 B、等 B 的整写出结果。这个放行点在旧形状里是
    // "锁外读之后、save 之前"（B 正好插进丢更新窗口），在新形状里是"写锁已到手、
    // 事务未提交"（B 只能撞 busy）。
    let (out_tx, out_rx) = mpsc::channel::<BOutcome>();
    let v = store
        .update_doc("agent-a", "dev-1", 3_000, move |payload| {
            gate_tx.send(()).unwrap();
            out_tx.send(back_rx.recv().unwrap()).unwrap();
            payload.persona.core_text = "A 的修改".to_string();
            Ok(true)
        })
        .unwrap();
    b.join().unwrap();
    let outcome = out_rx.recv().unwrap();

    let final_doc = doc(&store);
    assert_eq!(final_doc.persona.core_text, "A 的修改", "A 的变更必须活着落地");
    match outcome {
        BOutcome::Blocked => {
            // CAS 生效：B 被 A 的写锁挡下，干净失败、不落版本、不留半份。
            assert_eq!(v, 2, "B 被挡下后只应有 A 落的那一版：{v}");
            assert_eq!(version(&store), 2);
            assert!(
                final_doc.persona_proposals.is_empty(),
                "被挡下的写不许留下任何提案：{:?}",
                final_doc.persona_proposals
            );
        }
        BOutcome::Landed(b_id) => {
            // B 落了库就必须活着——被 A 的落盘整份盖掉就是丢更新（旧形状恰好红在这里）。
            assert!(
                final_doc.persona_proposals.iter().any(|p| p.id == b_id),
                "丢更新红线：B 的整份写报了成功，却在 A 的变更落地后被盖掉"
            );
            assert_eq!(version(&store), 3, "B 落 v2、A 落 v3，谁也不盖谁");
        }
        BOutcome::Other(e) => panic!("B 的写因无关原因失败：{e}"),
    }
}

/// 出队钉（工单 ③）：persona_apply/persona_reject 落定终态后，提案从
/// `payload.persona_proposals` 移除；旧版本载荷（提案还完整在、还是 `proposed`）在
/// rev 表里找得回，历史不丢。
#[test]
fn finalized_proposals_leave_the_payload_but_stay_in_revisions() {
    let (_dir, store) = new_store();
    seed(&store);

    // apply 分支。
    let (v2, id_a) = store
        .persona_propose(
            "agent-a",
            PersonaPatch { quirks: Some(vec!["爱用比喻".to_string()]), ..Default::default() },
            "提案 A",
            vec!["turn-1".to_string()],
            "dev-1",
            2_000,
        )
        .unwrap();
    assert!(
        doc(&store).persona_proposals.iter().any(|p| p.id == id_a),
        "前置：proposed 提案在载荷里"
    );
    let v3 = store.persona_apply("agent-a", &id_a, "dev-1", 3_000).unwrap();
    assert_eq!(v3, v2 + 1);
    let after_apply = doc(&store);
    assert!(
        after_apply.persona_proposals.iter().all(|p| p.id != id_a),
        "applied 提案必须出队：{:?}",
        after_apply.persona_proposals
    );
    assert_eq!(after_apply.persona.quirks, vec!["爱用比喻"], "persona 照常落 patch");
    let rev_v2 = store.load_revision("agent-a", v2).unwrap();
    assert!(
        rev_v2
            .persona_proposals
            .iter()
            .any(|p| p.id == id_a && p.status == "proposed"),
        "applied 提案的完整记录在 rev 表的 v2 里"
    );
    assert_eq!(rev_v2.persona.quirks, vec!["喜欢留白"], "v2 的 persona 还没被打补丁");

    // reject 分支。
    let (v4, id_r) = store
        .persona_propose(
            "agent-a",
            PersonaPatch::default(),
            "提案 R",
            vec![],
            "dev-1",
            4_000,
        )
        .unwrap();
    let v5 = store.persona_reject("agent-a", &id_r, "dev-1", 5_000).unwrap();
    assert_eq!(v5, v4 + 1);
    let after_reject = doc(&store);
    assert!(
        after_reject.persona_proposals.iter().all(|p| p.id != id_r),
        "rejected 提案必须出队：{:?}",
        after_reject.persona_proposals
    );
    assert!(
        after_reject.persona.quirks == after_apply.persona.quirks,
        "reject 不动 persona"
    );
    assert!(
        store
            .load_revision("agent-a", v4)
            .unwrap()
            .persona_proposals
            .iter()
            .any(|p| p.id == id_r && p.status == "proposed"),
        "被拒提案的完整记录在 rev 表的 v4 里"
    );

    // 出队之后数组不再无界增长：再来一轮 propose/reject，载荷里的提案数停在 0。
    let (_, id_x) = store
        .persona_propose(
            "agent-a",
            PersonaPatch::default(),
            "提案 X",
            vec![],
            "dev-1",
            6_000,
        )
        .unwrap();
    store.persona_reject("agent-a", &id_x, "dev-1", 7_000).unwrap();
    assert!(
        doc(&store).persona_proposals.is_empty(),
        "终态即离场：载荷里不该再积累提案"
    );
}

/// 旧载荷兼容钉：升级前写入的载荷里可能还挂着 `applied|rejected` 的历史提案——对它们
/// apply/reject 仍然是 `BadPayload`（不重新应用、也不静默清账）；未知 id 仍是 `NotFound`。
#[test]
fn a_terminal_proposal_in_a_legacy_payload_still_refuses_double_processing() {
    let (_dir, store) = new_store();
    seed(&store);
    let mut payload = doc(&store);
    payload.persona_proposals.push(
        serde_json::from_value(json!({
            "id": "legacy-applied",
            "patch": {"core_text": "旧提案"},
            "reason": "升级前的遗留",
            "status": "applied",
            "created_at_ms": 1_000,
            "applied_at_ms": 1_500
        }))
        .unwrap(),
    );
    let v2 = store.save_doc("agent-a", &payload, "dev-1", 2_000).unwrap();

    assert!(matches!(
        store.persona_apply("agent-a", "legacy-applied", "dev-1", 3_000),
        Err(CoreError::BadPayload(_))
    ));
    assert!(matches!(
        store.persona_reject("agent-a", "legacy-applied", "dev-1", 3_000),
        Err(CoreError::BadPayload(_))
    ));
    assert_eq!(version(&store), v2, "拒绝重复处理不落版本");
    assert!(matches!(
        store.persona_apply("agent-a", "no-such-proposal", "dev-1", 3_000),
        Err(CoreError::NotFound)
    ));
    assert_eq!(version(&store), v2);
}
