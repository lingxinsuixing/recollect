//! G17：`hub import` 对 v3 输入的下行投影——端到端（CLI 面）测试。
//!
//! 夹具全合成（dsh-sess-01 / dev-dsh-01 / user-a / agent-b / -7311 系列正文，
//! 与 rc-contract 的 `ingest_v3_project` 测试同构），v3 形状以 rc-contract 的
//! v3 类型与身份函数为准：uid 全部现场重算，不是手抄的常量。

use rc_contract::identity_v3::{
    compute_content_hash, compute_event_uid, compute_revision_id, compute_session_uid,
    compute_thread_uid, compute_turn_uid_from_anchor, normalize_text_v3, text_sha256,
};
use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

const SOURCE_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const TS_BASE: i64 = 1_761_474_800_000;

/// 合法 v3 文件：human(1) → tool_call(2) → tool_result(3) → agent(4) → system(5)。
/// 投影面应产出 3 条核心轮（human/agent/system），工具对整行丢弃并计数。
fn v3_fixture_json() -> serde_json::Value {
    let session_uid = compute_session_uid("dsh-adapter", "dev-dsh-01", "dsh-sess-01", None);
    let thread_uid = compute_thread_uid(&session_uid, "main");
    let revision_id = compute_revision_id(SOURCE_SHA, 1, 3);

    let t_human = normalize_text_v3("把回访表补齐-7311");
    let t_agent = normalize_text_v3("已列缺字段-7312");
    let t_result = normalize_text_v3("找到 3 条-7314");
    let t_system = normalize_text_v3("会话续接-7313");
    let hashes = [
        compute_content_hash("human_msg", &t_human, None, None, None),
        compute_content_hash("tool_call", "", Some(("c1", "search")), None, None),
        compute_content_hash("tool_result", "", None, Some(("c1", "ok", &t_result)), None),
        compute_content_hash("agent_msg", &t_agent, None, None, None),
        compute_content_hash("system", &t_system, None, None, None),
    ];
    let event_uids: Vec<String> = (0..5)
        .map(|i| compute_event_uid(&thread_uid, &format!("n{}", i + 1), &hashes[i], 0))
        .collect();
    let turn_uid = compute_turn_uid_from_anchor(&thread_uid, &event_uids[0]);

    let source_ref = |native: &str| {
        serde_json::json!({
            "adapter": "dsh-adapter", "adapter_version": 1,
            "path": "dsh-fixture.jsonl", "native_id": native,
        })
    };
    let event = |i: usize, extra: serde_json::Value| {
        let kinds = ["human_msg", "tool_call", "tool_result", "agent_msg", "system"];
        let speakers = ["user-a", "agent-b", "tool-c", "agent-b", "system-d"];
        let trusts = ["user", "agent", "tool", "agent", "harness"];
        let acts = ["assert", "request", "assert", "assert", "assert"];
        let mut ev = serde_json::json!({
            "seq": i + 1,
            "kind": kinds[i],
            "speaker_id": speakers[i],
            "ts_ms": TS_BASE + (i as i64) * 1000,
            "thread_uid": thread_uid.clone(),
            "turn_uid": turn_uid.clone(),
            "event_uid": event_uids[i].clone(),
            "native_key": format!("n{}", i + 1),
            "content_hash": hashes[i].clone(),
            "revision_id": revision_id.clone(),
            "text_sha256": text_sha256(""),
            "norm_version": 1,
            "refs": [],
            "trust": trusts[i],
            "speech_act": acts[i],
            "source_ref": source_ref(&format!("n{}", i + 1)),
        });
        let obj = ev.as_object_mut().unwrap();
        for (k, v) in extra.as_object().unwrap() {
            obj.insert(k.clone(), v.clone());
        }
        ev
    };

    serde_json::json!({
        "format_version": 3,
        "ingest_version": 3,
        "sessions": [{
            "session_key": "dsh-sess-01",
            "session_uid": session_uid,
            "source_agent": "dsh-adapter",
            "device_id": "dev-dsh-01",
            "environment": {
                "machine_id": "m-dsh",
                "machine": "windows",
                "cwd": "D:/proj-dsh",
                "harness": {"name": "dsh-adapter", "version": "1"},
                "timezone": "UTC",
            },
            "roster": {
                "agents": [{"id": "agent-b", "role": "main"}],
                "tools": [{"name": "search", "source": "harness"}],
            },
            "ownership": {
                "owner_agent_id": "agent-b",
                "scope": {"kind": "project", "key": "dsh-proj"},
                "visibility": "private_agent",
                "retention": "default",
            },
            "threads": [{
                "thread_key": "main", "thread_uid": thread_uid.clone(),
                "kind": "main", "agent_id": "agent-b",
            }],
            "turns": [{
                "turn_key": "t1", "turn_uid": turn_uid.clone(), "thread_uid": thread_uid.clone(),
                "derived": true, "start_seq": 1, "end_seq": 5,
            }],
            "participants": [
                {"id": "user-a", "kind": "human"},
                {"id": "agent-b", "kind": "agent", "role": "main"},
                {"id": "tool-c", "kind": "tool"},
                {"id": "system-d", "kind": "system"},
            ],
            "source": {
                "adapter": "dsh-adapter", "adapter_version": 1,
                "path": "dsh-fixture.jsonl", "sha256": SOURCE_SHA,
                "revision_id": revision_id,
            },
            "events": [
                event(0, serde_json::json!({
                    "text": t_human, "text_sha256": text_sha256(&t_human),
                })),
                event(1, serde_json::json!({
                    "tool": {"call_id": "c1", "name": "search", "arguments": {"q": "回访表"}},
                })),
                event(2, serde_json::json!({
                    "tool_result": {"call_id": "c1", "status": "ok", "text": t_result},
                })),
                event(3, serde_json::json!({
                    "text": t_agent, "text_sha256": text_sha256(&t_agent),
                })),
                event(4, serde_json::json!({
                    "text": t_system, "text_sha256": text_sha256(&t_system),
                    "system_kind": "harness_notice",
                })),
            ],
        }]
    })
}

