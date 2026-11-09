//! `core_*` 工具面的黑盒判据（工具契约 §2.2/§3/§6，schema §10.3）。
//!
//! 每条用例都起真子进程、真 `core.db`，主语经 `--agent-id` 由启动配置绑定：
//! 预注册走 `rc_core::CoreStore::open(dir).register_agent(...)`（与生产面同一个入口），
//! 之后所有断言只读 JSON-RPC 回执——工具面看不见密文、表名与 key。
//!
//! 两条纪律与 `tools.rs` 相同：一个 `Session` 里跑一串往返；读之前 `ask` 已经 flush。

mod common;
use common::*;
use serde_json::json;

/// 预注册 + 起进程：`--agent-id` 是 core 主语，`--device-id` 是转录/计数的设备主语。
fn core_session(dir: &std::path::Path) -> Session {
    rc_core::CoreStore::open(dir)
        .unwrap()
        .register_agent("agent-a", 0)
        .unwrap();
    let mut s = Session::start_with(dir, &["--device-id", "dev-synthetic", "--agent-id", "agent-a"]);
    let _ = s.ask(&init(1, "qoder-test"));
    s
}

#[test]
fn tools_list_carries_every_core_tool() {
    // 清单的唯一来源是 `capabilities().tools`；`tools/list` 必须与它逐项一致，
    // 而且每个 core 工具都要有 title/description/inputSchema（缺一个 `tool_defs` 就返回 None）。
    let dir = tempfile::tempdir().unwrap();
    let mut s = core_session(dir.path());
    let r = s.ask(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
    let arr = r["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = arr.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, rc_agent::capabilities().tools, "清单漂了：{names:?}");
    let core: Vec<&str> = rc_agent::capabilities()
        .tools
        .iter()
        .copied()
        .filter(|t| t.starts_with("core_"))
        .collect();
    assert_eq!(core.len(), 28, "核心层工具数：{core:?}");
    for name in &core {
        assert!(names.contains(name), "{name} 不在 tools/list：{names:?}");
        let def = arr.iter().find(|t| t["name"] == *name).unwrap();
        assert!(def["title"].is_string(), "{name} 缺 title：{def}");
        assert!(def["description"].is_string(), "{name} 缺 description：{def}");
        assert!(def["inputSchema"].is_object(), "{name} 缺 inputSchema：{def}");
        // 契约 §1.1：任何 schema 里都没有 `agent_id`。
        assert!(def["inputSchema"]["properties"].get("agent_id").is_none(), "{name} 暴露了 agent_id：{def}");
    }
}

#[test]
fn chinese_expression_switch_round_trips_and_injects_into_brief() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = core_session(dir.path());

    let got = call_json(&mut s, 2, "core_chinese_expression_get", json!({}));
    assert_eq!(got["enabled"], json!(false), "{got}");
    assert_eq!(got["version"], json!(0), "{got}");
    assert!(
        got["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r.as_str().unwrap().contains("就")),
        "固定规则里必须包含“就”的用法：{got}"
    );

    let set = call_json(&mut s, 3, "core_chinese_expression_set", json!({"enabled": true}));
    assert_eq!(set["version"], json!(1), "{set}");

    let got = call_json(&mut s, 4, "core_chinese_expression_get", json!({}));
    assert_eq!(got["enabled"], json!(true), "{got}");
    assert_eq!(got["version"], json!(1), "{got}");

    let brief = tool_text(&s.ask(&call(5, "core_brief", json!({"user_id": "user-a"}))));
    assert!(brief.contains("[中文表达优化]"), "{brief}");
    assert!(brief.contains("该用“就”的地方要写出来"), "{brief}");

    let off = call_json(&mut s, 6, "core_chinese_expression_set", json!({"enabled": false}));
    assert_eq!(off["version"], json!(2), "{off}");
    let brief = tool_text(&s.ask(&call(7, "core_brief", json!({"user_id": "user-a"}))));
    assert!(!brief.contains("[中文表达优化]"), "{brief}");
}

#[test]
fn core_brief_render_text_and_interaction_touch_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = core_session(dir.path());

    let plain = s.ask(&call(2, "core_brief", json!({"user_id": "user-a"})));
    assert_eq!(plain["result"]["isError"], json!(false), "{plain}");
    let text = tool_text(&plain);
    assert!(text.contains("常驻简报"), "core_brief 没给简报文本：{text}");
    assert!(text.contains("agent-a"), "简报要标出 caller：{text}");
    // 简报是自然语言，不是载荷 JSON：密文/密钥/表名一个都不许出现。
    for banned in ["ciphertext", "nonce", "core_doc_", "keys/"] {
        assert!(!text.contains(banned), "简报里出现了 {banned}：{text}");
    }

    let touched = call_json(
        &mut s,
        3,
        "core_interaction_touch",
        json!({"touch": {
            "last_user_message_at_ms": 1_700_000_000_123i64,
            "unanswered_user_messages": 2,
            "current_topic": "合成话题"
        }}),
    );
    assert_eq!(touched["version"], json!(1), "{touched}");

    let got = call_json(&mut s, 4, "core_interaction_get", json!({}));
    assert_eq!(got["interaction"]["last_user_message_at_ms"], json!(1_700_000_000_123i64), "{got}");
    assert_eq!(got["interaction"]["unanswered_user_messages"], json!(2), "{got}");
    assert_eq!(got["interaction"]["current_topic"], json!("合成话题"), "{got}");
}

#[test]
fn loop_upsert_list_and_close() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = core_session(dir.path());

    let up = call_json(
        &mut s,
        2,
        "core_loop_upsert",
        json!({"item": {"id": "loop-1", "kind": "promise", "text": "合成承诺", "status": "open"}}),
    );
    assert_eq!(up["version"], json!(1), "{up}");

    let open = call_json(&mut s, 3, "core_loop_list", json!({"status": "open"}));
    assert_eq!(open["loops"].as_array().unwrap().len(), 1, "{open}");
    assert_eq!(open["loops"][0]["id"], json!("loop-1"), "{open}");

    let done_before = call_json(&mut s, 4, "core_loop_list", json!({"status": "done"}));
    assert!(done_before["loops"].as_array().unwrap().is_empty(), "{done_before}");

    let closed = call_json(
        &mut s,
        5,
        "core_loop_close",
        json!({"id": "loop-1", "status": "done"}),
    );
    assert_eq!(closed["version"], json!(2), "{closed}");

    let done_after = call_json(&mut s, 6, "core_loop_list", json!({"status": "done"}));
    assert_eq!(done_after["loops"].as_array().unwrap().len(), 1, "{done_after}");
    assert_eq!(done_after["loops"][0]["status"], json!("done"), "{done_after}");
}

