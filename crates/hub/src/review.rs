//! S3a-3d：`hub review` 的命令面（设计 `2026-10-26-review-queue-s3` §9）。
//!
//! 分层与 `ownership` 一致：本模块持输出结构（`#[derive(Serialize)]`）与命令实现，
//! `main.rs` 只做 clap 解析、开库与退出码。队列的读写仍全部经 `rc_memory::review`：
//! 这里不直接拼 SQL 写 `review_item` / `review_decision`——`show` 为补齐 §9 的四列做一条
//! 只读 SELECT；`run`（S3b-4a）经 `rc_distill::review::run` 的引擎写判定，逐轮终态经
//! `rc_distill::review::run::mark_turns` 写 `extract_pass_state`。S4：applied 之后交给
//! `rc_distill::review::promote_applied_cards`——它按 `outcome.card_ids`（写库层带出来的
//! "这次真的插下的卡"）逐条 CAS 提升；hub 不解析 `payload_json`、也不重算卡的身份。
//!
//! 四条纪律：
//! - **默认不打印载荷/引文**：`list` 只给元数据；`show` 只有 `--payload` 才多一行载荷。
//! - **人工判定只追加**：`decide` 走 `record_decision(stage=Human, decider=Human)`，
//!   `--approve` 只把状态推进 `approved`，实际写库由后续 `hub review run` 执行（S3b）。
//! - **`run` 不放宽**：`quarantined` 复审走同一套窄审；LLM 故障只退避不写库；额度耗尽、
//!   存储失败立即停（避免级联）。
//! - **错误面静态**：`ReviewError` 只带类别与一句静态话，不回显 id/载荷/路径。

use rc_distill::review::run::{
    mark_batch, mark_turns, run_item_with_policy, write_watermark, RetryPolicy, RunError,
};
use rc_distill::DistillError;
use rc_ledger::pass_state::PassState;
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::{compute_mem_id, MemKey, MemTable, Scope, ScopeKind, Verdict};
use rc_memory::review::{
    self as queue, Decider, ItemMeta, ReviewStage, ReviewState, ReviewStats, ReviewVerdict,
    RETRY_BASE_MS,
};
use rc_store::{MemoryHandle, Root};
use serde::Serialize;

/// 人工判定的规则集版本（与程序闸同一族，设计 §3.2）。
pub const RULES_VERSION: &str = "review-gate-v0.1";

const FAULT_STATE: &str = "review --state must be pending_review|approved|applied|rejected|quarantined|stale";
// 闭集与 `check_pass` 一致（波三修 1/2 起含 `state`/`lesson`；`reuse`/`report` 只走 decide，不在内）。
const FAULT_PASS: &str = "review --pass must be naming|fact|event|time|rows|state|lesson";
const FAULT_REASON: &str = "review --reason must match [a-z0-9_]+";
/// `review run` 自己的来源错误面（C5）：这两句以前复用 `distill`/`cycle once` 的文案，
/// 操作员照它去跑另一条命令只会再撞一次墙。零插值、点名旗标名、不带路径。
pub const FAULT_REVIEW_NO_LLM_SOURCE: &str =
    "no llm reply source — pass --replies FILE or --llm-config FILE to: hub review run";
pub const FAULT_REVIEW_LLM_TIMEOUT: &str = "review run --llm-timeout-ms must be greater than zero";
/// H7a：`review prune` 的天数形状。0 天等于"清掉全部终态载荷"，负天数的 cutoff 落在未来——
/// 两者都只是把库一次清空，不是清理，按用法错误挡在开库之前。
pub const FAULT_REVIEW_PRUNE_DAYS: &str =
    "review prune --older-than-days must be greater than zero";

/// 一天的毫秒数（cutoff 与 7 天告警共用这一格，避免两处各写一遍 86_400_000）。
const DAY_MS: i64 = 86_400_000;

/// H7a：`review stats` 的待审告警阈值——最老 `pending_review` 的年龄超过 7 天。
pub const PENDING_ALERT_MS: i64 = 7 * DAY_MS;

