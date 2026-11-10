use rc_contract::{is_valid_principal_source, NormalizedTurn, SessionId, PRINCIPAL_SOURCES};
use rc_store::ddl::TURN_SHADOW;
use rc_store::{StoreError, TranscriptHandle};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InsertOutcome {
    pub inserted: usize,
    pub skipped_duplicate: usize,
    /// 本次真正产生新行时分配的中心提交号；全重复/合成写入为 None。
    pub commit_seq: Option<u64>,
    pub epoch: Option<u64>,
}

/// 参与提交哈希的一行。字段与 `turns` 表里可复制的列一一对应；
/// 正文只通过 `body_fp` 进哈希，复制记录本身仍带正文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitTurn<'a> {
    pub turn_uid: &'a str,
    pub role: &'a str,
    pub body: &'a str,
    pub body_fp: &'a str,
    pub created_at_ms: i64,
    pub ingested_at_ms: i64,
}

fn hash_field(h: &mut Sha256, value: &str) {
    h.update((value.len() as u64).to_le_bytes());
    h.update(value.as_bytes());
}

/// 一个提交的内容哈希。中心在导入时写进 `ingest_log`，备份在 apply 时重算，
/// 两边用同一份字段序；同 seq 不同 sha 才会被判成冲突。
pub fn commit_sha256(turns: &[CommitTurn<'_>]) -> String {
    let mut h = Sha256::new();
    h.update(b"rc.ingest_commit.v1");
    for turn in turns {
        hash_field(&mut h, turn.turn_uid);
        hash_field(&mut h, turn.role);
        hash_field(&mut h, turn.body);
        hash_field(&mut h, turn.body_fp);
        hash_field(&mut h, &turn.created_at_ms.to_string());
        hash_field(&mut h, &turn.ingested_at_ms.to_string());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn tombstone_sha256(scope_kind: &str, scope_key: &str, turns_removed: u64) -> String {
    let mut h = Sha256::new();
    h.update(b"rc.tombstone.v1");
    hash_field(&mut h, scope_kind);
    hash_field(&mut h, scope_key);
    hash_field(&mut h, &turns_removed.to_string());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// 这个 session/device 是否被 purge 墓碑挡住。
///
/// - `before_seq = None`：中心新导入时用，认**所有**墓碑——purge 之后不再收同 scope；
/// - `before_seq = Some(seq)`：备份按 commit_seq apply 时用，只认**早于这条记录**的墓碑；
///   晚于它的墓碑属于后续历史，应当先 apply 导入、再 apply 删除。
pub fn tombstone_blocks(
    t: &TranscriptHandle,
    session_key: &str,
    device_id: &str,
    before_seq: Option<u64>,
) -> Result<bool, StoreError> {
    let n: i64 = match before_seq {
        Some(seq) => t.conn().query_row(
            "SELECT count(*) FROM tombstone
             WHERE commit_seq < ?3
               AND ((scope_kind='session' AND scope_key=?1)
                 OR (scope_kind='device'  AND scope_key=?2))",
            params![session_key, device_id, seq as i64],
            |r| r.get(0),
        )?,
        None => t.conn().query_row(
            "SELECT count(*) FROM tombstone
             WHERE (scope_kind='session' AND scope_key=?1)
                OR (scope_kind='device'  AND scope_key=?2)",
            params![session_key, device_id],
            |r| r.get(0),
        )?,
    };
    Ok(n > 0)
}

/// 当前中心的复制纪元。新库/未提升的中心是 1；提升后由 center_state 递增。
pub fn center_epoch(t: &TranscriptHandle) -> Result<u64, StoreError> {
    let epoch: i64 = t.conn().query_row(
        "SELECT epoch FROM center_state WHERE id=1",
        [],
        |r| r.get(0),
    )?;
    Ok(epoch as u64)
}

// ───────────── 身份层 I1：会话主人身份的追加式盖章（设计 2026-10-27-principal-identity §3.1/§4） ─────────────

/// 一条盖章，列与 `session_principal_claim` 一一对应。
///
/// `id` 是 SQLite 的 **rowid**：设计 §3.1 的 DDL 没有 `id` 列（主键是
/// `(session_key, device_id, claimed_at_ms)`），而 `supersedes` 要指向"被覆盖的那一行"，
/// 所以行 id 只能取 rowid（`last_insert_rowid()` 与 `SELECT rowid` 同源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrincipalClaim {
    pub id: i64,
    pub session_key: String,
    pub device_id: String,
    pub principal_id: String,
    pub principal_name: Option<String>,
    pub source: String,
    pub claimed_by: String,
    pub claimed_at_ms: i64,
    pub supersedes: Option<i64>,
    pub reason: Option<String>,
}

/// 一次盖章的输入。八格拆成结构体而不是八个参数：两处 `Option<&str>`（可读名 / 原因）
/// 的顺序在调用点肉眼分不出，元组式传参一错就是"把原因写成名字"。
#[derive(Debug, Clone, Copy)]
pub struct NewPrincipalClaim<'a> {
    pub session_key: &'a str,
    pub device_id: &'a str,
    /// 稳定 id（账号 id 或 Agent 派生的稳定 id）。`unknown` 也写 `unknown`。
    pub principal_id: &'a str,
    pub principal_name: Option<&'a str>,
    /// 闭集见 [`PRINCIPAL_SOURCES`]。
    pub source: &'a str,
    /// 谁盖的章（Agent 标识，导入时取会话的 `source_agent`），供溯源。
    pub claimed_by: &'a str,
    pub reason: Option<&'a str>,
}

/// `resolve_principal` 的读数。**章从哪来**是读数的一部分，不许靠猜：
/// `claim = Some` ⇒ 来自 claim 表的最新一条；`claim = None` ⇒ 回落到
/// `sessions.owner_principal_id`（旧行/运维手填），此时没有可读名，也没有来源
/// （它不是 `source='unknown'` 的章——那一格是 Agent 明确回答"不知道"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPrincipal {
    pub principal_id: String,
    pub principal_name: Option<String>,
    pub claim: Option<PrincipalClaim>,
}

const CLAIM_SELECT: &str = "SELECT rowid, session_key, device_id, principal_id, principal_name,
                                   source, claimed_by, claimed_at_ms, supersedes, reason
                            FROM session_principal_claim";

fn row_to_claim(r: &rusqlite::Row<'_>) -> Result<PrincipalClaim, rusqlite::Error> {
    Ok(PrincipalClaim {
        id: r.get(0)?,
        session_key: r.get(1)?,
        device_id: r.get(2)?,
        principal_id: r.get(3)?,
        principal_name: r.get(4)?,
        source: r.get(5)?,
        claimed_by: r.get(6)?,
        claimed_at_ms: r.get(7)?,
        supersedes: r.get(8)?,
        reason: r.get(9)?,
    })
}

/// 该会话最新一条 claim（主键三元组里 `claimed_at_ms` 最大的一条）。走 `claim_by_session`。
/// 逐臂 `match`：`QueryReturnedNoRows` 只有"查无"这一支给 `None`，磁盘错误不许折成"没有章"。
fn latest_claim_on(
    conn: &Connection,
    session_key: &str,
    device_id: &str,
) -> Result<Option<PrincipalClaim>, StoreError> {
    let mut st = conn.prepare(&format!(
        "{CLAIM_SELECT} WHERE session_key=?1 AND device_id=?2
         ORDER BY claimed_at_ms DESC LIMIT 1"
    ))?;
    let mut rows = st.query_map(params![session_key, device_id], row_to_claim)?;
    match rows.next() {
        Some(Ok(claim)) => Ok(Some(claim)),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

/// `sessions.owner_principal_id` 这一列（旧锚）。会话行不存在 ⇒ `None`。
fn session_owner_principal_on(
    conn: &Connection,
    session_key: &str,
    device_id: &str,
) -> Result<Option<String>, StoreError> {
    let mut st = conn.prepare(
        "SELECT owner_principal_id FROM sessions WHERE session_key=?1 AND device_id=?2",
    )?;
    let mut rows = st.query_map(params![session_key, device_id], |r| {
        r.get::<_, Option<String>>(0)
    })?;
    match rows.next() {
        Some(Ok(v)) => Ok(v),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

/// 落章成功后同步 `sessions` 的归属两列（P1-7）。
///
/// 真相仍然是 claim 表（`resolve_principal` 先读它），这一列只是镜像：两列长期不一致时，
/// doctor / 归属面按旧列读就会误报"无主"。`ownership_src` 取闭集里已有的 `operator`
/// （schema 注释的四值 `v3|default|operator|legacy`）。
///
/// 会话行不存在 ⇒ 0 行更新，**不是错误**：导入顺序允许 claim 先于 sessions 落行（现状如此），
/// 这一条不新造"必须先有会话行"的约束。
fn sync_session_owner(
    conn: &Connection,
    session_key: &str,
    device_id: &str,
    principal_id: &str,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE sessions SET owner_principal_id=?1, ownership_src='operator'
         WHERE session_key=?2 AND device_id=?3",
        params![principal_id, session_key, device_id],
    )?;
    Ok(())
}

/// 追加一条章（**只追加，不更新**）。返回这次落库的那一行（或与最新一条逐格同值时那一行）。
///
/// 三条规则（工单规则 2，逐条都在这里执法）：
/// - `source` 闭集：[`PRINCIPAL_SOURCES`]，越界 ⇒ `Sql` 错误（不是静默折成 unknown）；
/// - `correction` 必须带 `reason`（`correction` 的全部意义是"上一章盖错了，这是原因"）；
/// - `supersedes` = 写前查到的**最新一条** claim 的 rowid（首条为 `NULL`）。非 correction
///   的追加同样填它：追加本来就覆盖了上一章，链因此是线性可读的（`hub principal list` 用它）。
///
/// 两处防御由设计 §7 第 2 条与主键形状决定：
/// - **重放不长第二行**：与最新一条逐格同值（含 reason）⇒ 直接返回那一行，不写库；
/// - **同毫秒不撞主键**：`claimed_at_ms` 顶到上一行之后（`max(now, latest+1)`），
///   于是 `ORDER BY claimed_at_ms DESC` 永远等于"后写的那条"。
fn claim_on(
    conn: &Connection,
    c: NewPrincipalClaim<'_>,
    now_ms: i64,
) -> Result<PrincipalClaim, StoreError> {
    if c.principal_id.trim().is_empty() {
        return Err(StoreError::Sql(
            "claim_principal: principal_id must not be empty".to_string(),
        ));
    }
    if !is_valid_principal_source(c.source) {
        return Err(StoreError::Sql(format!(
            "claim_principal: source must be one of {PRINCIPAL_SOURCES:?}"
        )));
    }
    // 空白 reason 与没给 reason 是同一件事：它是一个"为什么盖错"的句子，空句子不是原因。
    // 同理空白可读名：`principal_name` 是"怎么称呼"，空串不是称呼（存 NULL 更诚实）。
    let reason = c.reason.filter(|r| !r.trim().is_empty());
    let principal_name = c.principal_name.filter(|n| !n.trim().is_empty());
    if c.source == "correction" && reason.is_none() {
        return Err(StoreError::Sql(
            "claim_principal: source=correction requires a reason".to_string(),
        ));
    }
    let latest = latest_claim_on(conn, c.session_key, c.device_id)?;
    if let Some(latest) = &latest {
        if latest.principal_id == c.principal_id
            && latest.principal_name.as_deref() == principal_name
            && latest.source == c.source
            && latest.claimed_by == c.claimed_by
            && latest.reason.as_deref() == reason
        {
            // 重放同值的早退分支同样要同步会话列：不写的话，"先清空过镜像列"的库
            // 重放同一份导入也永远补不回来（它与"章表是真相"并不冲突，只补镜像）。
            sync_session_owner(conn, c.session_key, c.device_id, &latest.principal_id)?;
            return Ok(latest.clone());
        }
    }
    let claimed_at_ms = match &latest {
        Some(latest) => now_ms.max(latest.claimed_at_ms + 1),
        None => now_ms,
    };
    conn.execute(
        "INSERT INTO session_principal_claim(session_key, device_id, principal_id, principal_name,
                                             source, claimed_by, claimed_at_ms, supersedes, reason)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            c.session_key,
            c.device_id,
            c.principal_id,
            principal_name,
            c.source,
            c.claimed_by,
            claimed_at_ms,
            latest.as_ref().map(|l| l.id),
            reason
        ],
    )?;
    // rowid 先取：下面的 UPDATE 虽不改 `last_insert_rowid()`，但"先读后写"不依赖这条实现细节。
    let id = conn.last_insert_rowid();
    // 落章成功后同步会话归属镜像列（P1-7）：两列不一致会让 doctor/归属面按旧列误读。
    sync_session_owner(conn, c.session_key, c.device_id, c.principal_id)?;
    Ok(PrincipalClaim {
        id,
        session_key: c.session_key.to_string(),
        device_id: c.device_id.to_string(),
        principal_id: c.principal_id.to_string(),
        principal_name: principal_name.map(str::to_string),
        source: c.source.to_string(),
        claimed_by: c.claimed_by.to_string(),
        claimed_at_ms,
        supersedes: latest.map(|l| l.id),
        reason: reason.map(str::to_string),
    })
}

/// 盖一章（公开入口）。`hub principal claim` 与 `insert_batch*` 的导入自动盖章共用它。
pub fn claim_principal(
    t: &TranscriptHandle,
    c: NewPrincipalClaim<'_>,
    now_ms: i64,
) -> Result<PrincipalClaim, StoreError> {
    claim_on(t.conn(), c, now_ms)
}

/// 这个会话的主人是谁：**最新一条 claim**；没有 claim ⇒ 回落 `sessions.owner_principal_id`
/// （可为 NULL）。`pass_fact`/`pass_event`/`pass_naming` 一律走这一个函数——
/// 解析规则只许有一处实现（三份 SQL 就是三次漂移的机会）。
pub fn resolve_principal(
    t: &TranscriptHandle,
    session_key: &str,
    device_id: &str,
) -> Result<Option<ResolvedPrincipal>, StoreError> {
    if let Some(claim) = latest_claim_on(t.conn(), session_key, device_id)? {
        return Ok(Some(ResolvedPrincipal {
            principal_id: claim.principal_id.clone(),
            principal_name: claim.principal_name.clone(),
            claim: Some(claim),
        }));
    }
    Ok(session_owner_principal_on(t.conn(), session_key, device_id)?.map(|principal_id| {
        ResolvedPrincipal { principal_id, principal_name: None, claim: None }
    }))
}

/// 一个主人 id 算不算"身份已知"：去掉空白后非空、且不是 `unknown`。
///
/// 形状与 `rc_memory::writers_v4::principal_is_known` 同一条（本 crate 不依赖 rc-memory，
/// 所以这里只能重述一次）。闸 6c 对 human 轮收不收紧、`principal_claim_sessions` 与
/// `principal_fail_open_sessions` 两个读数都读这一处——**三处必须同时改**，否则会出现
/// "有章却没有锚"或"说得出主人却被算成 fail-open"的假数。
fn identity_known(principal_id: &str) -> bool {
    let id = principal_id.trim();
    !id.is_empty() && id != UNKNOWN_PRINCIPAL_ID
}

/// 一条章算不算"身份已知"：判据同上，喂它的那一格是章里的 id。
fn claim_identity_known(claim: &PrincipalClaim) -> bool {
    identity_known(&claim.principal_id)
}

/// "Agent 答了不知道"的那个字面量（与 rc-memory 同名常量同词；为什么重述见上）。
const UNKNOWN_PRINCIPAL_ID: &str = "unknown";

/// 章表按会话折成"现行章说得出主人"的那一格：`(session_key, device_id)` 集合。
///
/// `principal_claims` 每会话按 `claimed_at_ms DESC` 排，所以第一次见到的就是现行章；
/// 两张集合分开而不是一个计数器，理由与 [`principal_claim_sessions`] 同（`unknown` 的那条
/// 既被记成"看过"又不该进读数）。
fn sessions_with_known_claim(
    claims: &[PrincipalClaim],
) -> std::collections::HashSet<(&str, &str)> {
    let mut claimed: std::collections::HashSet<(&str, &str)> = std::collections::HashSet::new();
    let mut valid: std::collections::HashSet<(&str, &str)> = std::collections::HashSet::new();
    for claim in claims {
        let key = (claim.session_key.as_str(), claim.device_id.as_str());
        if !claimed.insert(key) {
            continue;
        }
        if claim_identity_known(claim) {
            valid.insert(key);
        }
    }
    valid
}

/// **有效章**覆盖的会话数（`doctor` / `status` 的 `principal_claims` 那一格）。
///
/// 三条口径都抄 [`resolve_principal`] 与 [`claim_principal`] 的既有形状，不新造判据：
/// - 一个 `(session_key, device_id)` 只数一次，取的是 `claimed_at_ms` 最大的那一行
///   （"最新一条就是现行章"——`claim_on` 保证它恒是后写的那条）；
/// - 身份未知的那一条**不算有效章**：`unknown` 是"Agent 答了不知道"，拿它建不出主人锚
///   （[`claim_identity_known`] 的口径）；
/// - 只看 claim 表，**不看** `sessions.owner_principal_id` 回落列——回落列不是盖章
///   （`record_principal_mentions_after_import` 与闸 6c 第 3 支同一条取舍）。
///
/// 走 [`principal_claims`] 那一处行解析，不写第二份 SQL：章表很小，读全表再折是安全的。
pub fn principal_claim_sessions(t: &TranscriptHandle) -> Result<i64, StoreError> {
    let claims = principal_claims(t, None)?;
    Ok(sessions_with_known_claim(&claims).len() as i64)
}

/// 6c 处于 **fail-open** 的会话数：有 human 轮、但闸门那一侧解析不出主人的会话有几个。
///
/// 口径与闸门入口**同一条**：逐个人类会话问 [`resolve_principal`]，按它返回的 id 判
/// [`identity_known`]——这里不合并 claim 与回落列，那件事只在 [`resolve_principal`] 里做
/// （第二套合并就是第二份真值表）。于是它的两支自动继承过来：
/// - 没有 claim ⇒ 会回落会话列，回落出已知 id 的会话**不算** fail-open（旧库/运维手填的
///   主人是真的能拿到，闸门拿得到就不是 fail-open）；
/// - 现行章答"不知道" ⇒ 那一支**遮住**回落，会话列再有效也拿不到 known ⇒ 计 1。
///
/// 主语是**有 human 轮的会话**：6c 只对 human 轮收紧，agent 轮没有"主人是谁"这一问，
/// 把它们算进来只会让这一格恒大于零。
/// `0` 是"每个有 human 轮的会话都说得出主人"这个读数，不是读失败。
pub fn principal_fail_open_sessions(t: &TranscriptHandle) -> Result<i64, StoreError> {
    let mut st = t
        .conn()
        .prepare("SELECT DISTINCT session_key, device_id FROM turns WHERE role='human'")?;
    let sessions: Vec<(String, String)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut fail_open = 0i64;
    for (session_key, device_id) in &sessions {
        let resolved = resolve_principal(t, session_key, device_id)?;
        let known = resolved.is_some_and(|r| identity_known(&r.principal_id));
        if !known {
            fail_open += 1;
        }
    }
    Ok(fail_open)
}

/// 章的台账，最新在前（`hub principal list` 的唯一读数）。
/// `session_key=Some` 只看那个会话；`None` 看全部。排序里带 `session_key, device_id`，
/// 于是"同一会话的相邻行"就是它的 supersedes 链（第一行是当前有效章）。
pub fn principal_claims(
    t: &TranscriptHandle,
    session_key: Option<&str>,
) -> Result<Vec<PrincipalClaim>, StoreError> {
    let mut out = Vec::new();
    match session_key {
        Some(key) => {
            let mut st = t.conn().prepare(&format!(
                "{CLAIM_SELECT} WHERE session_key=?1 ORDER BY session_key, device_id,
                 claimed_at_ms DESC"
            ))?;
            let rows = st.query_map(params![key], row_to_claim)?;
            for r in rows {
                out.push(r?);
            }
        }
        None => {
            let mut st = t.conn().prepare(&format!(
                "{CLAIM_SELECT} ORDER BY session_key, device_id, claimed_at_ms DESC"
            ))?;
            let rows = st.query_map([], row_to_claim)?;
            for r in rows {
                out.push(r?);
            }
        }
    }
    Ok(out)
}


fn insert_inner(
    t: &TranscriptHandle,
    s: &SessionId,
    turns: &[NormalizedTurn],
    now_ms: i64,
    committed_op: Option<&str>,
) -> Result<InsertOutcome, StoreError> {
    let conn = t.conn();
    let tx = conn.unchecked_transaction()?;
    // S1b-C1：会话归属 7 列在这里写下**保守默认**（设计 ownership-persistence §1.2）。
    // owner 只能是产出者自己，可见性 private、保留 default；`scope_key` 与
    // `rc_distill::batch::Batch::scope_key()` 同口径（project_hint 优先，缺省退回会话键），
    // 避免同一会话在两处落进两个 bucket。非默认归属留给 v3 适配器 / 运维 CLI
    // （ownership_src='v3'|'operator'），本层不接 v3 `Ownership`，`SessionId` 也不加字段。
    //
    // 冲突分支逐列 `COALESCE(旧, 新)`：**重传不清空已归属行**——唯一能填的是旧行
    // 迁移留下的 NULL（v4 旧行），以及后续记录里非空的归属值。`last_saved_at_ms` 仍取 max。
    tx.execute(
        "INSERT INTO sessions(session_key, device_id, source_agent, machine, project_hint,
                              last_saved_at_ms, owner_principal_id, owner_agent_id,
                              scope_kind, scope_key, visibility, retention, ownership_src)
         VALUES(?1,?2,?3,?4,?5,?6, NULL, ?3, 'project', COALESCE(?5, ?1),
                'private_agent', 'default', 'default')
         ON CONFLICT(session_key, device_id) DO UPDATE SET
           last_saved_at_ms = max(last_saved_at_ms, excluded.last_saved_at_ms),
           owner_principal_id = COALESCE(sessions.owner_principal_id, excluded.owner_principal_id),
           owner_agent_id = COALESCE(sessions.owner_agent_id, excluded.owner_agent_id),
           scope_kind = COALESCE(sessions.scope_kind, excluded.scope_kind),
           scope_key = COALESCE(sessions.scope_key, excluded.scope_key),
           visibility = COALESCE(sessions.visibility, excluded.visibility),
           retention = COALESCE(sessions.retention, excluded.retention),
           ownership_src = COALESCE(sessions.ownership_src, excluded.ownership_src)",
        params![s.session_key, s.device_id, s.source_agent, s.machine, s.project_hint, s.last_saved_at_ms.unwrap_or(now_ms)],
    )?;
    // 身份层 I1（设计 `2026-10-27-principal-identity.md` §5 的"导入"一行 / 工单规则 4）：
    // 导入带 `owner_principal_id` ⇒ **同一事务**里追加一条 claim。这样 Agent 侧不需要额外
    // 调 `hub principal claim` 就能盖章：`source` 取 `principal_source`（缺省 `agent_runtime`），
    // `claimed_by` = 该会话的 `source_agent`（自报的出处，留作溯源）。
    // 三件事由此免费拿到：重传不长第二行（`claim_on` 的幂等一支）、坏 source 与导入同生共死、
    // `sessions.owner_principal_id` 由 `claim_on` 同步写上（append-only 的**章表**才是真相，
    // 那一列是镜像；P1-7 之前它一直留空，doctor/归属面按旧列读就会误报"无主"）。
    if let Some(principal_id) = s.owner_principal_id.as_deref() {
        claim_on(
            &tx,
            NewPrincipalClaim {
                session_key: &s.session_key,
                device_id: &s.device_id,
                principal_id,
                principal_name: s.principal_name.as_deref(),
                source: s.principal_source.as_deref().unwrap_or("agent_runtime"),
                claimed_by: &s.source_agent,
                reason: None,
            },
            now_ms,
        )?;
    }
    let mut out = InsertOutcome::default();
    let mut inserted_uids: Vec<&str> = Vec::new();
    for n in turns {
        let changed = tx.execute(
            "INSERT OR IGNORE INTO turns(turn_uid, session_key, device_id, role, body, body_fp,
                                        turn_key_tier, project_hint, created_at_ms, ingested_at_ms,
                                        commit_seq)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,0)",
            params![n.turn_uid, s.session_key, s.device_id, n.role, n.body, n.body_fp,
                    n.tier.rank(), s.project_hint, n.created_at_ms, now_ms],
        )?;
        // 单行更新 rowcount != 1 即红（继承"写零行 = 错误"三个执法点之一）——
        // 但 INSERT OR IGNORE 的 0 是**幂等命中**，所以它记进 skipped_duplicate 而不是错误。
        if changed == 1 {
            out.inserted += 1;
            inserted_uids.push(n.turn_uid.as_str());
            // W38a 刀 A（设计 2026-11-08-w38a-transcript-search §二.2）：FTS 影子行与
            // turns 行**同事务**同生共死——rebuild 之前的新轮当场可检索（增量的那半条，
            // tests/turn_shadow.rs 的 insert_batch_is_searchable_immediately_without_rebuild 钉住）。
            // 影子分词用 `row_doc("", body)`：turns 无标题列（细案 §二.1 注）。
            // 重复导入（changed != 1）不长第二条影子——影子行随唯一的那次插入走。
            let (doc, len) = rc_index::analyzer::row_doc("", &n.body);
            tx.execute(
                &format!("INSERT INTO {TURN_SHADOW}(turn_uid, tokens, row_len) VALUES(?1,?2,?3)"),
                params![n.turn_uid, doc, len as i64],
            )?;
        } else {
            out.skipped_duplicate += 1;
        }
    }
    if let Some(op) = committed_op {
        if !inserted_uids.is_empty() {
            tx.execute(
                "INSERT OR IGNORE INTO center_state(id, last_commit_seq, epoch) VALUES(1, 0, 1)",
                [],
            )?;
            tx.execute(
                "UPDATE center_state SET last_commit_seq = last_commit_seq + 1 WHERE id = 1",
                [],
            )?;
            let (commit_seq, epoch): (i64, i64) = tx.query_row(
                "SELECT last_commit_seq, epoch FROM center_state WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let commit_seq = commit_seq as u64;
            for turn_uid in &inserted_uids {
                let changed = tx.execute(
                    "UPDATE turns SET commit_seq = ?1 WHERE turn_uid = ?2",
                    params![commit_seq as i64, turn_uid],
                )?;
                if changed != 1 {
                    return Err(StoreError::Sql(
                        "a committed turn could not be stamped".to_string(),
                    ));
                }
            }
            let commit_turns: Vec<CommitTurn<'_>> = turns
                .iter()
                .filter(|n| inserted_uids.contains(&n.turn_uid.as_str()))
                .map(|n| CommitTurn {
                    turn_uid: n.turn_uid.as_str(),
                    role: n.role.as_str(),
                    body: n.body.as_str(),
                    body_fp: n.body_fp.as_str(),
                    created_at_ms: n.created_at_ms,
                    ingested_at_ms: now_ms,
                })
                .collect();
            let sha = commit_sha256(&commit_turns);
            tx.execute(
                "INSERT INTO ingest_log(commit_seq, epoch, op, session_key, device_id,
                                        turns_count, content_sha256, created_at_ms)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![commit_seq as i64, epoch, op, s.session_key, s.device_id,
                        commit_turns.len() as i64, sha, now_ms],
            )?;
            out.commit_seq = Some(commit_seq);
            out.epoch = Some(epoch as u64);
        }
    }
    tx.commit()?;
    Ok(out)
}

