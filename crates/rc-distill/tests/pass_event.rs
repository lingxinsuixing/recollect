//! S2c-2：event pass 的库面（设计 `2026-10-26-s2-prompt-v0` §2.3、§3、§4、§7）。
//!
//! 这一层断六件事：选批按 `(turn, 'event', '')` 独立表态且继承会话归属/scope（含 place 候选）；
//! 抽取是一次调用、审核是**另一次**调用；只有审核 `pass` 的行写库（五表 + place/participants/
//! value_json 三格）；`reject`/`uncertain` 丢行且该轮落对应终态；LLM/契约/存储三类失败的落点；
//! 以及重放的行数不变。S2e-8b 起多一条：单条坏草稿只丢这一条（`dropped`），覆盖缺口只记
//! `uncovered`，两种情况都不再折整批；只有信封坏才整批拒。**不**跑真实 LLM、不碰 holdout。
//!
//! 正反例只用通用占位名（张三/李四/某打印机/某机房），不含任何语料专名。

use rc_distill::pass_event::{
    run_event, run_event_with, run_event_with_retry, select_event_batch, EventOutcome,
};
use rc_distill::pass_event_contract::parse_event_output_lenient;
use rc_distill::pass_prompt::{event_system, event_user};
use rc_distill::review::ReviewMode;
use rc_distill::{DistillError, EventPassError};
use rc_ledger::pass_state::{self, PassKey, PassRecord, PassState, REVIEW_PENDING_REASON};
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_v4::{ensure_agent_anchor, ensure_principal_anchor};
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{AnchorRole, BatchCtx, Provenance, Scope, ScopeKind};
use rc_store::{LedgerHandle, MemoryHandle, Root};
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

mod common;
use common::{adopt_rows, insert_anchor_full, seed_bodies};

const NOW: i64 = 1_700_000_000_000;
/// t1 的正文（`quote` 必须逐字出现在它里面；`prepare` 不改中文正文）。
/// §7.5 切片一a 起 `time_text` 必须是 `quote` 的子串，所以引文带上「昨晚」——
/// 从前这一行是「时间在引文外」，正是新闸要拒的形状。
const CONFIG_BODY: &str = "昨晚在机房我把配置改回去，服务就恢复了";
const CONFIG_QUOTE: &str = "昨晚在机房我把配置改回去，服务就恢复了";
/// t2 的正文：复合句拆成一条 event，participants 含设备锚。C2 起 `time_text` 必须接地，
/// 所以正文里带「刚才」（与 `printer_event` 的 `time_text` 同字；§7.5 切片一a 起
/// 该字还必须落在 `quote` 里）。
const PRINTER_BODY: &str = "刚才某打印机卡纸，李四拆开清理后恢复";
const PRINTER_QUOTE: &str = "刚才某打印机卡纸，李四拆开清理后恢复";

/// 夹具：一个会话两轮（配置恢复 + 打印机清理），归属改成 `p1/alice/project/keep`、
/// scope 改成 `project/proj-x`；再铺四个活锚：张三、李四、某打印机、某机房（place）。
/// 创建时刻决定 `a*` 的顺序（最近创建的在前）：张三 a1、李四 a2、某打印机 a3、某机房 a4。
fn seeded() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[CONFIG_BODY, PRINTER_BODY], 1_000);
    root.transcript_mut()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id='p1', owner_agent_id='alice',
                    visibility='project', retention='keep',
                    scope_kind='project', scope_key='proj-x', ownership_src='operator'
             WHERE session_key='sess-batch-A' AND device_id='dev-batch-A'",
            [],
        )
        .unwrap();
    put_candidates(&mut root, &uids);
    (dir, root, uids)
}

/// 单轮夹具：只有 t1（配置恢复）。拒绝/空产出/故障那几条用例不需要第二轮。
fn seeded_single() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[CONFIG_BODY], 1_000);
    root.transcript_mut()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id='p1', owner_agent_id='alice',
                    visibility='project', retention='keep',
                    scope_kind='project', scope_key='proj-x', ownership_src='operator'
             WHERE session_key='sess-batch-A' AND device_id='dev-batch-A'",
            [],
        )
        .unwrap();
    put_candidates(&mut root, &uids);
    (dir, root, uids)
}

/// 四个活锚（三个实体 + 一个地点）。place 也在候选里，`place` 引用才有入口。
fn put_candidates(root: &mut Root, uids: &[String]) {
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    for (kind, canonical, created_at_ms) in [
        ("person", "张三", NOW - 1_000),
        ("person", "李四", NOW - 2_000),
        ("object", "某打印机", NOW - 3_000),
        ("place", "某机房", NOW - 4_000),
    ] {
        put_entity(
            root.memory(),
            &EntityContent {
                kind: kind.into(),
                title: canonical.into(),
                scope: scope.clone(),
                aliases: vec![],
            },
            &[Provenance { turn_uid: uids[0].clone(), anchor_role: AnchorRole::Human }],
            created_at_ms,
        )
        .unwrap();
    }
    // 候选池按会话归属过滤（隐私修复）：夹具的锚改挂会话 owner（真实链路由命名 pass 做）。
    adopt_rows(root, "anchor", "alice", Some("p1"), "project", "project", "proj-x");
}

/// 与 `crate::candidates::CANDIDATE_LIMIT` 同值：候选池只收最近创建的 30 条可见锚。
const CANDIDATE_POOL_LIMIT: usize = 30;

/// P4a-1 夹具：正文 + 目标锚（可带别名）+ 一批**更晚创建**的填充锚。
///
/// 填充锚占满候选池（`fillers` 格）；目标锚创建得更早，`fillers = CANDIDATE_POOL_LIMIT`
/// 时它们一定在池外——只可能由"正文点名"这一层补进候选。归属统一改成会话的
/// `alice / p1 / project / proj-x`（与 `seeded` 同一份可见性上下文）。
fn seeded_with_targets(
    bodies: &[&str],
    targets: &[(&str, &str, i64, &[&str])],
    fillers: usize,
) -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, bodies, 1_000);
    root.transcript_mut()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id='p1', owner_agent_id='alice',
                    visibility='project', retention='keep',
                    scope_kind='project', scope_key='proj-x', ownership_src='operator'
             WHERE session_key='sess-batch-A' AND device_id='dev-batch-A'",
            [],
        )
        .unwrap();
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    let put = |kind: &str, canonical: &str, aliases: Vec<String>, created_at_ms: i64| {
        put_entity(
            root.memory(),
            &EntityContent {
                kind: kind.into(),
                title: canonical.into(),
                scope: scope.clone(),
                aliases,
            },
            &[Provenance { turn_uid: uids[0].clone(), anchor_role: AnchorRole::Human }],
            created_at_ms,
        )
        .unwrap();
    };
    for (kind, canonical, created_at_ms, aliases) in targets {
        put(kind, canonical, aliases.iter().map(|alias| alias.to_string()).collect(), *created_at_ms);
    }
    for i in 0..fillers {
        put("person", &format!("填充锚{i:02}"), Vec::new(), NOW - 1_000 + i as i64);
    }
    adopt_rows(&root, "anchor", "alice", Some("p1"), "project", "project", "proj-x");
    (dir, root, uids)
}

fn plan_of(root: &Root) -> rc_distill::pass_event::EventPlan {
    select_event_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("夹具里的轮都还没跑过 event")
}

fn key_of(uid: &str) -> PassKey<'_> {
    PassKey { turn_uid: uid, pass_id: "event", family: "" }
}

fn record(l: &LedgerHandle, uid: &str) -> PassRecord {
    pass_state::get(l, key_of(uid))
        .unwrap()
        .unwrap_or_else(|| panic!("{uid} 没有 event 状态行"))
}

