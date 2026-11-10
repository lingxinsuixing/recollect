//! 抽取作业。三步的形状不是风格：**事务外读批次 → 事务外调 LLM → 事务内只写结果 + 推水位**。
//! 原设计 §6 那句"写锁绝不跨 LLM 往返"是上一轮实测事故换来的，合回一个事务就是事故重演。
//! 事务的边界还有一个后果（§2.12）：记忆库先提交、水位库后提交，水位库那一步是**提交标记**——
//! 它失败 ⇒ 游标不动 ⇒ 下一轮重放同一批，而记忆行的 id 由内容算出且 `ON CONFLICT DO NOTHING`，
//! 所以重放一个字节都不重复。反过来（先推水位再写内容）会造出"蒸过了但没东西"的永久假空。

use crate::batch::{Batch, TurnRow};
use crate::contract::{normalize_kind, parse_output, ExtractOutput};
use crate::prompt::{extract_system, extract_user, is_compact_summary};
use crate::DistillError;
use rc_ledger::water::{self, Cursor, ExtractState};
use rc_llm::{LlmBackend, Prompt};
use rc_memory::owner::OwnerCtx;
use rc_memory::{BatchCtx, MemKey, MemTable, WriteItem};
use rc_memory::writers_x::put_batch_with_owner;
use rc_store::{LedgerHandle, MemoryHandle, TranscriptHandle};
use rusqlite::types::ValueRef;
use rusqlite::{params, Row};
use std::collections::HashSet;

/// P-22：一批的绝对上限。超出的部分留给下一轮（游标只推到本批最后一条）。
pub const MAX_TURNS_PER_BATCH: usize = 40;

/// 两张写入侧证据表，住 ledger.db、由本层懒建（§2.12 理由 2：memory.db 的表清单是逐名精确等式，
/// 派生表进那里就要动守卫 11 的豁免谓词，而那一句放宽就是一整族未来表漏过）。
/// 语义是**滚动覆盖**：`rc_extract_row` = 该会话最近一次成功批次的"哪条轮产出了哪一行"。
/// 它不是历史，也不是 P2 的读源（P2 读 `extract_state`，那张表在 ledger.db 的另一半）。
pub const EXTRACT_DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS rc_extract_row(
        session_key TEXT NOT NULL, device_id TEXT NOT NULL, turn_uid TEXT NOT NULL,
        table_kind TEXT NOT NULL, mem_id TEXT NOT NULL,
        PRIMARY KEY(turn_uid, table_kind, mem_id))",
    "CREATE TABLE IF NOT EXISTS rc_extract_worthless(
        session_key TEXT NOT NULL, device_id TEXT NOT NULL, turn_uid TEXT NOT NULL,
        PRIMARY KEY(session_key, device_id, turn_uid))",
];

pub struct BatchPlan {
    pub batch: Batch,
    /// 水位游标的键：`"{device_id}/{session_key}"`。
    /// 它和 `Batch::scope_key()`（记忆行的归属）**不是一件事**，两个词分开是为了读得出来。
    pub water_scope: String,
    pub cursor_after: Cursor,
    /// P 独占列 `source_agent` / `device_id` 的唯一来源（§2.13）。
    pub ctx: BatchCtx,
    /// 归属继承（S1b-C3）：owner/visibility/retention 从会话行读出来。
    /// 与 `ctx` 分开：前者是 P 独占列（设备/署名），后者是**资源归属声明**（属于谁、给谁看、留多久）。
    pub owner: OwnerCtx,
}

impl BatchPlan {
    pub fn rows_ref(&self) -> &[TurnRow] {
        &self.batch.rows
    }
}

fn session_cursor(l: &LedgerHandle, water_scope: &str) -> Result<Cursor, DistillError> {
    // "从没蒸过"与"游标在 0"是两件事，但**批次选取**这一层只需要一个下界：
    // 没有游标 ⇒ 从 (0, "") 起，`turn_uid > ''` 对任何真 uid 都成立。
    Ok(water::read(l, "distill", water_scope)?.unwrap_or(Cursor {
        at_ms: 0,
        turn_uid: String::new(),
    }))
}

