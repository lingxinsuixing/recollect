//! S2a-2：naming pass 的库面（设计 `2026-10-26-s2-prompt-v0` §2.1、§3、§7）。
//!
//! 这一层只断六件事：选批的判据（没有状态行 / 到期的 pending / stale 可重选，其余不选）、
//! 归属/scope 的继承、成功批落的四张表与 `done` 状态、三类失败的落点
//! （`rejected(schema)` / `pending` + 退避 / `empty`），以及重放的行数与归属不变。
//! **不**跑真实 LLM、不碰 CLI/fact/event/time。

use rc_distill::pass_naming::{
    drop_noise_anchors, is_noise_name, run_naming, run_naming_with, select_naming_batch,
};
use rc_distill::prompt_v4::{a1_shape, a1_system};
use rc_distill::review::ReviewMode;
use rc_distill::DistillError;
use rc_ledger::pass_state::{self, PassKey, PassRecord, PassState, REVIEW_PENDING_REASON};
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_v4::{ensure_agent_anchor, A1Anchor, A1Batch, A1Issue, A1Mention, A1Unresolved};
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{AnchorRole, BatchCtx, Provenance, Scope, ScopeKind};
use rc_store::{LedgerHandle, MemoryHandle, Root};
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

mod common;
use common::{adopt_rows, seed_bodies};

const NOW: i64 = 1_700_000_000_000;

/// 夹具：一个会话 3 轮（`common::seed_bodies` 的同一条导入链），再把会话归属改成
/// `p1/alice/project/keep`、scope 改成 `project/proj-x`——这是 S2a-2 要继承的那一行。
///
/// S3a-3b 的补充：正文里必须**逐字**出现 `good_reply` 的 canonical / 别名（`张三` / `老张`）。
/// 程序闸的 9c 判据是"canonical 与每个 alias 逐字出现在该批某轮正文，或等于某个 mention 的
/// surface"，而旧的合成体（"合成第 N 句话…"）里一个名字都没有——夹具补正文，闸门不动。
fn seeded() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(
        &mut root,
        &[
            "张三说明天把报价单发给你",
            "老张说报价单后天到，张三负责跟进",
            "小结：老张把报价单交给了张三",
        ],
        1_000,
    );
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
    (dir, root, uids)
}

fn plan_of(root: &Root) -> rc_distill::NamingPlan {
    select_naming_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("夹具里 3 轮都还没表态")
}

fn key_of(uid: &str) -> PassKey<'_> {
    PassKey { turn_uid: uid, pass_id: "naming", family: "" }
}

/// 选批 → 批内 uid（顺序即会话内顺序）。没有可选中的轮次 ⇒ 空表。
fn selected_uids(root: &Root, limit: usize, now_ms: i64) -> Vec<String> {
    select_naming_batch(root.transcript(), root.ledger(), root.memory(), limit, now_ms)
        .unwrap()
        .map(|p| p.batch.rows.into_iter().map(|r| r.turn_uid).collect())
        .unwrap_or_default()
}

fn record(l: &LedgerHandle, uid: &str) -> PassRecord {
    pass_state::get(l, key_of(uid))
        .unwrap()
        .unwrap_or_else(|| panic!("{uid} 没有 naming 状态行"))
}

fn count(m: &MemoryHandle, table: &str) -> i64 {
    m.conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// 1 个 person 锚 + 1 条提及；`t1`/`t2` 指向批内真实行，kind 取九值闭集里的 `person`。
fn good_reply() -> String {
    r#"{"schema_version":4,"pass":"a1",
      "anchors":[{"ref":"a1","kind":"person","canonical":"张三","aliases":["老张"],
                  "reality_status":"real","evidence":["t1","t2"],"reason":"两轮点名"}],
      "mentions":[{"anchor_ref":"a1","surface":"老张","turn_uid":"t2","speaker_ref":null,
                   "discourse_role":"mentioned","resolution":"resolved","confidence":0.9}],
      "issues":[],"unresolved_surfaces":[]}"#
        .to_string()
}

/// 合法空批：六键齐全、四张列表全空。
fn empty_reply() -> String {
    r#"{"schema_version":4,"pass":"a1","anchors":[],"mentions":[],"issues":[],
        "unresolved_surfaces":[]}"#
        .to_string()
}

/// 真实冒烟的形状（发现 2）：一个真锚 + 一个反引号代码令牌；两条提及各指向一个。
/// 过滤闸必须留下张三、丢掉令牌，并连带丢掉令牌那条提及。
fn noise_reply() -> String {
    r#"{"schema_version":4,"pass":"a1",
      "anchors":[
        {"ref":"a1","kind":"person","canonical":"张三","aliases":["老张"],
         "reality_status":"real","evidence":["t1","t2"],"reason":"两轮点名"},
        {"ref":"a2","kind":"object","canonical":"`99c05800…`","aliases":[],
         "reality_status":"unknown","evidence":["t1"],"reason":"代码令牌"}],
      "mentions":[
        {"anchor_ref":"a1","surface":"老张","turn_uid":"t2","speaker_ref":null,
         "discourse_role":"mentioned","resolution":"resolved","confidence":0.9},
        {"anchor_ref":"a2","surface":"`99c05800…`","turn_uid":"t1","speaker_ref":null,
         "discourse_role":"mentioned","resolution":"resolved","confidence":0.8}],
      "issues":[],"unresolved_surfaces":[]}"#
        .to_string()
}

/// W14e 的两臂同批：一个**形状类**（提交哈希）与一个**真名**（批里没有它的落点）。
/// 闸必须把它们分到两个桶，而不是一起数进真名桶。
fn unresolved_reply() -> String {
    r#"{"schema_version":4,"pass":"a1",
      "anchors":[{"ref":"a1","kind":"person","canonical":"张三","aliases":["老张"],
                  "reality_status":"real","evidence":["t1","t2"],"reason":"两轮点名"}],
      "mentions":[{"anchor_ref":"a1","surface":"老张","turn_uid":"t2","speaker_ref":null,
                   "discourse_role":"mentioned","resolution":"resolved","confidence":0.9}],
      "issues":[],
      "unresolved_surfaces":[
        {"surface":"14d6e8ce","turn_uid":"t1","reason":"提交哈希，不是名字"},
        {"surface":"那台打印机","turn_uid":"t3","reason":"信息不足"}]}"#
        .to_string()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Good,
    Empty,
    BadJson,
    Timeout,
    Noise,
    /// W14e：一臂真是名字（批里没有它的落点），一臂是形状类（提交哈希）。
    Unresolved,
}

/// naming 窄审回复（S3b-3c 的第 3 次调用）：`Good`/`Noise` 过滤后都只剩 `a1`，
/// 所以只判它 pass（`pass` 的 code 必须是显式 null）。
fn naming_review_pass_a1() -> String {
    r#"{"schema_version":0,"pass":"naming_review",
        "verdicts":[{"ref":"a1","verdict":"pass","code":null}]}"#
        .to_string()
}

/// 测试自带的后端：按模式回放，记录调用次数。生产代码里没有它的引用点。
///
/// 调用序（S3b-3c）：第 1、2 次是 A1 双跑（按 mode 回），第 3 次是 naming 窄审
/// （回 [`naming_review_pass_a1`]）。`Empty` 走不到第 3 次（不入队），`BadJson`/`Timeout`
/// 在双跑里就失败。
struct FakeBackend {
    mode: Mode,
    calls: Cell<usize>,
}

impl FakeBackend {
    fn new(mode: Mode) -> Self {
        FakeBackend { mode, calls: Cell::new(0) }
    }
}

impl LlmBackend for FakeBackend {
    fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
        let n = self.calls.get() + 1;
        self.calls.set(n);
        if n == 3 {
            return Ok(naming_review_pass_a1());
        }
        match self.mode {
            Mode::Good => Ok(good_reply()),
            Mode::Empty => Ok(empty_reply()),
            Mode::BadJson => Ok("{}".to_string()),
            Mode::Timeout => Err(LlmFailure::Timeout),
            Mode::Noise => Ok(noise_reply()),
            Mode::Unresolved => Ok(unresolved_reply()),
        }
    }
    fn slug(&self) -> Option<&str> {
        Some("fake-model")
    }
}

