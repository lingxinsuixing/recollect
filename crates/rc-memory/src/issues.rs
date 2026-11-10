//! `extraction_issue` 的写入、读取与解决面（设计 `2026-10-27-batch-quarantine-and-rerun` §4）。
//!
//! 这一层**不解析 `detail`**：与 `review.rs` 同一条纪律——载荷是调用方拼好的 opaque 字符串，
//! 本层只做参数化 SQL 与状态戳。解析落在有 `serde_json` 的调用方（`hub derive retry`）。
//! 守卫 12 照旧：本文件任何错误都只带静态短语或行 id，不带 detail、路径或正文。
//!
//! `pass` 列**没有** CHECK 闭集（DDL 实测），所以这里的常量是生产侧的拼写家，不是 schema 约束。

use crate::keys::{MemTable, Scope, ScopeKind, Verdict};
use crate::writers_c::{VERDICT_SOURCE_AUDIT, VERDICT_SOURCE_REVIEW_APPLY};
use crate::MemError;
use rc_contract::normalize_body;
use rc_store::MemoryHandle;
use rusqlite::{params, OptionalExtension};
use std::collections::{HashMap, HashSet};

/// 推导家族在 `extraction_issue.pass` 里的落点。
pub const DERIVE_PASS: &str = "derive";
/// 契约丢弃的封闭类别（与 `DeriveReadout::drop_reason` 同一个词）。
pub const DERIVE_SCHEMA_CODE: &str = "schema";

/// W14e：两个悬空桶的 `code` 拼写。只住这一处——写入侧（`writers_v4`）、
/// 读侧（`doctor`）与测试都从这里取，任何一处手写第二个副本，两个桶就会在
/// 某次改名之后各自数各自的。
///
/// 第一个是"真名/真歧义"桶（语义与 W14e 之前逐字相同）；第二个是"形状类"
/// 诊断桶——它不是待人工解决队列，只是"模板让我们别收、模型还是塞进来了"的读数。
pub const UNRESOLVED_SURFACE_CODE: &str = "unresolved_surface";
pub const UNRESOLVED_SURFACE_SHAPE_CODE: &str = "unresolved_surface_shape";

/// `run_id` 形（`derive-<now_ms>`）的唯一算法：写入侧（`rc-distill::derive`）与
/// 读回侧（`hub cycle once` 的读数、`hub derive retry` 的审计）共用这一处，
/// 两处各写一遍 `format!` 就是 P-63 那一族——漂的那个字符不报错，它只让读回变成 `None`。
pub fn derive_run_id(now_ms: i64) -> String {
    format!("derive-{now_ms}")
}

/// 一行问题审计。`detail` 是调用方拼的 JSON 串，本层原样存、原样回。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub id: i64,
    pub run_id: String,
    pub pass: String,
    pub code: String,
    pub detail: String,
    pub resolved_at_ms: Option<i64>,
}

/// 落一行 issue 并返回它的自增 id。调用方负责 `detail` 里不带正文/路径（守卫 12）。
pub fn record(
    m: &MemoryHandle,
    pass: &str,
    code: &str,
    run_id: &str,
    detail: &str,
    now_ms: i64,
) -> Result<i64, MemError> {
    m.conn().execute(
        "INSERT INTO extraction_issue(run_id, pass, code, detail, created_at_ms)
         VALUES(?1, ?2, ?3, ?4, ?5)",
        params![run_id, pass, code, detail, now_ms],
    )?;
    Ok(m.conn().last_insert_rowid())
}

/// 按 id 读一行。不存在 ⇒ `None`（不是错误：运维点名一个不存在的 id 是输入问题，
/// 该由调用方把它变成退出码 3 的运维错误，而不是一条 `Sql` 故障）。
pub fn get(m: &MemoryHandle, id: i64) -> Result<Option<Issue>, MemError> {
    Ok(m.conn().query_row(
        "SELECT id, run_id, pass, code, detail, resolved_at_ms
           FROM extraction_issue WHERE id=?1",
        params![id],
        |r| Ok(Issue {
            id: r.get(0)?,
            run_id: r.get(1)?,
            pass: r.get(2)?,
            code: r.get(3)?,
            detail: r.get(4)?,
            resolved_at_ms: r.get(5)?,
        }),
    ).optional()?)
}

