//! G19 体验刀的**线上**判据（`mcp` 侧）：whoami 两态、ingest_status 的三段计数与
//! no_rows、status counts 面的 wire 形状。工具层的分型/键名判据在
//! `rc-agent/tests/g19_selfserve.rs`；本文件证的是"缝的实现方真的在查那两个库"——
//! 与 `p2_extract_state.rs` 同一条分工。夹具全在 `common`（P-65）。
//!
//! 白盒播种走 `rc_ledger`（mcp 的正式依赖）给轮盖终态 pass 行，不引 `rc-distill`（§2.3）。

mod common;
use common::{call_json, call_normal_state_text, init, Session, tool_text};
use rc_ledger::pass_state::{mark_terminal, PassKey, PassState};
use rc_store::Root;
use serde_json::{json, Value};

/// 终态盖白盒：与 `commit` 内 `mark_terminal` 同一个写入口，指纹/run_id 走空
/// （终态行没有重试面，G19 只关心 state 那一格）。
fn stamp_terminal(data: &std::path::Path, uid: &str) {
    let root = Root::open(data).unwrap();
    mark_terminal(
        root.ledger(),
        PassKey { turn_uid: uid, pass_id: "naming", family: "" },
        PassState::Done,
        None,
        1_700_000_000_000,
    )
    .unwrap();
}

#[test]
fn tools_list_carries_the_two_selfcheck_tools() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, "qoder-test"));
    let r = s.ask(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
    let arr = r["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = arr.iter().map(|t| t["name"].as_str().unwrap()).collect();
    // 清单的唯一来源是 `capabilities().tools`（core_tools.rs 的等式判据管全表），
    // 这里只钉两件新工具的注册四件套：名字、title、description、inputSchema。
    for name in ["recollect_whoami", "recollect_ingest_status"] {
        assert!(names.contains(&name), "{name} 不在 tools/list：{names:?}");
        let def = arr.iter().find(|t| t["name"] == name).unwrap();
        assert!(def["title"].is_string(), "{name} 缺 title：{def}");
        assert!(def["description"].is_string(), "{name} 缺 description：{def}");
        assert!(def["inputSchema"].is_object(), "{name} 缺 inputSchema：{def}");
    }
    // 描述对齐（刀 2）：status 的描述必须说得出 counts 面到底带什么、不带什么——
    // DSH 实测的缺口就是"描述与形状对不上"。
    let status_def = arr.iter().find(|t| t["name"] == "recollect_status").unwrap();
    let d = status_def["description"].as_str().unwrap();
    assert!(d.contains("view=counts"), "描述没说 counts 面：{d}");
    assert!(d.contains("no capability declaration"), "描述没说 counts 面不带声明：{d}");
    assert!(d.contains("no entity directory"), "描述没说 counts 面不带目录：{d}");
    assert!(!d.starts_with("Counts only"), "旧描述还说 counts-only（与缺省大对象相反）：{d}");
}

#[test]
fn whoami_reports_the_four_identity_cells_when_registered() {
    let dir = tempfile::tempdir().unwrap();
    // 预注册 + 预写主用户：与 core_session 同一个入口（生产面 register 的等价物）。
    rc_core::CoreStore::open(dir.path()).unwrap().register_agent("agent-a", 0).unwrap();
    let mut s = Session::start_with(dir.path(),
        &["--device-id", "dev-synthetic", "--agent-id", "agent-a", "--principal-id", "p-owner"]);
    let _ = s.ask(&init(1, "qoder-test"));
    let subj = call_json(&mut s, 2, "core_subject_update", json!({"subject": {
        "subject_id": "user-a", "is_primary": true, "profile_text": "合成的主用户"}}));
    assert_eq!(subj["version"], json!(1), "{subj}");

    let me = call_json(&mut s, 3, "recollect_whoami", json!({}));
    assert_eq!(me["agent_id"], json!("agent-a"), "{me}");
    assert_eq!(me["device_id"], json!("dev-synthetic"), "{me}");
    assert_eq!(me["principal_id"], json!("p-owner"), "{me}");
    assert_eq!(me["registered"], json!(true), "{me}");
    assert_eq!(me["primary_user"]["user_id"], json!("user-a"), "{me}");
    assert_eq!(me["primary_user"]["profile_text"], json!("合成的主用户"), "{me}");

    // 正证（发现性闭环）：whoami 给出的 user_id 拿去 core_brief，立刻可用。
    let brief = s.ask(&json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
        "params":{"name":"core_brief",
                  "arguments":{"user_id": me["primary_user"]["user_id"]}}}));
    assert_eq!(brief["result"]["isError"], json!(false), "{brief}");
    assert!(tool_text(&brief).contains("常驻简报"), "{}", tool_text(&brief));
}

