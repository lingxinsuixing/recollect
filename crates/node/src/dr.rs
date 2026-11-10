//! 第 5 天的容灾自备份：**两段清单 + 未确认永不删**。
//!
//! 一份备份 = `<data-dir>/dr/backup-<now_ms>/`：当前副本、暂存层（若有）、本机转录、
//! 节点状态文件，以及中心签名清单的原文（若有）。目录里的 `manifest.json` 分两段：
//!
//! - `downlink`：中心清单的签名字段 + `signed_manifest` 指向 `center-manifest.json`
//!   —— **这一段可以回到中心验签**；
//! - `local`：本机增量文件的 sha 与转录高水位 —— **这一段只能自证**。
//!
//! 对外措辞不许写成"备份带签名"：只有下行段带中心的签名。
//!
//! 保留锚是**事件不是时钟**：`reconcile_confirmed_at_ms == 0`（中心还没确认收下回并的
//! 那一批）时一份都不删；中心确认之后，才允许删"早于确认时刻且超出最近 `keep` 份"的
//! 旧目录。最新一份永不删——中心永远不回来时，最后一份不许被保留器自己清掉。

use crate::state::{self, SyncState};
use rc_store::snapshot::{file_sha256, vacuum_into};
use std::path::{Path, PathBuf};

/// 一次自备份的读数。`bytes` 是这一份备份落盘的总字节数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrReport {
    pub dir: PathBuf,
    pub bytes: u64,
    /// 本函数不删旧份（保留窗口归 [`rotate`]），这里恒为 0；字段留着给调用方的统一读数。
    pub removed: usize,
    /// 转录层 `max(ingested_at_ms)`；没有行 ⇒ 0。
    pub local_turn_highwater_ms: i64,
}

/// 自备份的封闭故障码。不携带路径与错误串。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrFault {
    /// 落盘失败（建目录、拷贝、`VACUUM INTO`、写清单）。
    Io,
    /// 没有 `node-state.json`：没有可自证的本机状态，这次不做。
    NoState,
    /// 状态点名的副本与旧名 `memory.db` 都不在：没有可备的下行段。
    NoReplica,
}

