//! 消费面的三个确定性视图：`overview / agenda / timeline`。
//!
//! 它们回答的是"结构问题"，不是"相似度问题"：
//! - `overview`：这个范围里还有哪些没做完、最近定过什么、谁还活跃、哪些审核没解决；
//! - `agenda`：带截止日的承诺/计划，按到期时间排，逾期单列；
//! - `timeline`：某时间窗内发生了什么（事件/事实），按时间倒序。
//!
//! 三条硬纪律：
//! 1. **不调模型**。全部是 SQL + 内存过滤；模型不参与"哪几行算数"。
//! 2. **只读活行**。退场、被顶掉、被否决的行不出现在视图里（口径由 `reader::fact_rows` 承担）。
//! 3. **过读闸**。视图与 `recall` 用同一个 `GateIndex`：被拒的行与"查不到"同形。
//!
//! 输出形状：一个 JSON 对象，`brief` 是给人/agent 读的自然语言摘要，其余字段给程序。
//! 这是消费契约"自然语言 + JSON sidecar"的落地；MCP 的 `read_view` 会在节点面再补
//! `source/as_of/lag`，本层不重复。

use rc_memory::gate::{GateIndex, Viewer};
use rc_memory::reader::{self, FactRow};
use rc_memory::MemError;
use rc_store::MemoryHandle;
use serde_json::{json, Value};
use std::collections::HashMap;

pub const MAX_LIMIT: usize = 50;
const SCAN: usize = 500;
const RECENT_MS: i64 = 7 * 24 * 3600 * 1000;

/// 三个视图的名字（MCP 侧闭集校验的唯一来源）。
pub const VIEWS: [&str; 3] = ["overview", "agenda", "timeline"];

/// 一次视图请求。参数收成一个结构体：`render` 的参数因此压到 4 个
///（clippy 的 7 是给"能不能一眼读清调用点"划的线，不为了少写一行去 `allow`）。
#[derive(Debug, Clone, Copy)]
pub struct ViewRequest<'a> {
    pub view: &'a str,
    pub scope: Option<&'a str>,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub limit: usize,
}

/// 入口：`view` 必须已由调用方校验过闭集；这里再拒一次而不是 `unreachable!`，
/// 因为闭集校验和渲染分派一旦漂开，静默走到别的分支就是假话。
pub fn render(
    m: &MemoryHandle,
    req: &ViewRequest<'_>,
    viewer: &Viewer,
    now_ms: i64,
) -> Result<String, MemError> {
    let limit = req.limit.clamp(1, MAX_LIMIT);
    let value = match req.view {
        "overview" => overview(m, req.scope, viewer, now_ms)?,
        "agenda" => agenda(m, req.scope, viewer, now_ms, limit)?,
        "timeline" => timeline(m, req.scope, req.from_ms, req.to_ms, viewer, now_ms, limit)?,
        _ => {
            return Err(MemError::Sql(format!("unknown view {}", req.view)));
        }
    };
    Ok(value.to_string())
}

/// 状态行在**过读闸之前**的库侧扫描帽：先截后滤会把前排的私有行整片丢掉，那时库里明明有、
/// 这边一条不给看。与 `SCAN` 同一类帽（取够再过滤）。
const STATE_SCAN: usize = 20;

