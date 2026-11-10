//! 快照的**内容面**：活行过滤、派生清理、摘要编排。
//! 两张可指认表的条件不许在这里重写——逐支走 `reader::alive_where`（P-31 的同一个家）；
//! 派生表按"主人还在不在"连带清理，规则住本文件。
//!
//! 过滤顺序是判定的一部分：先删事实行，后面三张按主人查的 SQL 才能看到删后的状态。

use crate::keys::MemTable;
use crate::reader::alive_where;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use rc_store::MemoryHandle;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// 两张可指认表里被删掉的行（退场 / 否决 / 被顶替）。
    pub dead_rows: usize,
    /// 两端有一端不在、或自己被退场的边（canonical 只有 `fact_link` 一张）。
    pub orphan_edges: usize,
    /// 主人不在的出处、别名、合证标注，以及目标已不在的 attention 与 `resource_owner`
    /// 归属行（C4b2b：死行的归属行跟着行一起走）。
    pub orphan_derived: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot io: {0:?}")]
    Io(std::io::ErrorKind),
    #[error("snapshot sqlite: {0}")]
    Sql(String),
    #[error("source memory handle is read-only")]
    ReadOnly,
    #[error("snapshot vacuum failed")]
    Vacuum,
    /// 发布前一致性检查：活卡的 `pointers_json` 指向不存在的 fact。计数是**指针条数**
    /// （不是卡数：一张卡可能有多条坏指针），由运维面回执用；不把 id 带进错误串。
    #[error("snapshot has {0} card pointer(s) to missing facts")]
    DanglingCard(usize),
}

impl From<rusqlite::Error> for SnapshotError {
    fn from(e: rusqlite::Error) -> Self {
        SnapshotError::Sql(crate::sqlite_text(&e))
    }
}

/// 产出的临时文件。`path` 与最终快照同目录，调用方要么把它改成名（rc-net 的 `publish`），
/// 要么在"内容没变"的分支里删掉它。
#[derive(Debug, Clone)]
pub struct Built {
    pub path: PathBuf,
    pub digest: String,
    pub bytes: u64,
    pub pruned: PruneReport,
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// `VACUUM INTO` → 过滤 → 摘要 → `VACUUM`，全部发生在**副本**上；中心库一行不动。
/// 失败时删掉临时文件（失败产物不许留给 retain 当历史）。
///
/// `keep_review` 是快照的**受众**：`true` = 备份通道，完整记忆（含两张审查侧表）；
/// `false` = 记忆通道（agent），副本上 `review_item` / `review_decision` 整表清空。
/// 清空发生在摘要**之前**，所以 agent 副本的 digest 描述的就是它自己装了什么——
/// 两份同一时点的快照因此 digest 必然不同，两条通道的 `CURRENT` 不会把对方认成
/// "内容没变"（各自的清单/保留数也各住各的目录）。中心库本身一行不动。
pub fn build(
    memory: &MemoryHandle,
    dest_dir: &Path,
    now_ms: i64,
    keep_review: bool,
) -> Result<Built, SnapshotError> {
    if memory.is_read_only() {
        return Err(SnapshotError::ReadOnly);
    }
    std::fs::create_dir_all(dest_dir).map_err(|e| SnapshotError::Io(e.kind()))?;
    let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = dest_dir.join(format!(
        ".snapshot-tmp-{now_ms}-{}-{n}.db",
        std::process::id()
    ));
    rc_store::snapshot::vacuum_into(memory.conn(), &temp).map_err(|_| SnapshotError::Vacuum)?;
    match finish(&temp, keep_review) {
        Ok((digest, pruned)) => {
            let bytes = std::fs::metadata(&temp)
                .map_err(|e| SnapshotError::Io(e.kind()))?
                .len();
            Ok(Built {
                path: temp,
                digest,
                bytes,
                pruned,
            })
        }
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            Err(e)
        }
    }
}

fn finish(temp: &Path, keep_review: bool) -> Result<(String, PruneReport), SnapshotError> {
    let conn = Connection::open(temp)?;
    let pruned = prune(&conn)?;
    // 发布前一致性检查：过滤完活卡之后，任何活卡的 `fact:` 指针都必须落到一行真实 fact 上。
    // 放在摘要之前——悬空卡一旦发布，agent 面会拿到"有卡、没依据、也补不出出处"的行，
    // 而副本上没有中心那本账可查；宁可不发这一代（临时文件由 `build` 删除）。
    check_card_pointers(&conn)?;
    // 审查侧表的清空排在 digest 之前：digest 是"这份副本装了什么"的读数，
    // 先删后算，agent 副本的摘要才与它的实际内容一致（也因此与 backup 通道不同）。
    if !keep_review {
        drop_review(&conn)?;
    }
    let digest = digest(&conn)?;
    // 删行留下的空闲页要还回去：快照的字节数会进清单，空页会让"内容没变"看起来像变大。
    conn.execute_batch("VACUUM")?;
    drop(conn);
    Ok((digest, pruned))
}

