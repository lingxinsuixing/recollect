//! S2a-1：多 pass 抽取的**按 `(turn, pass, family)`** 状态机（设计 `2026-10-26-s2-prompt-v0` §3）。
//!
//! 这一层只回答四件事：这一格**跑过没**、**谁产的**（指纹 + run_id）、**失败了几次/什么时候可再试**、
//! **终态是什么**。它不选批、不调模型、不写记忆行——那些是 S2a-2 的 runner。
//!
//! S2e-3 起还多一张旁表 `extract_pass_artifact`：拒收那一次调用的模型原文（限长副本），
//! 只追加、只被读。它是"为什么是这个 slug"的证据，不是状态机的一部分。
//!
//! 三条纪律：
//! 1. **旧表不动**：`extract_state`（单键 `turn_uid`）是 legacy v3 的轮级汇总，本模块一个字节都不读它。
//!    新表 `extract_pass_state` 是旁表，主键 `(turn_uid, pass_id, family)`。
//! 2. **非法键与坏 slug 都响亮**：`pass_id` 不在 [`PASS_IDS`]、`turn_uid` 为空、`family` 越界 ⇒
//!    `StoreError::Sql`；库里躺着读不出的 `state` 也报错，**不许折成 `Pending`**（同
//!    `water::extract_record` 的纪律：那会把"数据不一致"读成"还没跑"）。
//! 3. **重复写是覆盖**：同一行重复 `mark_running` / `mark_terminal` 都是重写这一条，不长第二条
//!    （与 `water::set_extract_state` 同一条不变量形状）。
use rc_store::{LedgerHandle, StoreError};
use rusqlite::params;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

/// 六个 pass 的封闭清单，与 `extract_pass_state.pass_id` 的 CHECK 一对镜像。
///
/// P2 加 `state`（状态 pass：单一 pass，`family` 恒空串，与 naming/event/time 同一条纪律）。
/// 波三修 1 加 `lesson`（第 6 pass，`family` 恒空串）：它**只有终态**——空产出 / 契约拒收 /
/// 审核完成各落一行，选批按"批内轮是否已有终态"判幂等（见 [`settled_turns`]）。
/// 旧库的 CHECK 由 `rc-store` 的 `widen_closed_set_check` 按 canonical 重建表加宽。
pub const PASS_IDS: [&str; 6] = ["naming", "fact", "event", "time", "state", "lesson"];

/// 终态的 SQL 清单（S3b）：与 `PassState::is_terminal` 的五个值**逐字一致**。
///
/// 它是 `status.distill_lag_turns` 与 `last_extract_at_ms` 唯一的读侧判据，也是
/// [`note_attempt`] 那条 `CASE` 的唯一来源——那里用的是「这份清单**减 `stale`**」：
/// `stale` 是唯一允许退回 `pending` 的终态（见 [`note_attempt`] 的文档）。改枚举时这些
/// 地方同改；`tests/pass_state.rs` 有一条用例把这串拆开逐条喂 `mark_terminal`（那个入口
/// 只收终态），再断言 `pending`/`running` 一个都不在里面——漏改一边会当场红。
///
/// 为什么是常量而不是在各条 SQL 里各抄一遍：每处分叉就是一次漂移机会，而这里只回答一件事
/// "哪些状态算这一轮已经蒸完了"。
pub const TERMINAL_STATES: &str = "('done','empty','rejected','quarantined','stale')";

/// `fact` 的调用变体（一次只带一个）。`family` 只对这一族取值，其余 pass 一律空串。
const FACT_FAMILIES: [&str; 3] = ["decision", "promise", "plan"];

const FACT: &str = "fact";

/// 抽取完成、窄审还没跑时 `pending` 行的理由（S3b-4b）。
///
/// 它与"失败退避后的 `pending`"共用同一个 state 列，靠这一格区分：`pending` +
/// `review_pending` = 产出已在 `review_item` 里等审核；`pending` + 空 reason = 等待重试。
/// 这一行**有指纹、有 run_id、可能有 attempt**（抽取真跑过），不像失败行那样只有计数。
pub const REVIEW_PENDING_REASON: &str = "review_pending";

/// 一个单元（`(turn, pass, family)`）最多试几次（W19c1）：第 [`MAX_PASS_ATTEMPTS`] 次仍失败
/// 就隔离它，不再每轮重烧同一个卡死的对端。
///
/// 它与 `rc_memory::review::MAX_ATTEMPTS`（双审队列的 3）同一个形状，但**不是**同一个数：
/// 那个数管的是审核项，这个数管的是抽取格；两边各自有读者，合并会把一处调参变成两处行为。
pub const MAX_PASS_ATTEMPTS: i64 = 3;

/// 隔离理由的固定一格（W19c1）：试满次数仍失败，与 `rejected(<slug>)`（模型给了不合格的答复）
/// 是两件事——这一格的产出**没有被判坏**，只是没能拿到答复。
pub const RETRY_EXHAUSTED_REASON: &str = "retry_exhausted";

/// 一格的七个状态。`slug`/`parse` 往返是这一列唯一的安全读法（同 `water::ExtractState`）。
///
/// `Stale` = "这一行的指纹过期了，旧产出不再作数"：`should_run` 见到它就跑，重跑时
/// `mark_running` 覆盖成 `Running`。它不是第四类终态，也不代表产出被删。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassState {
    Pending,
    Running,
    Done,
    Empty,
    Rejected,
    Quarantined,
    Stale,
}