/// 第二格是**读闸拒掉的行**（不是布尔）：SQL 的 scope/时间过滤之后有行、但过读闸时被拒。
/// 调用方自己决定"这些被拒的行算不算我这页的内容"——agenda 只认承诺，timeline 认所有
/// 事实行，overview 认所有事实行（八个计数与清单都从这一格里出）。把它做成布尔会在
/// agenda 上过报（一条被藏的决策会让"当前身份看不到任何事项"说出来），这是 D32-c
/// 验收时补的一刀。
///
/// 这个向量只在扫描帽内（`SCAN` 先截后滤），不是全库被藏条数；对外仍只给布尔。
fn visible(
    m: &MemoryHandle,
    scope: Option<&str>,
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    viewer: &Viewer,
    now_ms: i64,
) -> Result<(Vec<FactRow>, Vec<FactRow>, usize, GateIndex), MemError> {
    let rows = reader::fact_rows(m, scope, from_ms, to_ms, SCAN)?;
    // 这里用 `GateIndex::allows`（只知 `table:id`）而不是 `decide`（要 `RowFacts`）：
    // 它比 `decide` **窄**——没有归属行的旧行一律拒（`decide` 还放行"行署名恰等于 viewer"
    // 的同 agent 行）。生产 fact 写入会落归属行；旧行需要 `hub ownership backfill` 才可见。
    let gate = GateIndex::load(m, viewer, now_ms)?;
    let mut gated = Vec::new();
    let mut hidden = Vec::new();
    for row in rows {
        if gate.allows("fact", &row.id) {
            gated.push(row);
        } else {
            hidden.push(row);
        }
    }
    // G1 刀 A 的三视图消费：归档行**缺省不入视图**（与 `rank_counted` 同一语义与同一顺序——
    // 身份闸在前、归档在后；被身份拒的行不算归档，归档计数也不泄露身份沉默）。
    // 显式寻址（handles / view=card|expand）不过这一层：原文全留、指认即给。
    let archived_facts = rc_memory::value_gate::archived_fact_ids(m)?;
    let before = gated.len();
    gated.retain(|row| !archived_facts.contains(&row.id));
    let archived = before - gated.len();
    // 闸随取数一起交回去（D8 方案二：`name_of` 对"解析到幸存锚"那支要过同一把闸，
    // 复用这次装载，不给视图面开第二次全表扫描）。
    Ok((gated, hidden, archived, gate))
}

fn is_commitment(c: &str) -> bool {
    c == "promise" || c == "plan"
}

/// `done/cancelled/expired` 之外都算"还没完"；`status` 今天没有生产者（NULL），
/// 所以 NULL 按未完成处理——这与消费面"还没做完的承诺"语义一致。
fn is_open(status: Option<&str>) -> bool {
    !matches!(status, Some("done") | Some("cancelled") | Some("expired"))
}

/// 视图面的锚名渲染（fact subject / state subject）。D8 方案二：
/// - 解析：旧锚 id 经 `redirect::resolve` 归到链终点（Alive/Merged→终点；Broken→旧锚
///   自己的名字面——可见不静默；Missing→`None`）；
/// - **指向不绕闸**：解析**发生**时（Merged），幸存锚先过同一把读闸
///   （`GateIndex::allows("anchor", to)`），被拒 ⇒ 与"锚不存在"同形（回落既有的
///   id 前缀兜底），无权读者不会经指向行看到幸存锚的名字。Alive（零跳解析）不新增
///   闸判定——那是既有形状，本单不收紧；
/// - **只有 person 过闸后显示别名**（W20c，用户 2026-11-06 拍板 B）：名字面取
///   [`reader::display_name`]（它自己解 redirect、优先取活别名、取不到才回落正名）；
///   work/agent/org/place 等仍用正名——`display_name` 取的是"最新有效别名"，那是别名史
///   而不是被挑过的显示名，拿它替换正名会让非人锚的标签随别名史漂。旧主人锚的 kind
///   也是 person，自动覆盖。
///   解析或名字面读失败（含 Missing）⇒ 回落到既有的正名路径，再取不到才用 id 前缀。
fn name_of(
    m: &MemoryHandle, id: &str, cache: &mut HashMap<String, String>,
    gate: Option<&GateIndex>,
) -> String {
    if let Some(v) = cache.get(id) {
        return v.clone();
    }
    use rc_memory::redirect::RedirectResolve;
    let resolved = rc_memory::redirect::resolve(m, id).ok();
    let allowed = match resolved.as_ref() {
        Some(RedirectResolve::Merged { to, .. }) => {
            gate.map(|g| g.allows("anchor", to)).unwrap_or(true)
        }
        _ => true,
    };
    // 被闸拒 ⇒ 一个字都不问（问一次就是一次泄露面）。
    let name = if allowed {
        // W20c（用户 2026-11-06 拍板 B）：只有 `kind='person'` 走别名。`display_name` 取的是
        // "最新有效别名"，不是被挑过的显示名——对 work/agent 这类锚，正名才是稳定标识，
        // 用别名替换只会让标签随别名史漂。person 与旧主人锚（kind 也是 person）走显示名。
        let target_id = resolved
            .as_ref()
            .map(|r| r.terminal_or_self().to_string())
            .unwrap_or_else(|| id.to_string());
        let is_person = m
            .conn()
            .query_row("SELECT kind FROM anchor WHERE id=?1", [&target_id], |r| {
                r.get::<_, String>(0)
            })
            .is_ok_and(|kind| kind == "person");
        if is_person {
            reader::display_name(m, &target_id)
                .ok()
                .or_else(|| reader::anchor_names(m, &target_id).ok().flatten().map(|n| n.canonical))
        } else {
            reader::anchor_names(m, &target_id).ok().flatten().map(|n| n.canonical)
        }
    } else {
        None
    }
    .unwrap_or_else(|| id.chars().take(8).collect());
    cache.insert(id.to_string(), name.clone());
    name
}