/// append-only 的唯一写入点。`P` 独占列（turn_uid/body_fp/tier/ingested_at）在这里生成，
/// 内容列（body/role/created_at_ms）由契约侧算好后传入——归属表的两半在同一个函数里可见。
///
/// 这个入口不分配中心提交号（值为 0）：合成行、节点本地行与旧测试走它。
/// 中心导入必须走 [`insert_batch_committed`]，否则复制轴会缺一段。
pub fn insert_batch(
    t: &TranscriptHandle,
    s: &SessionId,
    turns: &[NormalizedTurn],
    now_ms: i64,
) -> Result<InsertOutcome, StoreError> {
    insert_inner(t, s, turns, now_ms, None)
}

/// 中心导入的唯一写入点：turn 行、`commit_seq`、`ingest_log` 在同一个事务里。
/// 全重复的批次不分配新提交号，也不写日志；已写入的旧行保留原 commit_seq。
pub fn insert_batch_committed(
    t: &TranscriptHandle,
    s: &SessionId,
    turns: &[NormalizedTurn],
    now_ms: i64,
    op: &str,
) -> Result<InsertOutcome, StoreError> {
    insert_inner(t, s, turns, now_ms, Some(op))
}

/// `turn_shadow` 的**全量重建**（W38a 刀 A，设计 §二.2：存量与修复的唯一入口；
/// `hub index rebuild` 的转录段调它）。影子层可整层丢弃：先 DELETE 再从 `turns`
/// 权威面逐行重灌，因此幂等——连跑两次读数不变、`turns` 一字不动
/// （`tests/turn_shadow.rs` 的 `rebuild_twice_is_idempotent_and_turns_rows_are_untouched` 钉住）。
/// 复制 apply / restore 这两条不走 `insert_inner` 的写点留下的影子缺口，由这里一次补齐。
/// 返回灌进的影子行数（= turns 权威行数）。
pub fn rebuild_turn_shadow(t: &TranscriptHandle) -> Result<u64, StoreError> {
    let conn = t.conn();
    let tx = conn.unchecked_transaction()?;
    tx.execute(&format!("DELETE FROM {TURN_SHADOW}"), [])?;
    let mut n: u64 = 0;
    {
        let mut read = tx.prepare("SELECT turn_uid, body FROM turns")?;
        let mut write = tx.prepare(&format!(
            "INSERT INTO {TURN_SHADOW}(turn_uid, tokens, row_len) VALUES(?1,?2,?3)"
        ))?;
        let rows = read.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (uid, body) = row?;
            // 与 `insert_inner` 的增量写同一把尺子（`row_doc("", body)`），两侧不各写一遍词面。
            let (doc, len) = rc_index::analyzer::row_doc("", &body);
            write.execute(params![uid, doc, len as i64])?;
            n += 1;
        }
    }
    tx.commit()?;
    Ok(n)
}

