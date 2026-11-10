//! 转录检索面（W38a 刀 A，设计 2026-11-08-w38a-transcript-search §二.3/§二.4/§三）：
//! `turn_shadow` FTS 命中 + 会话归属读闸 + snippet + coverage 读数。
//!
//! 三条读数（[`search_turns`] / [`session_turns`] / [`visible_sessions`]）共用
//! **一个**判定函数 [`session_visible`]——第二份真值表就是第二处漂移的机会：
//! turn 的可见性由**其会话归属**决定，判定数据就在 transcript.db `sessions` 行上
//! （`owner_agent_id` / `owner_principal_id` 两列已有）：
//! ① `viewer.agent_id == sessions.owner_agent_id` ⇒ 可见；
//! ② 否则 viewer 的 principal 已知（非 None/空/`unknown`，`principal_is_known`）
//!    且 `== sessions.owner_principal_id` ⇒ 可见（同主人互读）；
//! ③ 其余一律不可见，**fail-closed**：无 principal 的 viewer 只见自己 agent 的会话；
//!    owner 列 NULL 的旧行两条都吃不到（NULL 永不参与相等）。
//! `Viewer::maintenance` 在转录面**没有**特权通道，人面 viewer（`Viewer::human`）
//! 按 principal 规则同判——转录面无锚，环境锚例外不适用（细案 §二.3）。
//!
//! 检索一律 JOIN `turns`（权威面）：purge/restore 留下的孤儿影子行自然缺席，
//! coverage 读数显形（细案 §二.1）。gate 判定在 Rust 侧逐行做（转录面量小，
//! 细案 §二.6 的代价账），SELECT 把 owner 两列一并取回，不为闸写第二份 SQL。
//!
//! 第四个读数 [`turn_window`] 按 `turn_uid` 取同会话窗口（W39 细案 §3.4）：闸同样复用
//! [`session_visible`]；不可见与轮不存在**同形**（`Ok(None)`，不泄露存在性）；
//! 前后邻居各 ≤ [`MAX_WINDOW_SIDE`] 轮。
//!
//! W39 刀 C 起判定有两个**信任臂**（[`Trust`]）：`Ownership` 是本地/中心模式的 W38a
//! 归属判定，`Lan` 是局域网远端通道的**明示旁路**（不是默认）。两条臂共用同一个判定
//! 函数 [`session_visible`]，不写第二份 SQL、不写第二份可见性表。

use rc_index::analyzer::query_tokens;
use rc_memory::gate::Viewer;
use rc_memory::writers_v4::principal_is_known;
use rc_store::TranscriptHandle;
use rusqlite::named_params;
use rusqlite::OptionalExtension;
use rc_store::ddl::TURN_SHADOW;
use serde::{Deserialize, Serialize};

/// 结果条数上限（细案 §二.4：`limit ≤ 20`）。
pub const MAX_TURN_LIMIT: usize = 20;
/// 结果条数缺省（细案 §二.4：缺省 10）。`limit = 0` 时取它。
pub const DEFAULT_TURN_LIMIT: usize = 10;
/// 会话回放的条数上限（细案 §三：`limit ≤ 200`）；`limit = 0` 时也取它。
pub const MAX_SESSION_LIMIT: usize = 200;
/// 按轮取窗口时前后各侧的上限（W39 细案 §3.4：`before/after ≤ 10`）。
pub const MAX_WINDOW_SIDE: usize = 10;
/// snippet 硬上限（**含**省略号，细案 §二.4）。
const SNIPPET_MAX_CHARS: usize = 240;
const ELLIPSIS: char = '…';

