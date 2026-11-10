//! 转换器配套（设计 `2026-10-30-converter-requirements` §3/§4）：`role='system'` 的轮
//! （工具调用/工具结果）**只作上下文，永不作 TARGET**。
//!
//! 三条判据，一条一支用例：
//! 1. 一批里混着 system 轮 ⇒ 它不进 TARGET、不进锚/事实/事件的出处；
//! 2. 与目标轮相邻的 system 轮进 `CTX (not evidence)` 段（可看、不可引）；
//! 3. 读数 `system_turns_skipped` 在"批里没有 system 轮"与"有几条"两种情形下都数对。
//!
//! 正反例只用通用占位名（张三/李四）与合成的工具输出，不含任何语料专名。

use rc_contract::{normalize, parse};
use rc_distill::pass_event::{run_event, select_event_batch};
use rc_distill::pass_fact::{run_fact, select_fact_batch};
use rc_distill::pass_naming::{run_naming, select_naming_batch};
use rc_distill::pass_prompt::{event_user, fact_user, FactFamily};
use rc_ledger::insert_batch;
use rc_ledger::pass_state::{self, PassKey};
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{AnchorRole, Provenance, Scope, ScopeKind};
use rc_store::Root;
use serde_json::json;
use std::cell::RefCell;
use std::collections::VecDeque;

mod common;
use common::adopt_rows;

const NOW: i64 = 1_700_000_000_000;
const SESSION: &str = "sess-batch-A";
const DEVICE: &str = "dev-batch-A";
const AGENT: &str = "agent-batch-A";

/// 合成的工具输出（转换器会把 tool_result 落成这一形状的 `role='system'` 轮）。
const TOOL_BODY: &str = "[tool_result for call-1] 运行完成，退出码 0";

/// naming 的两轮（名字必须在正文里**逐字**出现，噪声闸与逐字接地都要看到它）。
const NAME_ONE: &str = "张三说明天把对账单发给你";
const NAME_TWO: &str = "老张说对账单后天到，张三负责跟进";

/// fact 的目标轮（`quote`/`time_text` 都必须逐字落在正文里）。
const PROMISE: &str = "我下周三之前把对账单发给你";

/// event 的目标轮：`time_text` 是 `quote` 的子串，`object_text` 在正文里。
const EVENT_BODY: &str = "张三昨晚在机房把配置文件改回去，服务恢复了";

fn fresh() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    (dir, root)
}

/// 与 `common::seed_bodies` 同一条导入链，但 role 由调用方给：system 轮只能这样落库。
fn seed(root: &mut Root, turns: &[(&str, &str)], from_ms: i64) -> Vec<String> {
    let doc = json!({
        "format_version": 1,
        "sessions": [{
            "session_key": SESSION,
            "device_id": DEVICE,
            "source_agent": AGENT,
            "turns": turns.iter().enumerate().map(|(i, (role, body))| json!({
                "role": role,
                "body": body,
                "created_at_ms": from_ms + i as i64,
            })).collect::<Vec<_>>()
        }]
    });
    let file = parse(doc.to_string().as_bytes()).expect("夹具必须过契约");
    let rep = normalize(file, from_ms + 100_000);
    let mut uids = Vec::new();
    for (i, a) in rep.accepted.iter().enumerate() {
        // 到达顺序与 created_at 顺序一致：邻轮/区间判据读的都是 created_at。
        insert_batch(
            root.transcript_mut(),
            &a.session,
            std::slice::from_ref(&a.turn),
            from_ms + i as i64,
        )
        .unwrap();
        uids.push(a.turn.turn_uid.clone());
    }
    assert_eq!(uids.len(), turns.len(), "夹具的条数必须真是 turns.len()");
    uids
}

/// 会话归属（与 naming/fact/event 三处夹具同一份）：候选池按这一行过滤。
fn own(root: &Root) {
    root.transcript()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id='p1', owner_agent_id='alice',
                    visibility='project', retention='keep',
                    scope_kind='project', scope_key='proj-x', ownership_src='operator'
             WHERE session_key=?1 AND device_id=?2",
            rusqlite::params![SESSION, DEVICE],
        )
        .unwrap();
}

