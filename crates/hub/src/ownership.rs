//! 旧数据回填（S1b-C4b2a）：给已有的记忆行补 `resource_owner`，顺手清掉孤儿归属行。
//!
//! 设计依据 `design/2026-10-26-ownership-persistence.md` §6、§8-C4。三条纪律：
//!
//! ① **不跨库 JOIN**：记忆行/出处从 memory.db 读，会话归属从 transcript.db 读，
//!    两次独立查询、中间不 ATTACH；transcript 侧按 `turn_uid` 分块（≤[`SESSION_CHUNK`] 个/块），
//!    因为 SQLite 的绑定变量数有上限。
//! ② **不覆盖已有归属**：`owner::insert` 是 `INSERT OR IGNORE`（先写者赢），本模块在写之前
//!    就按 `resource_owner` 的内存集合跳过已有行 ⇒ 重跑 `owners_written=0`、`memory_op` 不重写。
//! ③ **推不出就落哨兵，不写 NULL**：`owner_agent_id = 行 source_agent`（anchor 没有该列 ⇒
//!    字面量 `unknown`），`visibility=private_agent`、`retention=default`、scope 取行，并写
//!    `memory_op(op='NOOP', reason='owner_backfill_unresolved', actor='program')`。
//!    读闸（`rc_memory::gate`）因此永远能判：哨兵是一个真实 agent 名，走"owner 之外不可见"。
//!    `owner_agent_id` 是 NOT NULL，写 NULL 会把"未知"摊给每个读者各解释一遍。
//!
//! 归属只从 `provenance.turn_uid → turns → sessions` 推；载荷外的任何来源（行标题、目录名）
//! 都不是权限来源。恰好一个身份（`(owner_principal_id, owner_agent_id)`）才算推导成功：
//! 跨两个身份的行不挑、不合并——静默合并等于把 A 的内容算给 B。
//!
//! **本切片不做**快照 prune、不做 export edges 过滤（那是 C4b2b）。
use rc_memory::owner::{self, OwnerRow, Visibility};
use rc_memory::Scope;
use rc_store::{MemoryHandle, Root, StoreError, TranscriptHandle};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// 一次 transcript 查询的 `turn_uid` 个数上限。500 是保守的一档（与 `hub::rows` 同口径）。
const SESSION_CHUNK: usize = 500;

/// 回填的读数（CLI 的文本行与 `--json` 是同一组字段）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackfillReport {
    /// `card` + `anchor` 的行数（已归属的也算）。
    pub rows_scanned: usize,
    /// 已有归属行、本次跳过的行数。
    pub already_owned: usize,
    /// 本次真正写下的归属行数（含未决的哨兵行）。
    pub owners_written: usize,
    /// 本次判为推不出、落哨兵并记账的行数。
    pub unresolved: usize,
    /// 删掉的孤儿归属行数（`(card|anchor, id)` 已不存在的行）。
    pub orphans_removed: usize,
}

/// 记忆行回填所需的那几列（`anchor` 没有 `source_agent`/`device_id` ⇒ 两格恒 `None`）。
#[derive(Debug, Clone)]
struct MemRow {
    mem_table: &'static str,
    mem_id: String,
    scope_kind: String,
    scope_key: String,
    source_agent: Option<String>,
    device_id: Option<String>,
    created_at_ms: i64,
}

/// 一个 turn 所属会话的归属读数（NULL 已在 SQL 里 COALESCE）。
#[derive(Debug, Clone)]
struct SessionOwner {
    owner_agent: String,
    owner_principal_id: Option<String>,
    visibility: String,
    retention: String,
    scope_key: String,
    source_agent: String,
}

/// 可见性的保守序（closed set）。`rc_memory::owner::rank` 是那边的私有实现，
/// hub 只在**取最小**这一处按枚举顺序过一遍——不复制解析与落库字面量（那是 `owner` 的事）。
const VISIBILITY_BY_CONSERVATISM: [Visibility; 5] = [
    Visibility::PrivateAgent,
    Visibility::Person,
    Visibility::Project,
    Visibility::Org,
    Visibility::Public,
];

