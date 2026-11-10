//! 快照的**文件面**：`VACUUM INTO`、文件 sha、逻辑内容摘要。
//! 这里不认识任何表名——摘要的表格清单由 `rc-memory` 传入（`MEMORY_TABLES` 只有一个家）。
//! 也不认识 `HubKey`/`Manifest`：签名与清单住 `rc-net`，本模块只交字节与十六进制串。

use crate::StoreError;
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

/// `VACUUM INTO` 是 SQLite 唯一能产出"不含 WAL 的单文件"的手段。
/// 目标必须不存在（SQLite 的硬要求）：调用方给的是同目录下的临时名。
/// 本机实测过三格：绑定参数可用；已存在的目标返回错误；副本的 `journal_mode` 是
/// `delete` 而不是 `wal`，且目标旁边没有 `-wal`/`-shm`。
pub fn vacuum_into(conn: &Connection, dest: &Path) -> Result<(), StoreError> {
    let dest = dest
        .to_str()
        .ok_or_else(|| StoreError::Io("snapshot path is not valid UTF-8".to_string()))?;
    conn.execute("VACUUM INTO ?1", [dest])?;
    Ok(())
}

/// 文件内容的 sha256（小写十六进制）。分块读，不把整份快照装进内存。
pub fn file_sha256(path: &Path) -> Result<String, StoreError> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(h.finalize()))
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

struct Column {
    name: String,
    pk: i64,
}

/// 逻辑内容摘要：**只由行内容决定**，与文件布局、插入顺序、`VACUUM` 无关。
/// 编码是自定界的（类型标签 + 8 字节小端长度），因此两个相邻字段的内容拼不出同一个字节串；
/// 类型标签同时把 `NULL` 与空串、整数与同形的文本分开。
///
/// `tables` 由调用方给（`rc-memory` 传 `MEMORY_TABLES`），本函数排序后再编码：
/// 同一组表无论以什么顺序传入都得到同一个摘要，少一张表或加一张表一定改变摘要。
/// 列清单走 `pragma_table_info` 现读，因此以后 `ALTER TABLE ADD COLUMN` 不需要改这里；
/// 行序用表自己的主键列（`pk` 序号），所以也不需要在这里登记表的键（清单由调用方给）。
pub fn logical_digest(conn: &Connection, tables: &[&str]) -> Result<String, StoreError> {
    let mut names = tables.to_vec();
    names.sort_unstable();
    let mut buf = Vec::new();
    buf.extend_from_slice(b"recollect-snapshot-digest-v1\n");
    for name in names {
        encode_table(conn, name, &mut buf)?;
    }
    let mut h = Sha256::new();
    h.update(&buf);
    Ok(hex(h.finalize()))
}

fn encode_table(conn: &Connection, table: &str, out: &mut Vec<u8>) -> Result<(), StoreError> {
    let cols = table_columns(conn, table)?;
    if cols.is_empty() {
        return Err(StoreError::Sql(format!("snapshot: no such table {table}")));
    }
    let mut pk: Vec<&Column> = cols.iter().filter(|c| c.pk > 0).collect();
    if pk.is_empty() {
        return Err(StoreError::Sql(format!(
            "snapshot: table {table} has no primary key"
        )));
    }
    pk.sort_by_key(|c| c.pk);
    let select = format!(
        "SELECT {} FROM {} ORDER BY {}",
        cols.iter()
            .map(|c| quote_ident(&c.name))
            .collect::<Vec<_>>()
            .join(", "),
        quote_ident(table),
        pk.iter()
            .map(|c| quote_ident(&c.name))
            .collect::<Vec<_>>()
            .join(", ")
    );
    out.push(b'T');
    put_bytes(out, table.as_bytes());
    let mut st = conn.prepare(&select)?;
    let mut rows = st.query([])?;
    while let Some(row) = rows.next()? {
        for i in 0..cols.len() {
            put_value(out, row.get_ref(i)?);
        }
        out.push(b'\n');
    }
    out.push(b'E');
    Ok(())
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<Column>, StoreError> {
    let mut st = conn.prepare("SELECT name, pk FROM pragma_table_info(?1)")?;
    let cols = st
        .query_map([table], |r| {
            Ok(Column {
                name: r.get(0)?,
                pk: r.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(cols)
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn put_value(out: &mut Vec<u8>, value: ValueRef<'_>) {
    match value {
        ValueRef::Null => out.push(b'n'),
        ValueRef::Integer(i) => {
            out.push(b'i');
            out.extend_from_slice(&i.to_le_bytes());
        }
        ValueRef::Real(f) => {
            out.push(b'r');
            out.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        ValueRef::Text(t) => {
            out.push(b't');
            put_bytes(out, t);
        }
        ValueRef::Blob(b) => {
            out.push(b'b');
            put_bytes(out, b);
        }
    }
}
