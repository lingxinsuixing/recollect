//! 锚点一键换绑（D27）：把挂在旧锚上的关联数据搬到新锚，旧锚转成 `merged` 并留下台账。
//!
//! 为什么必须有（用户 2026-11-01）：**锚是在证据不全时被命名的**——"我同事"、"老方"、
//! "那个客户"都是当时能拿到的最强称呼。锚不可换，后来的认知就写不回已经落库的判断上，
//! 描述会永久停在第一次猜的位置。
//!
//! 四条这片守住的规矩：
//!
//! 1. **重键，不是改列**。`state_assertion.id` 与 `mention.id` 都把主体/锚算进哈希
//!    （[`compute_state_id`] / [`compute_mention_id`]），所以换主体必须按新锚重算 id；
//!    只 UPDATE 一列会让"id 可从内容复算"这条不变式失效——审核重放（`review::replay`）与
//!    去重都靠它。
//! 2. **搬不动的就挡下来**。`fact` 的 id 输入含 `time_text`，而 **`fact` 表没有这一列**
//!    （时间短语落在 `time_expr`）⇒ 事实行无法只凭行内容重算 id。这片**不假装能搬**：
//!    旧锚上只要有 `fact.subject_anchor_id`/`fact.object_anchor_id`、`relation`、`attribute`、
//!    `utterance` 的行，或 `card.pointers_json` 指到它，apply 直接拒绝
//!    （[`RebindError::Blocked`]），预演把原因码印出来。
//!    出路两条（另开一刀）：给 `fact` 补一列 `time_text`，或从审核载荷重放重建。
//! 3. **零悬空，按"id 里有没有这一格"分三种走法**。12 个 anchor 外键列 + 无 FK 保护的
//!    `memory_mention.anchor_id`，逐列要么搬要么挡，不许"能搬多少搬多少"：
//!    - **重键**（这一格参与行 id）：`state_assertion.subject_anchor_id`、`mention.anchor_id`；
//!    - **就地改列**（不参与行 id，所以行 id 不变、它的出处与归属一格都不动）：
//!      `fact.place_anchor_id`、`fact.speaker_anchor_id`；
//!    - **挡下来**（id 规则未核实或输入不全）：`fact.subject/object`、`relation.subject/object`、
//!      `attribute.anchor_id`、`utterance.speaker/addressee`；
//!    - `alias` 并进新锚、`memory_mention` 改指新锚；`provenance`/`resource_owner`/`attention`
//!      按 `(mem_table, mem_id)` 存，所以只在重键的那两类上跟着搬。
//! 4. **旧锚出声不消失**：`status='merged'`（这是它的第一生产者）、正名并进新锚的 `alias`，
//!    并写一行 `memory_op(op='MERGE')` 记下"来自哪一格、去了哪一格、正名是什么、谁做的、为什么"。
//!    `attribute` 表到今天没有生产者、id 算法未核实 ⇒ **不往它里面发明值**，称呼的残留走别名 + 台账。
use crate::identity::{compute_mention_id, compute_state_id};
use crate::keys::{Scope, ScopeKind};
use crate::MemError;
use rc_store::MemoryHandle;
use rusqlite::OptionalExtension;

/// 换绑被拒的原因（闭集，逐条可对账）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebindError {
    /// 两格是同一格。
    SameAnchor,
    /// 有一格在库里不存在。
    Missing,
    /// `kind` 不同：那是"重新命名实体"，不是换绑（走抽取线）。
    KindMismatch,
    /// scope 不同：跨隔离域的同名是合法的另一个实体。
    ScopeMismatch,
    /// 源锚已经 merged（换过的格子不许再换）。
    AlreadyMerged,
    /// 目标锚已经 merged（merged 链必须一步到头）。
    TargetMerged,
    /// 写指向行前检测到环（`into` 的出链走回 `from`）——不该出现的树形，宁可拒绝。
    RedirectCycle,
    /// 有搬不动的引用（见模块文档第 2 条），原因码在 [`RebindPlan::blockers`]。
    Blocked,
    /// 只读句柄。
    ReadOnly,
    /// 底层存储错误（带原文，操作员要能看到是哪条 SQL 炸的）。
    Store(String),
}

impl From<MemError> for RebindError {
    fn from(e: MemError) -> Self {
        Self::Store(e.to_string())
    }
}

