//! 波二刀 A（设计 `2026-11-08-g1-reuse-lesson-design.md` §波次二）：`recollect_reuse` 的
//! 工具面黑盒判据。
//!
//! 一条进程一串往返（`common::Session` 的既有纪律），断言只读 JSON-RPC 回执与库里的行数：
//! - 工具注册面：`tools/list` 与 `capabilities().tools` 同源（清单多一个少一个当场红），
//!   含 `recollect_reuse` 且总数 = 39 + 1；
//! - 闭集在**工具面**这一层拒（写口那一层由 `rc-memory/tests/reuse_event.rs` 钉）；
//! - handle→card 解析失败不写（形状坏 / 锚句柄 / 悬空卡三种都各断一次）；
//! - append-only：同卡同 outcome 两次调用 = 两行；
//! - 回执带卡与 outcome，**不带正文**。

mod common;
use common::*;
use serde_json::json;

/// 一张对 `agent` 可见的卡（owner=该 agent，`private_agent` ⇒ 规则 4 "owner 永远能读自己"）。
/// 白盒播种：这里证的不是落卡链路，是复用反馈的写入口。
fn seed_card(data: &std::path::Path, agent: &str, id: &str) {
    let root = rc_store::Root::open(data).unwrap();
    let conn = root.memory().conn();
    conn.execute(
        "INSERT INTO card(id,kind,title,text,scope_kind,scope_key,as_of_ms,
                          source_agent,device_id,created_at_ms,norm_version)
         VALUES(?1,'decision','复用工具面夹具','正文里有一句不该出现在回执里的话',
                'project','proj-reuse',1,?2,'dev-synthetic',1,1)",
        rusqlite::params![id, agent],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                    created_by_agent, visibility, retention, scope_kind,
                                    scope_key, source_agent, device_id, created_at_ms)
         VALUES('card', ?1, NULL, ?2, ?2, 'private_agent', 'default', 'project',
                'proj-reuse', ?2, 'dev-synthetic', 1)",
        rusqlite::params![id, agent],
    )
    .unwrap();
}

fn reuse_rows(data: &std::path::Path) -> Vec<(String, String, Option<String>, String)> {
    let root = rc_store::Root::open(data).unwrap();
    let mut st = root
        .memory()
        .conn()
        .prepare("SELECT card_id, outcome, note, by_agent FROM card_reuse_event ORDER BY id")
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

/// `recollect_reuse` 的工具定义必须在 `tools/list` 里，且清单与 `capabilities()` 逐项一致。
/// 总数钉在 41（波二刀 A 之前是 39，W39 刀 B 的 `recollect_turn` 再加一）：
/// 每加一件工具都要有**一条**改动这个数的读数，这里就是那一处。
#[test]
fn tools_list_registers_recollect_reuse_and_the_count_is_one_more() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, "qoder-test"));
    let r = s.ask(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
    let arr = r["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = arr.iter().map(|t| t["name"].as_str().unwrap()).collect();
    // 唯一来源的对撞：两份清单漂一个名字这条就红（照 `tools.rs` 的既有镜像用例）。
    assert_eq!(names, rc_agent::capabilities().tools, "清单漂了：{names:?}");
    assert_eq!(names.len(), 41, "工具总数必须是 40 + 1（W39 刀 B）：{names:?}");
    let recollect: Vec<&&str> = names.iter().filter(|n| n.starts_with("recollect_")).collect();
    assert_eq!(recollect.len(), 13, "recollect_* 工具数必须 12 + 1：{recollect:?}");
    assert!(names.contains(&"recollect_reuse"), "recollect_reuse 不在 tools/list：{names:?}");

    let def = arr.iter().find(|t| t["name"] == "recollect_reuse").unwrap();
    for key in ["title", "description", "inputSchema"] {
        assert!(def[key].is_string() || def[key].is_object(), "recollect_reuse 缺 {key}：{def}");
    }
    // 与 vote 分轴：复用工具的 schema 与描述都不许出现 vote 的 stance 词表。
    let schema = &def["inputSchema"];
    assert_eq!(
        schema["required"],
        json!(["handle", "outcome"]),
        "复用工具的形参是 handle/outcome：{schema}"
    );
    assert_eq!(
        schema["properties"]["outcome"]["enum"],
        json!(["worked", "failed", "partial", "mismatch", "stale"]),
        "outcome 的声明面必须是五档闭集：{schema}"
    );
    assert!(schema["properties"].get("stance").is_none(), "复用工具混进了 vote 的 stance：{schema}");
    let desc = def["description"].as_str().unwrap();
    assert!(desc.contains("recollect_vote"), "描述必须点明与 vote 分轴：{desc}");
}

#[test]
fn unknown_outcome_is_refused_on_the_tool_face_and_nothing_is_written() {
    let dir = tempfile::tempdir().unwrap();
    seed_card(dir.path(), "qoder-test", "c-bad-outcome");
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, "qoder-test"));
    // 闭集精确匹配：大小写变体与近义词都不收（与 vote 的 bad_stance 同一把尺子）。
    for bad in ["Worked", "helped", "成功", ""] {
        let text = call_error_text(
            &mut s,
            2,
            "recollect_reuse",
            json!({"handle": "card:c-bad-outcome", "outcome": bad}),
        );
        // 空串走 `required_str` 的 missing_argument，其余走闭集。
        assert!(
            text.contains("bad_outcome") || text.contains("missing_argument"),
            "未知 outcome {bad:?} 的拒法漂了：{text}"
        );
    }
    drop(s);
    assert!(reuse_rows(dir.path()).is_empty(), "被拒的反馈在库里留下了行");
}