/// 检索面的错误。**没有一支是"没有结果"的形状**（G7 同一条：沉默必须可枚举）——
/// 空读数是 `Ok(hits = [])`，被闸拒也是 `Ok(hits = [])`，两者用信封的 `matched`
/// 分型（细案 §五空读数，D32 教训）；只有"没法匹配"与"读不出来"是错误。
#[derive(Debug, thiserror::Error)]
pub enum TranscriptSearchError {
    /// 与 `rc_index::IndexError::NoQueryTokens` 同一形状：查询切不出任何词
    /// 不是"没有匹配"，是"没法匹配"——绝不许退化成全表扫。
    #[error("query yields no index tokens")]
    NoQueryTokens,
    /// prepare 期错误。SQL 文本全是静态 schema 词（用户查询只经 `?` 绑定），
    /// 载荷过 [`sqlite_text`] 消毒（守卫 12）。
    #[error("sqlite: {0}")]
    Sql(String),
    /// step 期错误：FTS5 的原串可能回显用户查询片段 ⇒ 无载荷（裁定 R18-2 同一条）。
    /// 运维面只需要知道"这一层坏了 ⇒ 跑 doctor"。
    #[error("transcript index unreadable")]
    LayerUnreadable,
}

/// 守卫 12 在构造点的消毒。规则与 `rc-index/src/rebuild.rs`、`rc-memory/src/lib.rs`
/// 的同名函数逐字对齐——这已是**第三份副本**（前两处的注释都写明"第三份实现出现前
/// 必须抽"）；本轮受允许文件集约束无法落公共模块（rc-store 的 lib.rs 不在清单内），
/// 抽取挂账，与那两处同一条账。
fn sqlite_text(e: &rusqlite::Error) -> String {
    let head = e.to_string().lines().next().unwrap_or_default().to_string();
    let cut: String = head.chars().take(120).collect();
    if cut.contains('/') || cut.contains('\\') || cut.to_lowercase().contains(".db") {
        return "error detail redacted (path-like text)".to_string();
    }
    cut
}

fn sql_err(e: rusqlite::Error) -> TranscriptSearchError {
    TranscriptSearchError::Sql(sqlite_text(&e))
}

fn unreadable(_: rusqlite::Error) -> TranscriptSearchError {
    TranscriptSearchError::LayerUnreadable
}

/// 读闸的信任臂（W39 细案 §二.2/§3.1）：判定仍只有 [`session_visible`] 一处实现。
///
/// - [`Trust::Ownership`]：本地/中心模式，沿用 W38a 归属判定（agent 自读 / 同 principal 互读）；
/// - [`Trust::Lan`]：局域网远端通道（W39 用户裁定），已配对设备读全部会话，不做归属过滤。
///   它是远端通道的**明示旁路**，不是默认——本地三个读数与工具面都走 `Ownership`。
#[derive(Clone, Copy)]
pub enum Trust<'a> {
    /// 本地/中心模式：沿用 W38a 归属判定（agent 自读 / 同 principal 互读）。
    Ownership(&'a Viewer),
    /// 局域网远端通道（W39 用户裁定）：已配对设备读全部会话，不做归属过滤。
    Lan,
}

/// 转录面读闸的**唯一判定点**（四个读数共用；`crates/rc-ledger/tests/turn_shadow.rs` 的闸红线钉住它）。
///
/// 返回 `true` = 这个会话对这位读者可见。owner 两列从 `sessions` 行取，
/// `None`（NULL）永不参与相等——旧行两条都吃不到，方向只会更窄。
/// `Trust::Lan` 直接放行（远端通道的明示旁路）；`Trust::Ownership(viewer)` 走下面三支。
///
/// **变异点**（工单 6 的闸红线变异证明）：删掉/注释下面"principal 判定臂"那一段，
/// `cross_principal_session_is_invisible_to_search_and_the_gate_mutation_flips_this_test`
/// 当场红（同 principal 互读断言先红）；恢复后回到绿。
fn session_visible(
    owner_agent_id: Option<&str>,
    owner_principal_id: Option<&str>,
    trust: Trust<'_>,
) -> bool {
    let viewer = match trust {
        Trust::Lan => return true,
        Trust::Ownership(viewer) => viewer,
    };
    // ① owner 自读：只比 agent（同 principal 不在这一条放行）。
    if owner_agent_id == Some(viewer.agent_id.as_str()) {
        return true;
    }
    // ② principal 判定臂（同主人互读，W37 裁定②语义的转录面映射）。
    //    fail-closed 的两半都在 known_principal 里：viewer 拿不出**已知** principal
    //    （None/空/`unknown`）⇒ 这一臂永不命中。
    if let (Some(p), Some(owner)) = (known_principal(viewer), owner_principal_id) {
        if p == owner {
            return true;
        }
    }
    // ③ 其余一律不可见。
    false
}