/// 两个活锚（张三较新 ⇒ 候选池里在前）、挂到会话归属上。出处指向批内第一轮。
fn put_persons(root: &Root, evidence: &str) {
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    for (canonical, created_at_ms) in [("李四", NOW - 2_000), ("张三", NOW - 1_000)] {
        put_entity(
            root.memory(),
            &EntityContent {
                kind: "person".into(),
                title: canonical.into(),
                scope: scope.clone(),
                aliases: vec![],
            },
            &[Provenance { turn_uid: evidence.to_string(), anchor_role: AnchorRole::Human }],
            created_at_ms,
        )
        .unwrap();
    }
    adopt_rows(root, "anchor", "alice", Some("p1"), "project", "project", "proj-x");
}

/// 按序回放脚本（脚本耗尽 = `Timeout`，与回放座位同一条语义）。
struct Script {
    replies: RefCell<VecDeque<Result<String, LlmFailure>>>,
}

impl Script {
    fn new(replies: Vec<String>) -> Script {
        Script { replies: RefCell::new(replies.into_iter().map(Ok).collect()) }
    }
}

impl LlmBackend for Script {
    fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
        self.replies.borrow_mut().pop_front().unwrap_or(Err(LlmFailure::Timeout))
    }
    fn slug(&self) -> Option<&str> {
        Some("fake-system-ctx-model")
    }
}

fn naming_replies() -> Vec<String> {
    let a1 = r#"{"schema_version":4,"pass":"a1",
      "anchors":[{"ref":"a1","kind":"person","canonical":"张三","aliases":["老张"],
                  "reality_status":"real","evidence":["t1","t2"],"reason":"两轮点名"}],
      "mentions":[{"anchor_ref":"a1","surface":"老张","turn_uid":"t2","speaker_ref":null,
                   "discourse_role":"mentioned","resolution":"resolved","confidence":0.9}],
      "issues":[],"unresolved_surfaces":[]}"#
        .to_string();
    let review = r#"{"schema_version":0,"pass":"naming_review",
        "verdicts":[{"ref":"a1","verdict":"pass","code":null}]}"#
        .to_string();
    // 双跑两次 + naming 窄审一次。
    vec![a1.clone(), a1, review]
}

fn fact_replies(empty_turns: &[&str]) -> Vec<String> {
    let extraction = json!({
        "schema_version": 0, "pass": "fact", "family": "promise",
        "facts": [{
            "ref": "f1", "turn": "t1", "subject": "a1", "predicate": "promise",
            "object": "a2", "value": "把对账单发给你", "category": "promise",
            "mode": "stated", "time_text": "下周三之前", "quote": PROMISE,
            "evidence": ["t1"], "text": "张三承诺在下周三之前把对账单发给李四。"
        }],
        "empty_turns": empty_turns
    })
    .to_string();
    let review = json!({
        "schema_version": 0, "pass": "fact_review", "family": "promise",
        "verdicts": [{"ref": "f1", "verdict": "pass", "code": null, "evidence": "对账单"}]
    })
    .to_string();
    // 抽取一次 + 审核两档（proposer/challenger 同形）。
    vec![extraction, review.clone(), review]
}

fn event_replies(empty_turns: &[&str]) -> Vec<String> {
    let extraction = json!({
        "schema_version": 0, "pass": "event",
        "events": [{
            "ref": "e1", "turn": "t1", "category": "state_change", "actor": "a1",
            "trigger": "改回去", "action": "restore_config", "object": null,
            "object_text": "配置文件", "participants": ["a1"], "place": null,
            "result": "done", "mode": "stated", "time_text": "昨晚", "quote": EVENT_BODY,
            "evidence": ["t1"], "text": "张三把配置文件改回去，服务恢复。"
        }],
        "empty_turns": empty_turns
    })
    .to_string();
    let review = json!({
        "schema_version": 0, "pass": "event_review",
        "verdicts": [{"ref": "e1", "verdict": "pass", "code": null}]
    })
    .to_string();
    vec![extraction, review]
}

fn key<'a>(uid: &'a str, pass_id: &'a str, family: &'a str) -> PassKey<'a> {
    PassKey { turn_uid: uid, pass_id, family }
}

fn provenance_of(root: &Root, turn_uid: &str) -> i64 {
    root.memory()
        .conn()
        .query_row(
            "SELECT count(*) FROM provenance WHERE turn_uid=?1",
            [turn_uid],
            |r| r.get(0),
        )
        .unwrap()
}

fn fact_rows(root: &Root) -> Vec<(String, String)> {
    let mut st = root
        .memory()
        .conn()
        .prepare("SELECT evidence_turn_uid, predicate FROM fact ORDER BY evidence_turn_uid")
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
}

fn target_lines(user: &str) -> Vec<&str> {
    user.lines().filter(|line| line.starts_with("TARGET [")).collect()
}

