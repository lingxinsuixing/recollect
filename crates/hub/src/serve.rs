//! 中心侧的 HTTP 面。**时间从这里进、数据库从这里下**：
//! `rc-net` 只认识 `Handler`，不认识 `Root`；`HubState` 是两者唯一的接缝。
//! 本文件不取系统时钟（N8）——`now` 是构造参数，测试用固定钟。

use crate::hub_cfg::HubConfig;
use crate::snapshot::SnapshotChannel;
use rc_ledger::device::{self, DeviceRow, PairOutcome};
use rc_ledger::invite::{self as invite_store, InviteOutcome};
use rc_net::http::{parse_range, Handler, RangeError, Request, Response};
use rc_net::invite::{HubIdentity, IDENTITY_VERSION, InviteError, IDENTITY_TTL_MS};
use rc_net::key::HubKey;
use rc_net::pair::{self, NonceStore};
use rc_net::protocol::{API_VERSION, MAX_SNAPSHOT_CHUNK, NONCE_TTL_MS, SCHEMA_MAX, SCHEMA_MIN};
use rc_net::snapshot::{load_state, Manifest};
use rc_store::Root;
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct AuthRejection {
    pub status: u16,
    pub code: &'static str,
}

/// 操作员标记：设备全部换成带 epoch 的客户端后，用它关掉 epoch 1 的 v1 兼容窗口。
/// 存在即要求每个签名请求都带 epoch 头；`promote` 之后 epoch>1 本来就已经要求。
pub const REQUIRE_EPOCH_FILE: &str = "REQUIRE_EPOCH";

fn epoch_marker_set(data_dir: &Path) -> bool {
    data_dir.join(REQUIRE_EPOCH_FILE).is_file()
}

fn reject(status: u16, code: &'static str) -> AuthRejection {
    AuthRejection { status, code }
}

pub struct HubState {
    root: Root,
    key: HubKey,
    cfg: HubConfig,
    /// 快照**根**目录：两条 GET 按设备能力在 `snapshots`/`snapshots-backup` 之间选。
    data_dir: PathBuf,
    nonces: NonceStore,
    /// `Send + Sync`：测试与嵌入方要把 `HubState` 搬进服务线程；时钟本身仍是注入的。
    now: Box<dyn Fn() -> i64 + Send + Sync>,
    /// S3c-4：`/v1/rows` 的 fact 行同步窄审座位。`None` = 这台中心没有在线审核能力
    /// （fact 行请求回 503 `review_unavailable`）；`new` 恒为 `None`，由
    /// [`HubState::with_backend_factory`] 注入。
    backend_factory: Option<BackendFactory>,
    /// S4b1：本进程 TLS 终端的证书指纹（`sha256(cert DER)` 小写 hex）。
    /// 明文模式下 `None` ⇒ 公告里的 `tls_fp` 是空串、票里的 `tls_fp` 也必须是空串。
    /// 它由 serve 在起 TLS 时从 `TlsIdentity` 抄来，不重新读盘：读盘会多一条
    /// "公告的指纹与握手的指纹不是同一份"的岔路。
    tls_fingerprint: Option<String>,
}

/// `/v1/rows` 的审核座位构造器：每次请求现造一个后端（闭包里持有 `llm.toml` 路径）。
///
/// `Send + Sync` 是把它放进 `HubState` 的条件；`Err(())` 只表示"这一次造不出来"，
/// 与"没有座位"（`None`）在 HTTP 上是同一格：503 `review_unavailable`。
pub type BackendFactory = Box<dyn Fn() -> Result<Box<dyn rc_llm::LlmBackend>, ()> + Send + Sync>;

impl HubState {
    pub fn new(
        root: Root,
        key: HubKey,
        cfg: HubConfig,
        data_dir: &Path,
        now: Box<dyn Fn() -> i64 + Send + Sync>,
    ) -> HubState {
        HubState {
            root,
            key,
            cfg,
            data_dir: data_dir.to_path_buf(),
            nonces: NonceStore::new(NONCE_TTL_MS),
            now,
            backend_factory: None,
            tls_fingerprint: None,
        }
    }

    /// S4b1：serve 起 TLS 时把终端指纹交进来；明文档不调它（保持 `None`）。
    pub fn with_tls_fingerprint(mut self, fingerprint: Option<String>) -> Self {
        self.tls_fingerprint = fingerprint;
        self
    }

    /// S3c-4：注入 fact 行同步窄审的座位构造器（`serve --llm-config` 与测试用）。
    pub fn with_backend_factory(mut self, factory: BackendFactory) -> Self {
        self.backend_factory = Some(factory);
        self
    }

    fn now_ms(&self) -> i64 {
        (self.now)()
    }

    /// 旧中心被提升后由操作员写 `FENCED`。写入口一律拒绝；读面仍可降级服务。
    /// 文件存在但读不动 ⇒ 失败关闭（宁可拒绝写，也不让一个坏标记当作没标记）。
    fn ensure_not_fenced(&self) -> Option<Response> {
        match crate::promote::read_fence(&self.data_dir) {
            Ok(None) => None,
            Ok(Some(fence)) => match rc_ledger::transcript::center_epoch(self.root.transcript()) {
                Ok(epoch) if fence.fenced_epoch >= epoch => {
                    Some(Response::error(409, "center_fenced"))
                }
                Ok(_) => None,
                Err(_) => Some(Response::error(503, "store_unavailable")),
            },
            Err(_) => Some(Response::error(409, "center_fenced")),
        }
    }