/// viewer 的 principal 过掉"未知"之后的样子：`None` = 拿不出已知身份。
/// 口径只有一条：`rc_memory::writers_v4::principal_is_known`（空/`unknown` = 不知道）。
fn known_principal(viewer: &Viewer) -> Option<&str> {
    viewer.principal_id.as_deref().filter(|p| principal_is_known(p))
}

/// MATCH 串：每个词包成字符串字面量、内部引号翻倍，再 `OR` 起来。
/// 与 `rc_index::shadow::match_string` 同一条规则——那边是 `pub(crate)` 拿不到，
/// 本地重述；用户字节永远只经 `?1` 绑定进 MATCH，`*`/`-`/`NEAR` 因此只是词面
/// （`crates/rc-ledger/tests/turn_shadow.rs::match_syntax_characters_are_word_faces_not_operators` 钉住）。
fn match_string(tokens: &[String]) -> String {
    tokens
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// 覆盖读数（细案 §二.6）：影子行数 vs 权威轮数。缺口大提示 rebuild——
/// 不建 marker 表，一行读数够用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    pub indexed: i64,
    pub total: i64,
}

/// 一条命中轮。载荷与细案 §三 的工具返回逐格对应；snippet 只出命中轮原文片段
/// （原文本来就在转录层，不新增暴露面）。
///
/// W39 §二.4：归属必须随原文走——每条命中带它所在会话的 `owner_*` 两列
/// （SELECT 本来就为读闸取回这两列，这里只是让它们进结构体）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnHit {
    pub turn_uid: String,
    pub session_key: String,
    pub device_id: String,
    pub role: String,
    pub created_at_ms: i64,
    pub snippet: String,
    pub owner_agent_id: Option<String>,
    pub owner_principal_id: Option<String>,
}

/// [`search_turns`] 的响应信封。`matched` 是**闸前**命中数（JOIN turns 权威面之后、
/// 读闸之前）：`hits` 全空而它 > 0 ⇒ 命中都关在闸外；它也是 0 ⇒ 索引里根本没有——
/// 两个"空"由此可分型（细案 §五空读数）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnSearch {
    pub hits: Vec<TurnHit>,
    pub matched: i64,
    /// 可见命中超过 limit 被截断。
    pub truncated: bool,
    pub coverage: Coverage,
}

/// [`session_turns`] 回放的一轮（正文全文：同闸之下正文本来就可读）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTurn {
    pub turn_uid: String,
    pub role: String,
    pub body: String,
    pub created_at_ms: i64,
}

/// [`session_turns`] 的会话回放信封（W39 刀 E）：轮正文 + 该会话的归属两列与 device。
/// 归属取**第一条可见轮**所属 sessions 行（同 `session_key` 跨 device 时见
/// [`session_turns`] 的注释）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReplay {
    pub session_key: String,
    pub device_id: Option<String>,
    pub owner_agent_id: Option<String>,
    pub owner_principal_id: Option<String>,
    pub turns: Vec<SessionTurn>,
}

/// [`visible_sessions`] 的一行（细案 §三：会话清单的读数格）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_key: String,
    pub device_id: String,
    pub source_agent: String,
    pub project_hint: Option<String>,
    /// 该会话的轮数（含 system 轮——它数的是 turns 权威面）。
    pub turns: i64,
    pub first_ms: Option<i64>,
    pub last_ms: Option<i64>,
    /// 会话归属（W39 §3.1）：记忆层只有指针，归属必须随原文走。
    pub owner_agent_id: Option<String>,
    pub owner_principal_id: Option<String>,
}

/// [`turn_window`] 的窗口：目标轮所在会话的 key/device、归属两列与按升序排好的轮。
/// `truncated_before` / `truncated_after` 表示该侧还有更多轮没进来（多取一条探测得出）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnWindow {
    pub session_key: String,
    pub device_id: String,
    pub owner_agent_id: Option<String>,
    pub owner_principal_id: Option<String>,
    pub turns: Vec<SessionTurn>,
    pub truncated_before: bool,
    pub truncated_after: bool,
}

