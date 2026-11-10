//! 消费面三视图：`overview / agenda / timeline` 的确定性口径。
//!
//! 夹具用真库 + 真 SQL 写入（不经 LLM、不经 writer 校验），断的是**视图自己的规则**：
//! 活行口径（退场/顶掉/否决三类都要排掉）、scope、时间窗、due 的逾期判定、读闸。

use rc_agent::views;
use rc_memory::gate::Viewer;
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_state::{put_state_rows, StateWriteItem};
use rc_memory::writers_x::BatchCtx;
use rc_memory::{AnchorRole, Scope, ScopeKind};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;
const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;

fn anchor(root: &Root, id: &str, canonical: &str, kind: &str, scope: &str) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO anchor(id,kind,canonical,reality_status,scope_kind,scope_key,status,
                                created_at_ms,updated_at_ms,norm_version)
             VALUES(?1,?2,?3,'real','project',?4,'active',?5,?5,1)",
            (id, kind, canonical, scope, NOW),
        )
        .unwrap();
}

struct Fact {
    id: String,
    agent: String,
    category: String,
    text: String,
    due: Option<i64>,
    status: Option<String>,
    occurred: Option<i64>,
    created: i64,
    superseded_by: Option<String>,
    scope: String,
}

fn fact(root: &Root, subject: &str, f: Fact) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO fact(id,subject_anchor_id,predicate,category,participants_json,mode,
                              confidence,text,evidence_turn_uid,scope_kind,scope_key,
                              source_agent,device_id,created_at_ms,norm_version,due_at_ms,status,
                              occurred_at_ms,superseded_by)
             VALUES(?1,?2,'p',?3,'[]','stated',0.8,?4,?5,'project',?6,?7,?8,?9,1,?10,?11,?12,?13)",
            (
                &f.id, subject, &f.category, &f.text, "e".repeat(64), &f.scope, &f.agent,
                format!("dev-{}", f.agent), f.created, f.due, f.status.as_deref(), f.occurred,
                f.superseded_by.as_deref(),
            ),
        )
        .unwrap();
}

fn owner(root: &Root, id: &str, agent: &str, scope: &str) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO resource_owner(mem_table,mem_id,owner_principal_id,owner_agent_id,
                                        created_by_agent,visibility,retention,scope_kind,scope_key,
                                        source_agent,device_id,created_at_ms)
             VALUES('fact',?1,NULL,?2,?2,'private_agent','default','project',?3,?2,?4,?5)",
            (id, agent, scope, format!("dev-{agent}"), NOW),
        )
        .unwrap();
}

fn retire(root: &Root, id: &str) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO retirement(mem_table,mem_id,reason,created_at_ms,resolved_at_ms)
             VALUES('fact',?1,'test',?2,NULL)",
            (id, NOW),
        )
        .unwrap();
}

fn reject(root: &Root, id: &str) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO attestation(mem_table,mem_id,stance,actor,anchor_turn_uid,reason,
                                     created_at_ms,resolved_at_ms)
             VALUES('fact',?1,'rejected','center',NULL,'test',?2,NULL)",
            (id, NOW),
        )
        .unwrap();
}

/// 一条**走生产写入口**（`put_state_rows`）落下的当前状态行：归属行也由它写，
/// 读闸才有东西可拒。裸 SQL 插 `state_assertion` 只会有行、没有归属，那样"被藏"
/// 是缺归属而不是身份，用例就测错了东西。
fn state(root: &Root, subject: &str, value: &str, agent: &str) {
    state_with_mode(root, subject, value, "expressed", agent);
}

/// 同一条生产写入口，`mode` 由调用方给：C1 之后 `inferred` 也是合法值，消费面必须分开呈现。
fn state_with_mode(root: &Root, subject: &str, value: &str, mode: &str, agent: &str) {
    put_state_rows(
        root.memory(),
        &[StateWriteItem {
            subject_anchor_id: subject.to_string(),
            dimension: "emotion".into(),
            value_after: value.into(),
            mode: mode.into(),
            text: format!("情绪为「{value}」"),
            evidence: vec![("e".repeat(64), AnchorRole::Human)],
            scope: Scope { kind: ScopeKind::Project, key: "proj".into() },
            source_ms: NOW - HOUR,
        }],
        &BatchCtx { source_agent: agent.into(), device_id: format!("dev-{agent}") },
        &OwnerCtx {
            owner_principal_id: None,
            owner_agent_id: agent.into(),
            // 私有：换一个身份读时归属行会把它挡住（owner 本人仍然可读）。
            visibility: Visibility::PrivateAgent,
            retention: "default".into(),
        },
        NOW,
    )
    .unwrap();
}

