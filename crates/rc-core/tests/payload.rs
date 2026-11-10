//! §9.2 逻辑载荷：JSON 往返、旧载荷默认值与结构自检。

use rc_core::{
    Affect, CoreError, CorePayload, LoopItem, Persona, PinnedEpisode, Relationship, Subject,
    SCHEMA_VERSION,
};
use serde_json::json;

#[test]
fn old_payload_without_chinese_expression_defaults_to_off() {
    let p: CorePayload = serde_json::from_value(json!({"agent_id": "agent-a"})).unwrap();
    assert!(!p.chinese_expression.enabled, "旧载荷必须默认关闭");
    assert_eq!(p.chinese_expression.version, 0);
    assert_eq!(p.chinese_expression.updated_at_ms, 0);
}

fn rich_payload() -> CorePayload {
    let mut p = CorePayload::empty("agent-a");
    p.schema_version = SCHEMA_VERSION;
    p.l0 = vec!["边界：不冒充人类".to_string(), "诚实：不确定就说".to_string()];
    p.persona = Persona {
        core_text: "慢热、记得小事".to_string(),
        traits: [
            ("warmth".to_string(), json!(0.7)),
            ("verbosity".to_string(), json!("low")),
        ]
        .into_iter()
        .collect(),
        style: [("punct".to_string(), json!("…"))].into_iter().collect(),
        quirks: vec!["喜欢用比喻".to_string()],
        boundaries: vec!["不替用户做决定".to_string()],
        voice_examples: vec!["你回来了。".to_string()],
    };
    p.subjects = vec![Subject {
        subject_id: "user-main".to_string(),
        is_primary: true,
        profile_text: "夜猫子".to_string(),
        preferences: [("tea".to_string(), json!("oolong"))].into_iter().collect(),
        triggers: vec!["被忽略".to_string()],
        taboos: vec!["公开隐私".to_string()],
    }];
    p.interaction.last_user_message_at_ms = 100;
    p.interaction.last_agent_initiated_at_ms = 90;
    p.interaction.unanswered_user_messages = 2;
    p.interaction.attention_debt_ms = 60_000;
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
            confidence: 0.5,
            valid_from_ms: 50,
            cause: "加班".to_string(),
        },
        Affect {
            subject_kind: "agent".to_string(),
            label: "专注".to_string(),
            valence: 0.3,
            arousal: 0.4,
            energy: 0.7,
            confidence: 0.6,
            valid_from_ms: 55,
            cause: String::new(),
        },
    ];
    p.loops = vec![LoopItem {
        id: "loop-1".to_string(),
        kind: "promise".to_string(),
        text: "下次带那本书".to_string(),
        status: "open".to_string(),
        due_at_ms: 1_000,
    }];
    p.pinned_episodes = vec![PinnedEpisode {
        episode_id: "ep-1".to_string(),
        pin_reason: "第一次一起熬夜".to_string(),
    }];
    p
}

#[test]
fn json_round_trip_preserves_every_field() {
    let payload = rich_payload();
    payload.validate().unwrap();
    let text = serde_json::to_string(&payload).unwrap();
    let back: CorePayload = serde_json::from_str(&text).unwrap();
    assert_eq!(back, payload);

    // 设计 §9.2 的段名逐字在 JSON 里。
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    for key in [
        "schema_version",
        "agent_id",
        "l0",
        "persona",
        "subjects",
        "interaction",
        "relationship",
        "affects",
        "loops",
        "pinned_episodes",
    ] {
        assert!(v.get(key).is_some(), "missing {key}");
    }
    assert_eq!(v["persona"]["core_text"], json!("慢热、记得小事"));
    assert_eq!(v["affects"][0]["subject_kind"], json!("user"));
    assert_eq!(v["pinned_episodes"][0]["episode_id"], json!("ep-1"));
}

#[test]
fn an_old_payload_without_collections_loads_with_defaults() {
    let p: CorePayload = serde_json::from_str(r#"{"agent_id":"agent-a"}"#).unwrap();
    assert_eq!(p.schema_version, SCHEMA_VERSION);
    assert_eq!(p.agent_id, "agent-a");
    assert!(p.l0.is_empty());
    assert!(p.persona.core_text.is_empty());
    assert!(p.persona.traits.is_empty());
    assert!(p.subjects.is_empty());
    assert_eq!(p.interaction.unanswered_user_messages, 0);
    assert!(p.interaction.current_topic.is_empty());
    assert_eq!(p.relationship.trust, 0.5);
    assert_eq!(p.relationship.intimacy, 0.5);
    assert_eq!(p.relationship.tension, 0.0);
    assert_eq!(p.relationship.status, "normal");
    assert_eq!(p.relationship.confidence, 0.6);
    assert!(p.affects.is_empty());
    assert!(p.loops.is_empty());
    assert!(p.pinned_episodes.is_empty());
    p.validate().unwrap();
}

#[test]
fn empty_payload_is_valid_and_carries_the_identity() {
    let p = CorePayload::empty("agent-a");
    assert_eq!(p.schema_version, SCHEMA_VERSION);
    assert_eq!(p.agent_id, "agent-a");
    assert_eq!(p, {
        // 同一份期望载荷：`..Default::default()` 与逐字段赋值等价，clippy 只认前者的形状。
        CorePayload { agent_id: "agent-a".to_string(), ..CorePayload::default() }
    });
    p.validate().unwrap();
}

#[test]
fn validate_rejects_structural_confusion() {
    let p = CorePayload::empty("");
    assert!(matches!(p.validate(), Err(CoreError::BadPayload(_))));

    let mut duplicate = CorePayload::empty("agent-a");
    for id in ["loop-1", "loop-1"] {
        duplicate.loops.push(LoopItem {
            id: id.to_string(),
            ..LoopItem::default()
        });
    }
    assert!(matches!(
        duplicate.validate(),
        Err(CoreError::BadPayload(_))
    ));

    let mut bad_kind = CorePayload::empty("agent-a");
    bad_kind.affects.push(affect("system"));
    assert!(matches!(bad_kind.validate(), Err(CoreError::BadPayload(_))));

    let mut no_episode = CorePayload::empty("agent-a");
    no_episode.pinned_episodes.push(PinnedEpisode::default());
    assert!(matches!(
        no_episode.validate(),
        Err(CoreError::BadPayload(_))
    ));

    let mut ok = CorePayload::empty("agent-a");
    ok.affects.push(affect("agent"));
    ok.validate().unwrap();
}

fn affect(subject_kind: &str) -> Affect {
    Affect {
        subject_kind: subject_kind.to_string(),
        label: String::new(),
        valence: 0.0,
        arousal: 0.0,
        energy: 0.0,
        confidence: 0.6,
        valid_from_ms: 0,
        cause: String::new(),
    }
}