/// `select_batch` 读到的会话行。归属四格（S1b-C3）用结构体而不是 8 元组：
/// 两个 `Option<String>` 的语义不同（`project_hint`=归属键提示、`owner_principal_id`=
/// 未登记自然人），元组位置一错就是静默串值，clippy 的 `type_complexity` 也在这里替读者报警。
/// G13②：`owner_principal_id` **不再在这一条 SQL 里读**——它改走
/// [`session_owner_principal`]（章优先、列回落，读侧唯一入口）。
struct SessionRow {
    session_key: String,
    device_id: String,
    source_agent: String,
    project_hint: Option<String>,
    owner_agent: String,
    visibility: String,
    retention: String,
}

/// G13②（设计 `2026-11-07-w37-readgate-design.md` §4）：归属继承的主人一格**不再裸读
/// `sessions.owner_principal_id` 列**。那一列是 append-only 旧锚，章后补的会话它保持
/// NULL ⇒ 章后补的会话蒸出的行永久缺章（存量 226 行的主因之一）。现在走读侧唯一入口
/// [`rc_ledger::transcript::resolve_principal`]：章（claim）优先、sessions 列回落；
/// 读到的 id 再过 [`rc_memory::writers_v4::principal_is_known`]（`unknown`/空白不是人，
/// 与盖章入口 `claim_declared_principal` 的守卫同一把尺）——两级 whichever 产出的值
/// 都要过它，两级皆未知才落 `None`（旧行为的最保守形状一格不变）。
fn session_owner_principal(
    t: &TranscriptHandle,
    session_key: &str,
    device_id: &str,
) -> Result<Option<String>, DistillError> {
    Ok(rc_ledger::transcript::resolve_principal(t, session_key, device_id)?
        .filter(|rp| rc_memory::writers_v4::principal_is_known(&rp.principal_id))
        .map(|rp| rp.principal_id))
}