fn pending_review(root: &Root) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO review_item(source,pass_id,turn_uids_json,owner_json,payload_json,
                                     payload_sha256,state,created_at_ms,updated_at_ms)
             VALUES('distill','event','[]','{}','{}','x','pending_review',?1,?1)",
            [NOW],
        )
        .unwrap();
}

fn seeded() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    anchor(&root, "a1", "hub", "object", "proj");
    anchor(&root, "a2", "alice", "person", "proj");
    let mk = |id: &str, category: &str, text: &str, due: Option<i64>, status: Option<&str>,
              occurred: Option<i64>, created: i64, sup: Option<&str>, agent: &str| Fact {
        id: id.into(),
        agent: agent.into(),
        category: category.into(),
        text: text.into(),
        due,
        status: status.map(str::to_string),
        occurred,
        created,
        superseded_by: sup.map(str::to_string),
        scope: "proj".into(),
    };
    // 三条未完成承诺：到期、逾期、无日期。
    fact(&root, "a1", mk("f1", "promise", "明天交报告", Some(NOW + DAY), Some("pending"), None, NOW - DAY, None, "agent-A"));
    fact(&root, "a1", mk("f2", "plan", "昨天该部署", Some(NOW - DAY), Some("pending"), None, NOW - DAY, None, "agent-A"));
    fact(&root, "a2", mk("f3", "promise", "有空再聊", None, None, None, NOW - DAY, None, "agent-A"));
    // 已完成、被顶掉、被退场、被否决：都不许出现在视图里。
    fact(&root, "a1", mk("f7", "plan", "已经做完的事", Some(NOW + DAY), Some("done"), None, NOW - DAY, None, "agent-A"));
    fact(&root, "a1", mk("f8", "promise", "被顶掉的旧承诺", Some(NOW + DAY), Some("pending"), None, NOW - DAY, Some("f1"), "agent-A"));
    fact(&root, "a1", mk("f9", "promise", "被退场的承诺", Some(NOW + DAY), Some("pending"), None, NOW - DAY, None, "agent-A"));
    fact(&root, "a1", mk("f10", "plan", "被否决的计划", Some(NOW + DAY), Some("pending"), None, NOW - DAY, None, "agent-A"));
    // 最近决定 + 两条事件（一条在 7 天内、一条在窗外）。
    fact(&root, "a2", mk("f4", "decision", "采用方案 B", None, None, None, NOW - HOUR, None, "agent-A"));
    fact(&root, "a1", mk("f5", "event", "部署成功", None, None, Some(NOW - 2 * HOUR), NOW - 2 * HOUR, None, "agent-A"));
    fact(&root, "a1", mk("f6", "event", "十天前的事", None, None, Some(NOW - 10 * DAY), NOW - 10 * DAY, None, "agent-A"));
    // 别的 agent 的私有行：A 看不见。
    fact(&root, "a1", mk("f11", "promise", "B 的私有承诺", Some(NOW + DAY), Some("pending"), None, NOW - DAY, None, "agent-B"));

    for id in ["f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10"] {
        owner(&root, id, "agent-A", "proj");
    }
    owner(&root, "f11", "agent-B", "proj");
    // 当前状态：生产路径写出的行 + 归属行，agent-A 可见、agent-B 看不见。
    state(&root, "a2", "焦虑得很", "agent-A");
    retire(&root, "f9");
    reject(&root, "f10");
    pending_review(&root);
    (dir, root)
}

fn json(v: &str) -> serde_json::Value {
    serde_json::from_str(v).expect("视图输出必须是 JSON")
}

fn run(
    m: &rc_store::MemoryHandle, view: &str, scope: Option<&str>, from: Option<i64>,
    to: Option<i64>, limit: usize, agent: &str,
) -> String {
    views::render(
        m,
        &views::ViewRequest { view, scope, from_ms: from, to_ms: to, limit },
        &Viewer::agent(agent),
        NOW,
    )
    .unwrap()
}

