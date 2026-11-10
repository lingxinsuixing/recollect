//! 节点状态文件（JSON，**非新表**）。
//!
//! 里面没有一行 turn、没有一条文件名、没有 token：
//! token 住 `node-token`（私钥级权限），私钥住配置指的那个文件。
//! 状态文件里只有**计数**、**路名**，以及凭票加入时中心给的**端点地址**——
//! 后者不是秘密（它只是"去哪儿找这台中心"），它会被 doctor 读出来给人看。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const STATE_FILE: &str = "node-state.json";
pub const TOKEN_FILE: &str = "node-token";

/// 上行游标。形状与 `rc_ledger::water::Cursor` 同形（到达钟 + turn_uid）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushCursor {
    pub at_ms: i64,
    pub turn_uid: String,
}

/// 节点当前持有的那一份副本的清单。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentManifest {
    pub epoch: u64,
    pub seq: u64,
    pub sha256: String,
    pub created_at_ms: i64,
}

/// 断点续传的位置。`received` 是"已经收下并核过 sha 的字节数"。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialDownload {
    pub epoch: u64,
    pub seq: u64,
    pub received: u64,
}

/// 离线自蒸的状态。**只描述节点自己的动作**，不描述中心是否活着。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelfDistill {
    #[default]
    Normal,
    /// 离线未满一天，或在等钥匙。
    OfflineWaiting,
    /// 离线满一天且钥匙可开；B1 只落状态，蒸馏在 B2 接。
    OfflineDistilling,
    /// 中心回来了，先回传再下发；B3 接。
    Reconciling,
    /// 离线满五天，自己当中心；B4 接。
    DetachedDistilling,
    /// 自备份已建立；B4 接。
    DetachedPromoted,
}

impl SelfDistill {
    pub fn as_str(self) -> &'static str {
        match self {
            SelfDistill::Normal => "normal",
            SelfDistill::OfflineWaiting => "offline_waiting",
            SelfDistill::OfflineDistilling => "offline_distilling",
            SelfDistill::Reconciling => "reconciling",
            SelfDistill::DetachedDistilling => "detached_distilling",
            SelfDistill::DetachedPromoted => "detached_promoted",
        }
    }

    /// 第 5 天的两态。`status` 与 doctor 的 `detached` 那一格都由它算，只算一次。
    pub fn is_detached(self) -> bool {
        matches!(self, SelfDistill::DetachedDistilling | SelfDistill::DetachedPromoted)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncState {
    /// 配对时中心给的公钥。与配置的钉死值不一致 ⇒ 响亮失败。
    pub paired_hub_pub: String,
    /// 凭票配对时票里钉的那枚中心证书指纹（小写 64 hex）。
    /// 之后 `sync` 用它建立 TLS 并钉死链路——**地址与指纹一起从状态来**，
    /// 于是 `node.toml` 里一个端点都不用写。旧状态文件照读（缺省 `None`）。
    #[serde(default)]
    pub paired_tls_fp: Option<String>,
    /// 凭票配对时票里带来的端点 URL（`https://IP:port`，顺序同票）。
    /// 它们是**地址**，不是秘密：doctor 读得出、`status` 不印，但落盘无妨。
    #[serde(default)]
    pub paired_endpoints: Vec<String>,
    pub push_cursor: PushCursor,
    pub pending_turns: i64,
    pub current_manifest: CurrentManifest,
    /// 当前连得上的那条路（**路名**，不是地址）。
    pub current_endpoint: String,
    pub last_rate_bps: u64,
    pub resume_count: u64,
    pub last_push_at_ms: i64,
    pub last_pull_at_ms: i64,
    pub last_push_accepted: i64,
    pub last_push_rejected: i64,
    /// 封闭码，不放错误串。
    pub last_error: String,
    /// 上一次失败的时刻。成功一轮**不清**这一格：`last_error` 说"现在还有没有错"，
    /// 它说"那个错是什么时候的事"——否则读数分不清陈旧错误与当前错误。
    #[serde(default)]
    pub last_error_at_ms: i64,
    pub partial: PartialDownload,
    /// 最后一次成功同步的时刻。
    #[serde(default)]
    pub center_seen_at_ms: i64,
    /// 本次离线起点。
    #[serde(default)]
    pub offline_since_ms: i64,
    /// 单调累计的离线时长；重启后与上次取 max，不许清零。
    #[serde(default)]
    pub offline_elapsed_ms: i64,
    #[serde(default)]
    pub self_distill: SelfDistill,
    /// B2b-3 的钥匙面读数：`locked` / `unlocked` / `fault` 三值（`selfdistill::live_key_reading`）。
    /// `None` = 这台机器还没报告过（旧状态文件照读；status 的那一格现算，不抄这里）。
    #[serde(default)]
    pub llm_key: Option<String>,
    /// B4 的确认锚：回并批次被中心收下的时刻。**0 = 未确认** ⇒ 自备份一份都不删。
    #[serde(default)]
    pub reconcile_confirmed_at_ms: i64,
    /// 最近一次自备份的时刻（`0` = 还没备份过）。
    #[serde(default)]
    pub dr_backup_last_at_ms: i64,
    /// 已建立的自备份份数。
    #[serde(default)]
    pub dr_backup_count: u32,
    /// 本机转录层到达钟的高水位（自备份清单的本机段）。state 是 0 而转录有行时，
    /// `dr::backup_once` 成功后会把它写回。
    #[serde(default)]
    pub local_turn_highwater_ms: i64,
    /// S4d1：因"中心换了地址"而改地址重试过的次数。
    #[serde(default)]
    pub rediscover_ok: u64,
    /// S4d1：问过一句但没找到"我那台中心"的次数。它与 `rediscover_ok` 分开，
    /// 因为两者的下一步动作不同：一个是等中心起回来，一个是查它为什么换了身份。
    #[serde(default)]
    pub rediscover_miss: u64,
    /// 最近一次**真的发了 probe** 的时刻（`0` = 还没问过）。
    /// 找不到候选、地址没变、重试失败都算问过：这一格回答的是"问没问"，
    /// "问到没有"由 `rediscover_ok`/`rediscover_miss` 说，两者混用会让静默 miss 不可见。
    #[serde(default)]
    pub last_rediscover_at_ms: i64,
}

pub fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join(STATE_FILE)
}

