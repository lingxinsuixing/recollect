//! S3c-3a：`review.rows.v1` 载荷（`/v1/rows` 的 fact 行待审面）。
//!
//! 与 fact/event/naming 的 `review.pass.v1` 分开一个 schema：rows 的 `items` 是**已经写好
//! 列值的行**，不是抽取草稿；它们不进 `gate::parse_*` 的草稿形状，而进 `gate_rows` 的列形状。
//! 信封仍是同一条纪律：`batch` 段复用 [`batch_ref`]、`owner` 段复用 [`owner_value`]。
//! **S3c-fix：`batch_key` 换成 rows 专用键**——rows 按行切片，轮集合不足以区分两片
//! （见 [`rows_new_item`]）。
//!
//! 一条 item 固定三键：`{"row": {...}, "evidence_turn_uid": "...", "conflict": null|{...}}`。
//! `row` 是 fact 行的原列（键集合由 hub 的 `allowed_columns("fact")` 把关，本层不裁剪）；
//! `evidence_turn_uid` 也是 `row.evidence_turn_uid` 的副本，单独一格是给重放与窄审的
//! 证据轮入口；`conflict` 是既有事实的异值（S3c-3 填），只作为矛盾判定的输入。

use crate::batch::Batch;
use crate::review::{batch_ref, new_item, owner_value};
use rc_memory::owner::OwnerCtx;
use rc_memory::review::NewItem;
use rc_memory::{BatchCtx, Scope};
use serde_json::{json, Map, Value};

/// rows 载荷的 schema 字面量。与 `review.pass.v1` 并列，不互相冒充。
pub const ROWS_SCHEMA: &str = "review.rows.v1";

/// 一行待审的 fact 行：原列 + 证据轮 + 冲突值。
#[derive(Debug, Clone)]
pub struct RowsRowInput {
    /// fact 行的原列（`id`/`text`/`evidence_turn_uid` 等）。本层不裁剪键集合。
    pub values: Map<String, Value>,
    /// 证据轮：闸门 C1 与窄审只看这一轮。
    pub evidence_turn_uid: String,
    /// 既有事实的异值：`null` 或缺省 ⇒ 无冲突。
    pub conflict: Option<Value>,
}

/// 拼 `review.rows.v1` 信封：`schema/pass_id=rows/family=""/run_id/batch/owner/items`。
///
/// `batch` 与 `owner` 走与 fact/event 同一组函数：同一批的轮清单/源时刻/归属只有一份算法。
pub fn rows_payload(
    run_id: &str,
    batch: &Batch,
    ctx: &BatchCtx,
    owner: &OwnerCtx,
    scope: &Scope,
    rows: &[RowsRowInput],
) -> Value {
    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "row": row.values,
                "evidence_turn_uid": row.evidence_turn_uid,
                "conflict": row.conflict,
            })
        })
        .collect();
    json!({
        "schema": ROWS_SCHEMA,
        "pass_id": "rows",
        "family": "",
        "run_id": run_id,
        "batch": batch_ref(batch, ctx),
        "owner": owner_value(owner, scope),
        "items": items,
    })
}

/// 载荷里逐行的 `row.id`（按 `items` 顺序、原样）。
///
/// 只读 `items[].row.id`：这是行身份的唯一出处（与 hub 的列白名单、replay 的 `rows` 分支
/// 同一格）。缺键/非字符串/空串都收成空串参与判定，由 [`rows_new_item`] 决定怎么处置。
fn row_ids_of(payload: &Value) -> Vec<String> {
    payload
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    item.get("row")
                        .and_then(|row| row.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 构造 rows 的待入队项：`source=distill`、`pass_id=rows`、`family=""`。
///
/// **S3c-fix：`batch_key` 是 rows 专用键**（[`rc_memory::review::rows_batch_key`]：
/// `session`/`device`/排序去重后的 **row id 集合**）。为什么不用 fact/event 那条
/// "轮集合"公式：hub 把一次请求的 fact 行按归属/scope 分组、组内 `chunks(10)` 逐片入队，
/// 同一请求的两片常常共享同一份轮集合（一条长轮抽出多条 fact），于是两片**同键**
/// ⇒ `enqueue` 的顶替把前一片打成 `stale`，前 ≤10 行既不审也不写、HTTP 仍 200
/// （S3c 复审 P0）。行 id 不同 ⇒ 键不同；同一片重试（行集合与顺序规范化后相同）⇒ 键相同。
///
/// 行 id 缺失（空串/缺键/非字符串）或重复 ⇒ **空 key**：`enqueue` 的兼容分支不参与顶替
/// （[`rc_memory::review::enqueue`]），载荷去重照旧。这种形状只可能来自一行坏行——闸在
/// 送审前就按 `payload_shape` 拒（行没有 `id` ⇒ 它的 provenance 轮集合为空），所以空 key
/// 只影响"那条被拒的 item 不顶别人"，不会让正常行互相顶替。这里**不 panic**：载荷里的
/// 行是节点上传的输入，不是本仓的程序不变量。
pub fn rows_new_item(batch: &Batch, owner_json: String, payload: &Value) -> NewItem {
    let row_ids = row_ids_of(payload);
    let unique: std::collections::BTreeSet<&String> = row_ids.iter().collect();
    let well_formed = !row_ids.is_empty()
        && unique.len() == row_ids.len()
        && !row_ids.iter().any(String::is_empty);
    let mut item = new_item("rows", "", batch, owner_json, payload);
    item.batch_key = if well_formed {
        rc_memory::review::rows_batch_key(&batch.session_key, &batch.device_id, &row_ids)
    } else {
        String::new()
    };
    item
}