#[test]
fn overview_counts_only_live_rows_and_respects_the_gate() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let v = json(&run(m, "overview", Some("proj"), None, None, 20, "agent-A"));
    let c = &v["counts"];
    assert_eq!(c["open_commitments"], 3, "{v}");
    assert_eq!(c["dated_commitments"], 2, "{v}");
    assert_eq!(c["overdue_commitments"], 1, "{v}");
    assert_eq!(c["recent_decisions"], 1, "{v}");
    assert_eq!(c["recent_events"], 1, "{v}");
    assert_eq!(c["active_entities"], 2, "{v}");
    assert_eq!(c["unresolved_review"], 1, "{v}");
    let text = v.to_string();
    for banned in ["B 的私有承诺", "已经做完的事", "被顶掉的旧承诺", "被退场的承诺", "被否决的计划"] {
        assert!(!text.contains(banned), "视图里出现了不该出现的行 {banned}: {text}");
    }
    assert!(v["brief"].as_str().unwrap().contains("未完成承诺 3 条"), "{v}");
}

#[test]
fn agenda_orders_by_due_and_lists_undated_separately() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let v = json(&run(m, "agenda", Some("proj"), None, None, 20, "agent-A"));
    assert_eq!(v["counts"]["dated"], 2, "{v}");
    assert_eq!(v["counts"]["overdue"], 1, "{v}");
    assert_eq!(v["counts"]["undated"], 1, "{v}");
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{v}");
    assert_eq!(items[0]["fact_id"], "f2", "逾期项必须排在前面: {v}");
    assert_eq!(items[0]["overdue"], true, "{v}");
    assert_eq!(items[1]["fact_id"], "f1", "{v}");
    assert_eq!(v["undated"][0]["fact_id"], "f3", "{v}");
}

#[test]
fn timeline_honours_the_window_and_limit() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let win = json(&run(m, "timeline", Some("proj"),
                        Some(NOW - 3 * HOUR), Some(NOW), 20, "agent-A"));
    assert_eq!(win["counts"]["items"], 2, "窗口内应只有决定 + 2 小时前的事件: {win}");
    let one = json(&run(m, "timeline", Some("proj"),
                        Some(NOW - 3 * HOUR), Some(NOW), 1, "agent-A"));
    assert_eq!(one["counts"]["items"], 1, "limit=1 必须生效: {one}");
    assert!(!win.to_string().contains("十天前的事"), "{win}");
}

#[test]
fn empty_db_returns_zeros_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let v = json(&run(m, "overview", None, None, None, 20, "agent-A"));
    assert_eq!(v["counts"]["open_commitments"], 0, "{v}");
    assert_eq!(v["counts"]["active_entities"], 0, "{v}");
    assert!(!v["brief"].as_str().unwrap().is_empty(), "{v}");
    // 真没有状态行 ≠ 身份沉默：布尔恒在场且为假，brief 里一个字都不多。
    assert_eq!(v["states_hidden_by_identity"], false, "{v}");
    assert_eq!(v["facts_hidden_by_identity"], false, "{v}");
    assert!(!v["brief"].as_str().unwrap().contains("看不到"), "{v}");
    // 空库上 agenda / timeline 的新键也必须恒在场且为假。
    let a = json(&run(m, "agenda", None, None, None, 20, "agent-A"));
    assert_eq!(a["rows_hidden_by_identity"], false, "{a}");
    let t = json(&run(m, "timeline", None, None, None, 20, "agent-A"));
    assert_eq!(t["rows_hidden_by_identity"], false, "{t}");
}

/// 错身份读同一座库：状态行在库里、过 scope，但读闸全拒 ⇒ 零可见必须自证。
/// 三条一起断（计数 / 布尔 / brief）——只断布尔会被"恒真"骗过，只断 brief 会被措辞骗过。
#[test]
fn a_wrong_identity_sees_zero_states_and_is_told_why() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let v = json(&run(m, "overview", Some("proj"), None, None, 20, "agent-B"));
    assert_eq!(v["counts"]["current_states"], 0, "{v}");
    assert_eq!(v["states_hidden_by_identity"], true, "{v}");
    assert!(v["brief"].as_str().unwrap().contains("看不到"), "{v}");
}