/// 命令面的失败类别。`main` 把它们折成退出码 3 与一句静态话。
#[derive(Debug)]
pub enum ReviewError {
    /// 旗标形状/取值问题（静态话，零插值）。
    Usage(&'static str),
    /// `show`/`decide` 的 id 不存在。
    NotFound,
    /// H2：队列状态机拒绝（item 当前状态不可判定）。与存储故障分开：它不是"再试一次"
    /// 也不是"跑 doctor"，是一条**业务裁定**——`main` 据此给一句 review 自己的静态话。
    Illegal,
    /// 存储/IO 故障。
    Store,
}

fn store_err() -> ReviewError {
    ReviewError::Store
}

/// `list` 的输出行（`--json` 是同一组字段；`ItemMeta` 本身不带 `Serialize`）。
#[derive(Debug, Clone, Serialize)]
pub struct ItemMetaOut {
    pub id: i64,
    pub source: String,
    pub pass_id: String,
    pub family: String,
    pub session_key: String,
    pub device_id: String,
    pub state: String,
    pub program_code: Option<String>,
    pub attempt: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub applied_at_ms: Option<i64>,
}

impl From<ItemMeta> for ItemMetaOut {
    fn from(m: ItemMeta) -> Self {
        Self {
            id: m.id,
            source: m.source,
            pass_id: m.pass_id,
            family: m.family,
            session_key: m.session_key,
            device_id: m.device_id,
            state: m.state.slug().to_string(),
            program_code: m.program_code,
            attempt: m.attempt,
            created_at_ms: m.created_at_ms,
            updated_at_ms: m.updated_at_ms,
            applied_at_ms: m.applied_at_ms,
        }
    }
}

/// 判定历史的一行。比 `rc_memory::review::DecisionRow` 多 model 等列（见模块文档）。
///
/// H4b：`input_sha256` 是载荷指纹（三个 stage 同格），`review_input_sha256` 只在真正送审的
/// review 行有值（送审输入指纹）；其它 stage 行与程序侧的 review 裁定为空串。JSON 自然带出
/// 两列，文本行不变（见 [`render_show`]）。
#[derive(Debug, Clone, Serialize)]
pub struct DecisionOut {
    pub id: i64,
    pub item_id: i64,
    pub stage: String,
    pub verdict: String,
    pub reason_code: String,
    pub rules_version: String,
    pub model: String,
    pub prompt_version: String,
    pub input_sha256: String,
    pub review_input_sha256: String,
    pub decider: String,
    pub detail: String,
    pub created_at_ms: i64,
}

/// `show` 的结构化对象：meta 摊平在顶层，判定历史在 `decisions`，
/// `payload_json` 只有给了 `--payload` 才出现。
#[derive(Debug, Clone, Serialize)]
pub struct ShowOut {
    #[serde(flatten)]
    pub item: ItemMetaOut,
    pub decisions: Vec<DecisionOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_json: Option<String>,
}

/// 一个 pass 的六态计数（顺序与 [`StatsOut`] 的六个字段相同）。
#[derive(Debug, Clone, Serialize)]
pub struct PassStats {
    pub pass_id: String,
    pub pending_review: i64,
    pub approved: i64,
    pub applied: i64,
    pub rejected: i64,
    pub quarantined: i64,
    pub stale: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsOut {
    pub pending_review: i64,
    pub approved: i64,
    pub applied: i64,
    pub rejected: i64,
    pub quarantined: i64,
    pub stale: i64,
    pub oldest_pending_age_ms: Option<i64>,
    /// H7a：最老 `pending_review` 已超过 7 天（`oldest_pending_age_ms` 为 `None` ⇒ false）。
    /// 阈值不配置化：这是维护面的第一条红线，多一个开关就多一种"为什么没报"。
    pub oldest_pending_over_7d: bool,
    pub by_pass: Vec<PassStats>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecideOut {
    pub id: i64,
    pub state: String,
}

/// `hub review prune` 的读数（`--json` 是同一组字段）。
#[derive(Debug, Clone, Serialize)]
pub struct PruneOut {
    /// 本次清掉载荷的 item 数（已清空的行不重复计，见 `rc_memory::review::prune`）。
    pub pruned: usize,
    /// 操作员给的窗口（天），原样回显，便于对照"我请的是什么、它做了什么"。
    pub older_than_days: i64,
}

/// 该 item 的判定台账（全列，只读）。stage/verdict 仍先过 `parse`：读不出的判定
/// 不能折成"没有判定"。
fn decision_rows(m: &MemoryHandle, item_id: i64) -> Result<Vec<DecisionOut>, ReviewError> {
    let mut st = m
        .conn()
        .prepare(
            "SELECT id, item_id, stage, verdict, reason_code, rules_version, model,
                    prompt_version, input_sha256, review_input_sha256, decider, detail,
                    created_at_ms
               FROM review_decision WHERE item_id=?1 ORDER BY id",
        )
        .map_err(|_| store_err())?;
    let mut rows = st.query([item_id]).map_err(|_| store_err())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(|_| store_err())? {
        let stage: String = row.get(2).map_err(|_| store_err())?;
        let verdict: String = row.get(3).map_err(|_| store_err())?;
        if ReviewStage::parse(&stage).is_none() || ReviewVerdict::parse(&verdict).is_none() {
            return Err(store_err());
        }
        out.push(DecisionOut {
            id: row.get(0).map_err(|_| store_err())?,
            item_id: row.get(1).map_err(|_| store_err())?,
            stage,
            verdict,
            reason_code: row.get(4).map_err(|_| store_err())?,
            rules_version: row.get(5).map_err(|_| store_err())?,
            model: row.get(6).map_err(|_| store_err())?,
            prompt_version: row.get(7).map_err(|_| store_err())?,
            input_sha256: row.get(8).map_err(|_| store_err())?,
            review_input_sha256: row.get(9).map_err(|_| store_err())?,
            decider: row.get(10).map_err(|_| store_err())?,
            detail: row.get(11).map_err(|_| store_err())?,
            created_at_ms: row.get(12).map_err(|_| store_err())?,
        });
    }
    Ok(out)
}

/// `hub review list`：元数据行，载荷/turn_uid 一律不返回。
pub fn list(
    root: &Root,
    state: Option<&str>,
    pass: Option<&str>,
    limit: usize,
) -> Result<Vec<ItemMetaOut>, ReviewError> {
    let state = match state {
        None => None,
        Some(slug) => Some(ReviewState::parse(slug).ok_or(ReviewError::Usage(FAULT_STATE))?),
    };
    if let Some(p) = pass {
        // 刀 B：`reuse` 与 `report` 同族（不是抽取 pass，但列表读数要能按它过滤——
        // `hub review list --pass reuse` 是复核队列的唯一读数口）。
        if !matches!(p, "naming" | "fact" | "event" | "time" | "rows" | "state" | "reuse") {
            return Err(ReviewError::Usage(FAULT_PASS));
        }
    }
    let items = queue::list(root.memory(), state, pass, limit).map_err(|_| store_err())?;
    Ok(items.into_iter().map(ItemMetaOut::from).collect())
}

/// `hub review show`：meta + 判定历史；`with_payload` 才带载荷。
pub fn show(root: &Root, id: i64, with_payload: bool) -> Result<ShowOut, ReviewError> {
    let item = queue::get(root.memory(), id)
        .map_err(|_| store_err())?
        .ok_or(ReviewError::NotFound)?;
    let decisions = decision_rows(root.memory(), id)?;
    Ok(ShowOut {
        item: ItemMetaOut::from(item.meta),
        decisions,
        payload_json: with_payload.then_some(item.payload_json),
    })
}

/// `--reason` 的唯一尺子：非空、且只含小写字母/数字/下划线。
///
/// `review decide --reason` 与 `distill reset --reason` 共用这一把——两个闭集是同一件事，
/// 各抄一份只会让它们有一天不一样。
pub fn reason_slug_ok(reason: &str) -> bool {
    !reason.is_empty()
        && reason
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// `hub review decide`：追加一条 `decider=human` 判定并推进/回退状态。
/// `approve=true` 只到 `approved`——写库留给 S3b 的 `hub review run`。
pub fn decide(
    root: &Root,
    id: i64,
    approve: bool,
    reason: &str,
    now_ms: i64,
) -> Result<String, ReviewError> {
    if !reason_slug_ok(reason) {
        return Err(ReviewError::Usage(FAULT_REASON));
    }
    let item = queue::get(root.memory(), id)
        .map_err(|_| store_err())?
        .ok_or(ReviewError::NotFound)?;
    // H7c2：人工判定成功后要写该 item 的 pass 水位，载荷指纹那一格要 move 进 decision，
    // 所以 pass_id 先取出来。
    let pass_id = item.meta.pass_id.clone();
    let decision = queue::Decision {
        stage: ReviewStage::Human,
        verdict: if approve {
            ReviewVerdict::Pass
        } else {
            ReviewVerdict::Reject
        },
        reason_code: reason.to_string(),
        rules_version: RULES_VERSION.to_string(),
        model: String::new(),
        prompt_version: String::new(),
        input_sha256: item.payload_sha256,
        // H4b：人工裁定没有送审输入（不调模型）⇒ 送审指纹为空串。
        review_input_sha256: String::new(),
        decider: Decider::Human,
        detail: "operator".to_string(),
    };
    // H2：非法转换（如 `applied` 上人工改判）是业务裁定，不是存储故障——不再落进
    // "review write failed — run: doctor" 那一格。
    let state = queue::record_decision(root.memory(), id, &decision, now_ms).map_err(|e| {
        if e.is_review_conflict() {
            ReviewError::Illegal
        } else {
            store_err()
        }
    })?;
    // H7c2：判定已落，写该 pass 的审查轴水位。best-effort：水位是读数，写不进去不改变
    // 这次人工判定的结果（下一次引擎收尾会再写一遍）。
    let _ = write_watermark(root.memory(), root.ledger(), &pass_id, now_ms);
    Ok(state.slug().to_string())
}

/// `hub review stats`：六态计数、最老 pending 年龄（含 7 天告警）、按 pass 分组。
pub fn stats(root: &Root, now_ms: i64) -> Result<StatsOut, ReviewError> {
    let s = queue::stats(root.memory(), now_ms).map_err(|_| store_err())?;
    Ok(StatsOut {
        pending_review: s.pending_review,
        approved: s.approved,
        applied: s.applied,
        rejected: s.rejected,
        quarantined: s.quarantined,
        stale: s.stale,
        oldest_pending_age_ms: s.oldest_pending_age_ms,
        oldest_pending_over_7d: oldest_pending_over_7d(s.oldest_pending_age_ms),
        by_pass: s
            .by_pass
            .into_iter()
            .map(|(pass_id, c)| PassStats {
                pass_id,
                pending_review: c[0],
                approved: c[1],
                applied: c[2],
                rejected: c[3],
                quarantined: c[4],
                stale: c[5],
            })
            .collect(),
    })
}

/// 7 天告警的唯一判据：严格大于（正好 7 天不报）。
fn oldest_pending_over_7d(age_ms: Option<i64>) -> bool {
    age_ms.is_some_and(|age| age > PENDING_ALERT_MS)
}

// ── `hub review backfill-verdict`（T1）────────────────────────────────────────

/// `--pass` 的闭集。naming/time（还有 rows/state）**不产卡**：它们的载荷里没有"卡片正文"
/// 这一格，把它们扫进来只会算出 0 张卡，报错而不静默空跑。
pub const BACKFILL_PASSES: [&str; 2] = ["fact", "event"];
pub const FAULT_BACKFILL_PASS: &str = "review backfill-verdict --pass must be fact|event";
/// `--revert` 说的是"只回滚这份清单"，与扫描的三个旗标（`--dry-run`/`--manifest`/`--pass`）
/// 是两件事；同给会让读者以为"先扫再回滚"或"回滚后落单"，静默取一个就是在说谎。
pub const FAULT_BACKFILL_REVERT_COMBINED: &str =
    "review backfill-verdict --revert cannot be combined with --dry-run/--manifest/--pass";
pub const FAULT_BACKFILL_MANIFEST: &str =
    "review backfill-verdict --revert needs a manifest of card ids";

/// 卡 title 的长度：**与写库点逐字同一格**（`crates/rc-memory/src/writers_fact.rs:465` 的
/// `it.text.chars().take(30)`）。重算 id 时 title 差一个 char 就命中不了，所以它必须是常量
/// 而不是在两处各写一个 30。
const CARD_TITLE_CHARS: usize = 30;

/// 一次回填对一条 item 的处置（`--json` 的 `action` 列，闭集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackfillAction {
    /// 命中卡且它是 `unreviewed`：真跑会（干跑"会"）用 CAS 提升。
    Promote,
    /// 命中卡但无需提升：已是 `passed`（幂等），或已被 audit 判成 `low`/`failed`
    /// ——后者是 audit 的结论，review 不许翻案。[`BackfillRow::verdict_before`] 带着实值，
    /// 两种"已经"因此分得开。
    Already,
    /// 重算出的 id 在 `card` 里没有那一行：当年 `INSERT OR IGNORE` 去重把它吞了。
    /// **不是错误**（现网 46 项里 10 项如此）。
    Absent,
    /// 防御支：item 的 pass 不在本次扫描集里。SQL 已经按 pass 过滤，这一支只有
    /// 过滤条件被改动时才可达——留着是让 `action` 的闭集与四道闸一一对应（与
    /// `run_audit` 里 `Verdict::Unreviewed` 那一支同一个用途：穷尽是守卫）。
    SkipPass,
}

impl BackfillAction {
    fn slug(self) -> &'static str {
        match self {
            BackfillAction::Promote => "promote",
            BackfillAction::Already => "already",
            BackfillAction::Absent => "absent",
            BackfillAction::SkipPass => "skip_pass",
        }
    }
}

/// 清单的一行（`--json` 的元素、干跑清单的载体）。字段顺序就是 JSON 的键序。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackfillRow {
    pub review_item_id: i64,
    pub item_index: usize,
    pub card_id: String,
    pub title: String,
    /// 卡在写之前（干跑则是"如果现在写"）的判决；`absent` 一格为 `None`。
    pub verdict_before: Option<String>,
    pub action: String,
}

/// 一次回填的读数。`promoted` 在干跑里是"会提升的条数"（不改库）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct BackfillOut {
    pub promoted: usize,
    pub already: usize,
    pub absent: usize,
    pub skipped: usize,
    /// 载荷读不出/写失败：**不整批回滚**，这一条计 error，其余照常（退出码 3，清单可续）。
    pub errors: usize,
    pub dry_run: bool,
    pub rows: Vec<BackfillRow>,
}