#[test]
fn persona_propose_apply_round_trips_and_l0_patch_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = core_session(dir.path());

    let proposed = call_json(
        &mut s,
        2,
        "core_persona_propose",
        json!({
            "patch": {"core_text": "慢一点，但记住"},
            "reason": "合成理由",
            "evidence": ["turn:合成的证据"]
        }),
    );
    assert_eq!(proposed["version"], json!(1), "{proposed}");
    let id = proposed["proposal_id"].as_str().unwrap().to_string();
    assert_eq!(id.len(), 64, "提案 id 应是 sha256 hex：{id}");

    let applied = call_json(&mut s, 3, "core_persona_apply", json!({"proposal_id": id}));
    assert_eq!(applied["version"], json!(2), "{applied}");

    let got = call_json(&mut s, 4, "core_persona_get", json!({}));
    assert_eq!(got["persona"]["core_text"], json!("慢一点，但记住"), "{got}");
    // 出队（core.db CAS 波）：终态提案不再留在载荷里，get 的 proposals 是待审队列，apply 后为空。
    assert_eq!(got["proposals"], json!([]), "applied 提案必须出队：{got}");

    // L0 不可自改（契约 §6）：带 `l0` 的 patch 在反序列化阶段就失败。
    let bad = call_error_text(
        &mut s,
        5,
        "core_persona_propose",
        json!({"patch": {"l0": ["agent 自改底线"]}, "reason": "合成理由"}),
    );
    assert!(bad.contains("bad_patch"), "带 l0 的 patch 没被拒：{bad}");
    let unchanged = call_json(&mut s, 6, "core_persona_get", json!({}));
    assert_eq!(unchanged["persona"]["core_text"], json!("慢一点，但记住"), "{unchanged}");
}

