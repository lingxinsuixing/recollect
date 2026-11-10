//! 快照的**编排面**：临时副本 → 文件 sha → 清单发布。
//! 三件事分别住在三个 crate（内容过滤在 rc-memory、文件字节在 rc-store、清单状态机在 rc-net），
//! 本模块是唯一知道"按这个顺序调它们"的地方——顺序写错（先签名后过滤）就是发一份假清单。
//!
//! 零路径：错误只有静态短语；`PathBuf` 只进 `std::fs`，从不进任何 `Display`。

use rc_net::key::HubKey;
use rc_net::snapshot::{Manifest, PublishOutcome, SnapshotBuild, RETAIN_COUNT};
use rc_store::Root;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotError {
    Key,
    Build,
    /// 发布前一致性检查拒绝：活卡的指针指向缺失的 fact。单独一格是为了让运维面
    /// 一眼读出"是哪类不变量拦下了发布"，而不是笼统的 build failed。
    Dangling,
    Hash,
    Publish,
}

impl SnapshotError {
    /// 运维面的**唯一**文案来源。一个变体一句静态短语，零插值。
    pub fn message(self) -> &'static str {
        match self {
            SnapshotError::Key => "hub key is unreadable or malformed",
            SnapshotError::Build => "snapshot build failed",
            SnapshotError::Dangling => {
                "snapshot refused: a live card points at a missing fact — run: doctor"
            }
            SnapshotError::Hash => "snapshot file could not be hashed",
            SnapshotError::Publish => "snapshot publish failed",
        }
    }
}

#[derive(Debug)]
pub struct PublishReport {
    /// `false` = 内容没变，旧清单继续有效（临时副本已删除）。
    pub published: bool,
    pub manifest: Manifest,
    /// 当前快照的字节数（`manifest.bytes`，不是这一轮临时文件的大小）。
    pub bytes: u64,
    /// 这一轮在**副本**上过滤掉的计数。`published == false` 时它依然是真的：
    /// 它描述"如果发出去会少哪些行"，不是"这一轮动了中心库"。
    pub pruned: rc_memory::snapshot::PruneReport,
    /// 这一轮发的是哪条通道。目录本身就区分了通道；带上它是给运维面回执用的，
    /// 让"我到底发了 agent 还是 backup"不必再靠目录名反推。
    pub channel: SnapshotChannel,
}

/// 快照的两条通道，各自一个目录、各自一套清单与保留数（`RETAIN_COUNT` 按目录各算各的）。
///
/// - `Agent`：记忆通道，设备日常 pull 用。副本上 `review_item`/`review_decision` 已清空
///   （`keep_review = false`），因此 digest 与备份通道不同。
/// - `Backup`：备份通道，有 `replicate` 能力的设备（standby）用。完整记忆。
///
/// 通道是**按受众**分的，不是按内容新旧：standby 拿 agent 副本应用下去会清空本地审查队列，
/// 所以有能力的一方绝不能回落 agent（回落判定住在 `serve::snapshot_channel_for` 的调用方）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotChannel {
    Agent,
    Backup,
}

impl SnapshotChannel {
    /// 目录名。`Agent` 沿用既有的 `snapshots`：已部署中心的清单指针不能搬家。
    pub fn dir_name(self) -> &'static str {
        match self {
            SnapshotChannel::Agent => "snapshots",
            SnapshotChannel::Backup => "snapshots-backup",
        }
    }

    /// 运维面/回执里的通道名（与 CLI 的 `--channel` 取值同字面）。
    pub fn as_str(self) -> &'static str {
        match self {
            SnapshotChannel::Agent => "agent",
            SnapshotChannel::Backup => "backup",
        }
    }

    /// 只有备份通道保留两张审查侧表。
    pub fn keeps_review(self) -> bool {
        matches!(self, SnapshotChannel::Backup)
    }
}

pub fn channel_dir(data_dir: &Path, channel: SnapshotChannel) -> PathBuf {
    data_dir.join(channel.dir_name())
}

/// 记忆通道目录（既有调用方的入口）。
pub fn snapshots_dir(data_dir: &Path) -> PathBuf {
    channel_dir(data_dir, SnapshotChannel::Agent)
}

/// 按通道发布一代快照。两条通道的临时副本、清单与保留数互不影响。
pub fn publish_channel(
    root: &mut Root,
    key: &HubKey,
    data_dir: &Path,
    now_ms: i64,
    channel: SnapshotChannel,
) -> Result<PublishReport, SnapshotError> {
    let center_commit_seq = crate::replicate::status(root.transcript())
        .map_err(|_| SnapshotError::Build)?
        .last_commit_seq;
    let dir = channel_dir(data_dir, channel);
    let built =
        rc_memory::snapshot::build(root.memory_mut(), &dir, now_ms, channel.keeps_review()).map_err(
            |e| match e {
                // 保持"这一代没发出去"的形状：错误只带类别，不带 card/fact 的 id。
                rc_memory::snapshot::SnapshotError::DanglingCard(_) => SnapshotError::Dangling,
                _ => SnapshotError::Build,
            },
        )?;
    let sha256 = rc_store::snapshot::file_sha256(&built.path).map_err(|_| SnapshotError::Hash)?;
    let outcome = rc_net::snapshot::publish(
        &dir,
        SnapshotBuild {
            temp_db: &built.path,
            sha256: &sha256,
            digest: &built.digest,
            schema_version: rc_store::ddl::MEMORY_VERSION,
            norm_version: rc_store::capability::NORM_VERSION,
            now_ms,
            center_commit_seq,
        },
        key,
        RETAIN_COUNT,
    )
    .map_err(|_| SnapshotError::Publish)?;
    let (published, manifest) = match outcome {
        PublishOutcome::Published(m) => (true, m),
        PublishOutcome::Unchanged(m) => (false, m),
    };
    Ok(PublishReport {
        published,
        bytes: manifest.bytes,
        manifest,
        pruned: built.pruned,
        channel,
    })
}

/// 记忆通道发布：`publish_channel(Agent)` 的薄壳，旧调用点（`/v1/rows` 吸收后）用它。
pub fn publish_snapshot(
    root: &mut Root,
    key: &HubKey,
    data_dir: &Path,
    now_ms: i64,
) -> Result<PublishReport, SnapshotError> {
    publish_channel(root, key, data_dir, now_ms, SnapshotChannel::Agent)
}