/// 对身份读同一座库：同一行必须照常读出，布尔为假，brief 不得出现"看不到"。
/// 与上一条成对，防"恒真"（见工单第五节变异 A）。
#[test]
fn the_owning_identity_still_sees_the_state_and_gets_no_hint() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let v = json(&run(m, "overview", Some("proj"), None, None, 20, "agent-A"));
    assert_eq!(v["counts"]["current_states"], 1, "{v}");
    assert_eq!(v["states_hidden_by_identity"], false, "{v}");
    let brief = v["brief"].as_str().unwrap();
    assert!(!brief.contains("看不到"), "{v}");
    assert!(brief.contains("焦虑得很"), "可见的状态必须照常进 brief: {v}");
}

/// 部分可见：库里两条状态、agent-A 只看得到自己那条 ⇒ 不是零可见，布尔必须为假。
/// 防"只要库里有被藏的行就报"的近似实现（验收 F1：既有「1 可见 + 1 被藏」没断言布尔）。
#[test]
fn partial_visibility_is_not_identity_silence() {
    let (_dir, root) = seeded();
    // 再加一条 agent-B 的私有状态：同一座库里有第二条状态，但 agent-A 看不到它。
    state(&root, "a1", "谈得很顺", "agent-B");
    let m = root.memory();
    let v = json(&run(m, "overview", Some("proj"), None, None, 20, "agent-A"));
    assert_eq!(v["counts"]["current_states"], 1, "{v}");
    assert_eq!(v["states_hidden_by_identity"], false, "部分可见不是身份沉默：{v}");
    assert!(!v["brief"].as_str().unwrap().contains("看不到"), "{v}");
}

/// C1 的消费面对照：`expressed` 行的自然语言渲染与 C1 之前**逐字相同**（一个字节都不许变），
/// `inferred` 行多一个"（推断）"后缀；JSON 面两条都带 `mode`（降级不是暗账）。
///
/// 这条用例钉的是"降级之后读者能不能看出来"：没有它，`inferred` 与 `expressed` 在
/// 消费面上不可分，闸收紧的意义归零。
#[test]
fn a_state_line_is_byte_identical_when_expressed_and_marked_when_inferred() {
    let (_dir, root) = seeded();
    // 库里已有一条 expressed 的 a2/「焦虑得很」（`seeded()` 走生产写入口写的）。
    // 推断行换一个主体：同主体同维度的第二条会被 D10 的替代规则处理掉，测不到两行并存。
    anchor(&root, "a3", "bob", "person", "proj");
    state_with_mode(&root, "a3", "有点累", "inferred", "agent-A");
    let m = root.memory();
    let v = json(&run(m, "overview", Some("proj"), None, None, 20, "agent-A"));
    let brief = v["brief"].as_str().unwrap();
    // 逐字节基线：这一串是 C1 之前的渲染结果，改写它等于改了既有消费面。
    assert!(
        brief.contains("alice 的情绪是「焦虑得很」"),
        "expressed 行的渲染被改动了: {brief}"
    );
    assert!(brief.contains("bob 的情绪是「有点累」（推断）"), "{brief}");
    let states = v["current_states"].as_array().unwrap();
    assert_eq!(states.len(), 2, "{v}");
    let mode_of = |value: &str| {
        states
            .iter()
            .find(|s| s["value"] == value)
            .and_then(|s| s["mode"].as_str())
            .map(str::to_string)
            .unwrap_or_else(|| panic!("当前状态里没有 {value}：{v}"))
    };
    assert_eq!(mode_of("焦虑得很"), "expressed".to_string());
    assert_eq!(mode_of("有点累"), "inferred".to_string());
}

/// scope 把状态行筛掉：这是**内容过滤**，不是身份沉默 ⇒ 零可见但布尔必须为假。
/// 防"库里有活行就报身份沉默"的近似实现（工单第五节变异 A）。
#[test]
fn a_scope_filter_that_empties_states_is_not_identity_silence() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let v = json(&run(m, "overview", Some("别的"), None, None, 20, "agent-A"));
    assert_eq!(v["counts"]["current_states"], 0, "{v}");
    assert_eq!(v["states_hidden_by_identity"], false, "{v}");
    assert!(!v["brief"].as_str().unwrap().contains("看不到"), "{v}");
}