/// 转录检索（细案 §三 `recollect_search` 的库层）。FTS MATCH（查询侧走
/// `analyzer::query_tokens`，含引号逃逸语义）+ JOIN turns 取正文 + 会话归属读闸
/// + snippet（首个命中 token 为中心、≤240 字符、char 边界+省略号）+ coverage。
///
/// `trust` 必给（[`Trust`]，不是 `Option`）：转录面没有"不过闸"的调用——
/// 比记忆侧 `rank` 的 `Option` 更严（细案 §二.3：新读面结构性无法漏）。
/// `limit` 按 `1..=MAX_TURN_LIMIT` 收口，`0` 取 [`DEFAULT_TURN_LIMIT`]。
pub fn search_turns(
    t: &TranscriptHandle,
    trust: Trust<'_>,
    query: &str,
    limit: usize,
) -> Result<TurnSearch, TranscriptSearchError> {
    let toks = query_tokens(query);
    if toks.is_empty() {
        return Err(TranscriptSearchError::NoQueryTokens);
    }
    let limit = if limit == 0 { DEFAULT_TURN_LIMIT } else { limit.min(MAX_TURN_LIMIT) };
    let conn = t.conn();
    let match_str = match_string(&toks);
    let sql = format!(
        "SELECT {TURN_SHADOW}.turn_uid, t.session_key, t.device_id, t.role,
                t.created_at_ms, t.body, s.owner_agent_id, s.owner_principal_id
         FROM {TURN_SHADOW}
         JOIN turns t ON t.turn_uid = {TURN_SHADOW}.turn_uid
         JOIN sessions s ON s.session_key = t.session_key AND s.device_id = t.device_id
         WHERE {TURN_SHADOW} MATCH :match
         ORDER BY t.created_at_ms DESC, t.turn_uid"
    );
    let mut st = conn.prepare(&sql).map_err(sql_err)?;
    let rows = st
        .query_map(
            named_params! { ":match": match_str },
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                ))
            },
        )
        .map_err(unreadable)?;
    let raw = rows.collect::<Result<Vec<_>, _>>().map_err(unreadable)?;
    drop(st);
    let matched = raw.len() as i64;
    // 闸在 Rust 侧逐行判（唯一的判定函数）；snippet 只为真正返回的命中算。
    let mut hits = Vec::new();
    let mut visible = 0usize;
    for (uid, sk, dev, role, created, body, owner_agent, owner_principal) in &raw {
        if !session_visible(owner_agent.as_deref(), owner_principal.as_deref(), trust) {
            continue;
        }
        visible += 1;
        if hits.len() < limit {
            hits.push(TurnHit {
                turn_uid: uid.clone(),
                session_key: sk.clone(),
                device_id: dev.clone(),
                role: role.clone(),
                created_at_ms: *created,
                snippet: snippet(body, &toks),
                owner_agent_id: owner_agent.clone(),
                owner_principal_id: owner_principal.clone(),
            });
        }
    }
    let truncated = visible > limit;
    let coverage = read_coverage(conn)?;
    Ok(TurnSearch { hits, matched, truncated, coverage })
}

/// 覆盖读数：影子行数 vs 权威轮数。表读不出来是错误，不是 0（不许让
/// "索引层坏了"伪装成"零覆盖"）。
fn read_coverage(conn: &rusqlite::Connection) -> Result<Coverage, TranscriptSearchError> {
    let indexed: i64 = conn
        .query_row(&format!("SELECT count(*) FROM {TURN_SHADOW}"), [], |r| r.get(0))
        .map_err(sql_err)?;
    let total: i64 = conn
        .query_row("SELECT count(*) FROM turns", [], |r| r.get(0))
        .map_err(sql_err)?;
    Ok(Coverage { indexed, total })
}

