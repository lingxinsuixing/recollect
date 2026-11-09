//! W37-A2 / G13④：存量缺章记忆行的**带账回填**（设计 `2026-11-07-w37-readgate-design.md`
//! §4"存量 226 行处置"）。
//!
//! 断点回顾：蒸馏继承在 G13② 修复前裸读 `sessions.owner_principal_id` 列，章后补的会话
//! 蒸出的行在 `resource_owner.owner_principal_id` 上**永久缺章**（存量 226 行 ≈18%）
//! ⇒"同主人互读"对这些老记忆失效。回填补的是**归属元数据**不是内容：
//!
//! ① **章才是真相**：每行的主人从 `provenance.turn_uid → turns → 会话` 经
//!    [`rc_ledger::transcript::resolve_principal`] 求得，且**只认章**（`claim.is_some()`
//!    且过 [`rc_memory::writers_v4::principal_is_known`]）——列上的回落值正是这一轮要补的
//!    缺口，拿它当真相等于自己抄自己；
//! ② **不挑不猜**：恰好一个身份才回填；无出处 / 出处会话无章 / 跨 ≥2 个章的行原样留着
//!    （`unresolved` 计数）——静默合并等于把 A 的内容算给 B（与 `hub::ownership` 同一条律）；
//! ③ **带账**：每写一行归属，同一事务里写一条
//!    `memory_op(op='UPDATE', reason='principal_backfill', actor='program')`，
//!    `before_json`/`after_json` 记旧值与新值——与 A9 的"回拨 + 记账"同形，事后能查
//!    是哪一波补的、补成了什么；
//! ④ **幂等**：UPDATE 带"仍是缺章值才写"的守卫（`IS NULL OR '' OR 'unknown'`），
//!    回填过的行第二轮扫不到、`memory_op` 不重写；并发写者先落地时守卫落空、不计账。
//!
//! **默认 dry-run，一格不写**（CLI 不给 `--apply`）：扫全量、报数、零写入——
//! 回执先给操作员过目，真执行是部署后控制器的事。归属行整行缺席的旧行不归本工具
//! （那是 `hub ownership backfill` 的既有面）。
use rc_ledger::transcript::resolve_principal;
use rc_memory::writers_v4::principal_is_known;
use rc_store::{Root, StoreError};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// 一次 transcript 查询的 `turn_uid` 个数上限（与 `hub::ownership` 同一保守档）。
const TURN_CHUNK: usize = 500;

/// 回填的读数（CLI 的文本行与 `--json` 是同一组字段）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackfillReport {
    /// 扫到的缺章行数（`resource_owner.owner_principal_id` 为 NULL/空白/`unknown` 的
    /// card/anchor/fact/state_assertion 行）。
    pub rows_scanned: usize,
    /// 本次真正写了归属与记账的行数（dry-run 恒 0）。
    pub backfilled: usize,
    /// 推不出唯一章的行数（无出处 / 出处会话无章 / 跨 ≥2 个章）：不挑不猜，原样留着。
    pub unresolved: usize,
    pub dry_run: bool,
}

/// 一条回填明细（回执逐行列出；dry-run 列的是"将会写什么"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfilledRow {
    pub mem_table: String,
    pub mem_id: String,
    pub principal_id: String,
}

/// 缺章行的读出面（`before` 是扫描时的旧值：NULL 或 `unknown`/空白字面量）。
struct MissingRow {
    mem_table: String,
    mem_id: String,
    before: Option<String>,
}

