//! 一批待蒸的 turn。**它是从会话库读出来的行，不是模型给的行**——
//! 所有归属（uid、角色、时间）都从这里取，模型只许引用 uid，不许声明它的角色。
//!
//! C6b 起同一份文件也住**邻轮**（[`ExtractCtx`]）：目标轮在会话里 `created_at` 的前后各
//! 一轮，只为让抽取模型看得见语式/条件/指代。邻轮**不是批**——不进 `Batch::rows`、
//! 不进 `extract_pass_state`、不计 `turns` 读数、不能当 `evidence`。

use rc_memory::AnchorRole;
use rc_store::{MemoryHandle, StoreError, TranscriptHandle};
use rusqlite::{params, OptionalExtension, Row};

#[derive(Debug, Clone)]
pub struct TurnRow {
    pub turn_uid: String,
    pub role: String,
    pub body: String,
    pub created_at_ms: i64,
    /// 到达钟。`select_batch` 的翻页轴是它（A24/A31），`created_at_ms` 是设备钟、留给现有读者。
    pub ingested_at_ms: i64,
    /// 轮级说话人（D 子 agent 接线）：`mention` 里 `discourse_role='speaker'` +
    /// `resolution='resolved'` 那一条指向的锚 **canonical**。`None` = 这一轮没有这一格
    /// （绝大多数轮），渲染逐字回落到 role 推导（`pass_prompt::speaker_ref`）。
    pub speaker: Option<String>,
    /// 与 [`Self::speaker`] 同一行的**锚 id**。渲染按 id 找 `a*`（同一 canonical 在不同
    /// scope 可以是两格，按名字找会撞），候选补位按 id 去重。
    pub speaker_anchor_id: Option<String>,
}

/// 一条轮级说话人锚（D 子 agent 接线）：`mention` 里 `discourse_role='speaker'` 且
/// `resolution='resolved'` 的那一条指向的锚。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSpeaker {
    pub turn_uid: String,
    pub anchor_id: String,
    pub kind: String,
    pub canonical: String,
}

