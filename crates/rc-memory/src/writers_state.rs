//! P2 状态 pass 的写库层（方案 `design/2026-10-31-state-pass-p2.md` §3–§4）。
//!
//! `state_assertion` 这张表早有 DDL、有读面（`rc-agent/src/views.rs` 的 `current_states`）、
//! 有复制路径（`hub/src/rows.rs` 的 `INSERT_ORDER`），**独缺生产者**——本片补的就是这一处。
//!
//! 一条状态 = **四处行、一个事务**：`state_assertion` + 逐条 `provenance` + 主体的
//! `memory_mention` + `resource_owner`。任何一步失败 ⇒ 整批回滚，不存在"行在、出处不在"
//! 或"行在、归属不在"（与 [`crate::writers_fact`] 同一条纪律）。
//! 与 fact 的差别只有一处：**不建 `card`**。状态的消费面走 `state_assertion` 本表
//! （`views.rs` 按 `valid_to_ms IS NULL` 取当前值），再造一张卡就是同一件事存两处。
//!
//! 模型面不许写的东西在这一层**结构上写不出来**：
//! - `id` 由 [`compute_state_id`] 现算（内容寻址）；
//! - `subject_kind` 恒 `'anchor'`、`subject_relation_id` 恒 NULL（v1 边界：relation 表零生产者，
//!   留 P6）；列上的两条跨列 CHECK 会把任何别的写法顶回；
//! - `scope_*` / `source_agent` / `device_id` 只从 [`StateWriteItem::scope`] 与 [`BatchCtx`] 来；
//! - `owner/visibility/retention/created_by_agent` 只从 [`OwnerCtx`] 与 `BatchCtx` 来；
//! - `valid_from_ms` / `created_at_ms` 一律取**源轮时刻** `source_ms`（导入钟不参与，同 I3）；
//! - `valid_to_ms` 只在 D10 的"迟到的旧话"那一支被程序写成旧行起点，`superseded_by` 只出现在
//!   **被关掉的旧行**上（由 [`STATE_CLOSE`] 写）；两者都不是模型递得进来的格子；
//! - `confidence` 是程序常量 0.8；
//! - `value_before` 由本层从库里同主体的既有行读出来（见 [`live_value_before`]），模型递不进来；
//! - `text` 由程序按 `主体 · 维度 · 值` 组装，不是模型写的整句。
//!
//! 幂等有两道，缺一不可：
//! 1. 内容寻址 id + `ON CONFLICT(id) DO NOTHING`——同一轮同一句直查重放不增行；
//! 2. [`find_live_twin`]——**不同轮**说了同一个 `(主体, 维度, 值)` 时不写第二行，计 `duplicate`，
//!    但那几轮的 `provenance` 照写（D20：不新增"当前状态"，不等于这话没被说过）。
//!    没有第二道，"我很焦虑"被说十次就会在库里留十行当前状态，而 `current_states` 只按
//!    `valid_from_ms DESC` 取前五条 ⇒ 同一个值吃掉整个当前状态面。
//!
//! **同维度异值不靠抽取侧偷偷覆盖，也不"只拦不裁"**：程序闸把"更新的那一条"放行到审核
//! （`gate.rs` 第 9 步按 D10 的规则分流），到了本层由 [`put_state_rows`] 按两条确定性规则落笔——
//! 后来者上位并在同一事务里关掉旧行，迟到的旧话自己被关在旧行起点之前
//! （方案 `design/2026-11-01-state-conflict-adjudication.md` §3）。
//!
//! 校验发生在**事务之前**：一条坏 item ⇒ 整批 `Err`，库里零行。错误载荷只带列名，不带正文
//! （守卫 12）。复用 [`MemError::BadA1Field`] 而不新开端：`MemError` 是
//! `rc-agent/src/error.rs` 那张逐变体点名的分型表的输入，新增一支会逼那个文件补 match 臂。
use crate::identity::compute_state_id;
use crate::keys::{AnchorRole, Scope};
use crate::owner::{self, OwnerCtx};
use crate::writers_x::BatchCtx;
use crate::MemError;
use rc_contract::normalize_body;
use rc_store::MemoryHandle;
use rusqlite::params;