#[test]
fn subject_and_affect_updates_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = core_session(dir.path());

    let sub = call_json(
        &mut s,
        2,
        "core_subject_update",
        json!({"subject": {
            "subject_id": "user-a", "is_primary": true,
            "profile_text": "合成的主用户", "preferences": {"称呼": "示例主人"},
            "triggers": ["合成触发"], "taboos": ["合成禁忌"]
        }}),
    );
    assert_eq!(sub["version"], json!(1), "{sub}");
    let subs = call_json(&mut s, 3, "core_subject_get", json!({}));
    assert_eq!(subs["subjects"].as_array().unwrap().len(), 1, "{subs}");
    assert_eq!(subs["subjects"][0]["subject_id"], json!("user-a"), "{subs}");
    assert_eq!(subs["subjects"][0]["profile_text"], json!("合成的主用户"), "{subs}");

    let aff = call_json(
        &mut s,
        4,
        "core_affect_update",
        json!({"affect": {
            "subject_kind": "user", "label": "疲惫",
            "valence": -0.4, "arousal": 0.2, "energy": 0.3,
            "confidence": 0.6, "valid_from_ms": 1_700_000_000_000i64,
            "cause": "合成的今天"
        }}),
    );
    assert_eq!(aff["version"], json!(2), "{aff}");
    let affects = call_json(&mut s, 5, "core_affect_get", json!({}));
    assert_eq!(affects["affects"].as_array().unwrap().len(), 1, "{affects}");
    assert_eq!(affects["affects"][0]["label"], json!("疲惫"), "{affects}");
    assert_eq!(affects["affects"][0]["confidence"], json!(0.6), "{affects}");
}

#[test]
fn episode_pin_expand_and_unpin() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = core_session(dir.path());

    let pinned = call_json(
        &mut s,
        2,
        "core_episode_pin",
        json!({"episode_id": "episode:合成", "reason": "合成的钉住理由"}),
    );
    assert_eq!(pinned["version"], json!(1), "{pinned}");

    let expanded = call_json(
        &mut s,
        3,
        "core_episode_expand",
        json!({"episode_id": "episode:合成"}),
    );
    assert_eq!(
        expanded,
        json!({"episode_id": "episode:合成", "pin_reason": "合成的钉住理由"}),
        "{expanded}"
    );

    let unpinned = call_json(
        &mut s,
        4,
        "core_episode_unpin",
        json!({"episode_id": "episode:合成"}),
    );
    assert_eq!(unpinned["version"], json!(2), "{unpinned}");

    let gone = call_error_text(
        &mut s,
        5,
        "core_episode_expand",
        json!({"episode_id": "episode:合成"}),
    );
    assert!(gone.contains("not_found"), "取消钉住后展开要报 not_found：{gone}");
}

#[test]
fn agent_id_argument_and_unregistered_agent_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = core_session(dir.path());

    // 参数带 `agent_id` ⇒ denied（身份由运行时绑定），且回执不回显那个值。
    let text = call_error_text(
        &mut s,
        2,
        "core_brief",
        json!({"user_id": "user-a", "agent_id": "agent-b"}),
    );
    assert!(text.contains("agent_id_forbidden"), "{text}");
    assert!(!text.contains("agent-b"), "回执回显了 agent_id：{text}");

    // 写路径同样拒绝：参数不能把 caller 换掉。
    let write = call_error_text(
        &mut s,
        3,
        "core_episode_pin",
        json!({"episode_id": "episode:合成", "reason": "合成理由", "agent_id": "agent-b"}),
    );
    assert!(write.contains("agent_id_forbidden"), "{write}");
}