impl PassState {
    pub fn slug(self) -> &'static str {
        match self {
            PassState::Pending => "pending",
            PassState::Running => "running",
            PassState::Done => "done",
            PassState::Empty => "empty",
            PassState::Rejected => "rejected",
            PassState::Quarantined => "quarantined",
            PassState::Stale => "stale",
        }
    }
    pub fn parse(s: &str) -> Option<PassState> {
        match s {
            "pending" => Some(PassState::Pending),
            "running" => Some(PassState::Running),
            "done" => Some(PassState::Done),
            "empty" => Some(PassState::Empty),
            "rejected" => Some(PassState::Rejected),
            "quarantined" => Some(PassState::Quarantined),
            "stale" => Some(PassState::Stale),
            _ => None,
        }
    }
    /// 只有这五个能由 [`mark_terminal`] 写。`Running`/`Pending` 走 [`mark_running`] 与
    /// [`note_attempt`]——把它们传给终态入口是调用方 bug，不是"写个中间态"。
    fn is_terminal(self) -> bool {
        matches!(self, PassState::Done | PassState::Empty | PassState::Rejected
                       | PassState::Quarantined | PassState::Stale)
    }
}

/// 一格的主键。借用而不是持有 `String`：调用方手里本来就有这些切片，键不该逼它复制。
#[derive(Debug, Clone, Copy)]
pub struct PassKey<'a> {
    pub turn_uid: &'a str,
    pub pass_id: &'a str,
    pub family: &'a str,
}

/// 一格的完整读数。[`get`] 是这一行的唯一读法（同 `water::extract_record` 的"一行一把读法"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassRecord {
    pub state: PassState,
    pub attempt: i64,
    pub next_retry_at_ms: Option<i64>,
    pub fingerprint: Option<String>,
    pub reason: Option<String>,
    pub run_id: Option<String>,
    pub updated_at_ms: i64,
    /// 产下这一行的 system prompt 版本（P0-2）。`None` = 旧行/没有模型面的 pass。
    pub prompt_version: Option<String>,
}

fn check_pass_id(pass_id: &str) -> Result<(), StoreError> {
    if !PASS_IDS.contains(&pass_id) {
        return Err(StoreError::Sql(format!("pass_state: unknown pass_id {pass_id}")));
    }
    Ok(())
}

/// 键的形状是 API 层的一条闸，不是 DDL 的全部：`pass_id` 的闭集 DDL 也有，
/// 而"`family` 只对 fact 取值"只能在这里表达（DDL 不知道两个列的联合约束）。
fn check_key(key: PassKey<'_>) -> Result<(), StoreError> {
    if key.turn_uid.is_empty() {
        return Err(StoreError::Sql("pass_state: empty turn_uid".into()));
    }
    check_pass_id(key.pass_id)?;
    if key.pass_id == FACT {
        if !key.family.is_empty() && !FACT_FAMILIES.contains(&key.family) {
            return Err(StoreError::Sql(format!(
                "pass_state: unknown fact family {}", key.family)));
        }
    } else if !key.family.is_empty() {
        return Err(StoreError::Sql(format!(
            "pass_state: family {} is only for pass {FACT}", key.family)));
    }
    Ok(())
}