/// `resource_owner` 的一行（naming 批的断言面）。
struct OwnerRow {
    owner_principal_id: Option<String>,
    owner_agent_id: String,
    created_by_agent: String,
    visibility: String,
    retention: String,
    scope_kind: String,
    scope_key: String,
    source_agent: Option<String>,
    device_id: Option<String>,
    created_at_ms: i64,
}

fn owner_of(m: &MemoryHandle, anchor_id: &str) -> OwnerRow {
    m.conn()
        .query_row(
            "SELECT owner_principal_id, owner_agent_id, created_by_agent, visibility, retention,
                    scope_kind, scope_key, source_agent, device_id, created_at_ms
             FROM resource_owner WHERE mem_table='anchor' AND mem_id=?1",
            [anchor_id],
            |r| {
                Ok(OwnerRow {
                    owner_principal_id: r.get(0)?,
                    owner_agent_id: r.get(1)?,
                    created_by_agent: r.get(2)?,
                    visibility: r.get(3)?,
                    retention: r.get(4)?,
                    scope_kind: r.get(5)?,
                    scope_key: r.get(6)?,
                    source_agent: r.get(7)?,
                    device_id: r.get(8)?,
                    created_at_ms: r.get(9)?,
                })
            },
        )
        .unwrap()
}

fn assert_no_memory_rows(m: &MemoryHandle) {
    for table in ["anchor", "alias", "mention", "provenance", "resource_owner"] {
        assert_eq!(count(m, table), 0, "{table} 在失败批里留下了行");
    }
}

/// 1. 选批：批内 3 轮、ctx/owner/scope 继承会话行、候选只给 kind/canonical/aliases；
///    pending 未到期 / running / done 都不重选（pending 到期与 stale 的判据见下面第 7–10 条）。
#[test]
fn selection_inherits_the_session_owner_and_scope_and_skips_stated_turns() {
    let (_dir, root, uids) = seeded();
    // 一个已有活锚 + 别名：候选只作提示，结构里没有 id 位。
    put_entity(
        root.memory(),
        &EntityContent {
            kind: "person".into(),
            title: "李四".into(),
            scope: Scope { kind: ScopeKind::Person, key: "u-alice".into() },
            aliases: vec!["小李".into()],
        },
        &[Provenance { turn_uid: uids[0].clone(), anchor_role: AnchorRole::Human }],
        NOW,
    )
    .unwrap();

    // C4a：候选池与 fact/event 共用归属谓词 ⇒ 夹具的锚必须挂到本会话的 owner/scope 上，
    // 否则它对该批不可见（这正是 `a_foreign_private_anchor_never_reaches_the_candidate_list` 断的事）。
    adopt_rows(&root, "anchor", "alice", Some("p1"), "project", "project", "proj-x");

    let plan = plan_of(&root);
    assert_eq!(plan.batch.session_key, "sess-batch-A");
    assert_eq!(plan.batch.device_id, "dev-batch-A");
    assert_eq!(plan.batch.source_agent, "agent-batch-A");
    assert_eq!(plan.ctx.source_agent, "agent-batch-A");
    assert_eq!(plan.ctx.device_id, "dev-batch-A");
    let got: Vec<String> = plan.batch.rows.iter().map(|r| r.turn_uid.clone()).collect();
    assert_eq!(got, uids, "批内轮次按 created_at_ms, turn_uid 排序");
    assert_eq!(plan.owner.owner_principal_id.as_deref(), Some("p1"));
    assert_eq!(plan.owner.owner_agent_id, "alice");
    assert_eq!(plan.owner.visibility, Visibility::Project);
    assert_eq!(plan.owner.retention, "keep");
    assert_eq!(plan.scope.kind, ScopeKind::Project);
    assert_eq!(plan.scope.key, "proj-x");
    assert_eq!(plan.candidates.len(), 1, "{}", plan.candidates.len());
    assert_eq!(plan.candidates[0].kind, "person");
    assert_eq!(plan.candidates[0].canonical, "李四");
    assert_eq!(plan.candidates[0].aliases, vec!["小李"]);

    // 三条都表态，但状态各异：未到期的 pending / running / done 都不重选。
    for (i, uid) in uids.iter().enumerate() {
        match i {
            0 => pass_state::note_attempt(root.ledger(), key_of(uid), 60_000, NOW).unwrap(),
            1 => pass_state::mark_running(root.ledger(), key_of(uid), "fp", "run", NOW).unwrap(),
            _ => pass_state::mark_terminal(root.ledger(), key_of(uid), PassState::Done, None, NOW)
                .unwrap(),
        }
    }
    assert_eq!(record(root.ledger(), &uids[0]).state, PassState::Pending);
    assert_eq!(record(root.ledger(), &uids[1]).state, PassState::Running);
    assert!(
        select_naming_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
            .unwrap()
            .is_none(),
        "未到期的 pending、running、done 都算没有活"
    );
}

/// 1b. C4a：候选池与 fact/event 共用同一条可见性谓词。**别人家的名字不许进提示面**——
/// 命名会按候选名"别再起一个"，一个越界的私有名进了提示，就等于把它复刻进本会话。
///
/// 谓词的两条分支各断一次：
/// ① 别人的私有锚（owner=bob、private_agent）⇒ 不可见；
/// ② 别人的共享锚但 scope 不同（owner=bob、project、`other-proj`）⇒ 不可见（scope 必须全等）；
/// ③ 自己的锚跨项目仍可见（owner=alice、`other-proj`）——这是**有意**的：锚跟着 owner 走，
///    同一个人的名字在几个项目里应当能被复用，否则每个项目都要给同一个人再起一遍名。
#[test]
fn a_foreign_private_anchor_never_reaches_the_candidate_list() {
    let (_dir, root, uids) = seeded();
    let here = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    for canonical in ["李四", "王五", "孙七", "赵六"] {
        put_entity(
            root.memory(),
            &EntityContent {
                kind: "person".into(),
                title: canonical.into(),
                scope: here.clone(),
                aliases: if canonical == "李四" { vec!["老四".to_string()] } else { Vec::new() },
            },
            &[Provenance { turn_uid: uids[0].clone(), anchor_role: AnchorRole::Human }],
            NOW,
        )
        .unwrap();
    }
    // 四个锚先都挂成本会话可见，再逐个改成"该可见 / 不该可见"。
    adopt_rows(&root, "anchor", "alice", Some("p1"), "project", "project", "proj-x");
    let mem = root.memory().conn();
    mem.execute(
        "UPDATE resource_owner SET owner_agent_id='bob', owner_principal_id=NULL,
                visibility='private_agent'
          WHERE mem_id=(SELECT id FROM anchor WHERE canonical='王五')",
        [],
    )
    .unwrap();
    mem.execute(
        "UPDATE resource_owner SET owner_agent_id='bob', owner_principal_id=NULL,
                visibility='project', scope_key='other-proj'
          WHERE mem_id=(SELECT id FROM anchor WHERE canonical='孙七')",
        [],
    )
    .unwrap();
    mem.execute(
        "UPDATE resource_owner SET scope_key='other-proj'
          WHERE mem_id=(SELECT id FROM anchor WHERE canonical='赵六')",
        [],
    )
    .unwrap();

    let plan = plan_of(&root);
    let mut names: Vec<&str> = plan.candidates.iter().map(|c| c.canonical.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["李四", "赵六"], "候选池的归属过滤与 fact/event 不同源：{names:?}");
    let li = plan.candidates.iter().find(|c| c.canonical == "李四").unwrap();
    assert_eq!(li.aliases, vec!["老四"], "别名仍要进提示（命名的主料）");
}