/// `--revert` 的读数：只回滚清单内、当前 `passed` 的 id。
#[derive(Debug, Clone, Default, Serialize)]
pub struct RevertOut {
    pub reverted: usize,
    /// 清单内但当前不是 `passed`（已退回、或被 audit 判成 low/failed）：0 行，不是错误。
    pub not_passed: usize,
    /// 清单内但库里没有这张卡。
    pub missing: usize,
    pub errors: usize,
    pub ids: usize,
}

/// `--pass` 的解析：只认 [`BACKFILL_PASSES`]，空段与闭集外都是用法错误。
/// 返回 `&'static str` 而不是 `String`：SQL 里那一段是拿它们拼的，闭集外进不来。
pub fn parse_passes(spec: Option<&str>) -> Result<Vec<&'static str>, ReviewError> {
    let Some(spec) = spec else {
        return Ok(BACKFILL_PASSES.to_vec());
    };
    let mut out: Vec<&'static str> = Vec::new();
    for token in spec.split(',') {
        let token = token.trim();
        let slug = BACKFILL_PASSES
            .iter()
            .copied()
            .find(|p| *p == token)
            .ok_or(ReviewError::Usage(FAULT_BACKFILL_PASS))?;
        if !out.contains(&slug) {
            out.push(slug);
        }
    }
    // `""`（或 `","`）split 出来的是空段，闭集比对已经不放过；这里再挡一次"空清单"。
    if out.is_empty() {
        return Err(ReviewError::Usage(FAULT_BACKFILL_PASS));
    }
    Ok(out)
}

/// 载荷 → `(scope, [(category, text)])`。读不出就是**这一条 item 的**失败（计 error），
/// 不猜、也不跳过解析继续用半份数据：一个猜错的 scope 会算出另一张卡的 id。
fn payload_items(payload_json: &str) -> Result<(Scope, Vec<(String, String)>), ()> {
    let payload: serde_json::Value = serde_json::from_str(payload_json).map_err(|_| ())?;
    let owner = payload.get("owner").ok_or(())?;
    let kind = owner
        .get("scope_kind")
        .and_then(serde_json::Value::as_str)
        .and_then(ScopeKind::parse)
        .ok_or(())?;
    let key = owner
        .get("scope_key")
        .and_then(serde_json::Value::as_str)
        .ok_or(())?;
    let items = payload
        .get("items")
        .and_then(serde_json::Value::as_array)
        .ok_or(())?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let category = item
            .get("category")
            .and_then(serde_json::Value::as_str)
            .ok_or(())?;
        let text = item
            .get("text")
            .and_then(serde_json::Value::as_str)
            .ok_or(())?;
        out.push((category.to_string(), text.to_string()));
    }
    Ok((
        Scope {
            kind,
            key: key.to_string(),
        },
        out,
    ))
}

fn card_verdict(m: &MemoryHandle, id: &str) -> Result<Option<String>, rc_memory::MemError> {
    use rusqlite::OptionalExtension;
    m.conn()
        .query_row("SELECT verdict FROM card WHERE id=?1", [id], |r| r.get(0))
        .optional()
        .map_err(rc_memory::MemError::from)
}

/// 一条 item 的处置：重算 id → 读卡 → 四道闸 → （真跑）CAS 提升。
/// 返回 `Err` 只表示存储故障；业务上的"没有/不用改"走 [`BackfillAction`]。
#[allow(clippy::too_many_arguments)]
fn backfill_one(
    m: &MemoryHandle,
    item_id: i64,
    item_index: usize,
    category: &str,
    text: &str,
    scope: &Scope,
    dry_run: bool,
    now_ms: i64,
) -> Result<BackfillRow, rc_memory::MemError> {
    // ① 卡的身份只由 `compute_mem_id` 给（内含 `normalize_body`）：本命令**不自带**
    //    规范化——那会是第二把尺子，尺子一漂就提升错卡。
    let title: String = text.chars().take(CARD_TITLE_CHARS).collect();
    let card_id = compute_mem_id(MemTable::Card, category, &title, text, scope);
    let before = card_verdict(m, &card_id)?;
    let (action, verdict_before) = match before.as_deref() {
        // ② 未命中：当年去重吞掉的那一批（`INSERT OR IGNORE` 返回 0），不是错误。
        None => (BackfillAction::Absent, None),
        // ③ 命中且 unreviewed ⇒ 提升（干跑只记账）。
        Some(v) if v == Verdict::Unreviewed.as_str() => {
            if dry_run {
                (BackfillAction::Promote, Some(v.to_string()))
            } else {
                let key = MemKey {
                    table: MemTable::Card,
                    id: card_id.clone(),
                };
                // ④ `promote_verdict` 是**唯一**的 `unreviewed → passed` 入口（单向 CAS）；
                //    0 行 = 期间被人改过，重读一次按实值记账，不瞎报 promote。
                match rc_memory::promote_verdict(m, &key, now_ms)? {
                    1 => (BackfillAction::Promote, Some(v.to_string())),
                    _ => (BackfillAction::Already, card_verdict(m, &card_id)?),
                }
            }
        }
        // 命中但已 passed ⇒ 幂等；low/failed ⇒ audit 的结论，review 不翻案。
        Some(_) => (BackfillAction::Already, before.clone()),
    };
    Ok(BackfillRow {
        review_item_id: item_id,
        item_index,
        card_id,
        title,
        verdict_before,
        action: action.slug().to_string(),
    })
}