fn count(m: &MemoryHandle, table: &str) -> i64 {
    m.conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn anchor_id(m: &MemoryHandle, canonical: &str) -> String {
    m.conn()
        .query_row("SELECT id FROM anchor WHERE canonical=?1", [canonical], |r| r.get(0))
        .unwrap()
}

/// 直插一个锚 + 它的归属行（隐私用例的夹具：owner/visibility/scope 由调用方指定）。
///
/// 形状只有一份实现（`common::insert_anchor_full`）。这里的默认值就是旧夹具逐字那一份：
/// `person` / `real` / `active`、归属行的设备是会话设备、principal 为空。需要造别的形状
/// （主人锚、休眠锚、合并前的旧锚）时直接调 `insert_anchor_full`，别再复制一份 SQL。
#[allow(clippy::too_many_arguments)]
fn insert_anchor_owned(
    m: &MemoryHandle,
    id: &str,
    canonical: &str,
    owner_agent: &str,
    visibility: &str,
    scope_kind: &str,
    scope_key: &str,
    created_at_ms: i64,
) {
    insert_anchor_full(
        m,
        id,
        canonical,
        "person",
        "real",
        "active",
        owner_agent,
        None,
        Some("dev-batch-A"),
        visibility,
        scope_kind,
        scope_key,
        created_at_ms,
    );
}

/// `events` + `empty_turns` 的顶层形状（`schema_version=0`、`pass="event"`）。
fn event_reply(events: Vec<Value>, empty_turns: &[&str]) -> String {
    serde_json::json!({
        "schema_version": 0, "pass": "event",
        "events": events, "empty_turns": empty_turns
    })
    .to_string()
}

/// t1 的一条 state_change：actor a3=张三、object_text 配置文件、place a4=某机房、result done。
///
/// C3 起候选按"名字出现在目标轮正文"重排：t2 正文点了李四与某打印机，于是
/// 李四 a1、某打印机 a2；张三 a3、某机房 a4（未命中者仍按 created_at 降序）。
fn config_event() -> Value {
    serde_json::json!({
        "ref": "e1", "turn": "t1", "category": "state_change", "actor": "a3",
        "trigger": "改回去", "action": "restore_config", "object": null, "object_text": "配置文件",
        "participants": ["a3"], "place": "a4", "result": "done", "mode": "stated",
        "time_text": "昨晚", "quote": CONFIG_QUOTE, "evidence": ["t1"],
        "text": "张三昨晚把配置改回去，服务恢复。"
    })
}

/// t2 的一条 event：actor a1=李四、object a2=某打印机、participants 两个、result done。
fn printer_event() -> Value {
    serde_json::json!({
        "ref": "e2", "turn": "t2", "category": "event", "actor": "a1",
        "trigger": "清理", "action": "clear_jam", "object": "a2", "object_text": null,
        "participants": ["a1", "a2"], "place": null, "result": "done", "mode": "stated",
        "time_text": "刚才", "quote": PRINTER_QUOTE, "evidence": ["t2"],
        "text": "李四拆开清理卡纸的某打印机，设备恢复。"
    })
}

fn success_reply() -> String {
    event_reply(vec![config_event(), printer_event()], &[])
}

fn single_reply() -> String {
    event_reply(vec![config_event()], &[])
}

fn empty_reply() -> String {
    event_reply(vec![], &["t1"])
}

fn review_reply(decisions: &[(&str, &str, Option<&str>)]) -> String {
    let verdicts: Vec<Value> = decisions
        .iter()
        .map(|(reference, verdict, code)| {
            serde_json::json!({"ref": reference, "verdict": verdict, "code": code})
        })
        .collect();
    serde_json::json!({
        "schema_version": 0, "pass": "event_review", "verdicts": verdicts
    })
    .to_string()
}

fn review_pass() -> String {
    review_reply(&[("e1", "pass", None), ("e2", "pass", None)])
}

/// 测试自带的后端：按序回放脚本，记录调用次数（脚本耗尽 = `Timeout`，与回放座位同一条语义）。
struct ScriptBackend {
    replies: RefCell<VecDeque<Result<String, LlmFailure>>>,
    calls: Cell<usize>,
}

impl ScriptBackend {
    fn new(replies: Vec<Result<String, LlmFailure>>) -> Self {
        ScriptBackend { replies: RefCell::new(replies.into()), calls: Cell::new(0) }
    }

    /// 抽取 + 审核两条都成功。
    fn two(extraction: String, review: String) -> Self {
        ScriptBackend::new(vec![Ok(extraction), Ok(review)])
    }
}

impl LlmBackend for ScriptBackend {
    fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
        self.calls.set(self.calls.get() + 1);
        self.replies.borrow_mut().pop_front().unwrap_or(Err(LlmFailure::Timeout))
    }
    fn slug(&self) -> Option<&str> {
        Some("fake-event-model")
    }
}

/// `fact` 表的一行关键列（断言面）。event 复用同一张表，多两列：`place_anchor_id` 与
/// `participants_json`；`value_json` 是程序现编的 JSON 文本。
struct EventRow {
    id: String,
    subject_anchor_id: String,
    object_anchor_id: Option<String>,
    predicate: String,
    value_json: Option<String>,
    value_text: Option<String>,
    category: String,
    mode: String,
    place_anchor_id: Option<String>,
    participants_json: String,
    evidence_turn_uid: String,
    occurred_at_ms: Option<i64>,
    status: Option<String>,
    scope_kind: String,
    scope_key: String,
    source_agent: String,
    device_id: String,
    created_at_ms: i64,
    speaker_anchor_id: Option<String>,
}

fn event_rows(m: &MemoryHandle) -> Vec<EventRow> {
    let conn = m.conn();
    let mut st = conn
        .prepare(
            "SELECT id, subject_anchor_id, object_anchor_id, predicate, value_json, value_text,
                    category, mode, place_anchor_id, participants_json, evidence_turn_uid,
                    occurred_at_ms, status, scope_kind, scope_key, source_agent, device_id,
                    created_at_ms, speaker_anchor_id
             FROM fact ORDER BY created_at_ms, id",
        )
        .unwrap();
    let rows = st
        .query_map([], |r| {
            Ok(EventRow {
                id: r.get(0)?,
                subject_anchor_id: r.get(1)?,
                object_anchor_id: r.get(2)?,
                predicate: r.get(3)?,
                value_json: r.get(4)?,
                value_text: r.get(5)?,
                category: r.get(6)?,
                mode: r.get(7)?,
                place_anchor_id: r.get(8)?,
                participants_json: r.get(9)?,
                evidence_turn_uid: r.get(10)?,
                occurred_at_ms: r.get(11)?,
                status: r.get(12)?,
                scope_kind: r.get(13)?,
                scope_key: r.get(14)?,
                source_agent: r.get(15)?,
                device_id: r.get(16)?,
                created_at_ms: r.get(17)?,
                speaker_anchor_id: r.get(18)?,
            })
        })
        .unwrap();
    rows.collect::<Result<_, _>>().unwrap()
}