/// SQL 层错误统一走 `MemError` 再落文案：与库内其它写入口同一层翻译，
/// 免得 `?` 在每个调用点各写一遍 `map_err`（写了三处就该有一个实现）。
impl From<rusqlite::Error> for RebindError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(MemError::from(e).to_string())
    }
}

/// 一格锚的身份三格（校验用）。
struct AnchorFace {
    kind: String,
    scope: Scope,
    status: String,
}

fn face(m: &MemoryHandle, id: &str) -> Result<Option<AnchorFace>, MemError> {
    let mut st = m.conn().prepare(
        "SELECT kind, scope_kind, scope_key, status FROM anchor WHERE id=?1",
    )?;
    let mut rows = st.query([id])?;
    Ok(match rows.next()? {
        None => None,
        Some(row) => Some(AnchorFace {
            kind: row.get(0)?,
            scope: Scope { kind: ScopeKind::parse(&row.get::<_, String>(1)?)
                .unwrap_or(ScopeKind::Project), key: row.get(2)? },
            status: row.get(3)?,
        }),
    })
}

/// 换绑会动到什么（预演与实做共用同一份计数，所以"看到的"就是"会做的"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebindPlan {
    pub from: String,
    pub into: String,
    /// 要按新主体重算 id 的状态行数。
    pub states: usize,
    /// 其中会与目标行撞车、合并掉不长新行的条数。
    pub collapse: usize,
    pub mentions: usize,
    /// 旧锚自己的别名条数（并进新锚）。
    pub aliases: usize,
    /// 指向旧锚的 `memory_mention` 行数。
    pub memory_mentions: usize,
    /// 指向旧锚的 `fact.place_anchor_id` / `fact.speaker_anchor_id` 列数。
    /// 这两格**不参与事实行 id**（见 `keys::compute_fact_id` 的入参），所以换绑是就地改列，
    /// 不需要重算 id，也就不属于 blockers。
    pub fact_refs: usize,
    /// 搬不动的引用，逐条一个闭集词（非空 ⇒ 拒绝 apply）。
    pub blockers: Vec<&'static str>,
}

/// 实做后的读数。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RebindReport {
    pub states_moved: usize,
    pub states_merged: usize,
    pub mentions_moved: usize,
    pub aliases_moved: usize,
    pub memory_mentions_moved: usize,
    /// `fact` 里就地改列搬走的格数（`place_anchor_id` / `speaker_anchor_id`）。
    pub fact_refs_moved: usize,
    /// 旧锚上被解决的歧义告警条数。
    pub attention_resolved: usize,
}

/// 数一列有几行（表名/列名都是本模块内的字面量，不接受外部拼）。
fn count_rows(
    conn: &rusqlite::Connection, table: &str, column: &str, anchor_id: &str,
) -> Result<usize, MemError> {
    let sql = format!("SELECT count(*) FROM {table} WHERE {column}=?1");
    let mut st = conn.prepare(&sql)?;
    let n: i64 = st.query_row([anchor_id], |r| r.get(0))?;
    Ok(n as usize)
}

fn blocked_tables(
    m: &MemoryHandle, from: &str, into: &str,
) -> Result<Vec<&'static str>, MemError> {
    let mut out = Vec::new();
    // fact：id 输入含未落库的 time_text ⇒ 重算不出来（模块文档第 2 条）。
    if count_rows(m.conn(), "fact", "subject_anchor_id", from)? > 0
        || count_rows(m.conn(), "fact", "object_anchor_id", from)? > 0
    {
        out.push("fact_id_inputs_not_stored");
    }
    for (table, column, code) in [
        ("relation", "subject_anchor_id", "relation_unverified_id_rule"),
        ("relation", "object_anchor_id", "relation_unverified_id_rule"),
        ("attribute", "anchor_id", "attribute_unverified_id_rule"),
        ("utterance", "speaker_anchor_id", "utterance_unverified_id_rule"),
        ("utterance", "addressee_anchor_id", "utterance_unverified_id_rule"),
    ] {
        if count_rows(m.conn(), table, column, from)? > 0 && !out.contains(&code) {
            out.push(code);
        }
    }
    // card 的指针是 JSON：只判"指到了这一格"，不解析。
    let cards: i64 = m.conn().query_row(
        "SELECT count(*) FROM card WHERE pointers_json LIKE '%'||?1||'%'",
        [from],
        |r| r.get(0),
    )?;
    if cards > 0 {
        out.push("card_pointers_reference_anchor");
    }
    let _ = into;
    Ok(out)
}