/// `state_assertion.dimension` 的 DDL 九值闭集（`rc-store/src/ddl.rs` 的 CHECK 逐字对拍）。
/// **不加新维度**：闭集是列的一部分，扩它要走 DDL 与迁移，不是在 prompt 里多写一个词。
pub const STATE_DIMENSIONS: [&str; 9] = [
    "emotion", "mood", "sentiment", "affect_episode", "trust", "relationship", "health",
    "config", "progress",
];

/// 原文直陈（`state_assertion.mode` 两值之一）。v1 只收这一值，C1 起 [`STATE_MODE_INFERRED`]
/// 也合法：不由模型声明，只由程序在"主语与值不同句/自锚且值句无第一人称"时盖上。
pub const STATE_MODE_EXPRESSED: &str = "expressed";

/// 程序降级值（C1）。它与 [`STATE_MODE_EXPRESSED`] 是这一列的**全部**合法值（DDL 的 CHECK
/// 逐字同集）；模型面不写它——写侧不区分两者，只认闭集。
pub const STATE_MODE_INFERRED: &str = "inferred";

/// 程序盖章的默认置信度，与 fact 同一格（审核通过后才允许升，本层不冒充）。
const STATE_CONFIDENCE: f64 = 0.8;

/// 状态行所在的记忆表名。`provenance.mem_table` / `memory_mention.mem_table` /
/// `resource_owner.mem_table` 三处共用这一格，`ref_kind` 用
/// [`crate::keys::REF_KINDS`] 里的同名项。
pub const STATE_MEM_TABLE: &str = "state_assertion";

/// 一条待写的状态断言。字段与方案 §3 的输出契约一一对应；另外三格（`scope`/`source_ms`/
/// `text`）由调用方从批次与源轮带进来，它们不在模型输出里。
#[derive(Debug, Clone)]
pub struct StateWriteItem {
    /// 已过审的主体锚 id（v1 只允许 anchor 主体）。
    pub subject_anchor_id: String,
    /// 九值闭集，见 [`STATE_DIMENSIONS`]。
    pub dimension: String,
    /// 直陈出来的当前值，非空。
    pub value_after: String,
    /// 只收 [`STATE_MODE_EXPRESSED`] / [`STATE_MODE_INFERRED`]。
    pub mode: String,
    /// 程序组装的消费句（`主体 · 维度 · 值`），不是模型写的整句。
    pub text: String,
    /// 多证据全写 `provenance`；`evidence_turn_uid` 取第一条。非空。
    pub evidence: Vec<(String, AnchorRole)>,
    pub scope: Scope,
    /// 源轮时刻：`valid_from_ms` / `created_at_ms` / mention / 归属的入库时刻都取它。
    pub source_ms: i64,
}

/// 本次调用真写进去的行数。三格都是**插入行数**，不是计划条数：重放全 0。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StateWriteOutcome {
    /// 真插入的 `state_assertion` 行数。
    pub states: usize,
    /// 真插入的 `memory_mention` 行数。
    pub mentions: usize,
    /// 第二道幂等命中数：库里已有同 `(主体, 维度, 值)` 的未过期行 ⇒ 不写第二行。
    /// 整批都被去重时 `states=0` 才有解释（与 fact 的 `duplicate` 同一格读法）。
    pub duplicate: usize,
    /// D10 第一支：这条新断言更晚 ⇒ 同一事务里关掉了一条旧行（`states` 与 `superseded` 成对增）。
    pub superseded: usize,
    /// D10 第二支：迟到的旧话——新断言自己带着 `valid_to_ms` 落库（等于旧行的起点），旧行仍是当前。
    pub backfilled: usize,
    /// 关旧行时 UPDATE 命中 0 行（那一行在本次事务之外已被别人关掉）。写侧不动它，只记账。
    pub superseded_skipped: usize,
}

