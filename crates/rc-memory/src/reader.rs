//! 读回：一行记忆"现在是什么形态"。`RowFacts` 是本阶段第一个装正文的结构，
//! 它住 `rc-memory` 是因为它被 `rc-search` 消费（`Scored` 与 `Decision` 都带着它走）。
//! 守卫 12 的执法点不在这里——执法点是"谁能把它变成输出"（任务 7 的 `rc-agent`），
//! 不是"谁能读到它"；读得到正文而变不出输出，正是这一层该有的形状。
//!
//! SQL 里的每一个字面量 slug 都从枚举取（`AnchorRole::Human.as_str()` 而不是手写 'human'），
//! 因为 `ddl_shape.rs` 只拍 Rust ↔ SQL CHECK 那三处，拍不到这里的 CASE。

use crate::keys::{AnchorRole, MemKey, MemTable, Scope, ScopeKind, Stance, Verdict};
use crate::redirect::{self, RedirectResolve};
use crate::MemError;
use rc_store::MemoryHandle;
use rusqlite::OptionalExtension;

/// 方案二（D8）解析的"可用终点"：Alive/Merged → 链终点；**Broken → 原 id 自己**
/// （解析失败回落旧锚自己的行——可见，不静默成"没有"；`redirect.rs` 档头第 1 条）；
/// Missing → 原 id（与既有行为逐位相同，不改历史形状）。
/// 读侧消费 anchor id 的**唯一**入口：六个读者（entity_ids_for_name/row_mentions_any/
/// row_participants/anchor_names/display_name/turn_spoken_by）都走它，不各写一份解析。
///
/// W15c 起 **写入侧的身份查询也走这一处**（`writers_v4::session_agent_anchor_id` /
/// `session_principal_anchor_id`）：别各写一份 `redirect::resolve`，两条路径的终端语义
/// 必须逐位相同，否则写进去的说话人与读回来的说话人会分叉。
pub(crate) fn redirect_terminal(m: &MemoryHandle, id: &str) -> Result<String, MemError> {
    match redirect::resolve(m, id)? {
        r @ (RedirectResolve::Alive(_) | RedirectResolve::Merged { .. }) => {
            Ok(r.terminal_or_self().to_string())
        }
        _ => Ok(id.to_string()),
    }
}

/// `time_window` 落在**哪一口钟**上（C6）。两值的差别只有一件事：没有事件时间的行怎么办。
///
/// - `AsOf`（缺省 = 加这一格之前的唯一行为）：按 [`RowFacts::as_of_ms`] 判。
///   card 行读的是 `card.as_of_ms` 列——抽取行那格是**源轮时刻**（发话钟），episode 镜像卡
///   与派生卡另有写侧口径（`writers_x::episode_time_arm` 等）；anchor 行读侧补的是字面量 0。
/// - `Event`：按 `card.time_json` 里 `kind='event_time'` 的 `start_ms` 判（判据的家是
///   [`card_event_times`]，坏 JSON / 缺键 / 非整数一律当"没有事件时间"）。
///   **没有事件时间的行不匹配**，不回落 `as_of_ms`——"筛的时候悄悄换一口会回落的钟"
///   正是这一格要消掉的东西。
///
/// 注意它**不是** filter 的一项：单独给 `time_basis` 而没有 `time_window` 时它不筛掉任何行
/// （窗才是判据，这一格只是选钟），所以 [`RecallFilter::is_empty`] 的判定不变。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeBasis {
    #[default]
    AsOf,
    Event,
}

impl TimeBasis {
    /// 封闭两值（键就是参数面那两个 slug）；认不出的串返回 `None`——调用方决定那是拒绝还是
    /// 缺省，不在这一层静默折成 `as_of`（那会把一个拼错的参数读成"按默认口径筛过了"）。
    pub fn parse(s: &str) -> Option<Self> {
        match s { "as_of" => Some(TimeBasis::AsOf), "event" => Some(TimeBasis::Event), _ => None }
    }
}

/// recall 的四个过滤项（§10 任务 10）。**四可选字段 AND 组合，全 `None` ⇒ 不过滤**；
/// `entity` 取的是**名字**，判据侧走别名/标题双路解析（`entity_ids_for_name`）。
///
/// 这里只声明"能筛什么"，**不声明怎么筛**——过滤发生在 rank 之后的内存侧
/// （`rc-agent` 的 `recall_filter_match`），不进 `facts()` 的 `WHERE`：把过滤下推进 SQL
/// 就是 rank 之前改候选集 ⇒ 排序不真 ⇒ G6 判据③破。所以本文件与这一族相关的只有两件事：
/// `RowFacts` 把四个过滤项要读的列取出来，`entity_ids_for_name` 把名字解析成 id。
#[derive(Debug, Clone, Default)]
pub struct RecallFilter {
    pub device_id: Option<String>,
    pub app: Option<String>,
    pub entity: Option<String>,
    /// 闭区间 `(from, to)`，按 [`Self::time_basis`] 选定的那口钟比（缺省 `as_of`）。
    pub time_window: Option<(i64, i64)>,
    /// `time_window` 的选钟（C6）。`None` 与 `Some(TimeBasis::AsOf)` 是同一件事：
    /// 按 `as_of_ms` 判，与加这一格之前逐字节相同。
    pub time_basis: Option<TimeBasis>,
    /// **不是筛选条件，是读者身份**（S1b-C4a）。`None` = 不过读闸
    /// （中心 CLI/测试/旧调用）；节点面的 mcp 生产读路径必须给 `Some(Viewer::agent(..))`。
    /// 它住这里而不另开一个形参：`recall` 的两条臂共用同一个 `filter`，而"谁能读"
    /// 与"筛什么"必须一起传到取数点，否则第一条臂接了、第二条臂漏了就是静默越权。
    pub viewer: Option<crate::gate::Viewer>,
}
impl RecallFilter {
    pub fn empty() -> Self { Self::default() }
    /// `viewer` **不算**过滤项：它不是"筛选条件"，所以 `is_empty()` 不变——
    /// 把它算进来会让"空过滤器"这个既有口径在接上读闸的那天悄悄改义。
    /// `time_basis` 同理不进这一句：它选的是 `time_window` 的钟，没有窗就没有判据。
    pub fn is_empty(&self) -> bool {
        self.device_id.is_none() && self.app.is_none()
            && self.entity.is_none() && self.time_window.is_none()
    }
}

/// 读侧的表清单：`card` → `anchor`（anchor 放最后，它没有正文那一半形状）。
/// 两张可指认表。**别把它们叫成"两张有 `verdict` 列的表"**——`verdict` 只归
/// `card`，`anchor` 结构上没有那一列（DDL 与 OWNERSHIP 两处都对过表），
/// 所以 `anomaly::revived_despite_failed` 对 `anchor` 是早退而不是查空。
/// `facts` 与 `all_titles` 共用这一份，两处不许各写一遍清单；`anomaly::find_contradictions`
/// 与推导批的候选臂（`derive_pool_union`）是它的另外两类读者，所以这份清单现在有四个读者、
/// 一处定义。
/// 这份清单定的是**臂的拼法**（有哪几张表），不是吐出来的行序——行序由两条查询末尾共用的
/// `ORDER BY tbl, id` 定（见 `facts` 的文档），于是同一份库的读序与插入序无关。
/// `anomaly` 侧还有一层含义：扫描面**就是这两张表**，`fact_link` 那本账（用字面量
/// `mem_table='fact_link'` 写进 `retirement`）天然不在里面，不在这里做 filter，
/// 也不许在 SQL 里加 `mem_table <> 'fact_link'`——边的退场账绕开 `MemKey`，
/// 而 `MemTable` 没有边的 variant，"一条边既有退场账又有正面表态"在类型上写不出。
pub(crate) const ROW_TABLES: [MemTable; 2] = [MemTable::Card, MemTable::Anchor];

/// 一行的当前事实。字段全是 `pub`：任务 5 要用 `RowFacts { key, ..base.clone() }` 造候选，
/// 私有字段 + 构造器会把那条判据挤成"测试迁就封装"（方向反了）。
#[derive(Debug, Clone)]
pub struct RowFacts {
    pub key: MemKey,
    /// 行的写入钟，游标键；两张可指认表都有这一列。
    pub created_at_ms: i64,
    pub kind: String,
    pub title: String,
    /// `anchor` 行是空串：那张表没有正文列，`canonical` 就是它的全部可检索内容。
    /// `card` 行读 `text`（canonical 的消费行正文列）。
    pub body: String,
    pub as_of_ms: i64,
    pub valid_to_ms: Option<i64>,
    pub superseded_by: Option<String>,
    pub retired: bool,
    pub rejected: bool,
    pub verdict: Verdict,
    pub attestation: Option<Stance>,
    pub anchor_role: AnchorRole,
    pub cooldown_until_ms: Option<i64>,
    // §10（任务 10）：四过滤项的载体。`scope` 是事实行"属于谁"那一半（守卫 10 的归属那条），
    // `device_id` + `source_agent` 是事实行"在哪台机器、哪个 agent"那一半。
    pub scope: Scope,
    pub device_id: String,
    pub source_agent: String,
}

/// SELECT 的列序，也是 `Raw` 的元组序：
/// tbl, id, kind, title, body, as_of_ms, valid_to_ms, superseded_by, verdict,
/// cooldown_until_ms, scope_kind, scope_key, source_agent, device_id,
/// retired, rejected, attestation, anchor_role, created_at_ms
///
/// 两张可指认表都有 `scope_kind/scope_key` 真列；`source_agent`/`device_id` 只有 `card` 有，
/// `anchor` 那两格由 `select_sql` 的 `anchor` 支补空串。四列插在 `cooldown_until_ms`
/// 与四个判定标量之间而不是末尾，因为末尾那一段是 `judgement_cols` 一个函数返回的文本——
/// 列序即元组序，元组序即 `read_raw` 的下标序，三段必须逐位对齐，差一位就是"读到隔壁那一列"。
type Raw = (String, String, String, String, String, i64, Option<i64>, Option<String>, String,
            Option<i64>, String, String, String, String, i64, i64, Option<String>, String, i64);

/// 四个判定/出处标量子查询。外层表一律别名 `f`，内层表用短别名——
/// `attestation` 自己有一列叫 `id`，不点外层别名就会静默命中内层那个 `id`（假绿）。
fn judgement_cols(prefix: &str) -> String {
    let human = AnchorRole::Human.as_str();
    let agent = AnchorRole::Agent.as_str();
    let system = AnchorRole::System.as_str();
    let corroborated = Stance::Corroborated.as_str();
    let confirmed = Stance::Confirmed.as_str();
    format!(
        "(SELECT count(*) FROM retirement rt WHERE {}) AS retired,
         (SELECT count(*) FROM attestation aj WHERE {}) AS rejected,
         (SELECT ap.stance FROM attestation ap WHERE ap.mem_table='{prefix}'
                 AND ap.mem_id=f.id AND ap.resolved_at_ms IS NULL
                 AND ap.stance IN ('{corroborated}','{confirmed}')
                 ORDER BY CASE ap.stance WHEN '{corroborated}' THEN 0 ELSE 1 END,
                          ap.created_at_ms DESC, ap.id DESC LIMIT 1) AS attestation,
         (SELECT CASE WHEN EXISTS(SELECT 1 FROM provenance pv WHERE pv.mem_table='{prefix}' AND pv.mem_id=f.id
                                       AND pv.anchor_role='{human}') THEN '{human}'
                      WHEN EXISTS(SELECT 1 FROM provenance pv WHERE pv.mem_table='{prefix}' AND pv.mem_id=f.id
                                       AND pv.anchor_role='{agent}') THEN '{agent}'
                      ELSE '{system}' END) AS anchor_role",
        retirement_pred(prefix, "f.id"), rejection_pred(prefix, "f.id")
    )
}

/// 一张表的一段 SELECT。两段的列名列序完全一致，所以能 `UNION ALL`。
fn select_sql(table: MemTable) -> String {
    let prefix = table.prefix();
    let unreviewed = Verdict::Unreviewed.as_str();
    // anchor 那张表没有正文/时间/保质期/取代指针/verdict/冷却列（DDL 的列序是 canonical 的），
    // 这里用字面量补出同名列，于是读侧对两张表只有一套渲染代码。anchor 的标题列是 `canonical`。
    let (title, body, as_of, valid_to, superseded, verdict, cooldown, source_agent, device_id) = match table {
        MemTable::Anchor => (
            "f.canonical".to_string(),
            "''".to_string(),
            "0".to_string(),
            "NULL".to_string(),
            "NULL".to_string(),
            format!("'{unreviewed}'"),
            "NULL".to_string(),
            // anchor 表没有署名两列（canonical 的本体表不带 source_agent/device_id）：
            // 读侧补空串，署名证据留在 provenance 里。
            "''".to_string(),
            "''".to_string(),
        ),
        MemTable::Card => (
            "f.title".to_string(),
            "f.text".to_string(),
            "f.as_of_ms".to_string(),
            "f.valid_to_ms".to_string(),
            "f.superseded_by".to_string(),
            "f.verdict".to_string(),
            "f.cooldown_until_ms".to_string(),
            "f.source_agent".to_string(),
            "f.device_id".to_string(),
        ),
    };
    format!(
        "SELECT '{prefix}' AS tbl, f.id AS id, f.kind AS kind, {title} AS title,
                {body} AS body, {as_of} AS as_of_ms, {valid_to} AS valid_to_ms,
                {superseded} AS superseded_by, {verdict} AS verdict,
                {cooldown} AS cooldown_until_ms,
                f.scope_kind AS scope_kind, f.scope_key AS scope_key,
                {source_agent} AS source_agent, {device_id} AS device_id, {},
                f.created_at_ms AS created_at_ms
         FROM {} AS f",
        judgement_cols(prefix),
        prefix
    )
}

/// 19 个 `get`，下标 0..=18 逐位对上 `Raw` 的元组序（少一个就是"读到隔壁那一列"的静默错值：
/// 它不报错，只是把 `anchor_role` 那一列当成 `created_at_ms` 用，游标排序键凭空消失）。
fn read_raw(r: &rusqlite::Row<'_>) -> rusqlite::Result<Raw> {
    Ok((
        r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?,
        r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?, r.get(12)?, r.get(13)?, r.get(14)?,
        r.get(15)?, r.get(16)?, r.get(17)?, r.get(18)?,
    ))
}

/// 文本 → 枚举的两处都**不许 panic**：库里躺着读不出来的 slug 是历史数据（快照整替、
/// 阶段 2 之前的旧行），读它的人不该因为一行脏数据崩掉。脏 slug 落到该列的下界：
/// verdict ⇒ Unreviewed（没被中心判过），anchor_role ⇒ System（最弱的那一档）。
fn decode_raw(raw: Raw) -> Result<RowFacts, MemError> {
    let (tbl, id, kind, title, body, as_of_ms, valid_to_ms, superseded_by, verdict,
         cooldown_until_ms, scope_kind, scope_key, source_agent, device_id,
         retired, rejected, attestation, anchor_role, created_at_ms) = raw;
    let table = MemTable::parse(&tbl)
        .ok_or_else(|| MemError::Sql(format!("reader got an unknown mem_table slug: {tbl}")))?;
    Ok(RowFacts {
        key: MemKey { table, id },
        created_at_ms,
        kind,
        title,
        body,
        as_of_ms,
        valid_to_ms,
        superseded_by,
        retired: retired != 0,
        rejected: rejected != 0,
        verdict: Verdict::parse(&verdict).unwrap_or(Verdict::Unreviewed),
        attestation: attestation.as_deref().and_then(Stance::parse),
        anchor_role: AnchorRole::parse(&anchor_role).unwrap_or(AnchorRole::System),
        cooldown_until_ms,
        scope: Scope { kind: ScopeKind::parse(&scope_kind).unwrap_or(ScopeKind::Project), key: scope_key },
        device_id,
        source_agent,
    })
}

impl RowFacts {
    /// 这一行**现在还活着吗**——`alive_where` 的内存形（第五个读者：任务 12 的重复提示）。
    ///
    /// 三支一支都不许多，也一支都不能少，而这一层**不重新拼任何条件**：三支的条件分别住在
    /// `retirement_pred`、`rejection_pred` 与 `select_sql` 的 `superseded` 那一格，这里读的
    /// 是它们已经算好的三列读数。所以守卫 3 的"一个家"是按**条件**算的，不是按表达式算的。
    ///
    /// `entity` 一个分岔都不需要：`select_sql` 给它的 `superseded_by` 是字面量 `NULL`
    /// （那张表没有这一列），于是 `superseded_by.is_none()` 对实体行恒真——这与 `alive_where`
    /// 里那句 `if table == Entity { "1=1" }` 是同一件事的两种写法。它们同口径吗？
    /// 那条对撞用例是 `tests/candidate_read.rs` 的
    /// `the_in_memory_alive_predicate_agrees_with_the_sql_predicate`。
    pub fn alive(&self) -> bool {
        !self.retired && !self.rejected && self.superseded_by.is_none()
    }
}

/// 全部行（card + anchor 两张表）。相关性顺序不在这一层（那是任务 5 的 `rank`），
/// 这里只保证**同一份库两次读出来一样**——`UNION ALL` 的分段顺序 SQLite 不合同保证，
/// 所以这句承诺的载体是拼接完再挂上去的 `ORDER BY tbl, id`，不是臂序本身。
pub fn facts(m: &MemoryHandle) -> Result<Vec<RowFacts>, MemError> {
    let sql = ROW_TABLES.iter().map(|t| select_sql(*t)).collect::<Vec<String>>()
        .join(" UNION ALL ") + " ORDER BY tbl, id";
    let mut st = m.conn().prepare(&sql)?;
    let rows = st.query_map([], read_raw)?;
    let mut out = Vec::new();
    for raw in rows {
        out.push(decode_raw(raw?)?);
    }
    Ok(out)
}

/// 一行的参与者正式名（推导面用）。`card` 行沿 `pointers_json` 找到它的 `fact`，
/// 读 `fact.participants_json` 里的锚 id，再映射成 `anchor.canonical`。
///
/// 为什么从这里取而不加进 `RowFacts`：参与者是**事实/事件结构**的一部分，检索面
/// （`rc-search`/`rc-agent`）不消费它；塞进 `RowFacts` 会让每个检索读者多背两列，
/// 而推导是唯一消费者。`anchor` 行没有参与者概念 ⇒ 空；指针坏/锚已删 ⇒ 少不多，不编造。
pub fn row_participants(m: &MemoryHandle, row: &RowFacts) -> Result<Vec<String>, MemError> {
    if row.key.table != MemTable::Card { return Ok(Vec::new()); }
    let pointers = match m.conn().query_row(
        "SELECT pointers_json FROM card WHERE id=?1", rusqlite::params![row.key.id],
        |r| r.get::<_, String>(0)) {
        Ok(p) => p,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(Vec::new()),
        Err(e) => return Err(MemError::from(e)),
    };
    let mut ids: Vec<String> = Vec::new();
    for pointer in json_array_strings(&pointers) {
        let Some(fact_id) = pointer.strip_prefix("fact:") else { continue; };
        let stored = match m.conn().query_row(
            "SELECT participants_json FROM fact WHERE id=?1", rusqlite::params![fact_id],
            |r| r.get::<_, String>(0)) {
            Ok(v) => v,
            Err(rusqlite::Error::QueryReturnedNoRows) => continue,
            Err(e) => return Err(MemError::from(e)),
        };
        for id in json_array_strings(&stored) {
            if !ids.contains(&id) { ids.push(id); }
        }
    }
    // D8 方案二：裸 id 先归到链终点再取名（Broken 回落原 id=旧锚自己的行，可见不静默；
    // Missing 与既有行为同形——查无此锚就少一个，不编造）。
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let target = redirect_terminal(m, &id)?;
        if out.contains(&target) { continue; }
        match m.conn().query_row(
            "SELECT canonical FROM anchor WHERE id=?1", rusqlite::params![target],
            |r| r.get::<_, String>(0)) {
            Ok(name) => out.push(name),
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(e) => return Err(MemError::from(e)),
        }
    }
    Ok(out)
}