/// 会话回放（细案 §三 `recollect_session` 的库层）：该会话的轮正文，按
/// `(created_at_ms, turn_uid)` 升序。同闸同语义：turn 的可见性由会话归属决定，
/// 所以**整个会话同判**——不可见 ⇒ `Ok(None)`，与"查无"同形，不泄露存在性。
///
/// 游标 `after_uid`：返回**严格排在该轮之后**的那一段。游标必须是**本会话内**的
/// turn_uid（它指向的轮找不到时给 `Ok(None)`——回卷到开头是"悄悄重放一遍"的形状，
/// 不许有）；跨会话的游标在本会话里本来就找不到，同样给 `Ok(None)`。
/// `limit` 按 `1..=MAX_SESSION_LIMIT` 收口，`0` 取 `MAX_SESSION_LIMIT`。
///
/// W39 刀 E：返回值从"轮数组"升成 [`SessionReplay`]——归属必须随原文走（细案 §二.4），
/// 空页（没有可见轮 / 游标落在末轮之后）仍是 `Ok(None)`，与旧空表同形。
/// `device_id` / `owner_*` 取**第一条可见轮**的 sessions 行：同一 `session_key` 跨 device
/// 时那是两条 sessions 行、两种归属，**以第一条可见轮所属 device 为准**（与读闸逐行判定
/// 同一口径；分页游标改不了这个基准，它只决定本页从哪一行开始）。
pub fn session_turns(
    t: &TranscriptHandle,
    trust: Trust<'_>,
    session_key: &str,
    limit: usize,
    after_uid: Option<&str>,
) -> Result<Option<SessionReplay>, TranscriptSearchError> {
    let limit = if limit == 0 { MAX_SESSION_LIMIT } else { limit.min(MAX_SESSION_LIMIT) };
    let conn = t.conn();
    let sql = "SELECT t.turn_uid, t.role, t.body, t.created_at_ms, s.device_id,
                s.owner_agent_id, s.owner_principal_id
         FROM turns t
         JOIN sessions s ON s.session_key = t.session_key AND s.device_id = t.device_id
         WHERE t.session_key = :session_key
         ORDER BY t.created_at_ms, t.turn_uid";
    let mut st = conn.prepare(sql).map_err(sql_err)?;
    let rows = st
        .query_map(
            named_params! { ":session_key": session_key },
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .map_err(unreadable)?;
    let raw = rows.collect::<Result<Vec<_>, _>>().map_err(unreadable)?;
    drop(st);
    // 闸逐行判（每一行带自己的 owner 两列）：同一 session_key 跨 device 时
    // 是两条 sessions 行、两种判定，"首行定生死"会放错人。
    let visible: Vec<_> = raw
        .into_iter()
        .filter(|(.., owner_agent, owner_principal)| {
            session_visible(owner_agent.as_deref(), owner_principal.as_deref(), trust)
        })
        .collect();
    // 一条可见轮都没有 ⇒ 与查无同形（不泄露存在性）。
    let Some((.., device_id, owner_agent, owner_principal)) = visible.first() else {
        return Ok(None);
    };
    let (device_id, owner_agent, owner_principal) =
        (device_id.clone(), owner_agent.clone(), owner_principal.clone());
    // 游标必须落在本会话（可见部分）的轮上；找不到 ⇒ 空页（不许回卷）。
    let start = match after_uid {
        None => 0,
        Some(after) => match visible.iter().position(|(uid, ..)| uid == after) {
            Some(i) => i + 1,
            None => return Ok(None),
        },
    };
    let turns: Vec<SessionTurn> = visible
        .into_iter()
        .skip(start)
        .take(limit)
        .map(|(uid, role, body, created, _, _, _)| SessionTurn {
            turn_uid: uid,
            role,
            body,
            created_at_ms: created,
        })
        .collect();
    // 空页（游标恰在末轮）与"没有可见轮"同形：都不去编一个没有原文的信封。
    if turns.is_empty() {
        return Ok(None);
    }
    Ok(Some(SessionReplay {
        session_key: session_key.to_string(),
        device_id: Some(device_id),
        owner_agent_id: owner_agent,
        owner_principal_id: owner_principal,
        turns,
    }))
}

/// 取窗口时一轮的四列映射（before 与 after 两条 SQL 的列序相同）。
fn session_turn_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionTurn> {
    Ok(SessionTurn {
        turn_uid: r.get(0)?,
        role: r.get(1)?,
        body: r.get(2)?,
        created_at_ms: r.get(3)?,
    })
}

/// 按 `turn_uid` 取窗口（W39 细案 §3.4）：目标轮 + 同会话内按 `(created_at_ms, turn_uid)`
/// 排序的前后邻居，各 ≤ [`MAX_WINDOW_SIDE`] 轮，整体升序。归属判定复用唯一判定点
/// [`session_visible`]；目标轮不存在与不可见**同形**（`Ok(None)`，不泄露存在性），
/// 两侧超限时 `truncated_*` 如实上报。
pub fn turn_window(
    t: &TranscriptHandle,
    trust: Trust<'_>,
    turn_uid: &str,
    before: usize,
    after: usize,
) -> Result<Option<TurnWindow>, TranscriptSearchError> {
    let before = before.min(MAX_WINDOW_SIDE);
    let after = after.min(MAX_WINDOW_SIDE);
    let conn = t.conn();
    // 目标轮：JOIN sessions 取回归属两列，闸的唯一输入。
    let target = conn
        .query_row(
            "SELECT t.session_key, t.device_id, t.created_at_ms, t.role, t.body,
                    s.owner_agent_id, s.owner_principal_id
             FROM turns t
             JOIN sessions s ON s.session_key = t.session_key AND s.device_id = t.device_id
             WHERE t.turn_uid = :turn_uid",
            named_params! { ":turn_uid": turn_uid },
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .optional()
        .map_err(sql_err)?;
    let Some((session_key, device_id, target_ms, role, body, owner_agent, owner_principal)) = target
    else {
        return Ok(None);
    };
    // 不复制判定逻辑：走同一个 session_visible。
    if !session_visible(owner_agent.as_deref(), owner_principal.as_deref(), trust) {
        return Ok(None);
    }
    // before：严格排在目标轮之前，DESC 多取一条判截断，再反转成升序。
    let mut st = conn
        .prepare(
            "SELECT t.turn_uid, t.role, t.body, t.created_at_ms
             FROM turns t
             WHERE t.session_key = :session_key AND t.device_id = :device_id
               AND (t.created_at_ms < :target_ms
                    OR (t.created_at_ms = :target_ms AND t.turn_uid < :target_uid))
             ORDER BY t.created_at_ms DESC, t.turn_uid DESC
             LIMIT :n",
        )
        .map_err(sql_err)?;
    let rows = st
        .query_map(
            named_params! {
                ":session_key": session_key,
                ":device_id": device_id,
                ":target_ms": target_ms,
                ":target_uid": turn_uid,
                ":n": (before + 1) as i64,
            },
            session_turn_row,
        )
        .map_err(unreadable)?;
    let mut prior = rows.collect::<Result<Vec<_>, _>>().map_err(unreadable)?;
    drop(st);
    let truncated_before = prior.len() > before;
    prior.truncate(before);
    prior.reverse();
    // after：严格排在目标轮之后，ASC 多取一条判截断。
    let mut st = conn
        .prepare(
            "SELECT t.turn_uid, t.role, t.body, t.created_at_ms
             FROM turns t
             WHERE t.session_key = :session_key AND t.device_id = :device_id
               AND (t.created_at_ms > :target_ms
                    OR (t.created_at_ms = :target_ms AND t.turn_uid > :target_uid))
             ORDER BY t.created_at_ms, t.turn_uid
             LIMIT :n",
        )
        .map_err(sql_err)?;
    let rows = st
        .query_map(
            named_params! {
                ":session_key": session_key,
                ":device_id": device_id,
                ":target_ms": target_ms,
                ":target_uid": turn_uid,
                ":n": (after + 1) as i64,
            },
            session_turn_row,
        )
        .map_err(unreadable)?;
    let mut next = rows.collect::<Result<Vec<_>, _>>().map_err(unreadable)?;
    drop(st);
    let truncated_after = next.len() > after;
    next.truncate(after);
    let mut turns = prior;
    turns.push(SessionTurn { turn_uid: turn_uid.to_string(), role, body, created_at_ms: target_ms });
    turns.extend(next);
    Ok(Some(TurnWindow {
        session_key,
        device_id,
        owner_agent_id: owner_agent,
        owner_principal_id: owner_principal,
        turns,
        truncated_before,
        truncated_after,
    }))
}

/// 可见会话清单（细案 §三 `recollect_sessions` 的库层）。同闸同语义：
/// 闸外的会话不出现（不泄露存在性）。清单是全量的——转录面的会话数是
/// 个位到十位量级（细案 §二.6 的代价账），刀 B 的工具层再收 limit。
pub fn visible_sessions(
    t: &TranscriptHandle,
    trust: Trust<'_>,
) -> Result<Vec<SessionSummary>, TranscriptSearchError> {
    let conn = t.conn();
    let sql = "SELECT s.session_key, s.device_id, s.source_agent, s.project_hint,
                      count(t.turn_uid), min(t.created_at_ms), max(t.created_at_ms),
                      s.owner_agent_id, s.owner_principal_id
               FROM sessions s
               LEFT JOIN turns t ON t.session_key = s.session_key AND t.device_id = s.device_id
               GROUP BY s.session_key, s.device_id
               ORDER BY s.session_key, s.device_id";
    let mut st = conn.prepare(sql).map_err(sql_err)?;
    let rows = st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
            ))
        })
        .map_err(unreadable)?;
    let raw = rows.collect::<Result<Vec<_>, _>>().map_err(unreadable)?;
    drop(st);
    Ok(raw
        .into_iter()
        .filter(|(.., owner_agent, owner_principal)| {
            session_visible(owner_agent.as_deref(), owner_principal.as_deref(), trust)
        })
        .map(|(session_key, device_id, source_agent, project_hint, turns, first, last, owner_agent, owner_principal)| {
            SessionSummary {
                session_key,
                device_id,
                source_agent,
                project_hint,
                turns,
                first_ms: first,
                last_ms: last,
                owner_agent_id: owner_agent,
                owner_principal_id: owner_principal,
            }
        })
        .collect())
}

