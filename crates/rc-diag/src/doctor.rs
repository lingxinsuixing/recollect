//! doctor 的读数面。
//!
//! **A38 对账**：`rows_indexed`（索引层的行数）与 `reader::facts().len()`（内容层的行数）
//! 对不上，就是索引层与内容层脱节了。取不到任何一头时给 `None`（"对不了账"），
//! **不是 0**——0 会被读成"两头顶上了、差 0"，那是最像成功的一种读数。

use rc_store::MemoryHandle;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reconcile {
    pub rows_indexed: u64,
    pub facts: usize,
}

impl Reconcile {
    pub fn delta(&self) -> i64 {
        self.rows_indexed as i64 - self.facts as i64
    }
    pub fn agrees(&self) -> bool {
        self.delta() == 0
    }
}

/// 手写而不是 derive：doctor 的读数面要一眼看见 `delta`（`a_mismatch...` 与
/// `a_reconcile_mismatch_raises_one_alert` 都按 `reconcile.delta` 读），而 `delta`
/// 是两格算出来的，不是第三格状态——序列化时现算，就没有第二份真相可漂。
impl serde::Serialize for Reconcile {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("Reconcile", 3)?;
        st.serialize_field("rows_indexed", &self.rows_indexed)?;
        st.serialize_field("facts", &self.facts)?;
        st.serialize_field("delta", &self.delta())?;
        st.end()
    }
}

/// 对账。**两头都在 `rc-diag` 里取**：`rows_indexed` 来自索引层的影子 marker
/// （`rc_index::index_status`），`facts` 来自内容层。`index_status` 的 `None`
/// （没建过索引）与 `facts` 的读失败都让本函数给 `None`——"对不了账"是三态里的第三态，
/// 不是"差 0"。
pub fn reconcile(m: &MemoryHandle) -> Option<Reconcile> {
    let rows_indexed = rc_index::index_status(m).ok()??.rows_indexed;
    let facts = rc_memory::reader::facts(m).ok()?.len();
    Some(Reconcile { rows_indexed, facts })
}

/// 按 kind 过滤 attention 的未解决条数。**按 kind 分格**是那一格第一次有读者：
/// 此前 `AttentionKind::parse` 全仓零调用点。
pub fn attention_open_by_kind(m: &MemoryHandle, kind: rc_memory::keys::AttentionKind) -> i64 {
    // 用 `open_count` 的同一把读法按 kind 过滤：这里只加一个 WHERE 条件，
    // 不新造一条 SQL 家族（守卫 12：同一件事不许有第二个家）。
    let sql = "SELECT count(*) FROM attention WHERE resolved_at_ms IS NULL AND kind = ?1";
    let conn = m.conn();
    conn.query_row(sql, [kind.as_str()], |r| r.get::<_, i64>(0))
        .unwrap_or(0)
}

/// 最近一次抽取运行触顶 ⇒ 一条告警（S3a）。**`consequences` 此前只有测试在读**，
/// 而"配额烧完"是真实运维事件：`halted_of` 只覆盖"停了"，不覆盖"为什么停"。
///
/// S3a 换了判据的输入：旧口径把生命周期 `llm_calls` 求和与单轮上限相比 ⇒ 累计一旦越过 40
/// 就永远告警（88 次历史调用被读成"本轮烧完"）。新口径读 [`rc_ledger::counters::quota_stopped`]
/// ——hub 每次运行收尾写下的 0/1 覆盖值，`None`（还没有运行记录）与 `Some(false)` 都不告警。
/// `status` 的 `quota_exhausted` 来自同一个函数（经 `CycleHealth` 搬运），两面结论同源。
///
/// `rc_llm::Consequence` 是**枚举**（`StopNewExtract` / `KeepViewsOpen` /
/// `BidirectionalAlert` / `ResumeAndCatchUp`），**没有 `halts_job` 字段**——判据就在变体名上。
pub fn quota_exhausted_alerts(rows: &[rc_ledger::counters::UsageRow]) -> Vec<&'static str> {
    if rc_ledger::counters::quota_stopped(rows) != Some(true) {
        return Vec::new();
    }
    // `consequences` 对四种失败态返回同一组动作（它的形参被 `_f` 忽略）。
    // 所以这一格判的不是"配额耗尽会停抽取"（那对四种失败态都成立），
    // 而是"**这一族失败的后果里有停抽取**"——判据在变体上，不在文案上。
    let conseq = rc_llm::consequences(&rc_llm::LlmFailure::QuotaExhausted);
    if conseq.contains(&rc_llm::Consequence::StopNewExtract) {
        return vec!["quota_exhausted"];
    }
    Vec::new()
}

/// 推导家族**休眠** ⇒ 一条告警（S5）。判据只有两格：`last_derive_at_ms` 是 `None`
/// （水位行的 `last_success_at_ms` 从没写过）**且** `derive_lag_rows > 0`（池里还有活行
/// 没进过批）。两格缺一都不告警：
/// - `lag == 0` 是"追平了"，不是"休眠"；
/// - `last` 有值是"跑过"，落后几行由 `derive_lag_rows` 自己说，不是这条告警的事。
///
/// 为什么两格都要：只拿 `None` 判会把每个新建的空库读成故障（判据 43 钉的是空库
/// `doctor --json` 的 `alerts` 必须是 `[]`）；只拿 `lag > 0` 判会把"今天正好有五行待推导"
/// 读成告警，那是正常运转。
///
/// 为什么不是"距上次推导超过 N 小时"：那个窗口没有生产者定过值（全仓没有任何常数写着
/// "推导超过多久算死"），而"从没成功过 + 还有活行"是**结构性**的休眠，不需要时钟。
///
/// 返回 `&'static str` 的 slug 清单，与 [`quota_exhausted_alerts`] 同形：判定（有没有、
/// 叫什么）住这里，数字的渲染住 `doctor`（守卫 3 的镜像——同一件事不许有第二个家）。
pub fn derive_dormant_alert(
    last_derive_at_ms: Option<i64>,
    derive_lag_rows: i64,
) -> Vec<&'static str> {
    if last_derive_at_ms.is_none() && derive_lag_rows > 0 {
        return vec!["derive_dormant"];
    }
    Vec::new()
}