impl SyncState {
    /// 状态里那台中心的短名（`hub_fingerprint`）。**公式只在 `rc-net` 有一份**：
    /// 这里转出它，给只依赖 `node` 的读面（`rc-diag`）用；空公钥 ⇒ 空串。
    pub fn hub_fp(&self) -> String {
        if self.paired_hub_pub.is_empty() {
            String::new()
        } else {
            rc_net::invite::hub_fingerprint(&self.paired_hub_pub)
        }
    }
}

pub fn replica_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("replica")
}

pub fn replica_file(data_dir: &Path, epoch: u64, seq: u64) -> PathBuf {
    replica_dir(data_dir).join(format!("snapshot-{epoch}-{seq}.db"))
}

/// 中心签名清单的落档名：`/v1/state` 的**原始 JSON**（含 `signature`）。
/// 名字里带代际：自备份要证明"下行段可验签"，就得有与副本同代的那一份原文。
pub fn manifest_file(data_dir: &Path, epoch: u64, seq: u64) -> PathBuf {
    replica_dir(data_dir).join(format!("manifest-{epoch}-{seq}.json"))
}

/// 状态文件点名的代际底座（未经索引改写的原始副本）。没有状态时回退旧名。
pub fn base_replica_path(data_dir: &Path) -> Option<PathBuf> {
    let state = load(&state_path(data_dir)).ok().flatten();
    let manifest = state.map(|s| s.current_manifest).unwrap_or_default();
    base_replica_path_of(data_dir, &manifest)
}

/// 同一件事的**内存清单**版本：`run_sync` 刚 pull 完、状态还没落盘时用它，
/// 否则会拿旧清单找不到刚下载的那一代。
pub fn base_replica_path_of(data_dir: &Path, manifest: &CurrentManifest) -> Option<PathBuf> {
    if manifest.epoch != 0 || manifest.seq != 0 {
        let path = replica_file(data_dir, manifest.epoch, manifest.seq);
        if path.is_file() {
            return Some(path);
        }
    }
    let legacy = replica_dir(data_dir).join("memory.db");
    legacy.is_file().then_some(legacy)
}

/// 派生索引副本的文件名：`snapshot-1-1.db` → `snapshot-1-1.indexed.db`。
/// 底座文件保持清单的逐字节形状（`complete()` 用它判"已落地"），索引只写这份副本。
pub fn indexed_replica_path(base: &Path) -> PathBuf {
    base.with_extension("indexed.db")
}

/// 当前应该读哪一份：优先节点自己建过索引的副本；没有就回退底座。
pub fn current_replica_path(data_dir: &Path) -> Option<PathBuf> {
    let base = base_replica_path(data_dir)?;
    let indexed = indexed_replica_path(&base);
    Some(if indexed.is_file() { indexed } else { base })
}

/// 只删"当前代确实已经落地"之后的旧文件。当前代不在时**一个都不删**：
/// 升级回退可能正靠旧名 `memory.db` 提供唯一的副本。
/// 顺带清掉非当前代的 `manifest-*.json`：签名清单必须与副本同代，旧代留着只会误导自备份。
pub fn cleanup_old_replicas(data_dir: &Path, current: &CurrentManifest) {
    if current.epoch == 0 && current.seq == 0 {
        return;
    }
    let keep = replica_file(data_dir, current.epoch, current.seq);
    let keep_indexed = indexed_replica_path(&keep);
    let keep_manifest = manifest_file(data_dir, current.epoch, current.seq);
    if !keep.is_file() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(replica_dir(data_dir)) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep || path == keep_indexed || path == keep_manifest {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "memory.db"
            || name == "memory.indexed.db"
            || (name.starts_with("snapshot-") && name.ends_with(".db"))
            || (name.starts_with("manifest-") && name.ends_with(".json"))
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

pub fn token_path(data_dir: &Path) -> PathBuf {
    data_dir.join(TOKEN_FILE)
}

/// 读状态。**缺文件 ⇒ `Ok(None)`**（还没跑过同步），坏文件 ⇒ `Err`。
pub fn load(path: &Path) -> Result<Option<SyncState>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("the node state file is unreadable".to_string()),
    };
    serde_json::from_str(&text).map(Some).map_err(|_| "the node state file is malformed".to_string())
}

/// 写状态。**先写临时名再改名**：崩在两步之间时旧状态还在，
/// 而不是留下一个半截的 JSON（半截的 JSON 读出来是"配对没了"，那是更坏的后果）。
pub fn save(path: &Path, state: &SyncState) -> Result<(), String> {
    let text = serde_json::to_string_pretty(state)
        .map_err(|_| "the node state cannot be rendered".to_string())?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text.as_bytes())
        .map_err(|_| "the node state file is unwritable".to_string())?;
    std::fs::rename(&tmp, path).map_err(|_| "the node state file is unwritable".to_string())
}