fn assert_no_event_rows(m: &MemoryHandle) {
    assert_eq!(count(m, "fact"), 0, "fact 在失败批里留下了行");
    assert_eq!(count(m, "card"), 0, "card 在失败批里留下了行");
    let fact_provenance: i64 = m
        .conn()
        .query_row("SELECT count(*) FROM provenance WHERE mem_table='fact'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fact_provenance, 0, "fact 的 provenance 在失败批里留下了行");
    let fact_mentions: i64 = m
        .conn()
        .query_row("SELECT count(*) FROM memory_mention WHERE mem_table='fact'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fact_mentions, 0, "fact 的 mention 在失败批里留下了行");
    assert_eq!(
        m.conn()
            .query_row(
                "SELECT count(*) FROM resource_owner WHERE mem_table IN ('fact','card')",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0,
        "失败批不写 fact/card 的归属"
    );
}

/// 1b. event 候选池的隐私边界（与 fact 同一份 `crate::candidates`）。
///
/// A 的私有锚不进 B 的候选表；他人但共享可见（project + scope 全等）的 place 锚保留，
/// 自锚仍排最后。修复前私有锚会排在 `a1`。C3 起先按"名字在目标轮正文"重排（t2 点了
/// 李四与某打印机），未命中的三格仍按创建时刻降序。
#[test]
fn event_candidate_pool_excludes_other_owners_private_anchors() {
    let (_dir, root, _uids) = seeded();
    insert_anchor_owned(
        root.memory(),
        "anchor-private-a",
        "A 的私有锚",
        "agent-synthetic-2",
        "private_agent",
        "project",
        "proj-x",
        NOW,
    );
    insert_anchor_owned(
        root.memory(),
        "anchor-shared-place",
        "共享地点",
        "agent-other",
        "project",
        "project",
        "proj-x",
        NOW - 500,
    );
    let plan = plan_of(&root);
    let canonicals: Vec<&str> =
        plan.candidates.iter().map(|candidate| candidate.canonical.as_str()).collect();
    assert!(
        !canonicals.contains(&"A 的私有锚"),
        "别人的私有锚不得进 event 候选表：{canonicals:?}"
    );
    assert_eq!(
        canonicals,
        vec!["李四", "某打印机", "共享地点", "张三", "某机房", "agent-batch-A", "p1"],
        "过滤只删行；C3 预筛把证据点名的两格提到前面，其余保持创建时刻降序，自锚与主人锚接在最后"
    );
    assert_eq!(plan.candidates[0].reference, "a1");
    assert_eq!(plan.candidates[5].reference, "a6", "自锚的位置不变");
    assert_eq!(plan.candidates[6].reference, "a7", "主人锚是最后一格");
}

/// 1. 选批：批内两轮、owner/scope/ctx 继承会话行、候选按位置编号并包含 place 锚。
#[test]
fn selection_inherits_the_session_owner_scope_and_place_candidates() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root);

    assert_eq!(plan.batch.session_key, "sess-batch-A");
    assert_eq!(plan.batch.device_id, "dev-batch-A");
    assert_eq!(plan.batch.source_agent, "agent-batch-A");
    assert_eq!(plan.ctx.source_agent, "agent-batch-A");
    assert_eq!(plan.ctx.device_id, "dev-batch-A");
    assert_eq!(plan.owner.owner_principal_id.as_deref(), Some("p1"));
    assert_eq!(plan.owner.owner_agent_id, "alice");
    assert_eq!(plan.owner.visibility, Visibility::Project);
    assert_eq!(plan.owner.retention, "keep");
    assert_eq!(plan.scope.kind, ScopeKind::Project);
    assert_eq!(plan.scope.key, "proj-x");
    let got: Vec<String> = plan.batch.rows.iter().map(|r| r.turn_uid.clone()).collect();
    assert_eq!(got, uids, "批内轮次按 created_at_ms, turn_uid 排序");

    assert_eq!(
        plan.candidates.len(),
        6,
        "三个实体锚 + 一个 place 锚 + 一个程序自锚（S2e-7）+ 一个主人锚（I2）"
    );
    // C3 证据内预筛：t2 正文点名李四与某打印机 ⇒ 这两格排到前面（彼此仍按创建时刻降序），
    // 未命中的张三、某机房保持原序。候选总数不变——只重排，不删。
    let want = [
        ("a1", "person", "李四"),
        ("a2", "object", "某打印机"),
        ("a3", "person", "张三"),
        ("a4", "place", "某机房"),
    ];
    for (index, (reference, kind, canonical)) in want.iter().enumerate() {
        let candidate = &plan.candidates[index];
        assert_eq!(&candidate.reference, reference);
        assert_eq!(&candidate.kind, kind);
        assert_eq!(&candidate.canonical, canonical);
        assert_eq!(candidate.anchor_id, anchor_id(root.memory(), canonical));
    }
    // 自锚排在原列表之后：`aN | agent | <source_agent>`，id/scope/时刻都由程序填。
    assert_eq!(plan.candidates[4].reference, "a5");
    assert_eq!(plan.candidates[4].kind, "agent");
    assert_eq!(plan.candidates[4].canonical, "agent-batch-A", "canonical = 会话的 source_agent");
    assert_eq!(plan.candidates[4].anchor_id, anchor_id(root.memory(), "agent-batch-A"));
    // I2：主人锚接在自锚之后（会话列 `owner_principal_id='p1'`，没有可读名 ⇒ canonical = id）。
    assert_eq!(plan.candidates[5].reference, "a6", "主人锚的位置连续");
    assert_eq!(plan.candidates[5].kind, "person");
    assert_eq!(plan.candidates[5].canonical, "p1");

    // 锚是活锚、scope 是批的 scope；归属取会话（不是合成默认）。
    let (status, reality, scope_kind, scope_key, created): (String, String, String, String, i64) =
        root.memory()
            .conn()
            .query_row(
                "SELECT status, reality_status, scope_kind, scope_key, created_at_ms
                 FROM anchor WHERE id=?1",
                [plan.candidates[4].anchor_id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
    assert_eq!((status.as_str(), reality.as_str()), ("active", "real"));
    assert_eq!((scope_kind.as_str(), scope_key.as_str()), ("project", "proj-x"));
    assert_eq!(created, NOW);
    let (owner_agent, created_by, visibility, retention, owner_principal): (
        String,
        String,
        String,
        String,
        Option<String>,
    ) = root
        .memory()
        .conn()
        .query_row(
            "SELECT owner_agent_id, created_by_agent, visibility, retention, owner_principal_id
             FROM resource_owner WHERE mem_table='anchor' AND mem_id=?1",
            [plan.candidates[4].anchor_id.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(owner_principal.as_deref(), Some("p1"));
    assert_eq!(owner_agent, "alice");
    assert_eq!(created_by, "agent-batch-A");
    assert_eq!(visibility, "project");
    assert_eq!(retention, "keep");

    // 状态键是 `(turn, 'event', '')`：选批不读也不写 fact/naming 的格。
    let any: i64 = root
        .ledger()
        .conn()
        .query_row("SELECT count(*) FROM extract_pass_state", [], |r| r.get(0))
        .unwrap();
    assert_eq!(any, 0, "选批不写状态");
}

/// 2. 成功批：抽取 + 审核 = 两次调用；审核 pass 的行写五表，participants/place/value_json 三格
///    逐列取程序值；每轮 done、owner 继承 alice、源轮时刻落 created_at_ms。
#[test]
fn a_reviewed_pass_writes_event_rows_with_participants_place_and_value_json() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root);
    let backend = ScriptBackend::two(success_reply(), review_pass());

    let out: EventOutcome = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(
        (out.turns, out.drafts, out.written, out.rejected, out.uncertain),
        (2, 2, 2, 0, 0)
    );
    assert_eq!((out.facts, out.cards, out.mentions), (2, 2, 3), "两条 fact、两张卡、三条提及");
    assert_eq!(out.state, PassState::Done);
    assert_eq!(backend.calls.get(), 2, "抽取一次 + 审核一次（审核是独立第二次调用）");

    let zhang = anchor_id(root.memory(), "张三");
    let li = anchor_id(root.memory(), "李四");
    let printer = anchor_id(root.memory(), "某打印机");
    let room = anchor_id(root.memory(), "某机房");

    let rows = event_rows(root.memory());
    assert_eq!(rows.len(), 2);

    // 行 1：t1 的 state_change，object 缺席 ⇒ value 取 object_text，place 落列。
    let config = &rows[0];
    assert_eq!(config.subject_anchor_id, zhang);
    assert_eq!(config.object_anchor_id, None);
    assert_eq!(config.predicate, "restore_config", "predicate = action");
    assert_eq!(config.value_text.as_deref(), Some("配置文件"), "object 为空 ⇒ value = object_text");
    assert_eq!(config.category, "state_change");
    assert_eq!(config.mode, "stated");
    assert_eq!(config.place_anchor_id.as_deref(), Some(room.as_str()), "place_anchor_id 落列");
    assert_eq!(config.participants_json, format!("[\"{zhang}\"]"));
    assert_eq!(config.evidence_turn_uid, uids[0]);
    assert_eq!(config.occurred_at_ms, None, "时间列等 time pass 回填");
    assert_eq!(config.status, None);
    assert_eq!((config.scope_kind.as_str(), config.scope_key.as_str()), ("project", "proj-x"));
    assert_eq!(config.source_agent, "agent-batch-A");
    assert_eq!(config.device_id, "dev-batch-A");
    assert_eq!(config.created_at_ms, 1_000, "源轮时刻");
    let value: Value = serde_json::from_str(
        config.value_json.as_deref().expect("event 行必须带 value_json"),
    )
    .expect("value_json 必须是合法 JSON");
    assert_eq!(value["result"], "done");
    assert_eq!(value["action"], "restore_config");
    assert_eq!(value["object_text"], "配置文件");

    // 行 2：t2 的 event，object 是设备锚 ⇒ participants 含 actor + object，value 为 null。
    let jam = &rows[1];
    assert_eq!(jam.subject_anchor_id, li);
    assert_eq!(jam.object_anchor_id.as_deref(), Some(printer.as_str()));
    assert_eq!(jam.predicate, "clear_jam");
    assert_eq!(jam.value_text, None, "object 非空且没有 object_text ⇒ value 为空");
    assert_eq!(jam.category, "event");
    assert_eq!(jam.place_anchor_id, None);
    assert_eq!(
        jam.participants_json,
        format!("[\"{li}\",\"{printer}\"]"),
        "subject 在前、object 次之，extras 的重复项只留一次"
    );
    assert_eq!(jam.evidence_turn_uid, uids[1]);
    assert_eq!(jam.created_at_ms, 1_001);
    let value: Value = serde_json::from_str(jam.value_json.as_deref().unwrap()).unwrap();
    assert_eq!(value["result"], "done");
    assert_eq!(value["action"], "clear_jam");
    assert_eq!(value["object_text"], Value::Null);

    // 归属：fact 与 card 各一行，逐列取会话归属。
    let card_ids: Vec<String> = {
        let conn = root.memory().conn();
        let mut st = conn.prepare("SELECT id FROM card ORDER BY as_of_ms, id").unwrap();
        st.query_map([], |r| r.get::<_, String>(0)).unwrap().collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(card_ids.len(), 2);
    for (table, mem_id) in
        [("fact", config.id.as_str()), ("fact", jam.id.as_str()), ("card", card_ids[0].as_str())]
    {
        let (owner, created_by, visibility, retention) = root
            .memory()
            .conn()
            .query_row(
                "SELECT owner_agent_id, created_by_agent, visibility, retention
                 FROM resource_owner WHERE mem_table=?1 AND mem_id=?2",
                [table, mem_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?, r.get::<_, String>(3)?)),
            )
            .unwrap();
        assert_eq!(owner, "alice", "{table}/{mem_id}");
        assert_eq!(created_by, "agent-batch-A", "{table}/{mem_id}");
        assert_eq!(visibility, "project", "{table}/{mem_id}");
        assert_eq!(retention, "keep", "{table}/{mem_id}");
    }

    // 每轮终态：两条都过审 ⇒ done；run_id 是 `event-<now>`，指纹是 sha256 十六进制。
    let want_run = format!("event-{NOW}");
    for uid in &uids {
        let r = record(root.ledger(), uid);
        assert_eq!(r.state, PassState::Done, "{uid}");
        assert!(r.reason.is_none(), "{uid}");
        assert_eq!(r.attempt, 0, "{uid}");
        assert_eq!(r.run_id.as_deref(), Some(want_run.as_str()), "{uid}");
        let fp = r.fingerprint.expect("done 行必须带指纹");
        assert_eq!(fp.len(), 64, "sha256 十六进制");
        assert!(fp.bytes().all(|b| b.is_ascii_hexdigit()), "{fp}");
    }
    // 状态行的 family 必须是空串（event 不分族）。
    let rows: i64 = root
        .ledger()
        .conn()
        .query_row(
            "SELECT count(*) FROM extract_pass_state WHERE pass_id='event' AND family=''",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 2);
}

/// S4 的 event 内联路径（`distill once --pass event` / `cycle once` 的默认窄审）：
/// item applied ⇒ 本次写下的**两张**卡都必须被提升为 `passed`。与 fact 内联、`hub review run`
/// 共用同一个 helper；换一条命令落库，判决不能退回 `unreviewed`。
#[test]
fn an_inline_review_promotes_the_cards_it_wrote() {
    let (_dir, root, _uids) = seeded();
    let plan = plan_of(&root);
    let backend = ScriptBackend::two(success_reply(), review_pass());
    let out =
        run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!((out.facts, out.cards), (2, 2), "夹具前提：这一批真写下两张卡");

    // 白盒读数：item 真的 applied，两张卡都是 passed。
    let (item_state, verdicts): (String, Vec<String>) = {
        let conn = root.memory().conn();
        let state = conn
            .query_row("SELECT state FROM review_item", [], |r| r.get(0))
            .unwrap();
        let mut st = conn.prepare("SELECT verdict FROM card ORDER BY id").unwrap();
        let verdicts = st
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        (state, verdicts)
    };
    assert_eq!(item_state, "applied", "夹具前提：内联窄审真的写库了");
    assert_eq!(
        verdicts,
        vec!["passed".to_string(), "passed".to_string()],
        "event 内联窄审 applied ⇒ 两张卡都必须被提升（S4）"
    );
}

/// 3. 重放：成功后选批 None；拿同一份 plan 手动重跑 ⇒ 行数与归属不变，计数为插入数。
#[test]
fn a_replay_keeps_row_counts_and_selection_returns_none() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root);
    let first = ScriptBackend::two(success_reply(), review_pass());
    run_event(root.memory(), root.transcript(), root.ledger(), &plan, &first, NOW).unwrap();
    assert!(
        select_event_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
            .unwrap()
            .is_none(),
        "成功的轮已被 event 状态表态"
    );

    let tables = ["fact", "card", "provenance", "memory_mention", "resource_owner"];
    let before: Vec<i64> = tables.iter().map(|t| count(root.memory(), t)).collect();

    let again = ScriptBackend::two(success_reply(), review_pass());
    let out = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &again, NOW + 5_000).unwrap();
    // C5b：重放读数与 fact 同口径（见 `review_run` 的
    // `applied_fact_replay_counts_the_dedup_hit_without_writing`）——去重循环照跑，
    // 库里已有同 `(轮, actor, action, object)` 的行 ⇒ 计 `duplicate`，没有行交给写库层。
    assert_eq!(out.written, 0, "两条都已在库 ⇒ 重放没有行可交");
    assert_eq!(out.duplicate, 2, "两条草稿各自命中一次去重");
    assert_eq!((out.facts, out.cards, out.mentions), (0, 0, 0), "重放不新增插入行");
    for uid in &uids {
        assert_eq!(record(root.ledger(), uid).state, PassState::Done,
                   "去重命中算已处理，不是 rejected（{uid}）");
    }
    assert_eq!(out.state, PassState::Done);

    let after: Vec<i64> = tables.iter().map(|t| count(root.memory(), t)).collect();
    assert_eq!(before, after, "重放不许长行");
}