pub fn get(l: &LedgerHandle, key: PassKey<'_>) -> Result<Option<PassRecord>, StoreError> {
    check_key(key)?;
    let mut st = l.conn().prepare(
        "SELECT state, attempt, next_retry_at_ms, fingerprint, reason, run_id, updated_at_ms,
                prompt_version
         FROM extract_pass_state WHERE turn_uid=?1 AND pass_id=?2 AND family=?3")?;
    let mut rows = st.query_map(params![key.turn_uid, key.pass_id, key.family], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?,
            r.get::<_, Option<String>>(3)?, r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?, r.get::<_, i64>(6)?,
            r.get::<_, Option<String>>(7)?))
    })?;
    match rows.next() {
        Some(Ok((raw, attempt, next_retry_at_ms, fingerprint, reason, run_id, updated_at_ms,
                 prompt_version))) => {
            // 坏 slug 是**响亮**的：读不出的状态 = 有人写了别的产物，不是"还没跑"。
            let state = PassState::parse(&raw).ok_or_else(|| {
                StoreError::Sql(format!("pass_state: unreadable state {raw}"))
            })?;
            Ok(Some(PassRecord {
                state, attempt, next_retry_at_ms, fingerprint, reason, run_id, updated_at_ms,
                prompt_version,
            }))
        }
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

/// 一版 system prompt 的版本号（P0-2）：**只喂 system**，SHA-256 后 64 位小写 hex。
///
/// 它与 [`rc_ledger::water::extraction_fingerprint`] 同一族做法（域分隔 + 字段长度前缀 +
/// SHA-256），但**故意不喂 user/批次/模型**：指纹含整批 user 文本，批组成一变就全不等，
/// 拿它当"改判据"的信号会把每一批都读成新版本。这一列只回答"这一格的产出是哪一版
/// system prompt 产的"，于是"改 prompt ⇒ 旧 done 轮回头"是一条与批组成无关的判据。
///
/// 域分隔用另一个常量：同一段 system 被别的用途哈希时不会与本列撞上。
pub fn prompt_version(prompt_system: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"rc.pass_prompt_version.v1");
    h.update((prompt_system.len() as u64).to_le_bytes());
    h.update(prompt_system.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// 改判据后的重跑判据（P0-2）：终态行（`done`/`empty`/`rejected`/`quarantined`）存下的
/// 版本号**非空且与当前不同** ⇒ 这一格可以重选。
///
/// **NULL 一律不可重选**：旧库升级后既有行没有版本号，若把 NULL 读成"不同"，升级一次就会
/// 全量重跑所有旧轮——那不是这条判据要的。要重跑旧轮的人显式清掉那一行（或改 state）。
/// `running`/`pending`/`stale` 不归这里管：它们各有自己的选批判据（不重入 / 退避 / 直接可跑）。
pub fn prompt_version_reselectable(
    state: PassState,
    stored: Option<&str>,
    current: &str,
) -> bool {
    matches!(state,
             PassState::Done | PassState::Empty | PassState::Rejected | PassState::Quarantined)
        && stored.is_some_and(|version| version != current)
}

/// 开跑：写入/覆盖这一行的 `state='running'`、指纹、`run_id`；`attempt=0`、
/// `next_retry_at_ms=NULL`（新一轮的计数从零开始），并把上一轮的终态理由清掉——
/// 一个 `running` 行带着上一轮的 `reason='schema'` 是读数上的谎。
///
/// W19c1 的唯一例外：上一行仍停在 `pending`（退避中的重试）时 **`attempt` 原样保留**。
/// 这一列是"这一格失败了几次"，[`note_attempt_or_quarantine`] 靠它数到
/// [`MAX_PASS_ATTEMPTS`]；每次重开都清零，那个上限永远到不了，卡死的对端就会一直重烧。
/// 其余入口（首次、`stale`、终态之后的重跑）照旧归零：换判据或换批是新一轮，计数从零算。
///
/// 这一版不带 prompt 版本（没有模型面的 pass，如 time，用它）：`prompt_version` 写 NULL。
/// 有模型面的四个 pass（naming / fact / event / state）用 [`mark_running_with_prompt_version`]，
/// 两者其余列逐字相同。
pub fn mark_running(
    l: &LedgerHandle,
    key: PassKey<'_>,
    fingerprint: &str,
    run_id: &str,
    now_ms: i64,
) -> Result<(), StoreError> {
    mark_running_with_prompt_version(l, key, fingerprint, None, run_id, now_ms)
}

/// 带 system prompt 版本号的开跑（P0-2）：与 [`mark_running`] 同一条不变量（覆盖而不是追加、
/// `attempt` 归零、退避清空、理由清空——`pending` 重试那一格的例外见 [`mark_running`]），
/// 只是把版本号与指纹**同一次写**落进这一行。
///
/// 版本号只由 system prompt 算（[`prompt_version`]）；它与指纹是两把尺子：指纹回答
/// "这一批的输入是不是同一份"，版本号回答"这一格的产出是哪一版判据产的"。
/// 写 `None` = 这一格没有 prompt 面（或调用方不提供），选批**不**据此重选旧轮。
pub fn mark_running_with_prompt_version(
    l: &LedgerHandle,
    key: PassKey<'_>,
    fingerprint: &str,
    prompt_version: Option<&str>,
    run_id: &str,
    now_ms: i64,
) -> Result<(), StoreError> {
    check_key(key)?;
    l.conn().execute(
        "INSERT INTO extract_pass_state(turn_uid, pass_id, family, state, attempt,
                                        next_retry_at_ms, fingerprint, reason, run_id,
                                        updated_at_ms, prompt_version)
         VALUES(?1,?2,?3,'running',0,NULL,?4,NULL,?5,?6,?7)
         ON CONFLICT(turn_uid, pass_id, family) DO UPDATE SET
           state='running',
           attempt=CASE WHEN extract_pass_state.state = ?8
                        THEN extract_pass_state.attempt ELSE 0 END,
           next_retry_at_ms=NULL,
           fingerprint=?4, reason=NULL, run_id=?5, updated_at_ms=?6, prompt_version=?7",
        params![key.turn_uid, key.pass_id, key.family, fingerprint, run_id, now_ms,
                prompt_version, PassState::Pending.slug()])?;
    Ok(())
}

/// 终态：只接受 `Done`/`Empty`/`Rejected`/`Quarantined`/`Stale`。`Running`/`Pending` 传进来是
/// 调用方 bug，返回 `StoreError::Sql`，不静默写。终态行没有"下一次重试"（`next_retry_at_ms` 清空），
/// 指纹与 `attempt` 保留——它们是"哪一轮、试了几次"的历史读数。
///
/// 没有先行 `mark_running` 的行也能落终态（空批/直接拒收是合法结果）：那一行 `attempt=0`、
/// 指纹与 `run_id` 为空。终态写**不动** `prompt_version`：它是 [`mark_running_with_prompt_version`]
/// 落下的开工记录，选批靠它判断"改判据后要不要回头"（没有开工记录的行自然是 NULL ⇒ 不回头）。
pub fn mark_terminal(
    l: &LedgerHandle,
    key: PassKey<'_>,
    state: PassState,
    reason: Option<&str>,
    now_ms: i64,
) -> Result<(), StoreError> {
    check_key(key)?;
    if !state.is_terminal() {
        return Err(StoreError::Sql(format!(
            "pass_state: {:?} is not a terminal state", state)));
    }
    l.conn().execute(
        "INSERT INTO extract_pass_state(turn_uid, pass_id, family, state, attempt,
                                        next_retry_at_ms, fingerprint, reason, run_id, updated_at_ms)
         VALUES(?1,?2,?3,?4,0,NULL,NULL,?5,NULL,?6)
         ON CONFLICT(turn_uid, pass_id, family) DO UPDATE SET
           state=?4, reason=?5, next_retry_at_ms=NULL, updated_at_ms=?6",
        params![key.turn_uid, key.pass_id, key.family, state.slug(), reason, now_ms])?;
    Ok(())
}