/// 2. 成功批：四张表 + 每个锚的归属逐列；每轮 `done`、指纹 64 hex、attempt=0。
#[test]
fn a_successful_pass_writes_rows_owner_and_done_states() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root);
    let backend = FakeBackend::new(Mode::Good);

    let out = run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();

    assert_eq!(out.turns, 3);
    assert_eq!(out.anchors, 1, "双跑同一个锚 ⇒ 一个");
    assert_eq!(out.mentions, 1);
    assert_eq!(out.filtered, 0, "干净的一批不该丢锚");
    assert_eq!(out.state, PassState::Done);
    assert_eq!(backend.calls.get(), 3, "A1 双跑 + 窄审一次");

    assert_eq!(count(root.memory(), "anchor"), 1);
    assert_eq!(count(root.memory(), "alias"), 1);
    assert_eq!(count(root.memory(), "mention"), 1);
    assert_eq!(count(root.memory(), "provenance"), 2, "evidence 两轮各一条");
    let status: String = root
        .memory()
        .conn()
        .query_row("SELECT status FROM anchor WHERE canonical='张三'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(status, "active", "双跑交集 ⇒ stable ⇒ active");

    // 反向保护（P0-2）：这一条 `person` 的证据轮里有人的轮（t1），会话身份已知（p1），
    // 所以 `real` 原样保留——加闸不是"一律不许 real"，是"不许没有依据地 real"。
    let reality: String = root
        .memory()
        .conn()
        .query_row("SELECT reality_status FROM anchor WHERE canonical='张三'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(reality, "real");

    let anchor_id: String = root
        .memory()
        .conn()
        .query_row("SELECT id FROM anchor WHERE canonical='张三'", [], |r| r.get(0))
        .unwrap();
    let row = owner_of(root.memory(), &anchor_id);
    assert_eq!(row.owner_principal_id.as_deref(), Some("p1"));
    assert_eq!(row.owner_agent_id, "alice");
    assert_eq!(row.created_by_agent, "agent-batch-A", "created_by 是会话 source_agent");
    assert_eq!(row.visibility, "project", "min(project, ceiling(project))");
    assert_eq!(row.retention, "keep");
    assert_eq!((row.scope_kind.as_str(), row.scope_key.as_str()), ("project", "proj-x"));
    assert_eq!(row.source_agent.as_deref(), Some("agent-batch-A"));
    assert_eq!(row.device_id.as_deref(), Some("dev-batch-A"));
    assert_eq!(row.created_at_ms, NOW);
    assert_eq!(
        count(root.memory(), "resource_owner"),
        1,
        "只为锚写归属，别名/提及不各写一行"
    );

    let want_run = format!("naming-{NOW}");
    for uid in &uids {
        let r = record(root.ledger(), uid);
        assert_eq!(r.state, PassState::Done, "{uid}");
        assert_eq!(r.attempt, 0);
        assert_eq!(r.run_id.as_deref(), Some(want_run.as_str()));
        let fp = r.fingerprint.expect("done 行必须带指纹");
        assert_eq!(fp.len(), 64, "sha256 十六进制");
        assert!(fp.bytes().all(|b| b.is_ascii_hexdigit()), "{fp}");
    }
}

/// 3. 坏 JSON：契约失败 ⇒ 每轮 `rejected`、理由非空；记忆库零行、零归属。
#[test]
fn malformed_json_rejects_every_turn_and_writes_nothing() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root);
    let backend = FakeBackend::new(Mode::BadJson);

    let err = run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap_err();
    assert!(matches!(err, DistillError::A1(_)), "{err}");
    assert_eq!(backend.calls.get(), 1, "第一跑就坏 ⇒ 不再烧第二跑");

    for uid in &uids {
        let r = record(root.ledger(), uid);
        assert_eq!(r.state, PassState::Rejected, "{uid}");
        assert!(r.reason.as_deref().is_some_and(|s| !s.is_empty()), "{uid}");
    }
    assert_no_memory_rows(root.memory());
}

/// 4. 单元级 LLM 故障（超时）：每轮 `pending`、attempt=1、退避 60s；记忆库零行。
///    整批按 `Ok(Pending)` 交回（W19c2：坏的是这一格，日路径不该在这里停族）。
#[test]
fn an_llm_failure_leaves_every_turn_pending_with_backoff() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root);
    let backend = FakeBackend::new(Mode::Timeout);

    let out = run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(out.state, PassState::Pending);
    assert_eq!(out.turns, uids.len(), "选中的轮数照实报");
    assert_eq!((out.anchors, out.mentions), (0, 0), "落库计数全 0");

    for uid in &uids {
        let r = record(root.ledger(), uid);
        assert_eq!(r.state, PassState::Pending, "{uid}");
        assert_eq!(r.attempt, 1, "{uid}");
        assert_eq!(r.next_retry_at_ms, Some(NOW + 60_000), "{uid}");
    }
    assert_no_memory_rows(root.memory());
}

/// 5. 重放：成功后选批返回 None；拿同一份 plan 再跑一次，行数与归属都不变（幂等）。
#[test]
fn a_replay_keeps_row_counts_and_the_owner_unchanged() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root);
    let backend = FakeBackend::new(Mode::Good);

    run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert!(
        select_naming_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
            .unwrap()
            .is_none(),
        "成功的轮已被 done 状态表态"
    );

    let tables = ["anchor", "alias", "mention", "provenance", "resource_owner"];
    let before: Vec<i64> = tables.iter().map(|t| count(root.memory(), t)).collect();
    let owner_before: (Option<String>, String, i64) = root
        .memory()
        .conn()
        .query_row(
            "SELECT owner_principal_id, owner_agent_id, created_at_ms FROM resource_owner
             WHERE mem_table='anchor'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();

    let again = run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW + 5_000).unwrap();
    assert_eq!(again.state, PassState::Done);

    let after: Vec<i64> = tables.iter().map(|t| count(root.memory(), t)).collect();
    assert_eq!(before, after, "重放不许长行");
    let owner_after: (Option<String>, String, i64) = root
        .memory()
        .conn()
        .query_row(
            "SELECT owner_principal_id, owner_agent_id, created_at_ms FROM resource_owner
             WHERE mem_table='anchor'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(owner_before, owner_after, "已有归属不被覆盖，入库时刻也不被刷新");
    for uid in &uids {
        assert_eq!(record(root.ledger(), uid).state, PassState::Done);
    }
}

/// 6. 合法空批：`empty` + 零行，不是失败也不是 `rejected`。
#[test]
fn an_empty_reply_is_recorded_as_empty_with_no_rows() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root);
    let backend = FakeBackend::new(Mode::Empty);

    let out = run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(out.state, PassState::Empty);
    assert_eq!((out.turns, out.anchors, out.mentions), (3, 0, 0));

    for uid in &uids {
        let r = record(root.ledger(), uid);
        assert_eq!(r.state, PassState::Empty, "{uid}");
        assert!(r.reason.is_none(), "{uid}");
    }
    assert_no_memory_rows(root.memory());
}

/// 选批的 ctx 与 owner 是两个来源：P 独占列取会话的 `source_agent`，归属取会话的
/// `owner_agent_id`（与 `tests/ownership.rs` 同一条判据）。
#[test]
fn the_selected_plan_carries_the_batch_context_separately_from_the_owner() {
    let (_dir, root, _uids) = seeded();
    let plan = plan_of(&root);
    assert_eq!(plan.ctx.source_agent, "agent-batch-A");
    assert_eq!(plan.ctx.device_id, "dev-batch-A");
    assert_eq!(plan.owner.owner_agent_id, "alice");
    assert_ne!(plan.ctx.source_agent, plan.owner.owner_agent_id);
}

/// 7. 重选判据：未到期的 pending 不重选；到期那一刻（`<=`）与 stale 重选；
///    done 始终不重选；`limit` 仍按会话内顺序截前 N。
#[test]
fn selection_re_picks_due_pending_and_stale_turns_only() {
    let (_dir, root, uids) = seeded();
    let l = root.ledger();
    // uids[0]：pending，退避到 NOW+60s（未到期）。
    pass_state::note_attempt(l, key_of(&uids[0]), 60_000, NOW).unwrap();
    // uids[1]：stale（旧指纹作废，与时间无关）。
    pass_state::mark_terminal(l, key_of(&uids[1]), PassState::Stale, None, NOW).unwrap();
    // uids[2]：done（终态，时间到了也不复活）。
    pass_state::mark_terminal(l, key_of(&uids[2]), PassState::Done, None, NOW).unwrap();

    assert_eq!(
        selected_uids(&root, 0, NOW),
        vec![uids[1].clone()],
        "退避期内的 pending 不重选，stale 选"
    );
    assert_eq!(
        selected_uids(&root, 0, NOW + 59_999),
        vec![uids[1].clone()],
        "差 1ms 仍未到期"
    );
    assert_eq!(
        selected_uids(&root, 0, NOW + 60_000),
        vec![uids[0].clone(), uids[1].clone()],
        "到期那一刻（<=）重选 pending；done 仍不选"
    );
    assert_eq!(
        selected_uids(&root, 1, NOW + 60_000),
        vec![uids[0].clone()],
        "limit 截的是会话内顺序的前 N"
    );
}