/// C5-fix（P0）：同一轮、同一 actor、同一 `action`、`object=null`，**`object_text` 不同**
/// ⇒ 两条事，两条都要落库。旧键把字面宾语折成空串 ⇒ 第二条被记 `duplicate` 丢掉，
/// 而该轮仍是 `done`——库里少一行，没有任何痕迹（审核 R1 的 P0）。
#[test]
fn two_events_on_one_turn_with_different_object_text_both_land() {
    let (_dir, root, _uids) = seeded_single();
    let plan = plan_of(&root);

    // e1 = 配置文件，e2 = 部署脚本：轮/actor/action 全同，只有字面宾语不同。
    let mut script = config_event();
    script["ref"] = serde_json::json!("e2");
    script["object_text"] = serde_json::json!("部署脚本");
    script["text"] = serde_json::json!("张三把部署脚本改回旧版。");

    let reply = event_reply(vec![config_event(), script], &[]);
    let review = review_reply(&[("e1", "pass", None), ("e2", "pass", None)]);
    let backend = ScriptBackend::two(reply, review);
    let out = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();

    assert_eq!(
        (out.written, out.duplicate),
        (2, 0),
        "宾语不同就是两条事，一条都不许静默丢：{out:?}"
    );
    assert_eq!(count(root.memory(), "fact"), 2, "库里必须真是两行");
    let objects: Vec<String> = {
        let mut st = root.memory().conn().prepare("SELECT value_text FROM fact").unwrap();
        let got = st
            .query_map([], |r| r.get::<_, Option<String>>(0))
            .unwrap()
            .map(|row| row.unwrap().unwrap_or_default())
            .collect();
        got
    };
    assert_eq!(objects.len(), 2);
    assert!(objects.contains(&"配置文件".to_string()), "{objects:?}");
    assert!(objects.contains(&"部署脚本".to_string()), "{objects:?}");
    assert_eq!(out.state, PassState::Done);
    assert_eq!(record(root.ledger(), &_uids[0]).state, PassState::Done);
}

/// C5-fix：同一宾语、只有 `text`（说法）不同 ⇒ 仍是同一条事，去重计 `duplicate`。
/// 与上面新用例是一对：进键的是**宾语身份**（锚 id 或字面量），不是说法。
#[test]
fn a_same_object_restated_still_dedupes() {
    let (_dir, root, _uids) = seeded_single();
    let plan = plan_of(&root);

    // e2 = e1 换一种说法：`object_text` 逐字相同，只有 `text` 不同。
    let mut restated = config_event();
    restated["ref"] = serde_json::json!("e2");
    restated["text"] = serde_json::json!("张三把配置改回去了。");

    let reply = event_reply(vec![config_event(), restated], &[]);
    let review = review_reply(&[("e1", "pass", None), ("e2", "pass", None)]);
    let backend = ScriptBackend::two(reply, review);
    let out = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();

    assert_eq!(
        (out.written, out.duplicate),
        (1, 1),
        "同一宾语的两种说法只落一行：{out:?}"
    );
    assert_eq!(count(root.memory(), "fact"), 1);
    assert_eq!(out.state, PassState::Done, "去重不是失败态");
}

/// C5b：同一轮、同一 actor、同一动作的**两种说法**只落一行；同一轮同一 actor 的
/// **不同动作**不并。真实数据（`rerun-e2`）里恰好两种形状都有：`write_fact` 被写成
/// "fact"/"1 条 fact" 两条（该并），`complete_tls_verification`/`create_build_dir`/
/// `create_data_dir` 三个动作同一 actor（不该并）。
///
/// C5-fix 起进键的是**宾语身份**（锚 id 或字面量），所以这里的"两种说法"必须连
/// `object_text` 一起相同——只有 `text` 不同。`object_text` 不同是同 action 的两条事，
/// 见 `two_events_on_one_turn_with_different_object_text_both_land`。
#[test]
fn one_turn_keeps_one_row_per_actor_and_action_but_not_per_wording() {
    let (_dir, root, _uids) = seeded_single();
    let plan = plan_of(&root);

    // e2 = e1 换一种说法（text 不同；轮/actor/action/object_text 相同）⇒ 重复。
    let mut restated = config_event();
    restated["ref"] = serde_json::json!("e2");
    restated["text"] = serde_json::json!("张三把配置改回去了。");
    // e3 = 同一轮同一 actor 的另一个动作 ⇒ 另一件事，必须落库。
    let mut another = config_event();
    another["ref"] = serde_json::json!("e3");
    another["action"] = serde_json::json!("restart_service");
    another["object_text"] = serde_json::json!("服务");

    let reply = event_reply(vec![config_event(), restated, another], &[]);
    let review = review_reply(&[("e1", "pass", None), ("e2", "pass", None),
                                ("e3", "pass", None)]);
    let backend = ScriptBackend::two(reply, review);
    let out = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();

    assert_eq!((out.drafts, out.written, out.duplicate), (3, 2, 1),
               "两种说法算一条；另一个动作是另一条");
    assert_eq!(count(root.memory(), "fact"), 2);
    assert_eq!(out.state, PassState::Done, "去重不是失败态");
    let r = record(root.ledger(), &_uids[0]);
    assert_eq!(r.state, PassState::Done);
    assert!(r.reason.is_none(), "去重不写 reason");
}

/// 4. 审核 reject：该行不写库、该轮 `rejected(review_reject)`。
#[test]
fn a_rejected_row_is_not_written_and_the_turn_is_rejected() {
    let (_dir, root, uids) = seeded_single();
    let plan = plan_of(&root);
    let review = review_reply(&[("e1", "reject", Some("not_atomic"))]);
    let backend = ScriptBackend::two(single_reply(), review);

    let out = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(
        (out.turns, out.drafts, out.written, out.rejected, out.uncertain),
        (1, 1, 0, 1, 0)
    );
    assert_eq!((out.facts, out.cards, out.mentions), (0, 0, 0));
    assert_eq!(out.state, PassState::Rejected, "正常拒收不是故障，也不冒充 done");
    assert_eq!(backend.calls.get(), 2);

    assert_no_event_rows(root.memory());
    let r = record(root.ledger(), &uids[0]);
    assert_eq!(r.state, PassState::Rejected);
    assert_eq!(r.reason.as_deref(), Some("review_reject"));
}

/// 5. 审核 uncertain：丢该行、该轮 `quarantined(review_uncertain)`、零行。
#[test]
fn an_uncertain_verdict_quarantines_the_turn_without_writing() {
    let (_dir, root, uids) = seeded_single();
    let plan = plan_of(&root);
    let review = review_reply(&[("e1", "uncertain", Some("result_unsupported"))]);
    let backend = ScriptBackend::two(single_reply(), review);

    let out = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(
        (out.turns, out.drafts, out.written, out.rejected, out.uncertain),
        (1, 1, 0, 0, 1)
    );
    assert_eq!(out.state, PassState::Quarantined);
    assert_no_event_rows(root.memory());

    let r = record(root.ledger(), &uids[0]);
    assert_eq!(r.state, PassState::Quarantined);
    assert_eq!(r.reason.as_deref(), Some("review_uncertain"));
}

/// 6. 审核单元级故障（超时）：抽取已成功但**一行不写**；每轮 pending + attempt=1 + 60s 退避，
///    整批按 `Ok(Pending)` 交回（W19c2：单元级失败不是这一族的失败）。
#[test]
fn a_review_llm_failure_writes_nothing_and_leaves_every_turn_pending() {
    let (_dir, root, uids) = seeded_single();
    let plan = plan_of(&root);
    let backend = ScriptBackend::new(vec![Ok(single_reply()), Err(LlmFailure::Timeout)]);

    let out = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(out.state, PassState::Pending, "单元级失败按 pending 交回");
    assert_eq!((out.turns, out.written, out.facts, out.mentions), (1, 0, 0, 0), "落库计数全 0");
    assert_eq!(backend.calls.get(), 2, "第二次调用就是审核");
    assert_no_event_rows(root.memory());

    for uid in &uids {
        let r = record(root.ledger(), uid);
        assert_eq!(r.state, PassState::Pending, "{uid}");
        assert_eq!(r.attempt, 1, "{uid}");
        assert_eq!(r.next_retry_at_ms, Some(NOW + 60_000), "{uid}");
    }
}

/// 6b. 审核座位级故障（传输）：**不隔离**、按 `Err` 交回，每轮仍 pending（W19c2）。
#[test]
fn a_review_backend_failure_is_a_hard_failure_not_a_quarantine() {
    let (_dir, root, uids) = seeded_single();
    let plan = plan_of(&root);
    let backend = ScriptBackend::new(vec![Ok(single_reply()), Err(LlmFailure::BackendError)]);

    let err = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap_err();
    assert!(matches!(err, DistillError::Llm(LlmFailure::BackendError)), "{err}");
    assert_no_event_rows(root.memory());

    for uid in &uids {
        let r = record(root.ledger(), uid);
        assert_eq!(r.state, PassState::Pending, "{uid} 座位级失败不许落终态");
        assert!(r.reason.is_none(), "{uid}");
        assert_eq!(r.attempt, 1, "{uid}");
    }
}