/// 取最保守的一档。`parse_visibility` 只产出这 5 个值 ⇒ 恒能命中一格；
/// 兜底 `private_agent` 只是为了让"以后有人加变体"编译得过、且方向仍是收紧。
fn most_conservative(values: &[Visibility]) -> Visibility {
    for candidate in VISIBILITY_BY_CONSERVATISM {
        if values.contains(&candidate) {
            return candidate;
        }
    }
    Visibility::PrivateAgent
}

/// 行集：`card` + `anchor`（设计 §6 第 1 段）。`ORDER BY` 固定可观测顺序；
/// 列序与 [`MemRow`] 一一对应，表名用字面量 `'card'`/`'anchor'` 而不是运行时拼串。
fn mem_rows(m: &MemoryHandle) -> Result<Vec<MemRow>, StoreError> {
    let mut st = m.conn().prepare(
        "SELECT 'card' AS t, id, scope_kind, scope_key, source_agent, device_id, created_at_ms
           FROM card
         UNION ALL
         SELECT 'anchor', id, scope_kind, scope_key, NULL, NULL, created_at_ms
           FROM anchor
         ORDER BY 1, 2",
    )?;
    let rows = st
        .query_map([], |r| {
            let table: String = r.get(0)?;
            Ok(MemRow {
                mem_table: if table == "card" { "card" } else { "anchor" },
                mem_id: r.get(1)?,
                scope_kind: r.get(2)?,
                scope_key: r.get(3)?,
                source_agent: r.get(4)?,
                device_id: r.get(5)?,
                created_at_ms: r.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 已有归属的 `(mem_table, mem_id)` 集合：判"要不要补"只看这两列。
fn existing_owners(m: &MemoryHandle) -> Result<HashSet<(String, String)>, StoreError> {
    let mut st = m
        .conn()
        .prepare("SELECT mem_table, mem_id FROM resource_owner")?;
    let rows = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<HashSet<_>, _>>()?;
    Ok(rows)
}

/// 出处（设计 §6 第 1 段）：`(mem_table, mem_id) → {turn_uid}`。
/// 只认 `ref_kind='turn'` 与 `card`/`anchor` 两张表——别的 ref_kind 不是会话出处。
fn provenance_turns(
    m: &MemoryHandle,
) -> Result<BTreeMap<(String, String), BTreeSet<String>>, StoreError> {
    let mut st = m.conn().prepare(
        "SELECT mem_table, mem_id, turn_uid FROM provenance
          WHERE ref_kind='turn' AND mem_table IN ('card','anchor')",
    )?;
    let mut out: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        out.entry((r.get(0)?, r.get(1)?)).or_default().insert(r.get(2)?);
    }
    Ok(out)
}

/// 会话（设计 §6 第 2 段，transcript.db 一侧）。按 `turn_uid` 分块（≤500/块）：
/// 查不到会话的 uid 不出现在结果里，由调用方走未决。
fn session_owners(
    t: &TranscriptHandle,
    turns: &BTreeSet<String>,
) -> Result<HashMap<String, SessionOwner>, StoreError> {
    let mut out = HashMap::new();
    let uids: Vec<&str> = turns.iter().map(String::as_str).collect();
    for chunk in uids.chunks(SESSION_CHUNK) {
        let slots: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT t.turn_uid,
                    COALESCE(s.owner_agent_id, s.source_agent) AS owner_agent,
                    s.owner_principal_id,
                    COALESCE(s.visibility,'private_agent') AS visibility,
                    COALESCE(s.retention,'default') AS retention,
                    COALESCE(s.scope_key, s.project_hint, s.session_key) AS scope_key,
                    s.source_agent
               FROM turns t JOIN sessions s
                 ON s.session_key=t.session_key AND s.device_id=t.device_id
              WHERE t.turn_uid IN ({})",
            slots.join(",")
        );
        let mut st = t.conn().prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(chunk.iter().copied()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                SessionOwner {
                    owner_agent: r.get(1)?,
                    owner_principal_id: r.get(2)?,
                    visibility: r.get(3)?,
                    retention: r.get(4)?,
                    scope_key: r.get(5)?,
                    source_agent: r.get(6)?,
                },
            ))
        })?;
        for row in rows {
            let (uid, owner) = row?;
            out.insert(uid, owner);
        }
    }
    Ok(out)
}

