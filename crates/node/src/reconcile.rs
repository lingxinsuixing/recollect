//! B3b：暂存层的新行 → `POST /v1/rows` 的载荷。
//!
//! 只回答一件事：**这一层里有什么行要交给中心**。行的范围由 `staging_new_row`（B2b-2 写下的
//! 行标）划，键表达式逐字复用 [`crate::selfdistill::KEY_SQL`]——同一份拼接不许有第二个家，
//! 否则"标记表里的 id"与"回并时选的 id"迟早会用两套分隔符。
//!
//! 表集是 `KEY_SQL` 的 9 张去掉 `attention`：告警面是中心独占，节点离线期产生的告警
//! 随暂存层一起丢弃（派发档 §三：不是缺陷）。
//!
//! 载荷里的列名与值都直接来自暂存库的 `SELECT *`：列名用 `stmt.column_names()` 现算，
//! 值按 `typeof()` 分派（Text / Integer / Real / Null）。记忆库没有 BLOB 列，遇到就
//! `RowsRejected`——**不静默跳过**，那会让一行在中心缺席而节点以为已经交完。

use crate::selfdistill::KEY_SQL;
use crate::staging;
use crate::sync::SyncError;
use serde_json::{Map, Value};
use std::path::Path;

/// 一次载荷的行数上限。与中心 `hub::rows::MAX_ROWS_PER_BATCH` 同一个数：
/// 超了按 `RowsRejected` 拒，**不静默截断**（保留暂存层，下一轮仍然会失败——这是已知边界）。
pub const MAX_ROWS_PER_PAYLOAD: usize = 20_000;

/// 暂存层标记表里的新行 → `/v1/rows` 载荷。标记为空 ⇒ `None`（不上传、仍可 pull）。
///
/// 输出只含非空表；行内列名与值直接来自暂存库（`SELECT *` + 标记表 JOIN）。
/// 打开/读库失败 ⇒ `StoreUnavailable`；超行数上限、BLOB、坏 UTF-8、序列化失败 ⇒ `RowsRejected`。
pub fn build_rows_payload(data_dir: &Path) -> Result<Option<Vec<u8>>, SyncError> {
    let memory = rc_store::open_replica(&staging::path(data_dir))
        .map_err(|_| SyncError::StoreUnavailable)?;
    let mut tables = Map::new();
    let mut total = 0usize;
    for (table, expr) in KEY_SQL {
        if table == "attention" {
            continue;
        }
        let columns = columns_of(&memory, table)?;
        let types = columns
            .iter()
            .map(|c| format!("typeof(t.{})", quote_ident(c)))
            .collect::<Vec<_>>()
            .join(", ");
        // 标记表的 id 先改名成 `marker_id`：card/fact_link/... 自己也有 `id` 列，
        // 直接把两张表的 `id` 放进同一个 ON 表达式会是 ambiguous column name。
        let sql = format!(
            "SELECT t.*, {types} FROM {table} t \
             JOIN (SELECT table_name, id AS marker_id FROM staging_new_row) m \
               ON m.table_name = ?1 AND m.marker_id = {expr}"
        );
        let mut stmt = memory
            .conn()
            .prepare(&sql)
            .map_err(|_| SyncError::StoreUnavailable)?;
        let mut rows = stmt.query([table]).map_err(|_| SyncError::StoreUnavailable)?;
        let mut out: Vec<Value> = Vec::new();
        while let Some(row) = rows.next().map_err(|_| SyncError::StoreUnavailable)? {
            let mut obj = Map::new();
            for (i, column) in columns.iter().enumerate() {
                let kind: String = row
                    .get(columns.len() + i)
                    .map_err(|_| SyncError::RowsRejected)?;
                let value = match kind.as_str() {
                    "null" => Value::Null,
                    "integer" => Value::from(
                        row.get::<_, i64>(i).map_err(|_| SyncError::RowsRejected)?,
                    ),
                    "real" => {
                        let f = row.get::<_, f64>(i).map_err(|_| SyncError::RowsRejected)?;
                        // JSON 没有 NaN/Inf：折算成 null 会让中心收到一个"值变了"的行。
                        if !f.is_finite() {
                            return Err(SyncError::RowsRejected);
                        }
                        Value::from(f)
                    }
                    // TEXT 必须是合法 UTF-8：载荷是 JSON，坏字节没有可表示的形状。
                    "text" => Value::String(
                        row.get::<_, String>(i).map_err(|_| SyncError::RowsRejected)?,
                    ),
                    // blob（以及 typeof 未知的值）：记忆库不该有，遇到就响亮失败。
                    _ => return Err(SyncError::RowsRejected),
                };
                obj.insert(column.clone(), value);
            }
            total += 1;
            if total > MAX_ROWS_PER_PAYLOAD {
                return Err(SyncError::RowsRejected);
            }
            out.push(Value::Object(obj));
        }
        if !out.is_empty() {
            tables.insert(table.to_string(), Value::Array(out));
        }
    }
    if tables.is_empty() {
        return Ok(None);
    }
    let doc = serde_json::json!({ "api_version": 1, "tables": tables });
    serde_json::to_vec(&doc)
        .map(Some)
        .map_err(|_| SyncError::RowsRejected)
}

/// 一张表的列名（DDL 顺序），从 `SELECT *` 的空结果上现算。
fn columns_of(memory: &rc_store::MemoryHandle, table: &str) -> Result<Vec<String>, SyncError> {
    let stmt = memory
        .conn()
        .prepare(&format!("SELECT t.* FROM {table} t LIMIT 0"))
        .map_err(|_| SyncError::StoreUnavailable)?;
    Ok(stmt.column_names().into_iter().map(str::to_string).collect())
}

/// SQL 标识符加引号。列名来自库的 schema 而不是载荷，但仍按标识符规则转义——
/// 列名里的引号/关键字不该把查询拆开。
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
