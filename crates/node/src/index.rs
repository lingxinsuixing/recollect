//! 节点副本的派生索引。
//!
//! 中心快照按设计**不带索引层**（`rc_memory::snapshot::prune` 会整层丢弃影子表），
//! 所以节点读完副本后要自己建一份。底座文件必须保持清单的逐字节形状——
//! `sync::complete()` 用清单字节数判"已落地"；索引写到 sibling（`*.indexed.db`），
//! 既不动底座，也不会让下一轮把同一份快照重下。

use crate::state::{self, CurrentManifest};
use crate::sync::SyncError;
use std::path::Path;

/// 为当前副本建索引（索引副本已存在就跳过）。底座缺失 ⇒ `Ok(())`（还没同步过）。
/// `manifest` 由调用方给：`run_sync` 刚 pull 完时状态还没落盘，必须用内存里的那一份。
pub fn ensure_current_replica(
    data_dir: &Path,
    manifest: &CurrentManifest,
    now_ms: i64,
) -> Result<(), SyncError> {
    let Some(base) = state::base_replica_path_of(data_dir, manifest) else { return Ok(()) };
    let indexed = state::indexed_replica_path(&base);
    if indexed.is_file() {
        return Ok(());
    }
    std::fs::copy(&base, &indexed).map_err(|_| SyncError::StoreUnavailable)?;
    let built = rc_store::AgentRoot::open_replica_rw(data_dir, &indexed)
        .map_err(|_| SyncError::StoreUnavailable)
        .and_then(|root| {
            rc_index::rebuild(root.memory(), now_ms)
                .map(|_| ())
                .map_err(|_| SyncError::StoreUnavailable)
        });
    if built.is_err() {
        let _ = std::fs::remove_file(&indexed);
        return built;
    }
    Ok(())
}