#[test]
fn unresolvable_handles_are_refused_and_nothing_is_written() {
    let dir = tempfile::tempdir().unwrap();
    seed_card(dir.path(), "qoder-test", "c-ok");
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, "qoder-test"));
    // ① 形状坏：不是 "<table>:<id>"。
    let bad = call_error_text(
        &mut s,
        2,
        "recollect_reuse",
        json!({"handle": "not-a-handle", "outcome": "worked"}),
    );
    assert!(bad.contains("bad_handle"), "形状坏必须报 bad_handle：{bad}");
    // ② 锚句柄：复用只对卡有意义。
    let anchor = call_error_text(
        &mut s,
        3,
        "recollect_reuse",
        json!({"handle": format!("anchor:{}", "f".repeat(64)), "outcome": "worked"}),
    );
    assert!(anchor.contains("bad_handle"), "锚句柄必须报 bad_handle：{anchor}");
    // ③ 悬空卡：形状合法但行不存在 ⇒ 与"被读闸拒"同形（存在性不外泄）。
    let ghost = call_error_text(
        &mut s,
        4,
        "recollect_reuse",
        json!({"handle": format!("card:{}", "e".repeat(64)), "outcome": "worked"}),
    );
    assert!(ghost.contains("empty:row_absent"), "悬空卡必须报 row_absent：{ghost}");
    drop(s);
    assert!(reuse_rows(dir.path()).is_empty(), "解析失败的句柄在库里留下了行");
}

#[test]
fn a_successful_reuse_writes_one_event_and_the_receipt_carries_no_body() {
    let dir = tempfile::tempdir().unwrap();
    seed_card(dir.path(), "qoder-test", "c-good");
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, "qoder-test"));
    let v = call_json(
        &mut s,
        2,
        "recollect_reuse",
        json!({"handle": "card:c-good", "outcome": "worked", "note": "答上了"}),
    );
    assert_eq!(v["card"], json!("card:c-good"), "回执要带卡：{v}");
    assert_eq!(v["outcome"], json!("worked"), "回执要带 outcome：{v}");
    assert!(v["event_id"].as_i64().is_some_and(|id| id > 0), "回执要带事件 id：{v}");
    assert!(v["at_ms"].as_i64().is_some(), "回执要带时间：{v}");
    // 守卫 12：回执不许带卡的正文。
    let text = v.to_string();
    assert!(!text.contains("不该出现在回执里"), "回执带出了卡的正文：{text}");
    drop(s);
    let rows = reuse_rows(dir.path());
    assert_eq!(rows.len(), 1, "成功调用必须写一行：{rows:?}");
    assert_eq!(rows[0].0, "c-good");
    assert_eq!(rows[0].1, "worked");
    assert_eq!(rows[0].2.as_deref(), Some("答上了"));
    assert_eq!(rows[0].3, "qoder-test", "by_agent 必须是调用方的身份（运行时绑定，不来自参数）");
}

#[test]
fn reporting_the_same_card_with_the_same_outcome_twice_is_two_rows() {
    let dir = tempfile::tempdir().unwrap();
    seed_card(dir.path(), "qoder-test", "c-twice");
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, "qoder-test"));
    let a = call_json(
        &mut s,
        2,
        "recollect_reuse",
        json!({"handle": "card:c-twice", "outcome": "worked"}),
    );
    let b = call_json(
        &mut s,
        3,
        "recollect_reuse",
        json!({"handle": "card:c-twice", "outcome": "worked"}),
    );
    assert_ne!(a["event_id"], b["event_id"], "两次调用必须是两个事件（append-only，不是 upsert）");
    drop(s);
    assert_eq!(reuse_rows(dir.path()).len(), 2, "append-only：同卡同 outcome 两次 = 两行");
}

/// 契约 §1.1：身份由运行时绑定。复用工具同样不许从参数里读 `agent_id`。
#[test]
fn agent_id_parameter_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    seed_card(dir.path(), "qoder-test", "c-denied");
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, "qoder-test"));
    let text = call_error_text(
        &mut s,
        2,
        "recollect_reuse",
        json!({"handle": "card:c-denied", "outcome": "worked", "agent_id": "agent-other"}),
    );
    assert!(text.contains("agent_id_forbidden"), "agent_id 形参必须被拒：{text}");
    drop(s);
    assert!(reuse_rows(dir.path()).is_empty(), "被拒的调用在库里留下了行");
}