    /// 认证的前两道：token 的主人 + 头里的 device 必须是同一个人。
    /// 少了第二道，一台设备可以拿自己的 token 顶着别人的 device_id 签名——
    /// 中心会把别人的名字写进审计与计数器。
    pub fn bearer_device(&self, req: &Request) -> Result<DeviceRow, AuthRejection> {
        let token = req
            .header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .filter(|t| !t.is_empty())
            .ok_or_else(|| reject(401, "invalid_token"))?;
        let row = device::find_by_token_hash(self.root.ledger(), &pair::hash_token(token))
            .map_err(|_| reject(503, "store_unavailable"))?
            .ok_or_else(|| reject(401, "invalid_token"))?;
        if row.revoked_at_ms.is_some() {
            return Err(reject(403, "device_revoked"));
        }
        let claimed = req
            .header("x-recollect-device")
            .ok_or_else(|| reject(403, "device_mismatch"))?;
        if claimed != row.device_id {
            return Err(reject(403, "device_mismatch"));
        }
        Ok(row)
    }

    pub fn authenticate(&mut self, req: &Request) -> Result<DeviceRow, AuthRejection> {
        self.authenticate_over(req, &req.body)
    }

    /// 五道的顺序是判定的一部分：
    /// 1. token → 设备行；2. 未撤销；3. device 头 = token 的主人；
    /// 4. **先验签**；5. **后消费 nonce**。
    ///
    /// 第 4、5 两道不能反：反过来的话，一个没有私钥的人拿一个偷看到的 nonce
    /// 就能把合法设备的这一发请求烧掉（合法设备再用同一 nonce 时撞 `bad_nonce`），
    /// 而签名根本还没被检查——这是一条不花钥匙就能做到的拒绝服务。
    pub fn authenticate_over(
        &mut self,
        req: &Request,
        signed_body: &[u8],
    ) -> Result<DeviceRow, AuthRejection> {
        let row = self.bearer_device(req)?;
        let nonce = req
            .header("x-recollect-nonce")
            .ok_or_else(|| reject(401, "bad_nonce"))?
            .to_string();
        let sig = req
            .header("x-recollect-sig")
            .ok_or_else(|| reject(403, "bad_signature"))?;
        let canonical = match req.header("x-recollect-epoch") {
            Some(value) => {
                let epoch = match value.parse::<u64>() {
                    Ok(epoch) => epoch,
                    Err(_) => return Err(reject(403, "bad_epoch")),
                };
                let current = rc_ledger::transcript::center_epoch(self.root.transcript())
                    .map_err(|_| reject(503, "store_unavailable"))?;
                if epoch != current {
                    return Err(reject(409, "stale_epoch"));
                }
                rc_net::protocol::request_canonical_v2(
                    &req.method,
                    &req.target,
                    &rc_net::protocol::sha256_hex(signed_body),
                    &row.device_id,
                    &nonce,
                    epoch,
                )
            }
            None => {
                let current = rc_ledger::transcript::center_epoch(self.root.transcript())
                    .map_err(|_| reject(503, "store_unavailable"))?;
                // epoch 1 的 v1 兼容只留给“还没升完”的窗口；标记一落就不再放行。
                if current != 1 || epoch_marker_set(&self.data_dir) {
                    return Err(reject(409, "epoch_required"));
                }
                rc_net::protocol::request_canonical(
                    &req.method,
                    &req.target,
                    &rc_net::protocol::sha256_hex(signed_body),
                    &row.device_id,
                    &nonce,
                )
            }
        };
        if !rc_net::key::verify_b64(&row.pubkey, canonical.as_bytes(), sig) {
            return Err(reject(403, "bad_signature"));
        }
        if !self.nonces.consume(&row.device_id, &nonce, self.now_ms()) {
            return Err(reject(401, "bad_nonce"));
        }
        Ok(row)
    }