/// `hub review backfill-verdict` 的主体：只扫 `state='applied'` 且 pass ∈ 本次集合的 item，
/// 逐项重算卡 id 并按四道闸处置。**逐条处理**：一条失败不撤销前面已经落的提升。
///
/// 四道闸（全真才写）：`state='applied'`（SQL）· pass ∈ {fact,event}（SQL + 防御支）·
/// 重算 id 命中 `card.id` · `card.verdict='unreviewed'`（CAS 的 from 在语句里再挡一次）。
pub fn backfill(
    m: &MemoryHandle,
    passes: &[&'static str],
    dry_run: bool,
    limit: usize,
    now_ms: i64,
) -> Result<BackfillOut, ReviewError> {
    let mut out = BackfillOut {
        dry_run,
        ..Default::default()
    };
    // pass 是闭集字面量（`parse_passes` 只给 BACKFILL_PASSES 里的两个），拼进 SQL 没有注入面；
    // limit 是 usize，直接拼同理。`card` 的写入不影响这条 review_item 扫描，但先把行读完
    // 再处理仍然更稳（同一条连接上不必让游标跨越自己的写）。
    let pass_list = passes
        .iter()
        .map(|p| format!("'{p}'"))
        .collect::<Vec<_>>()
        .join(",");
    let limit_clause = if limit == 0 {
        String::new()
    } else {
        format!(" LIMIT {limit}")
    };
    let sql = format!(
        "SELECT id, pass_id, payload_json FROM review_item
          WHERE state='applied' AND pass_id IN ({pass_list}) ORDER BY id{limit_clause}"
    );
    let items: Vec<(i64, String, String)> = {
        let mut st = m.conn().prepare(&sql).map_err(|_| ReviewError::Store)?;
        let rows = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|_| ReviewError::Store)?;
        let mut v = Vec::new();
        for row in rows {
            v.push(row.map_err(|_| ReviewError::Store)?);
        }
        v
    };
    for (item_id, pass_id, payload_json) in items {
        // 防御支：pass 不在这份扫描集里就一条都不碰（见 [`BackfillAction::SkipPass`]）。
        if !passes.contains(&pass_id.as_str()) {
            out.skipped += 1;
            out.rows.push(BackfillRow {
                review_item_id: item_id,
                item_index: 0,
                card_id: String::new(),
                title: String::new(),
                verdict_before: None,
                action: BackfillAction::SkipPass.slug().to_string(),
            });
            continue;
        }
        let (scope, payload) = match payload_items(&payload_json) {
            Ok(parsed) => parsed,
            // 载荷形状读不出：这一条计 error（退出码 3），下一轮还会再试。
            Err(()) => {
                out.errors += 1;
                continue;
            }
        };
        for (index, (category, text)) in payload.iter().enumerate() {
            match backfill_one(m, item_id, index, category, text, &scope, dry_run, now_ms) {
                Ok(row) => {
                    match row.action.as_str() {
                        "promote" => out.promoted += 1,
                        "already" => out.already += 1,
                        "absent" => out.absent += 1,
                        _ => out.skipped += 1,
                    }
                    out.rows.push(row);
                }
                Err(_) => out.errors += 1,
            }
        }
    }
    Ok(out)
}

/// 干跑清单：**就是** `--json` 的那份数组（回滚按 `card_id` 读，别的列是给人看的）。
pub fn manifest_rows(rows: &[BackfillRow]) -> String {
    serde_json::to_string_pretty(rows).unwrap_or_else(|_| "[]".to_string())
}

/// 真跑的清单：只装**实际提升成功**的卡 id（`absent`/`already` 不在里面——回滚它们等于
/// 撤销一件没发生过的事）。
pub fn manifest_promoted(rows: &[BackfillRow]) -> String {
    let ids: Vec<&str> = rows
        .iter()
        .filter(|r| r.action == BackfillAction::Promote.slug())
        .map(|r| r.card_id.as_str())
        .collect();
    serde_json::to_string_pretty(&ids).unwrap_or_else(|_| "[]".to_string())
}

/// 清单的两种形状都认：`["<id>", …]`（真跑写的）与 `[{... "card_id": "<id>" ...}, …]`
/// （干跑写的）。空串那一格是 `skip_pass` 行（没有卡可退），跳过不算错；
/// **非空但不是 64 位小写 hex 的 id 是用法错误**——它会拼进一条 UPDATE，宁可不跑。
pub fn parse_manifest(text: &str) -> Result<Vec<String>, ReviewError> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| ReviewError::Usage(FAULT_BACKFILL_MANIFEST))?;
    let array = value
        .as_array()
        .ok_or(ReviewError::Usage(FAULT_BACKFILL_MANIFEST))?;
    let mut ids: Vec<String> = Vec::new();
    for entry in array {
        let id = match entry {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Object(map) => map
                .get("card_id")
                .and_then(serde_json::Value::as_str)
                .ok_or(ReviewError::Usage(FAULT_BACKFILL_MANIFEST))?
                .to_string(),
            _ => return Err(ReviewError::Usage(FAULT_BACKFILL_MANIFEST)),
        };
        if id.is_empty() {
            continue;
        }
        let is_card_id = id.len() == 64 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !is_card_id {
            return Err(ReviewError::Usage(FAULT_BACKFILL_MANIFEST));
        }
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// `--revert`：把**清单内**、当前 `passed` 的卡退回 `unreviewed`（`revert_verdict` 的单向
/// CAS，不是 `set_verdict`——那会顺手改写 `low`/`failed`）。
///
/// 清单外的一行都碰不到：这里没有位置参数，唯一的输入就是清单本身。命中 0 行是读数
/// （已经退过、从没提升过、或那张卡不存在），不是错误；逐条处理，一条失败不撤销前面已退的。
pub fn revert(m: &MemoryHandle, ids: &[String], now_ms: i64) -> Result<RevertOut, ReviewError> {
    let mut out = RevertOut {
        ids: ids.len(),
        ..Default::default()
    };
    for id in ids {
        let before = match card_verdict(m, id) {
            Ok(v) => v,
            Err(_) => {
                out.errors += 1;
                continue;
            }
        };
        match before.as_deref() {
            None => out.missing += 1,
            Some(v) if v == Verdict::Passed.as_str() => {
                let key = MemKey {
                    table: MemTable::Card,
                    id: id.clone(),
                };
                match rc_memory::revert_verdict(m, &key, now_ms) {
                    Ok(1) => out.reverted += 1,
                    Ok(_) => out.not_passed += 1,
                    Err(_) => out.errors += 1,
                }
            }
            // 不是 passed（unreviewed/low/failed）：不是回滚的目标，原样留。
            Some(_) => out.not_passed += 1,
        }
    }
    Ok(out)
}

/// `backfill-verdict` 的两形输出。`--json` 是**清单数组**（与干跑清单逐字相同的一份）；
/// 文本形每行一条 + 一行读数。
pub fn render_backfill(out: &BackfillOut, json: bool) -> Result<String, ReviewError> {
    if json {
        return to_json(&out.rows);
    }
    let mut lines: Vec<String> = out
        .rows
        .iter()
        .map(|r| {
            format!(
                "item={} idx={} card={} before={} action={} title={}",
                r.review_item_id,
                r.item_index,
                dash_if_empty(&r.card_id),
                r.verdict_before.as_deref().unwrap_or("none"),
                r.action,
                r.title
            )
        })
        .collect();
    lines.push(format!(
        "backfill promoted={} already={} absent={} skipped={} errors={} dry_run={}",
        out.promoted, out.already, out.absent, out.skipped, out.errors, out.dry_run
    ));
    Ok(lines.join("\n"))
}

pub fn render_revert(out: &RevertOut, json: bool) -> Result<String, ReviewError> {
    if json {
        return to_json(out);
    }
    Ok(format!(
        "revert reverted={} not_passed={} missing={} errors={} ids={}",
        out.reverted, out.not_passed, out.missing, out.errors, out.ids
    ))
}

/// `hub review prune`：清终态（`applied`/`rejected`/`stale`）item 的载荷与占用列，
/// 判定台账不动、非终态不动——语义沿用存储层 [`queue::prune`]，这里只算 cutoff 与形状。
///
/// `older_than_days <= 0` 是用法错误：0 天会把全部终态载荷一次清掉，负天数的 cutoff
/// 落在未来（同样等于全清）。两格都先于开库被挡下。
pub fn prune(root: &Root, older_than_days: i64, now_ms: i64) -> Result<PruneOut, ReviewError> {
    if older_than_days <= 0 {
        return Err(ReviewError::Usage(FAULT_REVIEW_PRUNE_DAYS));
    }
    let cutoff = now_ms.saturating_sub(older_than_days.saturating_mul(DAY_MS));
    let pruned = queue::prune(root.memory(), cutoff, now_ms).map_err(|_| store_err())?;
    Ok(PruneOut {
        pruned,
        older_than_days,
    })
}

// ── `hub review run`（S3b-4a）─────────────────────────────────────────────────

/// `review run` 的读数。字段与文本行逐项同名（`--json` 是同一组字段的 `Serialize`）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct RunOut {
    /// 本轮实际碰过的 item 数（含失败的那一条：quota 停下的那一轮也计 1）。
    /// H1a 起只数 `claim` 成功的那几条：被别的 run 占住的走 `skipped`。
    pub processed: usize,
    pub applied: usize,
    pub rejected: usize,
    pub quarantined: usize,
    /// H1a：`claim` 没抢到而跳过的条数（另一个 run 正在处理它；不是错误）。
    pub skipped: usize,
    /// H1a：引擎报 `RunError::Conflict` 的条数（另一写者已推进状态/非法转换）；
    /// 这些条目不记退避、不改 ledger。
    pub conflicts: usize,
    pub errors: usize,
    /// 本轮因 provider 额度耗尽**提前停下**。
    pub exhausted: bool,
    pub remaining_pending_review: i64,
    pub remaining_quarantined: i64,
    /// H2：`remaining` 的第三格。`approved` 不是终态（还要补写），以前三格只报两格，
    /// `approved` 积压与"approved 上判了缺口"都读不出来。
    pub remaining_approved: i64,
    /// 观测面（W33-A10 · 评审缺陷②补线，只追加在最后）：这一发里最慢一发真实出网的
    /// 耗时（单调钟毫秒）。写口在 `Live`、读口在账本（`Metered`）；回放座位没有真实
    /// 出网 ⇒ 恒 0（stub 不发明耗时）；全 approved 不装配座位的那一支同样恒 0。
    pub max_call_ms: i64,
    /// 观测面第二格：这一发里 `LlmFailure::Timeout` 的计数。
    pub timeouts: u32,
    /// 波三修 2：队列里**可运行但引擎吃不动**的人工项条数（`report`/`reuse`/`time` 等）。
    /// 它们不被选中、不占位、不记 `errors`；这一格是"跳过了几条"，不是失败。
    /// `--pass` 显式指定时恒 0（那一支的域本来就是单一的）。
    pub skipped_unsupported: usize,
}

