//! 备份提升与旧中心 fencing。
//!
//! - 提升：备份追平后才允许；epoch+1；旧中心 key 给新中心/新 epoch 签一张提升证书。
//! - fencing：操作员在旧中心写 `FENCED`；旧中心的写入口一律 409，读面仍可降级服务。
//!
//! 这里不碰正文、不打印路径；证书只含中心 id、epoch 与签名。

use rc_net::key::HubKey;
use rc_net::protocol::promotion_canonical;
use rc_store::{Root, StoreError};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const PROMOTION_FORMAT_VERSION: u32 = 1;
pub const FENCE_FORMAT_VERSION: u32 = 1;
pub const FENCE_FILE: &str = "FENCED";
pub const PROMOTIONS_DIR: &str = "promotions";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotionCert {
    pub format_version: u32,
    pub prev_epoch: u64,
    pub new_epoch: u64,
    pub prev_center_id: String,
    pub new_center_id: String,
    pub signed_by_prev_center: bool,
    pub signature: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromoteReport {
    pub new_epoch: u64,
    pub last_commit_seq: u64,
    /// 文件名，不是路径。
    pub cert_file: String,
    pub signed_by_prev_center: bool,
}

#[derive(Debug)]
pub enum PromoteError {
    Store(StoreError),
    Key,
    AlreadyPromoted,
    Write,
    Lag {
        applied_seq: u64,
        expected_seq: u64,
        applied_epoch: u64,
        expected_epoch: u64,
    },
}

impl From<StoreError> for PromoteError {
    fn from(error: StoreError) -> Self {
        PromoteError::Store(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FenceRecord {
    pub format_version: u32,
    pub fenced_epoch: u64,
    pub fenced_at_ms: i64,
    pub reason: String,
}

fn promotions_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(PROMOTIONS_DIR)
}

fn cert_file_name(epoch: u64) -> String {
    format!("promotion-{epoch}.json")
}

pub struct PromoteRequest<'a> {
    pub prev_key: &'a HubKey,
    pub new_key: &'a HubKey,
    pub center_last_seq: u64,
    pub center_epoch: u64,
    pub now_ms: i64,
    pub force: bool,
}

/// 备份追平后才提升。`center_last_seq/center_epoch` 是操作员从中心 status 抄下来的读数；
/// 本地 apply 水位必须与之相等（`force` 才允许带 lag 提升，供演练/紧急只读降级）。
pub fn promote(
    root: &Root,
    data_dir: &Path,
    request: PromoteRequest<'_>,
) -> Result<PromoteReport, PromoteError> {
    let applied = crate::replicate::apply_status(root.transcript())?;
    if !request.force
        && (applied.last_applied_seq != request.center_last_seq
            || applied.last_epoch != request.center_epoch)
    {
        return Err(PromoteError::Lag {
            applied_seq: applied.last_applied_seq,
            expected_seq: request.center_last_seq,
            applied_epoch: applied.last_epoch,
            expected_epoch: request.center_epoch,
        });
    }
    let new_epoch = request
        .center_epoch
        .checked_add(1)
        .ok_or(PromoteError::Key)?;
    let prev_center_id =
        rc_net::key::device_id_of(&request.prev_key.public_b64()).ok_or(PromoteError::Key)?;
    let new_center_id =
        rc_net::key::device_id_of(&request.new_key.public_b64()).ok_or(PromoteError::Key)?;
    let signed_by_prev_center = true;
    let signature = request.prev_key.sign_b64(
        promotion_canonical(request.center_epoch, new_epoch, &prev_center_id, &new_center_id)
            .as_bytes(),
    );
    let cert = PromotionCert {
        format_version: PROMOTION_FORMAT_VERSION,
        prev_epoch: request.center_epoch,
        new_epoch,
        prev_center_id,
        new_center_id,
        signed_by_prev_center,
        signature,
        created_at_ms: request.now_ms,
    };

    let dir = promotions_dir(data_dir);
    std::fs::create_dir_all(&dir).map_err(|_| PromoteError::Write)?;
    let file = cert_file_name(new_epoch);
    let dest = dir.join(&file);
    if dest.exists() {
        return Err(PromoteError::AlreadyPromoted);
    }
    let bytes = serde_json::to_vec(&cert).map_err(|_| PromoteError::Write)?;
    let tmp = dest.with_extension(format!("json.tmp-{}", std::process::id()));
    std::fs::write(&tmp, &bytes).map_err(|_| PromoteError::Write)?;
    if std::fs::rename(&tmp, &dest).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Err(PromoteError::Write);
    }

    root.transcript()
        .conn()
        .execute(
            "UPDATE center_state SET epoch=?1, last_commit_seq=?2 WHERE id=1",
            rusqlite::params![new_epoch as i64, request.center_last_seq as i64],
        )
        .map_err(|error| PromoteError::Store(StoreError::from(error)))?;

    Ok(PromoteReport {
        new_epoch,
        last_commit_seq: request.center_last_seq,
        cert_file: file,
        signed_by_prev_center,
    })
}

/// 验证提升证书：字段链（new = prev+1）、签名者标记、旧 key 的 Ed25519 签名。
pub fn verify_cert(cert: &PromotionCert, prev_pubkey_b64: &str) -> bool {
    cert.format_version == PROMOTION_FORMAT_VERSION
        && cert.signed_by_prev_center
        && cert.prev_epoch.checked_add(1) == Some(cert.new_epoch)
        && rc_net::key::verify_b64(
            prev_pubkey_b64,
            promotion_canonical(
                cert.prev_epoch,
                cert.new_epoch,
                &cert.prev_center_id,
                &cert.new_center_id,
            )
            .as_bytes(),
            &cert.signature,
        )
}

pub fn read_fence(data_dir: &Path) -> Result<Option<FenceRecord>, StoreError> {
    let path = data_dir.join(FENCE_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StoreError::Io(error.to_string())),
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|_| {
        StoreError::Io("the fence file is unreadable".to_string())
    })
}

/// 写 fencing 标记。同 epoch 重写是幂等的；更高 epoch 覆盖旧标记，更低 epoch 不降级。
pub fn write_fence(data_dir: &Path, epoch: u64, now_ms: i64) -> Result<FenceRecord, StoreError> {
    if let Some(existing) = read_fence(data_dir)? {
        if existing.fenced_epoch >= epoch {
            return Ok(existing);
        }
    }
    std::fs::create_dir_all(data_dir)?;
    let record = FenceRecord {
        format_version: FENCE_FORMAT_VERSION,
        fenced_epoch: epoch,
        fenced_at_ms: now_ms,
        reason: "promoted".into(),
    };
    let bytes = serde_json::to_vec(&record)
        .map_err(|_| StoreError::Io("the fence record could not be encoded".to_string()))?;
    let dest = data_dir.join(FENCE_FILE);
    let tmp = data_dir.join(format!("{FENCE_FILE}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, &bytes)?;
    if std::fs::rename(&tmp, &dest).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Err(StoreError::Io("the fence record could not be published".to_string()));
    }
    Ok(record)
}