fn blank(part: &str) -> bool {
    normalize_body(part).is_empty()
}

/// 写侧校验（在任何一个事务开始**之前**）。任一条坏 ⇒ 整批 `Err`，不落半批。
fn validate(items: &[StateWriteItem]) -> Result<(), MemError> {
    for it in items {
        if blank(&it.subject_anchor_id) { return Err(MemError::BadA1Field("subject")); }
        if !STATE_DIMENSIONS.contains(&it.dimension.as_str()) {
            return Err(MemError::BadA1Field("dimension"));
        }
        if blank(&it.value_after) { return Err(MemError::BadA1Field("value_after")); }
        if it.mode != STATE_MODE_EXPRESSED && it.mode != STATE_MODE_INFERRED {
            return Err(MemError::BadA1Field("mode"));
        }
        if blank(&it.text) { return Err(MemError::BadA1Field("text")); }
        if it.evidence.is_empty() { return Err(MemError::BadA1Field("evidence")); }
        if it.evidence.iter().any(|(turn_uid, _)| turn_uid.trim().is_empty()) {
            return Err(MemError::BadA1Field("evidence"));
        }
    }
    Ok(())
}

/// 状态行的唯一插入字面量。列序与 `rc-store` 的 DDL 逐位对应，**每一列都显式出现**：
/// 交给 DEFAULT 会把"没人说过"和"程序说过是空"混成一格。`subject_relation_id` 恒 NULL
/// 与 `subject_kind='anchor'` 成对（列上的跨列 CHECK 只认这一种组合）。
///
/// `valid_to_ms` 是 `?17` 那一格而不是写死的 NULL：D10 之后它由 [`put_state_rows`] 的替代规则
/// 决定（迟到的旧话一落库就带上"到旧行起点为止"）。`superseded_by` 与 `cause_fact_id` 仍恒
/// NULL——被替代是**旧行**那一格的动作（见 [`STATE_CLOSE`]），因果归 P6，这一层都不冒充。
const STATE_INSERT: &str = "INSERT INTO state_assertion(
    id, subject_kind, subject_anchor_id, subject_relation_id, dimension, value_before,
    value_after, valid_from_ms, valid_to_ms, superseded_by, cause_fact_id, text, mode,
    confidence, evidence_turn_uid, world_id, scope_kind, scope_key, source_agent, device_id,
    created_at_ms, norm_version)
 VALUES(?1,'anchor',?2,NULL,?3,?4,?5,?6,?17,NULL,NULL,?7,?8,?9,?10,NULL,?11,?12,?13,?14,?15,?16)
 ON CONFLICT(id) DO NOTHING";

/// D10 §3 的第一支：后来者上位 ⇒ 在同一事务里关掉被顶掉的那条旧行，并指认替代者。
/// `AND valid_to_ms IS NULL` 是并发位——两个设备几乎同时推同一对 (旧行, 新行) 时，后提交那一边
/// 的 UPDATE 命中 0 行，计 `superseded_skipped` 而不是把已经关过的行再关一次。
const STATE_CLOSE: &str =
    "UPDATE state_assertion SET valid_to_ms=?1, superseded_by=?2 WHERE id=?3 AND valid_to_ms IS NULL";

/// 库里同 `(主体, 维度)` 且未过期的那条当前行，用于补 `value_before`、判去重与走 D10 的替代规则。
/// `None` = 这个主体在这个维度上还没有任何活着的状态。
///
/// 三格都要带出去：`id` 是替代时要关的那一行，`valid_from_ms` 是"谁更晚"的那把尺子
/// （方案 `design/2026-11-01-state-conflict-adjudication.md` §3）。只回 `value_after` 的话
/// 写侧就分不清"后来者"与"迟到的旧话"，而那正是这一片的全部价值。
///
/// 取值规则与消费面同一条尺子（`views.rs` 的 `current_states`）：`valid_to_ms IS NULL`，
/// 按 `valid_from_ms DESC` 取最近一条；同刻并列按 `id` 定序，免得两次重放拿到不同值。
pub struct LiveState {
    pub id: String,
    pub value_after: String,
    pub valid_from_ms: i64,
    /// 判"完全重复"要用的三格（时间/内容/来源里的**时间**与**来源**两半）：
    /// 同一句由**同一轮**、**同一 agent/device** 再说一遍才算重复，见
    /// `design/2026-11-01-repeat-counters-and-content-pointers.md`。
    pub evidence_turn_uid: String,
    pub source_agent: String,
    pub device_id: String,
}