/// 一次 run 结束后"还剩多少要处理"，**按 `--pass` 域**取：[`RunOut`] 的三格 remaining
/// 与 `main` 的退出码都只该看这一条命令负责的那部分。
///
/// - `pass=None` ⇒ 引擎闭集（[`ENGINE_PASSES`]）的三态和（人工来源 `report`/`reuse` 不计入）；
/// - `pass=Some(p)` ⇒ `stats.by_pass` 里那一组（6 格顺序见 [`ReviewStats`]：
///   pending/approved/applied/rejected/quarantined/stale）；库里没有该 pass 的 item ⇒ 全 0。
///
/// 跨 pass 不串：`--pass fact` 收完 fact 后，naming 的遗留不许把退出码抬到 4。
pub fn remaining_for(stats: &ReviewStats, pass: Option<&str>) -> (i64, i64, i64) {
    match pass {
        None => {
            // 波三修 2：无 `--pass` 的域是**引擎闭集**（[`ENGINE_PASSES`]）——人工来源
            // （`report`/`reuse`）不参与"还剩多少要跑"。修前这里读全库总数，队列里每有
            // 一条人工项，`review run` 的退出码就永远是 4（它永远跑不完）。
            let mut out = (0i64, 0i64, 0i64);
            for (pass_id, c) in &stats.by_pass {
                if ENGINE_PASSES.contains(&pass_id.as_str()) {
                    out.0 += c[0]; // pending_review
                    out.1 += c[4]; // quarantined
                    out.2 += c[1]; // approved
                }
            }
            out
        }
        Some(wanted) => stats
            .by_pass
            .iter()
            .find(|(pass_id, _)| pass_id == wanted)
            .map(|(_, c)| (c[0], c[4], c[1]))
            .unwrap_or((0, 0, 0)),
    }
}

/// `approved` 那条路不调模型（判决从台账重建、写库），所以"全是 approved"时调用方不该
/// 要求 LLM 来源。真的被问到（装配漏了）⇒ `BackendError`：按 LLM 故障退避并计 error，
/// 不静默放过一条没审过的 item。
struct NoLlm;

impl LlmBackend for NoLlm {
    fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
        Err(LlmFailure::BackendError)
    }
}

fn check_pass(pass: Option<&str>) -> Result<(), ReviewError> {
    if let Some(p) = pass {
        // 刀 B 不动这一格：`review run` 只跑引擎认得的 pass；`reuse`（与 `report` 同族）
        // 只走人工判定链，不许被人用 `--pass reuse` 送进窄审。
        // 波三修 1/2：`lesson` 与其余抽取 pass 同一闭集——引擎的 `supported_pass` 认它，
        // 只是它的终态行由 `pass_lesson::run_lesson` 自己写。
        if !matches!(p, "naming" | "fact" | "event" | "time" | "rows" | "state" | "lesson") {
            return Err(ReviewError::Usage(FAULT_PASS));
        }
    }
    Ok(())
}

/// 引擎（`rc_distill::review::run::supported_pass`）真正能跑的 pass 闭集
/// （波三修 2）。`hub review run` 的**无 `--pass`** 那一支只在这里选批：
///
/// - `report`/`reuse` 是消费者反馈（`review::check_item` 收、引擎不认），只走人工判定链
///   `hub review decide`——以前它们被选进来、逐条记 `Step::Error`，把退出码抬到 3；
/// - `time` 没有审核步骤（`supported_pass` 没有它），也不在这份闭集里。
///
/// 这是"引擎分派真实闭集"的**唯一**一份字面：改 `supported_pass` 时这里同改
/// （`remaining_without_a_pass_ignores_manual_sources` 那条用例钉住人工来源不进这一域）。
pub const ENGINE_PASSES: [&str; 6] = ["naming", "fact", "event", "rows", "state", "lesson"];

/// 本次 `run` 是否真的需要 LLM 来源：可运行项里出现 `pending_review`/`quarantined` 才需要
/// （`approved` 的判决从台账重建，不问模型）。`main` 用它在装配与退出 3 之间做选择。
///
/// 与 [`run`] 读的是同一条选批（同 `pass`/`limit`/`now_ms`、同 [`ENGINE_PASSES`] 域），
/// 所以两边的判断不会错位。读它需要开库：这一条命令的作业本来就是队列，来源的"惰性"
/// 指的是**不装配**（不读 `--replies`/`llm.toml`），不是不开库。
pub fn needs_llm(
    root: &Root,
    pass: Option<&str>,
    limit: usize,
    now_ms: i64,
) -> Result<bool, ReviewError> {
    check_pass(pass)?;
    let items = selectable(root.memory(), pass, now_ms, limit).map_err(|_| store_err())?;
    Ok(items.iter().any(|meta| meta.state != ReviewState::Approved))
}