/// 7. 抽取 LLM 故障：只开一次口；每轮 pending；零行。
#[test]
fn an_extraction_llm_failure_leaves_every_turn_pending() {
    let (_dir, root, uids) = seeded_single();
    let plan = plan_of(&root);
    let backend = ScriptBackend::new(vec![Err(LlmFailure::BackendError)]);

    let err = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap_err();
    assert!(matches!(err, DistillError::Llm(LlmFailure::BackendError)), "{err}");
    assert_eq!(backend.calls.get(), 1, "抽取就坏了，不再烧审核");

    assert_no_event_rows(root.memory());
    for uid in &uids {
        let r = record(root.ledger(), uid);
        assert_eq!(r.state, PassState::Pending, "{uid}");
        assert_eq!(r.attempt, 1, "{uid}");
    }
}

/// 8. 抽取坏 JSON：每轮 `rejected(json)`，零行，零审核调用。
#[test]
fn malformed_extraction_json_rejects_every_turn() {
    let (_dir, root, uids) = seeded_single();
    let plan = plan_of(&root);
    let backend = ScriptBackend::new(vec![Ok("{}".to_string())]);

    let err = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap_err();
    assert!(matches!(err, DistillError::Event(_)), "{err}");
    assert_eq!(backend.calls.get(), 1);

    assert_no_event_rows(root.memory());
    for uid in &uids {
        let r = record(root.ledger(), uid);
        assert_eq!(r.state, PassState::Rejected, "{uid}");
        assert_eq!(r.reason.as_deref(), Some("json"), "{uid}");
    }
}

/// 9. 空产出：`events=[]` + empty_turns 覆盖全部 ⇒ 每轮 empty、零行、审核不开口。
#[test]
fn an_empty_reply_is_recorded_as_empty_without_a_review_call() {
    let (_dir, root, uids) = seeded_single();
    let plan = plan_of(&root);
    let backend = ScriptBackend::new(vec![Ok(empty_reply())]);

    let out = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(
        (out.turns, out.drafts, out.written, out.rejected, out.uncertain),
        (1, 0, 0, 0, 0)
    );
    assert_eq!(out.state, PassState::Empty);
    assert_eq!(backend.calls.get(), 1, "没有待审行 ⇒ 不开第二次口");
    assert_no_event_rows(root.memory());
    for uid in &uids {
        let r = record(root.ledger(), uid);
        assert_eq!(r.state, PassState::Empty, "{uid}");
        assert!(r.reason.is_none(), "{uid}");
    }
}

/// 10. runner 侧的程序闸：place 引用非候选、participants 缺 actor 都只**丢这一条**
///     （S2e-8b）⇒ `dropped=1`、零行、零审核调用、该轮 `rejected(dropped)`。
#[test]
fn place_outside_the_candidates_and_participants_without_actor_drop_only_that_event() {
    // ① place = a9（候选只有 a1..a4）。
    let (_dir, root, uids) = seeded_single();
    let plan = plan_of(&root);
    let mut event = config_event();
    event["place"] = Value::String("a9".into());
    let backend = ScriptBackend::new(vec![Ok(event_reply(vec![event], &[]))]);
    let out = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(
        (out.turns, out.drafts, out.dropped, out.written, out.uncovered),
        (1, 0, 1, 0, 1),
        "坏 place 只丢这一条，不再整批拒"
    );
    assert_eq!(out.state, PassState::Rejected);
    assert_eq!(backend.calls.get(), 1, "没有保留草稿 ⇒ 不开审核的口");
    assert_no_event_rows(root.memory());
    let r = record(root.ledger(), &uids[0]);
    assert_eq!(r.state, PassState::Rejected);
    assert_eq!(r.reason.as_deref(), Some("unknown_candidate"), "§10.62：写坏在哪一格，不只写 dropped");

    // ② participants 不含 actor（actor a1，participants 只有 a2）。
    let (_dir2, root2, uids2) = seeded_single();
    let plan2 = plan_of(&root2);
    let mut event = config_event();
    event["participants"] = serde_json::json!(["a2"]);
    let backend = ScriptBackend::new(vec![Ok(event_reply(vec![event], &[]))]);
    let out = run_event(root2.memory(), root2.transcript(), root2.ledger(), &plan2, &backend, NOW).unwrap();
    assert_eq!((out.drafts, out.dropped, out.written), (0, 1, 0));
    assert_eq!(out.state, PassState::Rejected);
    assert_eq!(backend.calls.get(), 1);
    assert_no_event_rows(root2.memory());
    let r = record(root2.ledger(), &uids2[0]);
    assert_eq!(r.state, PassState::Rejected);
    assert_eq!(r.reason.as_deref(), Some("missing_actor"));
}

/// 11. 重选判据：未到期的 pending 不选、到期那一刻选、stale 不看时钟；done 不复活。
#[test]
fn selection_re_picks_due_pending_and_stale_turns_only() {
    let (_dir, root, uids) = seeded();
    let l = root.ledger();
    pass_state::note_attempt(l, key_of(&uids[0]), 60_000, NOW).unwrap();
    pass_state::mark_terminal(l, key_of(&uids[1]), PassState::Stale, None, NOW).unwrap();

    let selected = |now_ms: i64, limit: usize| -> Vec<String> {
        select_event_batch(root.transcript(), root.ledger(), root.memory(), limit, now_ms)
            .unwrap()
            .map(|p| p.batch.rows.into_iter().map(|r| r.turn_uid).collect())
            .unwrap_or_default()
    };
    assert_eq!(selected(NOW, 0), vec![uids[1].clone()], "退避期内的 pending 不选，stale 选");
    assert_eq!(selected(NOW + 59_999, 0), vec![uids[1].clone()], "差 1ms 仍未到期");
    assert_eq!(
        selected(NOW + 60_000, 0),
        vec![uids[0].clone(), uids[1].clone()],
        "到期那一刻（<=）重选 pending；stale 仍是可重跑"
    );
    pass_state::mark_terminal(l, key_of(&uids[0]), PassState::Done, None, NOW).unwrap();
    assert_eq!(selected(NOW + 60_000, 0), vec![uids[1].clone()], "done 不复活");
    assert_eq!(selected(NOW + 60_000, 1), vec![uids[1].clone()], "limit 截会话内前 N");
}

/// 11b. H5a：`pending(review_pending)` 不再被选批重选——产出已在 `review_item` 里等
///      `review run`，再选一次只会重抽一遍、白烧两次调用。
///
///      规则只看 reason 这一格（不看时钟）：清成 NULL、退避置到过去 ⇒ 同一条轮又能被选中。
#[test]
fn a_review_pending_turn_is_not_reselected_until_the_reason_clears() {
    let (_dir, root, uids) = seeded_single();
    let plan = plan_of(&root);
    let backend = ScriptBackend::new(vec![Ok(single_reply())]);
    let out = run_event_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
        ReviewMode::EnqueueOnly,
    )
    .unwrap();
    assert_eq!(backend.calls.get(), 1, "EnqueueOnly 只问抽取");
    assert_eq!(out.state, PassState::Pending);
    let awaiting = record(root.ledger(), &uids[0]);
    assert_eq!(awaiting.state, PassState::Pending);
    assert_eq!(awaiting.reason.as_deref(), Some(REVIEW_PENDING_REASON));
    assert_eq!(awaiting.next_retry_at_ms, None, "等审核不是等退避");

    let selected = |now_ms: i64| -> Vec<String> {
        select_event_batch(root.transcript(), root.ledger(), root.memory(), 0, now_ms)
            .unwrap()
            .map(|p| p.batch.rows.into_iter().map(|r| r.turn_uid).collect())
            .unwrap_or_default()
    };
    assert_eq!(
        selected(NOW),
        Vec::<String>::new(),
        "同批只剩这一轮待审：review_pending 不可重选"
    );
    assert_eq!(
        selected(NOW + 3_600_000),
        Vec::<String>::new(),
        "时钟走多远都一样：这一格不看时间"
    );

    // 规则只看 reason：清成 NULL + 退避到期 ⇒ 又能选。
    root.ledger()
        .conn()
        .execute(
            "UPDATE extract_pass_state SET reason=NULL, next_retry_at_ms=?1
              WHERE turn_uid=?2 AND pass_id='event' AND family=''",
            rusqlite::params![NOW - 1, uids[0]],
        )
        .unwrap();
    assert_eq!(
        selected(NOW),
        vec![uids[0].clone()],
        "reason 清空后按退避重选：不可重选的唯一原因是 review_pending"
    );
}

/// 11c（P0-2）. 改判据后旧轮自动可重跑：版本号只由 event 的 system prompt 算。
///   - 同一版本 ⇒ 已 done 的轮不重选（写状态时版本号同步落库）；
///   - 版本非空且不同 ⇒ 旧 done 轮可重选；
///   - NULL（旧库升级的老行）⇒ 不重选。
#[test]
fn a_changed_prompt_version_makes_done_event_turns_selectable_again() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root);
    let backend = ScriptBackend::two(success_reply(), review_pass());
    run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();

    let current = pass_state::prompt_version(&event_system());
    for uid in &uids {
        let r = record(root.ledger(), uid);
        assert_eq!(r.state, PassState::Done, "{uid}");
        assert_eq!(r.prompt_version.as_deref(), Some(current.as_str()),
                   "{uid}：开跑必须同步写 system-only 版本号");
    }
    let selected = |now_ms: i64| -> Vec<String> {
        select_event_batch(root.transcript(), root.ledger(), root.memory(), 0, now_ms)
            .unwrap()
            .map(|p| p.batch.rows.into_iter().map(|r| r.turn_uid).collect())
            .unwrap_or_default()
    };
    assert_eq!(selected(NOW + 1), Vec::<String>::new(), "同一版本 ⇒ done 不可重选");

    // 模拟"改了 system prompt 之后重跑"：旧行带的是上一版版本号。
    let next = pass_state::prompt_version("下一版 event system prompt（判据改了一句）");
    root.ledger()
        .conn()
        .execute(
            "UPDATE extract_pass_state SET prompt_version=?1
              WHERE pass_id='event' AND family=''",
            [next.as_str()],
        )
        .unwrap();
    assert_eq!(selected(NOW + 1), uids, "换了判据 ⇒ 旧 done 轮必须可重选");

    // NULL（升级前的老行）：不重选。
    root.ledger()
        .conn()
        .execute(
            "UPDATE extract_pass_state SET prompt_version=NULL
              WHERE pass_id='event' AND family=''",
            [],
        )
        .unwrap();
    assert_eq!(
        selected(NOW + 1),
        Vec::<String>::new(),
        "NULL 视为不可重选：升级后不自动全量重跑"
    );
}