/// 缺章行集：四张记忆表（card/anchor/fact/state_assertion——闸不分表，回填同口径），
/// 归属行的主人一格是 NULL/空白/`unknown`。
/// `ORDER BY` 固定可观测顺序；列序与 [`MissingRow`] 一一对应。
fn missing_rows(m: &rc_store::MemoryHandle) -> Result<Vec<MissingRow>, StoreError> {
    let mut st = m.conn().prepare(
        "SELECT ro.mem_table, ro.mem_id, ro.owner_principal_id
           FROM resource_owner ro
          WHERE ro.mem_table IN ('card','anchor','fact','state_assertion')
            AND (ro.owner_principal_id IS NULL
                 OR TRIM(ro.owner_principal_id) = ''
                 OR ro.owner_principal_id = 'unknown')
          ORDER BY ro.mem_table, ro.mem_id",
    )?;
    let rows = st
        .query_map([], |r| {
            Ok(MissingRow {
                mem_table: r.get::<_, String>(0)?,
                mem_id: r.get(1)?,
                before: r.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 出处：`(mem_table, mem_id) → {turn_uid}`。只认 `ref_kind='turn'` 的会话出处
/// （与 `hub::ownership` 同一条判据——别的 ref_kind 不是会话出处）。
fn provenance_turns(
    m: &rc_store::MemoryHandle,
) -> Result<BTreeMap<(String, String), BTreeSet<String>>, StoreError> {
    let mut st = m.conn().prepare(
        "SELECT mem_table, mem_id, turn_uid FROM provenance
          WHERE ref_kind='turn' AND mem_table IN ('card','anchor','fact','state_assertion')",
    )?;
    let mut out: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        out.entry((r.get(0)?, r.get(1)?)).or_default().insert(r.get(2)?);
    }
    Ok(out)
}

/// 出处 turn 所属的会话（transcript.db 一侧，跨库不 JOIN：先按 uid 分块取键）。
fn turn_sessions(
    t: &rc_store::TranscriptHandle,
    turns: &BTreeSet<String>,
) -> Result<HashMap<String, (String, String)>, StoreError> {
    let mut out = HashMap::new();
    let uids: Vec<&str> = turns.iter().map(String::as_str).collect();
    for chunk in uids.chunks(TURN_CHUNK) {
        let slots: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT turn_uid, session_key, device_id FROM turns WHERE turn_uid IN ({})",
            slots.join(",")
        );
        let mut st = t.conn().prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(chunk.iter().copied()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (uid, session_key, device_id) = row?;
            out.insert(uid, (session_key, device_id));
        }
    }
    Ok(out)
}

/// 回填主体。顺序是判定的一部分：memory.db 两次读（缺章行、出处）→ transcript.db
/// 一次取键、逐会话 `resolve_principal`（读侧唯一入口，不跨库 JOIN）→ 判定 →
/// **一个** memory 事务里逐行"守卫 UPDATE + memory_op 记账"。
///
/// dry-run（`apply=false`）走到判定为止，一格不写；`rows` 交回的是"将会写什么"。
pub fn backfill_principals(
    root: &Root,
    now_ms: i64,
    apply: bool,
) -> Result<(BackfillReport, Vec<BackfilledRow>), StoreError> {
    let rows = missing_rows(root.memory())?;
    let provenance = provenance_turns(root.memory())?;

    // transcript 侧在 memory 事务之外完成（不跨库 JOIN，也不跨库持事务）。
    let turns: BTreeSet<String> = provenance.values().flatten().cloned().collect();
    let sessions = turn_sessions(root.transcript(), &turns)?;
    // 每个会话只解析一次：出处多轮同会话是常态，解析规则只许跑一遍。
    let mut claimed: HashMap<(String, String), Option<String>> = HashMap::new();
    for (session_key, device_id) in sessions.values() {
        let key = (session_key.clone(), device_id.clone());
        if claimed.contains_key(&key) {
            continue;
        }
        let principal = resolve_principal(root.transcript(), session_key, device_id)?
            .filter(|rp| rp.claim.is_some() && principal_is_known(&rp.principal_id))
            .map(|rp| rp.principal_id);
        claimed.insert(key, principal);
    }

    let mut report = BackfillReport { rows_scanned: rows.len(), dry_run: !apply, ..Default::default() };
    let mut written: Vec<BackfilledRow> = Vec::new();
    let mut plan: Vec<(&MissingRow, String)> = Vec::new();
    for row in &rows {
        let key = (row.mem_table.to_string(), row.mem_id.clone());
        let principals: HashSet<String> = provenance
            .get(&key)
            .map(|uids| {
                uids.iter()
                    .filter_map(|uid| sessions.get(uid))
                    .filter_map(|k| claimed.get(k).cloned().flatten())
                    .collect()
            })
            .unwrap_or_default();
        match principals.len() {
            1 => {
                let principal_id = principals.into_iter().next().unwrap_or_default();
                plan.push((row, principal_id));
            }
            _ => report.unresolved += 1, // 0 个章或 ≥2 个章：不挑不猜，原样留着
        }
    }

    if apply {
        let tx = root.memory().conn().unchecked_transaction()?;
        for (row, principal_id) in &plan {
            // 守卫 UPDATE：仍是缺章值才写 ⇒ 重跑/并发先落地都不重写。
            let n = tx.execute(
                "UPDATE resource_owner SET owner_principal_id=?1
                  WHERE mem_table=?2 AND mem_id=?3
                    AND (owner_principal_id IS NULL
                         OR TRIM(owner_principal_id) = ''
                         OR owner_principal_id = 'unknown')",
                rusqlite::params![principal_id, row.mem_table, row.mem_id],
            )?;
            if n == 0 {
                continue; // 已被别的写者补上：幂等命中，不计数也不记账
            }
            let before = json_principal(row.before.as_deref());
            tx.execute(
                "INSERT INTO memory_op(op, mem_table, mem_id, before_json, after_json,
                                      reason, actor, created_at_ms)
                 VALUES('UPDATE', ?1, ?2, ?3, ?4, 'principal_backfill', 'program', ?5)",
                rusqlite::params![
                    row.mem_table,
                    row.mem_id,
                    before,
                    json_principal(Some(principal_id)),
                    now_ms
                ],
            )?;
            report.backfilled += 1;
            written.push(BackfilledRow {
                mem_table: row.mem_table.to_string(),
                mem_id: row.mem_id.clone(),
                principal_id: principal_id.clone(),
            });
        }
        tx.commit()?;
    } else {
        // dry-run：判定面照走，回执照给，只是零写入。
        for (row, principal_id) in &plan {
            written.push(BackfilledRow {
                mem_table: row.mem_table.to_string(),
                mem_id: row.mem_id.clone(),
                principal_id: principal_id.clone(),
            });
        }
    }
    Ok((report, written))
}

/// before/after JSON 的那一格：只记主人一个键（旧值缺章时是 `null`，不是省键——
/// "旧值没有"与"没记旧值"在台账里是两件事）。
fn json_principal(value: Option<&str>) -> String {
    serde_json::json!({ "owner_principal_id": value }).to_string()
}

// ───────────── W37 归一化：归属串 remap ─────────────
//
// 背景见设计 `2026-11-07-w37-readgate-design.md` 与台账 11-07 身份归一化段：章里的
// `principal_id` 曾被操作配置写成了可读名（`示例主人`），它过不了 MCP `--principal-id` 的
// safe_token 尺（设计明写与 `--agent-id` 同尺），也跨不过 agent（hermes 盖的是
// `human:<uuid>`，与 qoder/dsh 的字符串永不相等，4b 在章这一层就死）。归一化 =
// 章补 correction（`hub principal claim`）→ **归属串 remap（本节）** → 锚换绑
// （`hub anchor rebind`，机制 D27）→ MCP 启动项带上新 id。
//
// 与 [`backfill_principals`] 的分工：backfill 只补"缺"（NULL/空/`unknown`），remap 改
// "错"（已写但写成了旧 id）。两者共用同一条守卫 UPDATE + `memory_op` 记账的形状，
// reason 分开写（`principal_backfill` / `principal_remap`），事后能分清哪一波动了哪格。

/// remap 的读数（CLI 的文本行与 `--json` 是同一组字段）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemapReport {
    /// 扫到的旧 id 行数（`resource_owner.owner_principal_id = from`，全 mem_table）。
    pub rows_matched: usize,
    /// 本次真正改写并记账的行数（dry-run 恒 0）。
    pub remapped: usize,
    /// 确保过的新主人锚数（按受影响锚行触到的 scope 逐格
    /// [`rc_memory::writers_v4::ensure_principal_anchor`]；已存在的格幂等命中也计入）。
    pub anchors_ensured: usize,
    /// 触及的 scope（`kind:key`，升序）。
    pub scopes: Vec<String>,
    pub dry_run: bool,
}

/// 一条 remap 明细（回执逐行列出；dry-run 列的是"将会写什么"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemappedRow {
    pub mem_table: String,
    pub mem_id: String,
}