/// 失败退避：`attempt+1`，`next_retry_at_ms = now_ms + backoff_ms`，状态回到 `Pending`
/// （`Running` 是失败前的中间态；`Stale` 也走这条回归——它的重跑理由在指纹里，但退避
/// 只有落在 `pending` 上才生效：`should_run` 对 `stale` 无条件点头，留在 `stale` 就等于
/// 每轮重选、`next_retry_at_ms` 形同虚设）。
///
/// 例外是**除 `stale` 外的终态行**（`done`/`empty`/`rejected`/`quarantined`）：一次迟到的
/// 重试登记不许把它们改回 `pending`（那会让 `should_run` 对已完成的批次重新点头）。
/// 这些行的 `attempt` 仍会 +1，但状态与"要不要跑"不变。
pub fn note_attempt(
    l: &LedgerHandle,
    key: PassKey<'_>,
    backoff_ms: i64,
    now_ms: i64,
) -> Result<(), StoreError> {
    check_key(key)?;
    // `CASE` 的"保持原状"集合 = 终态清单**减 `stale`**：`IN` 用同一个 `TERMINAL_STATES`，
    // 减法由 `<> ?5`（`PassState::Stale.slug()`）兑现，不手抄第二份清单。手抄四格曾漏掉
    // `stale`（迟到登记把 stale 当成普通失败行，`next_retry_at_ms` 无人看），改抄五格又让
    // `stale` 丢掉退避——两次漂移都是第二份字面的代价。
    let sql = format!(
        "INSERT INTO extract_pass_state(turn_uid, pass_id, family, state, attempt,
                                        next_retry_at_ms, fingerprint, reason, run_id, updated_at_ms)
         VALUES(?1,?2,?3,'pending',1,?4,NULL,NULL,NULL,?6)
         ON CONFLICT(turn_uid, pass_id, family) DO UPDATE SET
           attempt=extract_pass_state.attempt + 1,
           next_retry_at_ms=?4,
           state=CASE WHEN extract_pass_state.state IN {TERMINAL_STATES}
                       AND extract_pass_state.state <> ?5
                      THEN extract_pass_state.state ELSE 'pending' END,
           updated_at_ms=?6");
    l.conn().execute(
        &sql,
        params![key.turn_uid, key.pass_id, key.family, now_ms + backoff_ms,
                PassState::Stale.slug(), now_ms])?;
    Ok(())
}

/// 试满次数就隔离：先记一次失败（[`note_attempt`] 的全部语义原样），再读回这一行。
///
/// 交回的永远是"这一格接下来算什么态"：
/// - 仍停在 `pending` 且 `attempt >= MAX_PASS_ATTEMPTS` ⇒ 落 `quarantined(retry_exhausted)`，
///   交回 `Quarantined`。调用方据此把这一批发成**成功**（日路径继续下一批，不是整族停摆）；
/// - 其余（还没到上限；或这一行本来就是终态 / `stale`）⇒ 交回 `Pending`，调用方照旧按失败重试。
///
/// **只对 `pending` 生效**：`done`/`empty`/`rejected`/`quarantined` 是别人的结论，
/// `stale` 是"判据变了、要重跑"，这四种的行一个字节都不改（`note_attempt` 的终态保护
/// 已经把它们挡在 `pending` 之外，这里只是不把它们的 `attempt` 误读成"该隔离了"）。
///
/// 题外但必需：`attempt` 能跨进程数到 3，靠的是 [`mark_running`] 在 `pending` 重试那一格
/// 不清零。每次开跑都清零的话，这一格永远读到 1，上限形同虚设。
pub fn note_attempt_or_quarantine(
    l: &LedgerHandle,
    key: PassKey<'_>,
    backoff_ms: i64,
    now_ms: i64,
) -> Result<PassState, StoreError> {
    note_attempt(l, key, backoff_ms, now_ms)?;
    // 读回本行：读不到是库被并发改了（`note_attempt` 刚写过），按"还没隔离"处理，
    // 下一轮会重新记账——这里不编一个不存在的 attempt。
    let Some(record) = get(l, key)? else { return Ok(PassState::Pending) };
    if record.state == PassState::Pending && record.attempt >= MAX_PASS_ATTEMPTS {
        mark_terminal(l, key, PassState::Quarantined, Some(RETRY_EXHAUSTED_REASON), now_ms)?;
        return Ok(PassState::Quarantined);
    }
    Ok(PassState::Pending)
}