/// 12a. 一条坏引文只丢那一条（S2e-8b）：同轮两条 event，第二条的 quote 挪了一个字
///      ⇒ `dropped=1`、保留的那条照常审核/写库、该轮 `done`。
///      读数关系：`drafts + dropped = 模型原始草稿数`（这里 1 + 1 = 2）。
#[test]
fn one_bad_quote_drops_only_that_event_and_the_rest_is_written() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root);
    let good = config_event();
    let mut bad = good.clone();
    bad["ref"] = Value::String("e2".into());
    bad["quote"] = Value::String("我把配置改回去，服务恢复了".into());
    let backend = ScriptBackend::two(
        event_reply(vec![good, bad], &["t2"]),
        review_reply(&[("e1", "pass", None)]),
    );

    let out = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(
        (out.turns, out.drafts, out.dropped, out.uncovered),
        (2, 1, 1, 0),
        "一条坏引文只丢一条，其余照常"
    );
    assert_eq!(out.drafts + out.dropped, 2, "drafts + dropped = 原始草稿数");
    assert_eq!((out.written, out.rejected, out.uncertain), (1, 0, 0));
    assert_eq!((out.facts, out.cards, out.mentions), (1, 1, 1));
    assert_eq!(out.state, PassState::Done, "保住的那条写出 ⇒ done");
    assert_eq!(backend.calls.get(), 2, "抽取 + 审核（审核只喂保留的草稿）");
    assert_eq!(count(root.memory(), "fact"), 1);

    assert_eq!(record(root.ledger(), &uids[0]).state, PassState::Done);
    assert_eq!(record(root.ledger(), &uids[1]).state, PassState::Empty);
}

/// 12b. 全部 event 都坏 ⇒ 一条都不留：`drafts=0`、`dropped=1`、零行、零审核调用，
///      该轮 `rejected(dropped)`——不是 `uncovered`：模型开过口，只是草稿被丢。
#[test]
fn a_batch_whose_events_are_all_dropped_is_rejected_as_dropped() {
    let (_dir, root, uids) = seeded_single();
    let plan = plan_of(&root);
    let mut event = config_event();
    event["quote"] = Value::String("我把配置改回去，服务恢复了".into());
    let backend = ScriptBackend::new(vec![Ok(event_reply(vec![event], &[]))]);

    let out = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(
        (out.turns, out.drafts, out.dropped, out.uncovered),
        (1, 0, 1, 1),
        "唯一一条被丢；这一轮没有合法草稿覆盖"
    );
    assert_eq!(out.drafts + out.dropped, 1, "drafts + dropped = 原始草稿数");
    assert_eq!((out.written, out.facts), (0, 0));
    assert_eq!(out.state, PassState::Rejected);
    assert_eq!(backend.calls.get(), 1, "没有保留草稿 ⇒ 不开审核的口");
    assert_no_event_rows(root.memory());

    let r = record(root.ledger(), &uids[0]);
    assert_eq!(r.state, PassState::Rejected);
    assert_eq!(r.reason.as_deref(), Some("quote_not_verbatim"), "§10.62：坏在引文，不写 dropped");
    // §10.62：被丢的批必须留下模型原文，否则事后无法复查坏在哪一格。
    let kept = rc_ledger::pass_state::latest_artifact(root.ledger(), "event", "", "extract").unwrap();
    assert!(kept.is_some(), "dropped>0 ⇒ 落 extract_pass_artifact");
}

/// 12c. 模型漏一轮（既不给 event、也不宣告 empty）⇒ 该轮 `rejected(uncovered)`，
///      另一轮照常审核/写库（S2e-8b：覆盖缺口不再折整批）。
#[test]
fn a_turn_the_model_left_out_is_recorded_as_uncovered() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root);
    let backend = ScriptBackend::two(
        single_reply(),
        review_reply(&[("e1", "pass", None)]),
    );

    let out = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(
        (out.turns, out.drafts, out.dropped, out.uncovered),
        (2, 1, 0, 1),
        "漏掉的 t2 只计 uncovered，不折整批"
    );
    assert_eq!((out.written, out.facts), (1, 1));
    assert_eq!(out.state, PassState::Done, "有草稿写出 ⇒ done");
    assert_eq!(backend.calls.get(), 2);

    assert_eq!(record(root.ledger(), &uids[0]).state, PassState::Done);
    let r = record(root.ledger(), &uids[1]);
    assert_eq!(r.state, PassState::Rejected);
    assert_eq!(r.reason.as_deref(), Some("uncovered"), "短 id 已映射回真实 turn_uid");
}

/// 13. 审核的拒收理由同样精确到格：坏 JSON ⇒ `quarantined(review_json)`；
///     一条判决都没给 ⇒ `quarantined(review_uncovered)`。
#[test]
fn a_review_failure_records_the_review_cell_as_the_reason() {
    // ① 审核回复不是 JSON 对象。
    let (_dir, root, uids) = seeded_single();
    let plan = plan_of(&root);
    let backend = ScriptBackend::two(single_reply(), "{}".to_string());

    let err = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap_err();
    assert!(matches!(err, DistillError::Event(EventPassError::Json)), "{err}");
    assert_no_event_rows(root.memory());
    for uid in &uids {
        let r = record(root.ledger(), uid);
        assert_eq!(r.state, PassState::Quarantined, "{uid}");
        assert_eq!(r.reason.as_deref(), Some("review_json"), "{uid}");
    }

    // ② 审核信封合法但没有 e1 的判决 ⇒ 缺失那一格。
    let (_dir2, root2, uids2) = seeded_single();
    let plan2 = plan_of(&root2);
    let backend2 = ScriptBackend::two(single_reply(), review_reply(&[]));

    let err = run_event(root2.memory(), root2.transcript(), root2.ledger(), &plan2, &backend2, NOW).unwrap_err();
    assert!(matches!(err, DistillError::Event(EventPassError::Uncovered(_))), "{err}");
    assert_no_event_rows(root2.memory());
    for uid in &uids2 {
        let r = record(root2.ledger(), uid);
        assert_eq!(r.state, PassState::Quarantined, "{uid}");
        assert_eq!(r.reason.as_deref(), Some("review_uncovered"), "{uid}");
    }
}

/// 14. 解析失败的原文证据（S2e-3）：抽取/审核解析失败 ⇒ 模型原文的限长副本落
///     `extract_pass_artifact`（event 的 family 是空串，stage 分 extract/review）；
///     一次失败一条、`turn_uid` 取批内第一轮；成功批一条都不留。
#[test]
fn a_parse_failure_keeps_the_model_text_as_a_ledger_artifact() {
    let artifact_count = |l: &LedgerHandle| -> i64 {
        l.conn()
            .query_row("SELECT count(*) FROM extract_pass_artifact", [], |r| r.get(0))
            .unwrap()
    };

    // ① 抽取坏 JSON：stage=extract，raw 逐字等于脚本原文，family 是空串。
    let (_dir, root, uids) = seeded_single();
    let plan = plan_of(&root);
    let bad = "{\"schema_version\": 0, \"pass\": \"event\", \"events\": [".to_string();
    let backend = ScriptBackend::new(vec![Ok(bad.clone())]);
    let err = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap_err();
    assert!(matches!(err, DistillError::Event(_)), "{err}");
    let a = pass_state::latest_artifact(root.ledger(), "event", "", "extract")
        .unwrap()
        .expect("抽取拒收必须留下原文");
    assert_eq!(a.pass_id, "event");
    assert_eq!(a.family, "", "event 没有调用变体");
    assert_eq!(a.stage, "extract");
    assert_eq!(a.run_id, format!("event-{NOW}"));
    assert_eq!(a.turn_uid, uids[0], "turn_uid 取批内第一轮");
    assert_eq!(a.raw, bad);
    assert_eq!(a.created_at_ms, NOW);
    assert_eq!(artifact_count(root.ledger()), 1, "一次失败一条");

    // ② 审核坏 JSON：stage=review，raw 是第二次调用的回复。
    let (_dir2, root2, _uids2) = seeded_single();
    let plan2 = plan_of(&root2);
    let review_raw = "{\"schema_version\": 0, \"verdicts\": [".to_string();
    let backend2 = ScriptBackend::two(single_reply(), review_raw.clone());
    let err = run_event(root2.memory(), root2.transcript(), root2.ledger(), &plan2, &backend2, NOW).unwrap_err();
    assert!(matches!(err, DistillError::Event(_)), "{err}");
    let a = pass_state::latest_artifact(root2.ledger(), "event", "", "review")
        .unwrap()
        .expect("审核拒收必须留下原文");
    assert_eq!(a.stage, "review");
    assert_eq!(a.raw, review_raw);
    assert_eq!(artifact_count(root2.ledger()), 1);

    // ③ 成功批：不存成功输出。
    let (_dir3, root3, _uids3) = seeded();
    let plan3 = plan_of(&root3);
    run_event(
        root3.memory(),
        root3.transcript(),
        root3.ledger(),
        &plan3,
        &ScriptBackend::two(success_reply(), review_pass()),
        NOW,
    )
    .unwrap();
    assert_eq!(artifact_count(root3.ledger()), 0, "成功批不留原文");
}

// ── S2e-6：错主语 / 压缩块隔离 ────────────────────────────────────────────────