/// 按句柄取一行。查不到就是 `None`——**不许**退化成"最接近的那条"（§6 连带②），
/// 否则宿主 agent 会以为自己确认过某件事。
pub fn lookup(m: &MemoryHandle, key: &MemKey) -> Result<Option<RowFacts>, MemError> {    let sql = format!("{} WHERE f.id=?1", select_sql(key.table));
    let mut st = m.conn().prepare(&sql)?;
    let mut rows = st.query_map(rusqlite::params![key.id], read_raw)?;
    match rows.next() {
        Some(raw) => Ok(Some(decode_raw(raw?)?)),
        None => Ok(None),
    }
}

/// 判同与审核的候选表清单：`card` 一张（旧三候选表合并后的唯一消费行表）。
/// `anchor` 不在这里，三条理由：那张表没有 `verdict` 也没有 `superseded_by` 列
/// （`writers_c::fact_table` 对它的这两支直接 `NotFound`）；`select_sql(Anchor)` 给 `verdict`
/// 补的是**字面量** `'unreviewed'`，于是任何"全表找未审"的查询会把每条锚当未审拉进来；
/// 锚归并的正确形状是 `alias`/`anchor_merge`，已见 `merge.rs`/`redirect.rs`。
/// 这份清单与 `ROW_TABLES` 是两份，不是"少一张的 `ROW_TABLES`"：
/// 前者定两臂 UNION 的拼法，后者定"可审可并"的业务集合，两者下一次会因不同的理由被改。
/// 它挂在 `pub` 上而不是模块私有：当前读者是 `rows_in_scope`、`unreviewed_rows`
/// 与 `directory_sql`——非 `pub` 的常量在"定义了但本层还没人读"
/// 的那一刻就被 `dead_code` 判死，而那条 lint 在这里否掉的恰恰是这份清单存在的理由。
pub const CANDIDATE_TABLES: [MemTable; 1] = [MemTable::Card];

/// "这一行有没有未解决的退场账"的**条件体**，外层由 `id_ref` 指认（读者 `"f.id"`、写侧 `"?1"`）。
/// 条件只能有一家（守卫 3）：它漂移的那一天，`retired` 这一列与候选集合会给出相反的答案，
/// 而那正是 P-31 说的"库里删掉的东西又长回来"的读数形状。
/// `prefix` 是**字面量表名**（`retirement.mem_table` 就等于它），`id_ref` 是外层指认。
fn retirement_pred(prefix: &str, id_ref: &str) -> String {
    retirement_pred_on("rt", &format!("'{prefix}'"), id_ref)
}

/// 同一条条件的第二种外层形状：外层的表名可以是**列**（`provenance` 自己的 `mem_table` /
/// `mem_id` 两列，见族①），也可以是**字面量**（族② 的 `'relationship'`）。段 0 两族的过滤
/// 都走这一支——那一族扫的是 `provenance` 与 `relationship`，被过滤的正是"这一行自己有没有
/// 未解决的退场账"，而两行各自的句柄拼法不同（两列 vs 一个 id）。
///
/// `pub(crate)` 而不是 `pub`：读者只有同 crate 的 `orphans.rs`，而集成测试看的是
/// `suppress_orphans` 的三格与 `retirement` 表，不需要这个函数本身。**不进 crate 根 re-export**
/// （同 `alive_where` 的理由：根导出会把"条件文本只有一个家"说成公开承诺）。
/// 拆成两个函数而不是给 `retirement_pred` 加一个 `Option`：既有三个读者传的都是字面量表名，
/// 混成一支会让"外层是字面量"这个不变量散掉——而那个不变量正是它能一字段替换、
/// 不必把条件文本抄第二份的原因。
pub(crate) fn retirement_pred_on(rt_alias: &str, table_ref: &str, id_ref: &str) -> String {
    format!("{rt_alias}.mem_table={table_ref} AND {rt_alias}.mem_id={id_ref} AND {rt_alias}.resolved_at_ms IS NULL")
}

/// "这一行有没有未解决的否决表态"的条件体。别名 `aj` 与 `judgement_cols` 里那个一致。
/// `id_ref` 的取舍同 `retirement_pred`。
fn rejection_pred(prefix: &str, id_ref: &str) -> String {
    let rejected = Stance::Rejected.as_str();
    format!("aj.mem_table='{prefix}' AND aj.mem_id={id_ref} AND aj.stance='{rejected}' \
             AND aj.resolved_at_ms IS NULL")
}

/// 这一行**有没有 turn 出处**（条件体；`EXISTS` 的壳由调用方给——与 `retirement_pred`
/// 同形，`id_ref` 的取舍也同：读者用 `"f.id"`）。调用方包上 `EXISTS` 之后读作
/// "这一行是抽取写的" ⇒ 保留在推导批的候选臂里。
///
/// **壳必须是 `EXISTS`，不许是 `NOT EXISTS`**（E37 记：这一格本轮真踩过一次，写反了）。
/// 条件体描述的是"有 turn 出处"这个**事实**，不是它的否定 ⇒ 配 `NOT EXISTS` 读作
/// "这行没有 turn 出处"，于是**抽取行被排除、推导行被全保留**——与本任务要做的正相反
/// （三条用例一起红，实施者按纪律停在 Step 2 把它交回控制器裁定）。
/// 对照 `alive_where` 里那处 `NOT EXISTS (… retirement_pred …)`：那边条件体是"已退场"，
/// 壳取否定才读作"没退场 ⇒ 活" ⇒ **两处的 `NOT` 不能照抄，方向天然相反**。
///
/// **判别列是 `provenance.ref_kind` 而不是事实表自己的 `source_agent`**，理由是一条可伪造性：
/// `source_agent` 在事实表与 `sessions` 表同名不同物，而 `BatchCtx::source_agent` 取自
/// `sessions` 那一列——那是**客户端自报家门**（`rc-mcp` 的 initialize 字段）。
/// 一个自称 `rc-derive` 的会话会写出与推导行逐字同形的行，拿它当判据等于把判据交给外部数据。
/// `ref_kind` 不可能伪造：全仓只有两个写入点——抽取路径**不写该列**（走 DDL 默认 `'turn'`），
/// 推导路径显式写依据行所在表的 `prefix()`。取值由**写入路径**决定，不由外部输入决定。
///
/// **为什么条件体找的是 `= 'turn'` 而不是 `<> 'turn'`**（两条形状的区别是本任务的核心）：
/// `put_timeline_rows` 的 provenance 是 `if let Some(anchor_id)` 才写的 ⇒ **无锚的推导行
/// provenance 零行**。写 `<> 'turn'` 的话零出处行一条 `ref_kind` 都没有，谓词恒不成立
/// ⇒ 那批行照进推导批——而 T4 评审 I-1 描述的毒化路径入口**正是零出处行**（模型对它产边 ⇒
/// `anchor_role_of` 查不到出处）。写 `= 'turn'` 则三种形状一并覆盖：抽取行有 turn 出处（保留）、
/// 带锚推导行只有非 turn 出处（排除）、无锚推导行零出处（排除）。
/// 附带的 fail-closed 方向：一张 provenance 全空的行（既没有 turn 也没有别的出处）会被当推导行
/// 排除在批外——那是本仓一贯的方向（宁可少一行进批，不要一条没有出处的结论继续往下传）。
fn turn_basis_pred(prefix: &str, id_ref: &str) -> String {
    format!("dp.mem_table='{prefix}' AND dp.mem_id={id_ref} AND dp.ref_kind = 'turn'")
}

/// "这一行没被判定同顶掉"的条件体，**含 anchor 的分岔**（那张表没有 `superseded_by` 列，
/// 而顶替本来就只发生在有行的地方，所以对锚恒真是事实而不是敷衍）。
/// 分岔住在这里面，`alive_where` 与 `dead_by_id_expr` 两个形状因此都不必知道 anchor 的特殊性。
fn superseded_pred(table: MemTable) -> String {
    if table == MemTable::Anchor { "1=1".to_string() } else { "f.superseded_by IS NULL".to_string() }
}

/// 一行"现在还活着吗"的谓词，**读者的形状**（P-31）：`rows_in_scope` 的候选筛、
/// `unreviewed_rows`（任务 8）的游标筛。三支一支都不能少：漏退场 ⇒ 票 1 判死的行重蒸后复活；
/// 漏否决 ⇒ 未解决否决的行照旧被审；漏 `superseded_by` ⇒ 被判同顶掉的行重蒸后复活。
/// 三种都不报错。写侧不用这一支，用下面的 `dead_by_id_expr`（理由就写在那一格的注释里）。
///
/// 它是 `pub` 却**不进 crate 根 re-export**（见 `lib.rs` 那一行）：测试 target 是独立 crate，
/// `pub(crate)` 会把它们挡在外面，而根导出会把"活行谓词只有一个家"这件事说成公开承诺。
pub fn alive_where(table: MemTable) -> String {
    let p = table.prefix();
    format!("NOT EXISTS (SELECT 1 FROM retirement rt WHERE {}) \
             AND {} \
             AND NOT EXISTS (SELECT 1 FROM attestation aj WHERE {})",
            retirement_pred(p, "f.id"), superseded_pred(table), rejection_pred(p, "f.id"))
}

/// 锚"还在认领名字"要补的那半句（W29），住这一家给 Q2 与 Q3 两支共用：
/// `alive_where` 只筛三本账，**不看 `status`**；而 redirect 合并零搬运（旧锚行与旧锚别名都留着，
/// 只盖 `status='merged'`）⇒ 已合并的旧锚会继续在同名/同份别名这两族里"认领"名字，
/// 每次合并长出一族永久告警。`status` 闭集见 DDL：`active/provisional/merged/retired`，
/// 其中前两个仍算在世，后两个不认领。别名是 `f`：两支 SQL 的事实行别名都是 `f`。
const ANCHOR_STATUS_CLAIMS_NAME: &str = " AND f.status NOT IN ('merged','retired')";

/// 一条内容"该不该被抑制"的表达式，**写侧的形状**（`writers_x::put_batch` 是它唯一的读者）：
/// 两本账（未解决退场、未解决否决）按绑进来的 id 查、**与行在不在无关**，取代那一支必须与行同查
/// （`superseded_by` 是行上的列，幽灵 id 没有行也就没有顶替这回事）。
/// 三支条件文本仍与 `alive_where` 各住同一家（守卫 3），这里的差别只有指认方式：`?1` 而不是 `f.id`。
///
/// 为什么不能直接拿 `alive_where` 数行（那正是它最初被写成 `count(*) … AND NOT (alive_where)` 的形状）：
/// `retire` 与 `attest` 都不校验行存在（`dangling_attestations` 这一格读数观察的就是这个状态），
/// 而 `compute_mem_id` 是公开的内容寻址 ⇒ "一张账、零行"是公共 API 走得到的形状。按行筛就查不到那张账，
/// 守卫 13 的复活窗口随之重开——不是理论问题，是这一支的语义本身。
/// 同样不改成"让 `retire` / `attest` 去校验行存在"：那会关掉 `dangling_attestations` 的唯一生产者，
/// 把一个既有读数变成恒零（悬空表态从"可观察"变成"不可能"），而架构 §6 说的是它**存在**、要被人看见。
///
/// 两本账的**解除路径不对等**，这一格要说清：退场账有 `writers_c::resolve_retirement`（按
/// `(mem_table, mem_id)` 解、与行在不在无关 ⇒ 幽灵账同样解得掉，`tests/put_batch.rs` 有那一半用例）；
/// 否决账的 `attestation.resolved_at_ms` 本阶段**没有任何写入口**（全树只有退场那一支 UPDATE 它），
/// 所以一条落在幽灵 id 上的 `rejected` 会永久抑制这一份内容。方向选的是 fail-closed：抑制在返回的
/// 账本上是可见的 `None` 一格（可数、能升成读数），复活是静默的。日后要给否决加解除，
/// 加的是 `writers_c` 里一支与 `resolve_retirement` 同形的 fn，而不是在这里把否决那支改回行作用域。
pub fn dead_by_id_expr(table: MemTable) -> String {
    let p = table.prefix();
    format!("EXISTS (SELECT 1 FROM retirement rt WHERE {}) \
             OR EXISTS (SELECT 1 FROM attestation aj WHERE {}) \
             OR EXISTS (SELECT 1 FROM {} AS f WHERE f.id=?1 AND NOT ({}))",
            retirement_pred(p, "?1"), rejection_pred(p, "?1"), p, superseded_pred(table))
}

/// 同表、同归属、**同归属人**、除自己之外的活行。没有 LIMIT（§2.15 第 3 条）：
/// 候选的截断发生在算分之后（`MAX_DEDUPE_CANDIDATES`），在比之前丢行 = 漏并。
/// 读宽度由归属本身界定（一个归属下的行数）。本阶段不给它加上界——
/// 阶段 4 的容量格子若报出"单归属行数异常"，跟着改的是这一句注释，不是别处的代码。
///
/// **为什么"同 scope"后面还要"同归属人"**：判同一旦成立就写 `superseded_by`，
/// 一行**永久**顶掉另一行。同一个 project scope 下两个 agent 各记一条"部署在十点"
/// 并不是同一条记忆，跨 owner 归并会把其中一方的行连同出处一起埋掉，而另一方白得一条。
/// 归属对 `resource_owner` 的那一对 `(owner_principal_id, owner_agent_id)` 判同，两条轴口径不同
/// （C5c-fix）：
/// - **agent 轴严格相等**——这是防跨 agent 误并的那一半，不动；
/// - **principal 轴放宽**：`NULL` 是"未知"，不是"另一个 principal" ⇒ 任一家为 NULL 时
///   只比 agent，两家都非空才要求相等。严格 `IS` 让中心库里 151 条 principal 为 NULL 的
///   行与 65 条 `'p1'` 的行互不可见——判同与票 3 因此看不到七成邻居。
///
/// 探针自己（`?3`）没有归属行时 `?6=0` ⇒ 一条邻居都不返回：fail-closed，与锚候选池
/// "没有归属证据的不进候选"同向。
///
/// **查询形状**（C5c-fix）：探针归属在外层**只读一次**，三格绑 `?4` agent / `?5` principal /
/// `?6` 是否有归属；SQL 里不再有逐行标量子查询（旧形状对每一行跑两个相同的子查询，
/// 语义与上面两条轴逐字等价）。
pub fn rows_in_scope(
    m: &MemoryHandle,
    table: MemTable,
    scope: &Scope,
    exclude_id: &str,
) -> Result<Vec<RowFacts>, MemError> {
    let prefix = table.prefix();
    // 探针归属：**一次**查询，不再让每一行各跑两个标量子查询。
    // 读不到归属行 ⇒ 三格落在 `(NULL, NULL, 0)`，`?6=1` 让 SQL 自己就是空的：
    // fail-closed 由谓词承担，不靠 Rust 侧提前 return 假装同一条规则。
    let probe_owner: Option<(String, Option<String>)> = match m.conn().query_row(
        &format!("SELECT owner_agent_id, owner_principal_id FROM resource_owner \
                  WHERE mem_table='{prefix}' AND mem_id=?1"),
        rusqlite::params![exclude_id],
        |r| Ok((r.get(0)?, r.get(1)?))) {
        Ok(row) => Some(row),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(MemError::from(e)),
    };
    let (agent, principal, has_owner): (Option<String>, Option<String>, i64) = match probe_owner {
        Some((agent, principal)) => (Some(agent), principal, 1),
        None => (None, None, 0),
    };
    let sql = format!(
        "{} WHERE f.scope_kind=?1 AND f.scope_key=?2 AND f.id<>?3 AND {} \
           AND ?6=1 \
           AND EXISTS (SELECT 1 FROM resource_owner AS own \
                        WHERE own.mem_table='{prefix}' AND own.mem_id=f.id \
                          AND own.owner_agent_id = ?4 \
                          AND (own.owner_principal_id IS NULL OR ?5 IS NULL \
                               OR own.owner_principal_id = ?5))",
        select_sql(table), alive_where(table));
    let mut st = m.conn().prepare(&sql)?;
    let rows = st.query_map(
        rusqlite::params![scope.kind.as_str(), scope.key, exclude_id, agent, principal, has_owner],
        read_raw)?;
    let mut out = Vec::new();
    for raw in rows {
        out.push(decode_raw(raw?)?);
    }
    Ok(out)
}

/// 审核游标：库里"还没被中心判过、而且还活着"的行。一张候选表（card），`anchor` 进不来（坑 P-30：
/// `select_sql(Anchor)` 的 `verdict` 是字面量 `'unreviewed'`，任何全表未审查询都会把每条锚当未审）。
/// **推导写的行同样不在游标里**（每臂加一道 `EXISTS (<turn_basis_pred(...)>)`，与
/// `derive_pool_union` 同一支谓词）：它是别的 pass 的结论，不该再花一次判定额度。
/// 顺序 = `(as_of_ms, tbl, id)`：审核按** oldest-first** 推进，所以这条顺序就是"谁先被审"的定义，
/// 而 `MAX_PER_PASS` 之外的那些行留在原地等下一次 pass（与判同的 A16 不同——判同的探针是**本轮批次**，
/// 错过了就没有下一次；审核的游标是**库的状态**，审过的行永久离开）。
/// 活行谓词的读者之二（P-31）：与 `rows_in_scope` 共用 `alive_where` 这一支（三支齐全：退场账 /
/// 未被顶掉 / 未解决否决，全部相关到 `f.id`）。**写侧不在这一列里**：它读 `dead_by_id_expr`——
/// 同一批条件文本、另一种作用域（两本账按绑参 id、取代那支按行）。把这两个入口合并回去的诱惑
/// 就是坑 P-76 那一次回归的来源，游标若跟着按行筛，幽灵账那一半会静默失效。
/// `?1` 在三段臂里各出现一次，SQLite 按编号绑定 ⇒ 同一个 slug 绑三次是**正确**而不是重复；
/// `?2` 给 `LIMIT`，所以它是 i64。`limit as i64` 这一转在 `usize::MAX` 附近会溢出成负数——
/// `MAX_PER_PASS = 4` 的调用点碰不到，但 `limit` 因此不是任意值。
pub fn unreviewed_rows(m: &MemoryHandle, limit: usize) -> Result<Vec<RowFacts>, MemError> {
    let unreviewed = Verdict::Unreviewed.as_str();
    let arms: Vec<String> = CANDIDATE_TABLES.iter()
        .map(|t| format!("{} WHERE f.verdict=?1 AND {} AND EXISTS (SELECT 1 FROM provenance dp WHERE {})",
                         select_sql(*t), alive_where(*t), turn_basis_pred(t.prefix(), "f.id")))
        .collect();
    let sql = format!("{} ORDER BY as_of_ms, tbl, id LIMIT ?2", arms.join(" UNION ALL "));
    let mut st = m.conn().prepare(&sql)?;
    let rows = st.query_map(rusqlite::params![unreviewed, limit as i64], read_raw)?;
    let mut out = Vec::new();
    for raw in rows {
        out.push(decode_raw(raw?)?);
    }
    Ok(out)
}

/// 一行的归属。判同与审核的入口只有 `MemKey`（`dedupe.rs` 的票、`audit.rs` 的邻居），
/// 手上没有整行 `RowFacts`，所以这个窄读口留着——任务 10 之后 `RowFacts` 自己也带
/// `scope` 了，两条路各有消费者，不是重复。
/// 脏 slug 落到 `Project`：与 `decode_raw` 同一族下界策略（读侧不许因为一行脏数据崩掉）。
pub fn scope_of(m: &MemoryHandle, key: &MemKey) -> Result<Scope, MemError> {
    let sql = format!("SELECT scope_kind, scope_key FROM {} WHERE id=?1", key.table.prefix());
    let (kind, k): (String, String) = m.conn().query_row(&sql, rusqlite::params![key.id],
        |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => MemError::NotFound(key.clone()),
            other => MemError::from(other),
        })?;
    Ok(Scope { kind: ScopeKind::parse(&kind).unwrap_or(ScopeKind::Project), key: k })
}

