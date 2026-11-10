//! 索引层的表名、形状与 MATCH 串的拼法。
//! 模块本身是 `pub(crate)`：`rc_shadow` / `rc_index_marker` 这两个名字在 `rc-index` 之外
//! 不可命名（守卫 2）。对外只有一个读数出口——`lib.rs` 的 `shadow_tables` 包装，
//! 它给表**清单**，不给任何人按名字写这张表的便利。

use rusqlite::Connection;

/// FTS5 虚拟表本身 + 它的五张自动辅助表（`_config/_content/_data/_docsize/_idx`，
/// 见 `tests/rebuild.rs` 的 `重建后归属过滤滤掉的是真实存在的索引层条目` 现算的 1 + 5 + 1）。
/// `rc_index_marker` 是"影子是哪把尺子建出来的"唯一记录处。
pub(crate) const SHADOW: &str = "rc_shadow";
pub(crate) const MARKER: &str = "rc_index_marker";

/// 本 crate 认得的形状。`tests/privacy.rs` 逐字对撞 `SHADOW_COLS`。
const SHADOW_COLS: [&str; 3] = ["key", "tokens", "row_len"];
const MARKER_COLS: [&str; 5] = ["id", "rows_indexed", "norm_version", "schema_version", "rebuilt_at_ms"];

fn table_exists(conn: &Connection, table: &str) -> Result<bool, rusqlite::Error> {
    let found: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
        rusqlite::params![table],
        |r| r.get(0),
    )?;
    Ok(found > 0)
}

fn columns_of(conn: &Connection, table: &str) -> Result<Vec<String>, rusqlite::Error> {
    let mut st = conn.prepare("SELECT name FROM pragma_table_info(?) ORDER BY cid")?;
    let mapped = st.query_map(rusqlite::params![table], |r| r.get::<_, String>(0))?;
    mapped.collect::<Result<Vec<_>, _>>()
}

fn same_shape(have: &[String], want: &[&str]) -> bool {
    have.len() == want.len() && have.iter().zip(want.iter()).all(|(h, w)| h.as_str() == *w)
}

/// 表在、但不是本 crate 的列集 ⇒ 删掉它。
///
/// 存在的理由：派生层的规范形状（三列影子 + 五列 marker）唯一住在本 crate，
/// `row_len` 是任务 5 长度归一的分母，`rows_indexed`/`schema_version`/`rebuilt_at_ms` 是 marker 的三个读数。
/// 裁定 R15 之后 `MEMORY_SQL`（`crates/rc-store/src/ddl.rs`）不再建这两张表，
/// 于是"任务 2 建了两列旧影子、与本 crate 的三列影子撞形状"这一处真实漂移从根上没了来源。
/// 这一条因此【不再是永不触发的保险】：裁定 R21-2 用旧 DDL 的真实形状（两列的
/// `rc_shadow(key, toks)` 与一列的 `rc_index_marker(norm_version)`）把它钉在
/// `tests/rebuild.rs` 的 `旧_ddl_形状读成没建过_重建把它建回规范形状` 上——删掉本函数的调用当场红。
/// 万一库里出现一张名字相同、列集不符的异物（外部砸进来的、或将来别处误建的），
/// 它仍然当场删掉再按本 crate 的形状重建。
/// **删一张自己建不了的影子表正好就是"索引层可整层丢弃"这条性质本身**，它一行记忆都不碰
/// （`tests/rebuild.rs` 断的就是这个）。R15 裁定它"可留可删"，本轮保留、不动代码。
fn reconcile_shape(conn: &Connection, table: &str, want: &[&str]) -> Result<(), rusqlite::Error> {
    if !table_exists(conn, table)? {
        return Ok(());
    }
    let have = columns_of(conn, table)?;
    if !same_shape(&have, want) {
        // FTS5 虚拟表被删时，五张自动辅助表一起消失，不需要逐个 DROP。
        conn.execute(&format!("DROP TABLE IF EXISTS {table}"), [])?;
    }
    Ok(())
}

/// 影子表的建造权在 `rebuild` 这一侧：它的规范形状（三列 + 五列 marker）只在这里声明一次。
/// 裁定 R15 之后 `MEMORY_SQL` 不再建它，本函数是索引层唯一的建表点。
/// 表名与列名在这里是唯一家；`tests/privacy.rs` 逐字对撞它。
///
/// 裁定 R20-4：`tokenize='unicode61'` 必须**显式写出来**。"这把尺子是谁建的"正是 `norm_version`
/// 要记的事，而 `reconcile_shape` 只比列名 ⇒ 分词器换了形状测试不出来。显式子句让这把尺子在
/// DDL 文本里就只有一个口径，不再靠 fts5 的默认值（默认值会随 SQLite 版本漂）。
pub(crate) fn create_if_missing(conn: &Connection) -> Result<(), rusqlite::Error> {
    reconcile_shape(conn, SHADOW, &SHADOW_COLS)?;
    reconcile_shape(conn, MARKER, &MARKER_COLS)?;
    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS {SHADOW} USING fts5(key UNINDEXED, tokens, row_len UNINDEXED, tokenize='unicode61');
         CREATE TABLE IF NOT EXISTS {MARKER}(
           id INTEGER PRIMARY KEY CHECK(id = 1),
           rows_indexed INTEGER NOT NULL,
           norm_version INTEGER NOT NULL,
           schema_version INTEGER NOT NULL,
           rebuilt_at_ms INTEGER NOT NULL
         );",
    ))
}