/// 17. 错主语（S2e-6）：审核给 `reject + wrong_subject` ⇒ 该行不写、该轮 `rejected(review_reject)`。
///     actor 是"项目记忆/阶段台账"这类来源时，程序侧必须挡在库外。
#[test]
fn a_wrong_subject_rejection_writes_nothing_and_rejects_the_turn() {
    let (_dir, root, uids) = seeded_single();
    let plan = plan_of(&root);
    let review = review_reply(&[("e1", "reject", Some("wrong_subject"))]);
    let backend = ScriptBackend::two(single_reply(), review);

    let out = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(
        (out.turns, out.drafts, out.written, out.rejected, out.uncertain),
        (1, 1, 0, 1, 0)
    );
    assert_eq!((out.facts, out.cards, out.mentions), (0, 0, 0));
    assert_eq!(out.state, PassState::Rejected);
    assert_eq!(backend.calls.get(), 2, "抽取 + 审核");
    assert_no_event_rows(root.memory());

    let r = record(root.ledger(), &uids[0]);
    assert_eq!(r.state, PassState::Rejected);
    assert_eq!(r.reason.as_deref(), Some("review_reject"));
}

/// 18. 压缩块隔离（S2e-6）：选批认出摘要轮、当场落 `rejected(compact_summary)` 并跳过；
///     它不进 batch、不进下一次选批，普通轮照常。
#[test]
fn a_compaction_turn_is_rejected_before_it_enters_the_batch() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(
        &mut root,
        &[
            CONFIG_BODY,
            "[[compact_summary]]\n## Key Decisions\n- 先做这个",
            "谢谢",
        ],
        1_000,
    );

    let plan = select_event_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("t1/t3 可选");
    let got: Vec<String> = plan.batch.rows.iter().map(|r| r.turn_uid.clone()).collect();
    assert_eq!(got, vec![uids[0].clone(), uids[2].clone()], "压缩摘要轮不进批");

    let r = record(root.ledger(), &uids[1]);
    assert_eq!(r.state, PassState::Rejected);
    assert_eq!(r.reason.as_deref(), Some("compact_summary"));

    // 第二次选批：摘要在场（rejected，不再重扫），普通轮仍在。
    let again = select_event_batch(
        root.transcript(),
        root.ledger(),
        root.memory(),
        0,
        NOW + 1,
    )
    .unwrap()
    .unwrap();
    let got: Vec<String> = again.batch.rows.iter().map(|r| r.turn_uid.clone()).collect();
    assert_eq!(got, vec![uids[0].clone(), uids[2].clone()], "同一轮不被重复扫");
}

// ── S2e-7：会话 agent 自锚进候选 ──────────────────────────────────────────────

/// t1 的一条 state_change，`actor` 指向调用方给的自锚引用（`aN`），participants 只含它。
fn agent_actor_event(actor: &str) -> Value {
    serde_json::json!({
        "ref": "e1", "turn": "t1", "category": "state_change", "actor": actor,
        "trigger": "改回去", "action": "restore_config", "object": null, "object_text": "配置文件",
        "participants": [actor], "place": null, "result": "done", "mode": "stated",
        "time_text": "昨晚", "quote": CONFIG_QUOTE, "evidence": ["t1"],
        "text": "已确认：昨晚把配置改回去，服务恢复。"
    })
}

/// 19. 模型用自锚作 actor：审核 pass 后 event 行的 subject_anchor_id 等于那条锚的 id，
///     锚在库里是 kind='agent' 的活锚（"本 agent 做了…"有合法主体）。
#[test]
fn the_agent_self_anchor_can_be_the_actor_of_a_written_event() {
    let (_dir, root, _uids) = seeded_single();
    let plan = plan_of(&root);
    let self_anchor = plan
        .candidates
        .iter()
        .find(|candidate| candidate.kind == "agent")
        .expect("自锚必须在候选里")
        .clone();
    assert_eq!(self_anchor.reference, "a5", "自锚排在原 active 列表之后");

    let backend =
        ScriptBackend::two(event_reply(vec![agent_actor_event(&self_anchor.reference)], &[]),
                           review_reply(&[("e1", "pass", None)]));
    let out = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();

    assert_eq!((out.drafts, out.written, out.rejected), (1, 1, 0));
    let rows = event_rows(root.memory());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].subject_anchor_id, self_anchor.anchor_id, "actor 落在程序自锚上");
    assert_eq!(rows[0].participants_json, format!("[\"{}\"]", self_anchor.anchor_id));
    let (kind, status): (String, String) = root
        .memory()
        .conn()
        .query_row("SELECT kind, status FROM anchor WHERE id=?1", [&self_anchor.anchor_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((kind.as_str(), status.as_str()), ("agent", "active"));
}

/// 20. naming 已经建过同 canonical 的 agent 锚 ⇒ 不重复 append（候选里只有一个 agent 项），
///     也不造第二行锚：复用判据是 anchor_id。
#[test]
fn an_existing_agent_anchor_is_reused_instead_of_appended_twice() {
    let (_dir, root, uids) = seeded();
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    put_entity(
        root.memory(),
        &EntityContent {
            kind: "agent".into(),
            title: "agent-batch-A".into(),
            scope,
            aliases: vec![],
        },
        &[Provenance { turn_uid: uids[0].clone(), anchor_role: AnchorRole::Human }],
        NOW - 500,
    )
    .unwrap();

    let plan = plan_of(&root);
    let agents: Vec<&rc_distill::pass_prompt::FactCandidate> =
        plan.candidates.iter().filter(|candidate| candidate.kind == "agent").collect();
    assert_eq!(agents.len(), 1, "同 canonical 的自锚只出现一次");
    assert_eq!(agents[0].canonical, "agent-batch-A");
    assert_eq!(agents[0].anchor_id, anchor_id(root.memory(), "agent-batch-A"));
    assert_eq!(
        count(root.memory(), "anchor"),
        6,
        "四个夹具锚 + naming 的 agent 锚 + 主人锚，没有第二行"
    );
}

// ── 主人臂：human 轮的说话人是主人锚（审计 P2-2）────────────────────────────────

/// 会话声明（与 `seeded` 改的那一行逐列相同）与批上下文：真生产者要的就是这两样。
fn session_owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p1".to_string()),
        owner_agent_id: "alice".to_string(),
        visibility: Visibility::Project,
        retention: "keep".to_string(),
    }
}

fn session_ctx() -> BatchCtx {
    BatchCtx {
        source_agent: "agent-batch-A".to_string(),
        device_id: "dev-batch-A".to_string(),
    }
}

fn session_scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-x".into() }
}

/// 19b（审计 P2-2）. human 轮的 event 行，`speaker_anchor_id` 是**主人锚**那一格。
///
/// 两条锚都由真生产者建：`ensure_agent_anchor` 出自锚、`ensure_principal_anchor` 出主人锚
/// （它的归属行不带设备）。写库层按"归属行不带设备"找回主人，夹具此前只会造"person +
/// 带设备"的行，于是这一臂在用例里永远走不到，也没有任何断言盯着它。
#[test]
fn a_human_turn_writes_the_principal_anchor_as_its_event_speaker() {
    let (_dir, root, uids) = seeded_single();
    let (owner, ctx, scope) = (session_owner(), session_ctx(), session_scope());
    // 选批内部先走一遍两个生产者；再显式调一次：同 (canonical, scope) 落在同一格（幂等），
    // 返回的就是这一批真正在用的那两格。
    let plan = plan_of(&root);
    let self_anchor =
        ensure_agent_anchor(root.memory(), &ctx, &owner, &scope, NOW).unwrap().id;
    let principal_anchor =
        ensure_principal_anchor(root.memory(), "p1", None, &owner, &scope, NOW)
            .unwrap()
            .expect("p1 是已知主人")
            .id;
    assert_ne!(self_anchor, principal_anchor, "自锚与主人锚必须是两格");
    assert_eq!(plan.candidates[4].anchor_id, self_anchor, "a5 就是 `ensure_agent_anchor` 那一格");
    assert_eq!(
        plan.candidates[5].anchor_id, principal_anchor,
        "a6 就是 `ensure_principal_anchor` 那一格"
    );

    // 生产者契约：主人锚的归属行不带设备，自锚带会话设备——这一格就是两者在库里的分界。
    let device_of = |anchor_id: &str| -> Option<String> {
        root.memory()
            .conn()
            .query_row(
                "SELECT device_id FROM resource_owner WHERE mem_table='anchor' AND mem_id=?1",
                [anchor_id],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(device_of(&principal_anchor), None, "主人锚的归属行不带设备");
    assert_eq!(device_of(&self_anchor).as_deref(), Some("dev-batch-A"), "自锚带会话设备");

    let backend = ScriptBackend::two(single_reply(), review_reply(&[("e1", "pass", None)]));
    let out =
        run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(out.written, 1, "夹具前提：这一条真写进库");

    // t1 是 human 轮 ⇒ 说话人是主人锚：不是自锚、也不是 NULL。
    let rows = event_rows(root.memory());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].evidence_turn_uid, uids[0], "证据轮就是那条 human 轮");
    assert_eq!(
        rows[0].speaker_anchor_id.as_deref(),
        Some(principal_anchor.as_str()),
        "human 说的那句话，说话人是主人锚"
    );
    assert_ne!(
        rows[0].speaker_anchor_id.as_deref(),
        Some(self_anchor.as_str()),
        "不许回落成会话自锚"
    );
}

// ── P4a-1：正文点名的第三方锚补进候选 ────────────────────────────────────────