/// 只读预演：校验 + 计数。**一格都不写**。
pub fn rebind_plan(m: &MemoryHandle, from: &str, into: &str) -> Result<RebindPlan, RebindError> {
    if from == into {
        return Err(RebindError::SameAnchor);
    }
    let (Some(src), Some(dst)) = (face(m, from)?, face(m, into)?) else {
        return Err(RebindError::Missing);
    };
    if src.status == "merged" {
        return Err(RebindError::AlreadyMerged);
    }
    if dst.status == "merged" {
        return Err(RebindError::TargetMerged);
    }
    if src.kind != dst.kind {
        return Err(RebindError::KindMismatch);
    }
    if src.scope.kind != dst.scope.kind || src.scope.key != dst.scope.key {
        return Err(RebindError::ScopeMismatch);
    }
    let conn = m.conn();
    let mut states = 0usize;
    let mut collapse = 0usize;
    let mut st = conn
        .prepare(
            "SELECT id, dimension, value_after, evidence_turn_uid, scope_kind, scope_key
               FROM state_assertion WHERE subject_anchor_id=?1",
        )
        .map_err(MemError::from)?;
    let rows = st
        .query_map([from], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })
        .map_err(MemError::from)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(MemError::from)?;
    for (_id, dimension, value, evidence, scope_kind, scope_key) in rows {
        let scope = Scope {
            kind: ScopeKind::parse(&scope_kind).unwrap_or(ScopeKind::Project),
            key: scope_key,
        };
        let next = compute_state_id(into, &dimension, &value, &evidence, &scope);
        states += 1;
        let exists: i64 = conn.query_row(
            "SELECT count(*) FROM state_assertion WHERE id=?1",
            [next.as_str()],
            |r| r.get(0),
        )?;
        if exists > 0 {
            collapse += 1;
        }
    }
    Ok(RebindPlan {
        from: from.to_string(),
        into: into.to_string(),
        states,
        collapse,
        mentions: count_rows(conn, "mention", "anchor_id", from)?,
        aliases: count_rows(conn, "alias", "anchor_id", from)?,
        memory_mentions: count_rows(conn, "memory_mention", "anchor_id", from)?,
        fact_refs: count_rows(conn, "fact", "place_anchor_id", from)?
            + count_rows(conn, "fact", "speaker_anchor_id", from)?,
        blockers: blocked_tables(m, from, into)?,
    })
}

/// 把一行 `(mem_table, mem_id)` 挂的出处/归属搬到新 id，撞车即丢弃（同 turn 同角色是同一条证据）。
fn move_child_rows(
    tx: &rusqlite::Transaction<'_>, table: &str, mem_table: &str, old: &str, new: &str,
) -> Result<(), MemError> {
    tx.execute(
        &format!(
            "INSERT OR IGNORE INTO {table}(mem_table, mem_id, turn_uid, anchor_role, ref_kind, ord, role)
             SELECT ?1, ?2, turn_uid, anchor_role, ref_kind, ord, role FROM {table}
              WHERE mem_table=?3 AND mem_id=?4"
        ),
        rusqlite::params![mem_table, new, mem_table, old],
    )?;
    tx.execute(
        &format!("DELETE FROM {table} WHERE mem_table=?1 AND mem_id=?2"),
        rusqlite::params![mem_table, old],
    )?;
    Ok(())
}

/// 归属表没有 turn 那一列，单独走一遍。
fn move_owner(
    tx: &rusqlite::Transaction<'_>, mem_table: &str, old: &str, new: &str,
) -> Result<(), MemError> {
    tx.execute(
        "INSERT OR IGNORE INTO resource_owner(
             mem_table, mem_id, owner_principal_id, owner_agent_id, created_by_agent,
             visibility, retention, scope_kind, scope_key, source_agent, device_id, created_at_ms)
         SELECT ?1, ?2, owner_principal_id, owner_agent_id, created_by_agent,
                visibility, retention, scope_kind, scope_key, source_agent, device_id, created_at_ms
           FROM resource_owner WHERE mem_table=?3 AND mem_id=?4",
        rusqlite::params![mem_table, new, mem_table, old],
    )?;
    tx.execute(
        "DELETE FROM resource_owner WHERE mem_table=?1 AND mem_id=?2",
        rusqlite::params![mem_table, old],
    )?;
    Ok(())
}

