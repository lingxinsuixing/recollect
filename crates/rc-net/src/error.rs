/// `rc-net` 的错误面。Display 只许静态短语或 `io::ErrorKind`：
/// 路径、请求行、正文、token、私钥一个字节都不许进来（守卫 12）。
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("io: {0:?}")]
    Io(std::io::ErrorKind),
    #[error("no entropy source")]
    NoEntropy,
    #[error("key file is unreadable")]
    KeyFileUnreadable,
    #[error("key file is malformed")]
    KeyFileMalformed,
    #[error("signature is invalid")]
    Signature,
    /// 中心拒绝这份身份（token 撤销、设备头不符）。不许折成坏应答：两者的下一步动作相反。
    #[error("the hub rejected this identity")]
    AuthRejected,
    #[error("nonce store is full")]
    NonceCapacity,
    #[error("snapshot state is malformed")]
    SnapshotState,
    #[error("manifest schema is outside the supported range")]
    Schema,
    #[error("manifest norm version does not match this build")]
    Norm,
    #[error("manifest epoch is older than the current one")]
    StaleEpoch,
    #[error("manifest seq is older than the current one")]
    StaleSeq,
    #[error("manifest conflicts with the current epoch/seq")]
    EpochSeqConflict,
    #[error("no endpoint is configured")]
    NoEndpoint,
    #[error("endpoint is unreachable")]
    EndpointUnreachable,
    /// TLS 握手或指纹钉死失败。**不是** `EndpointUnreachable`：网络可能通着，
    /// 只是对面不是钉住的那一枚证书——下一步动作是查指纹，不是重试。
    #[error("tls handshake or pinned fingerprint failed")]
    Tls,
    #[error("response is malformed")]
    BadResponse,
    #[error("chunk digest does not match the header")]
    ChunkMismatch,
}