fn find_live_state(
    conn: &rusqlite::Connection, subject_anchor_id: &str, dimension: &str, scope: &Scope,
) -> Result<Option<LiveState>, MemError> {
    let mut st = conn.prepare(
        "SELECT id, value_after, valid_from_ms, evidence_turn_uid, source_agent, device_id
         FROM state_assertion
         WHERE subject_anchor_id=?1 AND dimension=?2 AND valid_to_ms IS NULL
           AND scope_kind=?3 AND scope_key=?4
         ORDER BY valid_from_ms DESC, id LIMIT 1",
    )?;
    let mut rows = st.query(params![subject_anchor_id, dimension, scope.kind.as_str(), scope.key])?;
    match rows.next()? {
        None => Ok(None),
        Some(row) => Ok(Some(LiveState {
            id: row.get(0)?,
            value_after: row.get(1)?,
            valid_from_ms: row.get(2)?,
            evidence_turn_uid: row.get(3)?,
            source_agent: row.get(4)?,
            device_id: row.get(5)?,
        })),
    }
}

/// `memory_stats` 那一行立起来（**只在没有时**）：一物一出处，首次=末次=源轮时刻。
/// 老库里先写于行、后补 stats 的那批，第一次碰到时也从 `(1, 1)` 起步——原来那次断言的
/// 出处与时刻无从重建，**不许回填成 0，也不许冒充已知**。
fn ensure_stats(
    tx: &rusqlite::Transaction<'_>, mem_id: &str, source_ms: i64, pointers: i64,
) -> Result<(), MemError> {
    tx.execute(
        "INSERT OR IGNORE INTO memory_stats(
             mem_table, mem_id, occurrence_count, mention_count,
             first_mentioned_ms, last_mentioned_ms, salience_updated_ms)
         VALUES('state_assertion',?1,1,?2,?3,?3,?3)",
        params![mem_id, pointers, source_ms],
    )?;
    Ok(())
}

/// 两类重复各自那一格的递增。两义**不许共用一个数**：
/// `occurrence_count` 只收"同一句同一来源又被说了一遍"（完全重复，无新出处），
/// `mention_count` 只收"多了一个出处"（异轮/异来源说同一件事）。
fn bump_stats(
    tx: &rusqlite::Transaction<'_>, mem_id: &str, source_ms: i64,
    occurrence: i64, pointers: i64,
) -> Result<(), MemError> {
    tx.execute(
        "UPDATE memory_stats
         SET occurrence_count = occurrence_count + ?2,
             mention_count    = mention_count + ?3,
             last_mentioned_ms = MAX(COALESCE(last_mentioned_ms, 0), ?4),
             salience_updated_ms = ?4
         WHERE mem_table='state_assertion' AND mem_id=?1",
        params![mem_id, occurrence, pointers, source_ms],
    )?;
    Ok(())
}