/// 把一行标成已解决（`resolved_at_ms=now`）。返回**是否命中一行**：
/// 0 行是"id 不存在"，由调用方决定怎么报；`id` 是主键，命中数不可能 > 1。
pub fn resolve(m: &MemoryHandle, id: i64, now_ms: i64) -> Result<bool, MemError> {
    let n = m.conn().execute(
        "UPDATE extraction_issue SET resolved_at_ms=?2 WHERE id=?1",
        params![id, now_ms],
    )?;
    Ok(n == 1)
}

/// 某一轮 `run_id` 落下的最新一条 issue id。`hub cycle once` 的 `derive_quarantined` /
/// `derive_issue_id` 两格靠它读回**本轮**的隔离记录（`run_id` 里带 `now_ms`，一轮一条）。
pub fn find_by_run(m: &MemoryHandle, run_id: &str) -> Result<Option<i64>, MemError> {
    Ok(m.conn().query_row(
        "SELECT id FROM extraction_issue WHERE run_id=?1 ORDER BY id DESC LIMIT 1",
        params![run_id], |r| r.get(0),
    ).optional()?)
}

/// 某一 pass 的未解决 issue 数。`hub derive status` 的 `open_issues` 是它。
pub fn open_count(m: &MemoryHandle, pass: &str) -> Result<i64, MemError> {
    Ok(m.conn().query_row(
        "SELECT count(*) FROM extraction_issue WHERE pass=?1 AND resolved_at_ms IS NULL",
        params![pass], |r| r.get(0),
    )?)
}

/// 某一 pass 最老的未解决 issue 的 `created_at_ms`。空集 ⇒ `None`（不是 `Some(0)`：
/// "没有未解决"与"很久以前有一条"是两件事，与 `water::last_extract_at_ms` 同一条口径）。
pub fn oldest_open_created_at(m: &MemoryHandle, pass: &str) -> Result<Option<i64>, MemError> {
    Ok(m.conn().query_row(
        "SELECT min(created_at_ms) FROM extraction_issue
          WHERE pass=?1 AND resolved_at_ms IS NULL",
        params![pass], |r| r.get(0),
    )?)
}

