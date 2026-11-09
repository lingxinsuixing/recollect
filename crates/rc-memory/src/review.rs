//! S3a-1：双审队列的存储面（设计 `2026-10-26-review-queue-s3` §3/§4/§10）。
//!
//! 这是 `review_item` / `review_decision` 的**唯一写入点**。载荷与判定都是 opaque 字符串：
//! 本层不解析 JSON、不引 `serde`，只做键/长度校验、内容指纹、状态机与事务。
//! 所有 SQL 参数化；错误一律 `MemError::Sql` 的静态短语（守卫 12：不带正文/路径）。
//!
//! 状态机（§4）的合法边只有这些——
//! - `program`：`pending_review + pass` ⇒ 留在 `pending_review`；任意非终态 + `reject` ⇒ `rejected`；
//!   **H2：`pending_review|approved + uncertain` ⇒ `quarantined`**——程序侧发现确定性缺口
//!   （载荷不可重放、判决重建不出）时把待审项推进隔离，不再静默留在原状态；
//! - `review`：从 `pending_review` 出发，**S3b-4a 起也允许从 `quarantined` 复审**
//!   （`hub review run` 的重跑路径），pass/reject/uncertain ⇒ approved/rejected/quarantined；
//! - `apply`：只从 `approved` 出发，pass ⇒ `applied`；reject ⇒ 留在 `approved` 并退避；
//! - `human`：`pending_review|quarantined|approved` 且 `decider=human`，pass/reject ⇒ approved/rejected。
//!
//! 其它组合返回 `MemError::Sql("review: illegal transition …")`，不静默。

use crate::MemError;
use rc_store::MemoryHandle;
use rusqlite::{OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};

/// 载荷上限（字符），与 `extract_pass_artifact` 同一档（设计 §3.1）。
pub const MAX_PAYLOAD_CHARS: usize = 64_000;
/// `detail` 上限（字符）：只放码/计数，禁正文（设计 §3.2）。
pub const MAX_DETAIL_CHARS: usize = 512;
/// 退避基数：60s × 4^n（设计 §4，与 S2 同一组数）。
pub const RETRY_BASE_MS: i64 = 60_000;
/// 达到这个次数后由调用方改判 `quarantined`；本层只做算术（设计 §4）。
pub const MAX_ATTEMPTS: i64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReviewState {
    PendingReview,
    Approved,
    Applied,
    Rejected,
    Quarantined,
    Stale,
}

