//! P2：`state_assertion` 的写库层（方案 `design/2026-10-31-state-pass-p2.md` §3–§4）。
//!
//! 这一层钉六件事：
//! 1. 一条合法 item 同时落**四处行**：`state_assertion`、`provenance`、`memory_mention`
//!    与 `resource_owner`，且逐列关键值正确、模型面写不出的列（id/valid_to/superseded_by/
//!    subject_kind/confidence）全是程序给的形状；
//! 2. **不产卡**：状态不写 `card`（消费面读 `state_assertion` 本表）；
//! 3. 重放计数全 0、库里行数不变（第一道幂等：内容寻址 id）；
//! 4. **不同轮说了同一个 `(主体, 维度, 值)`** ⇒ 不写第二行、计 `duplicate`
//!    （第二道幂等：`find_live_state`；少了这一道，同一句话被说十次就有十行"当前状态"）；
//! 5. 同维度**异值** ⇒ 写第二行并把旧值填进 `value_before`，再按 D10 的两条确定性规则收尾：
//!    新断言更晚就在同一事务里关掉旧行并指认替代者（`superseded`），新断言更早（迟到的旧话）
//!    就给自己带上 `valid_to_ms` = 旧行起点（`backfilled`），旧行仍是当前值
//!    （`design/2026-11-01-state-conflict-adjudication.md` §3）；
//! 6. 非法 dimension/mode/空值/空证据 ⇒ 整批 `Err` 且零行；只读句柄 ⇒ `Err`。
//!
//! 正反例只用通用占位名，不含任何语料专名或 holdout。
use rc_memory::identity::compute_state_id;
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_state::{
    live_conflict, put_state_rows, StateWriteItem, STATE_DIMENSIONS,
};
use rc_memory::{compute_mem_id, AnchorRole, BatchCtx, MemError, MemTable, Scope, ScopeKind};
use rc_store::{MemoryHandle, Root};

const NOW: i64 = 1_700_000_999_000;
const SRC_MS: i64 = 1_700_000_000_000;
const SUBJECT: &str = "张三";

fn uid(index: usize) -> String {
    format!("{:064x}", 0x21 + index)
}

/// 1 号锚（= 主体锚）的句柄按生产算法算：kind/canonical/scope 就是 `seeded()` 交给
/// `seed_anchor` 铺下的那一行的三格（`person` / [`SUBJECT`] / `project/proj-state` = [`scope()`]）。
/// 2 号锚只做"换主体 ⇒ 换身份"的差分输入、从不落库 ⇒ 无行上字段可依：
/// opaque handle: not an identity（不是身份，只是"另一个主体"的占位）。
fn anchor(index: usize) -> String {
    match index {
        1 => compute_mem_id(MemTable::Anchor, "person", SUBJECT, "", &scope()),
        n => format!("{:064x}", 0xb0 + n),
    }
}

fn ctx() -> BatchCtx {
    BatchCtx { source_agent: "agent-state-w".into(), device_id: "dev-state-w".into() }
}

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-state".into() }
}

fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-state".into()),
        owner_agent_id: "agent-owner".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}

/// 一条合法的直陈：主体锚 1、维度 emotion、值逐字取自引文。
fn item() -> StateWriteItem {
    StateWriteItem {
        subject_anchor_id: anchor(1),
        dimension: "emotion".into(),
        value_after: "焦虑得很".into(),
        mode: "expressed".into(),
        text: format!("{SUBJECT}：情绪为「焦虑得很」。"),
        evidence: vec![(uid(1), AnchorRole::Human)],
        scope: scope(),
        source_ms: SRC_MS,
    }
}

/// 主体锚必须真实存在：`state_assertion.subject_anchor_id` 有 `REFERENCES anchor(id)`，
/// 悬空主体在**库这一层**就进不去（这不是测试的形状要求，是写侧的结构保证）。
fn seed_anchor(m: &MemoryHandle, id: &str, canonical: &str) {
    m.conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                                status, created_at_ms, updated_at_ms, norm_version)
             VALUES(?1, 'person', ?2, 'real', 'project', 'proj-state', 'active', ?3, ?3, 1)",
            rusqlite::params![id, canonical, SRC_MS],
        )
        .unwrap();
}