/// 写一批状态断言。**整批一个事务**：任何一条失败 ⇒ 一条都不写。
///
/// `now_ms` 是形参位（与其余写入口同形），本层**不使用**：所有时刻取源轮 `source_ms`
/// ——与 [`crate::writers_fact::put_fact_rows_with_extras`] 同一条 I3 纪律。
pub fn put_state_rows(
    m: &MemoryHandle, items: &[StateWriteItem], ctx: &BatchCtx,
    owner: &OwnerCtx, _now_ms: i64,
) -> Result<StateWriteOutcome, MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    validate(items)?;
    let mut out = StateWriteOutcome::default();
    if items.is_empty() { return Ok(out); }
    let norm_version = rc_store::capability::NORM_VERSION as i64;
    // 主体锚的 canonical 名要从库里读（组装 `text` 与判可见性都靠它）；本层不造锚。
    let tx = m.conn().unchecked_transaction()?;
    for it in items {
        let evidence_turn_uid = &it.evidence[0].0;   // validate 已保证非空
        let live = find_live_state(&tx, &it.subject_anchor_id, &it.dimension, &it.scope)?;
        // 第二道幂等：同 `(主体, 维度, 值)` 已经有活行 ⇒ 不写第二行。但这一支**分两义**
        // （用户 11-01 的要求，方案 `design/2026-11-01-repeat-counters-and-content-pointers.md`）：
        //  · 完全重复（同一轮 + 同一 agent/device + 同一内容）⇒ 只涨 `occurrence_count`，不新增出处；
        //  · 只有内容一致（另一轮/另一来源说的）⇒ 补 `provenance` 内容指针并涨 `mention_count`
        //    （D20：只 `continue` 会让第二次断言在库里零痕迹，撤销与复核就看不见它）。
        // 读数行的 `duplicate` 仍是**两类合计**（键序与不变式不动，两义的分家只在库里）。
        if let Some(live) = &live {
            if normalize_body(&live.value_after) == normalize_body(&it.value_after) {
                let exact = live.evidence_turn_uid == *evidence_turn_uid
                    && live.source_agent == ctx.source_agent
                    && live.device_id == ctx.device_id;
                out.duplicate += 1;
                if exact {
                    ensure_stats(&tx, &live.id, it.source_ms, 1)?;
                    bump_stats(&tx, &live.id, it.source_ms, 1, 0)?;
                } else {
                    let mut added = 0usize;
                    for (turn_uid, role) in &it.evidence {
                        added += tx.execute(
                            "INSERT OR IGNORE INTO provenance(mem_table, mem_id, turn_uid, anchor_role)
                             VALUES('state_assertion',?1,?2,?3)",
                            params![live.id, turn_uid, role.as_str()],
                        )?;
                    }
                    ensure_stats(&tx, &live.id, it.source_ms, 1 + added as i64)?;
                    if added > 0 { bump_stats(&tx, &live.id, it.source_ms, 0, added as i64)?; }
                }
                continue;
            }
        }
        // D10 §3 的两条确定性规则——谁更晚，谁就是"当前"：
        //  · 新断言**不早于**库里那条 ⇒ 新断言上位，旧行在同一个事务里被关闭并指认替代者；
        //  · 新断言**更早**（迟到的旧话）⇒ 它自己一落库就带上 `valid_to_ms` = 旧行的起点，
        //    它只描述"旧行之前那一段"，不改写现在。这一支是这片的全部价值：按"后来者胜"
        //    会把导入顺序读成事件顺序，三天后补进的一句"我上周很焦虑"会关掉当前的"平静得很"。
        let late = live.as_ref().is_some_and(|live| it.source_ms < live.valid_from_ms);
        let valid_to_ms = if late { live.as_ref().map(|live| live.valid_from_ms) } else { None };
        // `value_before` 是**库里既有值**（异值才写；同值已在上面跳过），由程序补。
        // "迟到的旧话"那一支例外：库里那条活行在时间上是它的**后继**不是前驱，而它真正的
        // 前驱在库里没有行——填活行的值会把"前一个值"说反，所以留 NULL。
        let value_before = if late {
            None
        } else {
            live.as_ref().map(|live| live.value_after.clone())
        };
        let id = compute_state_id(
            &it.subject_anchor_id, &it.dimension, &it.value_after, evidence_turn_uid, &it.scope,
        );
        let inserted = tx.execute(
            STATE_INSERT,
            params![
                id, it.subject_anchor_id, it.dimension, value_before, it.value_after,
                it.source_ms, it.text, it.mode, STATE_CONFIDENCE, evidence_turn_uid,
                it.scope.kind.as_str(), it.scope.key, ctx.source_agent, ctx.device_id,
                it.source_ms, norm_version, valid_to_ms,
            ],
        )?;
        out.states += inserted;
        if inserted > 0 {
            match live.as_ref() {
                Some(_) if late => out.backfilled += 1,
                Some(live) => {
                    let closed = tx.execute(STATE_CLOSE, params![it.source_ms, id, live.id])?;
                    if closed == 1 { out.superseded += 1 } else { out.superseded_skipped += 1 }
                }
                None => {}
            }
        }
        // 出处：多证据全写，role 取证据自己的（`evidence_turn_uid` 只是第一条的副本）。
        let mut pointers = 0usize;
        for (turn_uid, role) in &it.evidence {
            pointers += tx.execute(
                "INSERT OR IGNORE INTO provenance(mem_table, mem_id, turn_uid, anchor_role)
                 VALUES('state_assertion',?1,?2,?3)",
                params![id, turn_uid, role.as_str()],
            )?;
        }
        // 使用侧统计：新行按"一物、N 个出处"起步（N = 这次真落下的 provenance 条数，
        // 一条证据就是 1）。`salience`/`emotional_intensity` 那些格**一概不动**——
        // 在没有排序消费者之前给它们编值是假的（方案 §6）。
        ensure_stats(&tx, &id, it.source_ms, pointers.max(1) as i64)?;
        // 提及：主体一条。P 独占列只从 ctx 取。
        out.mentions += tx.execute(
            "INSERT OR IGNORE INTO memory_mention(
                 mem_table, mem_id, anchor_id, source_agent, device_id, created_at_ms,
                 norm_version)
             VALUES('state_assertion',?1,?2,?3,?4,?5,?6)",
            params![id, it.subject_anchor_id, ctx.source_agent, ctx.device_id,
                    it.source_ms, norm_version],
        )?;
        // 归属：visibility 再过一次上界（行 scope 是天花板）。
        owner::insert(&tx, &owner::OwnerRow {
            mem_table: STATE_MEM_TABLE.to_string(),
            mem_id: id.clone(),
            owner_principal_id: owner.owner_principal_id.clone(),
            owner_agent_id: owner.owner_agent_id.clone(),
            created_by_agent: ctx.source_agent.clone(),
            visibility: owner::effective(owner.visibility, it.scope.kind),
            retention: owner::normalize_retention(&owner.retention),
            scope: it.scope.clone(),
            source_agent: Some(ctx.source_agent.clone()),
            device_id: Some(ctx.device_id.clone()),
            created_at_ms: it.source_ms,
        })?;
    }
    tx.commit()?;
    Ok(out)
}