impl ReviewState {
    pub fn slug(self) -> &'static str {
        match self {
            ReviewState::PendingReview => "pending_review",
            ReviewState::Approved => "approved",
            ReviewState::Applied => "applied",
            ReviewState::Rejected => "rejected",
            ReviewState::Quarantined => "quarantined",
            ReviewState::Stale => "stale",
        }
    }

    pub fn parse(slug: &str) -> Option<Self> {
        match slug {
            "pending_review" => Some(ReviewState::PendingReview),
            "approved" => Some(ReviewState::Approved),
            "applied" => Some(ReviewState::Applied),
            "rejected" => Some(ReviewState::Rejected),
            "quarantined" => Some(ReviewState::Quarantined),
            "stale" => Some(ReviewState::Stale),
            _ => None,
        }
    }

    /// 终态：`applied` / `rejected` / `stale`（设计 §3.1）。`approved` 不是终态
    /// （apply 失败要留在原地等重试），`quarantined` 可被人工重新打开。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            ReviewState::Applied | ReviewState::Rejected | ReviewState::Stale
        )
    }

    /// 统计数组的固定下标（`ReviewStats.by_pass` 的 6 格顺序）。
    fn index(self) -> usize {
        match self {
            ReviewState::PendingReview => 0,
            ReviewState::Approved => 1,
            ReviewState::Applied => 2,
            ReviewState::Rejected => 3,
            ReviewState::Quarantined => 4,
            ReviewState::Stale => 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReviewStage {
    Program,
    Review,
    Apply,
    Human,
}

impl ReviewStage {
    pub fn slug(self) -> &'static str {
        match self {
            ReviewStage::Program => "program",
            ReviewStage::Review => "review",
            ReviewStage::Apply => "apply",
            ReviewStage::Human => "human",
        }
    }

    pub fn parse(slug: &str) -> Option<Self> {
        match slug {
            "program" => Some(ReviewStage::Program),
            "review" => Some(ReviewStage::Review),
            "apply" => Some(ReviewStage::Apply),
            "human" => Some(ReviewStage::Human),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReviewVerdict {
    Pass,
    Reject,
    Uncertain,
}

impl ReviewVerdict {
    pub fn slug(self) -> &'static str {
        match self {
            ReviewVerdict::Pass => "pass",
            ReviewVerdict::Reject => "reject",
            ReviewVerdict::Uncertain => "uncertain",
        }
    }

    pub fn parse(slug: &str) -> Option<Self> {
        match slug {
            "pass" => Some(ReviewVerdict::Pass),
            "reject" => Some(ReviewVerdict::Reject),
            "uncertain" => Some(ReviewVerdict::Uncertain),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Decider {
    Program,
    Center,
    Human,
}

impl Decider {
    pub fn slug(self) -> &'static str {
        match self {
            Decider::Program => "program",
            Decider::Center => "center",
            Decider::Human => "human",
        }
    }

    pub fn parse(slug: &str) -> Option<Self> {
        match slug {
            "program" => Some(Decider::Program),
            "center" => Some(Decider::Center),
            "human" => Some(Decider::Human),
            _ => None,
        }
    }
}

/// 一条待入队的载荷。`payload_sha256` 由存储层现算，调用方不传。
#[derive(Debug, Clone)]
pub struct NewItem {
    pub source: String,
    pub pass_id: String,
    pub family: String,
    pub session_key: String,
    pub device_id: String,
    pub turn_uids_json: String,
    pub owner_json: String,
    pub payload_json: String,
    /// H1c：批次身份（[`batch_key`] 的返回值），与输出内容无关。空串 = 调用方未提供
    /// （旧调用方/旧行）⇒ 不参与顶替，只走载荷去重。
    pub batch_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnqueueOutcome {
    pub id: i64,
    pub payload_sha256: String,
    /// true = 本次新建；false = 命中唯一索引，返回既有行。
    pub fresh: bool,
    pub state: ReviewState,
}

/// 一条判定。`detail` 只放码/计数，禁正文（≤512 字符）。
///
/// H4b：两个指纹分列，语义固定——
/// - `input_sha256` 恒为**载荷指纹**（`review_item.payload_sha256`，三个 stage 同格）；
/// - `review_input_sha256` 只在**真正送审**的 `stage=review` 行放**送审输入指纹**
///   （`extraction_fingerprint(pipeline, model, system, user)`）；program/apply/human 行、
///   以及程序侧的 review 裁定（载荷缺口、`program_only`）一律空串。
///
/// H4b 之前 `review_decision.input_sha256` 一格装两种哈希（program/apply=载荷、review=prompt
/// 指纹），下游无法判断一格是"哪一份输入"的指纹；拆开后任一列都可独立复核。
#[derive(Debug, Clone)]
pub struct Decision {
    pub stage: ReviewStage,
    pub verdict: ReviewVerdict,
    pub reason_code: String,
    pub rules_version: String,
    pub model: String,
    pub prompt_version: String,
    pub input_sha256: String,
    pub review_input_sha256: String,
    pub decider: Decider,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemMeta {
    pub id: i64,
    pub source: String,
    pub pass_id: String,
    pub family: String,
    pub session_key: String,
    pub device_id: String,
    pub state: ReviewState,
    pub program_code: Option<String>,
    pub attempt: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub applied_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub meta: ItemMeta,
    pub turn_uids_json: String,
    pub owner_json: String,
    pub payload_json: String,
    pub payload_sha256: String,
    pub conflict_json: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewStats {
    pub pending_review: i64,
    pub approved: i64,
    pub applied: i64,
    pub rejected: i64,
    pub quarantined: i64,
    pub stale: i64,
    /// 最老一条 `pending_review` 的年龄；没有待审项时为 `None`。
    pub oldest_pending_age_ms: Option<i64>,
    /// 按 `pass_id` 分组，6 格顺序 = pending_review/approved/applied/rejected/quarantined/stale。
    pub by_pass: Vec<(String, [i64; 6])>,
}

/// 查询列序；`meta_of` 的索引与它逐位对应。
const META_COLUMNS: &str = "id, source, pass_id, family, session_key, device_id, state, \
                            program_code, attempt, created_at_ms, updated_at_ms, applied_at_ms";

fn invalid(what: &str) -> MemError {
    MemError::Sql(format!("review: invalid {what}"))
}

fn parse_state(slug: &str) -> Result<ReviewState, MemError> {
    ReviewState::parse(slug)
        .ok_or_else(|| MemError::Sql("review: unknown state in review_item".to_string()))
}

fn illegal(d: &Decision, state: ReviewState) -> MemError {
    MemError::Sql(format!(
        "{} {}/{} from {} (decider={})",
        CONFLICT_PREFIX,
        d.stage.slug(),
        d.verdict.slug(),
        state.slug(),
        d.decider.slug()
    ))
}

/// 队列状态机拒绝的固定前缀。**只在本文件的 [`illegal`] 生产**：`RunError::Conflict`
/// 靠 [`MemError::is_review_conflict`] 认它，两处各拼一套字符串就会漏判。
const CONFLICT_PREFIX: &str = "review: illegal transition";

impl MemError {
    /// 队列状态机拒绝（非法转换/另一写者已把状态推进）。H1a 起 `RunError` 用它把
    /// "竞态或非法转换"从真存储故障里分出来：前者不改 ledger、不记退避。
    pub fn is_review_conflict(&self) -> bool {
        matches!(self, MemError::Sql(msg) if msg.starts_with(CONFLICT_PREFIX))
    }
}

/// 内容指纹：`sha256("review.v1\0" + pass_id + "\0" + family + "\0" + owner_json + "\0" + payload_json)`。
/// 段间 0u8 分隔：`("ab","c")` 与 `("a","bc")` 不许撞。session/device 不进 preimage——
/// 同一载荷换会话仍是同一条待审项。
///
/// **唯一一条公式**：[`enqueue`] 写列、[`payload_verified`] 复核、以及测试里"合法改写载荷后
/// 重新落章"都走这里。调用方不要自行拼 preimage。
pub fn payload_sha256(pass_id: &str, family: &str, owner_json: &str, payload_json: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"review.v1\0");
    for part in [pass_id, family, owner_json] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    h.update(payload_json.as_bytes());
    let mut s = String::with_capacity(64);
    for b in h.finalize() {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 载荷防篡改复核：用 [`enqueue`] 的同一条公式（[`payload_sha256`]）从
/// `meta.pass_id`/`meta.family`/`owner_json`/`payload_json` 现算一遍，与列
/// `payload_sha256` 逐字比对。
///
/// 只读、无 I/O、不看状态：调用方（引擎）在**任何重放/模型调用之前**用它把
/// "列里的指纹"与"这一行现在的载荷"钉在一起。`false` 表示两列已经不自洽——
/// 载荷被改过，或指纹被改过，两者都不许再当审核输入。
///
/// 返回 `Result` 是为了让调用点与其它只读 API 同一条错误形状（预留将来的
/// 格式校验）；当前实现不会失败。
pub fn payload_verified(item: &Item) -> Result<bool, MemError> {
    let recomputed = payload_sha256(
        &item.meta.pass_id,
        &item.meta.family,
        &item.owner_json,
        &item.payload_json,
    );
    Ok(recomputed == item.payload_sha256)
}

/// 批次身份（H1c）：`sha256(pass_id \x1f family \x1f session_key \x1f device_id \x1f
/// sorted(dedup(turn_uids)).join(\x1f))` 的 64 位小写 hex。
///
/// 与模型输出、[`payload_sha256`]、墙钟**都无关**：同一批换模型/换 prompt/输出走样，仍是同一批。
/// 设备、会话、pass/family 都是键的一部分；时间不参与——轮集合已经钉住内容。
/// 轮集合先去重再按字节序排序，`b,a,a` 与 `a,b` 得到同一个键。
/// 空串不是本函数的返回值：它只表示调用方没提供（见 [`NewItem::batch_key`]）。
pub fn batch_key(
    pass_id: &str,
    family: &str,
    session_key: &str,
    device_id: &str,
    turn_uids: &[&str],
) -> String {
    let mut uids: Vec<&str> = turn_uids.to_vec();
    uids.sort_unstable();
    uids.dedup();
    let mut h = Sha256::new();
    for part in [pass_id, family, session_key, device_id] {
        h.update(part.as_bytes());
        h.update([0x1f]);
    }
    h.update(uids.join("\x1f").as_bytes());
    let mut s = String::with_capacity(64);
    for b in h.finalize() {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// rows **专用**的批次身份（S3c-fix）：`sha256("rows" \x1f session \x1f device \x1f
/// sorted(dedup(row ids)).join(\x1f))` 的 64 位小写 hex。
///
/// 与 [`batch_key`] 的分量不同：rows 的 item 是**按行切片**的（hub 先按归属/scope 分组、
/// 组内 `chunks(10)`），同一请求的两片/两组常常共享同一份轮集合（一条长轮抽出多条 fact）。
/// 轮集合因此不能当 rows 的身份，否则后一片入队会把前一片顶成 `stale`，前 ≤10 行静默不入库
/// ——S3c 复审的 P0。行 id 才是"这一片审的是哪几行"，所以它进键。
///
/// 稳定性与 [`batch_key`] 同一条纪律：行集合去重后按字节序排序，`b,a,a` 与 `a,b` 同键
/// （重试同一批因此命中同一条 item）；不同行集合、不同会话/设备 ⇒ 不同键。
/// 空 `row_ids` 不是本函数的返回值：调用方必须给非空集合（
/// `rc_distill::review::rows_payload::rows_new_item` 对空/重复 id 落**空 key**，
/// 让 `enqueue` 的兼容分支不参与顶替，而不是拿一个错的键去顶别人的行）。
pub fn rows_batch_key(session_key: &str, device_id: &str, row_ids: &[String]) -> String {
    let mut ids: Vec<&str> = row_ids.iter().map(String::as_str).collect();
    ids.sort_unstable();
    ids.dedup();
    let mut h = Sha256::new();
    for part in ["rows", session_key, device_id] {
        h.update(part.as_bytes());
        h.update([0x1f]);
    }
    h.update(ids.join("\x1f").as_bytes());
    let mut s = String::with_capacity(64);
    for b in h.finalize() {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn check_item(item: &NewItem) -> Result<(), MemError> {
    // A9（2026-11-07 W34）：`report` 是消费者报错的最小入口（MCP `recollect_report`）。
    // 它不是任何抽取 pass 的产物，与 `distill`/`rows` 并列成第三种来源。
    // 波二刀 B：`reuse` 是第四种来源（`recollect_reuse` 的 failed/mismatch 反馈经
    // `reuse::ingest_audit` 入队）——与 `report` 同一支：不归任何抽取 pass，只走人工判定链。
    if !matches!(item.source.as_str(), "distill" | "rows" | "report" | "reuse") {
        return Err(invalid("source"));
    }
    if !matches!(
        item.pass_id.as_str(),
        // P2：`state` 进封闭清单（与 `rc_ledger::pass_state::PASS_IDS` 同一组词、
        // 与 `review_item.pass_id` 的 CHECK 一对镜像）。A9 加 `report`：报告项不归属
        // 任何抽取 pass，且引擎侧 `supported_pass` 不认它 ⇒ 只走人工判定链
        // （`hub review decide`），窄审模型不碰它。刀 B 同形加 `reuse`：引擎同样不认它
        // （`supported_pass` 一个字没动），入队后只等人判。
        // 波三刀 A：`lesson` 是第 6 个抽取 pass（经验卡有自己的窄审入口），引擎认它
        // （`supported_pass` 有 `Pass::Lesson`），与 fact/event 走同一条窄审链。
        "naming" | "fact" | "event" | "time" | "rows" | "state" | "report" | "reuse" | "lesson"
    ) {
        return Err(invalid("pass_id"));
    }
    // family 只对 fact 取值，其余 pass 必须空串（同 `extract_pass_state` 的 API 校验）。
    if item.pass_id != "fact" && !item.family.is_empty() {
        return Err(invalid("family"));
    }
    if item.turn_uids_json.trim().is_empty() {
        return Err(invalid("turn_uids_json"));
    }
    if item.owner_json.trim().is_empty() {
        return Err(invalid("owner_json"));
    }
    if item.payload_json.chars().count() > MAX_PAYLOAD_CHARS {
        return Err(invalid("payload_json size"));
    }
    Ok(())
}

fn check_decision(d: &Decision) -> Result<(), MemError> {
    if d.reason_code.is_empty()
        || !d
            .reason_code
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(invalid("reason_code"));
    }
    if d.rules_version.trim().is_empty() {
        return Err(invalid("rules_version"));
    }
    if d.detail.chars().count() > MAX_DETAIL_CHARS {
        return Err(invalid("detail"));
    }
    Ok(())
}

fn find_by_payload(
    conn: &rusqlite::Connection,
    pass_id: &str,
    family: &str,
    sha: &str,
) -> Result<Option<(i64, ReviewState)>, MemError> {
    let row = conn
        .query_row(
            "SELECT id, state FROM review_item
              WHERE pass_id=?1 AND family=?2 AND payload_sha256=?3",
            rusqlite::params![pass_id, family, sha],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?;
    match row {
        None => Ok(None),
        Some((id, slug)) => Ok(Some((id, parse_state(&slug)?))),
    }
}

/// H1c：同键的在途行（`pending_review|quarantined|approved`）。终态不占位——它们不挡新项。
/// 正常库里同键在途至多一条（部分唯一索引 `review_item_batch_inflight` 执法）；`ORDER BY id
/// LIMIT 1` 只是防御：真出现第二条时插入新行会撞索引，响亮失败而不是悄悄选一条。
fn find_inflight_batch(
    conn: &rusqlite::Connection,
    pass_id: &str,
    family: &str,
    batch_key: &str,
) -> Result<Option<(i64, String)>, MemError> {
    let row = conn
        .query_row(
            "SELECT id, payload_sha256 FROM review_item
              WHERE pass_id=?1 AND family=?2 AND batch_key=?3
                AND state IN ('pending_review','quarantined','approved')
              ORDER BY id LIMIT 1",
            rusqlite::params![pass_id, family, batch_key],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?;
    Ok(row)
}

/// H1c：顶替一在途行。三步同事务（调用方的事务）：
/// ① 追加 `program/reject/superseded` 判定（`input_sha256` = 旧行的载荷指纹，
///    `review_input_sha256` 为空：程序裁定没有送审输入）；
/// ② 旧行转 `stale`，清退避与占位（被别人占着的位一并作废——它的载荷已不作数）；
/// ③ 新行由调用方随后插入。
fn supersede(
    tx: &rusqlite::Transaction<'_>,
    id: i64,
    old_payload_sha256: &str,
    now_ms: i64,
) -> Result<(), MemError> {
    tx.execute(
        "INSERT INTO review_decision(
            item_id, stage, verdict, reason_code, rules_version, model, prompt_version,
            input_sha256, review_input_sha256, decider, detail, created_at_ms)
         VALUES(?1,'program','reject',?2,?3,'','',?4,'','program',?5,?6)",
        rusqlite::params![
            id,
            SUPERSEDED,
            SUPERSEDE_RULES_VERSION,
            old_payload_sha256,
            SUPERSEDE_DETAIL,
            now_ms,
        ],
    )?;
    tx.execute(
        "UPDATE review_item SET
            state='stale',
            program_code=?2,
            next_retry_at_ms=NULL,
            claimed_by=NULL,
            claim_expires_at_ms=NULL,
            updated_at_ms=?3
          WHERE id=?1",
        rusqlite::params![id, SUPERSEDED, now_ms],
    )?;
    Ok(())
}

/// 顶替判定的判定码与 detail（闭集字面量，只住这一处）。
const SUPERSEDED: &str = "superseded";
const SUPERSEDE_DETAIL: &str = "batch_superseded";
/// 顶替是程序侧的确定性裁定，规则版本跟程序闸同一格。
const SUPERSEDE_RULES_VERSION: &str = "review-gate-v0.1";

/// 入队。命中唯一索引（同 `(pass_id, family, payload_sha256)`）⇒ 返回既有 id/state、
/// `fresh=false`，不覆盖、不报错。
///
/// H1c：`batch_key` 非空且同键有在途行时**顶替**（旧项 `stale` + 一条 `superseded` 判定），
/// 新行照常插入 `pending_review`。同键同载荷由上面那条载荷去重先命中（`fresh=false`）。
/// 终态同键不挡新项；并发靠 `IMMEDIATE` 事务串行化 + 部分唯一索引兜底。
pub fn enqueue(m: &MemoryHandle, item: &NewItem, now_ms: i64) -> Result<EnqueueOutcome, MemError> {
    if m.is_read_only() {
        return Err(MemError::ReadOnly);
    }
    check_item(item)?;
    let sha = payload_sha256(
        &item.pass_id,
        &item.family,
        &item.owner_json,
        &item.payload_json,
    );
    // IMMEDIATE：同进程/多进程写者在这里串行化；事务内先查后插。
    let tx = rusqlite::Transaction::new_unchecked(m.conn(), TransactionBehavior::Immediate)?;
    if let Some((id, state)) = find_by_payload(&tx, &item.pass_id, &item.family, &sha)? {
        tx.commit()?;
        return Ok(EnqueueOutcome {
            id,
            payload_sha256: sha,
            fresh: false,
            state,
        });
    }
    // 空 key = 旧调用方，不参与顶替（只为兼容；生产路径一律非空）。
    let stored_key = (!item.batch_key.is_empty()).then_some(item.batch_key.as_str());
    if let Some(key) = stored_key {
        if let Some((old_id, old_sha)) =
            find_inflight_batch(&tx, &item.pass_id, &item.family, key)?
        {
            supersede(&tx, old_id, &old_sha, now_ms)?;
        }
    }
    let inserted = tx.execute(
        "INSERT INTO review_item(
            source, pass_id, family, session_key, device_id, turn_uids_json, owner_json,
            payload_json, payload_sha256, state, created_at_ms, updated_at_ms, batch_key)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'pending_review',?10,?10,?11)",
        rusqlite::params![
            item.source,
            item.pass_id,
            item.family,
            item.session_key,
            item.device_id,
            item.turn_uids_json,
            item.owner_json,
            item.payload_json,
            sha,
            now_ms,
            stored_key,
        ],
    );
    if let Err(e) = inserted {
        // 并发写者抢先插入：唯一索引命中 ⇒ 返回既有行；其余错误照旧上抛。
        if let Some((id, state)) = find_by_payload(&tx, &item.pass_id, &item.family, &sha)? {
            tx.commit()?;
            return Ok(EnqueueOutcome {
                id,
                payload_sha256: sha,
                fresh: false,
                state,
            });
        }
        return Err(e.into());
    }
    let id = tx.last_insert_rowid();
    tx.commit()?;
    Ok(EnqueueOutcome {
        id,
        payload_sha256: sha,
        fresh: true,
        state: ReviewState::PendingReview,
    })
}

fn meta_of(row: &rusqlite::Row<'_>) -> Result<ItemMeta, MemError> {
    let slug: String = row.get(6)?;
    Ok(ItemMeta {
        id: row.get(0)?,
        source: row.get(1)?,
        pass_id: row.get(2)?,
        family: row.get(3)?,
        session_key: row.get(4)?,
        device_id: row.get(5)?,
        state: parse_state(&slug)?,
        program_code: row.get(7)?,
        attempt: row.get(8)?,
        created_at_ms: row.get(9)?,
        updated_at_ms: row.get(10)?,
        applied_at_ms: row.get(11)?,
    })
}

/// 按 id 取全量（含载荷）。不存在 ⇒ `None`。
pub fn get(m: &MemoryHandle, id: i64) -> Result<Option<Item>, MemError> {
    let sql = format!(
        "SELECT {META_COLUMNS}, turn_uids_json, owner_json, payload_json, payload_sha256, \
                conflict_json
           FROM review_item WHERE id=?1"
    );
    let mut st = m.conn().prepare(&sql)?;
    let mut rows = st.query([id])?;
    match rows.next()? {
        None => Ok(None),
        Some(row) => Ok(Some(Item {
            meta: meta_of(row)?,
            turn_uids_json: row.get(12)?,
            owner_json: row.get(13)?,
            payload_json: row.get(14)?,
            payload_sha256: row.get(15)?,
            conflict_json: row.get(16)?,
        })),
    }
}

/// 一条判定的只读视图（`decisions` 的返回行）。`detail` 原样带回调用方解析，
/// 本层不做语义解释。H4b：两个指纹列一并读回（列语义见 [`Decision`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRow {
    pub id: i64,
    pub item_id: i64,
    pub stage: ReviewStage,
    pub verdict: ReviewVerdict,
    pub reason_code: String,
    pub detail: String,
    pub input_sha256: String,
    pub review_input_sha256: String,
    pub created_at_ms: i64,
}

/// C3 逐 draft 拒收审计行的 `detail` 前缀：`ref=<f1|e1|s1|a1|…>`。
///
/// 这些行与 item 级的程序判定共用 `stage=program / verdict=reject`，区别只在 `detail`：
/// 它们说的是"**这一条草稿**被闸拒了"，不是"这条 item 的闸结论是 reject"。读者（
/// `program_decided`、human pass 的重建）据此把它们排除在外——否则一条 pending 的 item
/// 会看起来"程序阶段已表态"，human pass 也会被自己写下的审计行顶掉。
pub const GATE_REJECT_DETAIL_PREFIX: &str = "ref=";

impl DecisionRow {
    /// 这一行是不是 C3 的逐 draft 拒收审计（见 [`GATE_REJECT_DETAIL_PREFIX`]）。
    pub fn is_gate_reject_audit(&self) -> bool {
        self.stage == ReviewStage::Program
            && self.verdict == ReviewVerdict::Reject
            && self.detail.starts_with(GATE_REJECT_DETAIL_PREFIX)
    }
}

/// 该 item 的判定台账（只读），按 `id` 升序。未知 stage/verdict slug 报错而不是折默认：
/// 读不出的判定不是"没有判定"，把坏行读成某一格会让重试路径拿错判决。
pub fn decisions(m: &MemoryHandle, item_id: i64) -> Result<Vec<DecisionRow>, MemError> {
    let mut st = m.conn().prepare(
        "SELECT id, item_id, stage, verdict, reason_code, detail, input_sha256,
                review_input_sha256, created_at_ms
           FROM review_decision WHERE item_id=?1 ORDER BY id",
    )?;
    let mut rows = st.query([item_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let stage_slug: String = row.get(2)?;
        let verdict_slug: String = row.get(3)?;
        out.push(DecisionRow {
            id: row.get(0)?,
            item_id: row.get(1)?,
            stage: ReviewStage::parse(&stage_slug).ok_or_else(|| {
                MemError::Sql("review: unknown stage in review_decision".to_string())
            })?,
            verdict: ReviewVerdict::parse(&verdict_slug).ok_or_else(|| {
                MemError::Sql("review: unknown verdict in review_decision".to_string())
            })?,
            reason_code: row.get(4)?,
            detail: row.get(5)?,
            input_sha256: row.get(6)?,
            review_input_sha256: row.get(7)?,
            created_at_ms: row.get(8)?,
        });
    }
    Ok(out)
}

/// 该 item 是否已有某一阶段的判定（只读）。S3a-3 的 runner 用它判断
/// "程序闸 pass 判定是否已记过"，避免同一次入队重跑时写第二条 program 判定。
/// 只查一张表的只读语句，不改状态机。
pub fn has_stage(m: &MemoryHandle, id: i64, stage: ReviewStage) -> Result<bool, MemError> {
    let n: i64 = m.conn().query_row(
        "SELECT count(*) FROM review_decision WHERE item_id=?1 AND stage=?2",
        rusqlite::params![id, stage.slug()],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// 列表（不含载荷）。`state`/`pass_id` 为 `None` 表示不过滤；`limit=0` 表示不设上限。
/// 顺序固定 `id` 升序。
pub fn list(
    m: &MemoryHandle,
    state: Option<ReviewState>,
    pass_id: Option<&str>,
    limit: usize,
) -> Result<Vec<ItemMeta>, MemError> {
    let limit = if limit == 0 { -1 } else { limit as i64 };
    let sql = format!(
        "SELECT {META_COLUMNS} FROM review_item
          WHERE (?1 IS NULL OR state = ?1)
            AND (?2 IS NULL OR pass_id = ?2)
          ORDER BY id
          LIMIT ?3"
    );
    let mut st = m.conn().prepare(&sql)?;
    let mut rows = st.query(rusqlite::params![state.map(|s| s.slug()), pass_id, limit])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(meta_of(row)?);
    }
    Ok(out)
}

/// 记一次失败尝试并推迟下一次可运行时刻（设计 §4 的退避；`hub review run` 的 LLM 故障路径）。
///
/// 只动 `attempt`/`next_retry_at_ms`/`updated_at_ms` 三列，**不写判定行**：LLM 故障不是判决
/// （设计 §1 第 5 条）。`state IN ('pending_review','quarantined','approved')` 之外（终态或不存在）
/// ⇒ 0 行命中 ⇒ `MemError::Sql`：一次迟到的重试登记不许把终态复活成"可重跑"。
pub fn note_attempt(
    m: &MemoryHandle,
    id: i64,
    backoff_ms: i64,
    now_ms: i64,
) -> Result<(), MemError> {
    if m.is_read_only() {
        return Err(MemError::ReadOnly);
    }
    let hit = m.conn().execute(
        "UPDATE review_item
            SET attempt=attempt+1, next_retry_at_ms=?2, updated_at_ms=?3
          WHERE id=?1 AND state IN ('pending_review','quarantined','approved')",
        rusqlite::params![id, now_ms.saturating_add(backoff_ms), now_ms],
    )?;
    if hit == 0 {
        return Err(MemError::Sql(
            "review: note_attempt hit a terminal or missing item".to_string(),
        ));
    }
    Ok(())
}

/// 现在可运行的 item（不含载荷）：三种非终态里退避已到期（或从未退避）的那些，
/// 可按 `pass_id` 过滤，`limit=0` 表示不设上限。顺序固定 `id` 升序。
///
/// 与 [`list`] 的差别只有两条：状态是"可运行"的闭集（`pending_review|quarantined|approved`），
/// 且多一道 `next_retry_at_ms` 到期的闸。调用方逐条 [`get`] 取载荷。
pub fn runnable(
    m: &MemoryHandle,
    pass_id: Option<&str>,
    now_ms: i64,
    limit: usize,
) -> Result<Vec<ItemMeta>, MemError> {
    let limit = if limit == 0 { -1 } else { limit as i64 };
    let sql = format!(
        "SELECT {META_COLUMNS} FROM review_item
          WHERE state IN ('pending_review','quarantined','approved')
            AND (next_retry_at_ms IS NULL OR next_retry_at_ms <= ?1)
            AND (?2 IS NULL OR pass_id = ?2)
          ORDER BY id
          LIMIT ?3"
    );
    let mut st = m.conn().prepare(&sql)?;
    let mut rows = st.query(rusqlite::params![now_ms, pass_id, limit])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(meta_of(row)?);
    }
    Ok(out)
}

/// 按**一组** pass 选可运行项（波三修 2）：判据、排序与 [`runnable`] 逐条相同，只是把
/// `pass_id` 的域从一个值换成一个闭集。空集 ⇒ 空表（"不限"请传 [`runnable`] 的 `None`）。
///
/// 实现是逐 pass 调一次 [`runnable`] 再按 `id` 归并，不另写一条带 `IN (?,?,…)` 的 SQL：
/// 判据只有一处（状态闭集 + 退避到期），不会因为参数个数变化漂出第二份。闭集大小是个位数
/// （引擎认的 pass 就那么几个），逐条查询的开销可以忽略；`limit=0` 不设上限，否则按
/// `id` 升序截断。
pub fn runnable_in(
    m: &MemoryHandle,
    pass_ids: &[&str],
    now_ms: i64,
    limit: usize,
) -> Result<Vec<ItemMeta>, MemError> {
    let mut out: Vec<ItemMeta> = Vec::new();
    for pass_id in pass_ids {
        out.extend(runnable(m, Some(pass_id), now_ms, 0)?);
    }
    out.sort_by_key(|meta| meta.id);
    if limit != 0 {
        out.truncate(limit);
    }
    Ok(out)
}

/// 现在可运行、但**不属于** `exclude` 这一组 pass 的条数（波三修 2 的"跳过计数"）。
///
/// 判据与 [`runnable`] 同一份（三态 + 退避到期）；`exclude` 为空 = 全库。它只回答
/// "队列里有几条是这条命令永远吃不动的人工项"（`report`/`reuse`），不选、不占位。
pub fn count_runnable_outside(
    m: &MemoryHandle,
    exclude: &[&str],
    now_ms: i64,
) -> Result<i64, MemError> {
    let all = runnable(m, None, now_ms, 0)?;
    Ok(all
        .iter()
        .filter(|meta| !exclude.contains(&meta.pass_id.as_str()))
        .count() as i64)
}

/// H1a：原子占位。只有 `pending_review|quarantined|approved`、退避已到期
/// （`next_retry_at_ms` 为空或 `<= now`），且无人占用（`claim_expires_at_ms` 为空或
/// `<= now`）时才成功；`rows_affected == 1` 即"本次抢到"，`false` 表示别人先占了或
/// 状态不该跑（调用方据此跳过，不是错误）。
///
/// 单条 `UPDATE` 就是那个原子点：SQLite 的写事务把并发写者串行化，第二个写者看到的是
/// 第一个提交后的 `claimed_by`/租约，条件不再成立。`runnable` **不**过滤占用——竞态由
/// 这里决出，选中列表与占位之间不再有"两个 run 都以为自己该跑"的窗口。
pub fn claim(
    m: &MemoryHandle,
    id: i64,
    owner: &str,
    lease_ms: i64,
    now_ms: i64,
) -> Result<bool, MemError> {
    if m.is_read_only() {
        return Err(MemError::ReadOnly);
    }
    let hit = m.conn().execute(
        "UPDATE review_item
            SET claimed_by=?2, claim_expires_at_ms=?3
          WHERE id=?1
            AND state IN ('pending_review','quarantined','approved')
            AND (next_retry_at_ms IS NULL OR next_retry_at_ms <= ?4)
            AND (claim_expires_at_ms IS NULL OR claim_expires_at_ms <= ?4)",
        rusqlite::params![id, owner, now_ms.saturating_add(lease_ms), now_ms],
    )?;
    Ok(hit == 1)
}

/// H1a：只清**自己**占的位（`owner` 不匹配不动）。0 行不是错误：位已被别人接管或
/// 租约已过期都由调用方的下一轮 claim 重新决出。返回值恒为 `Ok`（除非句柄只读/IO 错）。
pub fn release(m: &MemoryHandle, id: i64, owner: &str) -> Result<(), MemError> {
    if m.is_read_only() {
        return Err(MemError::ReadOnly);
    }
    m.conn().execute(
        "UPDATE review_item SET claimed_by=NULL, claim_expires_at_ms=NULL
          WHERE id=?1 AND claimed_by=?2",
        rusqlite::params![id, owner],
    )?;
    Ok(())
}

struct Transition {
    state: ReviewState,
    attempt: i64,
    next_retry_at_ms: Option<i64>,
    applied_at_ms: Option<i64>,
    /// 只有 program 阶段改写 `program_code`（pass ⇒ NULL，reject/uncertain ⇒ reason_code）。
    set_program_code: bool,
    program_code: Option<String>,
}/// 纯函数：把 `(current state, decision)` 折成目标行。非法边在这里返回错误，
/// 事务里因此不会落下任何一行。
fn transition(
    d: &Decision,
    state: ReviewState,
    attempt: i64,
    now_ms: i64,
) -> Result<Transition, MemError> {
    let mut t = Transition {
        state,
        attempt,
        next_retry_at_ms: None,
        applied_at_ms: None,
        set_program_code: false,
        program_code: None,
    };
    match d.stage {
        ReviewStage::Program => {
            t.set_program_code = true;
            match d.verdict {
                ReviewVerdict::Pass if state == ReviewState::PendingReview => {
                    t.state = ReviewState::PendingReview;
                    t.program_code = None;
                }
                // H2：程序侧的"不确定"是一条合法边——载荷不可重放/判决重建不出时，
                // 把 item 推到 `quarantined`，让 stats/remaining/退出码看得见。
                // 起点只有 `pending_review`/`approved`：`applied`/`rejected`/`stale` 是终态
                // （已写库/已驳回，隔离会改写终态语义），`quarantined` 的重跑走 `review` 阶段。
                ReviewVerdict::Uncertain
                    if matches!(state, ReviewState::PendingReview | ReviewState::Approved) =>
                {
                    t.state = ReviewState::Quarantined;
                    // 与 reject 同格：非 pass 的程序裁定把原因码留在 `program_code`，
                    // `review show`/`list` 不必翻判定台账就能看到为什么被隔离。
                    t.program_code = Some(d.reason_code.clone());
                }
                ReviewVerdict::Reject if !state.is_terminal() => {
                    t.state = ReviewState::Rejected;
                    t.program_code = Some(d.reason_code.clone());
                }
                _ => return Err(illegal(d, state)),
            }
        }
        ReviewStage::Review => {
            // S3b-4a：`quarantined` 也可复审（隔离项由 `hub review run` 重跑窄审）。
            // 其余状态不变：`rejected`/`applied`/`stale` 仍是终态，`approved` 只能走 apply。
            if !matches!(state, ReviewState::PendingReview | ReviewState::Quarantined) {
                return Err(illegal(d, state));
            }
            t.state = match d.verdict {
                ReviewVerdict::Pass => ReviewState::Approved,
                ReviewVerdict::Reject => ReviewState::Rejected,
                ReviewVerdict::Uncertain => ReviewState::Quarantined,
            };
        }
        ReviewStage::Apply => {
            if state != ReviewState::Approved {
                return Err(illegal(d, state));
            }
            match d.verdict {
                ReviewVerdict::Pass => {
                    t.state = ReviewState::Applied;
                    t.applied_at_ms = Some(now_ms);
                }
                ReviewVerdict::Reject => {
                    // `attempt` 取 +1 之前的次数（0 起）⇒ 第一次失败退避 60s，
                    // 与 S2 的 60s × 4^n 同一组数。上限由调用方改判 quarantined。
                    let exponent = attempt.max(0) as u32;
                    let backoff = RETRY_BASE_MS.saturating_mul(4_i64.saturating_pow(exponent));
                    t.attempt = attempt + 1;
                    t.next_retry_at_ms = Some(now_ms.saturating_add(backoff));
                    t.state = ReviewState::Approved;
                }
                ReviewVerdict::Uncertain => return Err(illegal(d, state)),
            }
        }
        ReviewStage::Human => {
            if d.decider != Decider::Human
                || !matches!(
                    state,
                    ReviewState::PendingReview | ReviewState::Quarantined | ReviewState::Approved
                )
            {
                return Err(illegal(d, state));
            }
            t.state = match d.verdict {
                ReviewVerdict::Pass => ReviewState::Approved,
                ReviewVerdict::Reject => ReviewState::Rejected,
                ReviewVerdict::Uncertain => return Err(illegal(d, state)),
            };
        }
    }
    Ok(t)
}

/// 记一条判定并推进状态。判定与状态更新在**同一事务**里：任何一步失败都回滚。
/// 非法转换返回 `MemError::Sql("review: illegal transition …")`。
pub fn record_decision(
    m: &MemoryHandle,
    id: i64,
    d: &Decision,
    now_ms: i64,
) -> Result<ReviewState, MemError> {
    if m.is_read_only() {
        return Err(MemError::ReadOnly);
    }
    check_decision(d)?;
    let tx = rusqlite::Transaction::new_unchecked(m.conn(), TransactionBehavior::Immediate)?;
    let current = tx
        .query_row(
            "SELECT state, attempt FROM review_item WHERE id=?1",
            [id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?;
    let Some((state_slug, attempt)) = current else {
        return Err(MemError::Sql("review: no such review item".to_string()));
    };
    let next = transition(d, parse_state(&state_slug)?, attempt, now_ms)?;

    tx.execute(
        "INSERT INTO review_decision(
            item_id, stage, verdict, reason_code, rules_version, model, prompt_version,
            input_sha256, review_input_sha256, decider, detail, created_at_ms)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        rusqlite::params![
            id,
            d.stage.slug(),
            d.verdict.slug(),
            d.reason_code,
            d.rules_version,
            d.model,
            d.prompt_version,
            d.input_sha256,
            d.review_input_sha256,
            d.decider.slug(),
            d.detail,
            now_ms,
        ],
    )?;
    tx.execute(
        "UPDATE review_item SET
            state=?2,
            program_code=CASE WHEN ?3 THEN ?4 ELSE program_code END,
            attempt=?5,
            next_retry_at_ms=?6,
            applied_at_ms=COALESCE(?7, applied_at_ms),
            updated_at_ms=?8
          WHERE id=?1",
        rusqlite::params![
            id,
            next.state.slug(),
            i64::from(next.set_program_code),
            next.program_code,
            next.attempt,
            next.next_retry_at_ms,
            next.applied_at_ms,
            now_ms,
        ],
    )?;
    tx.commit()?;
    Ok(next.state)
}

/// C3：逐 draft 程序拒收的**审计行**——只追加一行 `review_decision`，**不推进** `review_item`。
///
/// 与 [`record_decision`] 的分工：那一条是状态机的边（判定 + 状态 + 程序码在同一事务里），
/// 而程序闸对**单条 draft** 的拒收不改变 item 的状态——同一条 item 里其余 draft 照旧进审核、
/// 照旧写库。所以这一格只能是纯追加：形状固定在
/// `stage=program / verdict=reject / decider=program`，别的组合一律拒收（否则就是绕开状态机）。
///
/// 幂等：同一 `(item_id, reason_code, detail)` 只落一行（`INSERT … WHERE NOT EXISTS`）。
/// 同一条 item 重跑（`hub review run` 对 pending/approved/applied 的复算）取到同一份闸结论时
/// 不再追加；码变了说明是另一次裁定，另起一行——审计台账要能同时看到两次。
/// 返回 `true` 表示这次真的插了一行。
pub fn append_program_reject(
    m: &MemoryHandle,
    id: i64,
    d: &Decision,
    now_ms: i64,
) -> Result<bool, MemError> {
    if m.is_read_only() {
        return Err(MemError::ReadOnly);
    }
    if d.stage != ReviewStage::Program
        || d.verdict != ReviewVerdict::Reject
        || d.decider != Decider::Program
    {
        return Err(invalid("program reject audit shape"));
    }
    // 形状锁在 [`GATE_REJECT_DETAIL_PREFIX`] 上：审计行必须能被读者认出来（见
    // [`DecisionRow::is_gate_reject_audit`]），否则它会被读成 item 级的闸结论。
    if !d.detail.starts_with(GATE_REJECT_DETAIL_PREFIX) {
        return Err(invalid("program reject audit detail"));
    }
    check_decision(d)?;
    let inserted = m.conn().execute(
        "INSERT INTO review_decision(
            item_id, stage, verdict, reason_code, rules_version, model, prompt_version,
            input_sha256, review_input_sha256, decider, detail, created_at_ms)
         SELECT ?1, 'program', 'reject', ?2, ?3, ?4, ?5, ?6, ?7, 'program', ?8, ?9
          WHERE NOT EXISTS (
            SELECT 1 FROM review_decision
             WHERE item_id=?1 AND stage='program' AND verdict='reject'
               AND decider='program' AND reason_code=?2 AND detail=?8)",
        rusqlite::params![
            id,
            d.reason_code,
            d.rules_version,
            d.model,
            d.prompt_version,
            d.input_sha256,
            d.review_input_sha256,
            d.detail,
            now_ms,
        ],
    )?;
    Ok(inserted > 0)
}

/// 本地判定台账的最大 id（0 = 一条判定都没有）。
///
/// H7c2：审查轴水位的第二个分量。它只在**本地**读，不比较、不写入；接管后的比较归
/// `hub reconcile memory`（ledger 水位 vs 本地这一格）。`COALESCE` 让空库读回 0 而不是
/// 一行空——"没有判定"与"读到 0"是同一件事，调用方不必再折一次 `Option`。
pub fn max_decision_id(m: &MemoryHandle) -> Result<i64, MemError> {
    let id: i64 = m.conn().query_row(
        "SELECT COALESCE(MAX(id), 0) FROM review_decision",
        [],
        |r| r.get(0),
    )?;
    Ok(id)
}

/// 计数读数：六个状态、最老 pending 的年龄、按 pass 分组（6 格顺序见 [`ReviewStats`]）。
pub fn stats(m: &MemoryHandle, now_ms: i64) -> Result<ReviewStats, MemError> {
    let mut counts = [0i64; 6];
    {
        let mut st = m
            .conn()
            .prepare("SELECT state, count(*) FROM review_item GROUP BY state")?;
        let mut rows = st.query([])?;
        while let Some(row) = rows.next()? {
            let slug: String = row.get(0)?;
            counts[parse_state(&slug)?.index()] = row.get(1)?;
        }
    }
    let oldest: Option<i64> = m.conn().query_row(
        "SELECT MIN(created_at_ms) FROM review_item WHERE state='pending_review'",
        [],
        |r| r.get(0),
    )?;
    let mut by: std::collections::BTreeMap<String, [i64; 6]> = std::collections::BTreeMap::new();
    {
        let mut st = m.conn().prepare(
            "SELECT pass_id, state, count(*) FROM review_item
              GROUP BY pass_id, state ORDER BY pass_id, state",
        )?;
        let mut rows = st.query([])?;
        while let Some(row) = rows.next()? {
            let pass_id: String = row.get(0)?;
            let slug: String = row.get(1)?;
            let n: i64 = row.get(2)?;
            by.entry(pass_id).or_insert([0; 6])[parse_state(&slug)?.index()] = n;
        }
    }
    Ok(ReviewStats {
        pending_review: counts[0],
        approved: counts[1],
        applied: counts[2],
        rejected: counts[3],
        quarantined: counts[4],
        stale: counts[5],
        oldest_pending_age_ms: oldest.map(|t| now_ms - t),
        by_pass: by.into_iter().collect(),
    })
}

/// 清理终态 item 的载荷（`applied`/`rejected`/`stale` 且 `updated_at_ms < older_than_ms`）：
/// 把 `payload_json`/`turn_uids_json`/`owner_json` 置空并刷新 `updated_at_ms`。
/// 判定台账一行不动；已清空的 item 不重复计。返回本次处理的 item 数。
pub fn prune(m: &MemoryHandle, older_than_ms: i64, now_ms: i64) -> Result<usize, MemError> {
    if m.is_read_only() {
        return Err(MemError::ReadOnly);
    }
    let n = m.conn().execute(
        "UPDATE review_item
            SET payload_json='', turn_uids_json='', owner_json='', updated_at_ms=?2
          WHERE state IN ('applied','rejected','stale')
            AND updated_at_ms < ?1
            AND (payload_json <> '' OR turn_uids_json <> '' OR owner_json <> '')",
        rusqlite::params![older_than_ms, now_ms],
    )?;
    Ok(n)
}