/// 这条命令该选的那一批：显式 `--pass` 只选那一个（`check_pass` 已挡住人工来源）；
/// 无 `--pass` 只选引擎认的 pass（[`ENGINE_PASSES`]）——`report`/`reuse` 留在原状态等
/// 人工判定，见 [`run`] 的读数 `skipped_unsupported`。
fn selectable(
    m: &MemoryHandle,
    pass: Option<&str>,
    now_ms: i64,
    limit: usize,
) -> Result<Vec<ItemMeta>, rc_memory::MemError> {
    match pass {
        Some(p) => queue::runnable(m, Some(p), now_ms, limit),
        None => queue::runnable_in(m, &ENGINE_PASSES, now_ms, limit),
    }
}

/// 引擎失败的 reason（`extract_pass_state.reason`）：fact/event 用各自 parser 的
/// `review_reason()`（带 `review_` 前缀，与 runner 逐字同源）；其余变体是接线错误，
/// 按隔离收尾，不把内核错误伪装成判决。
fn review_reason_of(e: &DistillError) -> String {
    match e {
        DistillError::Fact(err) => err.review_reason(),
        DistillError::Event(err) => err.review_reason(),
        _ => rc_distill::review::REVIEW_UNCERTAIN.to_string(),
    }
}

/// `hub review run`：按 [`queue::runnable`] 顺序逐条重跑引擎（`quarantined` 也复审），
/// 失败按 60s × 4^n 退避，额度耗尽即停（设计 §4/§9/§11 的 S3b 段）。
///
/// H1a：每条先 [`queue::claim`] 原子占位，抢不到就跳过（另一个 run 正在跑它）；
/// 不论走哪条分支，出口都 [`queue::release`]。`lease_ms` 由 `main` 给
/// （`llm_timeout_ms + 2min`）：租约只覆盖"一条 item 的一次处理"，进程崩掉后它自己过期。
///
/// 退出码由调用方按读数决定：`errors>0 ⇒ 3`；否则三格 remaining
/// （`pending_review + quarantined + approved`）任一大于 0 ⇒ 4；否则 0。
pub fn run(
    root: &Root,
    pass: Option<&str>,
    limit: usize,
    backend: Option<&dyn LlmBackend>,
    now_ms: i64,
    lease_ms: i64,
) -> Result<RunOut, ReviewError> {
    check_pass(pass)?;
    let m = root.memory();
    // 波三修 2：无 `--pass` 时只选引擎认的 pass（[`ENGINE_PASSES`]）；人工来源
    // （`report`/`reuse`）留在原状态等 `hub review decide`，条数进 `skipped_unsupported`
    // 读数——不占位、不记 `errors`、不改退出码。
    let items = selectable(m, pass, now_ms, limit).map_err(|_| store_err())?;
    let mut out = RunOut::default();
    if pass.is_none() {
        out.skipped_unsupported = queue::runnable(m, None, now_ms, 0)
            .map_err(|_| store_err())?
            .iter()
            .filter(|meta| !ENGINE_PASSES.contains(&meta.pass_id.as_str()))
            .count();
    }
    let no_llm = NoLlm;
    // owner 带 pid：同一台机器上两个 run 的占位不会互相覆盖。
    let owner = format!("hub-run-{}-{now_ms}", std::process::id());
    for meta in items {
        // 原子占位：收到 false 就跳过——不计数、不报错（另一个 run 正在处理它）。
        match queue::claim(m, meta.id, &owner, lease_ms, now_ms) {
            Ok(true) => {}
            Ok(false) => {
                out.skipped += 1;
                continue;
            }
            Err(_) => return Err(store_err()),
        }
        out.processed += 1;
        let step = run_claimed(root, &meta, backend.unwrap_or(&no_llm), now_ms);
        // 所有路径（成功/失败/下面要 break 的）都先放位；release 失败不盖住原错误，
        // 但计入 errors（位没放掉会让下一条 run 白等到租约过期）。
        if queue::release(m, meta.id, &owner).is_err() {
            out.errors += 1;
        }
        let step = match step {
            Ok(step) => step,
            Err(_) => return Err(store_err()),
        };
        let mut stop = false;
        match step {
            Step::State(ReviewState::Applied) => out.applied += 1,
            Step::State(ReviewState::Rejected) => out.rejected += 1,
            Step::State(ReviewState::Quarantined) => out.quarantined += 1,
            // approved/pending 是"还没走完"，不是本轮的成功或失败。
            Step::State(_) => {}
            // 另一写者已推进/非法转换：不 note_attempt、不改 ledger、继续下一条。
            Step::Conflict => out.conflicts += 1,
            Step::Llm(_failure, quota) => {
                out.errors += 1;
                if quota {
                    out.exhausted = true;
                    stop = true;
                }
            }
            Step::Quarantined => out.quarantined += 1,
            Step::Error => out.errors += 1,
            // 存储/事务失败：不再写 `rejected(store)`，ledger 保持原状；本轮立即停。
            Step::StopError => {
                out.errors += 1;
                stop = true;
            }
        }
        if stop {
            break;
        }
    }
    let stats = queue::stats(m, now_ms).map_err(|_| store_err())?;
    let (pending, quarantined, approved) = remaining_for(&stats, pass);
    out.remaining_pending_review = pending;
    out.remaining_quarantined = quarantined;
    out.remaining_approved = approved;
    Ok(out)
}

/// 一条已占位的 item 跑一次的读数。`Store` 与 `Review` 分支里 `mark_*` 的写失败统一折成
/// `StopError`（立即停，避免级联）；`mark_turns` 的失败例外：H1a 起只计 error 并继续
/// 下一条（不覆盖 ledger、不 break）。
enum Step {
    State(ReviewState),
    Conflict,
    /// 第二个 bool = 额度耗尽（停下）。
    Llm(LlmFailure, bool),
    Quarantined,
    Error,
    StopError,
}