/// 整层下线路径：删表与删 marker 一次做完（`reconcile_shape` 只删形状不符的那一张）。
/// 裁定 R22：它的第一个调用者是 `rebuild.rs` 末尾那条 `#[cfg(test)]` 用例
/// （drop → rebuild → 命中集合与 7 条清单逐位对拍），"整层可丢弃"从此有了后半条。
/// 本档的 `tests/*.rs` 从 SQL 直接 DROP 两张表（它们要的是"外部把索引层砸了"这个形状），
/// 那是另一件事，不代替这条。生产侧仍无人调用（任务 9 的副本整替与任务 11 的 `doctor`
/// 还要下这行），所以 `#[allow(dead_code)]` 保留：它是 `pub(crate)`，守卫 2 不许它更公开。
#[allow(dead_code)]
pub(crate) fn drop_both(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(&format!("DROP TABLE IF EXISTS {SHADOW}; DROP TABLE IF EXISTS {MARKER};"))
}

/// marker 是否是本 crate 写出来的那一形状。不是 ⇒ 对 rc-index 而言"从没建过索引"，
/// 于是 `index_status` 报 `None`、`search` 报 `NotBuilt`，而不是让 SQLite 的
/// "no such column" 冒充成读数（那正是"未建索引被读成空库"的形状）。
pub(crate) fn marker_is_canonical(conn: &Connection) -> Result<bool, rusqlite::Error> {
    if !table_exists(conn, MARKER)? {
        return Ok(false);
    }
    Ok(same_shape(&columns_of(conn, MARKER)?, &MARKER_COLS))
}

/// MATCH 串：每个词包成字符串字面量、内部引号翻倍，再 `OR` 起来。
/// 用户字节永远只经 `?1` 绑定进 `MATCH`，从不进 SQL 文本；这一串里的 `*`、`-`、`NEAR` 只是词面，
/// 因为它们在双引号里——这就是"查询词可以长成任何样子"的执法点（`tests/rebuild.rs` 里那条 `card` 断言是同一族的另一半）。
pub(crate) fn match_string(tokens: &[String]) -> String {
    tokens
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// 索引层的表根本不存在（`search` 的裁定 R18-4 靠它按**事实**判 `NotBuilt`）。
/// 只看错误文本或错误代码都不算判据——那是把"磁盘坏了"读成"你还没建索引"的另一条路。
/// 两张主表都不在 ⇒ 这一层从没被本 crate 建过。
pub(crate) fn index_layer_missing(conn: &Connection) -> Result<bool, rusqlite::Error> {
    Ok(!table_exists(conn, MARKER)? && !table_exists(conn, SHADOW)?)
}

/// 索引层的表名清单（含 FTS5 自动建的辅助表），供 CLI 的 `shadow_tables` 与 `doctor` 用。
/// 从 `sqlite_master` 现算，不写死数字：写死的数会在下一次 SQLite 换实现时变成假读数。
///
/// 裁定 R20-3：前缀匹配用 `GLOB` 不用 `LIKE`。`LIKE` 的 `_` 是**单字符通配符**，
/// 于是 `rc_shadowXfoo` 会被算进索引层，而 `rc-memory/tests/ownership.rs` 的 `starts_with`
/// 认它不认——两份"唯一清单"就此漂移。`GLOB` 里 `_` 是字面量、`*` 才是通配，
/// 与 `starts_with("rc_shadow_")` 逐位同口径（`LIKE` 还额外做 ASCII 大小写折叠，`GLOB` 不会，
/// 表名由本 crate 的小写字面量建出来，这一侧也只会更严）。
pub(crate) fn index_layer_names(
    conn: &Connection,
) -> Result<Vec<String>, rusqlite::Error> {
    let mut st = conn.prepare(
        "SELECT name FROM sqlite_master WHERE name = ?1 OR name = ?2 OR name GLOB ?3 ORDER BY name",
    )?;
    let out = st
        .query_map(rusqlite::params![SHADOW, MARKER, &format!("{SHADOW}_*")], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(out)
}