fn scope_text(scope: Option<&str>) -> String {
    scope.map(|s| format!("范围 {s}：")).unwrap_or_else(|| "全库：".to_string())
}

fn age_text(ms: i64, now_ms: i64) -> String {
    let delta = now_ms - ms;
    let future = delta < 0;
    let secs = delta.abs() / 1000;
    let (n, unit) = if secs < 60 {
        (secs, "秒")
    } else if secs < 3600 {
        (secs / 60, "分钟")
    } else if secs < 86_400 {
        (secs / 3600, "小时")
    } else {
        (secs / 86_400, "天")
    };
    if future {
        format!("还有 {n} {unit}")
    } else {
        format!("{n} {unit}前")
    }
}

fn overview(
    m: &MemoryHandle, scope: Option<&str>, viewer: &Viewer, now_ms: i64,
) -> Result<Value, MemError> {
    let (rows, hidden_rows, archived, view_gate) = visible(m, scope, None, None, viewer, now_ms)?;
    // 事实通道的身份沉默：八个计数与两张清单都从 `rows` 出，所以"零可见"要与"库里真没有"
    // 分开看。agenda 只认承诺，这里不同——overview 的内容就是所有事实行，任何被藏的行都算。
    let facts_hidden = rows.is_empty() && !hidden_rows.is_empty();
    let mut cache = HashMap::new();
    let open: Vec<&FactRow> = rows
        .iter()
        .filter(|r| is_commitment(&r.category) && is_open(r.status.as_deref()))
        .collect();
    let dated = open.iter().filter(|r| r.due_at_ms.is_some()).count();
    let overdue = open
        .iter()
        .filter(|r| r.due_at_ms.is_some_and(|d| d < now_ms))
        .count();
    let decisions: Vec<&FactRow> = rows
        .iter()
        .filter(|r| r.category == "decision" && r.created_at_ms >= now_ms - RECENT_MS)
        .collect();
    let events: Vec<&FactRow> = rows
        .iter()
        .filter(|r| {
            matches!(r.category.as_str(), "event" | "state_change" | "observation")
                && r.at_ms >= now_ms - RECENT_MS
        })
        .collect();

    let (entity_count, top_entities) = entity_summary(m, scope)?;
    let review = review_counts(m)?;
    let (states, states_hidden) = current_states(m, scope, viewer, now_ms, &mut cache)?;
    let last_activity = rows.iter().map(|r| r.created_at_ms).max();

    let mut lines = vec![format!(
        "{}未完成承诺 {} 条（有截止 {}，已逾期 {}）；最近 7 天决定 {} 条、事件 {} 条；活跃实体 {} 个；未解决审核 {} 项。",
        scope_text(scope),
        open.len(),
        dated,
        overdue,
        decisions.len(),
        events.len(),
        entity_count,
        review.unresolved
    )];
    // 零可见且**不是**真没有：事实行过了 scope、却被读闸整片拒掉。这一句紧跟在计数后面
    // （此时承诺清单为空），空库/scope 筛空/部分可见时一个字都不多。与状态通道分开成两句：
    // 两个布尔各管各的，谁都不许替对方说话。
    if facts_hidden {
        lines.push(
            "当前身份看不到任何事实行（库里有，扫描到的都被读闸拒）；若这不是你期望的身份，\
             检查 `--agent-id` / `clientInfo.name`；旧行可能还需要 `hub ownership backfill`。"
                .to_string(),
        );
    }
    for r in open.iter().take(5) {
        let name = name_of(m, &r.subject_anchor_id, &mut cache, Some(&view_gate));
        let due = r
            .due_at_ms
            .map(|d| format!("，{}", age_text(d, now_ms)))
            .unwrap_or_default();
        lines.push(format!("- [{}] {}：{}{}", r.category, name, r.text, due));
    }
    // 状态进 `brief`：那一段是我默认读的入口。只在 JSON 数组里出现的状态等于写进了库
    // 但没人会被告诉它变了。最多念五条，多出来的看 `current_states`。
    if !states.is_empty() {
        let shown: Vec<&str> = states.iter().take(5).map(|s| s.line.as_str()).collect();
        let more = if states.len() > shown.len() {
            format!("（另有 {} 条见 current_states）", states.len() - shown.len())
        } else {
            String::new()
        };
        lines.push(format!("当前状态：{}。{}", shown.join("；"), more));
    }
    // 零可见且**不是**真没有：状态行过了 scope、却被读闸整片拒掉。这一句只在这里说，
    // 不塞进 `if !states.is_empty()` 的 else——空表/scope 筛空时 brief 一个字都不许多
    // （`p7` 的 `an_empty_state_table_reads_a_zero` 断的就是那条）。
    // 措辞只说"这份读者身份看不到"，不断言身份传错：`Viewer::agent` 的 scope 是 `None`，
    // 别人的 project 行对"名字对但没声明 scope"的读者同样会被拒（gate.rs 的 `scope_eq`）。
    if states_hidden {
        lines.push(
            "当前身份看不到任何状态行（库里有状态行，扫描到的都被读闸拒）；若这不是你期望的身份，\
             检查 `--agent-id` / `clientInfo.name`；旧行可能还需要 `hub ownership backfill`。"
                .to_string(),
        );
    }

    if archived > 0 {
        lines.push(format!("（另有 {} 条已归档，未计入本视图。）", archived));
    }
    Ok(json!({
        "view": "overview",
        "generated_at_ms": now_ms,
        "scope": scope,
        "brief": lines.join("\n"),
        "states_hidden_by_identity": states_hidden,
        "facts_hidden_by_identity": facts_hidden,
        "facts_hidden_by_value": archived,
        "counts": {
            "open_commitments": open.len(),
            "dated_commitments": dated,
            "overdue_commitments": overdue,
            "recent_decisions": decisions.len(),
            "recent_events": events.len(),
            "active_entities": entity_count,
            "unresolved_review": review.unresolved,
            "current_states": states.len(),
        },
        "open_commitments": open.iter().take(20).map(|r| item_json(m, r, &mut cache, now_ms, Some(&view_gate))).collect::<Vec<_>>(),
        "recent_decisions": decisions.iter().take(10).map(|r| item_json(m, r, &mut cache, now_ms, Some(&view_gate))).collect::<Vec<_>>(),
        "top_entities": top_entities,
        "review": review.json(),
        "current_states": states.iter().map(|s| s.json.clone()).collect::<Vec<_>>(),
        "last_activity_ms": last_activity,
    }))
}