/// 错身份读 agenda：库里有行、过 scope，但读闸全拒 ⇒ 零可见必须自证。
/// 用 `agent-C` 而不是 `agent-B`：`seeded()` 里 `f11` 的归属是 agent-B，B 会看见自己那一条，
/// 那是"部分可见"（见 `partial_row_visibility_is_not_identity_silence`），不是零可见。
#[test]
fn agenda_a_wrong_identity_is_told_rows_are_hidden() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let v = json(&run(m, "agenda", Some("proj"), None, None, 20, "agent-C"));
    assert_eq!(v["counts"]["dated"], 0, "{v}");
    assert_eq!(v["counts"]["undated"], 0, "{v}");
    assert_eq!(v["rows_hidden_by_identity"], true, "库里有行、读闸后为空必须自证: {v}");
    assert!(v["brief"].as_str().unwrap().contains("看不到"), "{v}");
}

/// 对身份：同一座库照常读出，布尔为假，brief 不得出现"看不到"。
#[test]
fn agenda_the_owning_identity_gets_no_hint() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let v = json(&run(m, "agenda", Some("proj"), None, None, 20, "agent-A"));
    assert_eq!(v["counts"]["dated"], 2, "{v}");
    assert_eq!(v["counts"]["undated"], 1, "{v}");
    assert_eq!(v["rows_hidden_by_identity"], false, "{v}");
    assert!(!v["brief"].as_str().unwrap().contains("看不到"), "{v}");
}

/// 错身份读 timeline：时间窗里有行（决定 + 事件），读闸后为空 ⇒ 必须自证。
#[test]
fn timeline_a_wrong_identity_is_told_rows_are_hidden() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let v = json(&run(m, "timeline", Some("proj"),
                        Some(NOW - 3 * HOUR), Some(NOW), 20, "agent-C"));
    assert_eq!(v["counts"]["items"], 0, "{v}");
    assert_eq!(v["rows_hidden_by_identity"], true, "{v}");
    assert!(v["brief"].as_str().unwrap().contains("看不到"), "{v}");
}

/// 对身份读同一时间窗：两行照常读出，布尔为假，brief 不得出现"看不到"。
#[test]
fn timeline_the_owning_identity_gets_no_hint() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let v = json(&run(m, "timeline", Some("proj"),
                        Some(NOW - 3 * HOUR), Some(NOW), 20, "agent-A"));
    assert_eq!(v["counts"]["items"], 2, "{v}");
    assert_eq!(v["rows_hidden_by_identity"], false, "{v}");
    assert!(!v["brief"].as_str().unwrap().contains("看不到"), "{v}");
}

/// 空时间窗是"真没有"：SQL 后一行不剩，布尔必须为假、brief 保持原句——
/// 两身份都读，免得把"错身份"误当成原因。
#[test]
fn an_empty_time_window_stays_a_plain_zero() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let from = NOW + DAY;
    let to = NOW + 2 * DAY;
    let want = format!("范围 proj：时间窗 {from}..{to}：0 条。");
    for agent in ["agent-A", "agent-C"] {
        let v = json(&run(m, "timeline", Some("proj"), Some(from), Some(to), 20, agent));
        assert_eq!(v["counts"]["items"], 0, "{agent}: {v}");
        assert_eq!(v["rows_hidden_by_identity"], false, "{agent}: {v}");
        assert_eq!(v["brief"].as_str(), Some(want.as_str()), "空时间窗必须保持原句，{agent}: {v}");
    }
}

/// scope 把行筛空是内容过滤，不是身份沉默：两个视图的布尔都必须为假。
#[test]
fn a_scope_filter_that_empties_rows_is_not_identity_silence() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let a = json(&run(m, "agenda", Some("别的"), None, None, 20, "agent-A"));
    assert_eq!(a["counts"]["dated"], 0, "{a}");
    assert_eq!(a["counts"]["undated"], 0, "{a}");
    assert_eq!(a["rows_hidden_by_identity"], false, "{a}");
    assert!(!a["brief"].as_str().unwrap().contains("看不到"), "{a}");
    let t = json(&run(m, "timeline", Some("别的"), Some(NOW - 3 * HOUR), Some(NOW), 20, "agent-A"));
    assert_eq!(t["counts"]["items"], 0, "{t}");
    assert_eq!(t["rows_hidden_by_identity"], false, "{t}");
    assert!(!t["brief"].as_str().unwrap().contains("看不到"), "{t}");
}