/// 8. running / rejected / quarantined / empty 都不重选（中间态与终态一视同仁，
///    且时间过去不会把它们复活）。
#[test]
fn selection_skips_running_rejected_quarantined_and_empty_turns() {
    let (_dir, root, uids) = seeded();
    let l = root.ledger();
    pass_state::mark_running(l, key_of(&uids[0]), "fp", "run", NOW).unwrap();
    pass_state::mark_terminal(l, key_of(&uids[1]), PassState::Rejected, Some("schema"), NOW)
        .unwrap();
    pass_state::mark_terminal(l, key_of(&uids[2]), PassState::Quarantined, Some("content"), NOW)
        .unwrap();
    assert_eq!(selected_uids(&root, 0, NOW), Vec::<String>::new());
    assert_eq!(selected_uids(&root, 0, NOW + 1_000_000), Vec::<String>::new());

    let (_dir2, root2, uids2) = seeded();
    for uid in &uids2 {
        pass_state::mark_terminal(root2.ledger(), key_of(uid), PassState::Empty, None, NOW)
            .unwrap();
    }
    assert_eq!(selected_uids(&root2, 0, NOW + 1_000_000), Vec::<String>::new(), "empty 是终态");
}

/// 9. `pending` 且 `next_retry_at_ms` 为空 ⇒ 可重选（"没有下一次"不是"永不重试"；
///    与 `pass_state::should_run` 的 `is_none_or` 同一条口径）。
#[test]
fn a_pending_row_without_a_due_time_is_selectable() {
    let (_dir, root, uids) = seeded();
    pass_state::note_attempt(root.ledger(), key_of(&uids[0]), 60_000, NOW).unwrap();
    for uid in &uids[1..] {
        pass_state::mark_terminal(root.ledger(), key_of(uid), PassState::Done, None, NOW).unwrap();
    }
    root.ledger()
        .conn()
        .execute(
            "UPDATE extract_pass_state SET next_retry_at_ms=NULL WHERE turn_uid=?1",
            [uids[0].as_str()],
        )
        .unwrap();
    assert_eq!(selected_uids(&root, 0, NOW), vec![uids[0].clone()]);
}

/// 10. 单元级 LLM 故障（超时）+ 退避到期 ⇒ 同一批被重新选中并跑完（不 sleep：钟是参数）。
#[test]
fn a_failed_batch_is_selected_again_after_the_backoff() {
    let (_dir, root, _uids) = seeded();
    let plan = plan_of(&root);
    let failing = FakeBackend::new(Mode::Timeout);
    let first = run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &failing, NOW).unwrap();
    assert_eq!(first.state, PassState::Pending, "超时是单元级失败：pending，不是 Err");

    assert_eq!(
        selected_uids(&root, 0, NOW + 59_999),
        Vec::<String>::new(),
        "退避期内不重选"
    );
    let again = select_naming_batch(
        root.transcript(), root.ledger(), root.memory(), 0, NOW + 60_000,
    )
    .unwrap()
    .expect("退避到期必须重新选中");
    assert_eq!(again.batch.rows.len(), 3);

    let good = FakeBackend::new(Mode::Good);
    let out = run_naming(root.memory(), root.transcript(), root.ledger(), &again, &good, NOW + 60_000).unwrap();
    assert_eq!(out.state, PassState::Done);
}

/// 10b. H5a：`pending(review_pending)` 不再被选批重选——A1 已在 `review_item` 里等
///      `review run`，再选一次只会重跑双抽、白烧调用。
///
///      规则只看 reason 这一格（不看时钟）：清成 NULL、退避置到过去 ⇒ 同一条轮又能被选中。
#[test]
fn a_review_pending_turn_is_not_reselected_until_the_reason_clears() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root);
    let backend = FakeBackend::new(Mode::Good);
    let out = run_naming_with(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &backend,
        NOW,
        ReviewMode::EnqueueOnly,
    )
    .unwrap();
    assert_eq!(out.state, PassState::Pending);
    assert_eq!(count(root.memory(), "anchor"), 0, "EnqueueOnly 不许写锚");

    // naming 的待审是**整批**：三轮回同一个 `pending(review_pending)`。
    for uid in &uids {
        let awaiting = record(root.ledger(), uid);
        assert_eq!(awaiting.state, PassState::Pending);
        assert_eq!(awaiting.reason.as_deref(), Some(REVIEW_PENDING_REASON));
        assert_eq!(awaiting.next_retry_at_ms, None, "等审核不是等退避");
    }
    assert_eq!(
        selected_uids(&root, 0, NOW),
        Vec::<String>::new(),
        "三轮回都待审：review_pending 不可重选"
    );
    assert_eq!(
        selected_uids(&root, 0, NOW + 3_600_000),
        Vec::<String>::new(),
        "时钟走多远都一样：这一格不看时间"
    );

    // 规则只看 reason：清成 NULL + 退避到期 ⇒ 只剩这一轮可选（其余两轮仍是待审）。
    root.ledger()
        .conn()
        .execute(
            "UPDATE extract_pass_state SET reason=NULL, next_retry_at_ms=?1
              WHERE turn_uid=?2 AND pass_id='naming' AND family=''",
            rusqlite::params![NOW - 1, uids[0]],
        )
        .unwrap();
    assert_eq!(
        selected_uids(&root, 0, NOW),
        vec![uids[0].clone()],
        "reason 清空后按退避重选：不可重选的唯一原因是 review_pending"
    );
}

// ── ⑬ 改判据后旧轮自动可重跑（P0-2）───────────────────────────────────────
//
// 指纹含整批 user 文本，批组成一变就全不等，不能当"改了 prompt"的信号。状态行另存一格
// 只由 system prompt 算的版本号：同版本 done 不回头；版本非空且不同 ⇒ 旧 done 轮可重选；
// NULL（旧库升级）一律不重选——不自动全量重跑。

/// 13a. 同一 system prompt ⇒ 版本号相同 ⇒ 已 done 的轮仍不可重选；写状态时版本号同步落库。
#[test]
fn the_same_prompt_version_keeps_done_turns_unselectable() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root);
    let backend = FakeBackend::new(Mode::Good);
    run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();

    let current = pass_state::prompt_version(&a1_system());
    for uid in &uids {
        let r = record(root.ledger(), uid);
        assert_eq!(r.state, PassState::Done, "{uid}");
        assert_eq!(r.prompt_version.as_deref(), Some(current.as_str()),
                   "{uid}：开跑必须同步写 system-only 版本号");
    }
    assert_eq!(
        selected_uids(&root, 0, NOW + 3_600_000),
        Vec::<String>::new(),
        "同一 system prompt 再跑一次 ⇒ done 仍不可选"
    );
}

/// 13b. system prompt 换了（版本号不同）⇒ 旧 `done` 轮可重选。
#[test]
fn a_changed_prompt_version_makes_done_turns_selectable_again() {
    let (_dir, root, uids) = seeded();
    let plan = plan_of(&root);
    let backend = FakeBackend::new(Mode::Good);
    run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(selected_uids(&root, 0, NOW + 1), Vec::<String>::new(), "先确认 done 不可选");

    // 模拟"改了一句 system prompt 之后重跑"：旧行带的是上一版版本号。
    let next = pass_state::prompt_version("下一版 system prompt（判据改了一句）");
    root.ledger()
        .conn()
        .execute(
            "UPDATE extract_pass_state SET prompt_version=?1
              WHERE pass_id='naming' AND family=''",
            [next.as_str()],
        )
        .unwrap();
    assert_eq!(
        selected_uids(&root, 0, NOW + 1),
        uids,
        "换了判据 ⇒ 旧 done 轮必须可重选"
    );
}