fn agenda(
    m: &MemoryHandle, scope: Option<&str>, viewer: &Viewer, now_ms: i64, limit: usize,
) -> Result<Value, MemError> {
    let (rows, hidden_rows, archived, view_gate) = visible(m, scope, None, None, viewer, now_ms)?;
    // agenda 的内容是**未完成承诺**：被藏的决策/事件、已完成的承诺都不算
    // "看不到任何事项"（完成行本来也不进这一页）。
    let rows_hidden = rows.is_empty()
        && hidden_rows
            .iter()
            .any(|r| is_commitment(&r.category) && is_open(r.status.as_deref()));
    let mut cache = HashMap::new();
    let mut dated: Vec<&FactRow> = rows
        .iter()
        .filter(|r| is_commitment(&r.category) && is_open(r.status.as_deref()) && r.due_at_ms.is_some())
        .collect();
    dated.sort_by_key(|r| r.due_at_ms.unwrap_or(i64::MAX));
    let undated: Vec<&FactRow> = rows
        .iter()
        .filter(|r| is_commitment(&r.category) && is_open(r.status.as_deref()) && r.due_at_ms.is_none())
        .collect();
    let overdue = dated
        .iter()
        .filter(|r| r.due_at_ms.is_some_and(|d| d < now_ms))
        .count();

    let mut brief = format!(
        "{}到期事项 {} 条（逾期 {}），未定日期承诺 {} 条。",
        scope_text(scope),
        dated.len(),
        overdue,
        undated.len()
    );
    if archived > 0 {
        brief.push_str(&format!("
（另有 {} 条已归档，未计入本视图。）", archived));
    }
    // 只在"库里有、读闸后为空"时说；空表/scope 筛空/部分可见一个字都不多。
    if rows_hidden {
        brief.push('\n');
        brief.push_str(
            "当前身份看不到任何事项（库里有，扫描到的都被读闸拒）；若这不是你期望的身份，\
             检查 `--agent-id` / `clientInfo.name`。",
        );
    }
    let items: Vec<Value> = dated
        .iter()
        .take(limit)
        .map(|r| {
            let mut v = item_json(m, r, &mut cache, now_ms, Some(&view_gate));
            let due = r.due_at_ms.unwrap_or(0);
            v["overdue"] = json!(due < now_ms);
            v["due_in"] = json!(age_text(due, now_ms));
            v
        })
        .collect();
    Ok(json!({
        "view": "agenda",
        "generated_at_ms": now_ms,
        "scope": scope,
        "brief": brief,
        "rows_hidden_by_identity": rows_hidden,
        "facts_hidden_by_value": archived,
        "counts": {"dated": dated.len(), "overdue": overdue, "undated": undated.len()},
        "items": items,
        "undated": undated.iter().take(10).map(|r| item_json(m, r, &mut cache, now_ms, Some(&view_gate))).collect::<Vec<_>>(),
    }))
}

fn timeline(
    m: &MemoryHandle, scope: Option<&str>, from_ms: Option<i64>, to_ms: Option<i64>,
    viewer: &Viewer, now_ms: i64, limit: usize,
) -> Result<Value, MemError> {
    let (rows, hidden_rows, archived, view_gate) = visible(m, scope, from_ms, to_ms, viewer, now_ms)?;
    // timeline 的内容是所有事实行：被藏的行非空、且可见行为空，才是身份沉默。
    let rows_hidden = rows.is_empty() && !hidden_rows.is_empty();
    let mut cache = HashMap::new();
    let items: Vec<Value> = rows.iter().take(limit).map(|r| item_json(m, r, &mut cache, now_ms, Some(&view_gate))).collect();
    let mut brief = format!(
        "{}时间窗 {}：{} 条。",
        scope_text(scope),
        match (from_ms, to_ms) {
            (Some(a), Some(b)) => format!("{a}..{b}"),
            (Some(a), None) => format!("{a} 起"),
            (None, Some(b)) => format!("截至 {b}"),
            (None, None) => "全量".to_string(),
        },
        items.len()
    );
    if archived > 0 {
        brief.push_str(&format!("
（另有 {} 条已归档，未计入本视图。）", archived));
    }
    // 空时间窗必须保持原句：那是"真没有"，不是身份沉默。只有"SQL 后有行、闸后为空"才补句。
    if rows_hidden {
        brief.push('\n');
        brief.push_str(
            "当前身份看不到时间窗内的任何条目（库里有，扫描到的都被读闸拒）；若这不是你期望的身份，\
             检查 `--agent-id` / `clientInfo.name`。",
        );
    }
    Ok(json!({
        "view": "timeline",
        "generated_at_ms": now_ms,
        "scope": scope,
        "from_ms": from_ms,
        "to_ms": to_ms,
        "brief": brief,
        "rows_hidden_by_identity": rows_hidden,
        "facts_hidden_by_value": archived,
        "counts": {"items": items.len()},
        "items": items,
    }))
}

fn item_json(m: &MemoryHandle, r: &FactRow, cache: &mut HashMap<String, String>, now_ms: i64,
             gate: Option<&GateIndex>) -> Value {
    json!({
        "fact_id": r.id,
        "category": r.category,
        "text": r.text,
        "subject": name_of(m, &r.subject_anchor_id, cache, gate),
        "scope": r.scope_key,
        "due_at_ms": r.due_at_ms,
        "due_in": r.due_at_ms.map(|d| age_text(d, now_ms)),
        "status": r.status,
        "at_ms": r.at_ms,
        "age": age_text(r.at_ms, now_ms),
    })
}

fn entity_summary(m: &MemoryHandle, scope: Option<&str>) -> Result<(i64, Vec<Value>), MemError> {
    let (count_sql, top_sql);
    let (count, top) = match scope {
        Some(s) => {
            count_sql = "SELECT count(*) FROM anchor WHERE status='active' AND scope_key=?1";
            top_sql = "SELECT canonical, kind FROM anchor WHERE status='active' AND scope_key=?1 \
                       ORDER BY updated_at_ms DESC, canonical LIMIT 5";
            let count: i64 = m.conn().query_row(count_sql, [s], |r| r.get(0))?;
            let mut st = m.conn().prepare(top_sql)?;
            let rows = st.query_map([s], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut out = Vec::new();
            for row in rows { let (name, kind) = row?; out.push(json!({"name": name, "kind": kind})); }
            (count, out)
        }
        None => {
            count_sql = "SELECT count(*) FROM anchor WHERE status='active'";
            top_sql = "SELECT canonical, kind FROM anchor WHERE status='active' \
                       ORDER BY updated_at_ms DESC, canonical LIMIT 5";
            let count: i64 = m.conn().query_row(count_sql, [], |r| r.get(0))?;
            let mut st = m.conn().prepare(top_sql)?;
            let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut out = Vec::new();
            for row in rows { let (name, kind) = row?; out.push(json!({"name": name, "kind": kind})); }
            (count, out)
        }
    };
    Ok((count, top))
}

struct ReviewCounts {
    pending_review: i64,
    quarantined: i64,
    approved: i64,
    unresolved: i64,
}

impl ReviewCounts {
    fn json(&self) -> Value {
        json!({
            "pending_review": self.pending_review,
            "quarantined": self.quarantined,
            "approved": self.approved,
            "unresolved": self.unresolved,
        })
    }
}

fn review_counts(m: &MemoryHandle) -> Result<ReviewCounts, MemError> {
    let mut st = m.conn().prepare(
        "SELECT state, count(*) FROM review_item GROUP BY state",
    )?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    let mut c = ReviewCounts { pending_review: 0, quarantined: 0, approved: 0, unresolved: 0 };
    for row in rows {
        let (state, n) = row?;
        match state.as_str() {
            "pending_review" => c.pending_review = n,
            "quarantined" => c.quarantined = n,
            "approved" => c.approved = n,
            _ => {}
        }
        if !matches!(state.as_str(), "applied" | "rejected" | "stale") {
            c.unresolved += n;
        }
    }
    Ok(c)
}

/// 一条当前状态的消费行。JSON 面给字段、`brief` 面给一句话——**同一个来源**，
/// 不让两个消费面各读一遍库、各拼一套主语。
struct StateLine {
    json: Value,
    line: String,
}

/// 当前状态（P2 的读侧）。取数与中文词表都在 `rc_memory::reader`（`live_states` /
/// `dimension_label`），召回面那份状态块用同一份——两处各写一套 SQL 与各写一份词表的话，
/// 同一个维度在两个视图里会说出两种话，读者分不清是两格还是一格写歪了。
/// 这里只补本层三条纪律：
/// - **过读闸**：`GateIndex::allows("state_assertion", id)`。这一格原先漏着——
///   `state_assertion` 一直没有生产者时它不存在，P2 接上生产者之后它就是跨 owner 泄露
///   （mcp 的 p7 用例把它抓成红）。
/// - **主语是名字**：裸 `anchor_id` 对读者没有语义；名字与承诺/决定两栏同一处解析（`name_of`），
///   `subject_anchor_id` 仍原样带出给程序读者连接用。
/// - **带出处**：`evidence_turn_uid` 必须在场，否则一条状态与一句编造不可分。
///
/// 返回值第二格是**身份沉默**：`content_passed > 0 && shown == 0`。两个计数都在既有的
/// 「取数 → scope → 读闸」循环里顺带累加，不新增查询：
/// - `content_passed` 是**扫描帽内**的下界（`STATE_SCAN` 先截后滤），所以对外只给布尔，
///   不给"库里有 N 条你看不到"——那个 N 最多是 20、且可能漏算帽外的行；
/// - `allows` 比 `decide` 窄：没有归属行的旧状态同样会走到"零可见"，所以 hint 里要提
///   `hub ownership backfill`，不能把沉默读成"身份一定传错了"。
fn current_states(
    m: &MemoryHandle,
    scope: Option<&str>,
    viewer: &Viewer,
    now_ms: i64,
    cache: &mut HashMap<String, String>,
) -> Result<(Vec<StateLine>, bool), MemError> {
    let rows = rc_memory::reader::live_states(m, None, STATE_SCAN)?;
    let gate = GateIndex::load(m, viewer, now_ms)?;
    let mut out = Vec::new();
    let mut content_passed = 0usize;
    for row in rows {
        if !scope_allows(m, &row.id, scope)? {
            continue;
        }
        // 过了内容过滤（对状态来说 scope 是唯一的内容过滤）——读闸之前。
        content_passed += 1;
        if !gate.allows("state_assertion", &row.id) {
            continue;
        }
        let name = name_of(m, &row.subject_anchor_id, cache, Some(&gate));
        // C1：`inferred` 行必须在自然语言面读得出来（降级才不是暗账）；`expressed` 行
        // 一个字节都不许变——那是既有消费面的对照基线。
        let line = format!(
            "{name} 的{}是「{}」",
            rc_memory::reader::dimension_label(&row.dimension),
            row.value_after
        );
        let line = if row.mode == rc_memory::STATE_MODE_INFERRED {
            format!("{line}（推断）")
        } else {
            line
        };
        out.push(StateLine {
            json: json!({
                "subject": name,
                "subject_anchor_id": row.subject_anchor_id,
                "dimension": row.dimension,
                "value": row.value_after,
                "mode": row.mode,
                "valid_from_ms": row.valid_from_ms,
                "evidence_turn_uid": row.evidence_turn_uid,
            }),
            line,
        });
    }
    // 读闸之后：有内容、一条没留下 ⇒ 不是"库里没有"，是"这份身份看不见"。
    let hidden = content_passed > 0 && out.is_empty();
    Ok((out, hidden))
}

/// `scope` 那一格对状态行的过滤。
///
/// 为什么在闸之前逐行判、不把 `scope_key` 塞进取数 SQL：`live_states` 是两份读者共用的
/// **唯一**取数点，召回面按主体取、没有 scope 参数。把它做成第二个可选条件会让那条查询
/// 长出一个"只有一侧用"的分支。状态行是低频小表，这里多一次定点查询换"取数点只有一处"。
fn scope_allows(m: &MemoryHandle, id: &str, scope: Option<&str>) -> Result<bool, MemError> {
    let want = match scope {
        None => return Ok(true),
        Some(key) => key,
    };
    let got: Option<String> = m.conn().query_row(
        "SELECT scope_key FROM state_assertion WHERE id=?1",
        [id],
        |r| r.get(0),
    )?;
    Ok(got.as_deref() == Some(want))
}