/// 治理小刀①（2026-11-07 core.db 治理分诊）：core.db 里没有这个 caller 的注册行时，
/// core_* 分派口**自动注册**（幂等）再执行——消"未注册撞墙"。注册只建表与注册行、
/// 不产内容，是低摩擦不是代写。判据四条：
/// ① 首调 core_* 成功（不再回 unregistered）；
/// ② 注册行真的落了（core_agents 一行、doc_version=0，注册本身不写文档）；
/// ③ 第二次调用幂等——版本不被重置、key 文件不被改写；
/// ④ 注册面只在 core_* 分派口：`recollect_whoami` 在首调 core_* 之前仍如实报
///   registered=false（whoami 是诊断面，不许自己把"未注册"抹掉）。
#[test]
fn an_unregistered_agent_self_registers_on_its_first_core_call_and_stays_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::start_with(
        dir.path(),
        &["--device-id", "dev-synthetic", "--agent-id", "agent-ghost"],
    );
    let _ = s.ask(&init(1, "qoder-test"));

    // ④ 首调 core_* 之前：whoami 如实报未注册（诊断面不自动注册）。
    let before = call_json(&mut s, 2, "recollect_whoami", json!({}));
    assert_eq!(before["registered"], json!(false), "{before}");

    // ① 首调 core_brief：自动注册成功，简报照常给。
    let first = s.ask(&call(3, "core_brief", json!({"user_id": "user-a"})));
    assert_eq!(first["result"]["isError"], json!(false), "首调没自动注册：{first}");
    assert!(tool_text(&first).contains("常驻简报"), "{first}");

    // ② 注册行真的落了：core_agents 一行 agent-ghost，doc_version=0。
    let conn = rusqlite::Connection::open(dir.path().join("core.db")).unwrap();
    let (rows, version): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), \
             COALESCE((SELECT doc_version FROM core_agents WHERE agent_id='agent-ghost'), -1)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(rows, 1, "自动注册必须落一行注册行");
    assert_eq!(version, 0, "注册本身不许写文档：{version}");
    assert_eq!(call_json(&mut s, 4, "recollect_whoami", json!({}))["registered"], json!(true));

    // ③ 写一个版本后再调 core_*：幂等——版本不清零、key 文件不改写。
    let key_path = dir
        .path()
        .join("keys")
        .join(format!("core_{}.key", rc_core::agent_table_suffix("agent-ghost")));
    let key_before = std::fs::read(&key_path).unwrap();
    let wrote = call_json(
        &mut s,
        5,
        "core_chinese_expression_set",
        json!({"enabled": true}),
    );
    assert_eq!(wrote["version"], json!(1), "{wrote}");
    let second = s.ask(&call(6, "core_brief", json!({"user_id": "user-a"})));
    assert_eq!(second["result"]["isError"], json!(false), "{second}");
    let (_, version_after): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), doc_version FROM core_agents WHERE agent_id='agent-ghost'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(version_after, 1, "第二次调用不许把版本清零");
    assert_eq!(
        std::fs::read(&key_path).unwrap(),
        key_before,
        "幂等注册不许改写 key 文件"
    );
}

/// 治理小刀③：relationship 写口（payload 11 段里唯一零写者的那一段）。判据四条：
/// ① 写成功落版本；② `core_interaction_get` 回执带出（§9.1 关系/交互分区读出面的那格）；
/// ③ `core_brief` 的「关系与交互」区带出（渲染 SEC_RELATIONSHIP）；④ 越界值被拒
/// （照 affect 的 0..=1 尺，整份不落）。
#[test]
fn relationship_update_round_trips_into_interaction_get_and_the_brief() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = core_session(dir.path());

    let wrote = call_json(
        &mut s,
        2,
        "core_relationship_update",
        json!({"relationship": {
            "trust": 0.8, "intimacy": 0.6, "tension": 0.1,
            "status": "normal", "text": "最近有点安静", "confidence": 0.7
        }}),
    );
    assert_eq!(wrote["version"], json!(1), "{wrote}");

    // ② 读出带回执：relationship 与 interaction 同分区，一起给。
    let got = call_json(&mut s, 3, "core_interaction_get", json!({}));
    assert_eq!(got["relationship"]["trust"], json!(0.8), "{got}");
    assert_eq!(got["relationship"]["text"], json!("最近有点安静"), "{got}");

    // ③ 简报渲染带出：信任读数（{:.2}）与关系描述都在「关系与交互」区里。
    let brief = tool_text(&s.ask(&call(4, "core_brief", json!({"user_id": "user-a"}))));
    assert!(brief.contains("[关系与交互]"), "{brief}");
    assert!(brief.contains("信任 0.80"), "{brief}");
    assert!(brief.contains("最近有点安静"), "{brief}");

    // ④ 越界值拒绝：整份不落，上一版原样活着。
    let bad = call_error_text(
        &mut s,
        5,
        "core_relationship_update",
        json!({"relationship": {
            "trust": 1.5, "intimacy": 0.6, "tension": 0.1,
            "status": "normal", "text": "越界", "confidence": 0.7
        }}),
    );
    assert!(bad.contains("bad_payload"), "{bad}");
    let unchanged = call_json(&mut s, 6, "core_interaction_get", json!({}));
    assert_eq!(unchanged["relationship"]["trust"], json!(0.8), "{unchanged}");
}