/// 13c. `prompt_version` 为 NULL 的老行（升级前落的 done）⇒ **不可**重选。
#[test]
fn a_null_prompt_version_is_never_reselected() {
    let (_dir, root, uids) = seeded();
    for uid in &uids {
        // 终态直落（没有先 mark_running）：这正是旧行的形状，版本号天然是 NULL。
        pass_state::mark_terminal(root.ledger(), key_of(uid), PassState::Done, None, NOW)
            .unwrap();
        assert_eq!(record(root.ledger(), uid).prompt_version, None, "{uid}");
    }
    assert_eq!(
        selected_uids(&root, 0, NOW + 3_600_000),
        Vec::<String>::new(),
        "NULL 视为不可重选：升级后不自动全量重跑"
    );
}

// ── ⑪ 噪声锚的后置闸（S2e-5）───────────────────────────────────────────
//
// 真实冒烟（`design/2026-10-26-dogfood-a-smoke.md` 发现 2）里，模型把 commit 哈希片段、
// `HEAD`、反引号代码令牌建成了锚。判据只用通用模式（hex/路径/URL/反引号/版本指针/纯符号），
// 不认任何私有语料里的名字。

fn anchor(reference: &str, canonical: &str, aliases: &[&str]) -> A1Anchor {
    A1Anchor {
        reference: reference.into(),
        kind: "object".into(),
        canonical: canonical.into(),
        aliases: aliases.iter().map(|a| a.to_string()).collect(),
        reality_status: "unknown".into(),
        evidence: Vec::new(),
        stable: true,
    }
}

fn mention(anchor_ref: &str, surface: &str, speaker_ref: Option<&str>) -> A1Mention {
    A1Mention {
        anchor_ref: anchor_ref.into(),
        surface: surface.into(),
        turn_uid: "t1".into(),
        speaker_ref: speaker_ref.map(str::to_string),
        discourse_role: "mentioned".into(),
        resolution: "resolved".into(),
        confidence: 0.9,
    }
}

fn issue(kind: &str, refs: &[&str], reason: &str) -> A1Issue {
    A1Issue {
        kind: kind.into(),
        refs: refs.iter().map(|r| r.to_string()).collect(),
        reason: reason.into(),
    }
}

/// `is_noise_name` 的表驱动：右列是"丢"。判据是通用形状，不是这张表里的某几个词。
#[test]
fn is_noise_name_matches_generic_shapes_only() {
    for name in [
        "",
        "   ",
        "\r\n",
        "`99c05800…`",
        "`阶段 4`",
        "99c05800…",
        "99c05800...",
        "99C05800",
        "HEAD",
        "head",
        "ORIGIN",
        "origin",
        "/var/tmp/log.txt",
        "\\share\\dir",
        "D:\\recollect\\rust",
        "C:/tmp/x",
        "https://example.com/a",
        "——",
        "302",
        "3.14",
    ] {
        assert!(is_noise_name(name), "该丢的没丢：{name:?}");
    }
    for name in [
        "张三",
        "老张",
        "某打印机",
        "批次 M",
        "阶段 4",
        "惠普 M126a",
        "99c0",
        "HEAD 版本",
    ] {
        assert!(!is_noise_name(name), "该留的被丢了：{name:?}");
    }
}

/// 过滤闸的动作面：canonical 噪声 ⇒ 丢锚并计数；别名噪声 ⇒ 只剔别名；被丢锚的提及、
/// 只引用被丢锚的 issue 一起走；还有别的 ref 的 issue 留剩下的；unresolved 原样。
#[test]
fn drop_noise_anchors_removes_the_anchor_and_what_can_only_refer_to_it() {
    let mut batch = A1Batch {
        anchors: vec![
            anchor("a1", "张三", &["老张", "HEAD", "`99c05800…`", "D:\\recollect\\rust"]),
            anchor("a2", "99c05800…", &[]),
            anchor("a3", "HEAD", &[]),
            anchor("a4", "`阶段 4`", &[]),
            anchor("a5", "https://example.com/x", &[]),
            anchor("a6", "某打印机", &[]),
            anchor("a7", "批次 M", &[]),
            anchor("a8", "阶段 4", &[]),
            anchor("a9", "——", &[]),
            anchor("a10", "302", &[]),
        ],
        mentions: vec![
            mention("a1", "老张", None),
            mention("a2", "99c05800…", None),
            mention("a6", "某打印机", Some("a3")),
        ],
        issues: vec![
            issue("ambiguous_anchor", &["a2"], "两个同名令牌"),
            issue("ambiguous_anchor", &["a2", "a1"], "令牌与真名混在一起"),
            issue("missing_subject", &["a1"], "缺主语"),
            issue("other", &[], "没有 ref 的记账"),
        ],
        unresolved: vec![A1Unresolved {
            surface: "99c05800…".into(),
            turn_uid: "t1".into(),
            reason: "没有可指认的东西".into(),
        }],
    };

    assert_eq!(drop_noise_anchors(&mut batch), 6, "a2/a3/a4/a5/a9/a10 该丢");

    let kept: Vec<&str> = batch.anchors.iter().map(|a| a.canonical.as_str()).collect();
    assert_eq!(kept, vec!["张三", "某打印机", "批次 M", "阶段 4"]);
    assert_eq!(batch.anchors[0].aliases, vec!["老张"], "别名里的噪声逐条剔除，锚保留");

    // 提及：a2 的那条没了；a6 的那条留下，但被丢的 `HEAD` 不能继续当转述者。
    assert_eq!(batch.mentions.len(), 2);
    assert_eq!(batch.mentions[0].anchor_ref, "a1");
    assert_eq!(batch.mentions[1].anchor_ref, "a6");
    assert_eq!(batch.mentions[1].speaker_ref, None);

    // issue：只引用 a2 的丢了；a2+a1 的只留 a1；与噪声无关的两条原样。
    assert_eq!(batch.issues.len(), 3);
    assert_eq!(batch.issues[0].refs, vec!["a1"]);
    assert_eq!(batch.issues[1].refs, vec!["a1"]);
    assert_eq!(batch.issues[2].refs, Vec::<String>::new());

    // unresolved 没有 anchor_ref，不是这个闸的活。
    assert_eq!(batch.unresolved.len(), 1);
}

/// 一支干净批次过闸 = 一个锚都不丢（过滤不能把整批判死，也不能顺手改别名）。
#[test]
fn a_clean_batch_passes_the_noise_gate_untouched() {
    let mut batch = A1Batch {
        anchors: vec![anchor("a1", "张三", &["老张"]), anchor("a2", "某打印机", &[])],
        mentions: vec![mention("a1", "老张", Some("a2"))],
        issues: vec![issue("missing_subject", &["a1"], "缺主语")],
        unresolved: Vec::new(),
    };
    let before = batch.anchors.len();
    assert_eq!(drop_noise_anchors(&mut batch), 0);
    assert_eq!(batch.anchors.len(), before);
    assert_eq!(batch.anchors[0].aliases, vec!["老张"]);
    assert_eq!(batch.mentions[0].speaker_ref.as_deref(), Some("a2"));
    assert_eq!(batch.issues[0].refs, vec!["a1"]);
}

/// 跑一批：模型照收，过滤在后；读数 `filtered` 可读，库里的锚/提及只剩干净的那几条。
#[test]
fn run_naming_drops_noise_anchors_and_reports_the_count() {
    let (_dir, root, _uids) = seeded();
    let plan = plan_of(&root);
    let backend = FakeBackend::new(Mode::Noise);

    let out = run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(out.filtered, 1, "双跑同一个令牌 ⇒ 合并后只丢一个锚");
    assert_eq!(out.turns, 3);
    assert_eq!(out.anchors, 1, "只剩张三");
    assert_eq!(out.mentions, 1, "令牌那条提及跟着锚一起走");
    assert_eq!(out.state, PassState::Done);

    assert_eq!(count(root.memory(), "anchor"), 1);
    assert_eq!(count(root.memory(), "mention"), 1);
    let canonical: String = root
        .memory()
        .conn()
        .query_row("SELECT canonical FROM anchor", [], |r| r.get(0))
        .unwrap();
    assert_eq!(canonical, "张三");
}

