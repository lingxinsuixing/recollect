use sha2::{Digest, Sha256};

/// canonical 基线 v6；v≤5 已由 store 拒绝，协议不接受旧快照。
pub const SCHEMA_MIN: i32 = 6;
pub const SCHEMA_MAX: i32 = 6;
pub const MAX_HEADER_BYTES: usize = 16 * 1024;
pub const MAX_HEADERS: usize = 64;
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
pub const PAIR_CODE_TTL_MS: i64 = 300_000;
pub const NONCE_TTL_MS: i64 = 120_000;
pub const DEFAULT_MAX_SNAPSHOT_BPS: u64 = 1_572_864;
/// 单次快照响应（含 Range 分片）的字节上限。大于它的整份快照必须走 Range；
/// 一个分片超过它直接 416。10 MiB 是"一个 `Vec` 装得下"的上限，
/// 也是"节点不会因为要一份快照就把整库读进内存"的保证。
pub const MAX_SNAPSHOT_CHUNK: usize = 10 * 1024 * 1024;
pub const API_VERSION: u32 = 1;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// 三个 canonical 各占一个函数，**不许**在调用点手拼字符串：
/// 客户端与服务端各拼一遍就是下一次"签名只在一边对"的来源。
pub fn challenge_canonical(device_id: &str, nonce: &str, expires_at_ms: i64) -> String {
    format!("recollect-challenge-v1\n{device_id}\n{nonce}\n{expires_at_ms}")
}

pub fn request_canonical(
    method: &str,
    target: &str,
    body_sha256: &str,
    device_id: &str,
    nonce: &str,
) -> String {
    format!("recollect-req-v1\n{method}\n{target}\n{body_sha256}\n{device_id}\n{nonce}")
}

/// v2 把中心 epoch 签进请求：旧中心即使还活着，也认不出新 epoch 的合法请求。
pub fn request_canonical_v2(
    method: &str,
    target: &str,
    body_sha256: &str,
    device_id: &str,
    nonce: &str,
    epoch: u64,
) -> String {
    format!(
        "recollect-req-v2\n{method}\n{target}\n{body_sha256}\n{device_id}\n{nonce}\n{epoch}"
    )
}

pub fn manifest_canonical(
    epoch: u64,
    seq: u64,
    sha256: &str,
    schema_version: i32,
    norm_version: u32,
) -> String {
    format!("recollect-manifest-v1\n{epoch}\n{seq}\n{sha256}\n{schema_version}\n{norm_version}")
}

/// 提升证书的签名体：旧中心用旧 key 证明“这个新 key/新 epoch 是继任者”。
/// 与请求/清单 canonical 一样，只住这一个函数，签发与验证共用。
pub fn promotion_canonical(
    prev_epoch: u64,
    new_epoch: u64,
    prev_center_id: &str,
    new_center_id: &str,
) -> String {
    format!("recollect-promotion-v1\n{prev_epoch}\n{new_epoch}\n{prev_center_id}\n{new_center_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_strings_are_byte_stable() {
        assert_eq!(
            challenge_canonical("dev-1", "n1", 123),
            "recollect-challenge-v1\ndev-1\nn1\n123"
        );
        assert_eq!(
            request_canonical("POST", "/v1/ingest?x=1", "ab", "dev-1", "n1"),
            "recollect-req-v1\nPOST\n/v1/ingest?x=1\nab\ndev-1\nn1"
        );
        assert_eq!(
            request_canonical_v2("POST", "/v1/ingest?x=1", "ab", "dev-1", "n1", 7),
            "recollect-req-v2\nPOST\n/v1/ingest?x=1\nab\ndev-1\nn1\n7"
        );
        assert_eq!(
            manifest_canonical(7, 3, "ff", 6, 1),
            "recollect-manifest-v1\n7\n3\nff\n6\n1"
        );
        assert_eq!(
            promotion_canonical(3, 4, "old", "new"),
            "recollect-promotion-v1\n3\n4\nold\nnew"
        );
    }

    #[test]
    fn sha256_hex_is_lowercase_and_matches_a_known_vector() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