/// agent 副本的受众过滤：审查队列与判定台账是中心的操作状态，不是设备要读的记忆内容。
/// 两张表之间没有外键，顺序无关（`review_decision.item_id` 只是普通列）。
fn drop_review(conn: &Connection) -> Result<(), SnapshotError> {
    conn.execute_batch("DELETE FROM review_item; DELETE FROM review_decision;")?;
    Ok(())
}

/// 逻辑摘要。表格清单只在 `schema.rs` 一处；本函数只是把它交给 `rc-store` 的编码器。
pub fn digest(conn: &Connection) -> Result<String, SnapshotError> {
    match rc_store::snapshot::logical_digest(conn, crate::schema::MEMORY_TABLES) {
        Ok(d) => Ok(d),
        Err(rc_store::StoreError::Sql(m)) => Err(SnapshotError::Sql(m)),
        Err(_) => Err(SnapshotError::Sql("logical digest failed".to_string())),
    }
}

/// 在**快照副本**上执行。调用方保证这是一份可写的临时文件，不是中心库。
pub fn prune(conn: &Connection) -> Result<PruneReport, SnapshotError> {
    let tx = conn.unchecked_transaction()?;
    let mut report = PruneReport::default();

    // ① 两张可指认表：与读侧同一组条件（退场 / 否决 / 被顶替）。
    //
    //    **anchor 多一道豁免**（2026-10-27 快照数据丢失的修复）：只要有一行 fact 把它当
    //    subject/object/place/speaker 引用，这一行就必须留在副本里——哪怕它已经退场。
    //    理由是中心的状态是**软退场**（行还在，读侧按账过滤），快照删行只是省空间；
    //    而 `fact.subject_anchor_id` 是 `ON DELETE CASCADE`：删一个退场锚会连带删掉
    //    中心仍持有、且读侧仍可见的 fact（真机读数：134 → 42），card 的 `pointers_json`
    //    随之指向缺失行。object/place/speaker 是 `SET NULL`：不豁免同样会把中心的值
    //    静默抹成 NULL。豁免只针对被 fact 引用的锚；没有 fact 引用的退场锚照旧删。
    for table in [MemTable::Card, MemTable::Anchor] {
        let mut sql = fact_delete(table);
        if table == MemTable::Anchor {
            sql = format!("{sql} AND NOT {}", referenced_by_fact("anchor.id"));
        }
        report.dead_rows += tx.execute(&sql, [])?;
    }

    // ② 索引层整层丢弃（守卫 2：影子表是派生物，副本只带记忆内容）。
    //    先删它：fts5 的五个伴生表跟着 `DROP TABLE rc_shadow` 一起走。
    tx.execute_batch("DROP TABLE IF EXISTS rc_shadow; DROP TABLE IF EXISTS rc_index_marker;")?;

    // ③ 别名：锚不在就删。
    report.orphan_derived += tx.execute(
        "DELETE FROM alias WHERE NOT EXISTS (
           SELECT 1 FROM anchor a WHERE a.id = alias.anchor_id)",
        [],
    )?;

    // ④ 记忆行之间的边：两端都在、且自己没有未解决的退场账。
    //    端点存 "<table>:<id>" 键；认不出（或指向别的表）⇒ 两个 EXISTS 全假 ⇒ 删；
    //    这与读侧 `MemTable::parse` 认不出即跳过同一条 fail-closed 方向。
    report.orphan_edges += tx.execute(
        &format!(
            "DELETE FROM fact_link WHERE NOT ({f}) OR NOT ({t}) OR EXISTS (
               SELECT 1 FROM retirement rt WHERE rt.mem_table='fact_link'
               AND rt.mem_id = fact_link.id AND rt.resolved_at_ms IS NULL)",
            f = endpoint_exists("fact_link.from_id"),
            t = endpoint_exists("fact_link.to_id")
        ),
        [],
    )?;

    // ⑤ 出处：主人不在的行一律删；`mem_table` 认不出的行也删（读侧本来就跳过它）。
    //    `fact` 在白名单里（2026-10-27）：card 的 `pointers_json` 指回 fact，卡片面的出处
    //    就是 fact 的 `provenance` 行与 `evidence_turn_uid`。把 fact 出处整族删掉，
    //    副本上的卡就只剩一个指不到任何轮次的 id——这是"90% 无出处"的副本侧根因。
    let owner_missing = |table: &str| {
        format!(
            "mem_table='{table}' AND NOT EXISTS (SELECT 1 FROM {table} o WHERE o.id = provenance.mem_id)"
        )
    };
    report.orphan_derived += tx.execute(
        &format!(
            "DELETE FROM provenance WHERE {two} OR {link} OR mem_table NOT IN
             ('anchor','card','fact','fact_link')",
            two = ["anchor", "card", "fact"]
                .iter()
                .map(|t| owner_missing(t))
                .collect::<Vec<_>>()
                .join(" OR "),
            link = owner_missing("fact_link")
        ),
        [],
    )?;

    // ⑥ 合证标注（memory_mention）：被提到的记忆行不在就删；`mem_table` 认不出也删。
    report.orphan_derived += tx.execute(
        &format!(
            "DELETE FROM memory_mention WHERE NOT ({})",
            ["anchor", "card"]
                .iter()
                .map(|t| format!(
                    "EXISTS (SELECT 1 FROM {t} x WHERE memory_mention.mem_table='{t}' AND x.id = memory_mention.mem_id)"
                ))
                .collect::<Vec<_>>()
                .join(" OR ")
        ),
        [],
    )?;

    // ⑦ attention：副本只留"未解决且目标还在"的那几条；已解决的是操作历史，不是记忆内容。
    report.orphan_derived += tx.execute(
        &format!(
            "DELETE FROM attention WHERE resolved_at_ms IS NOT NULL OR NOT ({})",
            ["anchor", "card"]
                .iter()
                .map(|t| format!(
                    "EXISTS (SELECT 1 FROM {t} x WHERE attention.mem_table='{t}' AND x.id = attention.mem_id)"
                ))
                .collect::<Vec<_>>()
                .join(" OR ")
        ),
        [],
    )?;

    // ⑨ 判定族（attestation / retirement）**不删**：它们是判定记录（D9：判出来的、不是算出来的），
    //    退场账还是软边的唯一撤销凭据。悬空账在副本上是读得到的事实，不是要抹掉的脏数据。

    // ⑩ 资源归属侧表（S1b-C4b2b）：只删**目标行已不在**的孤儿归属行——C3 发现的缺口是：
    //    上面第①步删完死行之后，`resource_owner` 还留着它们的归属行（副本上读闸看不到行、
    //    却看得到归属），审核不变量①的反向（"无孤儿"）也靠这一步。口径与 C4b2a 的回填一致：
    //    只有 `card`/`anchor` 两族的归属行会成孤儿；其余 `mem_table` 的值（今天没有生产者）
    //    一律不动——读闸对它们本来就是 fail-closed，不需要在这里二次裁决。
    //    `resource_grant` 不在这里：它不是"某行的归属"，是两个人之间的授权，没有目标行。
    report.orphan_derived += tx.execute(
        "DELETE FROM resource_owner
          WHERE (mem_table='card'
                 AND NOT EXISTS (SELECT 1 FROM card c WHERE c.id = resource_owner.mem_id))
             OR (mem_table='anchor'
                 AND NOT EXISTS (SELECT 1 FROM anchor a WHERE a.id = resource_owner.mem_id))",
        [],
    )?;

    tx.commit()?;
    Ok(report)
}

