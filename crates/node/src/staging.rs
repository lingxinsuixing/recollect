//! 离线暂存层：底座副本之外的一份**可整层丢弃**的记忆库。
//!
//! 离线自蒸（B2）写这里，不写中心副本；回并成功后整层删除。读面在暂存层存在时
//! 优先读它，并把 `source` 从 `replica` 切到 `pending_reconcile`。
//! 底座副本保持清单的逐字节形状（`sync::complete()` 仍用它判"已落地"）。

use crate::state::{self, CurrentManifest};
use crate::sync::SyncError;
use std::path::{Path, PathBuf};

pub const STAGING_FILE: &str = "pending.db";

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(STAGING_FILE)
}

pub fn is_active(data_dir: &Path) -> bool {
    path(data_dir).is_file()
}

/// 从当前底座复制出暂存层，建索引，并建"离线新行"标记表。已存在 ⇒ 幂等返回。
pub fn prepare(
    data_dir: &Path,
    manifest: &CurrentManifest,
    now_ms: i64,
) -> Result<(), SyncError> {
    let Some(base) = state::base_replica_path_of(data_dir, manifest) else {
        return Err(SyncError::StoreUnavailable);
    };
    let dest = path(data_dir);
    if dest.is_file() {
        return Ok(());
    }
    std::fs::copy(&base, &dest).map_err(|_| SyncError::StoreUnavailable)?;
    let built = (|| -> Result<(), SyncError> {
        let root = rc_store::AgentRoot::open_replica_rw(data_dir, &dest)
            .map_err(|_| SyncError::StoreUnavailable)?;
        rc_index::rebuild(root.memory(), now_ms).map_err(|_| SyncError::StoreUnavailable)?;
        root.memory()
            .conn()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS staging_new_row (
                   id TEXT PRIMARY KEY,
                   table_name TEXT NOT NULL
                 )",
            )
            .map_err(|_| SyncError::StoreUnavailable)?;
        Ok(())
    })();
    if built.is_err() {
        let _ = std::fs::remove_file(&dest);
    }
    built
}

/// 离线期新蒸出来的行句柄（`card:<hex>` 等）。读面用它给行加 `pending: true`。
pub fn new_rows(data_dir: &Path) -> Vec<String> {
    let Ok(m) = rc_store::open_replica(&path(data_dir)) else {
        return Vec::new();
    };
    let mut st = match m.conn().prepare("SELECT id FROM staging_new_row ORDER BY id") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    st.query_map([], |r| r.get::<_, String>(0))
        .map(|it| it.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

/// 整层丢弃。回并成功后由 B3 调用；底座与索引副本不动。
pub fn drop_layer(data_dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path(data_dir)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