    /// `POST /v1/pair`：两条入口，**恰给一个**——
    /// `{ticket, device_id, pubkey}`（S4b1 的正路）或 `{code, device_id, pubkey}`（旧路，弃用但兼容）。
    ///
    /// 顺序是判定的一部分：设备 id 必须真从公钥导出（否则一台设备能顶着别人的 id 配对）→
    /// 凭证分支（票要过签名/中心/终端指纹）→ 消费凭证并落设备（一个事务）。
    /// 事务之前任何一步失败都**不**碰票：一个错提交不该烧掉别人手上的票。
    fn pair(&mut self, req: &Request) -> Response {
        if let Some(response) = self.ensure_not_fenced() {
            return response;
        }
        let doc: serde_json::Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => return Response::error(400, "bad_pair_request"),
        };
        let (Some(device_id), Some(pubkey)) = (
            doc.get("device_id").and_then(|v| v.as_str()),
            doc.get("pubkey").and_then(|v| v.as_str()),
        ) else {
            return Response::error(400, "bad_pair_request");
        };
        // id 必须真的从公钥导出。否则一台设备可以顶着另一台的 id 配对，
        // 之后用自己的钥匙签自己的请求——中心读到的是别人的名字。
        if rc_net::key::device_id_of(pubkey).as_deref() != Some(device_id) {
            return Response::error(400, "bad_pair_request");
        }
        let ticket = doc.get("ticket").and_then(|v| v.as_str());
        let code = doc.get("code").and_then(|v| v.as_str());
        match (ticket, code) {
            (Some(_), Some(_)) => Response::error(400, "bad_pair_request"),
            (None, None) => Response::error(400, "bad_pair_request"),
            (Some(ticket), None) => self.pair_with_ticket(ticket, device_id, pubkey),
            (None, Some(code)) => self.pair_with_code(code, device_id, pubkey),
        }
    }

    /// 票这一支：验签 + 中心指纹 + TLS 指纹 + 过期，再交给账本消费。
    ///
    /// 票里的 `hub_fp` 由 `verify_ticket` 判（"本中心的票"只有一处规则）；这里判的是
    /// **这一次会话**的终端指纹——明文档要求空串，TLS 档要求逐字节相同。
    /// 两个指纹都不回显：错误面只说明是哪一格不对。
    fn pair_with_ticket(&mut self, ticket: &str, device_id: &str, pubkey: &str) -> Response {
        let now = self.now_ms();
        let verified = match rc_net::invite::verify_ticket(ticket, &self.key.public_b64()) {
            Ok(t) => t,
            Err(InviteError::HubMismatch) => return Response::error(403, "invite_hub_mismatch"),
            Err(InviteError::Version) => return Response::error(403, "invite_version"),
            Err(InviteError::Signature | InviteError::Format | InviteError::Encode) => {
                return Response::error(403, "invite_invalid")
            }
        };
        let own_tls_fp = self.tls_fingerprint.as_deref().unwrap_or_default();
        if verified.tls_fp != own_tls_fp {
            return Response::error(403, "invite_tls_mismatch");
        }
        // 过期的票不签发 token（省掉一发无用的随机数），账本那一步也会再判一次。
        if verified.expires_at_ms < now {
            return Response::error(403, "invite_expired");
        }
        // 时钟与 epoch 必须在**消费之前**读到：先落设备再读 epoch，读失败就只能回 503，
        // 而 token 已经不在任何人手上、票也已用过 ⇒ 这台设备永久失去这一发。
        let epoch = match rc_ledger::transcript::center_epoch(self.root.transcript()) {
            Ok(epoch) => epoch,
            Err(_) => return Response::error(503, "store_unavailable"),
        };
        let (token, token_hash) = match pair::new_token() {
            Ok(t) => t,
            Err(_) => return Response::error(503, "store_unavailable"),
        };
        let outcome = invite_store::consume(
            self.root.ledger_mut(),
            &verified.id,
            device_id,
            pubkey,
            &token_hash,
            now,
        );
        match outcome {
            Ok(InviteOutcome::Paired) => self.paired(&token, epoch),
            Ok(InviteOutcome::Unknown) => Response::error(403, "invite_unknown"),
            Ok(InviteOutcome::Used) => Response::error(409, "invite_used"),
            Ok(InviteOutcome::Expired) => Response::error(403, "invite_expired"),
            Ok(InviteOutcome::Revoked) => Response::error(403, "invite_revoked"),
            Ok(InviteOutcome::DeviceExists) => Response::error(409, "device_exists"),
            Err(_) => Response::error(503, "store_unavailable"),
        }
    }

    /// 旧路：裸配对码。**弃用但兼容**——已经有人在用 `hub pair` 的机器不该在一夜之间连不上。
    /// 行为与 S4b1 之前逐字相同（失败码也一样），包括"先读 epoch 再吃码"的顺序。
    fn pair_with_code(&mut self, code: &str, device_id: &str, pubkey: &str) -> Response {
        let Some(code_hash) = pair::hash_pair_code(code) else {
            return Response::error(403, "pair_code_unknown");
        };
        let (token, token_hash) = match pair::new_token() {
            Ok(t) => t,
            Err(_) => return Response::error(503, "store_unavailable"),
        };
        let now = self.now_ms();
        let epoch = match rc_ledger::transcript::center_epoch(self.root.transcript()) {
            Ok(epoch) => epoch,
            Err(_) => return Response::error(503, "store_unavailable"),
        };
        let outcome = device::pair(
            self.root.ledger_mut(),
            &code_hash,
            device_id,
            pubkey,
            &token_hash,
            now,
        );
        match outcome {
            Ok(PairOutcome::Paired) => self.paired(&token, epoch),
            Ok(PairOutcome::CodeUnknown) => Response::error(403, "pair_code_unknown"),
            Ok(PairOutcome::CodeUsed) => Response::error(403, "pair_code_used"),
            Ok(PairOutcome::CodeExpired) => Response::error(403, "pair_code_expired"),
            Ok(PairOutcome::DeviceExists) => Response::error(409, "device_exists"),
            Err(_) => Response::error(503, "store_unavailable"),
        }
    }

    /// 配对成功的响应。两条入口共用：字段多一处、少一处就是"节点只认一条路"的来源。
    /// `hub_fp`/`tls_fp` 让节点在拿到 token 的同一步就存下"这台中心是哪一台"。
    fn paired(&self, token: &str, epoch: u64) -> Response {
        Response::json(
            200,
            json!({
                "api_version": API_VERSION,
                "token": token,
                "hub_pub": self.key.public_b64(),
                "hub_fp": rc_net::invite::hub_fingerprint(&self.key.public_b64()),
                "tls_fingerprint": self.tls_fingerprint.clone(),
                "schema": { "min": SCHEMA_MIN, "max": SCHEMA_MAX },
                "endpoints": self.cfg.endpoints.clone(),
                "epoch": epoch,
            }),
        )
    }

    /// `GET /v1/identity`：匿名、**不要求**签名/nonce，fenced 之后也照常服务。
    ///
    /// 它回答的是"应答者是谁"：`hub_pub` 是名字、`hub_fp` 是短名、`tls_fp` 是链路的名字，
    /// 三者被一把私钥签在一起。发现层据此把"某个应答"钉到票里指定的那台中心上；
    /// 没有票的应答者只能被列成"未验证"，这一端点不承担首次信任。
    fn identity(&mut self) -> Response {
        let now = self.now_ms();
        let epoch = match rc_ledger::transcript::center_epoch(self.root.transcript()) {
            Ok(epoch) => epoch,
            Err(_) => return Response::error(503, "store_unavailable"),
        };
        let hub_pub = self.key.public_b64();
        let mut doc = HubIdentity {
            v: IDENTITY_VERSION,
            hub_pub: hub_pub.clone(),
            hub_fp: rc_net::invite::hub_fingerprint(&hub_pub),
            tls_fp: self.tls_fingerprint.clone().unwrap_or_default(),
            epoch,
            endpoints: self.cfg.endpoints.clone(),
            issued_at_ms: now,
            expires_at_ms: now + IDENTITY_TTL_MS,
            sig: String::new(),
        };
        // canonical **不含** `sig` 本身：先以空 sig 建结构体、算 canonical、再回填。
        doc.sig = self.key.sign_b64(rc_net::invite::identity_canonical(&doc).as_bytes());
        match serde_json::to_value(&doc) {
            Ok(value) => Response::json(200, value),
            Err(_) => Response::error(503, "store_unavailable"),
        }
    }

    /// 这一端点**不要求** nonce/签名：它发出的正是 nonce。
    /// 响应由中心签名（节点用 pair 时钉死的 `hub_pub` 验），请求本身不签——
    /// 签一个还没有 nonce 的请求只能得到一发可重放的签名，而它换来的只有另一个 nonce。
    fn challenge(&mut self, req: &Request) -> Response {
        let row = match self.bearer_device(req) {
            Ok(r) => r,
            Err(e) => return Response::error(e.status, e.code),
        };
        let now = self.now_ms();
        let epoch = match rc_ledger::transcript::center_epoch(self.root.transcript()) {
            Ok(epoch) => epoch,
            Err(_) => return Response::error(503, "store_unavailable"),
        };
        let (nonce, expires_at_ms) = match self.nonces.issue(&row.device_id, now) {
            Ok(v) => v,
            Err(rc_net::error::NetError::NonceCapacity) => {
                return Response::error(429, "nonce_capacity")
            }
            Err(_) => return Response::error(503, "store_unavailable"),
        };
        let sig = self.key.sign_b64(
            rc_net::protocol::challenge_canonical(&row.device_id, &nonce, expires_at_ms).as_bytes(),
        );
        Response::json(
            200,
            json!({
                "api_version": API_VERSION,
                "nonce": nonce,
                "expires_at_ms": expires_at_ms,
                "sig": sig,
                "epoch": epoch,
            }),
        )
    }

    /// 按设备能力选快照通道，顺带完成这两条 GET 的五道认证（nonce 只消费一次，调用方不要再签一遍）。
    ///
    /// `has_capability(..., CAPABILITY_REPLICATE)`：
    /// - `true` ⇒ `Backup`（完整记忆，含审查两表）；
    /// - `false` ⇒ `Agent`（记忆通道，副本上审查两表已清空）；
    /// - `Err` ⇒ 503，**不猜**：读不到授权就不知道该发哪一条通道，宁可失败一次。
    ///
    /// 选出来的通道就是这一发的全部答案：`Backup` 目录没有快照时回 404，**绝不回落**
    /// `Agent`——standby 把 agent 副本当成完整记忆应用下去会把本地审查队列清空，
    /// 那是一次静默的数据损失，比这一轮 pull 失败重试糟得多。
    fn snapshot_channel_for(&mut self, req: &Request) -> Result<SnapshotChannel, Response> {
        let device = self
            .authenticate(req)
            .map_err(|e| Response::error(e.status, e.code))?;
        match rc_ledger::device::has_capability(
            self.root.ledger(),
            &device.device_id,
            rc_ledger::device::CAPABILITY_REPLICATE,
        ) {
            Ok(true) => Ok(SnapshotChannel::Backup),
            Ok(false) => Ok(SnapshotChannel::Agent),
            Err(_) => Err(Response::error(503, "store_unavailable")),
        }
    }

    /// `GET /v1/state`：只报"当前是哪一份"，不发字节。
    /// 认证与 ingest 同五道；GET 的签名体是空字节串。
    fn state(&mut self, req: &Request) -> Response {
        let channel = match self.snapshot_channel_for(req) {
            Ok(channel) => channel,
            Err(response) => return response,
        };
        let dir = crate::snapshot::channel_dir(&self.data_dir, channel);
        match load_state(&dir) {
            Ok(Some(current)) => Response::json(
                200,
                json!({
                    "api_version": API_VERSION,
                    "epoch": current.manifest.epoch,
                    "seq": current.manifest.seq,
                    "sha256": current.manifest.sha256,
                    "bytes": current.manifest.bytes,
                    "schema_version": current.manifest.schema_version,
                    "norm_version": current.manifest.norm_version,
                    "created_at_ms": current.manifest.created_at_ms,
                    "signature": current.manifest.signature,
                    "center_commit_seq": current.center_commit_seq,
                    "endpoint": "/v1/snapshot/memory",
                    "max_chunk_bytes": MAX_SNAPSHOT_CHUNK,
                }),
            ),
            Ok(None) => Response::error(404, "no_snapshot"),
            Err(_) => Response::error(503, "store_unavailable"),
        }
    }

    /// `GET /v1/snapshot/memory`：发字节。
    /// - 整份 ≤ 10 MiB 且没带 `Range` ⇒ 200 全量；
    /// - 带了 `Range` ⇒ 206 + `Content-Range`；
    /// - 整份 > 10 MiB 且没带 `Range` ⇒ 400 `range_required`（不许把上百 MB 塞进一个 `Vec`）；
    /// - 分片超过 10 MiB ⇒ 416 `range_too_large`；起点越界 ⇒ 416 `range_not_satisfiable`。
    ///
    /// 本路由**不重算整份文件的 sha**：清单里的 sha 是发布时算的，文件改名之后不再改写；
    /// 每次下行验一遍就是拿 CPU 换一个节点本来也会做的检查。
    /// 这里只验"文件还在、大小与清单一致"——对不上就是 `snapshot_missing`。
    fn snapshot(&mut self, req: &Request) -> Response {
        let channel = match self.snapshot_channel_for(req) {
            Ok(channel) => channel,
            Err(response) => return response,
        };
        let dir = crate::snapshot::channel_dir(&self.data_dir, channel);
        let current = match load_state(&dir) {
            Ok(Some(s)) => s,
            Ok(None) => return Response::error(404, "no_snapshot"),
            Err(_) => return Response::error(503, "store_unavailable"),
        };
        let path = match rc_net::snapshot::resolved_file_name(&current) {
            Some(name) => dir.join(name),
            None => return Response::error(404, "snapshot_missing"),
        };
        let total = match std::fs::metadata(&path) {
            Ok(m) if m.is_file() && m.len() == current.manifest.bytes && m.len() > 0 => m.len(),
            _ => return Response::error(404, "snapshot_missing"),
        };
        let cap = MAX_SNAPSHOT_CHUNK as u64;
        let (start, end, partial) = match req.header("range") {
            None if total > cap => return Response::error(400, "range_required"),
            None => (0, total - 1, false),
            Some(value) => match parse_range(value, total, cap) {
                Ok((a, b)) => (a, b, true),
                Err(RangeError::NotSatisfiable) => {
                    return ranged_error(416, "range_not_satisfiable", total)
                }
                Err(RangeError::TooLarge) => return ranged_error(416, "range_too_large", total),
                Err(RangeError::Malformed) => return Response::error(400, "bad_request"),
            },
        };
        let body = match read_slice(&path, start, end) {
            Ok(b) => b,
            Err(_) => return Response::error(503, "store_unavailable"),
        };
        let chunk_sha = rc_net::protocol::sha256_hex(&body);
        let headers = snapshot_headers(&current.manifest, &chunk_sha, total, partial, start, end);
        let mut resp = Response::bytes(
            if partial { 206 } else { 200 },
            "application/octet-stream",
            body,
        );
        resp.headers = headers;
        resp
    }

    /// `GET /v1/replicate?since=N&limit=M`：按 `commit_seq` 增量取提交记录。
    /// 记录含正文，因此与 ingest 同一套五道认证；GET 的签名体是空字节串。
    fn replicate(&mut self, req: &Request) -> Response {
        let device = match self.authenticate(req) {
            Ok(device) => device,
            Err(e) => return Response::error(e.status, e.code),
        };
        match rc_ledger::device::has_capability(
            self.root.ledger(),
            &device.device_id,
            rc_ledger::device::CAPABILITY_REPLICATE,
        ) {
            Ok(true) => {}
            Ok(false) => return Response::error(403, "replicate_denied"),
            Err(_) => return Response::error(503, "store_unavailable"),
        }
        let since = match req.query_get("since") {
            None => 0,
            Some(value) => match value.parse::<u64>() {
                Ok(value) => value,
                Err(_) => return Response::error(400, "bad_request"),
            },
        };
        let limit = match req.query_get("limit") {
            None => 100usize,
            Some(value) => match value.parse::<usize>() {
                Ok(value) if value > 0 => value.min(1000),
                _ => return Response::error(400, "bad_request"),
            },
        };
        let status = match crate::replicate::status(self.root.transcript()) {
            Ok(status) => status,
            Err(_) => return Response::error(503, "store_unavailable"),
        };
        let records = match crate::replicate::records_since(self.root.transcript(), since, limit) {
            Ok(records) => records,
            Err(_) => return Response::error(503, "store_unavailable"),
        };
        let next_since = records.last().map(|record| record.commit_seq).unwrap_or(since);
        Response::json(
            200,
            json!({
                "api_version": API_VERSION,
                "epoch": status.epoch,
                "last_commit_seq": status.last_commit_seq,
                "next_since": next_since,
                "records": records,
            }),
        )
    }

    /// `GET /v1/replicate/ledger`：授权/审计账本的 commit_seq 快照。含 token 哈希与公钥，
    /// 与 transcript 复制同一套 capability，不发 raw token。
    fn replicate_ledger(&mut self, req: &Request) -> Response {
        let device = match self.authenticate(req) {
            Ok(device) => device,
            Err(e) => return Response::error(e.status, e.code),
        };
        match rc_ledger::device::has_capability(
            self.root.ledger(),
            &device.device_id,
            rc_ledger::device::CAPABILITY_REPLICATE,
        ) {
            Ok(true) => {}
            Ok(false) => return Response::error(403, "replicate_denied"),
            Err(_) => return Response::error(503, "store_unavailable"),
        }
        match crate::replicate::export_ledger(&self.root) {
            Ok(snapshot) => match serde_json::to_value(snapshot) {
                Ok(value) => Response::json(200, value),
                Err(_) => Response::error(503, "store_unavailable"),
            },
            Err(_) => Response::error(503, "store_unavailable"),
        }
    }

    /// `GET /v1/replicate/status`：备份侧用来算 lag，不发正文。
    fn replicate_status(&mut self, req: &Request) -> Response {
        let device = match self.authenticate(req) {
            Ok(device) => device,
            Err(e) => return Response::error(e.status, e.code),
        };
        match rc_ledger::device::has_capability(
            self.root.ledger(),
            &device.device_id,
            rc_ledger::device::CAPABILITY_REPLICATE,
        ) {
            Ok(true) => {}
            Ok(false) => return Response::error(403, "replicate_denied"),
            Err(_) => return Response::error(503, "store_unavailable"),
        }
        let status = match crate::replicate::status(self.root.transcript()) {
            Ok(status) => status,
            Err(_) => return Response::error(503, "store_unavailable"),
        };
        // 回执明细只给操作面（复制能力）：普通设备只需要 `/acked` 的那个合并水位。
        let acks = match crate::replicate::ack_status(
            self.root.transcript(),
            self.root.ledger(),
            self.now_ms(),
        ) {
            Ok(acks) => acks,
            Err(_) => return Response::error(503, "store_unavailable"),
        };
        Response::json(
            200,
            json!({
                "api_version": API_VERSION,
                "epoch": status.epoch,
                "last_commit_seq": status.last_commit_seq,
                "replicated_seq": acks.replicated_seq,
                "acks": acks.acks,
            }),
        )
    }

    /// `POST /v1/replicate/ack`：备份上报已落地水位。写面，fenced 后一律拒绝。
    fn replicate_ack(&mut self, req: &Request) -> Response {
        if let Some(response) = self.ensure_not_fenced() {
            return response;
        }
        let device = match self.authenticate(req) {
            Ok(device) => device,
            Err(e) => return Response::error(e.status, e.code),
        };
        match rc_ledger::device::has_capability(
            self.root.ledger(),
            &device.device_id,
            rc_ledger::device::CAPABILITY_REPLICATE,
        ) {
            Ok(true) => {}
            Ok(false) => return Response::error(403, "replicate_denied"),
            Err(_) => return Response::error(503, "store_unavailable"),
        }
        let applied_seq = match serde_json::from_slice::<Value>(&req.body)
            .ok()
            .and_then(|doc| doc.get("applied_seq").and_then(Value::as_u64))
        {
            Some(seq) => seq,
            None => return Response::error(400, "bad_request"),
        };
        match crate::replicate::record_ack(
            self.root.transcript(),
            &device.device_id,
            applied_seq,
            self.now_ms(),
        ) {
            Ok(()) => {}
            Err(crate::replicate::AckError::BeyondCenter { .. }) => {
                return Response::error(409, "ack_beyond_center")
            }
            Err(crate::replicate::AckError::Store(_)) => {
                return Response::error(503, "store_unavailable")
            }
        }
        match crate::replicate::ack_status(
            self.root.transcript(),
            self.root.ledger(),
            self.now_ms(),
        ) {
            Ok(acks) => Response::json(
                200,
                json!({
                    "api_version": API_VERSION,
                    "epoch": acks.epoch,
                    "applied_seq": applied_seq,
                    "replicated_seq": acks.replicated_seq,
                }),
            ),
            Err(_) => Response::error(503, "store_unavailable"),
        }
    }

    /// `GET /v1/replicate/acked`：任何已配对设备都能读的合并水位。
    /// 设备据此清理本地原始副本；只读，fenced 之后也照常服务。
    fn replicate_acked(&mut self, req: &Request) -> Response {
        if let Err(e) = self.authenticate(req) {
            return Response::error(e.status, e.code);
        }
        match crate::replicate::ack_status(
            self.root.transcript(),
            self.root.ledger(),
            self.now_ms(),
        ) {
            Ok(acks) => Response::json(
                200,
                json!({
                    "api_version": API_VERSION,
                    "epoch": acks.epoch,
                    "last_commit_seq": acks.last_commit_seq,
                    "replicated_seq": acks.replicated_seq,
                }),
            ),
            Err(_) => Response::error(503, "store_unavailable"),
        }
    }

    /// 上行的一批。顺序是判定的一部分：廉价的两道认证 → 解压 → 签名盖在**解压后**的字节上
    /// （`authenticate_over` 内部完成 nonce 的单次消费）→ 逐行拆 → 一次 normalize → 落库。
    ///
    /// 解压排在身份检查之后：gzip 能把几十字节放大到 `MAX_BODY_BYTES`，让未认证的请求
    /// 先触发解压就是把 CPU 白送出去。代价是设备行查两次（都是唯一索引），可以忽略。
    fn ingest(&mut self, req: &Request) -> Response {
        if let Some(response) = self.ensure_not_fenced() {
            return response;
        }
        if let Err(e) = self.bearer_device(req) {
            return Response::error(e.status, e.code);
        }
        let decoded = match rc_net::ingest::decode_body(req.header("content-encoding"), &req.body) {
            Ok(b) => b,
            Err(rc_net::ingest::DecodeError::TooLarge) => {
                return Response::error(413, "body_too_large")
            }
            Err(_) => return Response::error(400, "bad_request"),
        };
        let row = match self.authenticate_over(req, &decoded) {
            Ok(r) => r,
            Err(e) => return Response::error(e.status, e.code),
        };
        if decoded.is_empty() {
            return Response::error(400, "bad_request");
        }
        let now = self.now_ms();
        let batch = rc_net::ingest::assemble(&decoded, &row.device_id);
        if batch.lines_in == 0 {
            return Response::error(400, "bad_request");
        }
        let lines_in = batch.lines_in;
        let lines_accepted = batch.lines_accepted;
        let line_rejected_turns = batch.rejected_turns() as i64;
        let signature_turns = batch.signature_rejected_turns() as i64;
        let devices = batch.devices.clone();
        let session_line = batch.line_of_session.clone();
        let mut rejects = batch.rejects;
        // 每行都是一整份 ExchangeFile；把通过的会话并成一份再 normalize 一次。
        // 逐行 normalize 会把 T1 的跨会话判据（uid 去重、同会话 seq）拆成碎片。
        let rep = rc_contract::normalize(batch.file, now);
        for r in &rep.rejects {
            rejects.push(rc_net::ingest::LineReject {
                line: session_line.get(r.loc.session_index).copied().unwrap_or(1),
                turn: r.loc.turn_index,
                code: r.code.slug(),
                detail: r.detail.clone(),
                turns: 0,
            });
        }
        rejects.sort_by_key(|r| (r.line, r.turn.unwrap_or(usize::MAX)));
        if crate::ingest::bump_line_rejects(
            &mut self.root, &row.device_id, line_rejected_turns, signature_turns, now,
        )
        .is_err()
        {
            return Response::error(503, "store_unavailable");
        }
        let sha = rc_net::protocol::sha256_hex(&decoded);
        let committed = match crate::ingest::commit(
            &mut self.root,
            crate::ingest::CommitInput {
                started_at_ms: now,
                now_ms: now,
                device_id: crate::ingest::device_subject(&row.device_id),
                exchange_sha256: &sha,
                devices: &devices,
                rep: &rep,
                line_rejected_turns,
            },
        ) {
            Ok(c) => c,
            Err(_) => return Response::error(503, "store_unavailable"),
        };
        // D31-b：与 CLI `hub import` 共用同一遍「我 / 我们」→ 主人锚 mention。必须落在
        // `commit` 之后（claim 与 sessions 归属行这时才是真值）。失败**不让 ingest 失败**：
        // 转录/计数/import_log 已经写完，回 503 只会让节点重发并重复计数；mention 是派生行，
        // 重导即可补。
        let _ = crate::ingest::record_principal_mentions_after_import(&self.root, &rep, now);
        let omitted = rejects.len().saturating_sub(rc_net::ingest::MAX_REPORTED_REJECTS);
        rejects.truncate(rc_net::ingest::MAX_REPORTED_REJECTS);
        let turns_rejected = rep.rejected_turn_count() + line_rejected_turns as usize;
        let mut doc = json!({
            "api_version": API_VERSION,
            "lines_in": lines_in,
            "lines_accepted": lines_accepted,
            "lines_rejected": lines_in - lines_accepted,
            "turns_accepted": committed.inserted,
            "skipped_duplicate": committed.skipped_duplicate,
            "tombstoned_turns": committed.tombstoned,
            "commit_seq": committed.commit_seq,
            "epoch": committed.epoch,
            "turns_rejected": turns_rejected,
            "turns_rejected_signature": signature_turns,
            // 入库正文值级脱敏的计数（零正文）。HTTP 这条路没有映射文件这一格——
            // 没有显式路径就只计数、不留明文副本。
            "redacted_spans": rep.redacted_spans,
            "batch_reject": rep.batch_reject.map(rc_contract::RejectCode::slug).unwrap_or_default(),
            "rejects": rejects,
            "rejects_omitted": omitted,
        });
        if rep.batch_reject.is_some() {
            // 整批拒收 = 一行都没落库。200 会让节点把水位推过去，那批就永久丢了。
            doc["error"] = json!("ingest_failed");
            return Response::json(422, doc);
        }
        Response::json(200, doc)
    }

    /// `POST /v1/rows`：节点离线自蒸产生的新行。签名盖**原始 body**，不解 gzip。
    /// 记忆行与水位标记两段式：吸收成功而标记失败 ⇒ 503，节点不 pull、重试同一载荷。
    ///
    /// S3c-4：`fact` 行不再直写，而是逐行走闸 → 入队 → 同步窄审 → apply；结构行照旧直写。
    /// 座位先在这里取（`backend_factory` 造一次），再借 `root` 交给 `absorb`——一行
    /// `Option<&dyn LlmBackend>` 就是"这批能不能审"的全部输入。
    fn rows(&mut self, req: &Request) -> Response {
        if let Some(response) = self.ensure_not_fenced() {
            return response;
        }
        if let Err(e) = self.authenticate(req) {
            return Response::error(e.status, e.code);
        }
        let now = self.now_ms();
        let backend = self.backend_factory.as_ref().map(|factory| factory());
        let backend = match backend.as_ref() {
            // 没有座位、或这一次造不出来：都是"本批审不了"，由 absorb 在 fact 入队前 503。
            Some(Ok(backend)) => Some(&**backend),
            Some(Err(())) | None => None,
        };
        match crate::rows::absorb(&mut self.root, &req.body, now, backend) {
            Ok(a) => {
                // 节点回并的硬序是「先回传 → 中心确认 → 才下发新快照」。吸收成功后必须
                // 立刻发布一代新快照：否则节点 pull 到的还是旧代，丢层后本地看不到刚交上来的行。
                // 发布失败 ⇒ 503；节点不 pull、带同一载荷重试（行吸收与发布都可重放）。
                if crate::snapshot::publish_snapshot(&mut self.root, &self.key, &self.data_dir, now)
                    .is_err()
                {
                    return Response::error(503, "store_unavailable");
                }
                Response::json(
                    200,
                    json!({
                        "api_version": API_VERSION,
                        "rows_in": a.rows_in,
                        "absorbed": a.absorbed,
                        "skipped_duplicate": a.skipped_duplicate,
                        "turns_marked": a.turns_marked,
                        "rows_reviewed": a.rows_reviewed,
                        "rows_absorbed": a.rows_absorbed,
                        "rows_rejected": a.rows_rejected,
                        "rows_quarantined": a.rows_quarantined,
                        // S3c-fix2：撞 id（窄审 pass 但行已在 `fact` 里）也与 applied 分开亮出来，
                        // 否则节点只能从 `absorbed` 的差额里猜。
                        "rows_duplicate": a.rows_duplicate,
                        "rows_direct": a.rows_direct,
                    }),
                )
            }
            Err(crate::rows::AbsorbError::BadRequest) => Response::error(400, "bad_request"),
            Err(crate::rows::AbsorbError::BadRows) => Response::error(400, "bad_rows"),
            Err(crate::rows::AbsorbError::ReviewUnavailable) => {
                Response::error(503, "review_unavailable")
            }
            Err(crate::rows::AbsorbError::Store) => Response::error(503, "store_unavailable"),
        }
    }

    /// W39 刀 C：四个转录只读路由的装配。鉴权复用 [`HubState::authenticate`] 的
    /// 五道（token/未撤销/device 头/签名/nonce），**不加** capability（与 `/v1/ingest`
    /// 同档）；可见性判定与 SQL 全在 `rc-search` 与 `transcript_read`，这里只递句柄与时钟。
    fn transcript_sessions(&mut self, req: &Request) -> Response {
        let device = match self.authenticate(req) {
            Ok(device) => device,
            Err(e) => return Response::error(e.status, e.code),
        };
        let now = self.now_ms();
        crate::transcript_read::sessions(
            self.root.transcript(),
            self.root.ledger(),
            &device,
            req,
            now,
        )
    }

    fn transcript_session(&mut self, req: &Request) -> Response {
        let device = match self.authenticate(req) {
            Ok(device) => device,
            Err(e) => return Response::error(e.status, e.code),
        };
        let now = self.now_ms();
        crate::transcript_read::session(
            self.root.transcript(),
            self.root.ledger(),
            &device,
            req,
            now,
        )
    }

    fn transcript_search(&mut self, req: &Request) -> Response {
        let device = match self.authenticate(req) {
            Ok(device) => device,
            Err(e) => return Response::error(e.status, e.code),
        };
        let now = self.now_ms();
        crate::transcript_read::search(
            self.root.transcript(),
            self.root.ledger(),
            &device,
            req,
            now,
        )
    }

    fn transcript_turn(&mut self, req: &Request) -> Response {
        let device = match self.authenticate(req) {
            Ok(device) => device,
            Err(e) => return Response::error(e.status, e.code),
        };
        let now = self.now_ms();
        crate::transcript_read::turn(
            self.root.transcript(),
            self.root.ledger(),
            &device,
            req,
            now,
        )
    }
}