#[test]
fn retired_vote_and_default_export_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, "qoder-test"));

    // 契约 §3：agent 不能退休记忆；hint 也不再列 retired。
    let retired = call_error_text(
        &mut s,
        2,
        "recollect_vote",
        json!({"target": format!("card:{}", "f".repeat(64)), "stance": "retired"}),
    );
    assert!(retired.contains("agent_cannot_retire"), "{retired}");
    assert!(retired.ends_with("retirement is user/operator only"), "{retired}");

    // 导出默认关（user/operator 才导出）。
    let denied = call_error_text(&mut s, 3, "recollect_export", json!({}));
    assert!(denied.contains("export_disabled"), "{denied}");

    // 显式 `--enable-export` 才放行，且仍受条数上限约束。
    let mut open = Session::start_with(dir.path(), &["--device-id", "dev-synthetic", "--enable-export"]);
    let _ = open.ask(&init(1, "qoder-test"));
    let ok = open.ask(&call(2, "recollect_export", json!({})));
    assert_eq!(ok["result"]["isError"], json!(false), "{ok}");
    let doc = tool_text(&ok);
    assert!(doc.starts_with("# "), "开关打开后导出件不是 Markdown 头：{doc}");
    let too_many = call_error_text(&mut open, 3, "recollect_export", json!({"limit": 201}));
    assert!(too_many.contains("limit_too_large"), "{too_many}");
    let limited = open.ask(&call(4, "recollect_export", json!({"limit": 5})));
    assert_eq!(limited["result"]["isError"], json!(false), "{limited}");
    assert!(tool_text(&limited).starts_with("# "), "带 limit 的导出件形状漂了");
}

/// §2.6：过期与否由工具面标。过期条照常出现在 `list` 里，只是多一格 `expired=true`。
#[test]
fn handoff_list_marks_expired_items() {
    let dir = tempfile::tempdir().unwrap();
    // 用极早的时间戳造一条"到点但没人关"的条：工具面的 now 取系统时钟，必然已经过期。
    // （走工具面写不出来——`expires_at_ms` 必须晚于 now，这是另一条用例钉的事。）
    let pre = rc_core::CoreStore::open(dir.path()).unwrap();
    pre.register_agent("agent-a", 1_000).unwrap();
    let stale = pre
        .handoff_write("agent-a", "note", "早就到点了", Some(2_000), "dev-test", 1_000)
        .unwrap();
    let live = pre
        .handoff_write("agent-a", "note", "还早", Some(i64::MAX / 2), "dev-test", 1_000)
        .unwrap();

    let mut s = Session::start_with(
        dir.path(),
        &["--device-id", "dev-synthetic", "--agent-id", "agent-a"],
    );
    let _ = s.ask(&init(1, "qoder-test"));

    let listed = call_json(&mut s, 2, "core_handoff_list", json!({}));
    let items = listed["handoffs"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{listed}");
    let stale_row = items.iter().find(|h| h["id"] == json!(stale.id)).unwrap();
    assert_eq!(stale_row["expired"], json!(true), "过期条没被标出来：{listed}");
    assert_eq!(stale_row["expires_at_ms"], json!(2_000), "{listed}");
    let live_row = items.iter().find(|h| h["id"] == json!(live.id)).unwrap();
    assert_eq!(live_row["expired"], json!(false), "{listed}");
}

/// §2.7：`expires_at_ms` 必须是整数毫秒；字符串/浮点/布尔一律 `bad_expires`，
/// 不许静默回落成 7 天默认（那会把"今天到期"变成"下周才出现"）。
#[test]
fn handoff_write_rejects_a_non_integer_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = core_session(dir.path());

    for (id, value) in [
        (2i64, json!("明天")),
        (3, json!(1.5)),
        (4, json!(true)),
        (5, json!([1_700_000_000_000i64])),
    ] {
        let text = call_error_text(
            &mut s,
            id,
            "core_handoff_write",
            json!({"kind": "note", "text": "形状不对的有效期", "expires_at_ms": value}),
        );
        assert!(text.contains("bad_expires"), "{value} 没被拒：{text}");
        assert!(
            text.contains("expires_at_ms must be an integer epoch in milliseconds"),
            "{text}"
        );
    }

    // 被拒的写入一条都不许落盘。
    let listed = call_json(&mut s, 6, "core_handoff_list", json!({}));
    assert!(
        listed["handoffs"].as_array().unwrap().is_empty(),
        "被拒的写入落了条：{listed}"
    );

    // 整数照常收：同一格里给整型毫秒。
    let ok = call_json(
        &mut s,
        7,
        "core_handoff_write",
        json!({"kind": "note", "text": "整数有效期", "expires_at_ms": 4_102_444_800_000i64}),
    );
    assert!(ok["id"].as_str().unwrap().starts_with("h-"), "{ok}");
}