/// snippet（细案 §二.4）：以**第一个**命中的查询词在原文里的出现位置为中心，
/// 截 ≤240 字符（含省略号），char 边界截断、被截的两侧各补一个省略号。
/// 命中词在原文里找不到（大小写折叠会变长的极端字符、jieba 词内含空白）时
/// 退回首窗——snippet 是读数不是判据，不许 panic。
fn snippet(body: &str, tokens: &[String]) -> String {
    let chars: Vec<char> = body.chars().collect();
    let center = tokens.iter().filter_map(|tok| ci_find(&chars, tok)).min().unwrap_or(0);
    window(&chars, center)
}

/// 以 center 为中心取 ≤`SNIPPET_MAX_CHARS - 2` 个原文字符，两侧被截则各补一个省略号
/// （总长 ≤`SNIPPET_MAX_CHARS`）。尾部顶住时把窗口往回挪，保住"中心"两个字。
fn window(chars: &[char], center: usize) -> String {
    let n = chars.len();
    const BODY_MAX: usize = SNIPPET_MAX_CHARS - 2; // 两侧省略号的预算
    let start0 = center.saturating_sub(BODY_MAX / 2);
    let end0 = (start0 + BODY_MAX).min(n);
    let start = start0.min(end0.saturating_sub(BODY_MAX));
    let end = end0;
    let mut out = String::new();
    if start > 0 {
        out.push(ELLIPSIS);
    }
    out.extend(chars[start..end].iter());
    if end < n {
        out.push(ELLIPSIS);
    }
    out
}

/// 在字符数组里大小写不敏感地找词，返回**字符**下标。逐字符小写展开后做
/// 普通子串查找并记回射——不在字节偏移上做大小写折叠（`to_lowercase` 会变长，
/// 字节偏移会错位），char 边界由构造保证。
fn ci_find(hay: &[char], needle: &str) -> Option<usize> {
    let n: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    if n.is_empty() || hay.is_empty() {
        return None;
    }
    let mut lower: Vec<char> = Vec::with_capacity(hay.len());
    let mut origin: Vec<usize> = Vec::with_capacity(hay.len());
    for (i, c) in hay.iter().enumerate() {
        for lc in c.to_lowercase() {
            lower.push(lc);
            origin.push(i);
        }
    }
    if n.len() > lower.len() {
        return None;
    }
    for start in 0..=(lower.len() - n.len()) {
        if lower[start..start + n.len()] == n[..] {
            return Some(origin[start]);
        }
    }
    None
}