impl Handler for HubState {
    fn handle(&mut self, req: &Request) -> Response {
        let mut response = match (req.method.as_str(), req.path.as_str()) {
            ("POST", "/v1/pair") => self.pair(req),
            ("GET", "/v1/identity") => self.identity(),
            ("POST", "/v1/challenge") => self.challenge(req),
            ("POST", "/v1/ingest") => self.ingest(req),
            ("POST", "/v1/rows") => self.rows(req),
            ("GET", "/v1/state") => self.state(req),
            ("GET", "/v1/snapshot/memory") => self.snapshot(req),
            ("GET", "/v1/replicate") => self.replicate(req),
            ("GET", "/v1/replicate/status") => self.replicate_status(req),
            ("GET", "/v1/replicate/ledger") => self.replicate_ledger(req),
            ("POST", "/v1/replicate/ack") => self.replicate_ack(req),
            ("GET", "/v1/replicate/acked") => self.replicate_acked(req),
            ("GET", "/v1/transcript/sessions") => self.transcript_sessions(req),
            ("GET", "/v1/transcript/session") => self.transcript_session(req),
            ("GET", "/v1/transcript/search") => self.transcript_search(req),
            ("GET", "/v1/transcript/turn") => self.transcript_turn(req),
            (_, "/v1/pair" | "/v1/identity" | "/v1/challenge" | "/v1/ingest" | "/v1/rows" | "/v1/state" | "/v1/snapshot/memory" | "/v1/replicate" | "/v1/replicate/status" | "/v1/replicate/ledger" | "/v1/replicate/ack" | "/v1/replicate/acked" | "/v1/transcript/sessions" | "/v1/transcript/session" | "/v1/transcript/search" | "/v1/transcript/turn") => {
                Response::error(405, "unsupported_method")
            }
            _ => Response::error(404, "not_found"),
        };
        if let Ok(epoch) = rc_ledger::transcript::center_epoch(self.root.transcript()) {
            response
                .headers
                .push(("X-Recollect-Epoch".to_string(), epoch.to_string()));
        }
        response
    }
}