/// 可推导的行：owner 取那一个会话身份；`visibility` 取各会话 `effective(...)` 的最保守档；
/// `retention` 各会话一致才用、否则 `default`；`created_by_agent` 取行的 `source_agent`
/// （anchor 没有该列 ⇒ 第一个会话的）；`scope` 取行，`scope_key` 空才退到会话的；
/// `source_agent`/`device_id` 是行的两列（anchor ⇒ NULL）。
fn derived_row(row: &MemRow, sessions: &[&SessionOwner]) -> OwnerRow {
    let kind = owner::normalize_scope_kind(&row.scope_kind);
    let effective: Vec<Visibility> = sessions
        .iter()
        .map(|s| owner::effective(owner::parse_visibility(&s.visibility), kind))
        .collect();
    let retentions: BTreeSet<String> = sessions
        .iter()
        .map(|s| owner::normalize_retention(&s.retention))
        .collect();
    let retention = if retentions.len() == 1 {
        retentions.into_iter().next().unwrap_or_else(|| "default".to_string())
    } else {
        "default".to_string()
    };
    // 身份唯一 ⇒ 每个字段都来自同一组 (principal, agent)；agent 取第一个会话的那一格。
    let first = sessions[0];
    let created_by = row
        .source_agent
        .clone()
        .unwrap_or_else(|| first.source_agent.clone());
    OwnerRow {
        mem_table: row.mem_table.to_string(),
        mem_id: row.mem_id.clone(),
        owner_principal_id: first.owner_principal_id.clone(),
        owner_agent_id: first.owner_agent.clone(),
        created_by_agent: created_by,
        visibility: most_conservative(&effective),
        retention,
        scope: Scope {
            kind,
            key: if row.scope_key.is_empty() {
                first.scope_key.clone()
            } else {
                row.scope_key.clone()
            },
        },
        source_agent: row.source_agent.clone(),
        device_id: row.device_id.clone(),
        created_at_ms: row.created_at_ms,
    }
}

/// 推不出的行：`owner_agent_id = 行 source_agent`，没有该列（anchor）才是 `unknown`；
/// principal 为空，`private_agent` + `default`，scope 取行。
/// 探测用的不是"NULL 表示未知"：哨兵是一个真实取值，读闸因此可判定、操作员可 grep。
fn unresolved_row(row: &MemRow) -> OwnerRow {
    let agent = row
        .source_agent
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    OwnerRow {
        mem_table: row.mem_table.to_string(),
        mem_id: row.mem_id.clone(),
        owner_principal_id: None,
        owner_agent_id: agent.clone(),
        created_by_agent: agent,
        visibility: Visibility::PrivateAgent,
        retention: "default".to_string(),
        scope: Scope {
            kind: owner::normalize_scope_kind(&row.scope_kind),
            key: row.scope_key.clone(),
        },
        source_agent: row.source_agent.clone(),
        device_id: row.device_id.clone(),
        created_at_ms: row.created_at_ms,
    }
}

fn store_err(e: rc_memory::MemError) -> StoreError {
    StoreError::Sql(e.to_string())
}