/// 把一条称呼并进新锚，返回真正新插入的行数（`aliases_moved` 的既有口径）。
///
/// 两列**原样带上**：`valid_from_ms` 是这条称呼那次的时间、`source_turn_uid` 是"当初在哪一轮
/// 被说过"。换绑不是它的生产时刻 ⇒ 来源为 NULL 就写 NULL，**不补 `now_ms`**（补了等于伪造
/// 日期，而 `reader::display_name` 按 `COALESCE(valid_from_ms,0) DESC` 取显示名，被并进来的
/// 旧称呼会因此赢过幸存者自己的名字）。
///
/// 撞名（新锚已经有同名别名，PK 是 `(anchor_id, alias)`）时只补 NULL、不覆盖非 NULL——
/// 与 `writers_v4` 重放那条 `WHERE source_turn_uid IS NULL` 同一条纪律。
fn merge_alias(
    tx: &rusqlite::Transaction<'_>,
    into: &str,
    alias: &str,
    valid_from_ms: Option<i64>,
    source_turn_uid: Option<&str>,
) -> Result<usize, MemError> {
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO alias(anchor_id, alias, valid_from_ms, source_turn_uid)
         VALUES(?1,?2,?3,?4)",
        rusqlite::params![into, alias, valid_from_ms, source_turn_uid],
    )?;
    if inserted == 0 {
        // 已有那一行是"先到者"：只把空的补上（旧行没值就什么也不写）。
        tx.execute(
            "UPDATE alias SET source_turn_uid=?3
              WHERE anchor_id=?1 AND alias=?2 AND source_turn_uid IS NULL",
            rusqlite::params![into, alias, source_turn_uid],
        )?;
        tx.execute(
            "UPDATE alias SET valid_from_ms=?3
              WHERE anchor_id=?1 AND alias=?2 AND valid_from_ms IS NULL",
            rusqlite::params![into, alias, valid_from_ms],
        )?;
    }
    Ok(inserted)
}

/// 换绑并落库。**先跑 [`rebind_plan`]，有 blockers 就拒**。
pub fn rebind_apply(
    m: &MemoryHandle,
    from: &str,
    into: &str,
    reason: &str,
    actor: &str,
    now_ms: i64,
) -> Result<RebindReport, RebindError> {
    if m.is_read_only() {
        return Err(RebindError::ReadOnly);
    }
    let plan = rebind_plan(m, from, into)?;
    if !plan.blockers.is_empty() {
        return Err(RebindError::Blocked);
    }
    let tx = m.conn().unchecked_transaction()?;
    let report = rebind_apply_tx(&tx, from, into, reason, actor, now_ms)?;
    // 指向行（独立 rebind 入口专属）：merge_apply 在自己的事务里写 `anchor_merge`（run_id
    // `merge-<now>`），rebind_apply_tx 不能抢——写了会把 merge_apply 的 `WHERE NOT EXISTS`
    // 兜底挡掉、审计行对不上它的 run_id。独立入口没有那条兜底，重键搬空引用后旧锚缺指向
    // 行 ⇒ health scan 报 no_redirect、resolve 走 Broken→self 回退。环检查与
    // `redirect::merge_redirect` 同一条：沿 into 的出链走，走回 from 就拒绝（事务未提交，
    // 整笔回滚）。
    {
        let mut visited: Vec<String> = vec![from.to_string()];
        let mut current = into.to_string();
        loop {
            if visited.contains(&current) {
                return Err(RebindError::RedirectCycle);
            }
            visited.push(current.clone());
            match crate::redirect::latest_redirect(&tx, &current)? {
                None => break,
                Some(next) => current = next,
            }
        }
    }
    tx.execute(
        "INSERT INTO anchor_merge(from_id, to_id, run_id, reason, created_at_ms)
         SELECT ?1, ?2, ?3, ?4, ?5
          WHERE NOT EXISTS(SELECT 1 FROM anchor_merge WHERE from_id=?1 AND to_id=?2)",
        rusqlite::params![from, into, format!("rebind-{now_ms}"), reason, now_ms],
    )?;
    tx.commit()?;
    Ok(report)
}