/// 运维兜底：把过期的 `running` 行退回 `pending`，让选批能重新捡起它们。
///
/// 为什么只走 `note_attempt` 而不直接 `UPDATE state='pending'`：退避、attempt 递增、
/// 终态保护这三件事都已经在 `note_attempt` 里；这里再写一遍就是第二份状态机。
/// `older_than_ms` 用来避开"刚开跑、模型还在路上"的行；`backoff_ms=0` 表示立刻可重选。
pub fn reclaim_running(
    l: &LedgerHandle,
    pass_id: &str,
    older_than_ms: i64,
    backoff_ms: i64,
    now_ms: i64,
) -> Result<usize, StoreError> {
    check_pass_id(pass_id)?;
    let cutoff = now_ms.saturating_sub(older_than_ms.max(0));
    let rows: Vec<(String, String)> = {
        let mut st = l.conn().prepare(
            "SELECT turn_uid, family FROM extract_pass_state
             WHERE pass_id=?1 AND state='running' AND updated_at_ms <= ?2
             ORDER BY updated_at_ms, turn_uid")?;
        let mapped = st.query_map(params![pass_id, cutoff], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        mapped.collect::<Result<Vec<_>, _>>()?
    };
    for (turn_uid, family) in &rows {
        note_attempt(
            l,
            PassKey { turn_uid, pass_id, family },
            backoff_ms,
            now_ms,
        )?;
    }
    Ok(rows.len())
}

// ── W7b：按轮定向回拨终态行（重蒸指定的一批轮） ────────────────────────────────
//
// 为什么需要这条口子：重选只有三支开口（指纹不同 / `stale` / `pending` 且到期），而
// [`reclaim_running`] 只收 `running`、对账只按水位缺口批量标。操作员要的是"这几轮这一条
// pass 重蒸"，不是"整段缺口重蒸"，也不是手工改库。
//
// 三条纪律：
// 1. **只动状态列与理由/退避**：不删行、不换指纹、不动 `attempt` 与 `run_id`。指纹留着，
//    `should_run` 才会走 `Stale => true` 那一支；换指纹是"输入变了"的证据，不是这里的活。
// 2. **非终态一个字节都不写**：`running` 的行可能是活的、`pending` 的行在退避窗口里，
//    把它们当"回拨"改掉会把读数与退避一起弄乱。它们只进计数。
// 3. **单事务**：一条 `UPDATE` 抛错就整体回滚，不留"翻了一半"的批。

/// 一次定向回拨的三个计数（`hub distill reset` 的读数就是这三格）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResetCounts {
    /// 真的写成 `stale` 的**行**数（fact 的一轮可能有多族，各族各一行）。
    pub reset: usize,
    /// 命中这一格、但状态不在可回拨集合里的**行**数。
    pub skipped_non_terminal: usize,
    /// 库里根本没有这一格行的**轮**数。
    pub not_found: usize,
}

/// 可回拨的状态 = 终态清单**减 `stale`**：`stale` 本来就是要重跑的形状，再写一次只会
/// 刷新时间戳，读数还会把"本来就是 stale"报成"这一次回拨掉的"。
fn is_reset_target(state: PassState) -> bool {
    state.is_terminal() && state != PassState::Stale
}

/// 按 `(turn, pass)` 把终态行回拨成 `stale(reason_code)`，返回三项计数。
///
/// 每一轮先读它在这一条 pass 上的**全部族行**：没有行 ⇒ `not_found` 计一轮（不是错误：
/// 交进来的是一批轮，其中几轮还没蒸过是常态）；有行但状态不在可回拨集合里 ⇒
/// `skipped_non_terminal` 计那一行，一个字节都不写。`turns` 的重复项由调用方先去重
/// （重复的那一遍会把刚写成的 `stale` 读成"跳过"，读数会随文件顺序变）。
///
/// `reason_code` 是调用方的 slug（与 `note_attempt` 的退避理由同一族）；空串拒绝——
/// 一个 `stale` 行带着空理由，读的人分不出"为什么回头"。
pub fn reset_terminal_to_stale(
    l: &LedgerHandle,
    pass_id: &str,
    turns: &[String],
    reason_code: &str,
    now_ms: i64,
) -> Result<ResetCounts, StoreError> {
    check_pass_id(pass_id)?;
    if reason_code.is_empty() {
        return Err(StoreError::Sql("pass_state: empty reset reason".into()));
    }
    let mut counts = ResetCounts::default();
    if turns.is_empty() {
        return Ok(counts);
    }
    // `IN` 与 `<>` 读的是同一个常量：`stale` 的排除由 `?5` 兑现（那里的值也是
    // `PassState::Stale` 的 slug），不手抄第二份清单——`note_attempt` 的 `CASE` 同一条取向。
    let sql = format!(
        "UPDATE extract_pass_state SET state=?5, reason=?3, next_retry_at_ms=NULL,
                                       updated_at_ms=?4
         WHERE turn_uid=?1 AND pass_id=?2 AND state IN {TERMINAL_STATES} AND state <> ?5");
    let tx = l.conn().unchecked_transaction()?;
    {
        let mut read = tx.prepare(
            "SELECT state FROM extract_pass_state WHERE turn_uid=?1 AND pass_id=?2")?;
        let mut write = tx.prepare(&sql)?;
        for turn in turns {
            let states: Vec<String> = read
                .query_map(params![turn, pass_id], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            if states.is_empty() {
                counts.not_found += 1;
                continue;
            }
            let mut resettable = false;
            for raw in &states {
                // 读不出的 slug 与 [`get`] 同一条纪律：响亮，不折成"还没跑"。
                let state = PassState::parse(raw).ok_or_else(|| {
                    StoreError::Sql(format!("pass_state: unreadable state {raw}"))
                })?;
                if is_reset_target(state) {
                    resettable = true;
                } else {
                    counts.skipped_non_terminal += 1;
                }
            }
            if resettable {
                counts.reset += write.execute(params![
                    turn,
                    pass_id,
                    reason_code,
                    now_ms,
                    PassState::Stale.slug()
                ])?;
            }
        }
    }
    tx.commit()?;
    Ok(counts)
}

/// 抽取完成、窄审还没跑：把这一行改成 `pending(review_pending)`，**保留**指纹/`run_id`/
/// `attempt`，**清空**退避（`next_retry_at_ms=NULL`）——"等审核"不是"等退避到期"。
///
/// 这是 S3b-4b 的 `distill --no-review` 逐轮落的状态：产出已经入队，审核由后续
/// `hub review run` 补。调用方必须先 [`mark_running`]（那一行是这一格的开工记录）；
/// 0 行命中是调用方 bug，返回 `StoreError::Sql`，**不**替它插一行没有指纹的 `pending`
/// （那会把"没跑过"读成"跑过、等审核"，`should_run` 会因此跳过这一轮）。
pub fn mark_awaiting_review(
    l: &LedgerHandle,
    key: PassKey<'_>,
    now_ms: i64,
) -> Result<(), StoreError> {
    check_key(key)?;
    let changed = l.conn().execute(
        "UPDATE extract_pass_state SET state='pending', reason=?4, next_retry_at_ms=NULL,
                                       updated_at_ms=?5
         WHERE turn_uid=?1 AND pass_id=?2 AND family=?3",
        params![key.turn_uid, key.pass_id, key.family, REVIEW_PENDING_REASON, now_ms])?;
    if changed == 0 {
        return Err(StoreError::Sql(
            "pass_state: mark_awaiting_review without mark_running".to_string(),
        ));
    }
    Ok(())
}

/// 该不该跑这一格：
/// - 无行 ⇒ true（从没跑过）；
/// - 指纹不同 ⇒ true（输入/prompt/模型变了；旧行由下一次 `mark_running` 覆盖，不物理删）；
/// - `stale` ⇒ true；
/// - `pending` 且（没有 `next_retry_at_ms` 或已到期 `<= now`）⇒ true；
/// - 其余 false（`running` 不重入；`done`/`empty`/`rejected`/`quarantined` 要人工或新指纹才再跑）。
pub fn should_run(
    l: &LedgerHandle,
    key: PassKey<'_>,
    fingerprint: &str,
    now_ms: i64,
) -> Result<bool, StoreError> {
    check_key(key)?;
    let Some(r) = get(l, key)? else { return Ok(true) };
    if r.fingerprint.as_deref() != Some(fingerprint) {
        return Ok(true);
    }
    Ok(match r.state {
        PassState::Stale => true,
        PassState::Pending => r.next_retry_at_ms.is_none_or(|due| due <= now_ms),
        _ => false,
    })
}

// ── S3b：lag 换源到 pass 表（只读） ──────────────────────────────────────────
//
// 每天的抽取路径写的是 `extract_pass_state`（一轮多格），legacy `extract_state` 是 0 行；
// 所以"已蒸过几轮、最后一次什么时候"只能从这一张表读。两条纪律：
// 1. **只认终态**（`TERMINAL_STATES`）：`running`/`pending` 的轮还没蒸完，把它们算进已蒸
//    就是把"正在跑"读成"已经好了"（假绿的头号形状）。
// 2. **不并两张表**：legacy 表今天没有写者，把它的行并进来只会让同一个数有两个来源；
//    两表并集的计数还会把"只有半套 pass 的轮"读成整轮完成。lag 的错表守卫在
//    `mcp/tests/p5_lag_quota.rs`（legacy 有行、pass 表空 ⇒ lag 仍等于总轮数）。
//
// 按 `turn_uid` 去重是这条读数的**单位**：lag 的分母是轮，不是格；同一轮 naming/fact/event
// 都到终态也只算一轮。

/// 已有终态 pass 行的轮数（`count(DISTINCT turn_uid)`）。
///
/// `status.distill_lag_turns` 的减数。saturating 的处理在调用方：`purge` 留下的孤立终态行
/// 会让它短时超过转录总数，那时的正确读数是"没有落后"，不是负数、也不是报错。
pub fn count_terminal_turns(l: &LedgerHandle) -> Result<i64, StoreError> {
    let sql = format!(
        "SELECT count(DISTINCT turn_uid) FROM extract_pass_state WHERE state IN {TERMINAL_STATES}");
    Ok(l.conn().query_row(&sql, [], |r| r.get(0))?)
}

/// 最后一次终态落下的时刻；`None` = 一行终态都没有（"从没蒸完过一轮"是读数，不是错误）。
///
/// `status.last_extract_at_ms` 的唯一来源。取的是**最新的终态行 `updated_at_ms`**——它是这一轮
/// 真的蒸完的时刻，不是水位游标（`job_watermark` 的 daily 路径今天根本不写）。
///
/// 用 `max(updated_at_ms)`（brief 原式）而不是 `ORDER BY … DESC LIMIT 1`：禁令禁的是**跨两张表
/// 各取一个数再取大**，而这里只在一张表、一列上要最大值；排序式还要多一趟临时 B-tree。
/// 空表时聚合照旧返回一行 NULL ⇒ `None`，与"没有终态行"同形。
pub fn last_terminal_at_ms(l: &LedgerHandle) -> Result<Option<i64>, StoreError> {
    let sql = format!(
        "SELECT max(updated_at_ms) FROM extract_pass_state WHERE state IN {TERMINAL_STATES}");
    Ok(l.conn().query_row(&sql, [], |r| r.get::<_, Option<i64>>(0))?)
}

/// 每个 pass 的终态轮数，按 `pass_id` 升序（`count(DISTINCT turn_uid)`，同一轮多格只算一次）。
///
/// 运维面用（doctor 的 `pass_coverage` 曾读它，现改按 `count_by_state` 分桶）：lag 只有一个总数，
/// 说不出"卡在哪一支"；这一支把六个 pass 各自的覆盖摊开。空表回空表——"还没有任何 pass 跑完过"是读数。
pub fn count_terminal_by_pass(l: &LedgerHandle) -> Result<Vec<(String, i64)>, StoreError> {
    let sql = format!(
        "SELECT pass_id, count(DISTINCT turn_uid) FROM extract_pass_state
         WHERE state IN {TERMINAL_STATES} GROUP BY pass_id ORDER BY pass_id");
    let mut st = l.conn().prepare(&sql)?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// 读数：按状态计数，slug 字典序（`ORDER BY state` 就是这一序，`water::count_states` 的同形）。
/// 只数一个 pass；`family` 的细分是 fact 评测的事（S2a-2 再按 family 加读数）。
pub fn count_by_state(l: &LedgerHandle, pass_id: &str) -> Result<Vec<(PassState, i64)>, StoreError> {
    check_pass_id(pass_id)?;
    let rows = {
        let mut st = l.conn().prepare(
            "SELECT state, count(*) FROM extract_pass_state
             WHERE pass_id=?1 GROUP BY state ORDER BY state")?;
        let mapped = st.query_map(
            params![pass_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        mapped.collect::<Result<Vec<_>, _>>()?
    };
    rows.into_iter()
        .map(|(s, n)| {
            PassState::parse(&s).map(|p| (p, n)).ok_or_else(|| {
                StoreError::Sql(format!("pass_state: unreadable state {s}"))
            })
        })
        .collect()
}

// ── H7c1：按 turn 批量读（接管对账的唯一读法） ─────────────────────────────────
//
// 对账要回答"这些轮里哪些格已经 done、各状态各几行"，一轮一轮跑 `get` 会退化成
// N 次 prepare；这里只开一条按 `turn_uid IN (...)` 的只读查询。它不带指纹/attempt/
// 时间戳——对账的读者只要"哪一格、什么状态、什么理由"。

/// 一行批量读数。[`rows_for_turns`] 是这一行的唯一产地。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassStateRow {
    pub turn_uid: String,
    pub pass_id: String,
    pub family: String,
    pub state: PassState,
    pub reason: Option<String>,
}

/// `rows_for_turns` 的每块轮数。SQLite 的变量上限（旧版 999）之下留足余量；
/// 与 `hub::ownership` 的同族分块（500/块）同口径。
const ROWS_FOR_TURNS_CHUNK: usize = 500;

/// 按 turn 批量读这些轮的全部 pass 行，按 `(turn_uid, pass_id, family)` 升序。
///
/// 三条边界：
/// ① 空入参直接空表（"没有轮可查"是读数，不是错误，也不该为此开一次连接）；
/// ② 分块查询（`ROWS_FOR_TURNS_CHUNK`/块）绕开 SQLite 变量上限；各块先汇总再统一排序——
///    调用方拿到的序与分块方式无关；
/// ③ 库里躺着读不出的 `state` 仍然**响亮**（`StoreError::Sql`），与 [`get`] 同一条纪律：
///    不折成 `Pending`/跳过。
pub fn rows_for_turns(l: &LedgerHandle, uids: &[String]) -> Result<Vec<PassStateRow>, StoreError> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let mut out: Vec<PassStateRow> = Vec::new();
    for chunk in uids.chunks(ROWS_FOR_TURNS_CHUNK) {
        let slots: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT turn_uid, pass_id, family, state, reason
             FROM extract_pass_state WHERE turn_uid IN ({})",
            slots.join(",")
        );
        let mut st = l.conn().prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (turn_uid, pass_id, family, raw, reason) = row?;
            let state = PassState::parse(&raw).ok_or_else(|| {
                StoreError::Sql(format!("pass_state: unreadable state {raw}"))
            })?;
            out.push(PassStateRow { turn_uid, pass_id, family, state, reason });
        }
    }
    out.sort_by(|a, b| {
        (&a.turn_uid, &a.pass_id, &a.family).cmp(&(&b.turn_uid, &b.pass_id, &b.family))
    });
    Ok(out)
}

/// 这些轮在这一条 pass 上**已经落过终态结论**的那些（`done`/`empty`/`rejected`/`quarantined`）。
///
/// 选批判据（lesson 的选批用它）：批内每一轮都已有终态 ⇒ 这一批不必再烧模型；只剩新轮时
/// 只把新轮组进下一批。`stale` **不算**——它是"判据变了、要重跑"的形状，把它读成已完成
/// 会让重跑入口失效。`running`/`pending` 同样不算（还在跑 / 等重试）。
///
/// 三条边界与 [`rows_for_turns`] 同源：空 `uids` ⇒ 空集（不开查询）；分块读；坏 `state`
/// 仍然响亮（`StoreError::Sql`），不折成"没跑过"。
pub fn settled_turns(
    l: &LedgerHandle,
    pass_id: &str,
    uids: &[String],
) -> Result<HashSet<String>, StoreError> {
    check_pass_id(pass_id)?;
    let mut out = HashSet::new();
    for row in rows_for_turns(l, uids)? {
        if row.pass_id != pass_id {
            continue;
        }
        if matches!(
            row.state,
            PassState::Done | PassState::Empty | PassState::Rejected | PassState::Quarantined
        ) {
            out.insert(row.turn_uid);
        }
    }
    Ok(out)
}

// ── S2e-3：拒收原文证据（`extract_pass_artifact`） ───────────────────────────────
//
// 拒收只留一个 `reason` slug 时，冒烟分不出"模型输出了多键/缺键/引用错/覆盖缺"里的哪一种。
// 这一层把失败那一次调用的**模型原文**限长存进 ledger，供人/工具直接读；它不解析、不脱敏、
// 不判定——原文只进本地 `ledger.db`，不打印到 stdout/stderr，不进 25 表。
//
// 纪律与上面同源：非法 `stage`/空 `pass_id` 响亮（`StoreError::Sql`），读侧与写侧同一道闸；
// 截断按**字符**边界（不切坏 UTF-8）；只追加、不覆盖（`latest_artifact` 取最新一条）。

/// artifact 原文的字符上限（设计 `2026-10-26-s2-prompt-v0` §3 的"限长"）。
pub const ARTIFACT_MAX_CHARS: usize = 64_000;

/// 一次调用只有两个会留下原文的现场：抽取与审核。其余失败（LLM 故障）根本没有原文。
pub const ARTIFACT_STAGES: [&str; 2] = ["extract", "review"];

/// 一条拒收原文。[`latest_artifact`] 是这一行的唯一读法。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassArtifact {
    pub pass_id: String,
    pub family: String,
    pub stage: String,
    pub run_id: String,
    pub turn_uid: String,
    pub raw: String,
    pub created_at_ms: i64,
}

fn check_artifact_inputs(pass_id: &str, stage: &str) -> Result<(), StoreError> {
    if pass_id.is_empty() {
        return Err(StoreError::Sql("pass_state: empty artifact pass_id".into()));
    }
    if !ARTIFACT_STAGES.contains(&stage) {
        return Err(StoreError::Sql(format!("pass_state: unknown artifact stage {stage}")));
    }
    Ok(())
}

/// 按字符边界截断到 `max` 个字符（`char_indices` 只落在合法的 UTF-8 边界上，不 panic）。
fn truncate_chars(raw: &str, max: usize) -> String {
    match raw.char_indices().nth(max) {
        Some((index, _)) => raw[..index].to_string(),
        None => raw.to_string(),
    }
}

/// 记一条拒收原文：`raw` 截断到 [`ARTIFACT_MAX_CHARS`] 字符后**追加**一行（同格子重跑不覆盖，
/// `latest_artifact` 读最新）。`family` 只对 fact 取值，其余 pass 传空串；`turn_uid` 取该批
/// 第一轮。调用方（runner）用 `let _ =` 记账——artifact 写失败不许掩盖真正的解析失败。
#[allow(clippy::too_many_arguments)]
pub fn record_artifact(
    l: &LedgerHandle,
    pass_id: &str,
    family: &str,
    stage: &str,
    run_id: &str,
    turn_uid: &str,
    raw: &str,
    now_ms: i64,
) -> Result<(), StoreError> {
    check_artifact_inputs(pass_id, stage)?;
    let raw = truncate_chars(raw, ARTIFACT_MAX_CHARS);
    l.conn().execute(
        "INSERT INTO extract_pass_artifact(pass_id, family, stage, run_id, turn_uid, raw,
                                           created_at_ms)
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![pass_id, family, stage, run_id, turn_uid, raw, now_ms])?;
    Ok(())
}

/// 最近一条同 `(pass_id, family, stage)` 的 artifact（`created_at_ms` 大的赢，同刻后写的赢）。
/// 没有 ⇒ `None`（"这个格子没拒收过"，不是"原文为空"）。
pub fn latest_artifact(
    l: &LedgerHandle,
    pass_id: &str,
    family: &str,
    stage: &str,
) -> Result<Option<PassArtifact>, StoreError> {
    check_artifact_inputs(pass_id, stage)?;
    let mut st = l.conn().prepare(
        "SELECT pass_id, family, stage, run_id, turn_uid, raw, created_at_ms
         FROM extract_pass_artifact
         WHERE pass_id=?1 AND family=?2 AND stage=?3
         ORDER BY created_at_ms DESC, id DESC LIMIT 1")?;
    let mut rows = st.query_map(params![pass_id, family, stage], |r| {
        Ok(PassArtifact {
            pass_id: r.get(0)?,
            family: r.get(1)?,
            stage: r.get(2)?,
            run_id: r.get(3)?,
            turn_uid: r.get(4)?,
            raw: r.get(5)?,
            created_at_ms: r.get(6)?,
        })
    })?;
    match rows.next() {
        Some(Ok(a)) => Ok(Some(a)),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}