fn run_claimed(
    root: &Root,
    meta: &ItemMeta,
    engine: &dyn LlmBackend,
    now_ms: i64,
) -> Result<Step, ReviewError> {
    let m = root.memory();
    let item = match queue::get(m, meta.id) {
        Ok(Some(item)) => item,
        // runnable → get 之间被清掉：记为 error，继续（不是本轮的业务失败）。
        Ok(None) => return Ok(Step::Error),
        Err(_) => return Err(store_err()),
    };
    match run_item_with_policy(
        m,
        root.transcript(),
        root.ledger(),
        item.meta.id,
        engine,
        now_ms,
        RetryPolicy::Quarantined,
    ) {
        Ok(outcome) => {
            // 逐轮终态先落：它读的是这份 outcome 与载荷，不改 item 状态。
            if mark_turns(root.ledger(), &item, &outcome, now_ms).is_err() {
                return Ok(Step::Error);
            }
            // S4：applied 的边界就在这里——写库那一步已经在引擎里发生（`record_apply`
            // 之后才回到调用方）。提升的动作与口径都不在 hub 里抄一遍：四个 apply 入口
            // （本处、fact/event 的内联窄审、`hub review retry-uncertain`）共用
            // `rc_distill::review::promote_applied_cards`，它只认 `item_state == Applied`
            // 并逐条提升 `outcome.card_ids`（本次真的写下的卡）。hub 不解析 `payload_json`、
            // 也不重算卡的身份；推导卡（`mode='inferred'`）从不进那个集合。
            // 错误的收尾在 hub 侧仍是"停本轮"：与 `mark_*` 的写失败同一条尺子（card 留
            // `unreviewed` 由 audit 兜底，不吞、不级联）。pass runner 那边只能吞，因为
            // 那里上抛会让已 apply 的批被重铺成 pending——两处的差别见 helper 的文档。
            if rc_distill::review::promote_applied_cards(m, &outcome, now_ms).is_err() {
                return Ok(Step::StopError);
            }
            Ok(Step::State(outcome.item_state))
        }
        Err(RunError::Llm(failure)) => {
            // 退避 = 60s × 4^min(attempt,4)；`note_attempt` 的失败不掩盖 LLM 故障
            // （item 仍是可运行态，下一次照样会被选中）。
            let exponent = item.meta.attempt.clamp(0, 4) as u32;
            let backoff = RETRY_BASE_MS.saturating_mul(4_i64.saturating_pow(exponent));
            let _ = queue::note_attempt(m, item.meta.id, backoff, now_ms);
            Ok(Step::Llm(failure, failure == LlmFailure::QuotaExhausted))
        }
        Err(RunError::Review(e)) => {
            // 引擎已记 `uncertain(<reason>)` 并落了 artifact；逐轮铺同一个 reason。
            let reason = review_reason_of(&e);
            if mark_batch(root.ledger(), &item, PassState::Quarantined, Some(&reason), now_ms)
                .is_err()
            {
                return Ok(Step::StopError);
            }
            Ok(Step::Quarantined)
        }
        Err(RunError::ReviewNaming(slug)) => {
            // 与 naming runner 同格：reason 就是窄审 parser 的 slug。
            if mark_batch(root.ledger(), &item, PassState::Quarantined, Some(&slug), now_ms)
                .is_err()
            {
                return Ok(Step::StopError);
            }
            Ok(Step::Quarantined)
        }
        // S3c-3b：rows 的隔离码（parser slug 或 `review_ref_missing`）原样铺给调用方。
        // rows 不写 `extract_pass_state`——`mark_batch` 对它是 no-op（同一个入口、同一句
        // reason），这里保留调用形状：各 pass 的收尾只差"状态怎么记"这一处。
        Err(RunError::ReviewRows(slug)) => {
            if mark_batch(root.ledger(), &item, PassState::Quarantined, Some(&slug), now_ms)
                .is_err()
            {
                return Ok(Step::StopError);
            }
            Ok(Step::Quarantined)
        }
        // 波三刀 A：lesson 的审核 parser 拒收。与 rows 同格——引擎已记
        // `uncertain(review_<slug>)` 并落了 artifact，这里把同一个串铺给调用方。
        // `mark_batch` 对 lesson 是 no-op（它不写 `extract_pass_state`），保留调用形状。
        Err(RunError::ReviewLesson(e)) => {
            let reason = e.review_reason();
            if mark_batch(root.ledger(), &item, PassState::Quarantined, Some(&reason), now_ms)
                .is_err()
            {
                return Ok(Step::StopError);
            }
            Ok(Step::Quarantined)
        }
        // H1a：另一写者已推进（"没有可写的终态"），ledger 保持原状。
        Err(RunError::Conflict) => Ok(Step::Conflict),        // 存储/事务失败：**不**把批内轮写成 `rejected(store)`——那会覆盖已写好的 ledger
        // 终态（P0-1）。只计 error 并停，ledger 保持原状。
        Err(RunError::Store(_)) => Ok(Step::StopError),
        // 状态不对/找不到 item：没有可写的终态，记为 error 并继续。
        Err(RunError::Unsupported | RunError::MissingItem) => Ok(Step::Error),
    }
}

// ── 两形输出（文本 / JSON）。main 只负责 write_line。 ─────────────────────────

fn to_json<T: Serialize>(value: &T) -> Result<String, ReviewError> {
    serde_json::to_string(value).map_err(|_| ReviewError::Store)
}

fn none_or(v: &Option<String>) -> &str {
    v.as_deref().unwrap_or("none")
}

fn dash_if_empty(s: &str) -> &str {
    if s.is_empty() {
        "-"
    } else {
        s
    }
}

fn opt_ms(v: Option<i64>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "none".to_string())
}

pub fn render_list(items: &[ItemMetaOut], json: bool) -> Result<String, ReviewError> {
    if json {
        return to_json(&items);
    }
    if items.is_empty() {
        return Ok("no review items".to_string());
    }
    let lines: Vec<String> = items
        .iter()
        .map(|m| {
            format!(
                "{} {} {}/{} {} {}",
                m.id,
                m.state,
                m.pass_id,
                dash_if_empty(&m.family),
                dash_if_empty(&m.session_key),
                m.created_at_ms
            )
        })
        .collect();
    Ok(lines.join("\n"))
}

/// 文本形不打印指纹列（`input_sha256` 与 H4b 的 `review_input_sha256`）：台账里的两枚
/// 64 位 hex 只在 `--json` 出，文本行保持"id/stage/verdict/码/版本/模型/时间"的既有形状。
/// 只给新列加一行 `review_input=` 会让文本面对两枚同类指纹一显一隐，读起来像缺列。
pub fn render_show(doc: &ShowOut, with_payload: bool, json: bool) -> Result<String, ReviewError> {
    if json {
        return to_json(doc);
    }
    let m = &doc.item;
    let mut out = format!(
        "id={} state={} source={} pass={} family={} session={} device={} attempt={} \
         created_ms={} updated_ms={} applied_ms={} program_code={}",
        m.id,
        m.state,
        m.source,
        m.pass_id,
        dash_if_empty(&m.family),
        dash_if_empty(&m.session_key),
        dash_if_empty(&m.device_id),
        m.attempt,
        m.created_at_ms,
        m.updated_at_ms,
        opt_ms(m.applied_at_ms),
        none_or(&m.program_code),
    );
    if doc.decisions.is_empty() {
        out.push_str("\ndecisions: none");
    } else {
        for d in &doc.decisions {
            out.push_str(&format!(
                "\ndecision id={} stage={} verdict={} reason_code={} rules_version={} \
                 model={} prompt_version={} decider={} created_ms={}",
                d.id,
                d.stage,
                d.verdict,
                d.reason_code,
                d.rules_version,
                dash_if_empty(&d.model),
                dash_if_empty(&d.prompt_version),
                d.decider,
                d.created_at_ms,
            ));
        }
    }
    if with_payload {
        out.push_str(&format!(
            "\npayload_json={}",
            doc.payload_json.as_deref().unwrap_or("")
        ));
    }
    Ok(out)
}

pub fn render_decide(id: i64, state: &str, json: bool) -> Result<String, ReviewError> {
    let doc = DecideOut {
        id,
        state: state.to_string(),
    };
    if json {
        return to_json(&doc);
    }
    Ok(format!("id={} state={}", doc.id, doc.state))
}

/// `review prune` 的两形输出：一行读数（`--json` 是同一组字段）。
pub fn render_prune(out: &PruneOut, json: bool) -> Result<String, ReviewError> {
    if json {
        return to_json(out);
    }
    Ok(format!(
        "pruned={} older_than_days={}",
        out.pruned, out.older_than_days
    ))
}

fn counts_line(s: &StatsOut) -> String {
    format!(
        "pending_review={} approved={} applied={} rejected={} quarantined={} stale={} \
         oldest_pending_age_ms={} oldest_pending_over_7d={}",
        s.pending_review,
        s.approved,
        s.applied,
        s.rejected,
        s.quarantined,
        s.stale,
        opt_ms(s.oldest_pending_age_ms),
        s.oldest_pending_over_7d,
    )
}

pub fn render_stats(s: &StatsOut, json: bool) -> Result<String, ReviewError> {
    if json {
        return to_json(s);
    }
    let mut out = counts_line(s);
    for p in &s.by_pass {
        out.push_str(&format!(
            "\npass {}: pending_review={} approved={} applied={} rejected={} quarantined={} stale={}",
            p.pass_id,
            p.pending_review,
            p.approved,
            p.applied,
            p.rejected,
            p.quarantined,
            p.stale,
        ));
    }
    Ok(out)
}