/// anchor 名字 → anchor_id 解析（守卫 10 的入口）：**别名优先、canonical 兜底**，
/// 都用 `=` 比较、不做 case folding（记忆库本来不 normalize 这两列）。
/// 名字命中多条返回多条（同名锚），过滤侧按"任一匹配即可"取用。
/// `pub`：rc-agent 的 `recall_filter_match` 跨 crate 调它（rank 之后、内存侧过滤）。
///
/// 偏离 brief 的一处（必要编译修正）：brief 原文
/// `if !ids.contains(&r?) { ids.push(r?); }` 是 E0382（`r` 是 `Result<String, _>`，
/// `?` 按值吞掉它，同一表达式里再用一次即"用已移动的值"）。先把值绑出来再去重，
/// 去重口径与语义逐位相同（仍是"不在 `ids` 里才 push"）。
///
/// **W14c 起三个来源**：`alias` ∪ `anchor.canonical` ∪ `attribute.value_text`（只认活行）。
/// 第三个来源是账号昵称这类"不是实体、是某个环境的属性值"的词——不并入这一格，它们
/// 不建锚就查不到（W14a 已把账号形状的 person 锚拒收）。语义仍是"返回锚 id 集"：
/// 属性命中返回**属性所在的锚**（环境锚），调用方（`entity=` 过滤、mention 解析）
/// 因此把它当"命中了该锚"用。
///
/// 排序也变了（canonical 在前、alias 次之、属性最后）：旧实现在 alias 之后才查 canonical。
/// 返回值是**集合语义**（两处调用方都按"任一命中"用、测试都先 `sort`），但它不是承诺的序。
pub fn entity_ids_for_name(m: &MemoryHandle, name: &str) -> Result<Vec<String>, MemError> {
    let mut ids = Vec::new();
    for hit in resolve_surface(m, name)? {
        if !ids.contains(&hit.anchor_id) {
            ids.push(hit.anchor_id);
        }
    }
    Ok(ids)
}

/// 一次"这个词指向谁"的解析结果（W14c）。
///
/// `via` 是**来源**而不是描述：`canonical` / `alias` / `attribute:<key>`（键表见
/// [`crate::writers_attr::ATTRIBUTE_KEYS`]）。`role` 只有属性命中才有——一个 canonical
/// 或 alias 命中"它是谁"由锚自己回答，没有"是什么角色"这一层。
///
/// 为什么把来源交出去而不是只回 id 集：用户要的答案是
/// "`甲某随行` 是 `示例主人` 的 B 站账号昵称（环境 `example-host`）"——只有 id 集的话，
/// 读侧只能猜"这个网格子是哪一类的"。（`entity=` 过滤仍只要 id：它问的是"命中没命中"。）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceHit {
    pub anchor_id: String,
    /// `canonical` / `alias` / `attribute:<key>`。
    pub via: String,
    /// 属性键表里的中文角色名（如 `B站账号昵称`）；非属性命中为 `None`。
    pub role: Option<&'static str>,
}

fn push_hit(out: &mut Vec<SurfaceHit>, anchor_id: String, via: String, role: Option<&'static str>) {
    if out.iter().any(|h| h.anchor_id == anchor_id && h.via == via) {
        return;
    }
    out.push(SurfaceHit { anchor_id, via, role });
}

