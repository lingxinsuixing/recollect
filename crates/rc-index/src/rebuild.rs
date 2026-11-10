//! 重建与检索：索引层唯一的写入面与唯一的读数面。
//! V2：形参只有记忆句柄（+ 时钟）。计划 §7 原文写的是"会话句柄"，那是笔误——
//! 若照抄，`rc-index` 就成了第二个同时握两个句柄的 crate，守卫 1 从内部破掉。

use crate::analyzer::{query_tokens, row_doc};
use crate::shadow;
use rc_memory::keys::MemKey;
use rc_memory::reader;
use rc_store::{MemoryHandle, StoreError};
use rusqlite::Connection;
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IndexError {
    /// "从没建过索引"与"库是空的"必须是两个不同的答案。把它们混成一个，
    /// 用户读到的是"系统没有记忆"，而真实情况是"你还没跑 `hub index rebuild`"。
    #[error("index not built")]
    NotBuilt,
    /// 查询切不出任何词。这不是"没有匹配"，是"没法匹配"。
    #[error("query yields no index tokens")]
    NoQueryTokens,
    /// 影子表里的 `norm_version` 与当前 `NORM_VERSION` 不符 ⇒ 全量重建之前禁止使用旧影子。
    #[error("index built with norm_version {marker}, current {current}; rebuild required")]
    NormMismatch { marker: u32, current: u32 },
    #[error("read-only handle: {0}")]
    ReadOnly(&'static str),
    #[error(transparent)]
    Store(#[from] StoreError),
    /// 守卫 12：rusqlite 原文不经消毒就是给操作员吐路径的出口（`InvalidPath(p)` 直接打印磁盘路径、
    /// `SqlInputError { sql }` 打印整条 SQL）。不许 `#[from] rusqlite::Error` 直通——载荷是
    /// 消毒后的字符串，构造点过 `sqlite_text`（见文件末尾，与 rc-memory 的 `sqlite_text` 同规则）。
    /// 裁定 R18-2：这一支**只留给 prepare 期错误**（`InvalidPath`/整条 SQL/`no such table`，
    /// 它们回显的是 schema 文本，不是用户串）。逐行 step 期错误走下面的 `LayerUnreadable`。
    #[error("sqlite: {0}")]
    Sql(String),
    /// 裁定 R18：**索引层读不出来**（逐行 step 期失败，或影子行里的句柄解析不回来）。
    ///
    /// 故意**不带任何载荷**——不带消息、不带代码、不带查询串。step 期错误多半来自 FTS5，
    /// 而它的原串会把用户查询里的那一段**原样回显**（评审点名：`fts5: syntax error near "c++"`，
    /// `"c++"` 就是用户字节）。带上载荷等于修沉默顺手开一条守卫 12 的口子。
    /// 运维面只需要知道"这一层坏了 ⇒ 跑 doctor"，别的都不许往外说。
    #[error("index layer unreadable")]
    LayerUnreadable,
    /// 事实行读不回来 ⇒ 索引建不出来。这一支不在计划的变体清单里，但计划的 `rebuild`
    /// 代码写着 `reader::facts(m)?`，而 `facts` 的错误类型是 `rc_memory::MemError`——
    /// 两者只能有一个变体接得住它。`MemError` 的 Display 在构造点已过守卫 12 的消毒
    /// （`crates/rc-memory/src/lib.rs` 的 `sqlite_text`），所以这里仍是 `transparent`。
    #[error(transparent)]
    Memory(#[from] rc_memory::MemError),
}

/// rusqlite 的错误进 `IndexError` 的唯一入口：先消毒再装进 `Sql`（守卫 12）。
/// `?` 操作符在各调用点靠这一支转换，形状与 `impl From<rusqlite::Error> for MemError` 一致。
impl From<rusqlite::Error> for IndexError {
    fn from(e: rusqlite::Error) -> Self {
        IndexError::Sql(sqlite_text(&e))
    }
}

/// `PartialEq` 是为 `tests/rebuild.rs` 那句 `assert_eq!(index_status(&m).unwrap(), None)` 加的：
/// 计划的代码块只 derive 了 `Debug, Clone, Serialize`，那条断言却要比 `Option<RebuildReport>`，
/// 两边只能补一个。补在派生这一侧（`assert!(_.is_none())` 会把"报告等不等于 None"这件事
/// 从断言里抹掉），序列化形状与 CLI 输出一个字都没变。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RebuildReport {
    /// 声明顺序就是 CLI 输出顺序：`{"schema_version":…,"rows_indexed":…,"shadow_tables":…,"norm_version":…}`
    /// 里的字段由 CLI 另外组一个只含读数的结构（见 Step 7），这里保持"报告 = 事实"。
    pub rows_indexed: u64,
    pub norm_version: u32,
    pub schema_version: i32,
    pub rebuilt_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub key: MemKey,
    /// 影子表里存的词串拆回来的结果；`rc-search` 用它算命中数，不再分一次词。
    /// 这样"匹配度"用的是索引自己的口径，而不是查询时重新切一遍的口径。
    pub tokens: Vec<String>,
    pub row_len: usize,
}

fn conn_of(m: &MemoryHandle) -> Result<&Connection, IndexError> {
    if m.is_read_only() {
        // 不依赖 SQLite 的运行时报错：编译期之后、SQL 之前先把这道路拦下来，
        // 报错才可能是 `IndexError::ReadOnly` 而不是 `Sql(…)`。
        return Err(IndexError::ReadOnly("index rebuild"));
    }
    Ok(m.conn())
}

/// V2：形参只有记忆句柄（+ 时钟）。
/// `now_ms` 由调用方给（任务 0 起的纪律：时间不进内容 id，但进 marker）。
pub fn rebuild(m: &MemoryHandle, now_ms: i64) -> Result<RebuildReport, IndexError> {
    let conn = conn_of(m)?;
    let rows = reader::facts(m)?;
    shadow::create_if_missing(conn)?;
    let tx = conn.unchecked_transaction()?;
    tx.execute(&format!("DELETE FROM {}", shadow::SHADOW), [])?;
    {
        let mut st = tx.prepare(&format!(
            "INSERT INTO {}(key, tokens, row_len) VALUES(?1,?2,?3)",
            shadow::SHADOW
        ))?;
        for r in &rows {
            // entity 的 `RowFacts.body` 在任务 3 里就是空串，标题即全部可检索内容。
            let (doc, len) = row_doc(&r.title, &r.body);
            st.execute(rusqlite::params![r.key.key_string(), doc, len as i64])?;
        }
    }
    tx.execute(
        &format!(
            "INSERT OR REPLACE INTO {}(id, rows_indexed, norm_version, schema_version, rebuilt_at_ms)
             VALUES(1,?1,?2,?3,?4)",
            shadow::MARKER
        ),
        rusqlite::params![
            rows.len() as i64,
            rc_store::capability::NORM_VERSION as i64,
            rc_store::ddl::MEMORY_VERSION as i64,
            now_ms
        ],
    )?;
    tx.commit()?;
    Ok(RebuildReport {
        rows_indexed: rows.len() as u64,
        norm_version: rc_store::capability::NORM_VERSION,
        // `MEMORY_VERSION` 本来就是 `i32`（`crates/rc-store/src/ddl.rs`），计划那句 `as i32`
        // 会被 `clippy::unnecessary_cast` 直接拦下——去掉转换，字段类型不变。
        schema_version: rc_store::ddl::MEMORY_VERSION,
        rebuilt_at_ms: now_ms,
    })
}

pub fn index_status(m: &MemoryHandle) -> Result<Option<RebuildReport>, IndexError> {
    // marker 不存在、或存在但不是本 crate 写出来的那一形状 ⇒ `None`（"没建过"），
    // 而不是让 SQLite 的 "no such column" 冒出来冒充读数（见 `shadow::marker_is_canonical`）。
    if !shadow::marker_is_canonical(m.conn())? {
        return Ok(None);
    }
    let mut st = m.conn().prepare(&format!(
        "SELECT rows_indexed, norm_version, schema_version, rebuilt_at_ms FROM {}",
        shadow::MARKER
    ))?;
    let mut rows = st.query([])?;
    match rows.next()? {
        None => Ok(None),
        Some(r) => Ok(Some(RebuildReport {
            rows_indexed: r.get::<_, i64>(0)? as u64,
            norm_version: r.get::<_, i64>(1)? as u32,
            schema_version: r.get::<_, i64>(2)? as i32,
            rebuilt_at_ms: r.get(3)?,
        })),
    }
}

/// 影子 → 句柄 + 词。排序与阈值都不在这里：这一层只负责"能不能匹配上"，
/// 不负责"该不该被提起"（§8 的五个开关全在 `rc-search`）。
pub fn search(m: &MemoryHandle, query: &str) -> Result<Vec<SearchHit>, IndexError> {
    let conn = m.conn();
    // marker 缺失/表缺失 ⇒ NotBuilt（不许让 SQLite 的 "no such table" 冒出来冒充结果）。
    let marker = match index_status(m) {
        Ok(Some(rep)) => rep,
        Ok(None) => return Err(IndexError::NotBuilt),
        Err(e) => {
            // 裁定 R18-4：从前这里把 `Err(Sql(_))` 整体重贴成 `NotBuilt`，
            // 那是把"磁盘坏了"回答成"你还没建索引"。**只按事实判**：索引层的表根本不在 ⇒
            // 确实从没建过；表在、却读不出来 ⇒ 原样抛出，绝不靠错误文本或代码去猜。
            if matches!(shadow::index_layer_missing(conn), Ok(true)) {
                return Err(IndexError::NotBuilt);
            }
            return Err(e);
        }
    };
    if marker.norm_version != rc_store::capability::NORM_VERSION {
        return Err(IndexError::NormMismatch {
            marker: marker.norm_version,
            current: rc_store::capability::NORM_VERSION,
        });
    }
    let toks = query_tokens(query);
    if toks.is_empty() {
        return Err(IndexError::NoQueryTokens);
    }
    let match_str = shadow::match_string(&toks);
    let mut st = conn.prepare(&format!(
        "SELECT key, tokens, row_len FROM {} WHERE {} MATCH ?1",
        shadow::SHADOW, shadow::SHADOW
    ))?;
    let rows = st
        .query_map(rusqlite::params![match_str], |r| {
            let key_s: String = r.get(0)?;
            let doc: String = r.get(1)?;
            let len: i64 = r.get(2)?;
            Ok((key_s, doc, len))
        })?
        // 裁定 R18-1：从前这一行是 `.filter_map(|x| x.ok())`——逐行 step 期的每一个错误都被
        // 折进 `Ok(vec![])`，于是本 crate 唯一的读数面对"索引层坏了"回答"没有记忆"。
        // 那是 G7 明令拒绝的不可枚举沉默（证伪 #3 实测就走成了 `Ok(空)`）。从此 step 期错误枚举化。
        .collect::<Result<Vec<_>, rusqlite::Error>>()
        // 裁定 R18-2：step 期错误不进 `Sql(String)`——FTS5 的原串会回显用户查询片段。
        .map_err(|_| IndexError::LayerUnreadable)?;
    let out = rows
        .into_iter()
        .map(|(key_s, doc, len)| {
            // 裁定 R18-3：影子里的每一把句柄都是 `rebuild` 用 `key_string()` 写进去的，
            // 解析不回来就等价于"这一层被人动过" ⇒ 响亮报错，不再静默丢候选。
            let key = MemKey::parse(&key_s).ok_or(IndexError::LayerUnreadable)?;
            Ok(SearchHit {
                key,
                // 影子表自己的口径（任务 5 拿来算命中数，不再切第二遍词）。
                tokens: doc.split(' ').filter(|s| !s.is_empty()).map(String::from).collect(),
                // 裁定 R20-5：下界取 1——任务 5 要算 `score = 1024*matched/row_len`，
                // 让 0 过网就是除零。零词的行 MATCH 不上，不会因此丢分数。
                row_len: len.max(1) as usize,
            })
        })
        .collect::<Result<Vec<_>, IndexError>>()?;
    Ok(out)
}

/// 守卫 12 在构造点的消毒，规则逐字对齐 `rc-memory/src/lib.rs` 的 `sqlite_text`（首行 / 120 字符上界 /
/// 路径形状→占位符）。rusqlite 的 `Display` 里 `InvalidPath(p)` 直接打印磁盘路径、
/// `SqlInputError { sql, .. }` 打印整条 SQL（多行、含列名），sqlite 自己的 errmsg 在 I/O 类错误上
/// 也可能带库文件名。这里取三道闸，换到"任何一条 IndexError::Sql 渲染出去都不可能吐出一个路径或一段正文"。
///
/// 本轮裁定是"行为对齐、代码各自持有"：跨 crate 提公共模块会牵动 rc-store/rc-memory 的公开面，
/// 而本轮已有两处跨 crate 改动；抽取挂在最终全分支评审的账上（第三份已出现于
/// `rc-search/src/transcript.rs`，抽取仍未做，挂账中）。
fn sqlite_text(e: &rusqlite::Error) -> String {
    let head = e.to_string().lines().next().unwrap_or_default().to_string();
    let cut: String = head.chars().take(120).collect();
    if cut.contains('/') || cut.contains('\\') || cut.to_lowercase().contains(".db") {
        return "error detail redacted (path-like text)".to_string();
    }
    cut
}

/// 裁定 R22："`drop_both` 是 `pub(crate)`"（守卫 2 要求影子表名在 `rc-index` 之外不可命名），
/// 而这条用例要调它 ⇒ 只能住在 src 里（`tests/*.rs` 是外部 crate，看不见 `pub(crate)`）。
/// 为了这条用例把 `drop_both` 提公开，等于给外面一份"按名字删整层"的许可，方向是错的。
#[cfg(test)]
mod tests {
    use crate::rebuild::{index_status, rebuild, search, IndexError};
    use crate::shadow;
    use rc_memory::synth;

    /// 索引层的规范条目（1 + 5 + 1）。如实登记：这句"只写一次"曾不成立——写死的清单目前有三份：
    /// 本常量（`src/rebuild.rs`）、`tests/rebuild.rs` 的 `len() == 7` 计数与字面量清单
    /// （两处都注了"刻意钉住的快照"），外加前缀谓词两份（`tests/rebuild.rs` 的 `is_index_layer`
    /// 闭包与 `crates/rc-memory/tests/ownership.rs:49`）。唯一现算的家是 `shadow.rs` 的
    /// `index_layer_names`（它自己声明不写死数字，`shadow.rs:123-124`）。合并挂在后续任务。
    const LAYER: [&str; 7] = [
        "rc_index_marker",
        "rc_shadow",
        "rc_shadow_config",
        "rc_shadow_content",
        "rc_shadow_data",
        "rc_shadow_docsize",
        "rc_shadow_idx",
    ];

    #[test]
    fn drop_both_之后_rebuild_能把整层原样建回来() {
        // "整层可丢弃 ⇒ 可从零重建 ⇒ 不留残渣"这半条从前无人断言：`tests/rebuild.rs` 只证明了
        // 删掉整层不伤记忆、也不伪装成空库，却从没在 DROP 之后调 `rebuild`。
        // 这条同时兑现 `shadow::drop_both` 的第一个调用者。
        let dir = tempfile::tempdir().unwrap();
        let m = rc_store::Root::open(dir.path()).unwrap().into_memory();
        let now = 1_700_000_000_000;
        synth::seed(&m, now).unwrap();

        let first = rebuild(&m, now).unwrap();
        let keys_before: Vec<String> =
            search(&m, "同分合成词甲").unwrap().into_iter().map(|h| h.key.key_string()).collect();
        assert!(!keys_before.is_empty(), "夹具没命中，下面的命中对拍会恒真");

        shadow::drop_both(m.conn()).unwrap();
        // drop_both 删的是**两张**表：少删一张就会被读成"已建索引"（marker 残留 ⇒ status Some；
        // 影子残留 ⇒ 清单非空），所以这两条各守一个方向。
        assert_eq!(index_status(&m).unwrap(), None, "drop_both 没把 marker 删干净");
        let err = search(&m, "同分合成词甲").expect_err("整层被删之后不许假装检索成功");
        assert!(matches!(err, IndexError::NotBuilt), "错误面贴错了：{err:?}");
        assert_eq!(shadow::index_layer_names(m.conn()).unwrap(), Vec::<String>::new(), "drop_both 留了残渣");

        let again = rebuild(&m, now + 1).unwrap();
        assert_eq!(again.rows_indexed, first.rows_indexed, "从零重建的行数与首次不一致");
        let keys_after: Vec<String> =
            search(&m, "同分合成词甲").unwrap().into_iter().map(|h| h.key.key_string()).collect();
        assert_eq!(keys_before, keys_after, "重建回来的命中集合变了");
        // 建回来的仍然是那 7 条，不多不少：多出来的是残渣，少了的是没建全。
        let mut layer = shadow::index_layer_names(m.conn()).unwrap();
        layer.sort();
        assert_eq!(layer, LAYER, "drop 之后重建出的索引层条目与首次不同");
    }
}