/// 交接条往返（原有那条用例的邻居）：`write → list → close` 三格形状不变。
#[test]
fn handoff_tools_round_trip_over_jsonrpc() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = core_session(dir.path());

    let written = call_json(
        &mut s,
        2,
        "core_handoff_write",
        json!({"kind": "continue", "text": "下次从 C4 开始：先跑 ops 用例"}),
    );
    let id = written["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("h-"), "{written}");
    assert_eq!(written["superseded"], json!([]), "{written}");

    let listed = call_json(&mut s, 3, "core_handoff_list", json!({}));
    assert_eq!(listed["handoffs"].as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(
        listed["handoffs"][0]["text"],
        json!("下次从 C4 开始：先跑 ops 用例"),
        "{listed}"
    );
    assert_eq!(listed["handoffs"][0]["kind"], json!("continue"), "{listed}");

    let closed = call_json(&mut s, 4, "core_handoff_close", json!({"id": id}));
    assert_eq!(closed["id"], json!(id), "{closed}");
    assert_eq!(closed["reason"], json!("manual"), "{closed}");
    assert_eq!(closed["already_closed"], json!(false), "{closed}");

    let empty = call_json(&mut s, 5, "core_handoff_list", json!({}));
    assert!(
        empty["handoffs"].as_array().unwrap().is_empty(),
        "关闭后不该再列出：{empty}"
    );

    // 未知 kind 在工具面就是 validation 错误（闭集校验的家在 rc-core，这里只认它的码）。
    let bad = call_error_text(
        &mut s,
        6,
        "core_handoff_write",
        json!({"kind": "later", "text": "未知种类"}),
    );
    assert!(bad.contains("validation:"), "{bad}");
}

#[test]
fn handoff_notes_reach_the_brief() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = core_session(dir.path());

    let written = call_json(
        &mut s,
        2,
        "core_handoff_write",
        json!({"kind": "continue", "text": "下次接着做 H3 的 prune"}),
    );
    assert!(written["id"].as_str().unwrap().starts_with("h-"), "{written}");

    // 缺省 include：continue 自动出现在开场简报里。
    let brief = tool_text(&s.ask(&call(3, "core_brief", json!({"user_id": "user-a"}))));
    assert!(brief.contains("[交接（给下个会话）]"), "{brief}");
    assert!(brief.contains("下次接着做 H3 的 prune"), "{brief}");

    // note 默认不出现。
    let _ = call_json(
        &mut s,
        4,
        "core_handoff_write",
        json!({"kind": "note", "text": "这条备注默认不该出现"}),
    );
    let brief = tool_text(&s.ask(&call(5, "core_brief", json!({"user_id": "user-a"}))));
    assert!(!brief.contains("这条备注默认不该出现"), "{brief}");

    // 显式 include 才出现。
    let brief = tool_text(&s.ask(&call(
        6,
        "core_brief",
        json!({"user_id": "user-a", "include": ["continue", "note"]}),
    )));
    assert!(brief.contains("这条备注默认不该出现"), "{brief}");
    assert!(brief.contains("下次接着做 H3 的 prune"), "{brief}");

    // 闭集外 ⇒ validation("bad_kind", …)。
    let bad = call_error_text(
        &mut s,
        7,
        "core_brief",
        json!({"user_id": "user-a", "include": ["later"]}),
    );
    assert!(bad.contains("bad_kind"), "{bad}");
    assert!(
        bad.contains("include takes continue|due|reminder|note"),
        "{bad}"
    );
}