pub fn select_batch(t: &TranscriptHandle, l: &LedgerHandle, limit: usize)
    -> Result<Option<BatchPlan>, DistillError> {
    // ① 会话清单（transcript.db）。顺序确定 ⇒ 同一库同一状态每次都选同一个会话。
    // source_agent 真在 sessions 表里（TEXT NOT NULL，`crates/rc-store/src/ddl.rs`），
    // 所以 BatchCtx 的两个字段全部来自这一行读出的值，一个都不造。
    // S1b-C3：多读 4 列归属。旧会话（C1 之前）这 4 列是 NULL ⇒ SQL 侧 COALESCE 出自动化默认：
    // owner_agent=source_agent、visibility=private_agent、retention=default（"未知 ⇒ 最保守"）。
    // owner_principal_id 没有 COALESCE 的默认值（NULL=未登记自然人，S1b 恒 NULL）。
    let mut sessions: Vec<SessionRow> = Vec::new();
    {
        let mut st = t.conn().prepare(
            "SELECT session_key, device_id, source_agent, project_hint,
                    COALESCE(owner_agent_id, source_agent) AS owner_agent,
                    COALESCE(visibility, 'private_agent') AS visibility,
                    COALESCE(retention, 'default') AS retention
             FROM sessions
             ORDER BY session_key, device_id")?;
        let rows = st.query_map([], |r: &Row| {
            // project_hint 是 NULL 或 TEXT：用 ValueRef 判 NULL，`get::<_,Option<String>>` 会
            // 把空串与 NULL 混成一类，而"空串 project_hint"在契约侧是拒绝项（阶段 0 的档位对拍）。
            let hint: Option<String> = match r.get_ref(3)? {
                ValueRef::Null => None,
                ValueRef::Text(v) => Some(String::from_utf8_lossy(v).to_string()),
                _ => None,
            };
            Ok(SessionRow {
                session_key: r.get(0)?,
                device_id: r.get(1)?,
                source_agent: r.get(2)?,
                project_hint: hint,
                owner_agent: r.get(4)?,
                visibility: r.get(5)?,
                retention: r.get(6)?,
            })
        })?;
        for r in rows { sessions.push(r?); }
    }
    // ② 已表态的 uid 集合（ledger.db）——**一次**读，不许在会话循环里读（那是 N 次全表扫）。
    // 这里的判据是**状态行在场**，不是"是否已蒸过"：`Extracted` / `Worthless` / `Rejected` /
    // `Pending` 四态的行都进集合，所以 `set_extract_state(…, Pending, …)` 单独永远不能让一轮
    // 重蒸（执法 = `zero_output.rs::a_pending_state_row_is_spoken_for_even_with_the_cursor_at_zero`：
    // 游标停在 0、三条里中间那条带 `pending` 状态行 ⇒ 它不许进批。同文件那条老用例读到的 `None`
    // 是游标推过遮住的结果，证不了这一格过滤器的形状，所以那一条才是**独立**的牙。与 P-23 的字面一致）。
    // 若将来要让 `Pending`
    // 重新可选，判据得改成读状态列（形如 `state != Pending`）——那是任务 9 / 阶段 4 的裁定，
    // 不是这一轮过滤里能顺手放宽的事。
    let mut stated: HashSet<String> = {
        let mut st = l.conn().prepare("SELECT turn_uid FROM extract_state")?;
        let got = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<_, _>>()?;
        got
    };
    // B3a：节点离线蒸过、中心按行吸收的轮也进同一个差集。这些轮在 `extract_state` 里
    // 可能**没有**任何状态行（中心没蒸过它们），所以必须单独并进来；一次读、不放进会话循环。
    stated.extend(rc_ledger::water::pre_distilled_uids(l)?);
    for s in sessions {
        let SessionRow { session_key, device_id, source_agent, project_hint,
                         owner_agent, visibility, retention } = s;
        // P-63：键形的唯一家在水位层的拼键函数里（`mcp` 的读缝是它的第二个生产者），这里只调不拼。
        let water_scope = rc_ledger::water::distill_scope(&device_id, &session_key);
        if water::halted(l, "distill", &water_scope)? {
            continue;   // 停轮的会话跳过（任务 1 的 P-03：停轮不是回拨，也不是硬闯）
        }
        let cur = session_cursor(l, &water_scope)?;
        let mut rows: Vec<TurnRow> = Vec::new();
        {
            // **库内** JOIN。这里的 JOIN 其实**不需要**（`WHERE turns.session_key=?1 AND turns.device_id=?2`
            // 已经锁死会话主键，不放大行数），留它是把"库内 JOIN 合法、跨库 JOIN 才破守卫 1"写进代码形状。
            // 翻页轴是**到达钟**（`ingested_at_ms`），不是设备钟（`created_at_ms`）：迟到行的
            // created_at 可能早于游标、但 ingested_at 一定晚于它——用设备钟做键会把这条永久漏掉（A24/A31）。
            let mut st = t.conn().prepare(
                "SELECT turns.turn_uid, turns.role, turns.body, turns.created_at_ms,
                        turns.ingested_at_ms
                 FROM turns JOIN sessions
                   ON turns.session_key = sessions.session_key
                  AND turns.device_id  = sessions.device_id
                 WHERE turns.session_key = ?1 AND turns.device_id = ?2
                   AND (turns.ingested_at_ms > ?3
                        OR (turns.ingested_at_ms = ?3 AND turns.turn_uid > ?4))
                 ORDER BY turns.ingested_at_ms, turns.turn_uid
                 LIMIT ?5")?;
            // **keyset 翻页**，一页正好 `limit` 行：读满一页 ⇒ 换下界再读，凑够 limit 或读到尾就停。
            // 为什么不是"一次过量读再截断"：已表态的行只会在**游标之后**堆积（成功批次推游标，
            // 丢弃批次不推），堆积量 = 连续被丢弃的批数 × `limit`。固定窗口（旧形状 `LIMIT limit*4`）
            // 被四批丢弃铺满之后就**再也读不到第 161 条** ⇒ 会话永久停摆，比 P-32 登记的那条
            // "重烧配额"的代价更坏（见 P-74）。翻页的代价是债务每攒一批多一次 SELECT，且它有尽头。
            let (mut bound_ms, mut bound_uid) = (cur.at_ms, cur.turn_uid.clone());
            loop {
                let got = st.query_map(params![session_key, device_id, bound_ms, bound_uid,
                                               limit as i64], |r: &Row| {
                    Ok(TurnRow { turn_uid: r.get(0)?, role: r.get(1)?,
                                 body: r.get(2)?, created_at_ms: r.get(3)?,
                                 ingested_at_ms: r.get(4)?,
                                 speaker: None, speaker_anchor_id: None })
                })?;
                let mut page = 0usize;
                for r in got {
                    let row = r?;
                    page += 1;
                    bound_ms = row.ingested_at_ms;         // 下界跟着**读到的**最后一行走，
                    bound_uid = row.turn_uid.clone();      // 不是跟着**留下的**最后一行
                    if stated.contains(&row.turn_uid) { continue; }   // P-23 的差集在这里
                    rows.push(row);
                    if rows.len() == limit { break; }
                }
                if page < limit || rows.len() == limit { break; }   // 读到尾，或批已凑满
            }
        }
        if rows.is_empty() { continue; }
        // G13②：主人一格改经 `resolve_principal`（章优先、列回落，两级过 `principal_is_known`）。
        // 只对真有轮可蒸的会话读（空会话不必花这两条查询）。
        let owner_principal_id = session_owner_principal(t, &session_key, &device_id)?;
        // 游标/翻页仍按到达钟（`ingested_at_ms`）：迟到行不能因为源时间早而被永久跳过（A24/A31）。
        // 但 prompt 里的行必须按源会话顺序（`created_at_ms`，同刻 `turn_uid`）：批内上下文
        // 和“先说什么、后说什么”有关；同一次导入的 ingested_at 往往全同，直接按它排会退化成 uid 哈希序。
        let last_ingested = rows[rows.len() - 1].clone();
        rows.sort_by(|a, b| a.created_at_ms.cmp(&b.created_at_ms)
                                .then_with(|| a.turn_uid.cmp(&b.turn_uid)));
        let cursor_after = Cursor { at_ms: last_ingested.ingested_at_ms,
                                    turn_uid: last_ingested.turn_uid };
        return Ok(Some(BatchPlan {
            batch: Batch { session_key, device_id: device_id.clone(),
                           source_agent: source_agent.clone(), project_hint, rows },
            water_scope,
            cursor_after,
            ctx: BatchCtx { source_agent, device_id },
            // 归属：会话行的四格原文（visibility/retention 已在 SQL 侧 COALESCE 默认，
            // 这里只做字面量 → 枚举/归一，两个函数都是 `rc_memory::owner` 的同一个实现点）。
            owner: OwnerCtx {
                owner_principal_id,
                owner_agent_id: owner_agent,
                visibility: rc_memory::owner::parse_visibility(&visibility),
                retention: rc_memory::owner::normalize_retention(&retention),
            },
        }));
    }
    Ok(None)
}

#[derive(Debug)]
pub struct ExtractOutcome {
    /// **批内产出行数**，不是"新增行数"：`writers_x` 的写语句命中 `ON CONFLICT(id) DO NOTHING`
    /// 时（同批两条内容相同，或重放同一批），那一格仍是 `Some(key)`，账本记写成 ⇒ 这里照样计数。
    /// P5 的读者要能区分这两个意思：这一格读的是"这一批负责了行"，不是"库里多了行"。
    pub rows_written: usize,
    pub worthless: usize,
    /// 命中抑制账、没有写成的行数（守卫 13 的读数；`0` 是常态）。
    /// 它存在的全部理由见本任务 Interfaces 那一条：全被抑制是一次**成功**，不是 ZeroOutput。
    pub suppressed: usize,
    /// A45：模型这一批给了几条实体（**分母**，在 kind 闸门之前计数）。
    pub entities_seen: usize,
    /// A39：kind 归一后不在六词内、被单条丢弃的实体数。
    pub kinds_rejected: usize,
    /// A41：归一后不在 provenance 原文里的别名，被丢掉的数量（实体行照落）。
    pub aliases_dropped: usize,
}

/// 抽取契约的实体 kind 词表（`rc_contract::ENTITY_KINDS`：person/project/org/tool/place/concept）
/// 与 canonical `anchor.kind` 的 CHECK 词表（person/agent/org/place/object/animal/work/event/concept）
/// 今天**不是同一张表**：`project` / `tool` 两个词过不了 CHECK，落库就是整批回滚。
///
/// 这是 **M1 的兼容映射**（存量抽取契约不改）：M3 会由新 prompt 直接产出 canonical kind
/// （`project` 归 scope/world，`tool` 归 `object`/`agent`），届时本函数整段删除。
/// 映射必须是确定性的、且只发生在这一个点上——`ENTITY_KINDS` 本身不动，契约层仍按六词收草稿。
pub fn canonical_anchor_kind(kind: &str) -> &str {
    match kind {
        "project" => "org",     // 项目 = 组织/团队/课题组这一侧；world 语义等 M3 的 scope 承载
        "tool" => "object",     // 工具 = 物件（软件/设备/服务）；agent 语义等 M3 的 prompt 区分
        other => other,         // person/org/place/concept 逐字原样
    }
}

pub(crate) struct GateOutcome {
    pub items: Vec<WriteItem>,
    pub entities_seen: usize,
    pub kinds_rejected: usize,
    pub aliases_dropped: usize,
}

/// provenance 指向的 turn 原文，用与别名同一把 `normalize_body` 尺子归一后拼起来。
fn source_text(batch: &Batch, prov: &[rc_memory::Provenance]) -> String {
    prov.iter()
        .filter_map(|p| batch.rows.iter().find(|r| r.turn_uid == p.turn_uid))
        .map(|r| rc_contract::normalize_body(&r.body))
        .collect::<Vec<_>>()
        .join("\n")
}

/// I3：一条产出的"源时刻"= 它引用的源轮里**最晚**的那条 `created_at_ms`。
/// 选最晚而不是最早：as_of 的语义是"这条何时成立"，多轮证据下最后一个源轮才是它成立的时刻
///（审计里的批准断言引了提议轮 + 批准轮，取批准轮才对）。批内找不到行 ⇒ None，调用方回落写入钟。
fn source_ms_for(batch: &Batch, prov: &[rc_memory::Provenance]) -> Option<i64> {
    prov.iter()
        .filter_map(|p| batch.row(&p.turn_uid).map(|r| r.created_at_ms))
        .max()
}

pub fn run_extract(
    m: &MemoryHandle,
    l: &LedgerHandle,
    plan: &BatchPlan,
    backend: &dyn LlmBackend,
    now_ms: i64,
) -> Result<ExtractOutcome, DistillError> {
    if plan.rows_ref().is_empty() {
        return Err(DistillError::EmptyBatch);       // P-19：在调用 backend **之前**返回（用例断 calls()==0）
    }
    // ── 1. LLM 往返：事务外（上一条用例断的就是这一刻 `is_autocommit() == true`）──
    let raw = backend
        .complete(&Prompt { system: extract_system(), user: extract_user(&plan.batch),
                            derive: false })
        .map_err(DistillError::Llm)?;                 // 故障 ⇒ 什么都不写、状态也不落（第四态"恢复即续"）

    // ── 2. 契约：整批的闸门。这里只调用一次 parse_output，缺键 / 零产出 / 畸形都在它里面 ──
    let out: ExtractOutput = match parse_output(&raw, &plan.batch).map_err(DistillError::Contract) {
        Ok(out) => out,
        Err(e) => { mark_rejected(l, plan, &e, now_ms); return Err(e); }
    };
    let gate = match to_write_items(&out, &plan.batch) {
        Ok(gate) => gate,
        Err(e) => { mark_rejected(l, plan, &e, now_ms); return Err(e); }
    };
    let GateOutcome { items, entities_seen, kinds_rejected, aliases_dropped } = gate;
    if items.is_empty() && out.worthless.is_empty() {
        // 规则 7 的第二道闸门（正常情况下 parse_output 已拦）
        let e = DistillError::ZeroOutput;
        mark_rejected(l, plan, &e, now_ms);
        return Err(e);
    }

    // ── 3. 记忆库：一个事务写完整批（§2.13 的 put_batch；失败 ⇒ 库里没有半批）──
    // `slots` 与 `items` 逐格对齐：Some = 写成，None = 命中抑制账没写（守卫 13 的牙）。
    // I3：每一格的 as_of/created_at 取该行 provenance 对应的源轮时刻（最晚的那条），
    // 不再用导入钟 `now_ms`。`item_times` 与 `items` 逐格对齐是 `put_batch_with_owner` 的契约。
    // S1b-C3：`plan.owner` 是会话的归属声明，逐行继承进 `resource_owner`（与行同一个事务）。
    let item_times: Vec<Option<i64>> = items.iter()
        .map(|it| source_ms_for(&plan.batch, &it.prov))
        .collect();
    let slots: Vec<Option<MemKey>> =
        put_batch_with_owner(m, &items, &plan.ctx, &plan.owner, now_ms, &item_times)
            .map_err(DistillError::Memory)?;
    let rows_written = slots.iter().filter(|s| s.is_some()).count();
    let suppressed = slots.len() - rows_written;

    // ── 3.4. I7：抽取态的计数口径 ──
    // `extracted` 只发给产出 card/event/assertion 的轮；只产 entity 的轮另计
    // `entity_only_turns`，状态落 `pending` + reason='entity_only'（状态词表是库级 CHECK，
    // 本任务不许动 DDL ⇒ 用既有四态里语义最接近的一格 + 独立理由 + 独立计数器）。
    let fact_uids: HashSet<&str> = items.iter()
        .filter(|it| it.table == MemTable::Card)
        .flat_map(|it| it.prov.iter().map(|p| p.turn_uid.as_str()))
        .collect();
    let entity_uids: HashSet<&str> = items.iter()
        .filter(|it| it.table == MemTable::Anchor)
        .flat_map(|it| it.prov.iter().map(|p| p.turn_uid.as_str()))
        .collect();
    // ── 3.45. 压缩摘要（`[[compact_summary]]`）单列一类输入 ──
    // 带标记的轮**禁止直接判 worthless**：模型若把它列进 worthless，那一条被拒收
    // （状态 `rejected` + reason='compact_summary'），并进 `compact_summary_worthless` 计数。
    // 它不写进 `rc_extract_worthless` 证据表——那张表是"这一轮确实什么都不值钱"的账。
    let compact_uids: HashSet<&str> = plan.rows_ref().iter()
        .filter(|r| is_compact_summary(&r.body))
        .map(|r| r.turn_uid.as_str())
        .collect();
    let mut worthless: Vec<String> = Vec::new();
    let mut compact_worthless: Vec<String> = Vec::new();
    for uid in &out.worthless {
        if compact_uids.contains(uid.as_str()) { compact_worthless.push(uid.clone()); }
        else { worthless.push(uid.clone()); }
    }
    // 只产 entity 的轮 = 有 entity 出处、没有 fact 出处、也没被判 worthless（含压缩摘要那支）。
    let worthless_any: HashSet<&str> = worthless.iter().chain(compact_worthless.iter())
        .map(String::as_str).collect();
    let entity_only: Vec<&str> = entity_uids.iter().copied()
        .filter(|u| !fact_uids.contains(u) && !worthless_any.contains(u))
        .collect();

    // ── 3.5. 覆盖缺口：模型对某条 turn 一个字都没答（既没引用、也没列进 worthless）──
    // 这一格不能留空：`select_batch` 的判据是"状态行在场"，而游标会越过这一批；没有状态行
    // 的 turn 从此永远不会再被选中（P-23 的静默丢失面）。处置是**单轮 Rejected**：
    // 已抽出的行照写、游标照推，缺口在 `extract_state.reason='schema'` 与
    // `turns_uncovered` 计数器上同时可见。不整批拒——那会把同批已抽出的好行一起丢掉，
    // 并可能把会话拖进活锁。
    let covered: HashSet<&str> = items.iter()
        .flat_map(|item| item.prov.iter().map(|p| p.turn_uid.as_str()))
        .chain(out.worthless.iter().map(String::as_str))
        .collect();
    let uncovered: Vec<&str> = plan.rows_ref().iter()
        .map(|r| r.turn_uid.as_str())
        .filter(|uid| !covered.contains(uid))
        .collect();

    // ── 4. 水位库：提交标记。这一步里做四件事：证据表、worthless 表、每轮状态、推游标 ──
    let tx = l.conn().unchecked_transaction()?;
    for ddl in EXTRACT_DDL { tx.execute(ddl, [])?; }
    tx.execute("DELETE FROM rc_extract_row WHERE session_key=?1 AND device_id=?2",
               params![plan.batch.session_key, plan.batch.device_id])?;
    tx.execute("DELETE FROM rc_extract_worthless WHERE session_key=?1 AND device_id=?2",
               params![plan.batch.session_key, plan.batch.device_id])?;
    for (item, slot) in items.iter().zip(slots.iter()) {
        // 抑制的那格没有 mem_id 可记（它根本没写成），所以证据表只写 Some 的那些。
        // `zip` 在这里安全的原因就是 §2.13 那条"逐格对齐的账本"：换成返回短 Vec，
        // 这一行会静默错位，然后把 A 条的出处记到 B 条上——那比不记更坏。
        let Some(key) = slot else { continue };
        for pv in &item.prov {
            tx.execute("INSERT OR IGNORE INTO rc_extract_row(session_key, device_id, turn_uid,
                        table_kind, mem_id) VALUES(?1,?2,?3,?4,?5)",
                params![plan.batch.session_key, plan.batch.device_id, pv.turn_uid,
                        key.table.prefix(), key.id])?;
        }
    }
    for uid in &worthless {
        tx.execute("INSERT OR IGNORE INTO rc_extract_worthless(session_key, device_id, turn_uid)
                    VALUES(?1,?2,?3)",
            params![plan.batch.session_key, plan.batch.device_id, uid])?;
    }
    tx.commit()?;  // ← 状态与游标必须是**另一个**事务：`water::advance` 自己会开事务（规则 3）
    // I7：**只有产出 card/event/assertion 的轮**才拿 `extracted`。只产 entity 的轮此前
    // 也落 `extracted`，13/6 的读数因此高估了有效抽取（审计 5 轮都是这一型）。
    for uid in &fact_uids {
        water::set_extract_state(l, uid, ExtractState::Extracted, None, now_ms)?;
    }
    for uid in &worthless {
        water::set_extract_state(l, uid, ExtractState::Worthless, None, now_ms)?;
    }
    // 带 `[[compact_summary]]` 的轮被判 worthless ⇒ 拒收，并在计数上单列（I1 的可见出口）。
    for uid in &compact_worthless {
        water::set_extract_state(l, uid, ExtractState::Rejected, Some("compact_summary"), now_ms)?;
    }
    // 只产 entity 的轮：不拿 extracted，状态 `pending` + reason='entity_only'，计数单列。
    for uid in &entity_only {
        water::set_extract_state(l, uid, ExtractState::Pending, Some("entity_only"), now_ms)?;
    }
    // 覆盖缺口逐条落 Rejected：reason 复用契约族唯一的封闭短语 `schema`（`DistillError::
    // reason_slug` 的同一把尺子），不新增第五个状态词。
    for uid in &uncovered {
        water::set_extract_state(l, uid, ExtractState::Rejected, Some("schema"), now_ms)?;
    }
    if !uncovered.is_empty() {
        // 让"模型漏了一条 turn"在运维面有一个可数的出口；状态行本身只说 rejected，
        // 分不出"这条答复不合格"与"模型压根没提这条"。
        rc_ledger::counters::bump(l, "llm_job", "extract", "turns_uncovered",
                                  uncovered.len() as i64, now_ms)?;
    }
    // I7 的独立计数器（一臂一库口径：读的是本库 ledger.db 的 usage_counter）。
    if !entity_only.is_empty() {
        rc_ledger::counters::bump(l, "llm_job", "extract", "entity_only_turns",
                                  entity_only.len() as i64, now_ms)?;
    }
    if !compact_uids.is_empty() {
        rc_ledger::counters::bump(l, "llm_job", "extract", "compact_summary_turns",
                                  compact_uids.len() as i64, now_ms)?;
    }
    if !compact_worthless.is_empty() {
        rc_ledger::counters::bump(l, "llm_job", "extract", "compact_summary_worthless",
                                  compact_worthless.len() as i64, now_ms)?;
    }
    water::advance(l, "distill", &plan.water_scope, &plan.cursor_after, now_ms)?;
    rc_ledger::counters::bump(l, "llm_job", "extract", "entities_seen",
                              entities_seen as i64, now_ms)?;
    rc_ledger::counters::bump(l, "llm_job", "extract", "kinds_rejected",
                              kinds_rejected as i64, now_ms)?;
    rc_ledger::counters::bump(l, "llm_job", "extract", "aliases_dropped",
                              aliases_dropped as i64, now_ms)?;
    Ok(ExtractOutcome { rows_written, worthless: worthless.len(), suppressed,
                        entities_seen, kinds_rejected, aliases_dropped })
}

/// `ExtractOutput` → `WriteItem`：内容列从模型来，**归属与锚角色从批次行来**（守卫 11）。
fn to_write_items(out: &ExtractOutput, batch: &Batch) -> Result<GateOutcome, DistillError> {
    let mut v = Vec::new();
    let mut aliases_dropped = 0;
    for e in &out.entities {
        // 坏 kind 已在契约层（`parse_output` 的 `filter_entity_kinds`）单条丢弃，这一层只面对
        // kind 合法的草稿；留下的越界锚仍然是契约错误（整批），与 kind 的单条丢弃不是一层。
        let prov = e.to_provenance(batch).map_err(DistillError::Contract)?;
        let kind = normalize_kind(&e.kind);
        debug_assert!(rc_contract::is_valid_entity_kind(&kind));
        // 归一后的六词 → canonical `anchor.kind`：`anchor` 的 CHECK 才是最后一关，
        // `project`/`tool` 原样下去会把整批拖回滚（见 `canonical_anchor_kind` 的 M1 注记）。
        let anchor_kind = canonical_anchor_kind(&kind);
        let source = source_text(batch, &prov);
        let mut kept = Vec::new();
        for a in &e.aliases {
            let na = rc_contract::normalize_body(a);
            if rc_contract::is_alias_in_source(&na, &source) {
                kept.push(a.clone());
            } else {
                aliases_dropped += 1;
            }
        }
        // `scope_for` 必须看到**归一后**的 kind（`Project` → `project`）；先在字面量之外算出来，
        // 因为结构体字面量按书写顺序求值，`kind` 那一格会先把 String 移走。
        let scope = crate::contract::scope_for(MemTable::Anchor, anchor_kind, &e.title, batch);
        v.push(WriteItem { table: MemTable::Anchor, kind: anchor_kind.to_string(),
            title: e.title.clone(),
            // anchor 表没有 body 列，`write_item_in` 的 Anchor 分支不读这一格；`compute_mem_id`
            // 收到的也是同一个 ""，所以实体行的 id 与 `put_entity` 算出来的逐字节一致。
            body: String::new(), scope, valid_to_ms: None, aliases: kept, prov });
    }
    // card/event/assertion 三路候选各自保留内容形状与 kind，物理上同落 canonical `card` 表：
    // 表的归属合并，业务类别不合并（三路仍逐路生成，计数也仍逐类统计）。
    for (items, table) in [(&out.cards, MemTable::Card), (&out.events, MemTable::Card),
                           (&out.assertions, MemTable::Card)] {
        for f in items {
            let prov = f.to_provenance(batch).map_err(DistillError::Contract)?;
            v.push(WriteItem { table, kind: f.kind.clone(), title: f.title.clone(),
                body: f.body.clone(), scope: crate::contract::scope_for(table, &f.kind, &f.title, batch),
                valid_to_ms: f.valid_to_ms, aliases: vec![], prov });
        }
    }
    // 三个计数都从契约层带过来：`entities_seen` 是 kind 闸门之前的分母，
    // `kinds_rejected` 的计数点在 `filter_entity_kinds`（那里才看得见被丢的那几条）。
    Ok(GateOutcome { items: v, entities_seen: out.entities_seen,
                     kinds_rejected: out.kinds_rejected, aliases_dropped })
}

/// 契约族每一条 `Err` 返回前的落状态（规则 1）。reason 从 `e.reason_slug()` 取：
/// 它是契约族四词的**唯一**映射点（规则 7），调用点不抄字符串。
/// `Llm` 故障族**不许**调它：那一族要的是"恢复即续"，标成 rejected 就等于永久放弃。
/// `let _ =` 是唯一允许的忽略：**已经**要返回 Err 时，状态写失败的第二种失败没有可表达的形状。
fn mark_rejected(l: &LedgerHandle, plan: &BatchPlan, e: &DistillError, now_ms: i64) {
    for r in plan.rows_ref() {
        let _ = water::set_extract_state(l, &r.turn_uid, ExtractState::Rejected, e.reason_slug(), now_ms);
    }
}

pub fn state_of(l: &LedgerHandle, uid: &str) -> Result<Option<ExtractState>, DistillError> {
    Ok(water::get_extract_state(l, uid)?)
}