/// W14e 的端到端：同一批里一臂是形状类（提交哈希）、一臂是真名。
/// 落库之后两个桶各 1 行：形状那行进诊断桶并带 `shape=hex`，真名那行留真名桶并带 `cand=0`。
#[test]
fn run_naming_splits_shape_surfaces_from_real_names_even_end_to_end() {
    let (_dir, root, _uids) = seeded();
    let plan = plan_of(&root);
    let backend = FakeBackend::new(Mode::Unresolved);

    let out = run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(out.state, PassState::Done, "两臂都不拦批：{out:?}");
    assert_eq!(count(root.memory(), "anchor"), 1, "锚照旧只有张三");

    let details = |code: &str| -> Vec<String> {
        root.memory()
            .conn()
            .prepare("SELECT detail FROM extraction_issue WHERE code=?1 ORDER BY detail")
            .unwrap()
            .query_map([code], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let shape_rows = details(rc_memory::issues::UNRESOLVED_SURFACE_SHAPE_CODE);
    assert_eq!(shape_rows.len(), 1, "形状桶只收哈希那一行：{shape_rows:?}");
    assert!(shape_rows[0].contains("surface=14d6e8ce;"), "{shape_rows:?}");
    assert!(shape_rows[0].ends_with("shape=hex"), "{shape_rows:?}");

    let name_rows = details(rc_memory::issues::UNRESOLVED_SURFACE_CODE);
    assert_eq!(name_rows.len(), 1, "真名桶只收那台打印机：{name_rows:?}");
    assert!(name_rows[0].contains("surface=那台打印机;"), "{name_rows:?}");
    assert!(name_rows[0].ends_with("cand=0"), "{name_rows:?}");
}

/// prompt 的反例与 shape 计数：尾段多一句，METHOD/STANDARDS/POSITIVES/NEGATIVES 一格不动。
#[test]
fn the_a1_tail_names_the_noise_shapes_without_growing_the_shape() {
    let s = a1_system();
    assert!(
        s.contains(
            "A commit hash, a branch name such as HEAD, a file path, a URL, a code token \
             in backticks, a line range such as 第 40–52 行, or a sentence that only says \
             what to do next is not an entity; never emit it as an anchor."
        ),
        "A1 尾段缺那句反例"
    );
    let shape = a1_shape();
    assert_eq!(
        (shape.method_steps, shape.standards, shape.positives, shape.negatives),
        (6, 15, 4, 12),
        "§10.65：kind 判据进 STANDARDS（13→14）；W14d 的账号名/转述两条各进一格（14→15、10→11）；\
         W14e 的判空教学再进一格反例（11→12）"
    );
}


/// S2e-6 压缩块隔离：选批认出摘要轮、当场落 `rejected(compact_summary)` 并跳过；
/// 它不进 batch、不进下一次选批，普通轮照常。
#[test]
fn a_compaction_turn_is_rejected_before_it_enters_the_batch() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(
        &mut root,
        &[
            "张三明天把报价单发给你",
            "This session is being continued from a previous conversation that ran out of context.",
            "李四负责报价单",
        ],
        1_000,
    );

    let plan = select_naming_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("t1/t3 可选");
    let got: Vec<String> = plan.batch.rows.iter().map(|r| r.turn_uid.clone()).collect();
    assert_eq!(got, vec![uids[0].clone(), uids[2].clone()], "压缩摘要轮不进批");

    let r = record(root.ledger(), &uids[1]);
    assert_eq!(r.state, PassState::Rejected);
    assert_eq!(r.reason.as_deref(), Some("compact_summary"));

    // 第二次选批：摘要在场（rejected，不再重扫），普通轮仍在。
    assert_eq!(selected_uids(&root, 0, NOW + 1), vec![uids[0].clone(), uids[2].clone()]);
    assert!(
        selected_uids(&root, 0, NOW + 2).iter().all(|uid| uid != &uids[1]),
        "同一轮不被重复扫"
    );
}

/// §10.66 C：应用插入的运行期通知不是一次 utterance —— 选批认出它、当场落
/// `rejected(not_utterance)` 并跳过；它不进 batch、普通轮照常。
#[test]
fn a_runtime_notice_is_rejected_before_it_enters_the_batch() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(
        &mut root,
        &[
            "张三明天把报价单发给你",
            "Model response interrupted. Completed output is preserved; automatic continuation stopped.",
            "李四负责报价单",
        ],
        1_000,
    );

    let plan = select_naming_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("t1/t3 可选");
    let got: Vec<String> = plan.batch.rows.iter().map(|r| r.turn_uid.clone()).collect();
    assert_eq!(got, vec![uids[0].clone(), uids[2].clone()], "运行期通知不进批");

    let r = record(root.ledger(), &uids[1]);
    assert_eq!(r.state, PassState::Rejected);
    assert_eq!(r.reason.as_deref(), Some("not_utterance"));
}

// ── ⑫ 逐字收紧（C4b）──────────────────────────────────────────────────────//
// 9c 的判据从"该批任一轮"收到"**该锚自己的**证据轮"；新增非 speaker 提及的逐轮核对
// （speaker 自称不是文本 span）。中心实测：前者只多杀 1 个锚（0.55%）；后者照字面做会
// 杀掉 24 条 speaker 自称，那是误杀，所以 speaker 一律放过。

/// C4b 的两轮夹具：正文由每条用例给，会话归属与 [`seeded`] 同一行（两轮都不表态）。
fn seeded_pair(bodies: &[&str]) -> (tempfile::TempDir, Root, Vec<String>) {
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
    (dir, root, uids)
}

/// 回放后端：第 1、2 次是 A1 双跑（同一份），第 3 次是 naming 窄审（`a1` pass）。
/// 脚本耗尽 ⇒ `Timeout`（被闸拒的批走不到第三次）。
struct ScriptedBackend {
    replies: RefCell<VecDeque<String>>,
    calls: Cell<usize>,
}

impl ScriptedBackend {
    fn a1_then_review(a1: &str) -> Self {
        ScriptedBackend {
            replies: RefCell::new(
                vec![a1.to_string(), a1.to_string(), naming_review_pass_a1()].into(),
            ),
            calls: Cell::new(0),
        }
    }
}

impl LlmBackend for ScriptedBackend {
    fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
        self.calls.set(self.calls.get() + 1);
        self.replies.borrow_mut().pop_front().ok_or(LlmFailure::Timeout)
    }
    fn slug(&self) -> Option<&str> {
        Some("fake-c4b-model")
    }
}

/// 一条 A1 回复：一个锚（证据轮由调用方给，只有一轮）+ 可选的一条提及。
fn a1_with(
    evidence: &str,
    canonical: &str,
    aliases: &[&str],
    mention: Option<(&str, &str, &str)>,
) -> String {
    let mentions: Vec<Value> = mention
        .into_iter()
        .map(|(surface, turn, role)| {
            json!({
                "anchor_ref": "a1",
                "surface": surface,
                "turn_uid": turn,
                "speaker_ref": null,
                "discourse_role": role,
                "resolution": "resolved",
                "confidence": 0.9
            })
        })
        .collect();
    json!({
        "schema_version": 4, "pass": "a1",
        "anchors": [{
            "ref": "a1", "kind": "person", "canonical": canonical, "aliases": aliases,
            "reality_status": "real", "evidence": [evidence], "reason": "一轮点名"
        }],
        "mentions": mentions,
        "issues": [], "unresolved_surfaces": []
    })
    .to_string()
}

