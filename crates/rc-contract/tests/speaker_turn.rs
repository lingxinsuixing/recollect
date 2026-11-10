//! D 子 agent 接线：v1 的 **turn 级 `speaker`** 契约（additive）。
//!
//! 三条判据：
//! - `speaker` 可空：缺省与 `null` 都是"没说"（今天的行为），不报错；
//! - `speaker` 给了就原样进 `AcceptedTurn.speaker`，空白折成 `None`（空串不是说话人）；
//! - `deny_unknown_fields` 没有被这一格放松：未知字段照旧整行拒收。
use rc_contract::{normalize, parse};

fn doc(turn_extra: serde_json::Value) -> Vec<u8> {
    let mut turn = serde_json::json!({
        "role": "agent",
        "body": "[子 agent: 实施 X]\\n结论",
        "created_at_ms": 1_800_000_000_000i64
    });
    for (key, value) in turn_extra.as_object().unwrap() {
        turn[key] = value.clone();
    }
    serde_json::to_vec(&serde_json::json!({
        "format_version": 1,
        "sessions": [{
            "session_key": "sess-sub",
            "device_id": "dev-sub",
            "source_agent": "qoder",
            "turns": [turn]
        }]
    }))
    .unwrap()
}

/// `speaker` 缺省与 `null` 都是"这一轮没有独立说话人"：旧生产者一个字节都不用改。
#[test]
fn speaker可空_缺省与null都过契约() {
    for extra in [serde_json::json!({}), serde_json::json!({"speaker": null})] {
        let file = parse(&doc(extra)).expect("speaker 是 additive 的可选字段");
        assert_eq!(file.sessions.len(), 1);
        let rep = normalize(file, 1_800_000_000_100);
        assert_eq!(rep.accepted.len(), 1);
        assert_eq!(rep.accepted[0].speaker, None, "缺省/null = 没有独立说话人");
    }
}

/// 给了就透传；空白折成 `None`（不造无名 agent 锚）。
#[test]
fn speaker透传到accepted_turn且空白折成none() {
    let file = parse(&doc(serde_json::json!({"speaker": "subagent:call-1"}))).unwrap();
    let rep = normalize(file, 1_800_000_000_100);
    assert_eq!(rep.accepted[0].speaker.as_deref(), Some("subagent:call-1"));

    let file = parse(&doc(serde_json::json!({"speaker": "   "}))).unwrap();
    let rep = normalize(file, 1_800_000_000_100);
    assert_eq!(rep.accepted[0].speaker, None, "纯空白不是说话人");
}

/// `deny_unknown_fields` 仍然生效：`speaker` 之外的新字段整行拒收。
#[test]
fn 未知字段仍拒收_speaker不放宽契约() {
    let extra = serde_json::json!({"speaker": "subagent:call-1", "speaker_note": "x"});
    let errs = parse(&doc(extra)).unwrap_err();
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].code, rc_contract::RejectCode::JsonMalformed);
    assert!(
        errs[0].detail.contains("speaker_note"),
        "未知字段的键名可以回显（键不是正文）：{}",
        errs[0].detail
    );
}