/// W36 细案 §7-3 的真子进程闭环：append → core_brief 索引栏可见 → expand 取回全文；
/// search 空命中是合法空读数（isError=false）；list 回执零正文字段（没有 body 键）。
#[test]
fn notebook_append_brief_expand_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = core_session(dir.path());

    let appended = call_json(
        &mut s,
        2,
        "core_notebook_append",
        json!({
            "body": "W36 闭环用例的小本本正文：明天从索引栏对账",
            "title": "闭环标题",
            "tags": ["w36", "闭环"]
        }),
    );
    let id = appended["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("n-"), "小本本句柄形状：{appended}");
    assert!(appended["created_at_ms"].is_i64(), "{appended}");

    // 索引栏可见：core_brief 出「[小本本]」区，行形 `- {id}｜{title}｜{tags}｜{相对时间}`。
    let brief = tool_text(&s.ask(&call(3, "core_brief", json!({"user_id": "user-a"}))));
    assert!(brief.contains("[小本本]"), "{brief}");
    assert!(brief.contains(&id), "索引栏没有新条的 id：{brief}");
    assert!(brief.contains("闭环标题"), "{brief}");
    assert!(brief.contains("w36,闭环"), "索引栏的标签行：{brief}");
    // 索引栏是投影不是全文：正文不出现在简报里。
    assert!(!brief.contains("明天从索引栏对账"), "brief 泄了正文：{brief}");

    // expand 取回全文逐字节。
    let full = call_json(&mut s, 4, "core_notebook_expand", json!({"id": id}));
    assert_eq!(full["title"], json!("闭环标题"), "{full}");
    assert_eq!(full["tags"], json!(["w36", "闭环"]), "{full}");
    assert_eq!(
        full["body"],
        json!("W36 闭环用例的小本本正文：明天从索引栏对账"),
        "{full}"
    );

    // list 回执零正文字段：notes 里没有 body（也没有 source_turn_uid）。
    let listed = call_json(&mut s, 5, "core_notebook_list", json!({}));
    let notes = listed["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 1, "{listed}");
    assert!(notes[0].get("body").is_none(), "list 回执带了 body：{listed}");
    assert!(notes[0].get("source_turn_uid").is_none(), "{listed}");
    assert_eq!(notes[0]["id"], json!(id), "{listed}");
    assert_eq!(listed["unlisted"], json!(0), "{listed}");

    // search 空命中：`call_json` 已断 isError=false——空读数不是错误（照 no_rows 纪律）。
    let empty = call_json(&mut s, 6, "core_notebook_search", json!({"query": "绝不会出现的词"}));
    assert!(empty["notes"].as_array().unwrap().is_empty(), "{empty}");

    // search 命中：多词「或」，任一词命中即可；命中读数同样零正文。
    let hit = call_json(&mut s, 7, "core_notebook_search", json!({"query": "不存在的词 闭环"}));
    assert_eq!(hit["notes"].as_array().unwrap().len(), 1, "{hit}");
    assert!(hit["notes"][0].get("body").is_none(), "{hit}");

    // not_found：句柄形状没问题只是指不到东西 ⇒ not_found 空读数（照 core_episode_expand）。
    let gone = call_error_text(
        &mut s,
        8,
        "core_notebook_expand",
        json!({"id": "n-000000000000"}),
    );
    assert!(gone.contains("not_found"), "{gone}");
}

/// 契约 §1.1 对小本本写路径同样适用：参数带 `agent_id` 即 denied，身份由运行时绑定。
#[test]
fn notebook_write_refuses_an_agent_id_argument() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = core_session(dir.path());
    let text = call_error_text(
        &mut s,
        2,
        "core_notebook_append",
        json!({"body": "越权正文", "agent_id": "agent-b"}),
    );
    assert!(text.contains("agent_id_forbidden"), "{text}");
    assert!(!text.contains("越权正文"), "错误回执回显了正文：{text}");
    // 被拒的写一条都不落。
    let listed = call_json(&mut s, 3, "core_notebook_list", json!({}));
    assert!(listed["notes"].as_array().unwrap().is_empty(), "{listed}");
}