/// 一条轮在**它自己的会话里**的坐标。P2 的两个读数都从它出发：出处设备 + 会话键决定问哪一段水位，
/// 到达钟 `ingested_at_ms` 决定区间的右端点（设备钟 `created_at_ms` 只作旁证，不参与选批）。
/// 只要转录柄 ⇒ 守卫 1 在它身上不受考验。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOrigin {
    pub session_key: String,
    pub device_id: String,
    pub created_at_ms: i64,
    pub ingested_at_ms: i64,
}

/// 主键命中（`turns.turn_uid` 是 `PRIMARY KEY`）。
/// `Ok(None)` 是**读数**："转录里没有这一轮"。它**不**是错误，也不许由 `.ok()` 造出来：
/// 那种写法把 SQL 错误一起折成"查无此轮"，于是磁盘故障被报成"你那句话不存在"
/// （§11 D5 那一族）。所以下面是逐臂 `match`，`QueryReturnedNoRows` 只有这一支给 `None`。
/// 本仓没有 `OptionalExtension::optional()` 的用法先例（本机已核全仓零命中），
/// 这里的形状与本仓既有的"`match Err(QueryReturnedNoRows)`"一致。
pub fn turn_origin(t: &TranscriptHandle, turn_uid: &str) -> Result<Option<TurnOrigin>, StoreError> {
    let mut st = t.conn().prepare(
        "SELECT session_key, device_id, created_at_ms, ingested_at_ms FROM turns
         WHERE turn_uid=?1")?;
    let mut rows = st.query_map(rusqlite::params![turn_uid], |r| {
        Ok(TurnOrigin { session_key: r.get(0)?, device_id: r.get(1)?,
                        created_at_ms: r.get(2)?, ingested_at_ms: r.get(3)? })
    })?;
    match rows.next() {
        Some(Ok(o)) => Ok(Some(o)),
        // `StoreError` 的 `From<rusqlite::Error>` 会把行读取错误折成 `Sql`，与"查无"分家。
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

/// `(after_ms, before_ms)` 的**左开右开**区间内、该会话的转录行数——P2 的 `eta_turns` 唯一来源。
/// 两端都不含：`after_ms` 是水位游标指向的那一条（已经蒸过，不排队），`before_ms` 是问的那一条本身。
/// 区间量的是**到达钟**（`ingested_at_ms`）：队列位置问的是"还有几轮没蒸"，而选批的轴也是到达钟。
/// 所以实现里一个 `+1`/`-1` 都不许出现（P-44）：出现就说明有人在猜边界而不是在读区间。
/// 走 `turns_by_session_ingested(session_key, ingested_at_ms)` 那条索引；P-23 第③支"不许全表扫描"
/// 在这里不是约束而是**理由**：这把尺子每 `remember` 一次就要跑一遍。
pub fn count_between(
    t: &TranscriptHandle,
    session_key: &str,
    after_ms: i64,
    before_ms: i64,
) -> Result<u64, StoreError> {
    let n: i64 = t.conn().query_row(
        "SELECT count(*) FROM turns WHERE session_key=?1 AND ingested_at_ms>?2 AND ingested_at_ms<?3",
        rusqlite::params![session_key, after_ms, before_ms],
        |r| r.get(0),
    )?;
    Ok(n.max(0) as u64)
}

/// 转录库的**总行数**读数（含 `role='system'` 的压缩块/CTX）。
///
/// 三条边界都写死在这里，因为这一格今天起有两个读者（出口台账那条算式与运维面）：
/// ① 返回 `i64` 而不是 `u64`——`count_between` 给 `u64` 是因为它的读者是队列位置，
///    而这一支要和 `pass_state::count_terminal_turns` 的 `i64` 相减（S3b 起换源：
///    被减数是 pass 表的终态轮数，legacy `extract_state` 今天没有写者）；
/// ② 空库给 `0`，不给 `Option`——"一条都没有"是一个合法读数，不是"读不到"（P-44 那一族）；
/// ③ 只收 `&TranscriptHandle` 一把柄——两柄的减法住调用方（`mcp`），守卫 1 在这条签名上不受考验，
///    理由与 `count_between` / `turn_origin` 同一条（§2.17）。
///
/// **`status.distill_lag_turns` 不用这一格**：它数的是全部轮，而抽取候选在 SQL 层以
/// `role <> 'system'` 挡掉 system 轮，后者永远不会写 pass 终态行；用总行数当分母会留下一个
/// 恒等于 system 轮数的"假积压"。落后格的分母是下面的 `count_extractable_turns`。
pub fn count_turns(t: &TranscriptHandle) -> Result<i64, StoreError> {
    Ok(t.conn().query_row("SELECT count(*) FROM turns", [], |r| r.get(0))?)
}

/// **可抽取轮数**（`role <> 'system'`）。`status.distill_lag_turns` 的分母。
///
/// 与抽取候选 SQL 同一条判据（`pass_naming.rs` / `pass_event.rs` / `pass_fact.rs` /
/// `pass_state.rs` 的 `role <> 'system'`）：system 轮是工具调用/工具结果与压缩块，只作
/// 上下文，永不作 TARGET，也就永不会有一行 `extract_pass_state` 终态。分母若仍数它们，
/// 落后读数会有一个永远清不掉的底（2026-11-04 本机 361 轮里 258 条 system ⇒ lag 恒 258）。
///
/// 空库给 `0`；两边都是 `i64`，减法住调用方（与 `count_turns` 同一条边界）。
pub fn count_extractable_turns(t: &TranscriptHandle) -> Result<i64, StoreError> {
    Ok(t.conn().query_row(
        "SELECT count(*) FROM turns WHERE role <> 'system'", [], |r| r.get(0))?)
}

/// 返回 `commit_seq ∈ (from_exclusive, to_inclusive]` 的 `turn_uid`，按
/// `(commit_seq, turn_uid)` 升序（H7c1 的接管对账用它量"记忆落后了几轮"）。
///
/// 三条边界：
/// ① 区间**左开右闭**：`from_exclusive` 是记忆快照已经覆盖到的那一格（含），右端点才是缺口本身；
/// ② `limit = 0` 表示不限；`limit > 0` 时只返回前 `limit` 条，**截断提示归调用方**——
///    本函数不返回"还有多少条"，那要再跑一次 `count(*)`，而调用方的玩法是问 `limit + 1` 条；
/// ③ 空区间（`to_inclusive <= from_exclusive`）直接给空表，不查库——这个读数合法，不是错误。
/// 走 `turns_by_commit(commit_seq, turn_uid)` 索引，不做全表扫描。
pub fn turn_uids_in_commit_range(
    t: &TranscriptHandle,
    from_exclusive: u64,
    to_inclusive: u64,
    limit: usize,
) -> Result<Vec<String>, StoreError> {
    if to_inclusive <= from_exclusive {
        return Ok(Vec::new());
    }
    let limit = if limit == 0 { -1i64 } else { i64::try_from(limit).unwrap_or(i64::MAX) };
    let mut st = t.conn().prepare(
        "SELECT turn_uid FROM turns
         WHERE commit_seq > ?1 AND commit_seq <= ?2
         ORDER BY commit_seq, turn_uid
         LIMIT ?3",
    )?;
    let rows = st.query_map(
        params![from_exclusive as i64, to_inclusive as i64, limit],
        |r| r.get::<_, String>(0),
    )?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// 节点上行要挑的**一条轮**（本机转录层里的原样）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTurn {
    pub turn_uid: String,
    pub role: String,
    pub body: String,
    /// `turns.turn_key_tier` 的原值（1/2/3）。
    pub tier: u8,
    pub created_at_ms: i64,
    pub ingested_at_ms: i64,
}

/// 节点上行要挑的**一个会话**（含它的元数据与那一批轮）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSession {
    pub session_key: String,
    pub device_id: String,
    pub source_agent: String,
    pub machine: Option<String>,
    pub project_hint: Option<String>,
    pub last_saved_at_ms: Option<i64>,
    pub turns: Vec<LocalTurn>,
}

/// 挑出本机转录层里 `(ingested_at_ms, turn_uid)` **严格大于**游标的那一批，按会话归拢。
///
/// 选批的轴是**到达钟 + turn_uid**，与 `rc_ledger::water::Cursor` 同形：同毫秒的两条靠 turn_uid 定序，
/// 单靠毫秒会漏掉同毫秒里 turn_uid 较小的那条。
///
/// 归拢用**索引表**而不是"相邻即同会话"：全局序是按 `(ingested_at_ms, turn_uid)` 排的，
/// 同一个会话的两条轮可能被别的会话那条**隔开**（A@10、B@20、A@30）。
/// 按相邻归拢会把 A 拆成两个会话，而契约要求"一个会话只许出现在一行里"⇒ 上行会多发一行、
/// 中心按 `duplicate_session` 拒掉第二行。
pub fn sessions_after(
    t: &TranscriptHandle,
    at_ms: i64,
    turn_uid: &str,
) -> Result<Vec<LocalSession>, StoreError> {
    let conn = t.conn();
    let mut st = conn.prepare(
        "SELECT s.session_key, s.device_id, s.source_agent, s.machine, s.project_hint,
                s.last_saved_at_ms, t.turn_uid, t.role, t.body, t.turn_key_tier,
                t.created_at_ms, t.ingested_at_ms
         FROM turns t
         JOIN sessions s ON s.session_key = t.session_key AND s.device_id = t.device_id
         WHERE t.ingested_at_ms > ?1 OR (t.ingested_at_ms = ?1 AND t.turn_uid > ?2)
         ORDER BY t.ingested_at_ms, t.turn_uid",
    )?;
    let rows = st.query_map(params![at_ms, turn_uid], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<i64>>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, String>(7)?,
            r.get::<_, String>(8)?,
            r.get::<_, u8>(9)?,
            r.get::<_, i64>(10)?,
            r.get::<_, i64>(11)?,
        ))
    })?;
    let mut out: Vec<LocalSession> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for row in rows {
        let (sk, dev, agent, machine, hint, last_saved, uid, role, body, tier, created, ingested) = row?;
        let turn = LocalTurn {
            turn_uid: uid,
            role,
            body,
            tier,
            created_at_ms: created,
            ingested_at_ms: ingested,
        };
        match index.get(&sk) {
            Some(&i) => out[i].turns.push(turn),
            None => {
                index.insert(sk.clone(), out.len());
                out.push(LocalSession {
                    session_key: sk,
                    device_id: dev,
                    source_agent: agent,
                    machine,
                    project_hint: hint,
                    last_saved_at_ms: last_saved,
                    turns: vec![turn],
                });
            }
        }
    }
    Ok(out)
}