#[test]
fn whoami_on_an_unregistered_agent_says_so_with_a_pasteable_hint() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::start_with(dir.path(),
        &["--device-id", "dev-synthetic", "--agent-id", "agent-ghost"]);
    let _ = s.ask(&init(1, "qoder-test"));

    // 未注册是**读数**：isError=false，回执明说未注册并带可粘贴命令。
    let r = s.ask(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"recollect_whoami","arguments":{}}}));
    assert_eq!(r["result"]["isError"], json!(false), "未注册不是故障：{r}");
    let me: Value = serde_json::from_str(tool_text(&r).as_str()).unwrap();
    assert_eq!(me["registered"], json!(false), "{me}");
    assert_eq!(me["primary_user"], Value::Null, "{me}");
    let hint = me["register_hint"].as_str().unwrap_or_default();
    assert!(hint.contains("recollect-core") && hint.contains("register"),
            "hint 没指向注册入口：{me}");
    // 参数顺序是被执法的那一格（Task 9 终审 Important #1）：照抄进 shell 必须能活过 clap。
    let (data_dir_at, register_at) = (hint.find("--data-dir"), hint.find("register"));
    assert!(
        matches!((data_dir_at, register_at), (Some(d), Some(g)) if d < g),
        "`--data-dir` 必须在 `register` 之前（顺序反了照抄即失败）：{hint}"
    );
    // 零用户字节回显：命令模板是常量串，`<agent_id>`/`<dir>` 是占位符。
    assert!(!hint.contains("agent-ghost"), "hint 回显了 agent id：{me}");
    assert!(!hint.contains(dir.path().to_string_lossy().as_ref()),
            "hint 回显了数据目录：{me}");
}

#[test]
fn ingest_status_counts_the_two_phases_of_one_session() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::start_with(dir.path(),
        &["--device-id", "dev-synthetic", "--agent-id", "agent-a"]);
    let _ = s.ask(&init(1, "qoder-test"));
    let mut uids = Vec::new();
    for i in 0..3 {
        let uid = call_json(&mut s, 10 + i, "recollect_remember",
                            json!({"body": format!("G19 两阶段合成词-{i} 的一条合成记录"),
                                   "session_key": "sess-g19"}))
            ["turn_uid"].as_str().unwrap().to_string();
        uids.push(uid);
    }
    // 一条到终态（已蒸）：import 之后蒸馏是异步的，这一格就是"到没到可查"的判据。
    stamp_terminal(dir.path(), &uids[0]);

    let got = call_json(&mut s, 20, "recollect_ingest_status",
                        json!({"session_key": "sess-g19"}));
    assert_eq!(got["turns_total"], json!(3), "{got}");
    assert_eq!(got["turns_distilled"], json!(1), "{got}");
    assert_eq!(got["turns_pending"], json!(2), "{got}");
    assert!(got["last_arrival_ms"].as_i64().is_some_and(|ms| ms > 0),
            "到达时刻是转录层的事实：{got}");
    // 正文一个字都不许出现在读数里（守卫 12 对自查面照旧）。
    assert!(!got.to_string().contains("G19 两阶段合成词"), "{got}");

    // 查无此会话 = no_rows，**正常读数**（isError=false）——导入侧轮询一个还没落的
    // 会话键不该被读成故障。
    let ghost = call_normal_state_text(&mut s, 21, "recollect_ingest_status",
                                       json!({"session_key": "sess-ghost"}));
    assert!(ghost.contains("empty:no_rows"), "{ghost}");

    // 空会话键是输入校验，不是查无。
    let blank = common::call_error_text(&mut s, 22, "recollect_ingest_status",
                                        json!({"session_key": "   "}));
    assert!(blank.contains("validation:missing_argument"), "{blank}");
}

#[test]
fn status_counts_view_on_the_wire_drops_the_declaration_and_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::start_with(dir.path(),
        &["--device-id", "dev-synthetic", "--agent-id", "agent-a"]);
    let _ = s.ask(&init(1, "qoder-test"));
    let _ = common::seed(dir.path(), 1_700_000_000_000);

    // 缺省面（回归基准）：声明与目录都在——线上的缺省行为一字不变。
    let full = call_json(&mut s, 2, "recollect_status", json!({}));
    assert!(full.get("capabilities").is_some(), "缺省面丢了声明：{full}");
    assert!(full.get("entities").is_some(), "缺省面丢了目录：{full}");

    // counts 面：声明与目录两块拿掉，计数格照旧。
    let counts = call_json(&mut s, 3, "recollect_status", json!({"view": "counts"}));
    assert!(counts.get("capabilities").is_none(), "counts 面带了声明：{counts}");
    assert!(counts.get("entities").is_none(), "counts 面带了目录：{counts}");
    assert!(counts.get("rows").is_some() && counts.get("attention_open").is_some(),
            "counts 面丢了计数格：{counts}");

    // 闭集外与互斥组合都就地拒。
    let bad = common::call_error_text(&mut s, 4, "recollect_status", json!({"view": "bogus"}));
    assert!(bad.contains("validation:bad_view"), "{bad}");
    let clash = common::call_error_text(&mut s, 5, "recollect_status",
                                        json!({"view": "counts", "entity": "老王"}));
    assert!(clash.contains("validation:counts_and_entity"), "{clash}");
}
