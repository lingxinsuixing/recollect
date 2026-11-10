use rc_contract::{parse, ExchangeFile, RejectCode};

fn bytes_of(v: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&v).unwrap()
}

#[test]
fn rejects_unsupported_format_version_as_batch() {
    let raw = bytes_of(serde_json::json!({"format_version": 99, "sessions": []}));
    let errs = parse(&raw).unwrap_err();
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].code, RejectCode::FormatVersionUnsupported);
    assert_eq!(errs[0].loc.session_index, 0);
}

#[test]
fn identity_fields_are_copied_verbatim_not_cleaned() {
    // 身份字段逐字节照抄：空格与大小写都不许动
    let raw = bytes_of(serde_json::json!({
        "format_version": 1,
        "sessions": [{
            "session_key": " Key With Spaces ",
            "device_id": "dev-synthetic-1",
            "source_agent": "Qoder",
            "turns": [{"role": "human", "body": "x", "created_at_ms": 1730000000000i64}]
        }]
    }));
    let f: ExchangeFile = parse(&raw).unwrap();
    assert_eq!(f.sessions[0].session_key, " Key With Spaces ");
}

#[test]
fn missing_created_at_is_a_per_turn_reject_not_a_default() {
    let raw = bytes_of(serde_json::json!({
        "format_version": 1,
        "sessions": [{"session_key": "s", "device_id": "d", "source_agent": "a",
                      "turns": [{"role": "human", "body": "x"}]}]
    }));
    let errs = parse(&raw).unwrap_err();
    assert_eq!(errs[0].code, RejectCode::CreatedAtMissing);
    assert_eq!(errs[0].loc.turn_index, Some(0));
}

#[test]
fn unknown_field_is_rejected_on_input_side() {
    // 输入侧拒收、输出侧忽略（计划 Task 2 Step 5）：多一个字段就必须是 Reject。
    // 未知字段的「键名」可以回显（它是键不是正文），「值」绝不进输出（守卫 12）。
    const MYSTERY: &str = "must-never-appear-in-output-4f2a7c9b";
    let raw = bytes_of(serde_json::json!({
        "format_version": 1,
        "sessions": [
            {"session_key": "s0", "device_id": "d", "source_agent": "a",
             "turns": [{"role": "human", "body": "x", "created_at_ms": 1730000000000i64}]},
            {"session_key": "s1", "device_id": "d", "source_agent": "a",
             "turns": [{"role": "human", "body": "y", "created_at_ms": 1730000000001i64},
                       {"role": "agent", "body": "z", "created_at_ms": 1730000000002i64,
                        "export_debug_note": MYSTERY}]}
        ]
    }));
    let errs = parse(&raw).unwrap_err();
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].code, RejectCode::JsonMalformed);
    assert_eq!(errs[0].loc.session_index, 1);
    assert_eq!(errs[0].loc.turn_index, Some(1));
    // detail 回显键名，不回显值
    assert!(errs[0].detail.contains("export_debug_note"), "键名应可回显: {}", errs[0].detail);
    let rendered = format!("{:?} {}", errs[0].code, errs[0].detail);
    assert!(!rendered.contains(MYSTERY), "未知字段的值泄漏进了 reject: {rendered}");
}

#[test]
fn absolute_project_hint_is_rejected() {
    for hint in [
        "D:\recollect",
        "D:/example",
        "/home/u/recollect",
        "../x",
        "C:recollect",
        ".",
        "..",
        " recollect",
    ] {
        let raw = bytes_of(serde_json::json!({
            "format_version": 1,
            "sessions": [{
                "session_key": "s", "device_id": "d", "source_agent": "a",
                "project_hint": hint,
                "turns": [{"role": "human", "body": "x", "created_at_ms": 1730000000000i64}]
            }]
        }));
        let errs = parse(&raw).unwrap_err();
        assert_eq!(errs[0].code, RejectCode::ProjectHintNotLogical, "{hint}");
        assert_eq!(errs[0].loc.session_index, 0);
    }
}

#[test]
fn logical_project_hint_is_accepted() {
    let raw = bytes_of(serde_json::json!({
        "format_version": 1,
        "sessions": [{
            "session_key": "s", "device_id": "d", "source_agent": "a",
            "project_hint": "recollect 记忆服务",
            "turns": [{"role": "human", "body": "x", "created_at_ms": 1730000000000i64}]
        }]
    }));
    let f: ExchangeFile = parse(&raw).unwrap();
    assert_eq!(f.sessions[0].project_hint.as_deref(), Some("recollect 记忆服务"));
}