/// 给一批目标轮填 [`TurnRow::speaker`]/[`TurnRow::speaker_anchor_id`]，并按 `rows` 顺序
/// 返回**按锚去重**的说话人表（同一子 agent 说两轮只占候选一格）。
///
/// 唯一来源是 `mention`——库里唯一写着"这一轮是谁说的"的轮级形状，写入口是
/// `writers_v4::record_turn_speakers`（子 agent 报告：`speaker="subagent:…"`）。同一轮多条
/// 时取最早写入的那一条（`ORDER BY created_at_ms, id`），与
/// `writers_v4::turn_speaker_anchor_id` 同一条序；锚已退场（`status<>'active'`）不算——
/// 候选池也只认活锚，渲染照 role 推导走。
///
/// 读不到（绝大多数轮）= `None`：这一格不参与渲染，输出与改动前逐字一致。
pub fn load_turn_speakers(
    m: &MemoryHandle,
    rows: &mut [TurnRow],
) -> Result<Vec<TurnSpeaker>, StoreError> {
    let mut out: Vec<TurnSpeaker> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut st = m.conn().prepare(
        "SELECT m.anchor_id, a.kind, a.canonical
           FROM mention AS m
           JOIN anchor AS a ON a.id = m.anchor_id
          WHERE m.turn_uid=?1 AND m.discourse_role='speaker'
            AND m.resolution='resolved' AND a.status='active'
          ORDER BY m.created_at_ms, m.id
          LIMIT 1",
    )?;
    for row in rows.iter_mut() {
        let hit = st
            .query_row(params![row.turn_uid], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .optional()?;
        let Some((anchor_id, kind, canonical)) = hit else {
            continue;
        };
        row.speaker = Some(canonical.clone());
        row.speaker_anchor_id = Some(anchor_id.clone());
        if seen.insert(anchor_id.clone()) {
            out.push(TurnSpeaker { turn_uid: row.turn_uid.clone(), anchor_id, kind, canonical });
        }
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct Batch {
    pub session_key: String,
    pub device_id: String,
    pub source_agent: String,
    pub project_hint: Option<String>,
    pub rows: Vec<TurnRow>,
}

/// 位置别名的**唯一**语法：`t` + 十进制下标（从 1 起，不认前导零）。
///
/// 三个读者共读这一处：渲染（[`turn_ref`]）、解析（[`Batch::uid_of_ref`]）、
/// 错误载荷的形状关（`contract::uid_echo`）。返回的是下标，不是 uid——
/// "第几行"这件事只在 [`Batch::uid_of_ref`] 里与 `rows` 对齐一次。
///
/// 不认前导零是为了让一个行只有一种拼法：`t01` 与 `t1` 都能映到第一行的话，
/// 模型换了拼法就换出第二套编号，而"编号只按 `rows` 顺序"是本层的纪律。
pub fn ref_index(r: &str) -> Option<usize> {
    let digits = r.strip_prefix('t')?;
    if digits.is_empty() || (digits.len() > 1 && digits.starts_with('0')) {
        return None;
    }
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: usize = digits.parse().ok()?;
    (n > 0).then_some(n)
}

/// 批内第 `index` 行（0 起）的位置别名，即 `t1..tN`。
///
/// 编号规则**只有这一份**：渲染与解析都从这里走，所以"模型回的 `t{n}` 是哪一轮"
/// 不存在第二个答案。
pub fn turn_ref(index: usize) -> String {
    format!("t{}", index + 1)
}

impl Batch {
    pub fn row(&self, uid: &str) -> Option<&TurnRow> {
        self.rows.iter().find(|r| r.turn_uid == uid)
    }
    pub fn has_uid(&self, uid: &str) -> bool {
        self.row(uid).is_some()
    }
    /// 位置别名 ⇒ 它标的那一轮的真实 uid。`None` = 语法不对或下标越过本批：
    /// 调用方按既有契约错误处置，**不折叠**成某个默认行（静默映错比报错贵得多）。
    pub fn uid_of_ref(&self, r: &str) -> Option<&str> {
        ref_index(r).and_then(|n| self.rows.get(n - 1)).map(|row| row.turn_uid.as_str())
    }
    /// 锚角色的唯一来源。`None` 不是"没有锚"，是"这行的 role 值本不该出现"（DDL 有 CHECK）。
    pub fn role_of(&self, uid: &str) -> Option<AnchorRole> {
        self.row(uid).and_then(|r| AnchorRole::parse(&r.role))
    }
    pub fn uids(&self) -> Vec<&str> {
        self.rows.iter().map(|r| r.turn_uid.as_str()).collect()
    }
    /// 归属键：project_hint 优先，没有就落会话键。**永远有值**，且值不来自模型。
    pub fn scope_key(&self) -> String {
        self.project_hint.clone().unwrap_or_else(|| self.session_key.clone())
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

// ── system 轮（2026-10-30 转换器配套）：只作上下文，永不作 TARGET ─────────────

/// 会话转换器把工具调用/工具结果落成独立一轮时用的 role。四条模型 pass（naming/fact/
/// event/state）的候选 SQL 都以 `role <> 'system'` 把它们挡在批外（见这四族的
/// `SELECT ... FROM turns`），lesson 则在 Rust 里过滤。这一格是读数 SQL 的参数名；
/// 四处选批 SQL 仍写字面量 `'system'`，改 role 词要五处一起改。
///
/// 它们仍会出现在 [`ExtractCtx`] 里：邻轮读取（[`load_extract_ctx`]）不按 role 过滤，
/// 所以目标轮前后各一条工具输出的正文照旧进 `CTX (not evidence)`——可看、不可引。
pub const SYSTEM_ROLE: &str = "system";

/// 一批目标轮**区间内**的 `role='system'` 轮数（读数 `system_turns_skipped`）。
///
/// 候选 SQL 已经把 system 轮整段排除，这一格是"被排除掉的那几条"的可见读数：
/// 只数夹在首个与末个目标轮之间（含两端）的那些——批首之前、批末之后的 system 轮
/// 不属于这批，不计数（它们是上一批/下一批的上下文）。区间判据与选批、邻轮共用
/// `(created_at_ms, turn_uid)` 这一条序，同毫秒靠 uid 破平。
///
/// 空批是合法输入（返回 0）：没有区间就没有"批内"。
pub fn count_system_turns_in_span(
    t: &TranscriptHandle,
    session_key: &str,
    device_id: &str,
    rows: &[TurnRow],
) -> Result<usize, StoreError> {
    let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
        return Ok(0);
    };
    let mut st = t.conn().prepare(
        "SELECT COUNT(*)
           FROM turns
          WHERE session_key=?1 AND device_id=?2 AND role=?3
            AND (created_at_ms > ?4 OR (created_at_ms = ?4 AND turn_uid >= ?5))
            AND (created_at_ms < ?6 OR (created_at_ms = ?6 AND turn_uid <= ?7))",
    )?;
    let n: i64 = st.query_row(
        params![
            session_key,
            device_id,
            SYSTEM_ROLE,
            first.created_at_ms,
            first.turn_uid,
            last.created_at_ms,
            last.turn_uid
        ],
        |r| r.get(0),
    )?;
    Ok(n as usize)
}

// ── 邻轮（C6b）：±1 上下文，只为让模型看见语式，不是证据 ─────────────────────

/// 邻轮在目标轮的哪一侧。`created_at_ms` 的先后，不是到达顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtxSide {
    Before,
    After,
}

impl CtxSide {
    /// 渲染用的词（两个字母的英文，和 TARGET 段的 role 同属"程序给的标签"）。
    pub fn label(self) -> &'static str {
        match self {
            CtxSide::Before => "before",
            CtxSide::After => "after",
        }
    }
}

/// 一条邻轮：它紧邻哪个目标轮（`target_turn_uid`）、在哪一侧、正文是什么。
///
/// 行来源与会话库同一条读取路径（[`load_extract_ctx`]），**不**经过选批：选批的
/// `reselectable`/压缩块隔离/连续性策略一个字都不改。
#[derive(Debug, Clone)]
pub struct CtxTurn {
    pub target_turn_uid: String,
    pub side: CtxSide,
    pub row: TurnRow,
}

/// 一批目标轮的 ±1 邻轮集合（C6b）。**空的不是错误**：它渲染成 `CTX: (none)`。
///
/// 三条纪律（派发档 `2026-10-28-ctx-render`）：
/// 1. 只进 prompt 的 CTX 段——不进 `evidence`、不进 `pass_state`、不计 `turns`；
/// 2. 批内的邻轮不重复收：它已经是 `TARGET [t*]`，再放进 CTX 会让同一段正文有两个身份；
/// 3. 同一条邻轮被两个目标轮共用时只收一次（按 `turn_uid` 去重，保留先出现的那一格）。
#[derive(Debug, Clone, Default)]
pub struct ExtractCtx {
    pub turns: Vec<CtxTurn>,
}

impl ExtractCtx {
    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }
    /// 这一格是否已经在 CTX 里（去重用）。
    pub fn has_uid(&self, uid: &str) -> bool {
        self.turns.iter().any(|turn| turn.row.turn_uid == uid)
    }
}

/// 取一批目标轮的 ±1 邻轮（同会话、按 `(created_at_ms, turn_uid)` 排序的前一轮与后一轮）。
///
/// **跨批边界也要取**：邻轮不来自 `batch.rows`，而是直接从 `turns` 表读——选批只看
/// "这一轮有没有对 pass 表态"，批的首轮前一轮与末轮后一轮往往没被选中，却正是模型判断
/// 语式与指代要用的那一句。每次只 `LIMIT 1`，不把会话整段读进来（读取有界）。
///
/// 取不到（会话首/末、或库里就是没有）就是没有：调用方渲染 `CTX: (none)`，不静默省略。
pub fn load_extract_ctx(
    t: &TranscriptHandle,
    session_key: &str,
    device_id: &str,
    rows: &[TurnRow],
) -> Result<ExtractCtx, StoreError> {
    let mut ctx = ExtractCtx::default();
    for row in rows {
        for side in [CtxSide::Before, CtxSide::After] {
            let Some(neighbor) = neighbor_of(t, session_key, device_id, row, side)? else {
                continue;
            };
            // 批内邻轮已经是 TARGET（第 2 条），同一 uid 也不收第二遍（第 3 条）。
            let in_batch = rows.iter().any(|selected| selected.turn_uid == neighbor.turn_uid);
            if in_batch || ctx.has_uid(&neighbor.turn_uid) {
                continue;
            }
            ctx.turns.push(CtxTurn {
                target_turn_uid: row.turn_uid.clone(),
                side,
                row: neighbor,
            });
        }
    }
    Ok(ctx)
}

/// 目标轮在 `side` 一侧紧邻的那一轮。`(created_at_ms, turn_uid)` 为序：同一毫秒的轮
/// 用 uid 破平（与选批的 `ORDER BY created_at_ms, turn_uid` 同一条序）。
fn neighbor_of(
    t: &TranscriptHandle,
    session_key: &str,
    device_id: &str,
    row: &TurnRow,
    side: CtxSide,
) -> Result<Option<TurnRow>, StoreError> {
    let sql = match side {
        CtxSide::Before => {
            "SELECT turn_uid, role, body, created_at_ms, ingested_at_ms
               FROM turns
              WHERE session_key=?1 AND device_id=?2
                AND (created_at_ms < ?3 OR (created_at_ms = ?3 AND turn_uid < ?4))
              ORDER BY created_at_ms DESC, turn_uid DESC
              LIMIT 1"
        }
        CtxSide::After => {
            "SELECT turn_uid, role, body, created_at_ms, ingested_at_ms
               FROM turns
              WHERE session_key=?1 AND device_id=?2
                AND (created_at_ms > ?3 OR (created_at_ms = ?3 AND turn_uid > ?4))
              ORDER BY created_at_ms ASC, turn_uid ASC
              LIMIT 1"
        }
    };
    let mut st = t.conn().prepare(sql)?;
    let found = st
        .query_row(
            params![session_key, device_id, row.created_at_ms, row.turn_uid],
            |r: &Row| {
                Ok(TurnRow {
                    turn_uid: r.get(0)?,
                    role: r.get(1)?,
                    body: r.get(2)?,
                    created_at_ms: r.get(3)?,
                    ingested_at_ms: r.get(4)?,
                    // 邻轮不是批：TARGET 段没有 speaker= 这一格，不必读 mention。
                    speaker: None,
                    speaker_anchor_id: None,
                })
            },
        )
        .optional()?;
    Ok(found)
}