/// 一条合法写入的全部前置：主体锚在场。
fn seeded() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed_anchor(root.memory(), &anchor(1), SUBJECT);
    (dir, root)
}

/// 主表一行的断言面：`(id, 维度, 值, 前值, 有效期起点, 有效期终点, 模式)`。
type StateRow = (String, String, String, Option<String>, i64, Option<i64>, String);

fn rows(m: &MemoryHandle) -> Vec<StateRow> {
    let mut st = m
        .conn()
        .prepare(
            "SELECT id, dimension, value_after, value_before, valid_from_ms, valid_to_ms, mode
             FROM state_assertion ORDER BY created_at_ms, id",
        )
        .unwrap();
    let got = st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, String>(6)?,
            ))
        })
        .unwrap();
    got.collect::<Result<Vec<_>, _>>().unwrap()
}

fn count(m: &MemoryHandle, table: &str) -> i64 {
    m.conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// 某一行的生命周期三格：`(有效期起点, 有效期终点, 被谁替代)`。
/// 替代这件事的全部断言面就在这三格里，测试不该在每处再拼一份 SQL。
fn rows_of(m: &MemoryHandle, id: &str) -> (i64, Option<i64>, Option<String>) {
    m.conn()
        .query_row(
            "SELECT valid_from_ms, valid_to_ms, superseded_by FROM state_assertion WHERE id=?1",
            rusqlite::params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

fn superseded_by(m: &MemoryHandle, id: &str) -> Option<String> {
    rows_of(m, id).2
}

/// 某一行的使用侧统计 `(重复次数, 出处个数, 首次提及, 末次提及)`。
/// 方案 `design/2026-11-01-repeat-counters-and-content-pointers.md`：`occurrence_count` 记
/// "**同一句同一来源又被说了一遍**"（完全重复），`mention_count` 记"有几个出处"（内容指针）。
/// 这两义今天挤在同一个 `duplicate` 里，所以这四个数是它们唯一的分家处。
fn stats_of(m: &MemoryHandle, id: &str) -> (i64, i64, Option<i64>, Option<i64>) {
    m.conn()
        .query_row(
            "SELECT occurrence_count, mention_count, first_mentioned_ms, last_mentioned_ms \
             FROM memory_stats WHERE mem_table='state_assertion' AND mem_id=?1",
            rusqlite::params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
}

/// 状态行的出处轮（按轮序）。D20 的断言面就在这串里：同值二次断言**不新增行**，
/// 但第二次说它的那一轮必须在这里露头，否则"这话还被谁说过"在库里查不到。
fn provenance_turns(m: &MemoryHandle, id: &str) -> Vec<String> {
    let mut st = m
        .conn()
        .prepare(
            "SELECT turn_uid FROM provenance WHERE mem_table='state_assertion' AND mem_id=?1 \
             ORDER BY turn_uid",
        )
        .unwrap();
    let got = st
        .query_map(rusqlite::params![id], |r| r.get::<_, String>(0))
        .unwrap();
    got.collect::<Result<Vec<_>, _>>().unwrap()
}

/// 断言四处行都齐了，并把主表那一行的关键列一次看全。
#[test]
fn one_item_writes_state_provenance_mention_and_owner() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let out = put_state_rows(m, &[item()], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(out.states, 1);
    assert_eq!(out.mentions, 1);
    assert_eq!(out.duplicate, 0);
    assert_eq!(count(m, "state_assertion"), 1);
    assert_eq!(count(m, "provenance"), 1);
    assert_eq!(count(m, "memory_mention"), 1);
    assert_eq!(count(m, "resource_owner"), 1);
    // 状态不产卡：消费面走本表（`views.rs` 的 `current_states`）。
    assert_eq!(count(m, "card"), 0);
    assert_eq!(count(m, "fact"), 0);

    // 主表全部关键列的断言面：`(主体种类, 维度, 关系主体, 有效期起, 有效期止, 被谁替代,
    // 原因事实, 模式, 置信, 正文, 证据轮, 归属种类, 归属键, 来源 agent, 入库时刻)`。
    type Columns = (String, String, Option<String>, i64, Option<i64>, Option<String>,
                    Option<String>, String, f64, String, String, String, String, String, i64);
    let row: Columns = m
        .conn()
        .query_row(
            "SELECT subject_kind, dimension, subject_relation_id, valid_from_ms, valid_to_ms,
                    superseded_by, cause_fact_id, mode, confidence, text, evidence_turn_uid,
                    scope_kind, scope_key, source_agent, created_at_ms
             FROM state_assertion",
            [],
            |r| {
                Ok((
                    r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?,
                    r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?, r.get(12)?,
                    r.get(13)?, r.get(14)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(row.0, "anchor");
    assert_eq!(row.1, "emotion");
    assert_eq!(row.2, None, "v1 只允许 anchor 主体：relation 列必须空");
    assert_eq!(row.3, SRC_MS, "valid_from_ms 取源轮时刻，不是入库钟");
    assert_eq!(row.4, None, "库里这一维度还没有别的活行时，新行不该自带有效期终点");
    assert_eq!(row.5, None);
    assert_eq!(row.6, None);
    assert_eq!(row.7, "expressed");
    assert_eq!(row.8, 0.8);
    assert!(row.9.contains("焦虑得很"));
    assert_eq!(row.10, uid(1));
    assert_eq!(row.11, "project");
    assert_eq!(row.12, "proj-state");
    assert_eq!(row.13, "agent-state-w", "P 独占列只从 BatchCtx 进");
    assert_eq!(row.14, SRC_MS, "created_at_ms 同样取源轮时刻（I3）");
}

/// C1：`inferred` 是写侧闭集的第二个合法值——程序降级行必须原样落库，不被校验顶回。
#[test]
fn an_inferred_row_lands_with_its_mode_untouched() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let mut one = item();
    one.mode = "inferred".into();
    let out = put_state_rows(m, &[one], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(out.states, 1);
    let mode: String = m
        .conn()
        .query_row("SELECT mode FROM state_assertion", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "inferred");
}

/// 出处行的角色与归属行的可见性上界都对（与 fact 同一条纪律）。
#[test]
fn provenance_role_and_owner_visibility_follow_the_row() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let mut one = item();
    one.evidence = vec![(uid(1), AnchorRole::Human), (uid(2), AnchorRole::Agent)];
    put_state_rows(m, &[one], &ctx(), &owner(), NOW).unwrap();
    let roles: Vec<String> = {
        let mut st = m
            .conn()
            .prepare("SELECT anchor_role FROM provenance WHERE mem_table='state_assertion'
                      ORDER BY anchor_role")
            .unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(roles, vec!["agent".to_string(), "human".to_string()]);
    let (agent, principal, visibility): (String, Option<String>, String) = m
        .conn()
        .query_row(
            "SELECT owner_agent_id, owner_principal_id, visibility FROM resource_owner
             WHERE mem_table='state_assertion'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(agent, "agent-owner");
    assert_eq!(principal.as_deref(), Some("p-state"));
    assert_eq!(visibility, "project");
}

/// 第一道幂等：同一批重放，计数全 0、行数不变。
#[test]
fn replay_of_same_batch_writes_nothing() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let first = put_state_rows(m, &[item()], &ctx(), &owner(), NOW).unwrap();
    let second = put_state_rows(m, &[item()], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(first.states, 1);
    // 重放不写第二行；命中计数落在 `duplicate` 而不是"全 0"——这一格回答的是
    // "这批里有几条已经在库里了"，两道幂等共用同一个出口（方案 §4 的"同值幂等跳过并计数"）。
    assert_eq!(second.states, 0);
    assert_eq!(second.mentions, 0);
    assert_eq!(second.duplicate, 1);
    assert_eq!(count(m, "state_assertion"), 1);
    assert_eq!(count(m, "memory_mention"), 1);
}

/// 第二道幂等：**换一轮**说同一件事不写第二行（少了这一道，`current_states` 会被同一个值吃掉）。
#[test]
fn same_value_from_another_turn_counts_duplicate() {
    let (_dir, root) = seeded();
    let m = root.memory();
    put_state_rows(m, &[item()], &ctx(), &owner(), NOW).unwrap();
    let mut other = item();
    other.evidence = vec![(uid(7), AnchorRole::Human)];
    other.source_ms = SRC_MS + 60_000;
    let out = put_state_rows(m, &[other], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(out.states, 0, "同 (主体, 维度, 值) 的活行已经在了");
    assert_eq!(out.duplicate, 1);
    assert_eq!(count(m, "state_assertion"), 1);
}

/// D20：同值二次断言**不写第二行**，但第二次的那一轮要留下出处。
///
/// 现场证据是台账 §10.135 的七路臂读数——抽出 29 / 落库 23 / 审拒 2 / **去重丢 4**：
/// CPsyCoun 里来访者先说「我很担心自己跟不上」、后一轮说「我很担心自己落后」，归一后同
/// `(主体, 维度, 值)`，后那一轮在库里**零痕迹**，而审核决策链照样记 `applied`。
/// 项目承诺的是"可溯源撤销"，看不见"这话后来又被说过一次"就是缺的那一半。
#[test]
fn a_second_assertion_of_the_same_value_still_records_its_provenance() {
    let (_dir, root) = seeded();
    let m = root.memory();
    put_state_rows(m, &[item()], &ctx(), &owner(), NOW).unwrap();
    let live = rows(m)[0].0.clone();
    assert_eq!(provenance_turns(m, &live), vec![uid(1)]);

    let mut other = item();
    other.evidence = vec![(uid(7), AnchorRole::Human), (uid(8), AnchorRole::Human)];
    other.source_ms = SRC_MS + 60_000;
    let out = put_state_rows(m, &[other], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(out.states, 0, "仍然不新增当前状态行");
    assert_eq!(out.duplicate, 1);
    assert_eq!(count(m, "state_assertion"), 1);
    // 出处挂在**库里那条活行**上，而不是一个查无此行的 mem_id。
    assert_eq!(
        provenance_turns(m, &live),
        vec![uid(1), uid(7), uid(8)],
        "第二次说的两轮都要挂到同一条活行上"
    );
}

/// 完全重复（同一轮、同一来源、同一内容）⇒ **只涨计数指数**，不新增出处指针。
#[test]
fn an_exact_replay_bumps_the_occurrence_counter_only() {
    let (_dir, root) = seeded();
    let m = root.memory();
    put_state_rows(m, &[item()], &ctx(), &owner(), NOW).unwrap();
    let live = rows(m)[0].0.clone();
    assert_eq!(stats_of(m, &live), (1, 1, Some(SRC_MS), Some(SRC_MS)), "第一次落库：一物一出处");

    let second = put_state_rows(m, &[item()], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(second.duplicate, 1, "读数行的 `duplicate` 仍是总去重数（两义不分到读数行）");
    assert_eq!(stats_of(m, &live), (2, 1, Some(SRC_MS), Some(SRC_MS)));
    assert_eq!(provenance_turns(m, &live), vec![uid(1)], "同一轮不算第二个出处");
    assert_eq!(count(m, "memory_mention"), 1);
}

/// 只有内容一致（换一轮说同一件事）⇒ **加一个内容指针**，重复次数不动。
#[test]
fn another_turn_with_the_same_value_adds_a_pointer_not_an_occurrence() {
    let (_dir, root) = seeded();
    let m = root.memory();
    put_state_rows(m, &[item()], &ctx(), &owner(), NOW).unwrap();
    let live = rows(m)[0].0.clone();

    let mut other = item();
    other.evidence = vec![(uid(7), AnchorRole::Human)];
    other.source_ms = SRC_MS + 60_000;
    put_state_rows(m, &[other], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(stats_of(m, &live), (1, 2, Some(SRC_MS), Some(SRC_MS + 60_000)));
    assert_eq!(provenance_turns(m, &live), vec![uid(1), uid(7)]);
}

/// 两义不许共用一个数：重放两次 + 另说一轮 ⇒ 计数指数 3、出处 2。
/// 这条是上一对用例的**执法处**——若实现把两类塞进同一个计数器，这两个数会一起涨。
#[test]
fn the_two_kinds_of_repeat_never_share_one_counter() {
    let (_dir, root) = seeded();
    let m = root.memory();
    put_state_rows(m, &[item()], &ctx(), &owner(), NOW).unwrap();
    let live = rows(m)[0].0.clone();
    // 同一句同一来源再来两次（完全重复）
    put_state_rows(m, &[item()], &ctx(), &owner(), NOW).unwrap();
    put_state_rows(m, &[item()], &ctx(), &owner(), NOW).unwrap();
    // 换一轮说同一件事（内容指针）
    let mut other = item();
    other.evidence = vec![(uid(9), AnchorRole::Human)];
    other.source_ms = SRC_MS + 120_000;
    let out = put_state_rows(m, &[other], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(out.duplicate, 1, "读数行的计数是**本次调用**的：这一次有 1 条被去重挡下");
    let (occurrence, mention, first_ms, last_ms) = stats_of(m, &live);
    assert_eq!((occurrence, mention), (3, 2), "计数指数与出处个数是两格，不许一起涨");
    assert_eq!((first_ms, last_ms), (Some(SRC_MS), Some(SRC_MS + 120_000)), "首次不回填、末次跟最新");
}

/// 同维度**异值**、新断言更晚：写第二行、把旧值填进 `value_before`，并在**同一事务**里关掉旧行
/// （D10 §3 第一支：后来者上位）。旧行不是被"偷偷"改的——`superseded_by` 指认了替代者。
#[test]
fn different_value_writes_second_row_and_records_value_before() {
    let (_dir, root) = seeded();
    let m = root.memory();
    put_state_rows(m, &[item()], &ctx(), &owner(), NOW).unwrap();
    let mut changed = item();
    changed.value_after = "好一些".into();
    changed.evidence = vec![(uid(3), AnchorRole::Human)];
    changed.source_ms = SRC_MS + 120_000;
    let out = put_state_rows(m, &[changed], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(out.states, 1);
    assert_eq!(out.duplicate, 0);
    assert_eq!(out.superseded, 1, "后来者上位 ⇒ 必须关掉一条旧行");
    assert_eq!(out.backfilled, 0);
    let got = rows(m);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].3, None, "第一条没有前值");
    assert_eq!(got[1].3.as_deref(), Some("焦虑得很"), "第二条带上前值");
    assert_eq!(got[0].5, Some(SRC_MS + 120_000), "旧行的有效期终点 = 新断言的证据轮时刻");
    assert_eq!(got[1].5, None, "新行是当前值");
    let old_id = compute_state_id(&anchor(1), "emotion", "焦虑得很", &uid(1), &scope());
    let new_id = compute_state_id(&anchor(1), "emotion", "好一些", &uid(3), &scope());
    assert_eq!(superseded_by(m, &old_id).as_deref(), Some(new_id.as_str()), "旧行指认替代者");
    assert_eq!(superseded_by(m, &new_id), None, "当前行不被谁替代");
    // 冲突判据（闸用）在这里读到同一把尺子：异值算冲突、同值不算；关掉的旧行**不算**活行。
    assert!(live_conflict(m, &anchor(1), "emotion", "完全平静", &scope()).unwrap().is_some());
    assert!(live_conflict(m, &anchor(1), "emotion", "好一些", &scope()).unwrap().is_none());
    assert_eq!(live_conflict(m, &anchor(1), "emotion", "完全平静", &scope())
        .unwrap().unwrap().value_after, "好一些", "当前值只剩后来者那一条");
}

/// **迟到的旧话**（D10 §3 第二支）：证据轮时刻早于库里那条活行的新断言不改写现在——
/// 它自己一落库就带上 `valid_to_ms` = 活行的起点，旧行仍是当前值。
/// 这一条是这片的存在理由：按"后来者胜"会把**导入顺序**读成**事件顺序**。
#[test]
fn a_late_arrival_closes_itself_not_the_current_row() {
    let (_dir, root) = seeded();
    let m = root.memory();
    // 先落的是"现在"：更晚的一条证据轮。
    let mut current = item();
    current.value_after = "完全平静".into();
    current.evidence = vec![(uid(5), AnchorRole::Human)];
    current.source_ms = SRC_MS + 300_000;
    put_state_rows(m, &[current], &ctx(), &owner(), NOW).unwrap();
    // 三天后才导入的一句旧话：时刻更早、值不同。
    let mut late = item();
    late.value_after = "焦虑得很".into();
    late.evidence = vec![(uid(1), AnchorRole::Human)];
    late.source_ms = SRC_MS;
    let out = put_state_rows(m, &[late], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(out.states, 1, "迟到的旧话也要留一行（不删数据）");
    assert_eq!(out.backfilled, 1);
    assert_eq!(out.superseded, 0, "它没有顶掉任何人");
    let current_id = compute_state_id(&anchor(1), "emotion", "完全平静", &uid(5), &scope());
    let late_id = compute_state_id(&anchor(1), "emotion", "焦虑得很", &uid(1), &scope());
    assert_eq!(rows_of(m, &late_id).1, Some(SRC_MS + 300_000), "它只描述旧行起点之前那一段");
    assert_eq!(rows_of(m, &current_id).1, None, "当前行仍是后来那一条");
    // `value_before` 在迟到的那一支必须是 NULL：库里那条活行是它的**后继**，填进去就把
    // "前一个值"说反了，而它真正的前驱在库里没有行。
    let got = rows(m);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].2, "焦虑得很", "夹具按 created_at_ms 定序：迟到的那条在前");
    assert_eq!(got[0].3, None, "迟到的旧话不知道自己在库里的前驱");
    assert_eq!(got[1].3, None);
    assert_eq!(live_conflict(m, &anchor(1), "emotion", "还在担心", &scope())
        .unwrap().unwrap().value_after, "完全平静");
}

/// 替代跑两次不长行：第二次的同一句被**第二道幂等**挡在 `duplicate`，
/// 旧行也不会被关第二次（`valid_to_ms`/`superseded_by` 一格都不动）。
#[test]
fn re_running_a_supersede_pair_changes_nothing() {
    let (_dir, root) = seeded();
    let m = root.memory();
    put_state_rows(m, &[item()], &ctx(), &owner(), NOW).unwrap();
    let mut changed = item();
    changed.value_after = "好一些".into();
    changed.evidence = vec![(uid(3), AnchorRole::Human)];
    changed.source_ms = SRC_MS + 120_000;
    put_state_rows(m, &[changed.clone()], &ctx(), &owner(), NOW).unwrap();
    let old_id = compute_state_id(&anchor(1), "emotion", "焦虑得很", &uid(1), &scope());
    let before = rows_of(m, &old_id);

    let again = put_state_rows(m, &[changed], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(again.states, 0);
    assert_eq!(again.superseded, 0, "旧行已经关了，不该再算一次上位");
    assert_eq!(again.duplicate, 1, "库里当前值就是这一条");
    assert_eq!(count(m, "state_assertion"), 2);
    assert_eq!(rows_of(m, &old_id), before, "重放不许改动被替代那一行的任何一格");
    assert_eq!(count(m, "provenance"), 2, "每行各一条出处，重放不加");
}

/// 维度闭集就是 DDL 那九个词：写得出、也拒掉别的。
#[test]
fn dimension_closed_set_is_the_nine_ddl_values() {
    assert_eq!(
        STATE_DIMENSIONS,
        [
            "emotion", "mood", "sentiment", "affect_episode", "trust", "relationship", "health",
            "config", "progress"
        ]
    );
    let (_dir, root) = seeded();
    let m = root.memory();
    for dimension in STATE_DIMENSIONS {
        let mut one = item();
        one.dimension = dimension.to_string();
        one.evidence = vec![(uid(dimension.len()), AnchorRole::Human)];
        put_state_rows(m, &[one], &ctx(), &owner(), NOW).unwrap();
    }
    assert_eq!(count(m, "state_assertion"), STATE_DIMENSIONS.len() as i64);
}

/// 坏 item ⇒ 整批 `Err` 且库里零行（校验先于事务）。
#[test]
fn invalid_items_reject_the_whole_batch_with_zero_rows() {
    let cases: Vec<(StateWriteItem, &str)> = {
        let mut blank_subject = item();
        blank_subject.subject_anchor_id = "   ".into();
        let mut bad_dimension = item();
        bad_dimension.dimension = "feeling".into();
        let mut blank_value = item();
        blank_value.value_after = "  ".into();
        // C1 起 `inferred` 是合法值（它由程序在"主语与值不同句"时盖章）；闭集外的词
        // ——比如 fact/event 那一族的 `reported`——仍必须整批拒。
        let mut bad_mode = item();
        bad_mode.mode = "reported".into();
        let mut blank_text = item();
        blank_text.text = "".into();
        let mut no_evidence = item();
        no_evidence.evidence = vec![];
        let mut blank_turn = item();
        blank_turn.evidence = vec![("  ".into(), AnchorRole::Human)];
        vec![
            (blank_subject, "subject"),
            (bad_dimension, "dimension"),
            (blank_value, "value_after"),
            (bad_mode, "mode"),
            (blank_text, "text"),
            (no_evidence, "evidence"),
            (blank_turn, "evidence"),
        ]
    };
    for (bad, column) in cases {
        let (_dir, root) = seeded();
        let m = root.memory();
        let err = put_state_rows(m, &[bad], &ctx(), &owner(), NOW).unwrap_err();
        match (&err, column) {
            (MemError::BadA1Field(got), want) => assert_eq!(*got, want, "列名报错位"),
            other => panic!("预期 BadA1Field，拿到 {other:?}"),
        }
        assert_eq!(count(m, "state_assertion"), 0);
        assert_eq!(count(m, "provenance"), 0);
        assert_eq!(count(m, "resource_owner"), 0);
    }
}

/// 一条坏 item 混在好 item 里 ⇒ 整批回滚（不是"好的那条先落"）。
#[test]
fn one_bad_item_rolls_back_the_good_ones() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let mut bad = item();
    bad.dimension = "mood".into();
    let mut worse = item();
    worse.dimension = "not-a-dimension".into();
    worse.evidence = vec![(uid(9), AnchorRole::Agent)];
    let err = put_state_rows(m, &[bad, worse], &ctx(), &owner(), NOW).unwrap_err();
    assert!(matches!(err, MemError::BadA1Field("dimension")), "{err:?}");
    assert_eq!(count(m, "state_assertion"), 0);
}

/// 只读副本不收写入口（`conn()` 是逃生口，"只读"这一半由写入口自己守）。
#[test]
fn read_only_handle_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("replica.db");
    {
        let root = Root::open(dir.path()).unwrap();
        seed_anchor(root.memory(), &anchor(1), SUBJECT);
        put_state_rows(root.memory(), &[item()], &ctx(), &owner(), NOW).unwrap();
    }
    std::fs::copy(dir.path().join("memory.db"), &snap).unwrap();
    let ar = rc_store::AgentRoot::open_replica(dir.path(), &snap).unwrap();
    assert!(ar.memory().is_read_only(), "夹具前提破了：副本句柄不是只读的");
    let err = put_state_rows(ar.memory(), &[item()], &ctx(), &owner(), NOW).unwrap_err();
    assert!(matches!(err, MemError::ReadOnly), "{err:?}");
}

/// id 是内容寻址：换主体/维度/值/轮/scope 任一格就是新行；一格都不换就是同一行。
#[test]
fn state_id_is_content_addressed() {
    let base = compute_state_id(&anchor(1), "emotion", "焦虑得很", &uid(1), &scope());
    assert_eq!(base, compute_state_id(&anchor(1), "emotion", "焦虑得很", &uid(1), &scope()));
    let mut changed_scope = scope();
    changed_scope.key = "proj-state ".into();
    assert_eq!(
        base,
        compute_state_id(&anchor(1), "emotion", "焦虑得很", &uid(1), &changed_scope),
        "尾随空格是归一化差异，不是新行"
    );
    for (subject, dimension, value, turn) in [
        (anchor(2).as_str(), "emotion", "焦虑得很", uid(1).as_str()),
        (anchor(1).as_str(), "mood", "焦虑得很", uid(1).as_str()),
        (anchor(1).as_str(), "emotion", "好一些", uid(1).as_str()),
        (anchor(1).as_str(), "emotion", "焦虑得很", uid(2).as_str()),
    ] {
        assert_ne!(
            base,
            compute_state_id(subject, dimension, value, turn, &scope()),
            "{dimension}/{value} 的变动必须换行"
        );
    }
}