/// 部分可见不是身份沉默：库里同时有 A、B 两边的事实，A 看得到自己那条、B 的看不见 ⇒ 假。
/// 防"库里有被藏的行就报"的近似实现。
#[test]
fn partial_row_visibility_is_not_identity_silence() {
    let (_dir, root) = seeded();
    // 时间窗内再加一条 agent-B 的私有事件：扫描得到，agent-A 过不了读闸。
    fact(&root, "a1", Fact {
        id: "f12".into(),
        agent: "agent-B".into(),
        category: "event".into(),
        text: "B 的私有事件".into(),
        due: None,
        status: None,
        occurred: Some(NOW - HOUR),
        created: NOW - HOUR,
        superseded_by: None,
        scope: "proj".into(),
    });
    owner(&root, "f12", "agent-B", "proj");
    let m = root.memory();
    let v = json(&run(m, "timeline", Some("proj"),
                      Some(NOW - 3 * HOUR), Some(NOW), 20, "agent-A"));
    assert_eq!(v["counts"]["items"], 2, "A 的可见行照常出，B 的不出: {v}");
    assert_eq!(v["rows_hidden_by_identity"], false, "部分可见不是身份沉默: {v}");
    assert!(!v["brief"].as_str().unwrap().contains("看不到"), "{v}");
    assert!(!v.to_string().contains("B 的私有事件"), "{v}");
    // agenda 同理：库里另有 B 的承诺（f11），A 看得见自己的、看不见 B 的。
    let a = json(&run(m, "agenda", Some("proj"), None, None, 20, "agent-A"));
    assert_eq!(a["counts"]["dated"], 2, "{a}");
    assert_eq!(a["rows_hidden_by_identity"], false, "{a}");
    assert!(!a["brief"].as_str().unwrap().contains("看不到"), "{a}");
}

/// agenda 的"身份沉默"只由**承诺**触发：被藏的只是一条决策时，agenda 不允许说
/// "看不到任何事项"；timeline 的内容是所有事实行，所以同一座库上它应当报。
/// 这一条钉住 `visible()` 返回被藏行而不是布尔（否则 agenda 会过报）。
#[test]
fn agenda_silence_is_about_commitments_not_any_fact() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    anchor(&root, "a1", "hub", "object", "proj");
    fact(&root, "a1", Fact {
        id: "d1".into(),
        agent: "agent-B".into(),
        category: "decision".into(),
        text: "B 的决定".into(),
        due: None,
        status: None,
        occurred: None,
        created: NOW - HOUR,
        superseded_by: None,
        scope: "proj".into(),
    });
    owner(&root, "d1", "agent-B", "proj");
    // 再藏一条**已完成**的承诺：它本来就不进 agenda，也不该触发身份沉默。
    fact(&root, "a1", Fact {
        id: "p1".into(),
        agent: "agent-B".into(),
        category: "promise".into(),
        text: "B 已经做完的承诺".into(),
        due: Some(NOW + DAY),
        status: Some("done".into()),
        occurred: None,
        created: NOW - HOUR,
        superseded_by: None,
        scope: "proj".into(),
    });
    owner(&root, "p1", "agent-B", "proj");
    let m = root.memory();
    let a = json(&run(m, "agenda", Some("proj"), None, None, 20, "agent-A"));
    assert_eq!(a["rows_hidden_by_identity"], false, "被藏的只是决策、不是承诺：{a}");
    assert!(!a["brief"].as_str().unwrap().contains("看不到"), "{a}");
    let t = json(&run(m, "timeline", Some("proj"), None, None, 20, "agent-A"));
    assert_eq!(t["rows_hidden_by_identity"], true, "timeline 的条目确实被藏：{t}");
    assert!(t["brief"].as_str().unwrap().contains("看不到"), "{t}");
}

