//! S3c-3a：`/v1/rows` 的 **fact 行写库原语**（拍板 B1）。
//!
//! 这一层只做一件事：把一个已经过闸的 fact 行写进 `fact` 表，并在**同一事务**里补一行归属。
//! 表名不是形参——这条入口只认 `fact`；其余表没有走这里的入口。B1 的边界因此写在签名上，
//! 而不是运行时再传一个表白名单。非 fact 的行不经过本函数（调用方永远拿不到"换表"的旋钮）。
//!
//! 列白名单从 [`OWNERSHIP`] **现算**（`schema.rs` 是列清单的唯一出处，不手抄第二份）。
//! 未知列 / 重复列 / 空列集都在开事务之前拒掉；错误只带静态话，不回显列名或值（守卫 12）。
//! `id` / `text` / `evidence_turn_uid` 是行的最小身份：缺一格、空串、非 TEXT 都按同一个
//! 静态错误拒。
//!
//! 幂等：`INSERT INTO fact(...) ON CONFLICT(id) DO NOTHING`。只有真正插入（n==1）才写归属
//! ——重复 id 返回 0，既有行与既有归属都不动（"先写者赢"，与 `owner::insert` 同一条纪律）。
//! 写库前的 NOT NULL/CHECK/FK 错误仍由 SQLite 返回 [`MemError::Sql`]，不吞、也不改写成
//! 形状错：那是行的内容问题，不是列白名单问题。

use crate::owner::{self, OwnerCtx};
use crate::schema::OWNERSHIP;
use crate::writers_x::BatchCtx;
use crate::{MemError, Scope};
use rc_store::MemoryHandle;
use rusqlite::types::Value;
use std::collections::HashSet;

/// 这条入口唯一接受的表。不是形参：B1 的边界写进签名，运行时没有第二张表可传。
const FACT_TABLE: &str = "fact";

/// 行的最小身份列：必须存在、是 TEXT、非空串。其余必填列（NOT NULL 且无 DEFAULT）由
/// SQLite 自己判，错误原样回来。
const REQUIRED: [&str; 3] = ["id", "text", "evidence_turn_uid"];

/// 列校验失败的唯一错误族：静态话，不带列名/值。`reason` 只取本文件里的常量短句。
fn invalid(reason: &'static str) -> MemError {
    MemError::Sql(format!("rows: invalid fact column ({reason})"))
}

/// 校验列集并回 `id` 的借用。顺序：空列集 → 未知/重复列 → 必填三格。
///
/// 列名与 [`OWNERSHIP`] 里 `fact` 的登记逐条比对：这份白名单没有第二个副本。
fn validate(values: &[(String, Value)]) -> Result<&str, MemError> {
    if values.is_empty() {
        return Err(invalid("empty column set"));
    }
    let mut seen = HashSet::with_capacity(values.len());
    for (column, _) in values {
        let known = OWNERSHIP
            .iter()
            .any(|owner| owner.table == FACT_TABLE && owner.column == column);
        if !known {
            return Err(invalid("unknown column"));
        }
        if !seen.insert(column.as_str()) {
            return Err(invalid("duplicate column"));
        }
    }
    for required in REQUIRED {
        match values.iter().find(|(column, _)| column == required) {
            Some((_, Value::Text(text))) if !text.is_empty() => {}
            _ => return Err(invalid("missing required column")),
        }
    }
    match values.iter().find(|(column, _)| column == "id") {
        Some((_, Value::Text(id))) => Ok(id),
        // 上面刚逐条验证过；到不了这里，也不拿 `unwrap` 换一个 panic 面。
        _ => Err(invalid("missing required column")),
    }
}

/// 写一行 fact 并补归属；返回真正插入的行数（0 = 幂等命中）。
///
/// `values` 是列名 + SQLite 值，列名必须属于 `fact` 的 `OWNERSHIP` 登记；SQL 里只拼**已经
/// 校验过**的列名，值全部走绑定参数。归属列：`mem_table='fact'`、`mem_id=id`、
/// `created_by_agent=ctx.source_agent`、`visibility=owner::effective(owner.visibility, scope.kind)`、
/// `retention=owner::normalize_retention`、`scope`、`source_agent/device_id` 取 ctx、
/// `created_at_ms=now_ms`。
///
/// 只写 `fact` 一张表：`provenance`/`memory_mention`/`card` 等由各自的写入口负责，本函数
/// 不替它们补行。
pub fn insert_fact_row(
    m: &MemoryHandle,
    values: &[(String, Value)],
    owner: &OwnerCtx,
    scope: &Scope,
    ctx: &BatchCtx,
    now_ms: i64,
) -> Result<usize, MemError> {
    if m.is_read_only() {
        return Err(MemError::ReadOnly);
    }
    let id = validate(values)?.to_string();

    // 列名全部来自上面校验过的 `OWNERSHIP` 登记；值只作为绑定参数。
    let names: Vec<&str> = values.iter().map(|(column, _)| column.as_str()).collect();
    let slots: Vec<String> = (1..=values.len()).map(|i| format!("?{i}")).collect();
    let sql = format!(
        "INSERT INTO {FACT_TABLE}({}) VALUES ({}) ON CONFLICT(id) DO NOTHING",
        names.join(","),
        slots.join(",")
    );

    let tx = m.conn().unchecked_transaction()?;
    let n = tx.execute(
        &sql,
        rusqlite::params_from_iter(values.iter().map(|(_, value)| value)),
    )?;
    // 只有新行才写归属：重复 id 是幂等命中，已有归属不许被后到的调用者改写。
    if n == 1 {
        owner::insert(
            &tx,
            &owner::OwnerRow {
                mem_table: FACT_TABLE.to_string(),
                mem_id: id,
                owner_principal_id: owner.owner_principal_id.clone(),
                owner_agent_id: owner.owner_agent_id.clone(),
                created_by_agent: ctx.source_agent.clone(),
                visibility: owner::effective(owner.visibility, scope.kind),
                retention: owner::normalize_retention(&owner.retention),
                scope: scope.clone(),
                source_agent: Some(ctx.source_agent.clone()),
                device_id: Some(ctx.device_id.clone()),
                created_at_ms: now_ms,
            },
        )?;
    }
    tx.commit()?;
    Ok(n)
}