/// 发布前一致性检查（在快照副本上跑）：所有留下卡的 `fact:` 指针都必须有对应的 fact 行。
///
/// 读侧把 `pointers_json` 当**出处指针**：card → fact → `provenance(fact)` /
/// `evidence_turn_uid`。指针落空 = 卡上有一句没有依据的结论，且副本上没有任何账能解释
/// 它为什么不见了。这里用 SQLite 的 `json_each` 逐条展开（写侧只产字符串数组；
/// 坏 JSON 会让本查询报错 ⇒ 整个发布失败，与"宁可不发"同一方向）。
///
/// 只认字面前缀 `fact:`（`substr` 而不是 `LIKE`：LIKE 对 ASCII 大小写不敏感，会把
/// 别的形状也算进来）。其它前缀（`card:` 等）今天不是出处链的目标，不在这里裁决。
fn check_card_pointers(conn: &Connection) -> Result<(), SnapshotError> {
    let dangling: i64 = conn.query_row(
        "SELECT count(*) FROM card AS c, json_each(c.pointers_json) AS p
          WHERE p.type = 'text'
            AND substr(p.value, 1, 5) = 'fact:'
            AND NOT EXISTS (SELECT 1 FROM fact AS f WHERE p.value = 'fact:' || f.id)",
        [],
        |r| r.get(0),
    )?;
    if dangling > 0 {
        return Err(SnapshotError::DanglingCard(dangling as usize));
    }
    Ok(())
}

fn fact_delete(table: MemTable) -> String {
    format!(
        "DELETE FROM {t} WHERE id NOT IN (SELECT f.id FROM {t} f WHERE {alive})",
        t = table.prefix(),
        alive = alive_where(table)
    )
}

/// "这行锚还被某行 fact 引用"的条件体。四列都是 `REFERENCES anchor(id)`：
/// subject 是 CASCADE（删锚会删 fact），其余三列是 SET NULL（删锚会抹掉指针）。
/// 四处都要豁免——只留 subject 的话，退场锚仍会让 fact 的地点/说话人静默变 NULL。
fn referenced_by_fact(anchor_id: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM fact f
                  WHERE f.subject_anchor_id={anchor_id} OR f.object_anchor_id={anchor_id}
                     OR f.place_anchor_id={anchor_id} OR f.speaker_anchor_id={anchor_id})"
    )
}

/// `id` 是外层表的端点列名（`fact_link.from_id` / `fact_link.to_id` 这一类）。
/// 端点是 `"<table>:<id>"` 键：逐张可指认表拼一次前缀来解析存在性。
fn endpoint_exists(id: &str) -> String {
    ["anchor", "card"]
        .iter()
        .map(|t| format!("EXISTS (SELECT 1 FROM {t} x WHERE {id}='{t}:'||x.id)"))
        .collect::<Vec<_>>()
        .join(" OR ")
}