/// 错身份（agent-C）读同一座库：事实行过了 scope、却被读闸整片拒掉 ⇒ 八格计数全 0 不能与
/// 「库里真没有」同形。三条一起断（计数 / 布尔 / brief）——只断布尔会被"恒真"骗过；
/// 只断 brief 的「看不到」会被状态通道的同款措辞骗过（agent-C 的状态同样被藏），
/// 所以这里钉的是事实通道自己的那一句。
#[test]
fn overview_a_wrong_identity_reports_hidden_facts() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let v = json(&run(m, "overview", Some("proj"), None, None, 20, "agent-C"));
    let c = &v["counts"];
    assert_eq!(c["open_commitments"], 0, "{v}");
    assert_eq!(c["recent_decisions"], 0, "{v}");
    assert_eq!(c["recent_events"], 0, "{v}");
    assert_eq!(v["facts_hidden_by_identity"], true, "库里有事实、读闸后为空必须自证: {v}");
    assert!(v["brief"].as_str().unwrap().contains("看不到任何事实行"), "{v}");
    // 对身份读同一座库：事实照常出、布尔为假、brief 里不得出现任何「看不到」。
    let own = json(&run(m, "overview", Some("proj"), None, None, 20, "agent-A"));
    assert_eq!(own["counts"]["open_commitments"], 3, "{own}");
    assert_eq!(own["facts_hidden_by_identity"], false, "{own}");
    assert!(!own["brief"].as_str().unwrap().contains("看不到"), "{own}");
}

/// 部分可见：agent-B 只看得到自己那条 f11（其余事实行被读闸拒）⇒ 不是零可见，布尔为假。
/// 这里不断 brief 里的「看不到」：B 的状态行确实也全被藏（D32-b 的口径），状态通道该说那句；
/// 本片只钉事实通道那一段不许多嘴。
#[test]
fn partial_fact_visibility_is_not_identity_silence() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let v = json(&run(m, "overview", Some("proj"), None, None, 20, "agent-B"));
    assert_eq!(v["counts"]["open_commitments"], 1, "B 只看得见 f11: {v}");
    assert_eq!(v["counts"]["recent_decisions"], 0, "{v}");
    assert_eq!(v["facts_hidden_by_identity"], false, "部分可见不是身份沉默: {v}");
    assert!(!v["brief"].as_str().unwrap().contains("事实行"), "{v}");
    // 对身份 A 同样部分可见（f11 被藏），事实通道同样不报。
    let a = json(&run(m, "overview", Some("proj"), None, None, 20, "agent-A"));
    assert_eq!(a["facts_hidden_by_identity"], false, "{a}");
    assert!(!a["brief"].as_str().unwrap().contains("看不到"), "{a}");
}

/// scope 把事实行筛空是内容过滤，不是身份沉默：两个布尔都必须为假、brief 一个字不多。
#[test]
fn a_scope_filter_that_empties_facts_is_not_identity_silence() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let v = json(&run(m, "overview", Some("别的"), None, None, 20, "agent-A"));
    assert_eq!(v["counts"]["open_commitments"], 0, "{v}");
    assert_eq!(v["facts_hidden_by_identity"], false, "{v}");
    assert_eq!(v["states_hidden_by_identity"], false, "{v}");
    assert!(!v["brief"].as_str().unwrap().contains("看不到"), "{v}");
}

/// 两条通道不许互相冒充：事实行全可见、只有状态被藏 ⇒ `states_hidden_by_identity == true`
/// 而 `facts_hidden_by_identity == false`。反方向（事实被藏）见错身份用例。
#[test]
fn hidden_states_do_not_imply_hidden_facts() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    anchor(&root, "a1", "hub", "object", "proj");
    anchor(&root, "a2", "alice", "person", "proj");
    fact(&root, "a1", Fact {
        id: "f1".into(),
        agent: "agent-A".into(),
        category: "promise".into(),
        text: "A 的承诺".into(),
        due: Some(NOW + DAY),
        status: Some("pending".into()),
        occurred: None,
        created: NOW - DAY,
        superseded_by: None,
        scope: "proj".into(),
    });
    owner(&root, "f1", "agent-A", "proj");
    // 状态行只归 agent-B：A 读得到事实、读不到状态。
    state(&root, "a2", "焦虑得很", "agent-B");
    let m = root.memory();
    let v = json(&run(m, "overview", Some("proj"), None, None, 20, "agent-A"));
    assert_eq!(v["counts"]["open_commitments"], 1, "{v}");
    assert_eq!(v["counts"]["current_states"], 0, "{v}");
    assert_eq!(v["states_hidden_by_identity"], true, "{v}");
    assert_eq!(v["facts_hidden_by_identity"], false, "事实看得到，不许冒充身份沉默: {v}");
    let brief = v["brief"].as_str().unwrap();
    assert!(brief.contains("看不到任何状态行"), "状态通道该说的话不能少: {v}");
    assert!(!brief.contains("事实行"), "事实通道一个字都不许多: {v}");
}