/// ① 池外（候选池只有 30 格）的锚被正文点名、别名也算 ⇒ 补进候选且 a-ref 可被解析。
#[test]
fn a_third_party_anchor_named_in_the_batch_joins_the_candidates_with_a_resolvable_ref() {
    let body = "小安昨晚把配置改回去，服务就恢复了";
    let (_dir, root, _uids) = seeded_with_targets(
        &[body],
        &[("agent", "Agnes", NOW - 50_000, &["小安"])],
        CANDIDATE_POOL_LIMIT,
    );
    let plan = plan_of(&root);
    let third = plan
        .candidates
        .iter()
        .find(|candidate| candidate.canonical == "Agnes")
        .expect("正文点名了别名「小安」：Agnes 必须被补进候选");
    assert_eq!(
        plan.candidates.iter().filter(|candidate| candidate.canonical == "Agnes").count(),
        1,
        "同一个锚只补一格"
    );
    assert_eq!(third.anchor_id, anchor_id(root.memory(), "Agnes"));
    assert_eq!(third.mark, None, "第三方锚没有 self/principal 章");

    let reply = event_reply(
        vec![serde_json::json!({
            "ref": "e1", "turn": "t1", "category": "state_change", "actor": third.reference,
            "trigger": "改回去", "action": "restore_config", "object": null,
            "object_text": "配置文件", "participants": [third.reference], "place": null,
            "result": "done", "mode": "stated", "time_text": "昨晚", "quote": body,
            "evidence": ["t1"], "text": "Agnes 昨晚把配置改回去，服务恢复。"
        })],
        &[],
    );
    let parsed = parse_event_output_lenient(&reply, &plan.batch, &plan.candidates).unwrap();
    assert_eq!(parsed.drafts.len(), 1, "引用必须解析成一条 draft：{:?}", parsed.dropped);
    assert_eq!(parsed.drafts[0].actor_anchor_id, third.anchor_id, "a-ref 按位置解析回 Agnes");
}

/// ② 名字在库里没有锚 ⇒ 候选段与补位前**逐字一致**（池 + 自锚 + 主人锚，一格不多）。
#[test]
fn a_name_without_an_anchor_leaves_the_rendered_candidates_untouched() {
    let (_dir, root, _uids) = seeded_with_targets(
        &["王五昨晚把配置改回去，服务就恢复了", "好的，收到"],
        &[
            ("person", "张三", NOW - 1_000, &[]),
            ("person", "李四", NOW - 2_000, &[]),
            ("object", "某打印机", NOW - 3_000, &[]),
            ("place", "某机房", NOW - 4_000, &[]),
        ],
        0,
    );
    let plan = plan_of(&root);
    let user = event_user(&plan.batch, &plan.candidates, &plan.ctx_turns);
    let rendered: Vec<&str> = user.lines().filter(|line| line.starts_with("- a")).collect();
    assert_eq!(
        rendered,
        vec![
            "- a1 | person | 张三",
            "- a2 | person | 李四",
            "- a3 | object | 某打印机",
            "- a4 | place | 某机房",
            "- a5 | agent | agent-batch-A [self]",
            "- a6 | person | p1 [principal]",
        ],
        "王五在库里没有锚：候选段必须逐字不变"
    );
}

/// ③ 补位插在两格程序锚之前：自锚仍倒数第二、主人锚仍最后一格，编号从 a1 起连续。
#[test]
fn appended_third_party_anchors_keep_the_agent_and_principal_slots_in_place() {
    let (_dir, root, _uids) = seeded_with_targets(
        &["Agnes 昨晚把配置改回去，服务就恢复了"],
        &[("person", "Agnes", NOW - 50_000, &[])],
        CANDIDATE_POOL_LIMIT,
    );
    let plan = plan_of(&root);
    let total = CANDIDATE_POOL_LIMIT + 3; // 池 30 + 补位 1 + 自锚 + 主人锚
    assert_eq!(plan.candidates.len(), total);

    let agent = &plan.candidates[total - 2];
    assert_eq!((agent.kind.as_str(), agent.canonical.as_str()), ("agent", "agent-batch-A"));
    assert_eq!(agent.reference, format!("a{}", total - 1), "自锚仍是倒数第二格");
    assert_eq!(agent.mark, Some("self"));

    let principal = &plan.candidates[total - 1];
    assert_eq!(principal.canonical, "p1");
    assert_eq!(principal.reference, format!("a{total}"), "主人锚仍是最后一格");
    assert_eq!(principal.mark, Some("principal"));

    // 位置即契约：第 i 格就是 a{i+1}，从 a1 起连续。
    for (index, candidate) in plan.candidates.iter().enumerate() {
        assert_eq!(candidate.reference, format!("a{}", index + 1), "第 {index} 格编号错位");
    }
    // 补位进来的 Agnes 正是正文点名的那一方 ⇒ 证据预筛把它提到最前。
    assert_eq!(plan.candidates[0].canonical, "Agnes");
}

/// ④ 多个第三方锚：按"出现在几轮里"降序、再按 canonical 字典序，最多补 6 格。
#[test]
fn mentioned_anchors_are_ordered_by_turn_count_then_canonical_and_capped_at_six() {
    let (_dir, root, _uids) = seeded_with_targets(
        &["甲和乙还有丙去机房", "甲和丁、己、庚、戊对一下"],
        &[
            ("person", "甲", NOW - 50_000, &[]),
            ("person", "乙", NOW - 50_001, &[]),
            ("person", "丙", NOW - 50_002, &[]),
            ("person", "丁", NOW - 50_003, &[]),
            ("person", "戊", NOW - 50_004, &[]),
            ("person", "己", NOW - 50_005, &[]),
            ("person", "庚", NOW - 50_006, &[]),
        ],
        CANDIDATE_POOL_LIMIT,
    );
    let plan = plan_of(&root);
    let got: Vec<&str> =
        plan.candidates.iter().take(6).map(|candidate| candidate.canonical.as_str()).collect();
    assert_eq!(
        got,
        vec!["甲", "丁", "丙", "乙", "己", "庚"],
        "甲出现在两轮排最前；其余按 canonical 字典序；第 7 个被上限截掉"
    );
    assert!(
        plan.candidates.iter().all(|candidate| candidate.canonical != "戊"),
        "补位上限 6：字典序最后一个不进来"
    );
    assert_eq!(
        plan.candidates.len(),
        CANDIDATE_POOL_LIMIT + 6 + 2,
        "池 30 + 补位 6 + 两格程序锚"
    );
}

/// §10.66 C：应用插入的运行期通知不是一次 utterance —— 选批认出它、当场落
/// `rejected(not_utterance)` 并跳过；它不进批，真事件轮照常可选。
#[test]
fn a_runtime_notice_is_rejected_before_it_enters_the_event_batch() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(
        &mut root,
        &[
            CONFIG_BODY,
            "Model response interrupted. Completed output is preserved; automatic continuation stopped.",
        ],
        1_000,
    );
    put_candidates(&mut root, &uids);

    let plan = select_event_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("t1 可选");
    let got: Vec<String> = plan.batch.rows.iter().map(|r| r.turn_uid.clone()).collect();
    assert_eq!(got, vec![uids[0].clone()], "运行期通知不进批");

    let r = record(root.ledger(), &uids[1]);
    assert_eq!(r.state, PassState::Rejected);
    assert_eq!(r.reason.as_deref(), Some("not_utterance"));
}

/// §10.70：单条草稿被宽松 parser 丢掉时，原文要落在**那一轮**上，而不是 `uids[0]`。
#[test]
fn a_parser_dropped_turn_that_is_not_the_batch_head_keeps_its_own_artifact() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root);
    let mut bad = printer_event();
    bad["quote"] = Value::String("刚才某打印机卡纸，李四拆开清理后恢复。".into());
    let backend = ScriptBackend::new(vec![Ok(event_reply(vec![bad], &["t1"]))]);
    let out = run_event(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!((out.drafts, out.dropped), (0, 1));
    assert_eq!(record(root.ledger(), &uids[1]).reason.as_deref(), Some("quote_not_verbatim"));
    let n: i64 = root
        .ledger()
        .conn()
        .query_row(
            "SELECT count(*) FROM extract_pass_artifact WHERE pass_id='event' AND turn_uid=?1",
            [&uids[1]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "被丢稿的那一轮必须是原文的归属轮");
}

/// 12b. 空轮重试的**触发边界**（2026-10-30-retry-empty）：只有"模型说这一批什么都没有"
///     才算判空，才重采。草稿被宽松 parser 丢掉（`dropped`）与模型漏轮（`uncovered`）都
///     **不**重试——那不是"模型说没有"，是"模型说了却没通过"，重采只会再产一次同形的坏草稿、
///     白烧一次调用与一次审核。脚本各只给一条回复：真重试 ⇒ 座位耗尽（`Timeout`）⇒ 本用例红。
///
/// 读数那一侧同时钉住：开关开着时 `retries`/`recovered` 是 `Some(0)`（如实报"一次都没重试"），
/// 而不是"没有这两格"——后者是开关关着的形状。
#[test]
fn only_a_truly_empty_reply_triggers_the_retry() {
    // ① 有一条草稿被丢（place 引用批外候选 a9）⇒ dropped=1，这一批不是"判空"。
    let (_dir, root, _uids) = seeded_single();
    let plan = plan_of(&root);
    let mut event = config_event();
    event["place"] = Value::String("a9".into());
    let backend = ScriptBackend::new(vec![Ok(event_reply(vec![event], &[]))]);
    let out = run_event_with_retry(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
        ReviewMode::Inline,
        2,
    )
    .unwrap();
    assert_eq!((out.dropped, out.retries, out.recovered), (1, Some(0), Some(0)));
    assert_eq!(backend.calls.get(), 1, "有草稿被丢的批不许重试");

    // ② 模型漏轮（`events=[]` 但 `empty_turns` 也空）⇒ uncovered=1，同样不是"判空"。
    let (_dir2, root2, _uids2) = seeded_single();
    let plan2 = plan_of(&root2);
    let backend2 = ScriptBackend::new(vec![Ok(event_reply(vec![], &[]))]);
    let out2 = run_event_with_retry(
        root2.memory(),
        root2.transcript(),
        root2.ledger(),
        &plan2,
        &backend2,
        NOW,
        ReviewMode::Inline,
        2,
    )
    .unwrap();
    assert_eq!((out2.uncovered, out2.retries, out2.recovered), (1, Some(0), Some(0)));
    assert_eq!(backend2.calls.get(), 1, "模型漏轮的批不许重试");
}