/// 回填主体。顺序是判定的一部分：
/// ① 读行集/已有归属/出处（memory.db 两次查询）；② 读会话（transcript.db，分块）；
/// ③ **一个** memory 事务写缺的归属行（未决行连 memory_op 一起）；④ 同事务删孤儿归属行；
/// ⑤ 提交后把 `unresolved` 记进 `maintenance:ownership:ownership_unresolved`。
///
/// ④ 放在 ③ 之后：本次新写的行当然不是孤儿；放在同一事务里是为了"要么都生效、要么都不"。
/// ⑤ 在提交之后：行已落地，记账失败只让退出码非 0，重跑幂等（不会重复写 owner/memory_op）。
pub fn backfill(root: &mut Root, now_ms: i64) -> Result<BackfillReport, StoreError> {
    let rows = mem_rows(root.memory())?;
    let owned = existing_owners(root.memory())?;
    let provenance = provenance_turns(root.memory())?;

    // 两次独立查询之间不持有 memory 的事务：transcript 这一跳在事务之外完成。
    let turns: BTreeSet<String> = provenance.values().flatten().cloned().collect();
    let sessions = session_owners(root.transcript(), &turns)?;

    let mut report = BackfillReport {
        rows_scanned: rows.len(),
        ..BackfillReport::default()
    };

    let tx = root.memory().conn().unchecked_transaction()?;
    for row in &rows {
        let key = (row.mem_table.to_string(), row.mem_id.clone());
        if owned.contains(&key) {
            // 不覆盖：已有归属行是权限事实，回填只补缺。
            report.already_owned += 1;
            continue;
        }
        // 这一行 provenance 指到的会话（解析不到某个 uid 就只是少一格，不报错）。
        let resolved: Vec<&SessionOwner> = provenance
            .get(&key)
            .map(|uids| uids.iter().filter_map(|uid| sessions.get(uid)).collect())
            .unwrap_or_default();
        // 身份 = `(principal, agent)`：同一身份的多会话（多轮）只算一个 owner。
        let identities: HashSet<(Option<String>, String)> = resolved
            .iter()
            .map(|s| (s.owner_principal_id.clone(), s.owner_agent.clone()))
            .collect();
        let unique = identities.len() == 1 && !resolved.is_empty();
        let owner_row = if unique {
            derived_row(row, &resolved)
        } else {
            unresolved_row(row)
        };

        let written = owner::insert(&tx, &owner_row).map_err(store_err)?;
        if !written {
            // 已有行（读集合之后被别的写者插进来）⇒ 幂等命中，不算本次新写、也不记账。
            report.already_owned += 1;
            continue;
        }
        report.owners_written += 1;
        if !unique {
            // 未决的账与归属行同一个事务：不会出现"有哨兵行、没有记账"的半格。
            tx.execute(
                "INSERT INTO memory_op(op, mem_table, mem_id, before_json, after_json, reason,
                                      actor, created_at_ms)
                 VALUES('NOOP', ?1, ?2, NULL, NULL, 'owner_backfill_unresolved', 'program', ?3)",
                rusqlite::params![row.mem_table, row.mem_id, now_ms],
            )?;
            report.unresolved += 1;
        }
    }

    // 孤儿清理：只扫 card/anchor 两张表。其它表的归属行今天可能本来就没有事实行与之对应，
    // 它们不在本切片的判定面里（不猜、不删）。
    report.orphans_removed = tx.execute(
        "DELETE FROM resource_owner
          WHERE (mem_table='card'   AND mem_id NOT IN (SELECT id FROM card))
             OR (mem_table='anchor' AND mem_id NOT IN (SELECT id FROM anchor))",
        [],
    )?;
    tx.commit()?;

    if report.unresolved > 0 {
        // 失败 ⇒ StoreError：行已提交，重跑幂等（第二次 unresolved=0，不会再 bump）。
        rc_ledger::counters::bump(
            root.ledger_mut(),
            "maintenance",
            "ownership",
            "ownership_unresolved",
            report.unresolved as i64,
            now_ms,
        )?;
    }
    Ok(report)
}
