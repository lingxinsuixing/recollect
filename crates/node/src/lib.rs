//! 节点的库面：**配置、状态、同步编排**三样。
//!
//! bin 只做 CLI（`pair` / `sync` / `status`），判定与形状全在这里。
//! 状态文件的形状由 `state` 独占——`rc-diag`（doctor 的连通性读数）也读它，
//! 所以它**不能有第二个解析者**。

pub mod index;
/// B2a-3：节点自己的读箱（`secret_ref` → 值）。**只给 B2b 的离线自蒸**，
/// `sync` / `pair` / `status` 不碰它。
pub mod llm_key;
pub mod node_cfg;
pub mod offline;
/// W39 刀 D：边缘的**远端转录读**（细案 `2026-11-09-w39-remote-transcript` §3.2）。
/// 四个转录工具在 `--replica` 下远端优先、本地回落；材料全部来自边缘已有文件
/// （state / token / `device.key`），不新增配置项。
pub mod remote_transcript;
/// B4a：第 5 天的容灾自备份。两段清单、保留锚（未确认永不删）、`last_backup_ms` 读数。
pub mod dr;
/// B3b：暂存层的新行 → `/v1/rows` 载荷。键表达式与 `selfdistill::KEY_SQL` 是同一份。
pub mod reconcile;
/// B2b-2：离线自蒸的一个有界批次 + 生产后端的装配。触发接线（1 天闸、状态机）
/// 与 `status`/`doctor` 读数在 B2b-3；本模块只回答「本地能蒸出来、写暂存层、有行标」。
pub mod selfdistill;
pub mod staging;
pub mod state;
pub mod sync;