/// 一个词指向哪些锚、各自经由哪条来源（W14c 的读路径唯一入口）。
///
/// 三处来源按**优先级**收集：`anchor.canonical` → `alias` → `attribute.value_text`；
/// 同一 `(锚, 来源)` 只出现一次。**同一词命中多个锚 ⇒ 全部返回**（两台机器都写
/// `brand=Acer`、或者一个真叫这个昵称的人与环境同时命中），各自带来源与角色，
/// **不由这一层选一个**——选一个是身份层的裁定，不是解析层的。
///
/// 属性那一支只认**活行**（`valid_to_ms IS NULL`）：账号昵称改了之后，旧昵称不再回答
/// "现在是哪个账号"（历史仍在表里，`valid_to_ms` / `superseded_by` 指得出后继）。
///
/// 空词/空白词 ⇒ 空结果（不查库、不报错）：`""` 不是任何一个名字。
pub fn resolve_surface(m: &MemoryHandle, surface: &str) -> Result<Vec<SurfaceHit>, MemError> {
    let needle = surface.trim();
    if needle.is_empty() {
        return Ok(Vec::new());
    }
    let mut out: Vec<SurfaceHit> = Vec::new();
    // ① canonical：锚自己的名字（最先，因为它是最强的一档）。
    let mut st = m.conn().prepare("SELECT id FROM anchor WHERE canonical=?1 ORDER BY id")?;
    let ids: Vec<String> = st
        .query_map(rusqlite::params![needle], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for id in ids {
        let id = redirect_terminal(m, &id)?;
        push_hit(&mut out, id, "canonical".to_string(), None);
    }
    // ② alias：旧称呼/昵称（同一格锚，带有效期分段）。
    let mut st = m.conn().prepare("SELECT anchor_id FROM alias WHERE alias=?1 ORDER BY anchor_id")?;
    let ids: Vec<String> = st
        .query_map(rusqlite::params![needle], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for id in ids {
        let id = redirect_terminal(m, &id)?;
        push_hit(&mut out, id, "alias".to_string(), None);
    }
    // ③ attribute：环境/账号属性值（W14c 的第三个来源）。
    let mut st = m.conn().prepare(
        "SELECT anchor_id, key FROM attribute
          WHERE value_text=?1 AND valid_to_ms IS NULL
          ORDER BY anchor_id, key")?;
    let rows: Vec<(String, String)> = st
        .query_map(rusqlite::params![needle], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    for (id, key) in rows {
        let id = redirect_terminal(m, &id)?;
        let role = crate::writers_attr::attribute_role(&key);
        push_hit(&mut out, id, format!("attribute:{key}"), role);
    }
    Ok(out)
}

/// 一条"查询词是某个环境的属性值"的答案原料（W14c）。
///
/// `environment` 是环境锚的 `canonical`（`example-host` 这类主机名）；
/// `owner_*` 是**归属**那一侧：`resource_owner.owner_principal_id` 指向主人，再由
/// [`display_name`] 取主人锚现在被叫的名字（W14b 的契约：主人锚 `ro.device_id IS NULL`，
/// 找回它的唯一形状是 [`crate::writers_v4::session_principal_anchor_id`]）。
/// 主人锚不在（没盖过章/没建锚）⇒ `owner_name` 回落到 `principal_id` 本身，仍不猜人。
///
/// 两格 id（`anchor_id` / `owner_anchor_id`）是 **W17 的读闸判据**，不参与渲染：
/// 读者看不到环境锚或看不到主人锚时，这一条整条不出声（存在性本身也在隐私面内）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeOwner {
    /// 命中的属性值（库里那一格，不是调用方递进来的原文）。
    pub surface: String,
    /// 中文角色名（`B站账号昵称` 等）。
    pub role: &'static str,
    /// 属性所在环境锚的 `canonical`。
    pub environment: String,
    /// `resource_owner.owner_principal_id`；锚没有归属行 ⇒ `None`。
    pub owner_principal_id: Option<String>,
    /// 主人锚的显示名；没有主人锚时回落 `owner_principal_id`；两者都没有 ⇒ `None`。
    pub owner_name: Option<String>,
    /// 属性所在那一格锚（环境锚）的 id。只给读闸判定用，不渲染（W17）。
    pub anchor_id: String,
    /// 主人锚的 id；`owner_principal_id` 有、锚没建出来 ⇒ `None`。只给读闸判定用，不渲染（W17）。
    pub owner_anchor_id: Option<String>,
}

/// 把"这个词是某个环境的属性值"展开成可渲染的归属行（W14c 的工具面原料）。
///
/// 与 [`resolve_surface`] 同一把尺子（不另查一遍 canonical/alias）：只取属性来源的命中，
/// 每条补上环境名与主人。**不做歧义裁定**：命中几个环境就返回几条，各自带角色。
///
/// 归属那一侧同时交出两格锚 id（W17）：环境锚就是属性所在的那一格（`hit.anchor_id`），
/// 主人锚取 [`crate::writers_v4::session_principal_anchor_id`] **已经算出来的那个**——
/// 显示名与闸判据读的是同一次查找，不为闸再查第二遍。
pub fn attribute_owners(m: &MemoryHandle, surface: &str) -> Result<Vec<AttributeOwner>, MemError> {
    let needle = surface.trim();
    if needle.is_empty() {
        return Ok(Vec::new());
    }
    let mut out: Vec<AttributeOwner> = Vec::new();
    for hit in resolve_surface(m, needle)? {
        let Some(key) = hit.via.strip_prefix("attribute:") else { continue };
        let Some(role) = crate::writers_attr::attribute_role(key) else { continue };
        // 环境名：属性所在锚的 canonical（锚一定在——属性有 FK）。
        let environment: String = m
            .conn()
            .query_row("SELECT canonical FROM anchor WHERE id=?1", rusqlite::params![hit.anchor_id],
                       |r| r.get(0))
            .map_err(MemError::from)?;
        // 属性值取库里那一格（读侧不把调用方递进来的原文当显示值）。
        let value: String = m
            .conn()
            .query_row(
                "SELECT value_text FROM attribute
                  WHERE anchor_id=?1 AND key=?2 AND valid_to_ms IS NULL
                  ORDER BY created_at_ms, id LIMIT 1",
                rusqlite::params![hit.anchor_id, key],
                |r| r.get(0),
            )
            .map_err(MemError::from)?;
        let owner_principal_id: Option<String> = m
            .conn()
            .query_row(
                "SELECT owner_principal_id FROM resource_owner
                  WHERE mem_table='anchor' AND mem_id=?1",
                rusqlite::params![hit.anchor_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let (owner_name, owner_anchor_id) = match owner_principal_id.as_deref() {
            None => (None, None),
            Some(principal) => {
                let anchor_key = MemKey { table: MemTable::Anchor, id: hit.anchor_id.clone() };
                let scope = scope_of(m, &anchor_key)?;
                match crate::writers_v4::session_principal_anchor_id(m, principal, &scope)? {
                    Some(principal_anchor) => {
                        let name = display_name(m, &principal_anchor)?;
                        (Some(name), Some(principal_anchor))
                    }
                    None => (Some(principal.to_string()), None),
                }
            }
        };
        out.push(AttributeOwner {
            surface: value,
            role,
            environment,
            owner_principal_id,
            owner_name,
            anchor_id: hit.anchor_id.clone(),
            owner_anchor_id,
        });
    }
    Ok(out)
}

/// 一个锚的名字面：`canonical` + `alias` 表的全部别名（按 `alias` 升序）。
///
/// 闸门要的是"这个词表长什么样"，不是目录的邻居读数：[`entity_directory`] 会为全库算
/// provenance 邻居，按单锚取名字不该付那份代价，所以这里是一条只读的定点查询。
/// 锚行不在 ⇒ `None`（码由调用方按自己的判定序给，本函数不造判定码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorNames {
    pub canonical: String,
    /// 按 `alias` 升序。别名集合是词表不是读数，排序只求"两次调用给同一串字节"。
    pub aliases: Vec<String>,
}

/// 按 id 取锚的名字面。[`entity_ids_for_name`] 的反方向（名字 → id 的那一支）。
///
/// D8 方案二：id 先经 [`redirect_terminal`] 归到链终点——被合并旧锚的名字面按**幸存锚**
/// 渲染（"渲染归属 to 锚"）；Broken（断链）回落旧锚自己的行（可见，不静默）；
/// Missing 仍是 `None`（与既有行为逐位相同）。
pub fn anchor_names(m: &MemoryHandle, anchor_id: &str) -> Result<Option<AnchorNames>, MemError> {
    let target = redirect_terminal(m, anchor_id)?;
    let canonical = m.conn().query_row(
        "SELECT canonical FROM anchor WHERE id=?1",
        rusqlite::params![target],
        |r| r.get::<_, String>(0));
    let canonical = match canonical {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(MemError::from(e)),
    };
    let mut st = m.conn().prepare(
        "SELECT alias FROM alias WHERE anchor_id=?1 ORDER BY alias")?;
    let mut aliases = Vec::new();
    for r in st.query_map(rusqlite::params![target], |r| r.get::<_, String>(0))? {
        aliases.push(r?);
    }
    Ok(Some(AnchorNames { canonical, aliases }))
}

/// 一格锚的**显示名**：最新的有效别名优先，没有别名才回落 `canonical`。
///
/// 为什么要有这一格：`canonical` 是**身份字形**，主人锚那格现在恒等于账号主键
/// （`writers_v4::principal_anchor_canonical`，"改名不换锚"），给人/给 LLM 看的一面若直接用
/// 它，消费句就变成一串 UUID——违反"LLM 面用自然语言"那条契约。
/// 所以身份与显示分两格：身份不可变、可追溯；显示名跟着人现在被叫什么走。
///
/// 取法：只认未关段的别名（`valid_to_ms IS NULL`），同刻并列时按别名字节序倒序
/// （稳定、可复算，不做"多数票"那类裁定）。全被关段了才回落 `canonical`。
pub fn display_name(m: &MemoryHandle, anchor_id: &str) -> Result<String, MemError> {
    // D8 方案二：先归到链终点（Broken 回落原 id=旧锚自己的名字面，可见不静默）。
    let target = redirect_terminal(m, anchor_id)?;
    let mut st = m.conn().prepare(
        "SELECT alias FROM alias WHERE anchor_id=?1 AND valid_to_ms IS NULL \
         ORDER BY COALESCE(valid_from_ms, 0) DESC, alias DESC LIMIT 1",
    )?;
    if let Some(alias) = st
        .query_map(rusqlite::params![target], |r| r.get::<_, String>(0))?
        .next()
    {
        return Ok(alias?);
    }
    Ok(m.conn().query_row(
        "SELECT canonical FROM anchor WHERE id=?1",
        rusqlite::params![target],
        |r| r.get::<_, String>(0),
    )?)
}

/// 这一轮的说话人是不是该锚：`mention.speaker_anchor_id` 是库里唯一写着"谁说的"的列。
/// 同一轮的多条 mention 各带同一个说话人，任一命中即认（不做"多数票"——那是身份层的裁定，
/// 不是闸门的）。
///
/// **C1 起这一列是程序列**：`writers_v4` 按"该轮的转录角色 + 会话身份"盖章（human ⇒ 主人锚、
/// agent ⇒ 自锚），模型递的 `speaker_ref` 只在程序给不出值时保留。所以读它的地方（闸门第 3 支）
/// 读的是程序值，不是模型值。
///
/// D8 方案二：**两侧各解析一次**——入参锚与库里存的说话人锚都可能指向旧锚；
/// 先按原值直配（常见路径零额外解析），不中再各自归到链终点比。
pub fn turn_spoken_by(m: &MemoryHandle, turn_uid: &str, anchor_id: &str)
    -> Result<bool, MemError> {
    let mut st = m.conn().prepare(
        "SELECT DISTINCT speaker_anchor_id FROM mention \
          WHERE turn_uid=?1 AND speaker_anchor_id IS NOT NULL")?;
    let speakers: Vec<String> = st
        .query_map(rusqlite::params![turn_uid], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for speaker in &speakers {
        if speaker == anchor_id {
            return Ok(true);
        }
    }
    let target = redirect_terminal(m, anchor_id)?;
    for speaker in &speakers {
        if *speaker == target || redirect_terminal(m, speaker)?.as_str() == target {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 名字是否在正文里**按词出现**（闸门 6c 第 2 支的唯一实现，`gate` 只调用它）。
///
/// 为什么不是 `body.contains(name)`：短别名与通用词会命中任意正文，那条支变成恒真。
/// 两条口径：
/// - **ASCII 名字**（`lin`、`Acme-3`）：按词边界——名字的**首尾是词字符**的那一侧，正文相邻
///   字节必须不是词字符（ASCII 字母数字或 `_`）。`lin` 命中 `lin`/`Lin`/`(lin)`，
///   不命中 `lincoln`；非 ASCII 字节（汉字）不算词字符，所以 `用Qoder做` 里的 `Qoder` 照常命中。
/// - **含非 ASCII 的名字**（汉字/日文等）：中文没有词边界，按子串判，但**至少两个字符**才算——
///   单字名（`曹`、`李`）在任意中文正文里几乎必然出现，认它等于把这一支变成恒真。
///
/// 空名字/纯空白 ⇒ `false`（空串是任何正文的子串）。
pub fn name_occurs(body: &str, name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() {
        return false;
    }
    if !name.is_ascii() {
        return name.chars().count() >= 2 && body.contains(name);
    }
    let hay = body.as_bytes();
    let needle = name.as_bytes();
    if needle.len() > hay.len() {
        return false;
    }
    let mut from = 0usize;
    while from + needle.len() <= hay.len() {
        let Some(offset) = hay[from..]
            .windows(needle.len())
            .position(|window| window == needle)
        else {
            return false;
        };
        let start = from + offset;
        let end = start + needle.len();
        // 只在名字**首/尾是词字符**的那一侧要求边界：`(lin)` 里的括号不该参与判定。
        let left_ok = !is_word_byte(needle[0])
            || start == 0
            || !is_word_byte(hay[start - 1]);
        let right_ok = !is_word_byte(needle[needle.len() - 1])
            || end == hay.len()
            || !is_word_byte(hay[end]);
        if left_ok && right_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// 词字符 = ASCII 字母数字或 `_`。非 ASCII 字节（汉字/日文的 UTF-8 续字节）**不算**词字符：
/// 中文正文里夹一个英文名字（`用Qoder做`）是该命中，不是该被中文粘住。
fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// 一段正文按**句**切开（借用切片：不复制、不改形）。
///
/// 判据写死在这里（下一刀改漂了就会有用例红）：
/// - 全角 `。！？；` 与换行 `\n` **恒**为句界；
/// - ASCII `.`/`!`/`?`/`;` 为句界，当且仅当其**后一字符是空白或串尾**；
/// - ASCII 句点另有两道防误切：句点后紧跟非空白（`3.14`、`v1.6.0`、`www.example.com`，
///   以及 `U.S.A` 这类无空格缩写）时不是句界；句点前是"点 + 单个 ASCII 字母"
///   （`e.g.`、`i.e.` 这类点分缩写）时同样不是句界。
///
/// 取向是**宁可切窄**：多切只会让"主语与值同句"更难成立（那一侧判降级），少切才会把
/// 跨句借名认成同句——前者是保守的失手，后者是把无源行放成有源行。
///
/// 入参是**已归一**的正文（调用方走 `prepare` 那把剪刀），本函数只切句。
fn sentence_spans(body: &str) -> Vec<&str> {
    let chars: Vec<(usize, char)> = body.char_indices().collect();
    let mut out: Vec<&str> = Vec::new();
    let mut start = 0usize;
    for (index, (offset, ch)) in chars.iter().enumerate() {
        let boundary = match ch {
            '\n' | '。' | '！' | '？' | '；' => true,
            '.' | '!' | '?' | ';' => ascii_sentence_end(&chars, index),
            _ => false,
        };
        if boundary {
            let end = offset + ch.len_utf8();
            let piece = body[start..end].trim();
            if !piece.is_empty() {
                out.push(piece);
            }
            start = end;
        }
    }
    if start < body.len() {
        let piece = body[start..].trim();
        if !piece.is_empty() {
            out.push(piece);
        }
    }
    out
}

/// ASCII 终止符的判定（见 [`sentence_spans`] 的两道防误切）。
fn ascii_sentence_end(chars: &[(usize, char)], index: usize) -> bool {
    let blank_after = match chars.get(index + 1) {
        Some((_, next)) => next.is_whitespace(),
        None => true,
    };
    if !blank_after {
        return false;
    }
    if chars[index].1 == '.' {
        let prev = index.checked_sub(1).and_then(|at| chars.get(at)).map(|(_, ch)| ch);
        let before = index.checked_sub(2).and_then(|at| chars.get(at)).map(|(_, ch)| ch);
        if let (Some(prev), Some(before)) = (prev, before) {
            if prev.is_ascii_alphabetic() && *before == '.' {
                return false;
            }
        }
    }
    true
}

/// 承载 `value` 的那一句/那几句（值可能在同一条正文里出现多次，也可能一次都不出现）。
/// 空值或找不到值 ⇒ 空集——此时共现判据一律不成立（无常量真）。
fn value_sentences<'a>(body: &'a str, value: &str) -> Vec<&'a str> {
    if value.trim().is_empty() {
        return Vec::new();
    }
    sentence_spans(body)
        .into_iter()
        .filter(|sentence| sentence.contains(value))
        .collect()
}

/// 主语名是否与"承载值的那一句"同句（C1 的值句共现尺子）。
///
/// 名字面用的是同一把 [`name_occurs`]（不另写一条"包含"）；`body` 与 `value` 都必须是
/// 归一后的文本（调用方走 `prepare`）。值跨句出现（值本身含句界）⇒ 没有任何一句包含
/// 整个值 ⇒ 不成立：那一侧按降级处理，与本函数的取向一致。
pub fn name_in_value_sentence(body: &str, value: &str, name: &str) -> bool {
    value_sentences(body, value)
        .iter()
        .any(|sentence| name_occurs(sentence, name))
}

/// 承载 `value` 的那一句里有没有第一人称标记（C1 给自锚/主人锚/说话人那三支的那一半）。
///
/// 标记只认 `我` 与 `咱`（`我们` 含 `我`、`咱们` 含 `咱`，都算）：这三个词是**代词**不是
/// 名字，所以不走 [`name_occurs`]——那把尺子对单字非 ASCII 名恒 `false`（同一句说明见
/// `writers_v4` 里记第一人称 mention 的那一段）。
pub fn first_person_in_value_sentence(body: &str, value: &str) -> bool {
    const MARKERS: [&str; 2] = ["我", "咱"];
    value_sentences(body, value)
        .iter()
        .any(|sentence| MARKERS.iter().any(|marker| sentence.contains(marker)))
}

/// 该行是否在 `memory_mention` 表里指向 `anchor_ids` 中的任一锚（批次 M 段 C 的 #11a 读面）。
/// 名字→id 的解析归调用方（`entity_ids_for_name`，其返回已是**链终点**集）；
/// 本函数做 `(mem_table, mem_id, anchor_id)` 命中。
///
/// D8 方案二：库里存的 `anchor_id` 可能是**旧锚**——先按原值直配（常见路径零额外解析），
/// 不中再逐个归到链终点比。合并后按幸存锚名字过滤，旧锚的 mention 照样命中（读数判据 1）。
/// 逐行一条 DISTINCT 查询（候选锚数不再进 SQL 次数），调用量 = `recall` 过滤时的候选集大小。
pub fn row_mentions_any(m: &MemoryHandle, key: &MemKey, anchor_ids: &[String])
    -> Result<bool, MemError> {
    if anchor_ids.is_empty() { return Ok(false); }
    let mut st = m.conn().prepare(
        "SELECT DISTINCT anchor_id FROM memory_mention WHERE mem_table=?1 AND mem_id=?2")?;
    let stored: Vec<String> = st
        .query_map(rusqlite::params![key.table.prefix(), key.id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for id in &stored {
        if anchor_ids.iter().any(|c| c == id) {
            return Ok(true);
        }
        let terminal = redirect_terminal(m, id)?;
        if anchor_ids.contains(&terminal) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 链头行的 `causal_type`（批次 M 段 C 的 export 渲染用）。
/// 非 card 键、非 `kind='causal'`、或列值为 NULL ⇒ `None`（读侧渲染成 `unknown`）。
pub fn causal_type_of(m: &MemoryHandle, key: &MemKey) -> Result<Option<String>, MemError> {
    if key.table != MemTable::Card { return Ok(None); }
    match m.conn().query_row(
        "SELECT causal_type FROM card WHERE id=?1 AND kind='causal'",
        rusqlite::params![key.id], |r| r.get::<_, Option<String>>(0)) {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(MemError::from(e)),
    }
}

/// 标题清单（任务的 `recall` 面用它把句柄渲染成人能读的一行）。
/// 单列查询，不碰正文：能只要标题就不要把整库正文搬进内存（守卫 12 的顺手一半）。
/// 行序与 `facts()` 同一套 `ORDER BY tbl, id`：两条查询是两套臂，牙得各长一遍。
pub fn all_titles(m: &MemoryHandle) -> Result<Vec<(MemKey, String)>, MemError> {
    let sql = ROW_TABLES.iter()
        .map(|t| {
            let title = match t {
                MemTable::Anchor => "f.canonical",
                MemTable::Card => "f.title",
            };
            format!("SELECT '{}' AS tbl, f.id AS id, {title} AS title FROM {} AS f",
                    t.prefix(), t.prefix())
        })
        .collect::<Vec<String>>()
        .join(" UNION ALL ") + " ORDER BY tbl, id";
    let mut st = m.conn().prepare(&sql)?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (tbl, id, title) = row?;
        let table = MemTable::parse(&tbl)
            .ok_or_else(|| MemError::Sql(format!("reader got an unknown mem_table slug: {tbl}")))?;
        out.push((MemKey { table, id }, title));
    }
    Ok(out)
}

/// P0-1 的守卫读数：**证据里有 human 轮、说话人却指向 agent 锚**的 fact 行数。
///
/// 这一格量的是"用户说的被记成 agent 说的"——身份通道断掉时它就是这么长出来的。
/// 四条口径（改任何一条都是换了一个读数，不是修了同一个）：
/// - 只数 `fact` 行：`speaker_anchor_id` 只有这一族有（`state_assertion` 没有这一列）；
/// - "human 证据"读 `provenance.anchor_role`——它是程序值（`writers_fact` 从证据轮抄的
///   `turns.role`），模型递不进去；
/// - "指向 agent 锚"读 `anchor.kind='agent'`，不是"不等于主人锚"：主人锚不存在时
///   `speaker_anchor_id` 是 `NULL`（那是另一种形状，见下一格说明），不该混进来；
/// - **不按死活过滤**：一行退役了也仍然是"当时被记错了"的证据，而这一格是给运维看的
///   缺陷计数，不是召回面的活行口径（`fact_rows` 那一族才是）。
///
/// 与它成对的那一半读数（human 证据 + `speaker_anchor_id IS NULL`）今天**不建格**：
/// 它对应的是"身份未知"，而未知不是缺陷，是"这一格没有生产者"。P0-1 的生产者落地之后，
/// 那一格应当单调下降；等它有真实读者再立项（先量后建）。
pub fn facts_with_agent_speaker_but_human_evidence(m: &MemoryHandle) -> Result<i64, MemError> {
    let n: i64 = m.conn().query_row(
        "SELECT count(*) FROM fact f
           JOIN anchor a ON a.id = f.speaker_anchor_id
          WHERE a.kind = 'agent'
            AND EXISTS (SELECT 1 FROM provenance p
                         WHERE p.mem_table = 'fact' AND p.mem_id = f.id
                           AND p.anchor_role = 'human')",
        [],
        |r| r.get(0),
    )?;
    Ok(n)
}

/// 表态表里指向**不存在于行**的那些记录。它是架构 §6"悬空表态比没有表态更坏"这条判据
/// 在本阶段唯一能被看见的形状：`attest` 只校验锚不校验行存在（V6 之后锚在库内可查，
/// 行的存在性则由消费侧的 `lookup` 把关），所以一条免锚的 `rejected` 表态完全可以落在
/// 一个幽灵句柄上，而它对闸门永不可见。`status` 报这个数，阶段 2 的 `doctor` 会把它升成正式读数。
pub fn dangling_attestations(m: &MemoryHandle) -> Result<usize, MemError> {
    let n: i64 = m.conn().query_row(
        "SELECT count(*) FROM attestation a WHERE NOT EXISTS ( \
           SELECT 1 FROM card c   WHERE a.mem_table='card'   AND c.id=a.mem_id \
           UNION ALL SELECT 1 FROM anchor t WHERE a.mem_table='anchor' AND t.id=a.mem_id)",
        [],
        |r| r.get(0),
    )?;
    Ok(n as usize)
}

/// P3 目录的一格。**没有 `body` 字段**：那张表结构上没有正文列（`writers_x.rs:120-125` 给
/// `write_fact` 传的是空串），所以这不是"少带了一格"，而是这一族根本没有那一格（坑 P-50）。
/// 字段全 `pub`、没有构造器：与 `RowFacts` 同一条理由——测试要逐字段读，私有字段会把判据挤成
/// "夹具迁就封装"。
#[derive(Debug, Clone)]
pub struct EntityEntry {
    /// `entity` 表的主键（内容寻址那串 hex）。目录用它当 `id` 格。
    pub key: String,
    pub kind: String,
    /// 存储原样的 `title`——**未归一**。归一只发生在 `rc-agent` 比较输入的那一刻（坑 P-51）。
    pub name: String,
    /// `entity_alias` 回来的别名，按 SQL 的 `ORDER BY entity_id, alias` 排好。
    pub aliases: Vec<String>,
    /// 与本实体**共享出处轮**的活邻居行数（不是"提到几次"，坑 P-56 的定名理由）。
    pub refs: u64,
    /// 那些邻居行里最新的 `as_of_ms`。没有活邻居就是 `None`——**不是 0**（P-44 那一族）。
    pub latest_as_of_ms: Option<i64>,
}

/// 目录查询的 SQL 文本。**公开是判据要求的**（坑 P-57）：`the_directory_plan_uses_the_turn_index`
/// 里 `EXPLAIN` 的必须是 `entity_directory` 真正执行的那串文本，否则"证明索引被用上"
/// 证明的是另一条 SQL。它同时是"目录只有一条 SQL"这件事的执法点——第二条就是在拼第二份期望。
///
/// 四段形状（每段都对应一条坑，改之前先读那三条实测）：
/// ① 实体侧是**派生表**而不是外层 `FROM anchor AS f WHERE` 后挂那把活行谓词：那把谓词的三支
///    引用别名 `f`，三臂内部也已经用 `f`，外层再叫 `f` 是跨作用域同名遮蔽（能跑、慢 0.5 ms，
///    但读它的人要在脑子里换三次作用域）。派生表版外层叫 `e`、内层叫 `f`，遮蔽消失。
/// ② `pe` 与 `pf` 是**同一张 `provenance` 上的直接自连**：`pe` 取实体的出处轮，
///    `pf` 找同一轮上的其它行。索引被用上的就是 `pf` 这一侧（`turn_uid=?` 那个方向）。
/// ③ 活行集是一条 `UNION ALL` 子查询（当前 `CANDIDATE_TABLES` 只有 card 一臂），别名 `g`；`g.tbl = pf.mem_table AND g.cid = pf.mem_id`
///    就是"邻居必须还活着"。三支谓词来自那把唯一的活行谓词，这里**一个条件都不手写**（坑 P-58）。
/// ④ `count(DISTINCT g.tbl || ':' || g.cid)`：一条行可以有两个出处轮都与实体共享（重蒸、跨批），
///    邻居是**行**不是 (行, 轮) 对。表前缀进拼接串是因为这些表的 `id` 各自内容寻址，
///    理论上能撞（`compute_mem_id` 把表前缀也送进哈希了，所以实际不会——但那正是"不会"要写在这里的原因：
///    少了前缀，这条 DISTINCT 的正确性就依赖另一个 crate 的一个字段选择）。
///
/// 没有 `LIMIT`：目录的截断是**协议面**的事（`rc_agent::tools::MAX_STATUS_ENTITIES`），
/// 而 `entities_total` 要的是全量口径。在 SQL 里截断会把"我知道几个"变成"我知道前几个"。
pub fn directory_sql() -> String {
    let arms = CANDIDATE_TABLES.iter()
        .map(|t| format!(
            "SELECT '{}' AS tbl, f.id AS cid, f.as_of_ms AS as_of_ms FROM {} AS f WHERE {}",
            t.prefix(), t.prefix(), alive_where(*t)))
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let anchor_alive = alive_where(MemTable::Anchor);
    let p = MemTable::Anchor.prefix();
    format!(
        "SELECT e.id AS id, e.kind AS kind, e.canonical AS name, \
         count(DISTINCT g.tbl || ':' || g.cid) AS refs, max(g.as_of_ms) AS latest_as_of_ms \
         FROM (SELECT f.id, f.kind, f.canonical FROM anchor AS f WHERE {anchor_alive}) AS e \
         LEFT JOIN provenance AS pe ON pe.mem_table='{p}' AND pe.mem_id=e.id \
         LEFT JOIN provenance AS pf ON pf.turn_uid=pe.turn_uid AND pf.mem_table<>'{p}' \
         LEFT JOIN ({arms}) AS g ON g.tbl=pf.mem_table AND g.cid=pf.mem_id \
         GROUP BY e.id ORDER BY e.id"
    )
}

/// 别名只由**第二条查询**取回（P-50/P-51）：别名进 `GROUP BY` 就会把条目数乘出来，
/// 而那正是 P-50 的症状。两条查询而不是一条不是"数两遍"——条目数已经在第一条里定了。
const ALIAS_SQL: &str = "SELECT anchor_id, alias FROM alias ORDER BY anchor_id, alias";

/// §13 A40 · Q1 单边指认：一条别名撞上**另一张活实体行**的标题，而两边还没互挂。
///
/// 三支谓词一支都不能少，各有一张变异收据（Step 7 的 ①②）：
/// - `g.id <> e.id`：别名撞自己的标题是本人，不是重影（`t4` 钉住它零命中）。
/// - `NOT EXISTS` 反向那一条 = **收敛项**。闭包写完之后这一支让 Q1 归零，作业因此不复燃。
/// - `NOT EXISTS` 第二 claimant 那一条 = **防瞎并项**。同一串别名被多家认领时按 A40"歧义只报
///   不动"，这一支负责"不动"那一半（报的那一半是 Q2）。claimant 按**活行**数，与 Q2 同一个
///   口径——同一个概念在相邻两支查询里不许有两套定义（守卫 3 的读法）：否则一家退场之后
///   Q1 说"还是歧义、不能并"、Q2 说"没有歧义了"，两族告警同时挂着且互不解除，谁也说不清。
///
/// 只配**实体行对实体行**：`entity_alias.entity_id` 的 FK 指向 `entity(id)`，一条撞进 `card`
/// 标题的别名既写不进闭包、也不是"同一实体的两种写法"（那是 Q3 的事）。
/// 比较是精确串等（`f.title` 存原样，归一只发生在 `compute_mem_id` 的 preimage 里）：只漏不误。
pub fn unclosed_alias_pairs(m: &MemoryHandle) -> Result<Vec<(MemKey, MemKey)>, MemError> {
    let anchor_cte = format!("SELECT f.id AS id, f.canonical AS title FROM anchor AS f WHERE {}",
                             alive_where(MemTable::Anchor));
    let sql = format!(
        "WITH live AS ({anchor_cte}) \
         SELECT e.id AS from_id, g.id AS to_id FROM alias AS al \
         JOIN live AS e ON e.id = al.anchor_id \
         JOIN live AS g ON g.title = al.alias \
         WHERE g.id <> e.id \
           AND NOT EXISTS (SELECT 1 FROM alias AS r \
                           WHERE r.anchor_id = g.id AND r.alias = e.title) \
           AND NOT EXISTS (SELECT 1 FROM alias AS o JOIN live AS l ON l.id = o.anchor_id \
                           WHERE o.alias = al.alias AND o.anchor_id <> e.id) \
         ORDER BY e.id, g.id");
    let mut st = m.conn().prepare(&sql)?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (from, to) = row?;
        // 两侧都是 `MemTable::Anchor`：FK 已经保证了这一点，所以这里不需要 `MemTable::parse`。
        out.push((MemKey { table: MemTable::Anchor, id: from },
                  MemKey { table: MemTable::Anchor, id: to }));
    }
    Ok(out)
}

/// §13 A40 · Q2 歧义：同一串别名被**两家以上活实体**认领。
///
/// **只报不动**（A40 裁定）：这一支的产出只进告警，永远不进闭包写。
/// 返回的第二格是那串别名文本——它不是读数（`attention` 没有承载它的列，七列形状由 T1 定死），
/// 是**给用例的**：告警行只能指向实体行，"报的是哪一串"因此必须在这一侧才可断。
/// 生产读者要那串文本时经 `entity_directory` 自己取（别名格本来就是它的一部分）。
///
/// **W29：`live` 的 anchor 支要再筛 `status`**。`alive_where` 管的是三本账（退场/顶替/否决），
/// 它**不看** `status`；而 redirect 合并（`redirect::merge_redirect`）是零搬运的——旧锚行、
/// 旧锚的别名行全留在原处，只把 `status` 盖成 `merged`。于是已合并的旧锚会继续"认领"名字
/// （现网：`示例主人` 由 merged 的 19d87129… 与活锚 69ece1a1… 各持一份 ⇒ 每次合并长出两条永久告警，
/// 扫一轮解不掉、下一轮又升）。收窄在**这一支**而不是 `alive_where` 自己：后者还有别的读者，
/// 而"merged 的行还算不算活"是它们各自的口径，不是这里能替它们决定的。
pub fn ambiguous_alias_claimants(m: &MemoryHandle) -> Result<Vec<(MemKey, String)>, MemError> {
    let anchor_cte = format!(
        "SELECT f.id AS id, f.canonical AS title FROM anchor AS f WHERE {}{}",
        alive_where(MemTable::Anchor), ANCHOR_STATUS_CLAIMS_NAME);
    let sql = format!(
        "WITH live AS ({anchor_cte}) \
         SELECT al.anchor_id, al.alias FROM alias AS al \
         JOIN live AS e ON e.id = al.anchor_id \
         WHERE al.alias IN (SELECT al2.alias FROM alias AS al2 \
                            JOIN live AS e2 ON e2.id = al2.anchor_id \
                            GROUP BY al2.alias HAVING count(DISTINCT al2.anchor_id) > 1) \
         ORDER BY al.alias, al.anchor_id");
    let mut st = m.conn().prepare(&sql)?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (id, alias) = row?;
        out.push((MemKey { table: MemTable::Anchor, id }, alias));
    }
    Ok(out)
}

/// §13 A40 · Q3 同题多行：**同表 · 同归属 · 同标题**有两行以上，全部点名。
///
/// 组的口径两条都要写明（本节"两处口径收窄"的第 2 条）：
/// - **跨表同名不算重影**（`o.tbl = l.tbl` 那一支）：card 与 event 在同一归属下同名是
///   两个族的两件事。撤掉这一支的代价有变异 ⑤ 作证。
/// - 它是 **A39 的报告口径**：kind 不归一直接进 `compute_mem_id` 的 preimage ⇒ `"person"` 与
///   `"human"` 两种写法 = 同题两行。T14 关掉产生源，而**存量库里的两行不会自己合**，
///   所以这一族的读者在 T14 之后仍然在场（这一条就是它不是恒零读数的证明）。
///
/// 两张表逐支用 `alive_where` 筛（`ROW_TABLES` 那一份清单，与 `facts()` / `all_titles()` 同源）：
/// 已退场的行不参与组，因此"退掉一行"就是这一族的机械解除路径。
///
/// **W29：anchor 那一支还要按 `status` 收窄**（`ANCHOR_STATUS_CLAIMS_NAME`）。已合并的旧锚
/// 仍是库里的活行（三本账都没动它），但它的正名早就并进了幸存锚的 `alias` ⇒ 不收窄的话
/// "同题两行"这一族会一直点名它，跟 Q2 一样是每合并一次涨一族的永久告警。只改 anchor 这一支：
/// card 那张表没有 `status` 这一列，口径不动。
pub fn duplicate_title_rows(m: &MemoryHandle) -> Result<Vec<MemKey>, MemError> {
    let arms = ROW_TABLES.iter()
        .map(|t| {
            // anchor 的标题列是 `canonical`（那张表没有 `title`）。
            let title = match t {
                MemTable::Anchor => "f.canonical",
                MemTable::Card => "f.title",
            };
            // 只有 anchor 有 `status`；card 那一支的空串让两臂共用同一句 `format!`。
            let claims = match t {
                MemTable::Anchor => ANCHOR_STATUS_CLAIMS_NAME,
                MemTable::Card => "",
            };
            format!("SELECT '{}' AS tbl, f.id AS id, f.scope_kind AS sk, f.scope_key AS skk, \
                     {title} AS title FROM {} AS f WHERE {}{}",
                    t.prefix(), t.prefix(), alive_where(*t), claims)
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let sql = format!(
        "WITH live AS ({arms}) \
         SELECT l.tbl, l.id FROM live AS l WHERE EXISTS (SELECT 1 FROM live AS o \
             WHERE o.tbl = l.tbl AND o.sk = l.sk AND o.skk = l.skk \
               AND o.title = l.title AND o.id <> l.id) \
         ORDER BY l.tbl, l.id");
    let mut st = m.conn().prepare(&sql)?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (tbl, id) = row?;
        let table = MemTable::parse(&tbl)
            .ok_or_else(|| MemError::Sql(format!("reader got an unknown mem_table slug: {tbl}")))?;
        out.push(MemKey { table, id });
    }
    Ok(out)
}

/// P3 的目录：**全量、活行口径、按 id 升序**。
///
/// 三条边界（都在任务 11 的坑行里，各有一张用例）：
/// - 邻居的连接键是**出处轮**，不是归属（P-56）。两侧 `scope_key` 从来不是同一个键空间。
/// - 实体行自己也过同一把活行谓词（P-66）。`vote`/`retire` 都没有表闸门，
///   指向 `entity:<id>` 的账写得进去，所以"实体只是名字所以不筛"是错的。
/// - **不含任何输入过滤**。过滤住 `rc-agent::tools::status`（P-51 的定案：库里存的是入参原样，
///   等值比较会把 `"老王 "` 读成查无此人；归一的尺子在 `rc-contract`，那一侧才有它）。
///   本函数因此本来就出全量——`entities_total` 要的正是它的长度。
pub fn entity_directory(m: &MemoryHandle) -> Result<Vec<EntityEntry>, MemError> {
    use std::collections::BTreeMap;
    // 别名先取：`BTreeMap` 而不是 `HashMap`，因为"同一个库两次调用给同一串字节"
    // 在 P-53 那条对撞里是要断的性质，而 `HashMap` 的迭代序不进 JSON（这里只 push 不迭代），
    // 用 `BTreeMap` 是把"别依赖哈希序"这条写进类型而不是写进注释。
    let mut aliases: BTreeMap<String, Vec<String>> = BTreeMap::new();
    {
        let mut st = m.conn().prepare(ALIAS_SQL)?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (id, alias) = row?;
            aliases.entry(id).or_default().push(alias);
        }
    }
    let sql = directory_sql();
    let mut st = m.conn().prepare(&sql)?;
    let rows = st.query_map([], |r| Ok((
        r.get::<_, String>(0)?,
        r.get::<_, String>(1)?,
        r.get::<_, String>(2)?,
        r.get::<_, i64>(3)?,
        r.get::<_, Option<i64>>(4)?,
    )))?;
    let mut out = Vec::new();
    for row in rows {
        let (key, kind, name, refs, latest_as_of_ms) = row?;
        let refs = u64::try_from(refs).map_err(|_| MemError::Sql(
            format!("refs must be non-negative, got {refs}")
        ))?;
        out.push(EntityEntry {
            refs,
            latest_as_of_ms,
            aliases: aliases.remove(&key).unwrap_or_default(),
            key, kind, name,
        });
    }
    Ok(out)
}

/// 一跳溯源（任务 7 的 `export` 用它；`rc-agent` 的依赖集里没有 rusqlite，
/// 所以这条 SQL 只能住在拥有 `provenance` 那张表的 crate 里，守卫 11 的列归属才算得清）。
/// 按 turn_uid 升序：出处是一张集合，读回来的顺序不许是插入顺序（那会让导出件 diff 抖动）。
///
/// **`AND ref_kind='turn'`（任务 7 加）**：出处表的 `turn_uid` 列现在装两种东西——轮次 uid
/// （`ref_kind='turn'`）与被引用行的 id（`ref_kind` 就是那张表）。这一支筛把 `provenance_of`
/// 的语义收回它的名字："这一行的出处**轮次**"。不加的代价是现成的两条：
/// ① `crates/rc-distill/src/audit.rs` 的 `for (uid, _role) in provenance_of(m, key)?` 拿那个
///    uid 去会话库取正文，而审核队列（`unreviewed_rows` 走 `CANDIDATE_TABLES`，那里只有 card 一张）
///    从任务 4 起会收到 `kind='timeline'` 的事件行 ⇒ 一个行 id 被当真轮次 uid 递出去，取不到
///    正文、也不报错。② `export` 会把它渲成 `← turn <行 id>`，那是运维面上的一句假话。
/// 筛了之后：audit 对推导行取到空出处（诚实——它本来就没有轮次出处），推导行的依据由
/// `provenance_chain` 的 `Basis` 那一支负责渲，两边各有归宿。
/// 既有三个读者（`rc-memory/tests/writers.rs`、`rc-search/tests/causes.rs`、
/// `rc-search/tests/gate_switches.rs`）落的都是走 `ref_kind` DEFAULT `'turn'` 的行 ⇒ 一个字不变。
pub fn provenance_of(m: &MemoryHandle, key: &MemKey) -> Result<Vec<(String, AnchorRole)>, MemError> {
    let mut st = m.conn().prepare(
        "SELECT turn_uid, anchor_role FROM provenance
         WHERE mem_table=?1 AND mem_id=?2 AND ref_kind='turn' ORDER BY turn_uid")?;
    let rows = st.query_map(rusqlite::params![key.table.prefix(), key.id],
                             |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (turn_uid, role) = row?;
        out.push((turn_uid, AnchorRole::parse(&role).unwrap_or(AnchorRole::System)));
    }
    Ok(out)
}

/// A44 软边的一行读数。`decided_by` 可空（C 面今天无生产者）。
/// canonical 之后落在 `fact_link`：`from_id`/`to_id` 是 `"<table>:<id>"` 键（本族两端都是 `card:`）。
#[derive(Debug, Clone)]
pub struct EventLink {
    pub id: String,
    pub a_id: String,
    pub b_id: String,
    pub basis: String,
    pub evidence_json: String,
    pub decided_by: Option<String>,
    pub scope: Scope,
    pub created_at_ms: i64,
}

/// 指向 `b_id` 的**未撤销**软边（一条时间行只有一条入边；多条时取最新）。
/// `b_id` 是那行时间行的裸 id：软边落的 `to_id` 是 `card:<id>`（时间行是消费行）。
pub fn event_link_of(m: &MemoryHandle, b_id: &str) -> Result<Option<EventLink>, MemError> {
    let mut st = m.conn().prepare(
        "SELECT l.id, l.from_id, l.to_id, l.basis, l.evidence_json, l.decided_by,
                l.scope_kind, l.scope_key, l.created_at_ms
           FROM fact_link AS l
          WHERE l.to_id = 'card:' || ?1
            AND NOT EXISTS (SELECT 1 FROM retirement AS r
                            WHERE r.mem_table='fact_link' AND r.mem_id=l.id
                              AND r.resolved_at_ms IS NULL)
          ORDER BY l.created_at_ms DESC, l.id LIMIT 1")?;
    let mut rows = st.query_map([b_id], |r| Ok(EventLink {
        id: r.get(0)?, a_id: r.get(1)?, b_id: r.get(2)?, basis: r.get(3)?,
        evidence_json: r.get(4)?, decided_by: r.get(5)?,
        scope: Scope { kind: ScopeKind::parse(&r.get::<_, String>(6)?)
                            .unwrap_or(ScopeKind::Project), key: r.get(7)? },
        created_at_ms: r.get(8)?,
    }))?;
    match rows.next() { Some(row) => Ok(Some(row?)), None => Ok(None) }
}

/// 全量表态谱（§9）的载体。**字段只加不减（2E-C2）**：今天 6 个字段（E48 加了
/// `anchor_turn_uid`），加新字段时旧消费者不破（reader 把它读出来即够消费）。
#[derive(Debug, Clone)]
pub struct AttestationSummary {
    /// `None` = 库里的 stance 是一个认不出的 slug（库外写入/旧库残值）。
    /// **不许折成 `Rejected`**：那是把"读不懂"谎报成"被否决"，`alive_where` 会据此抑制行。
    pub stance: Option<Stance>,
    pub actor: String,
    pub reason: Option<String>,
    /// 表态所锚的那一轮（`attest` 的 `anchor_turn_uid` 列）。**E48 加的第 6 个字段**：
    /// 本任务的论证承诺 agent 看得到"重新表态的锚是哪条人工轮"，而前 5 个字段里没有它
    /// ——写入侧在 `attest` 里当场核对过这条锚确实是某条人工轮出处（V6），不暴露它
    /// 等于把那次校验对读者藏起来。`None` = 这一条没带锚（`rejected` / `retired` 免锚）。
    pub anchor_turn_uid: Option<String>,
    pub created_at_ms: i64,
    /// `None` = 仍在场（最新一条永远是这个形状）；`Some(t)` = 在时刻 t 被新表态解除。
    pub resolved_at_ms: Option<i64>,
}

/// 同一记忆的**整条表态史**——在场与已解除都返回（§9 的"时间序"由这条函数承担）。
/// 排序按 `created_at_ms` 而不是 `id`，因为同毫秒并发可能让 id 与时间次序相反；
/// 而 `attest` 的写入是按 `now_ms` 升序盖章（V1），同表内的 `created_at_ms` 单调是个事实。
/// 与 `provenance_of` 同族写法（住 `rc-memory` 才对得上 `attestation` 的列归属，守卫 11）。
pub fn attestations_of(m: &MemoryHandle, key: &MemKey) -> Result<Vec<AttestationSummary>, MemError> {
    let mut st = m.conn().prepare(
        "SELECT stance, actor, reason, anchor_turn_uid, created_at_ms, resolved_at_ms
         FROM attestation
         WHERE mem_table=?1 AND mem_id=?2 ORDER BY created_at_ms, id")?;
    let rows = st.query_map(rusqlite::params![key.table.prefix(), key.id],
        |r| Ok(AttestationSummary {
            stance: Stance::parse(&r.get::<_, String>(0)?),
            actor: r.get(1)?,
            reason: r.get(2)?,
            anchor_turn_uid: r.get(3)?,
            created_at_ms: r.get(4)?,
            resolved_at_ms: r.get(5)?,
        }))?;
    let mut out = Vec::new();
    for row in rows { out.push(row?); }
    Ok(out)
}

/// 两跳溯源（任务 7 的 `export` 用它）。**SQL 全住 rc-memory**（守卫 11 + R39）：
/// 第一跳读本行的**全部**出处行（两种 `ref_kind` 都要，否则推导行的依据就看不见）；
/// 第二跳对每个**在场**的依据行走 `provenance_of`（它已经只取 `ref_kind='turn'` 那一支）。
/// 排版与 missing 的措辞由 rc-agent 那边承担：这一函数只给有序步骤。
#[derive(Debug, Clone)]
pub enum ProvenanceStep {
    /// 本行的出处轮次（`ref_kind='turn'`）——事实族现状的那一支，渲染照旧。
    Turn { turn_uid: String, role: AnchorRole },
    /// 本行的依据行那一跳（`ref_kind != 'turn'`）。`basis_exists=false` ⇒ 依据行已经不在
    /// `ref_kind` 那张表里：渲染必须明写 missing、不许省略，而且这一支**不**带 `BasisTurn`
    /// （"没有'然后'就不能报"）。
    Basis { ref_kind: String, target_id: String, role: AnchorRole, basis_exists: bool },
    /// 依据行自己的出处轮次，紧跟它对应的那一条 `Basis`。
    BasisTurn { turn_uid: String, role: AnchorRole },
}

/// `pointers_json` 里的 JSON 字符串（只解一层 `[...]`：调用方随后按 `fact:` 前缀筛）。
/// **不引 `serde_json`**：`rc-memory` 的依赖清单里没有它（`writers_fact.rs` 文件头那条守卫 10 的
/// 红线），而这里的输入形状是写侧用 [`crate::writers_fact`] 的 `json_array` 盖出来的、
/// 只含内容哈希的串数组。坏 JSON（截断、非字符串项、控制字符）不报错：能认出的字符串照收，
/// 认不出的部分丢掉——读侧对库外残值一贯走"低格/跳过"，不 panic（同 `decode_raw` 的脏 slug 策略）。
///
/// `pub(crate)`（派发档 I-3a）：第二个读者是同 crate 的 `writers_x::put_episodes`——它要把成员
/// 的 `participants_json` 读成裸 anchor id 数组。**不进 crate 根 re-export**（同 `alive_where`
/// 的理由：根导出会把"这一个形状只有一个家"说成公开承诺）。
pub(crate) fn json_array_strings(doc: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut escaped = false;
    for ch in doc.chars() {
        if in_string {
            if escaped {
                match ch {
                    'n' => current.push('\n'),
                    'r' => current.push('\r'),
                    't' => current.push('\t'),
                    // `\uXXXX` 本形状用不到（指针只有 hex）：这里不还原四位码点，只留字符本体；
                    // 真出现时这串自然落不到 `fact:` 前缀，被下一层跳过。
                    other => current.push(other),
                }
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
                out.push(std::mem::take(&mut current));
            } else {
                current.push(ch);
            }
        } else if ch == '"' {
            in_string = true;
            current.clear();
            escaped = false;
        }
    }
    out
}

/// fact 自己的出处行（`ref_kind='turn'`），与 [`provenance_of`] 同一支 SQL；
/// 唯一差别是句柄表名是字面量 `'fact'`——`MemTable` 里没有 fact 变体（可指认表只有
/// `anchor`/`card`），所以这里不能借道 `MemKey`。
fn fact_turn_provenance(m: &MemoryHandle, fact_id: &str)
    -> Result<Vec<(String, AnchorRole)>, MemError> {
    let mut st = m.conn().prepare(
        "SELECT turn_uid, anchor_role FROM provenance
         WHERE mem_table='fact' AND mem_id=?1 AND ref_kind='turn' ORDER BY turn_uid")?;
    let rows = st.query_map(rusqlite::params![fact_id],
        |r| Ok((r.get::<_, String>(0)?,
                AnchorRole::parse(&r.get::<_, String>(1)?).unwrap_or(AnchorRole::System))))?;
    let mut out = Vec::new();
    for row in rows { out.push(row?); }
    Ok(out)
}

/// fact 的**单轮兜底**：`provenance` 零行时读 `evidence_turn_uid`。
/// 角色只在能推出时给：`speaker_anchor_id` 是库里唯一写着"谁说的"的列，它指向的
/// `anchor.kind` 是 `person` ⇒ Human、`agent` ⇒ Agent；其它 kind（org/place/…）与 NULL
/// 都**推不出这轮的角色** ⇒ 落 `AnchorRole::Agent`。这是兜底值、不是事实：库里没有这一轮的
/// 角色读数（`fact` 没有 role 列），编一个 Human 会把"代理写的"说成"人说的"。
fn fact_evidence_fallback(m: &MemoryHandle, fact_id: &str)
    -> Result<Option<(String, AnchorRole)>, MemError> {
    let row = m.conn().query_row(
        "SELECT evidence_turn_uid, speaker_anchor_id FROM fact WHERE id=?1",
        rusqlite::params![fact_id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)));
    let (turn_uid, speaker) = match row {
        Ok(row) => row,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(MemError::from(e)),
    };
    let role = match speaker {
        Some(speaker_id) => {
            let kind = m.conn().query_row(
                "SELECT kind FROM anchor WHERE id=?1", rusqlite::params![speaker_id],
                |r| r.get::<_, String>(0));
            match kind.as_deref() {
                Ok("person") => AnchorRole::Human,
                Ok("agent") => AnchorRole::Agent,
                // 锚不在 / 认不出的 kind ⇒ 推不出，走同一个兜底。
                _ => AnchorRole::Agent,
            }
        }
        None => AnchorRole::Agent,
    };
    Ok(Some((turn_uid, role)))
}

/// **card 的出处兜底**（审计 2026-10-27：中心库 134 张 card 的 `provenance` 行是 0 行）。
///
/// card 的出处不写在自己名下——事实行把 `(mem_table='fact', mem_id=<fact id>)` 写进
/// `provenance`，card 用 `pointers_json`（形如 `["fact:<id>"]`）指回事实行。所以
/// `chain_for(card)` 零行时走这一跳：逐指针读 fact 的 `ref_kind='turn'` 出处，
/// 再对"有指针、无出处"的 fact 用 `evidence_turn_uid` 兜底。
///
/// 只产 [`ProvenanceStep::Turn`]：指针指向的是事实行，而事实行自己的依据行（`Basis` 那一支）
/// 是另一层语义；在 card 面上把它渲出来等于替事实行编造"凭什么"。去重按**轮次**做
/// （同一轮被多条 fact 引用只报一步），顺序 = 指针顺序 × 每条 fact 的 `turn_uid` 升序。
/// 不在这里截断：`MAX_PROVENANCE_STEPS` 是消费面（`row_json`）的协议上限，
/// 导出面（`MAX_EXPAND_STEPS`）要用完整链。
fn card_pointer_chain(m: &MemoryHandle, card_id: &str) -> Result<Vec<ProvenanceStep>, MemError> {
    let pointers = match m.conn().query_row(
        "SELECT pointers_json FROM card WHERE id=?1", rusqlite::params![card_id],
        |r| r.get::<_, String>(0)) {
        Ok(pointers) => pointers,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(Vec::new()),
        Err(e) => return Err(MemError::from(e)),
    };
    let mut steps = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for pointer in json_array_strings(&pointers) {
        // 只认 `fact:` 指针；`card:` 等其它形状不是这一跳的目标（漏认只少不多，不猜）。
        let Some(fact_id) = pointer.strip_prefix("fact:") else { continue; };
        if fact_id.is_empty() { continue; }
        let turns = fact_turn_provenance(m, fact_id)?;
        if turns.is_empty() {
            if let Some((turn_uid, role)) = fact_evidence_fallback(m, fact_id)? {
                if seen.insert(turn_uid.clone()) {
                    steps.push(ProvenanceStep::Turn { turn_uid, role });
                }
            }
            continue;
        }
        for (turn_uid, role) in turns {
            if seen.insert(turn_uid.clone()) {
                steps.push(ProvenanceStep::Turn { turn_uid, role });
            }
        }
    }
    Ok(steps)
}

fn chain_for(m: &MemoryHandle, mem_table: &str, mem_id: &str) -> Result<Vec<ProvenanceStep>, MemError> {
    // `role` 这一格（批次③ 因果链）今天**必须**被取出来：因果链的环节行（`role='link'`）与依据行
    // 同在这张 `provenance` 表里、`ref_kind` 又都非 `'turn'` ⇒ 单看 `ref_kind` 分不出环节和依据。
    // 少了这一列就落不了下面那道筛（工单 §5-1 第二条的实测细节：原 SQL 三列里根本没有 role）。
    let mut st = m.conn().prepare(
        "SELECT ref_kind, turn_uid, anchor_role, role FROM provenance
         WHERE mem_table=?1 AND mem_id=?2 ORDER BY turn_uid")?;
    let first = st.query_map(rusqlite::params![mem_table, mem_id],
                             |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?,
                                     r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(st);

    // 审计 2026-10-27：card 的出处住在 fact 名下，自己名下零行。只在**零行**时走兜底，
    // 有 provenance 行的 card（`put_card` 的直接写入口）行为逐字节不变。
    if first.is_empty() && mem_table == MemTable::Card.prefix() {
        return card_pointer_chain(m, mem_id);
    }

    let mut steps = Vec::with_capacity(first.len() * 2);
    for (ref_kind, target_id, anchor_role, role) in first {
        // **环节不是依据**：`role='link'` 的那一行是因果链的一个环节，它和"依据"挤在同一张表、
        // `ref_kind` 又同属非 `'turn'` 那一族 ⇒ 不筛掉它，`export` 会把环节当依据报出去。
        // 归面按实测：`export` 的调用点在 `crates/mcp/src/main.rs`，走的是 MCP 工具 = **agent 面**
        // （守卫 12 的出口之一），所以"把环节当依据"是 agent 面上的假话，不是运维面的
        //（运维面是 hub/doctor 那六条命令，零正文、根本不渲出处链）。环节由 `chain_links` 按 `ord`
        // 单独读出、由 `export` 另渲一形；这一支只把依据留下。`ref_kind='turn'` 那一支一字未动。
        if role == "link" { continue; }
        // 与 `provenance_of` 同一把认法：认不出的 slug 折成 System（`AnchorRole::parse` 的家）。
        let role = AnchorRole::parse(&anchor_role).unwrap_or(AnchorRole::System);
        if ref_kind == "turn" {
            steps.push(ProvenanceStep::Turn { turn_uid: target_id, role });
            continue;
        }
        // 存在性按 `ref_kind` 分派直查对应表（债 #23）：`MemTable` 只有 `Anchor|Card` 两个变体
        // （`keys.rs`，本轮不扩），episode 镜像卡的依据步 `ref_kind='fact'` 修前在这里落进
        // "认不出"兜底、恒报 `basis_exists=false`——依据步（真 fact 行）存在却自称 missing。
        // 口径拍板：与 card/anchor 正常分支逐字同形，只判**行在不在**
        // （`SELECT EXISTS(… WHERE id=?)`），不套 `fact_rows` 那套活行三支谓词
        //（superseded_by/retirement/attestation）——`Basis` 这一格的语义是"依据行已经不在
        // `ref_kind` 那张表里"（本枚举的文档），被 `superseded_by` 顶掉或退场的依据行
        // 物理上还在，历史链不许说它不在。
        // 第二跳同口径：card/anchor 走 [`provenance_of`]（只读 `ref_kind='turn'` 的出处行），
        // fact 走 [`fact_turn_provenance`]（同一支 SQL、表名钉死 `'fact'`），都不带
        // `evidence_turn_uid` 兜底——那是 card 指针兜底那一层的活（[`card_pointer_chain`]），
        // 不进 Basis 第二跳。
        let target_table = MemTable::parse(&ref_kind);
        let (basis_exists, basis_turns) = match (target_table, ref_kind.as_str()) {
            (Some(target_table), _) => {
                let basis_exists: bool = m.conn().query_row(
                    &format!("SELECT EXISTS(SELECT 1 FROM {} WHERE id=?1)", target_table.prefix()),
                    rusqlite::params![target_id], |r| r.get(0))?;
                let turns = if basis_exists {
                    provenance_of(m, &MemKey { table: target_table, id: target_id.clone() })?
                } else {
                    Vec::new()
                };
                (basis_exists, turns)
            }
            (None, "fact") => {
                let basis_exists: bool = m.conn().query_row(
                    "SELECT EXISTS(SELECT 1 FROM fact WHERE id=?1)",
                    rusqlite::params![target_id], |r| r.get(0))?;
                let turns = if basis_exists {
                    fact_turn_provenance(m, &target_id)?
                } else {
                    Vec::new()
                };
                (basis_exists, turns)
            }
            // `ref_kind` 有 CHECK（T1 的 DDL）：闭集里除了本函数处理的 turn/card/anchor/fact
            // 四值，还剩 relation/state_assertion/utterance/episode/attribute/mention 六值——
            // 写侧今天对它们**零生产者**，真走到了 = 库里留着旧字符串或旁路写入。与 T6 段 0
            // 的残值同律：认不出就当依据不在、响亮地标 missing（Basis 步本身在场、值看得见，
            // 不静默吞这个值），不抛错。
            (None, _) => (false, Vec::new()),
        };
        steps.push(ProvenanceStep::Basis {
            ref_kind, target_id: target_id.clone(), role, basis_exists,
        });
        for (turn_uid, turn_role) in basis_turns {
            steps.push(ProvenanceStep::BasisTurn { turn_uid, role: turn_role });
        }
    }
    Ok(steps)
}

pub fn provenance_chain(m: &MemoryHandle, key: &MemKey) -> Result<Vec<ProvenanceStep>, MemError> {
    chain_for(m, key.table.prefix(), &key.id)
}

/// C20 的边读者：步法与事实行完全同源（同一个 `chain_for`），只是句柄换成边的 id。
/// 边的依据行可以是两张可指认表的任意一张；`Basis { basis_exists: false }` 在生产写入下不可达
/// （`put_edges` 先 `table_of`），只有库外删除才造得出——保留 missing 渲染是防御性读数，不是谎报。
pub fn edge_provenance(m: &MemoryHandle, edge_id: &str) -> Result<Vec<ProvenanceStep>, MemError> {
    chain_for(m, "fact_link", edge_id)
}

/// 边的一行读数。**不是 `RowFacts`**：边不可指认句柄（裁定 3），字段形状也不同
/// （两端 + relation/basis/scope，没有 title/body/verdict）。
///
/// `fact_link` 的端点存 `"<table>:<id>"` 键；这里在 Rust 侧切开，`from_kind`/`to_kind`
/// 就是键里的表前缀（认不出的键整段留在 `from_id`/`to_id`、`kind` 留空串——与读侧
/// 残值走低格同律，不 panic）。
///
/// 只 derive 到 `Clone`：`Scope` 本身没有 `PartialEq`/`Eq`（`keys.rs` 的 `#[derive(Debug, Clone)]`），
/// 而本任务不新增 `keys.rs` 的改动面 ⇒ 这里不 derive 那两个 trait，也不手写一份只比字段的
/// `PartialEq`（边读数的相等性本阶段没有消费者）。
#[derive(Debug, Clone)]
pub struct EdgeFacts {
    pub id: String,
    pub from_kind: String,
    pub to_kind: String,
    pub from_id: String,
    pub to_id: String,
    pub kind: String,
    pub reason: String,
    pub scope: Scope,
    pub created_at_ms: i64,
    pub retired: bool,
    /// Phase 4 读侧补的那一格（派发档 I-5a §2 ·「边证据」）。它**不是**边能不能露面的判据：
    /// `writers_x::put_edges_inner` 把这一列写死 `'[]'`（P-1 的逐字登记：证据落 `basis`，
    /// 因为那一格 NOT NULL 且有读者），只有 A44 软边那一条 INSERT 写真数组
    /// （`writers_x.rs` 的 `json_array(basis_ids)`）。所以消费面对"这条边凭什么"读的是
    /// `reason`（=`basis` 列），这一格是**它的补充**，不是替代——把它当唯一证据源，
    /// 就等于把全仓的边都判成不可见，那正是本会话要终结的"写了没人读"的镜像错。
    /// 加这一格的风险与 `RowFacts` 不同：`EdgeFacts` 全仓**零**外部结构体字面量
    /// （本轮复核：只有本文件的两处构造点），而 `RowFacts` 有 14 处结构体字面量，一律不许动。
    pub evidence_json: String,
}

/// 把 `"<table>:<id>"` 键切成 `(kind, id)`；没有冒号 ⇒ `("", 整段)`。
fn split_endpoint(key: String) -> (String, String) {
    match key.split_once(':') {
        Some((kind, id)) => (kind.to_string(), id.to_string()),
        None => (String::new(), key),
    }
}

/// 一条边的列清单（**唯一的一份**，[`edges`] 与 [`edges_touching`] 共用）。
///
/// 抽出来而不是各写一遍（守卫 3）：两条查询的 `FROM`/`WHERE`/`ORDER BY` 不同
/// （一条全表按 id、一条按端点集合按时间倒序），但**投影的十格必须逐字同形**——
/// 漂一格就是同一个 `EdgeFacts` 在两个读面上指不同的列，而那是一种不报错的错
/// （同 [`FACT_VIEW_COLUMNS`] 那条理由）。
const EDGE_COLUMNS: &str = "r.id, r.from_id, r.to_id, r.relation, r.basis,
                r.scope_kind, r.scope_key, r.created_at_ms,
                EXISTS (SELECT 1 FROM retirement AS x
                        WHERE x.mem_table='fact_link' AND x.mem_id=r.id
                          AND x.resolved_at_ms IS NULL),
                r.evidence_json";

/// [`EDGE_COLUMNS`] 的解码（同一份列清单的唯一读法）。
fn decode_edge_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<EdgeFacts> {
    let (id, from_key, to_key, kind, reason, sk, skk, created_at_ms, retired, evidence_json) = (
        r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
        r.get::<_, String>(3)?, r.get::<_, String>(4)?,
        r.get::<_, String>(5)?, r.get::<_, String>(6)?, r.get::<_, i64>(7)?,
        r.get::<_, bool>(8)?, r.get::<_, String>(9)?,
    );
    let (from_kind, from_id) = split_endpoint(from_key);
    let (to_kind, to_id) = split_endpoint(to_key);
    Ok(EdgeFacts {
        id, from_kind, to_kind, from_id, to_id, kind, reason,
        scope: Scope { kind: ScopeKind::parse(&sk).unwrap_or(ScopeKind::Project), key: skk },
        created_at_ms, retired, evidence_json,
    })
}

/// 全量边（按 id 升序，稳定）。`retired` = 有一条未解决的 `retirement(mem_table='fact_link')`。
pub fn edges(m: &MemoryHandle) -> Result<Vec<EdgeFacts>, MemError> {
    let sql = format!("SELECT {EDGE_COLUMNS} FROM fact_link AS r ORDER BY r.id");
    let mut st = m.conn().prepare(&sql)?;
    let rows = st.query_map([], decode_edge_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// **触达这批端点的边**（Phase 4 的 L3 读原语，派发档 I-5a §2 = `recall-contract.md` §5-新 reader）。
///
/// `endpoints` 是**存进 `fact_link.from_id`/`to_id` 的那种键**（`"<table>:<id>"`），一条 SQL：
/// `WHERE from_id IN (…) OR to_id IN (…)`，两臂各走 `fact_link_from` / `fact_link_to` 索引
/// （`ddl.rs` 里随 `fact_link` 建的 `fact_link_from` / `fact_link_to` 两条索引）。**深 2 不是逐点查**：调用方把第 1 跳读到的端点集合并进来**再调一次**，
/// 于是每一跳恰好 1 条 SQL，天然没有 N+1（这是这一格存在的理由，不是顺手的写法）。
///
/// 排序 `created_at_ms DESC, id`：与 [`event_link_of`] 同一把"最新一条先看"的尺子（那里是
/// `:1019`），保证"最新的那条因果"先进召回。`limit` 由调用方夹紧（消费面是
/// `rc_agent::recall::MAX_L3_EDGES`）；`endpoints` 为空 ⇒ 直接返回空集，**不拼** `IN ()`
/// （那是 SQLite 的语法错，而不是"没有边"）。
///
/// **`retired` 不在这里筛**：与 [`edges`] 同一口径（读数里带上"它有没有未解决的退场账"，
/// 由消费面决定不渲）。这里筛了，`edges` 与 `edges_touching` 就成了两套死活口径
/// （守卫 3），而边的退场面在 `export` 那一族已经有人读那一格。
pub fn edges_touching(
    m: &MemoryHandle, endpoints: &[String], limit: usize,
) -> Result<Vec<EdgeFacts>, MemError> {
    if endpoints.is_empty() {
        return Ok(Vec::new());
    }
    let list = endpoints.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    // 两臂各自要一份参数（`IN` 不能复用占位符），所以 args 是 `2N + 1` 格；
    // 最后那一格是 limit（`i64`，与仓内其余 `limit as i64` 同形）。
    let cap = limit as i64;
    let sql = format!(
        "SELECT {EDGE_COLUMNS} FROM fact_link AS r
          WHERE (r.from_id IN ({list}) OR r.to_id IN ({list}))
          ORDER BY r.created_at_ms DESC, r.id LIMIT ?{}",
        endpoints.len() * 2 + 1
    );
    let mut st = m.conn().prepare(&sql)?;
    let mut args: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(endpoints.len() * 2 + 1);
    for key in endpoints {
        args.push(key);
    }
    for key in endpoints {
        args.push(key);
    }
    args.push(&cap);
    let rows = st.query_map(rusqlite::params_from_iter(args.iter()), decode_edge_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// **fact → 它那张陈述面消费卡**的句柄（Phase 4 的 L2 过闸入口，派发档 I-5a §3 第一条硬线）。
///
/// 为什么必须有它：`episode_fact.fact_id` 指的是 `fact` 表，而 `MemTable` 只有
/// `Anchor|Card`（`keys.rs`）⇒ `fact` 行**表达不出一个 `MemKey`**，也就进不了
/// `GateIndex::decide`（它收 `RowFacts`）。派生族里唯一能过闸的稳定句柄就是那张镜像/消费卡，
/// 所以 L2 成员必须先解析成 `card:<id>` 再判，**而不是放宽 `allows`/`decide`**
/// （放宽就是把"派生视图"变成"绕过归属的后门"，控制器在台账 Phase 2.5 那一节已经裁定过方向）。
///
/// 反查走 `card.pointers_json` 里的 `fact:<id>` 指针，比对用 SQLite 自己的 `json_each`
/// 逐格相等、不用 `LIKE` 的子串近似（指针是定长 hex，`fact:ab` 的子串能命中 `fact:abc…`，
/// 那是一种不报错的错；同一个理由与逐字读法见 `writers_x::member_cards_are_silenced`）。
///
/// **`mode='stated'` 那一格是抄写侧那条实测结论，不是修饰**：指回同一条 fact 的卡有两族——
/// 抽取写的陈述面消费卡，和 `put_episodes` 写的 episode 镜像卡（`mode='inferred'`，
/// 它的 `pointers_json` 是**全体成员**）。取后者当"这一条成员的句柄"会让成员指向整段经历，
/// 而 `superseded_by` 在镜像卡上说的是"这段经历被下一段顶掉"，两种读法都会说谎。
///
/// `ORDER BY c.id LIMIT 1`：多条陈述面卡同时指回同一条 fact 是可达的（判同之前的两份内容
/// 相同的卡），取 id 最小那一条 = 与内容寻址同序的确定读数；没有卡 ⇒ `None`
/// （库里没有那张卡就没有那本账，**不许**在这里替它编一个句柄）。
pub fn card_for_fact(m: &MemoryHandle, fact_id: &str) -> Result<Option<MemKey>, MemError> {
    let pointer = format!("fact:{fact_id}");
    let got = m.conn().query_row(
        "SELECT c.id FROM card c, json_each(c.pointers_json) p
          WHERE p.value=?1 AND c.mode='stated' ORDER BY c.id LIMIT 1",
        [pointer],
        |r| r.get::<_, String>(0),
    );
    match got {
        Ok(id) => Ok(Some(MemKey { table: MemTable::Card, id })),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(MemError::from(e)),
    }
}

/// 与 [`card_for_fact`] 同一族、换一族的卡：**这一段经历的镜像卡**（`kind='episode'`，
/// `writers_x::put_episodes` 那一族唯一写 `'episode'` 的地方）。
///
/// 为什么单独一支而不复用 `card_for_fact`：同一条 fact 的指针被两族卡引用，两族说的是两件
/// 不同的事——陈述面卡说的是"这一条事实"，镜像卡说的是"这一段经历"（它的 `pointers_json`
/// 是**全体成员**、`superseded_by` 是**经历的结构史**）。混用就是把"这段经历被下一段顶掉"
/// 读成"这条事实被判死"，那条误读的现场与逐字理由见 `writers_x::member_cards_are_silenced`
/// 的 `mode='stated'` 那一格。
///
/// 它的存在还有一层消费价值：镜像卡才是 episode 的稳定句柄（`MemTable` 不扩），
/// 召回面要"看得见这一段、并能展开这一段"就必须有这一支。同一条 fact 落在两代经历里
/// （旧的一代还没被指过去）⇒ 取 id 最小那一条，确定读数而不是"最近写的"。
pub fn episode_card_for_fact(m: &MemoryHandle, fact_id: &str) -> Result<Option<MemKey>, MemError> {
    let pointer = format!("fact:{fact_id}");
    let got = m.conn().query_row(
        "SELECT c.id FROM card c, json_each(c.pointers_json) p
          WHERE p.value=?1 AND c.kind='episode' ORDER BY c.id LIMIT 1",
        [pointer],
        |r| r.get::<_, String>(0),
    );
    match got {
        Ok(id) => Ok(Some(MemKey { table: MemTable::Card, id })),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(MemError::from(e)),
    }
}

/// 一张卡点名的 **fact id 清单**（裸 id，已剥 `fact:` 前缀），按 `pointers_json` 的数组序。
///
/// L1 命中行要找到自己的 episode，链路是 `card.pointers_json → fact:<id> →
/// episode_of_fact(fact)`（`recall-contract.md` §4 那条链的第二跳）；这一格是它的第一跳。
/// 全仓此前只有两个指针读者，都是"给定 card 读它的成员"的**正向**用法
/// （[`row_participants`] 与 `card_pointer_chain`），两处都把解码埋在函数内部、不外给清单，
/// 所以召回面要么复刻一份 JSON 解法（守卫 3 的第二家），要么走这一格。
/// `ORDER BY p.key` = 数组下标序 = 写侧落库序（episode 镜像卡那一族按事件序 `ord` 写，
/// 所以这一格返回的就是成员的事件序）。锚行 / 卡不在 / 指针坏 ⇒ 空 `Vec`：
/// 读侧对残值一贯"少不多，不猜"（同 [`card_pointer_chain`]）。
pub fn card_fact_pointers(m: &MemoryHandle, card_id: &str) -> Result<Vec<String>, MemError> {
    let mut st = m.conn().prepare(
        "SELECT p.value FROM card c, json_each(c.pointers_json) p
          WHERE c.id=?1 ORDER BY p.key")?;
    let rows = st.query_map([card_id], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        if let Some(fact_id) = row?.strip_prefix("fact:") {
            if !fact_id.is_empty() {
                out.push(fact_id.to_string());
            }
        }
    }
    Ok(out)
}

/// 一批 card 的 `mode` + `time_confidence`（派发档 I-5a §2 最后一条读原语）。
///
/// **为什么不进 `RowFacts`**：那 14 处结构体字面量（含 `recall.rs::filter_tests::base()`
/// 那处不带 `..Default` 的）会当场全红，而 `RowFacts` 是 `rc-search` 与 `rc-agent` 共用的
/// 行事实形状——`recall-contract.md` §8-R1 的结论就是"不加字段，侧查"。
/// 侧查的形状是**批量点查一次**（`WHERE id IN (…)`），不是逐行一次：
/// 单次召回 ≤ `MAX_RECALL_ROWS` 行 ⇒ ≤1 条 SQL，与既有 `provenance_chain` 的那笔账同量级
/// 而更省（那里是 20 + 20×2）。
///
/// `time_confidence` 是**库里那一格的原文**，这一层不归一、不解释：它的值域没有 DDL CHECK
/// （`ddl.rs` 的 `card.time_confidence` 是裸 `TEXT`），写侧今天的形状按两族分：
/// - `writers_x::card_sql`（`put_card`/`put_event` 那一条 INSERT）显式写 `NULL,NULL,NULL,'unknown'`
///   ——裁定 13 的"无锚就写 unknown"；
/// - `writers_fact::CARD_INSERT`（fact 的最小陈述面消费卡）**列单里没有这一列** ⇒ 库里是 NULL；
/// - 时间线/链头两族按锚给 `'anchored'`/`'unknown'`（`writers_x.rs` 那两条派生 INSERT），
///   episode 镜像卡另有 `'speech'` 一支（[`crate::writers_x::episode_time_arm`]）。
///
/// **归一与渲染口径在消费面**（`rc_agent::recall::time_confidence_slug`），理由是守卫 12：
/// 读数只带身份值与 slug，闭集外的串不许原样吐进正文面；NULL 与 `'unknown'` 在那里同支。
/// `mode` 在 `card` 上是 NOT NULL DEFAULT `'stated'`（`ddl.rs` 的 `card` 建表那一格），所以只有 `anchor`
/// 那一族（结构上没有这两列）才会是 `None`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardMode {
    pub id: String,
    pub mode: Option<String>,
    pub time_confidence: Option<String>,
}

/// [`CardMode`] 的批量读法。`ids` 是**裸 card id**（不带 `card:` 前缀，与 [`lookup`] 同形）；
/// 空入参 ⇒ 空返回（不拼 `IN ()`）。返回顺序按 id 升序 = 与入参顺序无关的稳定读数，
/// 调用方按 id 取用。
pub fn card_modes(m: &MemoryHandle, ids: &[String]) -> Result<Vec<CardMode>, MemError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let list = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT id, mode, time_confidence FROM card WHERE id IN ({list}) ORDER BY id");
    let mut st = m.conn().prepare(&sql)?;
    let mut args: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(ids.len());
    for id in ids {
        args.push(id);
    }
    let rows = st.query_map(rusqlite::params_from_iter(args.iter()), |r| Ok(CardMode {
        id: r.get::<_, String>(0)?,
        mode: r.get::<_, Option<String>>(1)?,
        time_confidence: r.get::<_, Option<String>>(2)?,
    }))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// `card.time_json` 的读者侧解包（债 #9 · 修法甲，派发档 I-7）。
///
/// 修法甲的字面形状：写侧（`rc_distill::pass_time` 的三条 `CARD_*_BACKFILL` 与
/// `card_time_json` 形状）与 `as_of_ms` 列语义**都不动**；读者要事件时间时从这一格解
/// `time_json`。它是这一列**修前的第一个生产读者**（修前全仓零读者——写者、schema 登记、
/// 注释与断言写入的测试之外一个都不读，那是台账点名的"写了没人读"族第一实例）。
///
/// 事件时间的判据与写侧逐格对齐：`card_time_json` 的 `kind` 是解析 kinds 的封闭值
/// （`deadline`/`event_time`/`validity`/`recurrence`/`speech_time`，见 `pass_time::time_kind_for`），
/// 其中**只有 `kind='event_time'`**（`field='occurred'`，镜像 `fact.occurred_at_ms` 那一组）
/// 是"这件事发生在什么时候"；`due`/`valid` 两族是另外两种语义，`recurrence`/`speech_time`
/// 两族压根没有绝对事件时刻 ⇒ 只有 `start_ms` 为整数的 `event_time` 行才进返回。
/// `start_ms` 是事件开始（写侧 `EVENT_TIME_BACKFILL` 回填 `occurred_at_ms` 用的同一个数）；
/// `end_ms` 一并带出（区间事件有第二端），今天没有渲染面消费它——它是解析结果自带的半格，
/// 不是给"以后可能有读者"预留的（`pass_time` 的 JSON 形状里它本来就在）。
///
/// 脏数据策略与 [`json_array_strings`] 同族：`time_json` 是裸 TEXT、没有 CHECK，坏 JSON /
/// 缺键 / 类型不对一律当"没有事件时间"跳过，不报错不 panic（读侧不许因为一行残值崩掉）。
/// 解析在 SQL 里做（`json_extract`；`json_each` 在 [`card_fact_pointers`] 已有先例）——
/// **不引 `serde_json`**（`reader.rs` 头顶守卫 10：rc-memory 的依赖清单里没有它）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardEventTime {
    /// 裸 card id（与 [`card_modes`] 同形）。
    pub id: String,
    /// 事件开始（ms）。写侧回填 `fact.occurred_at_ms` 的同一个数。
    pub start_ms: i64,
    /// 事件结束（ms）；单点事件与开口区间为 `None`。
    pub end_ms: Option<i64>,
}

/// [`CardEventTime`] 的批量读法。`ids` 是裸 card id；空入参 ⇒ 空返回（不拼 `IN ()`）。
/// 入参里的重复 id 只读一次（内容寻址的 id 是唯一的，重复是调用方的账，不是读数的形状）。
/// 返回顺序按 id 升序 = 与入参顺序无关的稳定读数，调用方按 id 取用。
///
/// **为什么每个 `json_extract` 都套一层 `CASE WHEN json_valid(...)`**：SQLite 的
/// `json_extract` 撞上坏 JSON 会**抛错**（把整条查询带崩），而不是像解析器那样返回 NULL；
/// `CASE` 的分支是惰性求值的，`json_valid` 先行把关 ⇒ 坏 JSON / NULL 落到 ELSE（NULL），
/// 后面的判据自然为假。这一层护栏就是上面"脏数据不崩"承诺的执行点。
pub fn card_event_times(m: &MemoryHandle, ids: &[String]) -> Result<Vec<CardEventTime>, MemError> {
    let mut seen: Vec<&str> = Vec::new();
    let unique: Vec<&String> = ids.iter()
        .filter(|id| {
            if seen.contains(&id.as_str()) { false } else { seen.push(id.as_str()); true }
        })
        .collect();
    if unique.is_empty() {
        return Ok(Vec::new());
    }
    let list = unique.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT id, \
                CASE WHEN json_valid(time_json) THEN json_extract(time_json, '$.start_ms') END, \
                CASE WHEN json_valid(time_json) THEN json_extract(time_json, '$.end_ms') END \
           FROM card \
          WHERE id IN ({list}) \
            AND CASE WHEN json_valid(time_json) THEN json_extract(time_json, '$.kind') END \
                  = 'event_time' \
            AND CASE WHEN json_valid(time_json) \
                     THEN typeof(json_extract(time_json, '$.start_ms')) END = 'integer' \
          ORDER BY id");
    let mut st = m.conn().prepare(&sql)?;
    let mut args: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(unique.len());
    for id in &unique {
        args.push(*id);
    }
    let rows = st.query_map(rusqlite::params_from_iter(args.iter()), |r| {
        // `end_ms` 允许 null（开口区间）与非整数（坏 JSON）；两者都当"没有第二端"。
        let end = match r.get::<_, rusqlite::types::Value>(2)? {
            rusqlite::types::Value::Integer(ms) => Some(ms),
            _ => None,
        };
        Ok(CardEventTime {
            id: r.get::<_, String>(0)?,
            // `start_ms` 的整数性已由 WHERE 里的 `typeof(...)= 'integer'` 钉住。
            start_ms: r.get::<_, i64>(1)?,
            end_ms: end,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 因果链的一个**环节**（`provenance` 里 `role='link'` 的那一行，工单 §5-1 第一条的返回形状）。
/// 字段就是这一行今天**必然有值**的三格（派发档 §1a 明令按实测能读到的定，不许预留读不出来的格）：
/// - `target_id`：被引行的句柄——写侧 `put_chains` 把环节 id 落在 `turn_uid` 这一列
///   （`writers_x.rs` 的 `insert_derived_provenance`），所以环节"引的是哪一行"就读在这里；
/// - `ref_kind`：被引行所在表的前缀（写侧传 `link_table.prefix()`，两张可指认表之一）；
/// - `anchor_role`：被引行的最强锚角色（写侧 `anchor_role_of` 读出来的值，有真生产者）。
///
/// **不预留 `ord`**：环节的顺序由 `chain_links` 的 `ORDER BY ord` 保证在返回的 `Vec` 次序里，
/// 读回来就是一个**有序序列**——顺序已经是函数返回值的一部分，再把 `ord` 塞进结构体就是
/// "以后可能有别的读者"的预留字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainLink {
    pub target_id: String,
    pub ref_kind: String,
    pub anchor_role: AnchorRole,
}

/// 一条因果链头的**有序环节**列表（工单 §5-1 第一条）。谓词逐字照工单：
/// `WHERE mem_table=?1 AND mem_id=?2 AND role='link' ORDER BY ord`。
/// - 只取环节：`role='basis'` 的依据行不在这里——那是 `provenance_chain` 的 `Basis` 那一支的活；
/// - `ORDER BY ord`：环节序号（`0..n` 连续）由写侧盖下，本函数靠它把 `A→B` 与 `B→A` 读成两个
///   **不同**的序列（顺序读得回来是本批唯一的价值，P2 已在库面钉过，这一条钉在读侧）。
///
/// 入参 `key` 是链头那一行（`event(kind='causal')`）的句柄。链头不是因果链 / 环节全退场
/// ⇒ 返回空 `Vec`（不报错：链头行本身可以活着、只是它没有环节行，那是合法的形状）。
pub fn chain_links(m: &MemoryHandle, key: &MemKey) -> Result<Vec<ChainLink>, MemError> {
    let mut st = m.conn().prepare(
        "SELECT turn_uid, ref_kind, anchor_role FROM provenance
         WHERE mem_table=?1 AND mem_id=?2 AND role='link' ORDER BY ord")?;
    let rows = st.query_map(rusqlite::params![key.table.prefix(), key.id],
        |r| Ok(ChainLink {
            target_id: r.get(0)?,
            ref_kind: r.get(1)?,
            // 与 `provenance_chain` / `provenance_of` 同一把认法：认不出的 slug 折成 System。
            anchor_role: AnchorRole::parse(&r.get::<_, String>(2)?).unwrap_or(AnchorRole::System),
        }))?;
    let mut out = Vec::new();
    for row in rows { out.push(row?); }
    Ok(out)
}

/// **推导批的候选臂** = 两张可指认表的活行 **减去推导写的行**。臂清单来自 `ROW_TABLES`
/// （表清单进 `ROW_TABLES` 时跟着长，不必也不许再抄一遍表名）；活行谓词来自 `alive_where`
/// （"什么算活行"的家在 T0 那三支条件文本里，本函数不重判）；推导行谓词来自 `turn_basis_pred`。
///
/// `rows_since` 与 `count_rows_since` **共用这一份**——理由与 T0 写的那条同形：两条 SQL 各写一遍
/// 过滤，就是给 agent 面上那两格留一处会打架的余地。
///
/// **函数名从旧名改来**（**旧名全称在这段注释里同样不许出现**：写出来之后全仓 grep 旧名就不可能
/// 是零命中，而"零命中"正是改名改没改净的唯一机械证据）：旧名说的是"活行"，
/// 而本函数返回的是"活行 − 推导行"——
/// 推导行本身**是活行**（没退场、没被顶掉、没被否决），只是不该再被推导一轮、不该占审核额度。
/// 一个名字说谎的函数比改名贵（本仓纪律是注释不许说谎，函数名同理）。
fn derive_pool_union() -> String {
    ROW_TABLES.iter()
        .map(|t| format!("{} WHERE {} AND EXISTS (SELECT 1 FROM provenance dp WHERE {})",
                         select_sql(*t), alive_where(*t), turn_basis_pred(t.prefix(), "f.id")))
        .collect::<Vec<String>>()
        .join(" UNION ALL ")   // 尾空格是必需的：少了它 SQLite 把 "ALLSELECT" 当标识符 ⇒ 语法错
}

/// 推导批的"读侧"（裁定 6）：按 (created_at_ms, tbl, id) 全序取活行。
/// 死行不进批、不占行限——复活且已过水位者不追补，与"旧库判同结果不继承"同族。
/// `from` = 上次批的最后一行 `(created_at_ms, 游标键)`；`None` = 从头。**游标键的形状是契约**：
/// `"<tbl_prefix>:<id>"`（前缀取 `MemTable::prefix()`，与 `select_sql` 第一列的 `tbl` 同一来源）。
/// 少了前缀，同 ms 组里的跨表两行会互相顶掉——排序键有 `tbl` 而游标键没有，两者对"下一行是谁"
/// 给出相反答案，于是**静默漏选**（不报错、不红，只是永久少一批）。
/// `same_ms_group_pages_across_tables_on_the_cursor_tie_break` 钉的正是这一格。
/// 行限 `limit` 是上限（不是分页大小）——一批跑不完，**余量留下一轮**（P-24 同律）。
/// `limit as i64` 在 `usize::MAX` 附近会溢出成负数，而 SQLite 的负 `LIMIT` 等于无界；
/// 调用点给的是常数级小值，碰不到，但 `limit` 因此不是任意值（与 `unreviewed_rows` 同口径）。
///
/// 拼法与 `unreviewed_rows` 同形：一臂 = `select_sql(t) WHERE <alive_where(t)>
/// AND EXISTS (<turn_basis_pred(t.prefix(), "f.id")>)`，两臂 `UNION ALL`。
/// 臂文本来自 `derive_pool_union`，口径是**活行 − 推导行**（推导行已经是结论，不再被推导一轮）。
/// `select_sql` 的第一列已经是 `'<prefix>' AS tbl`，这里**不再另加** tbl 列；
/// 它给出的十九列正是 `Raw` 的形状，所以外面那层子查询用 `SELECT *` 包住即可，
/// 取值走 `read_raw`/`decode_raw`——不再逐行 `lookup`（那是 N+1，且 `lookup` 会二次过
/// `alive_where`，把"批里不许有死行"这条承诺变成两处真相）。
///
/// 游标过滤**必须套一层子查询**。SQLite 的复合选择语法里，挂在 `A UNION ALL B` 尾巴上的 `WHERE`
/// 只绑最后一个臂——臂自身无谓词时是**静默失效**（不报错、每轮重抽同一批），而本函数的两臂各自
/// 已带 `WHERE alive_where(t)`，同臂双 `WHERE` 会**当场语法错**。
/// 两种红形都指同一件事：外层那层子查询不能省。`limit_two_paginates_without_drop_or_duplicate`
/// 的牙就在这条上。
///
/// 两条臂的占位符编号各自自洽：带游标 = `?1`/`?2` 游标、`?3` 行限；不带游标 = 只有 `?1` 行限。
/// 不合并成一条 SQL 是因为参数个数与占位符个数对不上时 rusqlite 直接 `InvalidParameterCount`。
pub fn rows_since(
    m: &MemoryHandle, from: Option<(i64, String)>, limit: usize,
) -> Result<Vec<RowFacts>, MemError> {
    let compound = derive_pool_union();
    let tail = " ORDER BY created_at_ms ASC, tbl ASC, id ASC";
    let sql = match &from {
        Some(_) => format!("SELECT * FROM ({compound}) \
                            WHERE created_at_ms > ?1 OR (created_at_ms = ?1 AND (tbl || ':' || id) > ?2)\
                            {tail} LIMIT ?3"),
        None => format!("SELECT * FROM ({compound}){tail} LIMIT ?1"),
    };
    let mut st = m.conn().prepare(&sql)?;
    let rows = match &from {
        Some((ms, key)) => st.query_map(rusqlite::params![ms, key, limit as i64], read_raw)?,
        None => st.query_map(rusqlite::params![limit as i64], read_raw)?,
    };
    let mut out = Vec::new();
    for raw in rows {
        out.push(decode_raw(raw?)?);
    }
    Ok(out)
}

/// 水位之后**还剩几行推导池里的行没进过批**（`status.derive_lag_rows` 的生产者）。
/// 结构化视图读的一行事实（消费面 `overview / agenda / timeline`）。
///
/// 它不是 `RowFacts`：`fact` 不在 `ROW_TABLES` 里（读面是 card/anchor 的投影），
/// 而这三个视图要的不是"候选集"，是**按类型/时间/截止日的确定性查询**。
/// 列只取视图用得到的那些；正文仍是 `fact.text`，不在这里做摘要。
#[derive(Debug, Clone)]
pub struct FactRow {
    pub id: String,
    pub subject_anchor_id: String,
    pub category: String,
    pub text: String,
    pub due_at_ms: Option<i64>,
    pub status: Option<String>,
    pub occurred_at_ms: Option<i64>,
    pub valid_from_ms: Option<i64>,
    pub valid_to_ms: Option<i64>,
    pub superseded_by: Option<String>,
    pub scope_kind: String,
    pub scope_key: String,
    pub source_agent: String,
    pub device_id: String,
    pub created_at_ms: i64,
    /// 视图排序用的时间：occurred → valid_from → created（与时间线口径一致）。
    pub at_ms: i64,
}

/// 一行 `FactRow` 的列清单（**唯一的一份**，`fact_rows` 与 `episode_members` 共用）。
///
/// 抽成常量而不是各写一遍（守卫 3）：这两条查询的 `FROM`/`ORDER BY` 不同（一条按视图序取窗口，
/// 一条按 `episode_fact.ord` 取成员），但**投影的十六格必须逐字同形**——漂一格就是同一个
/// `FactRow` 在两个读面上指不同的列，而那是一种不报错的错。
const FACT_VIEW_COLUMNS: &str = "f.id, f.subject_anchor_id, f.category, f.text, f.due_at_ms, f.status,
                f.occurred_at_ms, f.valid_from_ms, f.valid_to_ms, f.superseded_by,
                f.scope_kind, f.scope_key, f.source_agent, f.device_id, f.created_at_ms,
                COALESCE(f.occurred_at_ms, f.valid_from_ms, f.created_at_ms) AS at_ms";

/// [`FACT_VIEW_COLUMNS`] 的解码（同一份列清单的唯一读法）。
fn decode_fact_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<FactRow> {
    Ok(FactRow {
        id: r.get(0)?,
        subject_anchor_id: r.get(1)?,
        category: r.get(2)?,
        text: r.get(3)?,
        due_at_ms: r.get(4)?,
        status: r.get(5)?,
        occurred_at_ms: r.get(6)?,
        valid_from_ms: r.get(7)?,
        valid_to_ms: r.get(8)?,
        superseded_by: r.get(9)?,
        scope_kind: r.get(10)?,
        scope_key: r.get(11)?,
        source_agent: r.get(12)?,
        device_id: r.get(13)?,
        created_at_ms: r.get(14)?,
        at_ms: r.get(15)?,
    })
}

/// 按 scope / 时间窗读活着的 fact 行。**确定性查询，不调模型**。
///
/// 活行口径与 `alive_where` 一致（未退场、未被顶掉、未被否决），但 `fact` 没有
/// `MemTable` variant，所以三条谓词在这里按同一个家（`retirement_pred`/`rejection_pred`）
/// 拼一次，不另写一份规则。`limit` 由调用方夹紧（视图层夹到 1..=50）。
pub fn fact_rows(
    m: &MemoryHandle, scope_key: Option<&str>, from_ms: Option<i64>, to_ms: Option<i64>,
    limit: usize,
) -> Result<Vec<FactRow>, MemError> {
    let sql = format!(
        "SELECT {FACT_VIEW_COLUMNS}
           FROM fact f
          WHERE f.superseded_by IS NULL
            AND NOT EXISTS (SELECT 1 FROM retirement rt WHERE {})
            AND NOT EXISTS (SELECT 1 FROM attestation aj WHERE {})
            AND (?1 IS NULL OR f.scope_key = ?1)
            AND (?2 IS NULL OR COALESCE(f.occurred_at_ms, f.valid_from_ms, f.created_at_ms) >= ?2)
            AND (?3 IS NULL OR COALESCE(f.occurred_at_ms, f.valid_from_ms, f.created_at_ms) <= ?3)
          ORDER BY at_ms DESC, f.id
          LIMIT ?4",
        retirement_pred("fact", "f.id"), rejection_pred("fact", "f.id")
    );
    let mut st = m.conn().prepare(&sql)?;
    let rows = st.query_map(
        rusqlite::params![scope_key, from_ms, to_ms, limit as i64],
        decode_fact_row,
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 一段经历的成员，**按 `episode_fact.ord` 读回**（派发档 I-3a 交付 3 · 正向）。
///
/// 这是新写的一条 SELECT，**不复用 [`fact_rows`]**（母档 §2.3 指定）：那一条按
/// `at_ms DESC, f.id` 排、带 scope/时间窗/存活筛，是三个结构化视图用的另一形；把它的
/// 排序套到成员上，"归组层定下的事件序"就在读侧被覆盖成视图序——那是一种不报错的走样。
/// 两条查询共用 [`FACT_VIEW_COLUMNS`] 与 [`decode_fact_row`]（投影十六格不许各写一份）。
/// 索引 `episode_fact_ord`（`rc-store/src/ddl.rs` 里随 `episode_fact` 建表一起建的那条）现成，不改 DDL。
///
/// 两件要说清的取舍：
/// ① **不筛死活**。成员表说的是"当初哪几条被归在一起"，`ord` 的连续性正是这一族的断言面
///    （同因果链环节序列那一格）；在这里滤掉死成员会留下序号空洞，而空洞与"当初就少一条"
///    在库里长得一样。行的生死由调用方看 `FactRow::superseded_by`/`status` 自己判。
/// ② 返回的是 `FactRow`（裸 id，无 `MemKey`）——`fact` 不在 `ROW_TABLES` 里，
///    而 `episode` 的稳定消费句柄是它的镜像卡（母档 §2.3 + 派发档 I-3a 设计要点）。
pub fn episode_members(m: &MemoryHandle, episode_id: &str) -> Result<Vec<FactRow>, MemError> {
    let sql = format!(
        "SELECT {FACT_VIEW_COLUMNS}
           FROM episode_fact ef JOIN fact f ON f.id = ef.fact_id
          WHERE ef.episode_id = ?1
          ORDER BY ef.ord");
    let mut st = m.conn().prepare(&sql)?;
    let rows = st.query_map([episode_id], decode_fact_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 反向查：这一行 fact 属于**哪一段活着的经历**（交付 3 的第二支；Phase 4 的召回要从成员回读
/// 它所在的那段，母档 §5 说正向优先，但两支都要有）。
///
/// **只认 `superseded_by IS NULL` 的那一条**：成员集变化时旧行只是结构史，把它当答案会说谎
/// （用例见 `tests/episode.rs` 的 `成员集变大时旧行与旧卡被指过去而新行是独立句柄`）。
/// 一个成员今天至多落在一行活着的 episode 里——那是写侧 [`crate::writers_x::put_episodes`]
/// 顶掉前驱时保证的（同归属 + 共享成员 ⇒ 旧行必被指走），不是这里筛出来的；`ORDER BY e.id
/// LIMIT 1` 只把"万一并发长出两行"收成确定的一条，方向是 fail-toward-nothing（返回更早的
/// 那个 id），不掩盖它。
pub fn episode_of_fact(m: &MemoryHandle, fact_id: &str) -> Result<Option<String>, MemError> {
    let got = m.conn().query_row(
        "SELECT e.id FROM episode_fact ef JOIN episode e ON e.id = ef.episode_id
          WHERE ef.fact_id = ?1 AND e.superseded_by IS NULL
          ORDER BY e.id LIMIT 1",
        [fact_id], |r| r.get::<_, String>(0));
    match got {
        Ok(id) => Ok(Some(id)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(MemError::from(e)),
    }
}

/// 归组内核读的一行 fact（派发档 I-3b 的只读口）。
///
/// **为什么不是 `FactRow`**：`FACT_VIEW_COLUMNS` 那十六格里没有归组要的东西（参与者、说话人、
/// 证据轮、`occurred_end_ms`、署名），却带着三个视图用的 `due_at_ms`/`valid_from`/`at_ms`。
/// 特别是最后那一格：`at_ms = COALESCE(occurred, valid_from, created)` 是**视图排序轴**，
/// 而 D9 的时间轴只许 `occurred_at_ms`（事件时间，已知）或 `created_at_ms`（发话时刻）——
/// 复用 `at_ms` 会把 `valid_from_ms`（状态生效轴，S0 §1.1 实测 0/36）悄悄放进邻近判定里，
/// 那是一格"读了但没人登记过它是第三口钟"的错。所以这里给的是裸列，轴的拼法留给归组层。
///
/// `subject_kind` / `subject_canonical` 是两格**JOIN 来的锚面**（不是 fact 的列）：
/// 归组要判"这一行的主语是不是说话 agent 自己的锚"（S0 §2.2 的自锚污染：`writers_fact.rs` 的 `put_fact_rows_with_extras`
/// 无条件把 subject 并进 `participants_json`），而库里唯一写着"这个锚是谁"的地方是 `anchor` 表。
/// 锚行已被删（悬空引用，今天 FK 拦着，仍按 `None` 读）⇒ 两格 `None` ⇒ 归组层认不出它是自锚，
/// 方向是**不剥**（宁可少剥一格，也不凭猜把一个人当成 agent）。
///
/// 只 `derive(Debug, Clone)`：`keys::Scope` 没有 `PartialEq`/`Default`，而本层不许为了
/// 一个结构体的派生去动 `keys.rs`（不在落点内）——比较一律按字段比（见 `rc-distill::episode`）。
#[derive(Debug, Clone)]
pub struct GroupFact {
    pub id: String,
    /// `fact.participants_json` 里的**裸 anchor id**（母档 §2.4：现网 36/36 都是裸 64-hex）。
    pub participants: Vec<String>,
    pub speaker_anchor_id: Option<String>,
    pub subject_anchor_id: String,
    pub subject_kind: Option<String>,
    pub subject_canonical: Option<String>,
    /// 这一行的署名（`BatchCtx::source_agent` 的落库列）。自锚判据的另一半。
    pub source_agent: String,
    pub device_id: String,
    pub scope: Scope,
    pub world_id: Option<String>,
    pub category: String,
    pub text: String,
    pub occurred_at_ms: Option<i64>,
    pub occurred_end_ms: Option<i64>,
    /// **发话时刻**（S0 §1.3：36/36 与证据轮 `turns.created_at_ms` 逐字节相等，不是导入钟）。
    pub created_at_ms: i64,
    pub evidence_turn_uid: String,
}

/// 归组候选行的列清单（唯一一份，`group_facts` 与 `count_group_facts_after` 共用同一个活行谓词）。
const GROUP_FACT_COLUMNS: &str = "f.id, f.participants_json, f.speaker_anchor_id, f.subject_anchor_id, \
    f.source_agent, f.device_id, f.scope_kind, f.scope_key, f.world_id, f.category, f.text, \
    f.occurred_at_ms, f.occurred_end_ms, f.created_at_ms, f.evidence_turn_uid, \
    (SELECT a.kind FROM anchor a WHERE a.id = f.subject_anchor_id), \
    (SELECT a.canonical FROM anchor a WHERE a.id = f.subject_anchor_id)";

/// fact 的活行谓词。**与 `fact_rows` 逐字同形**（同一个家：`retirement_pred`、`rejection_pred`
/// 与 `superseded_by IS NULL` 三条），因为"还活着的 fact"在两处必须是同一批行——
/// 视图少一行、归组多一段经历，就是"库里有、下发看不见"那一族的镜像。
fn fact_alive_where() -> String {
    format!("f.superseded_by IS NULL \
             AND NOT EXISTS (SELECT 1 FROM retirement rt WHERE {}) \
             AND NOT EXISTS (SELECT 1 FROM attestation aj WHERE {})",
            retirement_pred("fact", "f.id"), rejection_pred("fact", "f.id"))
}

fn decode_group_fact(r: &rusqlite::Row<'_>) -> rusqlite::Result<GroupFact> {
    let participants_json: String = r.get(1)?;
    let kind: String = r.get(6)?;
    Ok(GroupFact {
        id: r.get(0)?,
        participants: json_array_strings(&participants_json),
        speaker_anchor_id: r.get(2)?,
        subject_anchor_id: r.get(3)?,
        subject_kind: r.get(15)?,
        subject_canonical: r.get(16)?,
        source_agent: r.get(4)?,
        device_id: r.get(5)?,
        scope: Scope { kind: ScopeKind::parse(&kind).unwrap_or(ScopeKind::Project), key: r.get(7)? },
        world_id: r.get(8)?,
        category: r.get(9)?,
        text: r.get(10)?,
        occurred_at_ms: r.get(11)?,
        occurred_end_ms: r.get(12)?,
        created_at_ms: r.get(13)?,
        evidence_turn_uid: r.get(14)?,
    })
}

/// 按 `(created_at_ms, id)` **升序**取归组用的活 fact 行（派发档 I-3b：归组的扫描面）。
///
/// 与 `fact_rows` 的三处不同，每一处都是归组自己的口径：
/// ① 升序（`fact_rows` 是 `at_ms DESC`，那是"最近先看"的视图序；扫描要的是发话序）；
/// ② 不带 `valid_from_ms` 那条轴（见 [`GroupFact`]）；
/// ③ 无 scope 过滤：归组在**全库活行**上做，段内再按归属分块（不跨归属由写入口执法，
///    `writers_x::shared_member_scope`），在这里筛 scope 等于把"跨归属"这一格变成读不出来的东西。
///
/// `limit` 是这一趟的**扫描帽**（调用方 `episode::EPISODE_MAX_FACTS_PER_PASS` 逐趟放大）。
/// 这里刻意**没有**游标形参：归组是"窗内整段重算"（新行可能顶掉旧段），分页会让跨界的一段经历
/// 永远长不出来；`count_group_facts_after` 那一支才是游标该去的地方（它数的是帽之外的余量）。
pub fn group_facts(m: &MemoryHandle, limit: usize) -> Result<Vec<GroupFact>, MemError> {
    let alive = fact_alive_where();
    let sql = format!("SELECT {GROUP_FACT_COLUMNS} FROM fact f WHERE {alive} \
                       ORDER BY f.created_at_ms ASC, f.id ASC LIMIT ?1");
    let mut st = m.conn().prepare(&sql)?;
    let rows = st.query_map(rusqlite::params![limit as i64], decode_group_fact)?;
    let mut out = Vec::new();
    for row in rows { out.push(row?); }
    Ok(out)
}

/// 扫描窗之后**还剩几行活 fact**（`hub episode run` 的 `lag_rows` 生产者）。
/// 与 [`group_facts`] 同一个 `fact_alive_where()`（守卫 3：「还剩几行」与「这一趟扫到几行」
/// 必须共享同一个活行口径，否则余量与窗对不上账）。`None` = 全库活行数。
pub fn count_group_facts_after(m: &MemoryHandle, from: Option<(i64, String)>)
    -> Result<i64, MemError> {
    let alive = fact_alive_where();
    let sql = match &from {
        Some(_) => format!("SELECT count(*) FROM fact f WHERE {alive} \
                            AND (f.created_at_ms > ?1 OR (f.created_at_ms = ?1 AND f.id > ?2))"),
        None => format!("SELECT count(*) FROM fact f WHERE {alive}"),
    };
    let mut st = m.conn().prepare(&sql)?;
    let n: i64 = match &from {
        Some((ms, id)) => st.query_row(rusqlite::params![ms, id], |r| r.get(0))?,
        None => st.query_row([], |r| r.get(0))?,
    };
    Ok(n)
}

/// 与 `rows_since` 同臂同游标：臂文本来自 `derive_pool_union()`，游标谓词是下面这个常量。
/// 差别只有两处——不排序（计数无所谓）、不带 LIMIT（要的是余量全量，不是这一批）。
/// 死行同样不计，推导写的行同样不计（它们已经是结论，不是"还没推导的依据"）：
/// "还剩几行"与"下一批取到几行"必须共享同一个"推导池口径"的家（守卫 3）。
/// `from=None` ⇒ 全库推导池行数。
pub fn count_rows_since(m: &MemoryHandle, from: Option<(i64, String)>) -> Result<i64, MemError> {
    const WATERMARK: &str = " WHERE created_at_ms > ?1 \
                             OR (created_at_ms = ?1 AND (tbl || ':' || id) > ?2)";
    let compound = derive_pool_union();
    let sql = match &from {
        Some(_) => format!("SELECT count(*) FROM ({compound}){WATERMARK}"),
        None => format!("SELECT count(*) FROM ({compound})"),
    };
    let mut st = m.conn().prepare(&sql)?;
    let n: i64 = match &from {
        Some((ms, key)) => st.query_row(rusqlite::params![ms, key], |r| r.get(0))?,
        None => st.query_row([], |r| r.get(0))?,
    };
    Ok(n)
}

/// 一条活着的当前状态（P2 的读对面）。`id` 与 `subject_anchor_id` 都在场：前者给读闸判
/// （`GateIndex::allows("state_assertion", id)`），后者给调用方连接与渲染。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveState {
    pub id: String,
    pub subject_anchor_id: String,
    pub dimension: String,
    pub value_after: String,
    /// `expressed`（原文直陈）或 `inferred`（程序判出的降级，C1）。消费者必须把两者分开
    /// 呈现：一条推断行读起来若与直陈行同形，降级就没有意义。
    pub mode: String,
    pub valid_from_ms: Option<i64>,
    pub evidence_turn_uid: Option<String>,
}

/// 九个维度的中文读法。**值域的家是 `rc-store` 的 DDL CHECK**（九值闭集），这里只做渲染，
/// 但必须是唯一一处：`overview` 的 `brief` 与召回面的状态块若各写一份词表，
/// 同一个维度在两处会说出两种话，读者分不清是两格还是一格写歪了。
/// 闭集外的词落成"状态"这个通用说法——不 panic 也不静默丢行（行仍在 JSON 里，原维度名可见）。
pub fn dimension_label(dimension: &str) -> &'static str {
    match dimension {
        "emotion" => "情绪",
        "mood" => "心情",
        "sentiment" => "态度",
        "affect_episode" => "情绪片段",
        "trust" => "信任",
        "relationship" => "关系",
        "health" => "健康",
        "config" => "配置",
        "progress" => "进度",
        _ => "状态",
    }
}

/// 当前状态行：`valid_to_ms IS NULL`，按生效时刻从新到旧。
///
/// `subjects` 给 `None` = 不限主体（`overview` 用那一支），给 `Some(ids)` = 只取这些主体
/// （召回面"查询点到了谁"用那一支）。`limit` 是**库侧**的帽：调用方还要过读闸再截，
/// 所以这里给足余量（读闸可能把前排整片拿掉，先截后滤会读出"库里有、我看不见"）。
pub fn live_states(
    m: &MemoryHandle, subjects: Option<&[String]>, limit: usize,
) -> Result<Vec<LiveState>, MemError> {
    const COLUMNS: &str = "id, subject_anchor_id, dimension, value_after, mode, valid_from_ms, \
                           evidence_turn_uid";
    let read = |r: &rusqlite::Row| -> Result<LiveState, rusqlite::Error> {
        Ok(LiveState {
            id: r.get(0)?,
            subject_anchor_id: r.get(1)?,
            dimension: r.get(2)?,
            value_after: r.get(3)?,
            mode: r.get(4)?,
            valid_from_ms: r.get(5)?,
            evidence_turn_uid: r.get(6)?,
        })
    };
    let sql = match subjects {
        None => format!(
            "SELECT {COLUMNS} FROM state_assertion WHERE valid_to_ms IS NULL \
             ORDER BY valid_from_ms DESC, id LIMIT ?1"
        ),
        Some(ids) => format!(
            "SELECT {COLUMNS} FROM state_assertion WHERE valid_to_ms IS NULL \
               AND subject_anchor_id IN ({}) \
             ORDER BY valid_from_ms DESC, id LIMIT ?{}",
            ids.iter().map(|_| "?").collect::<Vec<_>>().join(","),
            ids.len() + 1
        ),
    };
    let mut st = m.conn().prepare(&sql)?;
    let rows = match subjects {
        None => st.query_map(rusqlite::params![limit as i64], read)?,
        Some(ids) => {
            let cap = limit as i64;
            let mut args: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(ids.len() + 1);
            for id in ids {
                args.push(id);
            }
            args.push(&cap);
            st.query_map(rusqlite::params_from_iter(args.iter()), read)?
        }
    };
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

/// 一段文本里点到了哪些活锚（按 `canonical` 与其全部别名，用 [`name_occurs`] 那一把
/// 带词边界的尺子——单字/短名不命中任意正文，与审核闸同一处真相）。
///
/// 扫描面有界：`max_scan` 条最近的活锚（按创建时刻倒序）。它服务的是"查询里提了谁"，
/// 不是全库实体识别——后者是 naming 的事，不在读侧现算。命中 `max_hits` 个就停。
pub fn anchors_named_in(
    m: &MemoryHandle, text: &str, max_scan: usize, max_hits: usize,
) -> Result<Vec<(String, String)>, MemError> {
    let mut hits: Vec<(String, String)> = Vec::new();
    if text.trim().is_empty() {
        return Ok(hits);
    }
    let mut candidates: Vec<(String, String, String)> = {
        let mut st = m.conn().prepare(
            "SELECT a.id, a.canonical, \
                    (SELECT group_concat(al.alias, '\u{1f}') FROM alias al WHERE al.anchor_id = a.id) \
             FROM anchor a WHERE a.status IN ('active','provisional') \
             ORDER BY a.created_at_ms DESC, a.id LIMIT ?1",
        )?;
        let rows = st.query_map(rusqlite::params![max_scan as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    // 稳定顺序：先按创建时刻（越新越近），同刻按 id——两次调用给同一串命中。
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    for (id, canonical, aliases) in candidates {
        let named = name_occurs(text, &canonical)
            || aliases
                .split('\u{1f}')
                .any(|alias| !alias.is_empty() && name_occurs(text, alias));
        if named {
            hits.push((id, canonical));
            if hits.len() >= max_hits {
                break;
            }
        }
    }
    Ok(hits)
}

#[cfg(test)]
mod value_sentence_tests {
    use super::{first_person_in_value_sentence, name_in_value_sentence, sentence_spans};

    /// 全角终止符恒切：一句话一个句界。
    #[test]
    fn 全角句号分号是句界() {
        let body = "张三这两天焦虑得很；晚上睡不着。小海倒是挺平静。";
        assert_eq!(
            sentence_spans(body),
            vec!["张三这两天焦虑得很；", "晚上睡不着。", "小海倒是挺平静。"]
        );
    }

    /// 换行恒切（一轮里的分行就是分句）。
    #[test]
    fn 换行是句界() {
        let body = "第一行说的是一件事\n第二行说的是另一件事";
        assert_eq!(
            sentence_spans(body),
            vec!["第一行说的是一件事", "第二行说的是另一件事"]
        );
    }

    /// 防误切①：小数点。`1.6.0` 里的句点后面不是空白 ⇒ 不是句界。
    #[test]
    fn 小数点不是句界() {
        let body = "版本 1.6.0 可以正常启动。";
        assert_eq!(sentence_spans(body), vec!["版本 1.6.0 可以正常启动。"]);
    }

    /// 防误切②：结尾是空格的点分缩写（`e.g.` / `i.e.`）不切。
    #[test]
    fn 点分缩写不是句界() {
        let body = "用浏览器连接器 e.g. browser-use 就能抓。";
        assert_eq!(sentence_spans(body), vec!["用浏览器连接器 e.g. browser-use 就能抓。"]);
    }

    /// 防误切③：域名/无空格缩写里的句点后面紧跟非空白或点后仍是词字符。
    #[test]
    fn 域名不是句界() {
        let body = "文档在 www.example.com 上，另见 U.S.A 那节。";
        assert_eq!(sentence_spans(body), vec!["文档在 www.example.com 上，另见 U.S.A 那节。"]);
    }

    /// 真句界：ASCII 句点后接空白仍是句界（上面三条防误切不能把它一起放过）。
    #[test]
    fn 句点后接空白仍是句界() {
        assert_eq!(sentence_spans("Done. Next one."), vec!["Done.", "Next one."]);
    }

    /// 共现：名字与值同句 ⇒ 成立。
    #[test]
    fn 名字与值同句_成立() {
        let body = "张三这两天焦虑得很，晚上睡不着。";
        assert!(name_in_value_sentence(body, "焦虑得很", "张三"));
    }

    /// 跨句借名：值句里没有名字，名字在同一条正文的另一句 ⇒ 不成立。
    #[test]
    fn 名字只在别的句子里_不成立() {
        let body = "可以，而且这台机器的浏览器连接器已经在线。\n\n1. **Qoder 浏览器连接器（browser-use）** 直接操作浏览器。";
        assert!(!name_in_value_sentence(body, "浏览器连接器已经在线", "Qoder"));
        // 同一个名字出现在**承载值的那一句**里，则成立——证明上面那句不是"名字不在正文"。
        assert!(name_in_value_sentence(body, "浏览器连接器", "Qoder"));
    }

    /// 名字面仍是 `name_occurs` 的口径：ASCII 名按词边界，`lin` 不命中 `lincoln`。
    #[test]
    fn 共现沿用词边界尺子() {
        assert!(!name_in_value_sentence("lincoln 这台机器上跑得很好", "跑得很好", "lin"));
        assert!(name_in_value_sentence("lin 这台机器上跑得很好", "跑得很好", "lin"));
    }

    /// 值不在正文里（或值是空串）⇒ 没有值句 ⇒ 一律不成立。
    #[test]
    fn 找不到值句一律不成立() {
        assert!(!name_in_value_sentence("张三很平静。", "焦虑", "张三"));
        assert!(!name_in_value_sentence("张三很平静。", "  ", "张三"));
        assert!(!first_person_in_value_sentence("张三很平静。", "焦虑"));
    }

    /// 第一人称标记：只在承载值的那一句里算。
    #[test]
    fn 第一人称只在值句里算() {
        let body = "我这轮跑得挺顺。测试全过了。";
        assert!(first_person_in_value_sentence(body, "跑得挺顺"));
        assert!(!first_person_in_value_sentence(body, "测试全过了"));
        // `我们`/`咱们` 含 `我`/`咱`，同样算（它们是代词不是名字，不走词边界尺子）。
        assert!(first_person_in_value_sentence("我们这轮跑得挺顺。", "跑得挺顺"));
        assert!(first_person_in_value_sentence("咱们这轮跑得挺顺。", "跑得挺顺"));
    }
}