fn write_fixture(dir: &Path, name: &str, v: &serde_json::Value) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, serde_json::to_vec_pretty(v).unwrap()).unwrap();
    p
}

fn run(dir: &Path, f: &Path, json: bool) -> (Option<i32>, String) {
    let mut args = vec![
        "import".to_string(),
        f.to_str().unwrap().to_string(),
        "--data-dir".to_string(),
        dir.to_str().unwrap().to_string(),
    ];
    if json {
        args.push("--json".to_string());
    }
    let out = Command::new(bin()).args(&args).output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).to_string(),
    )
}

/// 工单第 5 条①：DSH 形状的 v3 夹具经 run_import 端到端导入成功，
/// 轮数与内容对（轮数 3；内容一致性由幂等重导证明——uid 稳定才会全部命中 duplicate）。
#[test]
fn dsh_shaped_v3_imports_end_to_end_with_core_turns() {
    let dir = tempfile::tempdir().unwrap();
    let f = write_fixture(dir.path(), "dsh_v3.json", &v3_fixture_json());
    let (code, text) = run(dir.path(), &f, true);
    assert_eq!(code, Some(0), "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["schema_version"], 1, "加字段是 C2 加法，报告形状版本不动");
    assert_eq!(v["turns_accepted"], 3, "human/agent/system 三行入投影");
    assert_eq!(v["turns_rejected"], 0);
    assert_eq!(v["batch_reject"], "");
    // 工单第 5 条②：富字段丢弃计数正确（JSON 面）。
    assert_eq!(v["dropped_fields"]["event.kind.tool_call"], 1);
    assert_eq!(v["dropped_fields"]["event.kind.tool_result"], 1);
    assert_eq!(v["dropped_fields"]["environment.cwd"], 1);
    assert_eq!(v["dropped_fields"]["participants"], 4);
    // 守卫 12 的金丝雀：投影丢弃的不是把值抄出来。
    assert!(!text.contains("回访表"), "输出泄露正文：{text}");
    assert!(!text.contains("proj-dsh"), "输出泄露丢弃字段值：{text}");

    // 幂等：第二轮同一批 uid 全部命中 duplicate ⇒ 三行真以稳定身份落了库。
    let (code2, text2) = run(dir.path(), &f, true);
    assert_eq!(code2, Some(0), "{text2}");
    let v2: serde_json::Value = serde_json::from_str(&text2).unwrap();
    assert_eq!(v2["turns_accepted"], 0);
    assert_eq!(v2["skipped_duplicate"], 3);
}

/// 工单第 3 条：丢弃计数在文本输出面同样出现（v1 输入的文本面逐字节不新增格）。
#[test]
fn drop_counts_appear_on_the_text_face() {
    let dir = tempfile::tempdir().unwrap();
    let f = write_fixture(dir.path(), "dsh_v3.json", &v3_fixture_json());
    let (code, text) = run(dir.path(), &f, false);
    assert_eq!(code, Some(0), "{text}");
    // BTreeMap 迭代序：`environment.*` 排在 `event.*` 之前；两个 kind 键相邻，
    // 一次断言把相对顺序也钉住。
    assert!(text.contains("dropped=environment.cwd:1,"), "{text}");
    assert!(
        text.contains("event.kind.tool_call:1,event.kind.tool_result:1"),
        "{text}"
    );
}

/// 工单第 5 条③的接线侧证据：v1 输入行为不变，且丢弃读数为空。
#[test]
fn v1_import_is_untouched_and_reports_no_drops() {
    let dir = tempfile::tempdir().unwrap();
    let ok = serde_json::json!({
        "format_version": 1,
        "sessions": [{"session_key": "ses-A", "device_id": "dev-1", "source_agent": "qoder",
            "turns": [{"role": "human", "body": "CANARY-BODY-8842", "created_at_ms": 1_730_000_000_000i64}]}]
    });
    let f = write_fixture(dir.path(), "ok_v1.json", &ok);
    let (code, text) = run(dir.path(), &f, true);
    assert_eq!(code, Some(0), "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["turns_accepted"], 1);
    assert_eq!(v["dropped_fields"], serde_json::json!({}));

    let (code2, text2) = run(dir.path(), &f, false);
    assert_eq!(code2, Some(0), "{text2}");
    assert!(!text2.contains("dropped="), "v1 的文本面不许多出新格：{text2}");
}

/// 工单第 5 条④：投影失败的 v3 走整批拒收通道（exit 2）且审计行在。
#[test]
fn v3_gate_failure_is_whole_batch_rejected_with_an_audit_row() {
    let dir = tempfile::tempdir().unwrap();
    let mut bad = v3_fixture_json();
    bad["sessions"][0]["session_uid"] = serde_json::json!("0".repeat(64));
    let f = write_fixture(dir.path(), "bad_v3.json", &bad);
    let (code, text) = run(dir.path(), &f, true);
    assert_eq!(code, Some(2), "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["batch_reject"], "v3_invalid");
    assert_eq!(v["turns_accepted"], 0);

    let root = rc_store::Root::open(dir.path()).unwrap();
    let mut st = root
        .ledger()
        .conn()
        .prepare("SELECT batch_reject_code FROM import_log ORDER BY id")
        .unwrap();
    let codes: Vec<String> = st
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(codes, vec!["v3_invalid".to_string()], "审计行必须留痕");
}

/// 版本探测的误判闸：v1 形状的身子谎报 `format_version:3` 不许被静默投影——
/// 它读不成 v3（缺必填格），落 json_malformed 整批拒收，exit 2 与今天的坏文件同面。
#[test]
fn a_v1_body_declaring_v3_is_rejected_not_projected() {
    let dir = tempfile::tempdir().unwrap();
    let liar = serde_json::json!({
        "format_version": 3,
        "sessions": [{"session_key": "s", "device_id": "d", "source_agent": "a",
            "turns": [{"role": "human", "body": "x", "created_at_ms": 1}]}]
    });
    let f = write_fixture(dir.path(), "liar.json", &liar);
    let (code, text) = run(dir.path(), &f, true);
    assert_eq!(code, Some(2), "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["turns_accepted"], 0);
}

fn import_log_batch_codes(dir: &Path) -> Vec<String> {
    let root = rc_store::Root::open(dir).unwrap();
    let mut st = root
        .ledger()
        .conn()
        .prepare("SELECT batch_reject_code FROM import_log ORDER BY id")
        .unwrap();
    st.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

/// 审计行与当场报告共用同一个 slug（对照 import_cli.rs 的同族测试）。
#[test]
fn v3_reject_audit_code_matches_the_reported_slug() {
    let dir = tempfile::tempdir().unwrap();
    let mut bad = v3_fixture_json();
    bad["sessions"][0]["session_uid"] = serde_json::json!("0".repeat(64));
    let f = write_fixture(dir.path(), "bad_v3.json", &bad);
    let (code, text) = run(dir.path(), &f, true);
    assert_eq!(code, Some(2), "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        import_log_batch_codes(dir.path()),
        vec![v["batch_reject"].as_str().unwrap().to_string()]
    );
}