/// 1. system 轮不进 TARGET、也不成为任何一条记忆行的出处（锚/事实/事件三族各跑一遍）。
#[test]
fn a_system_turn_is_never_a_target_or_an_evidence_turn() {
    // ---- naming：系统的工具输出不进 TARGET，也不进锚的出处 ----------------------
    let (_dir, mut root) = fresh();
    let uids = seed(
        &mut root,
        &[("human", NAME_ONE), ("system", TOOL_BODY), ("human", NAME_TWO)],
        1_000,
    );
    own(&root);
    let plan = select_naming_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("三轮都还没表态");
    assert!(
        plan.batch.rows.iter().all(|row| row.turn_uid != uids[1]),
        "system 轮不许进批：{:?}",
        plan.batch.rows.iter().map(|row| &row.turn_uid).collect::<Vec<_>>()
    );
    assert_eq!(plan.batch.rows.len(), 2, "批里只剩两条真人轮");
    let out = run_naming(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &Script::new(naming_replies()),
        NOW,
    )
    .unwrap();
    assert_eq!(out.system_turns_skipped, 1);
    assert_eq!(provenance_of(&root, &uids[1]), 0, "system 轮不许当锚的出处");
    assert!(
        pass_state::get(root.ledger(), key(&uids[1], "naming", "")).unwrap().is_none(),
        "system 轮不该有 naming 状态行（它不是目标）"
    );

    // ---- fact：system 轮不进 TARGET，也不进事实的出处 --------------------------
    let (_dir2, mut root) = fresh();
    let uids = seed(
        &mut root,
        &[("human", PROMISE), ("system", TOOL_BODY), ("agent", "好的，收到"), ("human", "谢谢")],
        1_000,
    );
    own(&root);
    put_persons(&root, &uids[0]);
    let plan = select_fact_batch(
        root.transcript(),
        root.ledger(),
        root.memory(),
        0,
        FactFamily::Promise,
        NOW,
    )
    .unwrap()
    .expect("这一族还没跑过");
    assert!(plan.batch.rows.iter().all(|row| row.turn_uid != uids[1]));
    let out = run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &Script::new(fact_replies(&["t2", "t3"])),
        NOW,
    )
    .unwrap();
    assert_eq!(out.system_turns_skipped, 1);
    assert_eq!(
        fact_rows(&root),
        vec![(uids[0].clone(), "promise".to_string())],
        "事实只落在目标轮上"
    );
    assert_eq!(provenance_of(&root, &uids[1]), 0, "system 轮不许当事实的出处");

    // ---- event：system 轮不进 TARGET，也不进事件的出处 -------------------------
    let (_dir3, mut root) = fresh();
    let uids = seed(
        &mut root,
        &[("human", EVENT_BODY), ("system", TOOL_BODY), ("human", "谢谢")],
        1_000,
    );
    own(&root);
    put_persons(&root, &uids[0]);
    let plan = select_event_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("还没跑过 event");
    assert!(plan.batch.rows.iter().all(|row| row.turn_uid != uids[1]));
    let out = run_event(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &Script::new(event_replies(&["t2"])),
        NOW,
    )
    .unwrap();
    assert_eq!(out.system_turns_skipped, 1);
    assert_eq!(
        fact_rows(&root),
        vec![(uids[0].clone(), "restore_config".to_string())],
        "事件只落在目标轮上"
    );
    assert_eq!(provenance_of(&root, &uids[1]), 0, "system 轮不许当事件的出处");
    assert!(
        pass_state::get(root.ledger(), key(&uids[1], "event", "")).unwrap().is_none(),
        "system 轮不该有 event 状态行（它不是目标）"
    );
}