#[test]
fn unknown_view_is_an_error_not_a_default() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let err = views::render(
        root.memory(),
        &views::ViewRequest { view: "nope", scope: None, from_ms: None, to_ms: None, limit: 20 },
        &Viewer::agent("agent-A"),
        NOW,
    );
    assert!(err.is_err(), "未知视图必须响亮失败");
}

/// 给一条 fact 造镜像卡并盖 `sink='archive'` 章（G1 刀 A 的归档）。视图面必须把它当缺省不取，
/// 但**如实报数**（`facts_hidden_by_value` / `counts.archived`）。
fn archive_fact(root: &Root, fact_id: &str) {
    let card_id = format!("card-mirror-{fact_id}");
    root.memory()
        .conn()
        .execute(
            "INSERT INTO card(id,kind,title,text,pointers_json,scope_kind,scope_key,as_of_ms,
                              source_agent,device_id,created_at_ms,norm_version)
             VALUES(?1,'event',?2,'','[\"fact:' || ?3 || '\"]','project','proj',?4,'agent-A','dev-agent-A',?4,1)",
            (&card_id, format!("镜像 {fact_id}"), fact_id, NOW),
        )
        .unwrap();
    rc_memory::value_gate::mark_archive(root.memory().conn(), &card_id, "program_gate", NOW, 1)
        .unwrap();
}

/// G1 刀 A 的三视图消费：归档行三视图都缺省不取、身份沉默不受影响、归档条数如实可见。
#[test]
fn archived_rows_are_absent_from_all_three_views_with_a_count() {
    let (_dir, root) = seeded();
    let m = root.memory();
    archive_fact(&root, "f1"); // 一条到期承诺
    archive_fact(&root, "f5"); // 窗口内的一条事件

    let o = json(&run(m, "overview", Some("proj"), None, None, 20, "agent-A"));
    assert_eq!(o["counts"]["open_commitments"], 2, "{o}");
    assert_eq!(o["counts"]["recent_events"], 0, "{o}");
    assert_eq!(o["facts_hidden_by_value"], 2, "{o}");
    assert!(o["brief"].as_str().unwrap().contains("2 条已归档"), "{o}");
    assert!(!o.to_string().contains("明天交报告"), "归档行不许出现在 overview: {o}");

    let a = json(&run(m, "agenda", Some("proj"), None, None, 20, "agent-A"));
    assert_eq!(a["counts"]["dated"], 1, "{a}");
    assert_eq!(a["facts_hidden_by_value"], 2, "{a}");
    assert!(!a.to_string().contains("明天交报告"), "归档行不许出现在 agenda: {a}");

    let t = json(&run(m, "timeline", Some("proj"), Some(NOW - 3 * HOUR), Some(NOW), 20, "agent-A"));
    assert_eq!(t["counts"]["items"], 1, "归档后窗口内只剩那条决定: {t}");
    // 归档计数与"被藏"同口径：**只在本次扫描（这里是时间窗）内**计数——f1 在窗外，不算。
    assert_eq!(t["facts_hidden_by_value"], 1, "{t}");
    assert!(!t.to_string().contains("部署成功"), "归档行不许出现在 timeline: {t}");

    // 归档与身份沉默是两族：换身份读时，身份沉默照旧按自己的口径报（B 看得到自己的承诺，
    // 所以 facts 面不是沉默；状态行全被闸拒，states 面才是），归档计数不参与这两个布尔。
    let b = json(&run(m, "overview", Some("proj"), None, None, 20, "agent-B"));
    assert_eq!(b["facts_hidden_by_identity"], false, "{b}");
    assert_eq!(b["states_hidden_by_identity"], true, "{b}");
}