/// 形状校验的家在 rc-core；工具面只透传，形状错的整条不落（细案 §5）。
#[test]
fn notebook_append_shape_errors_come_from_rc_core() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = core_session(dir.path());

    // body 超 2000 字符（按字符不按字节）：2001 个汉字被拒。
    let long_body: String = "长".repeat(2_001);
    let too_long = call_error_text(
        &mut s,
        2,
        "core_notebook_append",
        json!({"body": long_body}),
    );
    assert!(too_long.contains("bad_payload"), "{too_long}");

    // tags 不是字符串数组 ⇒ 工具面解析层就拒（bad_tags），不透传。
    let bad_tags = call_error_text(
        &mut s,
        3,
        "core_notebook_append",
        json!({"body": "正文", "tags": ["形", 1]}),
    );
    assert!(bad_tags.contains("bad_tags"), "{bad_tags}");

    // 空白正文在工具面就折成 missing_argument（`required_str` 的既有语义：空白按"没给"），
    // rc-core 的"body is empty"那把尺在工具面上测不到——它属于 rc-core 自己的用例。
    let blank = call_error_text(&mut s, 4, "core_notebook_append", json!({"body": "  "}));
    assert!(blank.contains("missing_argument"), "{blank}");

    // 9 条标签 ⇒ rc-core 的 BadPayload（条数那把尺的家在 rc-core，工具面只透传）。
    let tags: Vec<String> = (0..9).map(|i| format!("t{i}")).collect();
    let too_many = call_error_text(
        &mut s,
        5,
        "core_notebook_append",
        json!({"body": "正文", "tags": tags}),
    );
    assert!(too_many.contains("bad_payload"), "{too_many}");

    // limit 超上限 ⇒ bad_limit（同 handoff 口径）。
    let limit = call_error_text(
        &mut s,
        6,
        "core_notebook_list",
        json!({"limit": 201}),
    );
    assert!(limit.contains("bad_limit"), "{limit}");

    // 全部被拒的写一条都不落：list 仍是空的。
    let listed = call_json(&mut s, 7, "core_notebook_list", json!({}));
    assert!(listed["notes"].as_array().unwrap().is_empty(), "{listed}");
}

#[test]
fn two_agents_do_not_see_each_others_core() {
    let dir = tempfile::tempdir().unwrap();
    let store = rc_core::CoreStore::open(dir.path()).unwrap();
    store.register_agent("agent-a", 0).unwrap();
    store.register_agent("agent-b", 0).unwrap();

    let mut a = Session::start_with(
        dir.path(),
        &["--device-id", "dev-synthetic", "--agent-id", "agent-a"],
    );
    let _ = a.ask(&init(1, "qoder-test"));
    let mut b = Session::start_with(
        dir.path(),
        &["--device-id", "dev-synthetic", "--agent-id", "agent-b"],
    );
    let _ = b.ask(&init(1, "qoder-test"));

    let proposed = call_json(
        &mut a,
        2,
        "core_persona_propose",
        json!({"patch": {"core_text": "A-ONLY-MARKER"}, "reason": "合成理由"}),
    );
    let id = proposed["proposal_id"].as_str().unwrap().to_string();
    let _ = call_json(&mut a, 3, "core_persona_apply", json!({"proposal_id": id}));

    let a_persona = call_json(&mut a, 4, "core_persona_get", json!({}));
    assert_eq!(a_persona["persona"]["core_text"], json!("A-ONLY-MARKER"), "{a_persona}");

    let b_persona = call_json(&mut b, 2, "core_persona_get", json!({}));
    assert_ne!(b_persona["persona"]["core_text"], json!("A-ONLY-MARKER"), "{b_persona}");
    let b_brief = tool_text(&b.ask(&call(3, "core_brief", json!({"user_id": "user-b"}))));
    assert!(!b_brief.contains("A-ONLY-MARKER"), "B 看到了 A 的 core：{b_brief}");
}