/// 归属串 remap 主体。`from`/`to` 的形状校验（非空、`from != to`、`to` 是调用方认定的
/// 稳定 id）在 CLI 层做——这一层只负责"扫 → 确保新锚 → 守卫 UPDATE + 记账"三步。
///
/// 新主人锚在 remap **之前**确保：rebind（下一步）要求 `--into` 已存在，而锚的
/// canonical/alias/归属行规则只在 [`rc_memory::writers_v4::ensure_principal_anchor`]
/// 有一份实现，这里不抄第二遍。锚的归属上下文从**被替换的旧锚归属行**照抄
/// （owner_agent/visibility/retention 都是每 scope 现读的），让新锚继承旧锚的归属形状；
/// 可读名经 `principal_name` 进 alias。dry-run 三步全部只读。
pub fn remap_principals(
    root: &Root,
    from: &str,
    to: &str,
    principal_name: Option<&str>,
    now_ms: i64,
    apply: bool,
) -> Result<(RemapReport, Vec<RemappedRow>), StoreError> {
    use rc_memory::owner::{normalize_scope_kind, parse_visibility, OwnerCtx};
    use rc_memory::writers_v4::ensure_principal_anchor;

    let mut report = RemapReport { dry_run: !apply, ..Default::default() };

    // ① 扫：所有 mem_table 上挂着旧 id 的归属行（gate 的 OwnerFacts 与 4b 的比对
    //    不分表，remap 也不分——漏一张表就是漏一类读者）。
    let mut st = root.memory().conn().prepare(
        "SELECT ro.mem_table, ro.mem_id, ro.scope_kind, ro.scope_key,
                ro.owner_agent_id, ro.visibility, ro.retention
           FROM resource_owner ro
          WHERE ro.owner_principal_id = ?1
          ORDER BY ro.mem_table, ro.mem_id",
    )?;
    let rows = st
        .query_map(rusqlite::params![from], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(st);
    report.rows_matched = rows.len();

    // ② 确保新主人锚：每个受影响的锚 scope 一格，归属上下文从该 scope 的旧锚归属行
    //    照抄。写操作，dry-run 跳过。
    if apply {
        let mut seen: BTreeMap<(String, Option<String>), (String, String, String)> =
            BTreeMap::new();
        for (table, _, scope_kind, scope_key, agent, visibility, retention) in &rows {
            if table != "anchor" {
                continue;
            }
            seen.entry((scope_kind.clone(), scope_key.clone()))
                .or_insert_with(|| (agent.clone(), visibility.clone(), retention.clone()));
        }
        for ((scope_kind, scope_key), (agent, visibility, retention)) in &seen {
            let owner = OwnerCtx {
                owner_principal_id: Some(to.to_string()),
                owner_agent_id: agent.clone(),
                visibility: parse_visibility(visibility),
                retention: retention.clone(),
            };
            let scope = rc_memory::Scope {
                kind: normalize_scope_kind(scope_kind),
                key: scope_key.clone().unwrap_or_default(),
            };
            ensure_principal_anchor(
                root.memory(),
                to,
                principal_name,
                &owner,
                &scope,
                now_ms,
            )
            .map_err(|e| StoreError::Sql(e.to_string()))?;
            report.anchors_ensured += 1;
            report.scopes.push(format!(
                "{}:{}",
                scope_kind,
                scope_key.as_deref().unwrap_or("")
            ));
        }
    }

    // ③ 守卫 UPDATE + 记账：一个 memory 事务里逐行改写；值已变的行幂等跳过。
    let mut written = Vec::new();
    if apply {
        let tx = root.memory().conn().unchecked_transaction()?;
        for (table, mem_id, _, _, _, _, _) in &rows {
            let n = tx.execute(
                "UPDATE resource_owner SET owner_principal_id=?1
                  WHERE mem_table=?2 AND mem_id=?3 AND owner_principal_id=?4",
                rusqlite::params![to, table, mem_id, from],
            )?;
            if n == 0 {
                continue; // 已被别的写者改走：幂等命中，不计数也不记账
            }
            tx.execute(
                "INSERT INTO memory_op(op, mem_table, mem_id, before_json, after_json,
                                      reason, actor, created_at_ms)
                 VALUES('UPDATE', ?1, ?2, ?3, ?4, 'principal_remap', 'program', ?5)",
                rusqlite::params![
                    table,
                    mem_id,
                    json_principal(Some(from)),
                    json_principal(Some(to)),
                    now_ms
                ],
            )?;
            report.remapped += 1;
            written.push(RemappedRow {
                mem_table: table.clone(),
                mem_id: mem_id.clone(),
            });
        }
        tx.commit()?;
    } else {
        for (table, mem_id, _, _, _, _, _) in &rows {
            written.push(RemappedRow {
                mem_table: table.clone(),
                mem_id: mem_id.clone(),
            });
        }
    }
    Ok((report, written))
}