/// 与这条断言**同主体同维度、但值不同**的那条活行（库里当前的旧值），带出 `id` 与它的起点。
///
/// 这是程序闸 `state_conflict_same_dim` 的判据输入。闸拿到 `valid_from_ms` 之后分流两件事
/// （D10 §4 的第四行）：**更新的那一条**不是冲突待裁，它是来顶掉旧值的，放行给审核；
/// **更早的那一条**（迟到的旧话）才是待人看的隔离。"谁更晚"这把尺子在本层与 [`put_state_rows`]
/// 用的是同一格 `source_ms`（证据轮 `created_at_ms` 的最大值），两侧各算一份就会分叉。
///
/// 扫描面与 [`find_live_state`] 逐字同一把尺子——两处各写一份 SQL 的话，"当前值"在闸那里和
/// 写库那里就不是同一个东西了。
///
/// 走 `MemoryHandle`（不是事务）：程序闸在读侧，它不能改库。
pub fn live_conflict(
    m: &MemoryHandle, subject_anchor_id: &str, dimension: &str, value_after: &str, scope: &Scope,
) -> Result<Option<LiveState>, MemError> {
    let live = find_live_state(m.conn(), subject_anchor_id, dimension, scope)?;
    Ok(match live {
        None => None,
        Some(live) if normalize_body(&live.value_after) == normalize_body(value_after) => None,
        Some(live) => Some(live),
    })
}