/// 做一份自备份。同一 `now_ms` 的目录已存在 ⇒ 先删再建（幂等）。
pub fn backup_once(data_dir: &Path, st: &SyncState, now_ms: i64) -> Result<DrReport, DrFault> {
    // 状态文件的拷贝是自备份的一部分：缺了它，"我是谁、同步到哪"就没有本机凭据。
    if !state::state_path(data_dir).is_file() {
        return Err(DrFault::NoState);
    }
    let Some(replica) = state::base_replica_path_of(data_dir, &st.current_manifest) else {
        return Err(DrFault::NoReplica);
    };
    let dr_dir = data_dir.join("dr");
    std::fs::create_dir_all(&dr_dir).map_err(|_| DrFault::Io)?;
    let dest = dr_dir.join(format!("backup-{now_ms}"));
    if dest.exists() {
        std::fs::remove_dir_all(&dest).map_err(|_| DrFault::Io)?;
    }
    std::fs::create_dir(&dest).map_err(|_| DrFault::Io)?;

    // 下行段：副本是中心发来的静态下载文件（无 WAL），普通拷贝即可保持清单的逐字节形状。
    std::fs::copy(&replica, dest.join("replica.db")).map_err(|_| DrFault::Io)?;

    // 本机增量段。暂存层（若有）与转录层都在 WAL 下，裸拷会丢最后一截 ⇒ 一律 `VACUUM INTO`。
    let mut local_files: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    let pending = crate::staging::path(data_dir);
    if pending.is_file() {
        let handle = rc_store::open_replica(&pending).map_err(|_| DrFault::Io)?;
        vacuum_into(handle.conn(), &dest.join("pending.db")).map_err(|_| DrFault::Io)?;
        let sha = file_sha256(&dest.join("pending.db")).map_err(|_| DrFault::Io)?;
        local_files.insert("pending.db".into(), serde_json::Value::String(sha));
    }

    let root = rc_store::NodeRoot::open_node(data_dir, None).map_err(|_| DrFault::Io)?;
    vacuum_into(root.transcript().conn(), &dest.join("transcript.db"))
        .map_err(|_| DrFault::Io)?;
    let sha = file_sha256(&dest.join("transcript.db")).map_err(|_| DrFault::Io)?;
    local_files.insert("transcript.db".into(), serde_json::Value::String(sha));
    let local_turn_highwater_ms = root
        .transcript()
        .conn()
        .query_row("SELECT max(ingested_at_ms) FROM turns", [], |r| {
            r.get::<_, Option<i64>>(0)
        })
        .map_err(|_| DrFault::Io)?
        .unwrap_or(0);
    drop(root);

    let state_src = state::state_path(data_dir);
    if state_src.is_file() {
        std::fs::copy(&state_src, dest.join("node-state.json")).map_err(|_| DrFault::Io)?;
        let sha = file_sha256(&dest.join("node-state.json")).map_err(|_| DrFault::Io)?;
        local_files.insert("node-state.json".into(), serde_json::Value::String(sha));
    }

    // 下行段的凭据：与副本同代的中心签名清单原文。不在也照常备份（`signed_manifest`
    // 仍指向这个名字，缺失本身是可见的），但那时"下行段可验签"就只剩一句空话。
    let center = state::manifest_file(data_dir, st.current_manifest.epoch, st.current_manifest.seq);
    if center.is_file() {
        std::fs::copy(&center, dest.join("center-manifest.json")).map_err(|_| DrFault::Io)?;
    }

    let manifest = serde_json::json!({
        "downlink": {
            "epoch": st.current_manifest.epoch,
            "seq": st.current_manifest.seq,
            "sha256": st.current_manifest.sha256,
            "created_at_ms": st.current_manifest.created_at_ms,
            "signed_manifest": "center-manifest.json",
        },
        "local": {
            "files": local_files,
            "local_turn_highwater_ms": local_turn_highwater_ms,
            "created_at_ms": now_ms,
        },
    });
    let text = serde_json::to_string_pretty(&manifest).map_err(|_| DrFault::Io)?;
    std::fs::write(dest.join("manifest.json"), text).map_err(|_| DrFault::Io)?;

    let bytes: u64 = std::fs::read_dir(&dest)
        .map_err(|_| DrFault::Io)?
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum();
    Ok(DrReport { dir: dest, bytes, removed: 0, local_turn_highwater_ms })
}

/// 扫描 `dr/backup-<ts>`。名字解析不出时间戳的目录不碰——那不是我们建的。
fn backups(data_dir: &Path) -> Vec<(i64, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(data_dir.join("dr")) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(ts) = name
            .to_string_lossy()
            .strip_prefix("backup-")
            .and_then(|s| s.parse::<i64>().ok())
        else {
            continue;
        };
        out.push((ts, path));
    }
    out
}

/// 按保留锚删旧份。`confirmed_at_ms == 0` ⇒ 一份都不删（只增不减）。
///
/// 确认之后：按时间戳降序保留最近 `keep` 份，其余**只删早于确认时刻**的；最新一份永不删。
pub fn rotate(data_dir: &Path, confirmed_at_ms: i64, keep: usize) -> Result<usize, DrFault> {
    // 保留锚是事件不是时钟：中心没确认收下之前，一份都不删。
    if confirmed_at_ms == 0 {
        return Ok(0);
    }
    let mut all = backups(data_dir);
    all.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts));
    let newest = all.first().map(|(ts, _)| *ts);
    let mut removed = 0;
    for (i, (ts, path)) in all.iter().enumerate() {
        if i < keep || Some(*ts) == newest {
            continue;
        }
        // 第二道保险：确认时刻为 0（未确认）时没有一份是"早于确认"的。
        // 上面那条早退已经在生产路径上挡住它；这里再挡一次，免得早退哪天被搬走。
        if confirmed_at_ms > 0 && *ts >= confirmed_at_ms {
            continue;
        }
        std::fs::remove_dir_all(path).map_err(|_| DrFault::Io)?;
        removed += 1;
    }
    Ok(removed)
}

/// 最近一份自备份的时刻（没有 ⇒ `None`）。
pub fn last_backup_ms(data_dir: &Path) -> Option<i64> {
    backups(data_dir).into_iter().map(|(ts, _)| ts).max()
}