/// 波二刀 B：`hub review reuse-ingest` 的逐卡读数（`--json` 的元素）。
#[derive(Debug, Clone, Serialize)]
pub struct ReuseIngestItem {
    /// `card:<hex>`（与工具面句柄同形）。
    pub card_id: String,
    pub review_item_id: i64,
    /// true = 本次新建；false = 命中去重（在途项或同载荷）。
    pub fresh: bool,
}

/// 波二刀 B：`hub review reuse-ingest` 的读数（`--json` 是同一组字段）。
#[derive(Debug, Clone, Serialize)]
pub struct ReuseIngestOut {
    pub candidates: usize,
    pub queued: usize,
    pub deduped: usize,
    pub items: Vec<ReuseIngestItem>,
}

/// 波二刀 B（设计 §波次二第 3 条审计臂）：把复用反馈的 `failed`/`mismatch` 入复核队列。
///
/// 全部判定与写入在 `rc_memory::reuse::ingest_audit`（程序判定 = 档位闭集 + 卡还在；
/// 查重 = 在途项；载荷 = 从卡的 provenance/device 取，与 `report` 先例同形）。这一层只做
/// **水位镜像**与两形输出——与 `report` 臂在 mcp 那一侧的分工逐字同律：
/// enqueue 之后补写 `pass_id='reuse'` 的审查轴水位，否则 pending 期间实表 +1、水位 +0
/// ⇒ doctor 的「水位和==实表」误报、`hub pending` 漏数
/// （`feedback-review-item-ingress-needs-watermark` 的教训）。写失败不改变这次入队的结果
/// （水位是读数，best-effort，与 `decide` 同一支）。
///
/// 这类 item **不进窄审**：引擎的 `supported_pass` 不认 `reuse`（照 `report` 先例，
/// 一个字没动），只走人工判定链（`hub review decide`）。
pub fn reuse_ingest(root: &Root, now_ms: i64) -> Result<ReuseIngestOut, ReviewError> {
    let out = rc_memory::reuse::ingest_audit(root.memory(), now_ms).map_err(|_| store_err())?;
    // 候选为 0 时不写水位：没有 ingress 就没有"虚挂"可镜像，也不给一张空 pass 造一行水位。
    if out.candidates > 0 {
        let _ = write_watermark(root.memory(), root.ledger(), "reuse", now_ms);
    }
    Ok(ReuseIngestOut {
        candidates: out.candidates,
        queued: out.queued,
        deduped: out.deduped,
        items: out
            .items
            .into_iter()
            .map(|i| ReuseIngestItem {
                card_id: i.card_id,
                review_item_id: i.review_item_id,
                fresh: i.fresh,
            })
            .collect(),
    })
}

/// `review reuse-ingest` 的两形输出：一行读数 + 逐卡一行（`--json` 是同一组字段的数组）。
pub fn render_reuse_ingest(out: &ReuseIngestOut, json: bool) -> Result<String, ReviewError> {
    if json {
        return to_json(out);
    }
    let mut text = format!(
        "reuse-ingest candidates={} queued={} deduped={}",
        out.candidates, out.queued, out.deduped
    );
    for item in &out.items {
        text.push_str(&format!(
            "\n{} review_item={} {}",
            item.card_id,
            item.review_item_id,
            if item.fresh { "new" } else { "deduped" }
        ));
    }
    Ok(text)
}

/// `review run` 的两形输出：一行读数（`--json` 是同一组字段）。
pub fn render_run(out: &RunOut, json: bool) -> Result<String, ReviewError> {
    if json {
        return to_json(out);
    }
    Ok(format!(
        "run processed={} applied={} rejected={} quarantined={} skipped={} conflicts={} \
         errors={} exhausted={} skipped_unsupported={} remaining_pending_review={} \
         remaining_quarantined={} remaining_approved={}",
        out.processed,
        out.applied,
        out.rejected,
        out.quarantined,
        out.skipped,
        out.conflicts,
        out.errors,
        out.exhausted,
        out.skipped_unsupported,
        out.remaining_pending_review,
        out.remaining_quarantined,
        out.remaining_approved,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 总数刻意不等于 `by_pass` 的和：`pass=None` 必须读总数那一组，
    /// 拿 `by_pass` 求和冒充总数会在第一条断言上红。
    fn fixture() -> ReviewStats {
        ReviewStats {
            pending_review: 9,
            approved: 4,
            applied: 2,
            rejected: 1,
            quarantined: 1,
            stale: 0,
            oldest_pending_age_ms: Some(1_000),
            by_pass: vec![
                ("fact".to_string(), [2, 1, 0, 0, 1, 0]),
                ("naming".to_string(), [3, 3, 0, 0, 0, 0]),
            ],
        }
    }

    /// `pass=None` ⇒ 引擎闭集（[`ENGINE_PASSES`]）的三态和。
    ///
    /// 波三修 2 改了口径：原名说"总数"，但人工来源（`report`/`reuse`）进总数后
    /// `review run` 永远跑不完（退出 4）——这个测试名按"历史锚点不改名"的纪律保留，
    /// 断言改读引擎闭集；夹具的 totals(9/1/4) 与 by_pass(fact+naming=5/1/4) 刻意不同，
    /// 正好把"读没读闭集"分开。
    #[test]
    fn remaining_without_a_pass_reads_the_totals() {
        assert_eq!(remaining_for(&fixture(), None), (5, 1, 4));
    }

    /// 波三修 2：人工来源的 `by_pass` 组不参与无 `--pass` 的剩余计数。
    #[test]
    fn remaining_without_a_pass_ignores_manual_sources() {
        let mut stats = fixture();
        stats.by_pass.push(("reuse".to_string(), [7, 0, 0, 0, 0, 0]));
        stats.by_pass.push(("report".to_string(), [2, 0, 0, 0, 1, 0]));
        assert_eq!(
            remaining_for(&stats, None),
            (5, 1, 4),
            "reuse/report 的待审项不进引擎剩余"
        );
    }

    /// 按 `--pass` 取该 pass 的三格；`by_pass` 的 6 格顺序是
    /// pending/approved/applied/rejected/quarantined/stale。
    #[test]
    fn remaining_for_a_pass_reads_that_group_only() {
        let stats = fixture();
        assert_eq!(remaining_for(&stats, Some("fact")), (2, 1, 1));
        assert_eq!(remaining_for(&stats, Some("naming")), (3, 0, 3));
        // 跨 pass 不串：naming 的 3 条 pending 不许算进 fact 的剩余。
        assert_ne!(
            remaining_for(&stats, Some("fact")).0,
            stats.pending_review,
            "fact 的剩余必须只来自 fact 那一组"
        );
    }

    /// 库里没有该 pass 的 item ⇒ 全 0（不是"回落成总数"）。
    #[test]
    fn remaining_for_a_missing_pass_is_all_zero() {
        assert_eq!(remaining_for(&fixture(), Some("time")), (0, 0, 0));
        let empty = ReviewStats::default();
        assert_eq!(remaining_for(&empty, None), (0, 0, 0));
        assert_eq!(remaining_for(&empty, Some("fact")), (0, 0, 0));
    }

    /// H7a：7 天告警是**严格大于**——正好 7 天不报，多 1ms 报；没有 pending ⇒ false。
    #[test]
    fn pending_alert_is_strictly_over_seven_days() {
        assert!(!oldest_pending_over_7d(None));
        assert!(!oldest_pending_over_7d(Some(0)));
        assert!(
            !oldest_pending_over_7d(Some(PENDING_ALERT_MS)),
            "正好 7 天不告警"
        );
        assert!(oldest_pending_over_7d(Some(PENDING_ALERT_MS + 1)));
    }

    /// H7a：`prune` 的两形输出——文本读数与 JSON 同字段。
    #[test]
    fn prune_renders_the_window_and_the_count() {
        let out = PruneOut {
            pruned: 3,
            older_than_days: 7,
        };
        assert_eq!(
            render_prune(&out, false).unwrap(),
            "pruned=3 older_than_days=7"
        );
        assert_eq!(
            render_prune(&out, true).unwrap(),
            r#"{"pruned":3,"older_than_days":7}"#
        );
    }
}