/// 换绑的**事务内**半边：调用方已经跑过 [`rebind_plan`]、并负责提交。
///
/// 抽出它只为一件事：`hub anchor merge` 要把"搬引用 + 写 `anchor_merge` 审计行"放**同一个**
/// 事务里（合并成功但审计行丢失，等于事后无法回答"这一格为什么不见了"）。本函数不取钟、
/// 不开事务、不提交，校验仍由调用方的 plan 负责——与 `rebind_apply` 的入口校验逐字相同。
pub(crate) fn rebind_apply_tx(
    tx: &rusqlite::Transaction<'_>,
    from: &str,
    into: &str,
    reason: &str,
    actor: &str,
    now_ms: i64,
) -> Result<RebindReport, RebindError> {
    let src_canonical: String = tx.query_row(
        "SELECT canonical FROM anchor WHERE id=?1",
        [from],
        |r| r.get(0),
    )?;
    let mut report = RebindReport::default();

    // 1) 状态行：按新主体重算 id ⇒ 撞车就合并、否则原地换 id，出处/归属跟着搬。
    //     读必须关在内层块里、并且**先绑定再返回**：`query_and_then` 的行借用了
    //     `Statement`，块尾表达式带着这个借用出来就是 E0597，而块外留着
    //     `Statement` 又会让 `tx.commit()` 撞 E0505。两头都要，就得中间落一个 `let got`。
    let states = {
        let mut state_stmt = tx.prepare(
        "SELECT id, dimension, value_after, evidence_turn_uid, scope_kind, scope_key
           FROM state_assertion WHERE subject_anchor_id=?1",
        )?;
        let got = state_stmt
            .query_and_then([from], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
            })?
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;
        got
    };
    for (id, dimension, value, evidence, scope_kind, scope_key) in states {
        let scope = Scope {
            kind: ScopeKind::parse(&scope_kind).unwrap_or(ScopeKind::Project),
            key: scope_key,
        };
        let next = compute_state_id(into, &dimension, &value, &evidence, &scope);
        let exists: i64 = tx.query_row(
            "SELECT count(*) FROM state_assertion WHERE id=?1",
            [next.as_str()],
            |r| r.get(0),
        )?;
        if exists > 0 || next == id {
            report.states_merged += usize::try_from(i64::from(exists > 0)).unwrap_or(0);
            move_child_rows(tx, "provenance", "state_assertion", &id, &next)?;
            move_owner(tx, "state_assertion", &id, &next)?;
            tx.execute(
                "DELETE FROM memory_mention WHERE mem_table='state_assertion' AND mem_id=?1",
                [id.as_str()],
            )?;
            tx.execute("DELETE FROM state_assertion WHERE id=?1", [id.as_str()])?;
        } else {
            tx.execute(
                "UPDATE state_assertion SET id=?2, subject_anchor_id=?3 WHERE id=?1",
                rusqlite::params![id, next, into],
            )?;
            move_child_rows(tx, "provenance", "state_assertion", &id, &next)?;
            move_owner(tx, "state_assertion", &id, &next)?;
            tx.execute(
                "UPDATE memory_mention SET mem_id=?2 WHERE mem_table='state_assertion' AND mem_id=?1",
                rusqlite::params![id, next],
            )?;
            tx.execute(
                "UPDATE state_assertion SET superseded_by=?2 WHERE superseded_by=?1",
                rusqlite::params![id, next],
            )?;
            report.states_moved += 1;
        }
    }

    // 2) 轮级指称：id 含 anchor ⇒ 同样重算。
    let mentions = {
        let mut mention_stmt = tx.prepare(
        "SELECT id, turn_uid, surface, span_start, span_end FROM mention WHERE anchor_id=?1",
        )?;
        let got = mention_stmt
            .query_and_then([from], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<i64>>(4)?,
            ))
            })?
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;
        got
    };
    for (id, turn_uid, surface, start, end) in mentions {
        let next = compute_mention_id(&turn_uid, into, &surface, start, end);
        let exists: i64 = tx.query_row(
            "SELECT count(*) FROM mention WHERE id=?1",
            [next.as_str()],
            |r| r.get(0),
        )?;
        if exists > 0 {
            tx.execute("DELETE FROM mention WHERE id=?1", [id.as_str()])?;
        } else {
            tx.execute(
                "UPDATE mention SET id=?2, anchor_id=?3 WHERE id=?1",
                rusqlite::params![id, next, into],
            )?;
            report.mentions_moved += 1;
        }
    }

    // 3) 别名与旧正名并进新锚（"称呼"活下来的地方）。
    //    旧行按 `(alias, valid_from_ms, source_turn_uid)` 读出：并进去时两列**原样带上**，
    //    来源为 NULL 就写 NULL——不许补 `now_ms`（见 [`merge_alias`]）。
    let aliases = {
        let mut alias_stmt = tx.prepare(
            "SELECT alias, valid_from_ms, source_turn_uid FROM alias WHERE anchor_id=?1",
        )?;
        let got = alias_stmt
            .query_and_then([from], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;
        got
    };
    // 旧正名的出处：旧锚在 `provenance` 里**最早**的那一轮。必须在第 5 步把 provenance
    // 搬到新锚**之前**查（搬完就分不出哪些轮属于旧锚了）；查不到就 NULL。
    let src_turn: Option<String> = tx
        .query_row(
            "SELECT turn_uid FROM provenance
              WHERE mem_table='anchor' AND mem_id=?1 ORDER BY rowid LIMIT 1",
            [from],
            |r| r.get(0),
        )
        .optional()?;
    for (alias, valid_from, source_turn) in aliases {
        report.aliases_moved +=
            merge_alias(tx, into, &alias, valid_from, source_turn.as_deref())?;
    }
    // 旧正名也并成别名，但它是"称呼"不是"日期"：`valid_from_ms=NULL`（A1 别名那条
    // `writers_v4` 不写这一列；主人可读名那条会写，但这里不发明日期），出处取旧锚最早的那一轮。
    report.aliases_moved += merge_alias(tx, into, &src_canonical, None, src_turn.as_deref())?;
    tx.execute("DELETE FROM alias WHERE anchor_id=?1", [from])?;

    // 4) 无 FK 保护的那一格：memory_mention 指向旧锚的行全部改指新锚（撞 PK 就丢）。
    let mm = {
        let mut mm_stmt = tx.prepare(
            "SELECT mem_table, mem_id FROM memory_mention WHERE anchor_id=?1",
        )?;
        let got = mm_stmt
            .query_and_then([from], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;
        got
    };
    for (mem_table, mem_id) in mm {
        tx.execute(
            "INSERT OR IGNORE INTO memory_mention(
                 mem_table, mem_id, anchor_id, source_agent, device_id, created_at_ms, norm_version)
             SELECT ?1, ?2, ?3, source_agent, device_id, created_at_ms, norm_version
               FROM memory_mention WHERE mem_table=?4 AND mem_id=?5 AND anchor_id=?6",
            rusqlite::params![mem_table, mem_id, into, mem_table, mem_id, from],
        )?;
        tx.execute(
            "DELETE FROM memory_mention WHERE mem_table=?1 AND mem_id=?2 AND anchor_id=?3",
            rusqlite::params![mem_table, mem_id, from],
        )?;
        report.memory_mentions_moved += 1;
    }

    // 4b) `fact` 的地点/说话人两格不参与事实行 id ⇒ **就地改列**，不重键、不动它的出处。
    //     （主体/客体参与 id 且 id 输入未全部落库 ⇒ 那两格在 blockers 里，走不到这一步。）
    for column in ["place_anchor_id", "speaker_anchor_id"] {
        report.fact_refs_moved += tx.execute(
            &format!("UPDATE fact SET {column}=?1 WHERE {column}=?2"),
            rusqlite::params![into, from],
        )?;
    }

    // 5) 旧锚自己的出处与归属并进新锚（它被谁在哪轮命名过，新锚要能追）。
    move_child_rows(tx, "provenance", "anchor", from, into)?;
    move_owner(tx, "anchor", from, into)?;
    report.attention_resolved += tx.execute(
        "UPDATE attention SET resolved_at_ms=?1 WHERE mem_table='anchor' AND mem_id=?2
           AND resolved_at_ms IS NULL",
        rusqlite::params![now_ms, from],
    )?;

    // 6) 盖章 + 台账。身份三格（kind/canonical/scope）一格不动：merged 锚仍是它自己。
    tx.execute(
        "UPDATE anchor SET status='merged', updated_at_ms=?1 WHERE id=?2",
        rusqlite::params![now_ms, from],
    )?;
    tx.execute(
        "INSERT INTO memory_op(run_id, op, mem_table, mem_id, before_json, after_json,
                              reason, actor, created_at_ms)
         VALUES(NULL,'MERGE','anchor',?1,?2,?3,?4,?5,?6)",
        rusqlite::params![
            from,
            format!("{{\"canonical\":\"{src_canonical}\",\"anchor_id\":\"{from}\"}}"),
            format!("{{\"into\":\"{into}\"}}"),
            reason,
            actor,
            now_ms,
        ],
    )?;
    Ok(report)
}