/// 416 的两种错误共用同一个信封：`Content-Range: bytes */total` 是 RFC 要求的
/// "这个文件到底有多长"，节点据此把分片请求重新算一遍，而不是盲目重试同一个区间。
fn ranged_error(status: u16, code: &'static str, total: u64) -> Response {
    let mut resp = Response::error(status, code);
    resp.headers.push(("Accept-Ranges".into(), "bytes".into()));
    resp.headers.push(("Content-Range".into(), format!("bytes */{total}")));
    resp
}

fn read_slice(path: &Path, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let len = (end - start + 1) as usize;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

/// 头部是**清单的投影**：节点拿到这一片之后可以立刻验落盘字节
/// （`X-Recollect-Chunk-Sha256`）与清单（`X-Recollect-Manifest-Sig`），
/// 不必先发第二发请求去问"这一份是不是你说的那一份"。
fn snapshot_headers(
    manifest: &Manifest,
    chunk_sha: &str,
    total: u64,
    partial: bool,
    start: u64,
    end: u64,
) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Accept-Ranges".to_string(), "bytes".to_string()),
        ("X-Recollect-Epoch".to_string(), manifest.epoch.to_string()),
        ("X-Recollect-Seq".to_string(), manifest.seq.to_string()),
        ("X-Recollect-Sha256".to_string(), manifest.sha256.clone()),
        ("X-Recollect-Chunk-Sha256".to_string(), chunk_sha.to_string()),
        ("X-Recollect-Bytes".to_string(), manifest.bytes.to_string()),
        (
            "X-Recollect-Schema-Version".to_string(),
            manifest.schema_version.to_string(),
        ),
        (
            "X-Recollect-Norm-Version".to_string(),
            manifest.norm_version.to_string(),
        ),
        (
            "X-Recollect-Manifest-Sig".to_string(),
            manifest.signature.clone(),
        ),
    ];
    if partial {
        headers.push((
            "Content-Range".to_string(),
            format!("bytes {start}-{end}/{total}"),
        ));
    }
    headers
}