/// 2. 与目标轮相邻的 system 轮进 `CTX (not evidence)` 段：模型看得到，但它不是 TARGET。
#[test]
fn an_adjacent_system_turn_rides_in_ctx_not_target() {
    let (_dir, mut root) = fresh();
    let uids = seed(
        &mut root,
        &[("human", PROMISE), ("system", TOOL_BODY), ("agent", "好的，收到"), ("human", "谢谢")],
        1_000,
    );
    own(&root);
    put_persons(&root, &uids[0]);

    let plan = select_fact_batch(
        root.transcript(),
        root.ledger(),
        root.memory(),
        0,
        FactFamily::Promise,
        NOW,
    )
    .unwrap()
    .expect("这一族还没跑过");
    assert!(
        plan.ctx_turns.has_uid(&uids[1]),
        "紧邻目标轮的工具输出要在邻轮集合里：{:?}",
        plan.ctx_turns.turns.iter().map(|t| &t.row.turn_uid).collect::<Vec<_>>()
    );
    let user = fact_user(&plan.batch, &plan.candidates, FactFamily::Promise, &plan.ctx_turns);
    let ctx_at = user.find("CTX (not evidence)").expect("有邻轮就必须有 CTX 段");
    assert!(user[ctx_at..].contains(TOOL_BODY), "CTX 段里要看得到工具输出");
    assert!(
        user[ctx_at..].lines().any(|line| line.contains("system") && line.contains(TOOL_BODY)),
        "CTX 行的 role 如实写 system：{user}"
    );
    assert!(
        target_lines(&user).iter().all(|line| !line.contains(TOOL_BODY)),
        "TARGET 段不许出现工具输出：{user}"
    );

    // event 共用同一份邻轮机制（`load_extract_ctx`），同一批换 pass 再看一次。
    let (_dir2, mut root) = fresh();
    let uids = seed(
        &mut root,
        &[("human", EVENT_BODY), ("system", TOOL_BODY), ("human", "谢谢")],
        1_000,
    );
    own(&root);
    put_persons(&root, &uids[0]);
    let plan = select_event_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("还没跑过 event");
    assert!(plan.ctx_turns.has_uid(&uids[1]));
    let user = event_user(&plan.batch, &plan.candidates, &plan.ctx_turns);
    assert!(user.contains("CTX (not evidence)"));
    assert!(user.contains(TOOL_BODY));
    assert!(
        target_lines(&user).iter().all(|line| !line.contains(TOOL_BODY)),
        "TARGET 段不许出现工具输出：{user}"
    );
}

/// 3. 读数 `system_turns_skipped`：没有 system 轮读 0，批区间内有几条读几条。
#[test]
fn system_turns_skipped_counts_zero_and_nonzero() {
    // ---- 0：三条 pass 各自的干净会话都没有 system 轮 ---------------------------
    let (_dir, mut root) = fresh();
    seed(&mut root, &[("human", PROMISE), ("human", "好的，收到")], 1_000);
    own(&root);
    let naming = select_naming_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("没跑过");
    assert_eq!(naming.system_turns_skipped, 0);
    let fact = select_fact_batch(
        root.transcript(),
        root.ledger(),
        root.memory(),
        0,
        FactFamily::Promise,
        NOW,
    )
    .unwrap()
    .expect("没跑过");
    assert_eq!(fact.system_turns_skipped, 0);
    let event = select_event_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("没跑过");
    assert_eq!(event.system_turns_skipped, 0);

    // ---- >0：两条 system 轮夹在同一批的首末目标轮之间 ⇒ 2 ----------------------
    let (_dir2, mut root) = fresh();
    let uids = seed(
        &mut root,
        &[
            ("human", PROMISE),
            ("system", TOOL_BODY),
            ("system", "[tool_call name=run id=call-2] cargo test"),
            ("human", "好的，收到"),
            // 批末之后的那一条属于下一批的上下文，不进这一格的计数。
            ("system", "[tool_result for call-2] 0 failed"),
        ],
        1_000,
    );
    own(&root);
    put_persons(&root, &uids[0]);
    let plan = select_fact_batch(
        root.transcript(),
        root.ledger(),
        root.memory(),
        0,
        FactFamily::Promise,
        NOW,
    )
    .unwrap()
    .expect("没跑过");
    assert_eq!(plan.batch.rows.len(), 2, "批里只有两条真人轮");
    assert_eq!(
        plan.system_turns_skipped, 2,
        "只数夹在首个与末个目标轮之间的那两条，批末之后的第三条不算"
    );
    let out = run_fact(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &Script::new(fact_replies(&["t2"])),
        NOW,
    )
    .unwrap();
    assert_eq!(out.system_turns_skipped, 2, "读数要把选批那一格原样带出来");

    // 同一份会话换 naming：同一区间、同一个数。
    let (_dir3, mut root) = fresh();
    seed(
        &mut root,
        &[
            ("human", NAME_ONE),
            ("system", TOOL_BODY),
            ("system", "[tool_call name=run id=call-2] cargo test"),
            ("human", NAME_TWO),
            ("system", "[tool_result for call-2] 0 failed"),
        ],
        1_000,
    );
    own(&root);
    let plan = select_naming_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("没跑过");
    assert_eq!(plan.batch.rows.len(), 2);
    assert_eq!(plan.system_turns_skipped, 2);
}