/// W14e：两个悬空桶的未解决行数，`(真名桶, 形状桶)`。
///
/// 谓词只在这一处：`resolved_at_ms IS NULL`——`0` 是"库里干净"这个**读数**，
/// 不是读失败。两个桶一次查出来而不是两次 `open_count_by_code` 调用，理由就是
/// P0-3 要消掉的那件事：分开写两份 SQL，两份谓词迟早会漂。
pub fn unresolved_surface_counts(m: &MemoryHandle) -> Result<(i64, i64), MemError> {
    Ok(m.conn().query_row(
        "SELECT coalesce(sum(code=?1),0), coalesce(sum(code=?2),0)
           FROM extraction_issue WHERE resolved_at_ms IS NULL",
        params![UNRESOLVED_SURFACE_CODE, UNRESOLVED_SURFACE_SHAPE_CODE],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

/// W15b2：同一格身份上出现 ≥2 个不同 `kind` 的活锚的**组数**。
///
/// 分组键是 `(scope_kind, scope_key, 归一正名)`——与写侧跨 kind 合流、候选池去重同一把尺。
/// 只数"活锚"：状态是 `active`/`provisional` **且**过 [`crate::reader::alive_where`] 的三支
/// （未退场、未被顶掉、未否决）；退场或合并掉的第二格不再算"库里还有一对"。
/// 组里出现两种以上 kind 才算数：同 kind 同名同 scope 的重复（内容寻址理论上不该有）不在
/// 这一格的主语里，它是另一件事。
///
/// 归一在 Rust 侧做（[`normalize_body`] 与 `compute_mem_id` 用的是同一支），SQL 只负责把
/// 活锚的四个面读回来：`kind` 只有九值、正名长度是名字量级，一次全扫在批级规模上够用。
/// `0` 是"库里的锚没有跨 kind 的同名对"这个**读数**，不是读失败。
pub fn cross_kind_duplicate_groups(m: &MemoryHandle) -> Result<i64, MemError> {
    let alive = crate::reader::alive_where(MemTable::Anchor);
    let sql = format!(
        "SELECT f.scope_kind, f.scope_key, f.canonical, f.kind FROM anchor f
          WHERE f.status IN ('active','provisional') AND ({alive})"
    );
    let mut st = m.conn().prepare(&sql)?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut groups: HashMap<(String, String, String), HashSet<String>> = HashMap::new();
    for row in rows {
        let (scope_kind, scope_key, canonical, kind) = row?;
        groups
            .entry((scope_kind, scope_key, normalize_body(&canonical)))
            .or_default()
            .insert(kind);
    }
    Ok(groups.values().filter(|kinds| kinds.len() >= 2).count() as i64)
}

/// W15a：`passed` 卡按**背书来源**分三桶，`(pass_audit, pass_review_apply, passed_unbacked)`。
///
/// 口径只在这一处，doctor 只做渲染：
/// - 只看 `card.verdict='passed'`（别的档位没有"谁盖的"这一问：`low`/`failed` 是 audit 的
///   降级结论，`unreviewed` 还没人盖）；
/// - 每张卡认**最新**一条事件（`max(id) per card_id`）的 `source`；先 audit 降档、后 review
///   晋升的卡因此只落在 `review_apply` 一桶；
/// - **没有事件**的 passed 卡落 `passed_unbacked`——那是诚实的"来源未知"（W15a 之前 audit
///   盖的存量卡全在这里），不猜、不补写、不回溯。
///
/// 两个来源字面量从 `writers_c` 的常量取（与 DDL 的 CHECK 同词），本函数不持有第三份拼写。
/// `0` 是"这一桶干净"这个**读数**，不是读失败。
pub fn verdict_source_counts(m: &MemoryHandle) -> Result<(i64, i64, i64), MemError> {
    Ok(m.conn().query_row(
        "SELECT coalesce(sum(e.source = ?1), 0),
                coalesce(sum(e.source = ?2), 0),
                coalesce(sum(e.source IS NULL), 0)
           FROM card c
           LEFT JOIN card_verdict_event e
             ON e.id = (SELECT max(id) FROM card_verdict_event WHERE card_id = c.id)
          WHERE c.verdict = ?3",
        params![
            VERDICT_SOURCE_AUDIT,
            VERDICT_SOURCE_REVIEW_APPLY,
            Verdict::Passed.as_str()
        ],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?)
}

/// W20（P3-2）：休眠的临时锚有多少——**建了够久、没人再提、也没有任何出处**的
/// `provisional` 锚行数。
///
/// 四支条件都住在这里（doctor 只做渲染）：
/// - `status='provisional'`：`active` 是确认过的名字面，不在这格的主语里；
/// - 过 [`crate::reader::alive_where`] 的三支（未退场、未被顶掉、未否决）——已处置的行不算
///   "休眠"，它们已经被人处理过一次；
/// - `created_at_ms` 早于 `now_ms - DORMANT_ANCHOR_AGE_MS`：刚建出来的临时锚本来就还没被
///   引用，报它只是噪声；
/// - 零 `mention` 且零 `provenance`（`mem_table='anchor'`）：还有引用就说明它还在被用；
/// - 零环境属性行：只有属性的锚（W14c 那一类）同样是"还在被引用"，它没有正文出处、
///   也没人再把它当名字提，但把它报成休眠锚就是假数。
///
/// **只读数，不 retire**（守卫 13）：一行锚的退场是用户/操作员的动作，这一格只回答
/// "有多少格锚看起来已经没人用了"。`0` 是"没有休眠锚"这个读数，不是读失败——与
/// [`cross_kind_duplicate_groups`] 同一条纪律。
pub fn dormant_anchor_count(m: &MemoryHandle, now_ms: i64) -> Result<i64, MemError> {
    let alive = crate::reader::alive_where(MemTable::Anchor);
    let sql = format!(
        "SELECT count(*) FROM anchor f
          WHERE f.status='provisional' AND ({alive})
            AND f.created_at_ms < ?1
            AND NOT EXISTS (SELECT 1 FROM mention mn WHERE mn.anchor_id = f.id)
            AND NOT EXISTS (SELECT 1 FROM provenance dp
                             WHERE dp.mem_table='anchor' AND dp.mem_id = f.id)
            AND NOT EXISTS (SELECT 1 FROM attribute at WHERE at.anchor_id = f.id)"
    );
    Ok(m.conn().query_row(&sql, params![now_ms - DORMANT_ANCHOR_AGE_MS], |r| r.get(0))?)
}

/// 休眠门槛：多久没人提算"休眠"。30 天是这一格唯一的拼写处——读写两侧都不许再抄一份。
pub const DORMANT_ANCHOR_AGE_MS: i64 = 30 * 24 * 3_600_000;

/// W22a（审计 P2-3 第一半）：每一条**活锚**的行 id 是否真等于它自己的 preimage，`(checked, mismatch)`。
///
/// 生产锚 id 是 `compute_mem_id(Anchor, kind, canonical, "", scope)`（唯一算法住
/// [`crate::identity::compute_mem_id`]），写入口都按它算；但库里**没有任何常驻检查**证明
/// 行上的那串 hex 真等于按行上的列重算出来的那串——存量行可能来自早期版本、迁移或手改，
/// 形状与正常行完全一样，直到有人按 id 指认它之前都不会露馅。这一格就是那把尺子。
///
/// **只读**：不改锚、不修 id、不删行。`mismatch > 0` 是读数不是故障——它只说"库里有这么多行
/// 的 id 与它自己的 preimage 对不上"，处置（回拨、重键、退休）是运维看过数之后再决定的事，
/// 这里自动去修反而会把一份可观察的证据变成一次不可回溯的改写。
///
/// 三支条件：
/// - 只看**活锚**（[`crate::reader::alive_where`] 的三支）：挂未解决退场账的行已经被处置过，
///   它 id 对不对不再是这一格的主语（同 [`dormant_anchor_count`] 的口径）；
/// - scope 由行上的 `scope_kind`/`scope_key` **构造**（[`ScopeKind::parse`] 的既有闭集），
///   不手写 kind→ScopeKind 的表：手写一张就是第二份闭集，闭集加一格时它会静默漏掉那一格；
/// - `checked` 是实际比过的行数（空库 0），`mismatch` 是其中 id ≠ 重算值的那几行。
///
/// `scope_kind` 解析不出（[`ScopeKind::parse`] 返回 `None`）的行**同时计入 checked 与
/// mismatch**：那一列在 DDL 里没有 CHECK（`rc-store` 的 `anchor` 建表原文只有
/// `scope_kind TEXT NOT NULL`），脏值落得进库，而生产侧根本写不出这样的行。把它排除在
/// checked 之外，一个全是脏 scope 的库会读出 `0/0`（"干净"），真相却是"这些行证明不了自己
/// 的 id"——fail-closed 的方向与 [`crate::reader::dead_by_id_expr`] 那一族一致：证明不了合法，
/// 就算它不合法。`0/0` 因此只有一种读法：库里没有活锚。
pub fn anchor_id_audit(m: &MemoryHandle) -> Result<(i64, i64), MemError> {
    let alive = crate::reader::alive_where(MemTable::Anchor);
    let sql = format!(
        "SELECT f.id, f.kind, f.canonical, f.scope_kind, f.scope_key FROM anchor f WHERE {alive}"
    );
    let mut st = m.conn().prepare(&sql)?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut checked = 0i64;
    let mut mismatch = 0i64;
    for row in rows {
        let (id, kind, canonical, scope_kind, scope_key) = row?;
        checked += 1;
        let expected = ScopeKind::parse(&scope_kind).map(|k| {
            crate::identity::compute_mem_id(
                MemTable::Anchor,
                &kind,
                &canonical,
                "",
                &Scope { kind: k, key: scope_key },
            )
        });
        if expected.as_deref() != Some(id.as_str()) {
            mismatch += 1;
        }
    }
    Ok((checked, mismatch))
}