/// `review_item.program_code`（最近一条 item）：闸拒的码就落在这格。
fn program_code(m: &MemoryHandle) -> Option<String> {
    m.conn()
        .query_row(
            "SELECT program_code FROM review_item ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

/// 1. canonical 必须落在**该锚自己的证据轮**：证据只指 t1，而「张三」只在 t2 ⇒
///    `not_verbatim`；把同一句补进 t1 ⇒ 通过。
#[test]
fn canonical_must_appear_in_its_own_evidence_turn() {
    let (_dir, root, _uids) =
        seeded_pair(&["今天的会议记录在下面", "张三说明天把报价单发给你"]);
    let plan = plan_of(&root);
    let backend = ScriptedBackend::a1_then_review(&a1_with("t1", "张三", &[], None));

    let out =
        run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(out.state, PassState::Rejected, "只在别轮出现的名字不是这条锚的证据");
    assert_eq!(out.anchors, 0);
    assert_eq!(program_code(root.memory()).as_deref(), Some("not_verbatim"));
    assert_eq!(count(root.memory(), "anchor"), 0, "闸拒的批不写锚");
    assert_eq!(backend.calls.get(), 2, "闸拒之后不问窄审");

    // 把名字补进证据轮 t1 ⇒ 同一条锚通过。
    let (_dir2, root2, _uids2) =
        seeded_pair(&["张三说明天把报价单发给你", "张三说报价单后天到"]);
    let plan2 = plan_of(&root2);
    let backend2 = ScriptedBackend::a1_then_review(&a1_with("t1", "张三", &[], None));
    let out2 =
        run_naming(root2.memory(), root2.transcript(), root2.ledger(), &plan2, &backend2, NOW)
            .unwrap();
    assert_eq!(out2.state, PassState::Done);
    assert_eq!(out2.anchors, 1);
    assert_eq!(count(root2.memory(), "anchor"), 1);
}

/// 2. alias 同样要落在该锚自己的证据轮：canonical 在 t1，alias「老张」只在 t2 ⇒
///    `not_verbatim`；把「老张」补进 t1 ⇒ 通过。
#[test]
fn alias_must_appear_in_its_own_evidence_turn() {
    let (_dir, root, _uids) = seeded_pair(&["张三说明天把报价单发给你", "老张说报价单后天到"]);
    let plan = plan_of(&root);
    // mention 的 surface 是证据轮里的「张三」（它自己的 canonical），不是别名。
    let reply = a1_with("t1", "张三", &["老张"], Some(("张三", "t1", "mentioned")));
    let backend = ScriptedBackend::a1_then_review(&reply);

    let out =
        run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(out.state, PassState::Rejected, "别名只在别的轮里，不算这条锚的证据");
    assert_eq!(program_code(root.memory()).as_deref(), Some("not_verbatim"));
    assert_eq!(count(root.memory(), "alias"), 0);

    let (_dir2, root2, _uids2) = seeded_pair(&["张三说老张明天到", "老张说报价单后天到"]);
    let plan2 = plan_of(&root2);
    let backend2 = ScriptedBackend::a1_then_review(&reply);
    let out2 =
        run_naming(root2.memory(), root2.transcript(), root2.ledger(), &plan2, &backend2, NOW)
            .unwrap();
    assert_eq!(out2.state, PassState::Done);
    assert_eq!(count(root2.memory(), "alias"), 1);
}

/// 3. 一条 `mentioned` 提及的 surface 不在它自己那条轮里 ⇒ 只丢这条提及：锚照常落库，
///    `ungrounded_mentions == 1`，批仍是 `done`（过滤不参与成败）。
#[test]
fn a_mention_whose_surface_is_not_in_its_turn_is_dropped_but_keeps_the_anchor() {
    let (_dir, root, _uids) = seeded_pair(&["张三明天到", "报价单已经发出"]);
    let plan = plan_of(&root);
    // 「老王」挂在 t2，而 t2 的正文里没有它（跨轮错挂）。
    let reply = a1_with("t1", "张三", &[], Some(("老王", "t2", "mentioned")));
    let backend = ScriptedBackend::a1_then_review(&reply);

    let out =
        run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(out.state, PassState::Done, "丢提及不是失败");
    assert_eq!(out.ungrounded_mentions, 1);
    assert_eq!(out.anchors, 1, "只丢提及，不杀锚");
    assert_eq!(out.mentions, 0);
    assert_eq!(count(root.memory(), "anchor"), 1);
    assert_eq!(count(root.memory(), "mention"), 0, "不接地的提及不落库");
}

/// 4. `discourse_role="speaker"` 的自称豁免逐轮核对：`agent-smoke` 在全批任何一轮都不出现，
///    但它不是文本 span ⇒ 提及保留、`ungrounded_mentions == 0`、锚保留。
#[test]
fn speaker_mentions_are_exempt_from_the_turn_check() {
    let (_dir, root, _uids) = seeded_pair(&["张三今天上线了", "好的，收到"]);
    let plan = plan_of(&root);
    let reply = a1_with("t1", "张三", &[], Some(("agent-smoke", "t1", "speaker")));
    let backend = ScriptedBackend::a1_then_review(&reply);

    let out =
        run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();
    assert_eq!(out.state, PassState::Done);
    assert_eq!(out.ungrounded_mentions, 0, "自称不参与逐字核对");
    assert_eq!(out.anchors, 1);
    assert_eq!(out.mentions, 1, "speaker 提及必须留下");
    let surface: String = root
        .memory()
        .conn()
        .query_row("SELECT surface FROM mention", [], |r| r.get(0))
        .unwrap();
    assert_eq!(surface, "agent-smoke");
}

// ── ⑬ P0-2：账号昵称不许升成 person ─────────────────────────────────────────
//
// 现网那条误捕锚的形状是：agent 轮转述一段登录态（**昵称 + UID**），命名 pass 建了一个
// `kind='person'`、`reality_status='real'` 的锚，唯一证据是一条 `mentioned` 提及。
// 三格判据（9f 在闸里；转述与身份未知两格在写库层）各自钉一条：
// - 9f：昵称与数字标识同行相邻 / 落在反引号或 `key: value` 里 ⇒ 服务账号，不是人（仍拒收）；
// - 写库层：证据轮全是 agent 轮 ⇒ 锚照写、现实性落未知、留一行审计（W14d 用户裁定：
//   转述"有来源、有去处"，只是这个人真不真我们不知道）；
// - 写库层：身份未知 ⇒ `real` 降 `unknown`（锚照写，不拦）。
//
// 占位名与占位 UID：这一节不引任何现网真名/真 UID（判据只看形状，不看具体串）。

/// 昵称形状的 A1 回复：一个 person 锚 + 一条 `mentioned` 提及，模型自报 `real`。
fn nickname_reply(canonical: &str, evidence: &str, surface_turn: &str) -> String {
    json!({
        "schema_version": 4, "pass": "a1",
        "anchors": [{
            "ref": "a1", "kind": "person", "canonical": canonical, "aliases": [],
            "reality_status": "real", "evidence": [evidence], "reason": "原文点名"
        }],
        "mentions": [{
            "anchor_ref": "a1", "surface": canonical, "turn_uid": surface_turn,
            "speaker_ref": null, "discourse_role": "mentioned",
            "resolution": "resolved", "confidence": 0.95
        }],
        "issues": [], "unresolved_surfaces": []
    })
    .to_string()
}

/// 会话归属清空（身份未知）：写库层的 `real → unknown` 只看这一格。
fn seeded_unknown_principal(bodies: &[&str]) -> (tempfile::TempDir, Root, Vec<String>) {
    let (dir, mut root, uids) = seeded_pair(bodies);
    root.transcript_mut()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id=NULL
              WHERE session_key='sess-batch-A' AND device_id='dev-batch-A'",
            [],
        )
        .unwrap();
    (dir, root, uids)
}

/// 负向臂（P2-1）：agent 轮转述登录态（占位昵称 + 占位 UID）+ 1 条 `mentioned` + 自报 `real`
/// ⇒ 仍拒收，但拒收理由变成**形状**（9e 撤走后由 9f 接住同一份载荷）。
/// 旧用例里同形状（person + real + 1 条 mentioned）被钉成"落库且 active"。
#[test]
fn an_agent_transcribed_nickname_is_not_a_person() {
    let (_dir, root, _uids) = seeded_pair(&[
        "先看一眼登录态",
        "命令行工具已确认登录：测试昵称甲，UID 10000001",
    ]);
    let plan = plan_of(&root);
    let backend = ScriptedBackend::a1_then_review(&nickname_reply("测试昵称甲", "t2", "t2"));

    let out =
        run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();

    assert_eq!(out.state, PassState::Rejected, "账号形状不是人，9f 仍拒收");
    assert_eq!(out.anchors, 0);
    assert_eq!(
        program_code(root.memory()).as_deref(),
        Some("service_account_shape"),
        "9e 撤走后，昵称+UID 形状由 9f 接住"
    );
    assert_eq!(count(root.memory(), "anchor"), 0, "person 一行都不许落");
    assert_eq!(count(root.memory(), "mention"), 0);
}

/// 9f 臂：同样的昵称形状，这次是**人**说的（证据轮是 human）⇒ 只有形状判据能拦。
/// 用户口径：账号昵称属于环境/设备，不因为"是本人说的"就变成人。
#[test]
fn a_nickname_with_a_uid_is_not_a_person_even_when_a_human_says_it() {
    let (_dir, root, _uids) = seeded_pair(&[
        "登录信息：测试昵称甲，UID 10000001",
        "好的，记下了",
    ]);
    let plan = plan_of(&root);
    let backend = ScriptedBackend::a1_then_review(&nickname_reply("测试昵称甲", "t1", "t1"));

    let out =
        run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();

    assert_eq!(out.state, PassState::Rejected);
    assert_eq!(
        program_code(root.memory()).as_deref(),
        Some("service_account_shape"),
        "人说的也一样：形状判据不看说话人"
    );
    assert_eq!(count(root.memory(), "anchor"), 0);
}

/// 会话自锚：生产链路上由 ingest 的 `record_turn_speakers` 建（naming 自己不造锚），
/// 夹具照它的形状补一格，好让 agent 轮提及的说话人落在"就是这个 agent"上。
fn seed_session_agent_anchor(root: &Root) {
    let ctx = BatchCtx { source_agent: "agent-batch-A".into(), device_id: "dev-batch-A".into() };
    let owner = OwnerCtx {
        owner_principal_id: Some("p1".into()),
        owner_agent_id: "alice".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    };
    let scope = Scope { kind: ScopeKind::Project, key: "proj-x".into() };
    ensure_agent_anchor(root.memory(), &ctx, &owner, &scope, NOW).unwrap();
}

/// W14d 的新硬判据（用户裁定）：agent 轮转述一个普通人名，锚必须落库。
/// 说话人仍是那个 agent、提及与出处保留；只有"这个人真实存在"降成未知。
#[test]
fn an_agent_transcribed_person_is_written_with_unknown_reality() {
    let (_dir, root, _uids) = seeded_pair(&[
        "今天的会议记录在下面",
        "赵启明说明天把报价单交给我",
    ]);
    seed_session_agent_anchor(&root);
    let plan = plan_of(&root);
    // t2 是 agent 轮：模型自报 `real`，但证据里没有一句人自己开口。
    let reply = a1_with("t2", "赵启明", &[], Some(("赵启明", "t2", "mentioned")));
    let backend = ScriptedBackend::a1_then_review(&reply);

    let out =
        run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();

    assert_eq!(out.state, PassState::Done, "转述不是拒收：有来源、有去处");
    assert_eq!((out.anchors, out.mentions), (1, 1), "锚与提及都要落");
    let (kind, reality): (String, String) = root
        .memory()
        .conn()
        .query_row(
            "SELECT kind, reality_status FROM anchor WHERE canonical='赵启明'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((kind.as_str(), reality.as_str()), ("person", "unknown"), "锚照写，现实性未知");
    let agent_id = rc_memory::compute_mem_id(
        rc_memory::MemTable::Anchor,
        "agent",
        "agent-batch-A",
        "",
        &Scope { kind: ScopeKind::Project, key: "proj-x".into() },
    );
    let (role, speaker): (String, Option<String>) = root
        .memory()
        .conn()
        .query_row(
            "SELECT discourse_role, speaker_anchor_id FROM mention WHERE surface='赵启明'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(role, "mentioned", "提及照留");
    assert_eq!(speaker.as_deref(), Some(agent_id.as_str()), "说话人还是转述的那个 agent");
    let detail: String = root
        .memory()
        .conn()
        .query_row(
            "SELECT detail FROM extraction_issue WHERE code='person_agent_reported'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(detail.contains("ref=a1"), "审计行要能指回批内短引用：{detail}");
    assert!(detail.contains("from=real"), "from 记降级前的模型自报值：{detail}");
}

/// 双跑合并：一跑自报 `real`、一跑 `unknown`，证据并集里没有任何 human 轮。
/// 合并的"未知让位"不许把降级掀回来：落库仍是未知，审计只记一次。
#[test]
fn a_dual_run_merge_does_not_resurrect_real_for_an_agent_reported_person() {
    let (_dir, root, _uids) = seeded_pair(&[
        "今天的会议记录在下面",
        "赵启明说明天把报价单交给我",
    ]);
    let plan = plan_of(&root);
    let real_run = a1_with("t2", "赵启明", &[], None);
    let unknown_run = json!({
        "schema_version": 4, "pass": "a1",
        "anchors": [{
            "ref": "a2", "kind": "person", "canonical": "赵启明", "aliases": [],
            "reality_status": "unknown", "evidence": ["t2"], "reason": "只有 agent 提到"
        }],
        "mentions": [], "issues": [], "unresolved_surfaces": []
    })
    .to_string();
    let backend = ScriptedBackend {
        replies: RefCell::new(vec![real_run, unknown_run, naming_review_pass_a1()].into()),
        calls: Cell::new(0),
    };

    let out =
        run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();

    assert_eq!(out.state, PassState::Done, "只有 agent 证据也要放行");
    assert_eq!(out.anchors, 1, "两跑说的是同一个锚");
    let reality: String = root
        .memory()
        .conn()
        .query_row("SELECT reality_status FROM anchor WHERE canonical='赵启明'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(reality, "unknown", "合并后仍是未知：让位规则不许掀回 real");
    let n: i64 = root
        .memory()
        .conn()
        .query_row(
            "SELECT count(*) FROM extraction_issue WHERE code='person_agent_reported'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "一个锚一条审计行，双跑不重复计数");
}

/// 正向臂：人在 human 轮里说出的名字 + 会话身份已知 ⇒ 照常落库、`real` 不动。
/// 加闸不许把"有人开口提到的人"一起杀掉。
#[test]
fn a_name_said_by_a_human_still_becomes_a_person() {
    let (_dir, root, _uids) = seeded_pair(&["测试人名丙说明天把报价单发给你", "收到"]);
    let plan = plan_of(&root);
    let backend = ScriptedBackend::a1_then_review(&nickname_reply("测试人名丙", "t1", "t1"));

    let out =
        run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();

    assert_eq!(out.state, PassState::Done, "人的轮 + 身份已知 ⇒ 放行");
    assert_eq!(out.anchors, 1);
    let (kind, reality): (String, String) = root
        .memory()
        .conn()
        .query_row("SELECT kind, reality_status FROM anchor", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!((kind.as_str(), reality.as_str()), ("person", "real"));
    assert_eq!(count(root.memory(), "extraction_issue"), 0, "没有降级就没有那条账");
}

/// 身份未知 ⇒ `real` 降 `unknown`，锚照写（不拦），并留一条确定性读数。
#[test]
fn a_person_named_by_a_human_is_written_unknown_when_the_principal_is_unknown() {
    let (_dir, root, _uids) = seeded_unknown_principal(&["测试人名丙说明天把报价单发给你", "收到"]);
    let plan = plan_of(&root);
    let backend = ScriptedBackend::a1_then_review(&nickname_reply("测试人名丙", "t1", "t1"));

    let out =
        run_naming(root.memory(), root.transcript(), root.ledger(), &plan, &backend, NOW).unwrap();

    assert_eq!(out.state, PassState::Done, "降级不是拒收");
    assert_eq!(out.anchors, 1);
    let (kind, reality): (String, String) = root
        .memory()
        .conn()
        .query_row("SELECT kind, reality_status FROM anchor", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!((kind.as_str(), reality.as_str()), ("person", "unknown"));
    let detail: String = root
        .memory()
        .conn()
        .query_row(
            "SELECT detail FROM extraction_issue WHERE code='person_reality_downgraded'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(detail.contains("ref=a1"), "降级要能指回批内短引用：{detail}");
}
