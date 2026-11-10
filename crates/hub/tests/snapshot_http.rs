use hub::hub_cfg::HubConfig;
use hub::serve::HubState;
use hub::snapshot::{channel_dir, publish_channel, SnapshotChannel};
use rc_net::http::{Handler, Request, Response};
use rc_net::key::{DeviceKey, HubKey};
use rc_net::protocol::MAX_SNAPSHOT_CHUNK;
use rc_net::snapshot::{Manifest, SnapshotState};
use rc_store::Root;
use serde_json::{json, Value};

const NOW: i64 = 1_000;

struct Fixture {
    _dir: tempfile::TempDir,
    data: std::path::PathBuf,
    state: HubState,
    dev: DeviceKey,
    token: String,
    manifest: Option<Manifest>,
}

fn challenge(state: &mut HubState, dev: &DeviceKey, token: &str) -> Value {
    let resp = state.handle(&Request::new(
        "POST",
        "/v1/challenge",
        vec![
            ("Authorization".into(), format!("Bearer {token}")),
            ("X-Recollect-Device".into(), dev.device_id()),
        ],
        Vec::new(),
    ));
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    serde_json::from_slice(&resp.body).unwrap()
}

fn signed_headers(
    dev: &DeviceKey,
    token: &str,
    nonce: &str,
    method: &str,
    target: &str,
    body: &[u8],
) -> Vec<(String, String)> {
    let canonical = rc_net::protocol::request_canonical(
        method,
        target,
        &rc_net::protocol::sha256_hex(body),
        &dev.device_id(),
        nonce,
    );
    vec![
        ("Authorization".into(), format!("Bearer {token}")),
        ("X-Recollect-Device".into(), dev.device_id()),
        ("X-Recollect-Nonce".into(), nonce.into()),
        ("X-Recollect-Sig".into(), dev.sign_b64(canonical.as_bytes())),
    ]
}

fn fixture(with_snapshot: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let mut root = Root::open(&data).unwrap();
    let key = HubKey::load_or_create(&data.join("hub.key")).unwrap();
    let manifest = if with_snapshot {
        Some(
            hub::snapshot::publish_snapshot(&mut root, &key, &data, NOW)
                .unwrap()
                .manifest,
        )
    } else {
        None
    };
    let dev = DeviceKey::generate().unwrap();
    let (code, hash) = rc_net::pair::new_pair_code().unwrap();
    rc_ledger::device::issue_pair_code(root.ledger_mut(), &hash, NOW, NOW + 300_000).unwrap();
    let cfg = HubConfig {
        bind: "127.0.0.1:7420".into(),
        endpoints: vec!["http://127.0.0.1:7420".into()],
        max_snapshot_bps: 4096,
    };
    let mut state = HubState::new(root, key, cfg, &data, Box::new(|| NOW));
    let body = serde_json::to_vec(&json!({
        "code": code,
        "device_id": dev.device_id(),
        "pubkey": dev.public_b64(),
    }))
    .unwrap();
    let resp = state.handle(&Request::new(
        "POST",
        "/v1/pair",
        vec![("Content-Type".into(), "application/json".into())],
        body,
    ));
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    Fixture {
        _dir: dir,
        data,
        state,
        dev,
        token: doc["token"].as_str().unwrap().to_string(),
        manifest,
    }
}

fn header<'a>(resp: &'a Response, name: &str) -> &'a str {
    resp.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .unwrap_or_else(|| panic!("missing header {name}"))
}

impl Fixture {
    fn get(&mut self, target: &str, extra: &[(&str, &str)]) -> Response {
        let doc = challenge(&mut self.state, &self.dev, &self.token);
        let nonce = doc["nonce"].as_str().unwrap().to_string();
        let mut headers = signed_headers(&self.dev, &self.token, &nonce, "GET", target, &[]);
        for (k, v) in extra {
            headers.push((k.to_string(), v.to_string()));
        }
        self.state
            .handle(&Request::new("GET", target, headers, Vec::new()))
    }
}

/// 直接摆一份状态与文件，绕过发布：大文件用例不可能真造 10 MiB 的 SQLite。
/// 服务面的确不验签（签是节点的活），所以这里的假签名是刻意的。
fn install_fake(f: &Fixture, bytes: &[u8]) {
    let dir = hub::snapshot::snapshots_dir(&f.data);
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = Manifest {
        epoch: 9,
        seq: 4,
        sha256: rc_net::protocol::sha256_hex(bytes),
        schema_version: 6,
        norm_version: 1,
        bytes: bytes.len() as u64,
        created_at_ms: NOW,
        signature: "serving-does-not-verify".into(),
    };
    let state = SnapshotState {
        manifest,
        digest: "fake".into(),
        file: rc_net::snapshot::snapshot_name(9, 4),
        center_commit_seq: 0,
    };
    std::fs::write(dir.join(&state.file), bytes).unwrap();
    rc_net::snapshot::save_state(&dir, &state).unwrap();
}

#[test]
fn state_requires_a_signed_request() {
    let mut f = fixture(false);
    let anonymous = f
        .state
        .handle(&Request::new("GET", "/v1/state", Vec::new(), Vec::new()));
    assert_eq!(anonymous.status, 401);
    assert_eq!(
        serde_json::from_slice::<Value>(&anonymous.body).unwrap()["error"],
        "invalid_token"
    );

    let doc = challenge(&mut f.state, &f.dev, &f.token);
    let nonce = doc["nonce"].as_str().unwrap();
    let mut headers = signed_headers(&f.dev, &f.token, nonce, "GET", "/v1/state", &[]);
    headers
        .iter_mut()
        .find(|(k, _)| k == "X-Recollect-Sig")
        .unwrap()
        .1 = "A".repeat(43);
    let forged = f
        .state
        .handle(&Request::new("GET", "/v1/state", headers, Vec::new()));
    assert_eq!(forged.status, 403);
    assert_eq!(
        serde_json::from_slice::<Value>(&forged.body).unwrap()["error"],
        "bad_signature"
    );
}

#[test]
fn state_without_a_snapshot_is_404_no_snapshot() {
    let mut f = fixture(false);
    assert!(f.manifest.is_none());
    let resp = f.get("/v1/state", &[]);
    assert_eq!(resp.status, 404);
    assert_eq!(
        serde_json::from_slice::<Value>(&resp.body).unwrap()["error"],
        "no_snapshot"
    );
}

#[test]
fn state_reports_the_published_manifest() {
    let mut f = fixture(true);
    let expected = f.manifest.clone().unwrap();
    let resp = f.get("/v1/state", &[]);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(doc["api_version"], 1);
    assert_eq!(doc["epoch"].as_u64().unwrap(), expected.epoch);
    assert_eq!(doc["seq"].as_u64().unwrap(), expected.seq);
    assert_eq!(doc["sha256"], expected.sha256);
    assert_eq!(doc["bytes"].as_u64().unwrap(), expected.bytes);
    assert_eq!(
        doc["schema_version"].as_i64().unwrap() as i32,
        expected.schema_version
    );
    assert_eq!(
        doc["norm_version"].as_u64().unwrap() as u32,
        expected.norm_version
    );
    assert_eq!(doc["signature"], expected.signature);
    assert_eq!(doc["endpoint"], "/v1/snapshot/memory");
    assert_eq!(
        doc["max_chunk_bytes"].as_u64().unwrap(),
        MAX_SNAPSHOT_CHUNK as u64
    );
}

#[test]
fn snapshot_requires_a_signed_request() {
    let mut f = fixture(true);
    let anonymous = f.state.handle(&Request::new(
        "GET",
        "/v1/snapshot/memory",
        Vec::new(),
        Vec::new(),
    ));
    assert_eq!(anonymous.status, 401);
    assert!(
        anonymous.body.len() < 64,
        "拒绝响应里带上了文件字节：{} 字节",
        anonymous.body.len()
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&anonymous.body).unwrap()["error"],
        "invalid_token"
    );

    let doc = challenge(&mut f.state, &f.dev, &f.token);
    let nonce = doc["nonce"].as_str().unwrap();
    let mut headers = signed_headers(&f.dev, &f.token, nonce, "GET", "/v1/snapshot/memory", &[]);
    headers
        .iter_mut()
        .find(|(k, _)| k == "X-Recollect-Sig")
        .unwrap()
        .1 = "A".repeat(43);
    let forged = f.state.handle(&Request::new(
        "GET",
        "/v1/snapshot/memory",
        headers,
        Vec::new(),
    ));
    assert_eq!(forged.status, 403);
    assert_eq!(
        serde_json::from_slice::<Value>(&forged.body).unwrap()["error"],
        "bad_signature"
    );
}

#[test]
fn full_snapshot_is_served_with_manifest_headers() {
    let mut f = fixture(true);
    let expected = f.manifest.clone().unwrap();
    let resp = f.get("/v1/snapshot/memory", &[]);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    assert_eq!(resp.content_type, "application/octet-stream");
    assert_eq!(resp.body.len() as u64, expected.bytes);
    assert_eq!(rc_net::protocol::sha256_hex(&resp.body), expected.sha256);
    assert_eq!(header(&resp, "Accept-Ranges"), "bytes");
    assert_eq!(header(&resp, "X-Recollect-Epoch"), expected.epoch.to_string());
    assert_eq!(header(&resp, "X-Recollect-Seq"), expected.seq.to_string());
    assert_eq!(header(&resp, "X-Recollect-Sha256"), expected.sha256);
    assert_eq!(header(&resp, "X-Recollect-Chunk-Sha256"), expected.sha256);
    assert_eq!(header(&resp, "X-Recollect-Bytes"), expected.bytes.to_string());
    assert_eq!(header(&resp, "X-Recollect-Schema-Version"), "6");
    assert_eq!(header(&resp, "X-Recollect-Norm-Version"), "1");
    assert_eq!(header(&resp, "X-Recollect-Manifest-Sig"), expected.signature);
    assert!(resp.headers.iter().all(|(k, _)| k != "Content-Range"));
}

#[test]
fn range_returns_206_with_content_range_and_chunk_sha() {
    let mut f = fixture(true);
    let expected = f.manifest.clone().unwrap();
    let resp = f.get("/v1/snapshot/memory", &[("Range", "bytes=0-15")]);
    assert_eq!(resp.status, 206, "{}", String::from_utf8_lossy(&resp.body));

    let dir = hub::snapshot::snapshots_dir(&f.data);
    let full =
        std::fs::read(dir.join(rc_net::snapshot::snapshot_name(expected.epoch, expected.seq))).unwrap();
    assert_eq!(resp.body.len(), 16);
    assert_eq!(&resp.body[..], &full[..16]);
    assert_eq!(
        header(&resp, "Content-Range"),
        format!("bytes 0-15/{}", full.len())
    );
    assert_eq!(
        header(&resp, "X-Recollect-Chunk-Sha256"),
        rc_net::protocol::sha256_hex(&resp.body)
    );
    assert_eq!(header(&resp, "X-Recollect-Sha256"), expected.sha256);
    assert_ne!(rc_net::protocol::sha256_hex(&resp.body), expected.sha256);
}

#[test]
fn range_past_the_end_is_416_with_the_total() {
    let mut f = fixture(true);
    let total = f.manifest.as_ref().unwrap().bytes;
    let range = format!("bytes={total}-");
    let resp = f.get("/v1/snapshot/memory", &[("Range", range.as_str())]);
    assert_eq!(resp.status, 416);
    assert_eq!(
        serde_json::from_slice::<Value>(&resp.body).unwrap()["error"],
        "range_not_satisfiable"
    );
    assert_eq!(header(&resp, "Content-Range"), format!("bytes */{total}"));
}

#[test]
fn oversized_range_is_416_range_too_large() {
    let mut f = fixture(true);
    let bytes = vec![0u8; MAX_SNAPSHOT_CHUNK + 1];
    install_fake(&f, &bytes);
    let range = format!("bytes=0-{MAX_SNAPSHOT_CHUNK}");
    let resp = f.get("/v1/snapshot/memory", &[("Range", range.as_str())]);
    assert_eq!(resp.status, 416);
    assert_eq!(
        serde_json::from_slice::<Value>(&resp.body).unwrap()["error"],
        "range_too_large"
    );
    assert_eq!(
        header(&resp, "Content-Range"),
        format!("bytes */{}", bytes.len())
    );
}

#[test]
fn large_snapshot_without_a_range_is_400_range_required() {
    let mut f = fixture(true);
    install_fake(&f, &vec![0u8; MAX_SNAPSHOT_CHUNK + 1]);
    let resp = f.get("/v1/snapshot/memory", &[]);
    assert_eq!(resp.status, 400);
    assert_eq!(
        serde_json::from_slice::<Value>(&resp.body).unwrap()["error"],
        "range_required"
    );
}

#[test]
fn missing_snapshot_file_is_404_snapshot_missing() {
    let mut f = fixture(true);
    install_fake(&f, b"fake-snapshot-bytes");
    let dir = hub::snapshot::snapshots_dir(&f.data);
    std::fs::remove_file(dir.join(rc_net::snapshot::snapshot_name(9, 4))).unwrap();
    let resp = f.get("/v1/snapshot/memory", &[]);
    assert_eq!(resp.status, 404);
    assert_eq!(
        serde_json::from_slice::<Value>(&resp.body).unwrap()["error"],
        "snapshot_missing"
    );
}

#[test]
fn tampered_current_file_is_404_snapshot_missing() {
    let mut f = fixture(true);
    install_fake(&f, b"fake-snapshot-bytes");
    let dir = hub::snapshot::snapshots_dir(&f.data);
    let current = dir.join(rc_net::snapshot::CURRENT_STATE);
    let mut doc: Value = serde_json::from_slice(&std::fs::read(&current).unwrap()).unwrap();
    let key_len = std::fs::metadata(f.data.join("hub.key")).unwrap().len();
    doc["file"] = Value::String("../../hub.key".into());
    doc["manifest"]["bytes"] = Value::from(key_len);
    std::fs::write(&current, serde_json::to_vec(&doc).unwrap()).unwrap();
    let resp = f.get("/v1/snapshot/memory", &[]);
    assert_eq!(resp.status, 404);
    assert_eq!(
        serde_json::from_slice::<Value>(&resp.body).unwrap()["error"],
        "snapshot_missing"
    );
}

#[test]
fn malformed_range_is_400_bad_request() {
    let mut f = fixture(true);
    let resp = f.get("/v1/snapshot/memory", &[("Range", "items=0-1")]);
    assert_eq!(resp.status, 400);
    assert_eq!(
        serde_json::from_slice::<Value>(&resp.body).unwrap()["error"],
        "bad_request"
    );
}

// ---- H7b：双通道与受众隔离 ----
//
// 下面这一组夹具不走 `/v1/pair`：两台设备的身份与（可选）`replicate` 授权都得在
// `HubState::new` 拿走 `Root` 之前就绪，所以直接在 ledger 上配对。

struct Device {
    key: DeviceKey,
    token: String,
}

struct Channels {
    _dir: tempfile::TempDir,
    state: HubState,
    /// 普通设备：没有 `replicate` 能力 ⇒ 记忆通道（agent）。
    plain: Device,
    /// standby：有 `replicate` 能力 ⇒ 备份通道（backup），且不回落 agent。
    replicator: Device,
    agent: Manifest,
    backup: Option<Manifest>,
}

fn seed_device(root: &mut Root, now: i64, replicate: bool) -> Device {
    let key = DeviceKey::generate().unwrap();
    let (code, _) = rc_net::pair::new_pair_code().unwrap();
    let hash = rc_net::pair::hash_pair_code(&code).unwrap();
    rc_ledger::device::issue_pair_code(root.ledger_mut(), &hash, now, now + 300_000).unwrap();
    let (token, token_hash) = rc_net::pair::new_token().unwrap();
    rc_ledger::device::pair(
        root.ledger_mut(),
        &hash,
        &key.device_id(),
        &key.public_b64(),
        &token_hash,
        now,
    )
    .unwrap();
    if replicate {
        rc_ledger::device::grant_capability(
            root.ledger_mut(),
            &key.device_id(),
            rc_ledger::device::CAPABILITY_REPLICATE,
            now,
        )
        .unwrap();
    }
    Device { key, token }
}

/// 中心里摆两条审查行 + 一条判定：审查两表非空，两条通道的副本才有可比性。
fn seed_review(root: &Root) {
    root.memory()
        .conn()
        .execute_batch(
            "INSERT INTO review_item(source,pass_id,family,session_key,device_id,turn_uids_json,
                owner_json,payload_json,payload_sha256,state,created_at_ms,updated_at_ms) VALUES
               ('distill','naming','','s-1','dev-1','[]','{}','{}','aa','pending_review',1,1),
               ('distill','fact','decision','s-1','dev-1','[]','{}','{}','bb','approved',1,1);
             INSERT INTO review_decision(item_id,stage,verdict,reason_code,rules_version,
                input_sha256,decider,created_at_ms) VALUES
               (1,'program','pass','ok','v1','aa','program',1);",
        )
        .unwrap();
}

fn channels_fixture(publish_backup: bool) -> Channels {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let mut root = Root::open(&data).unwrap();
    seed_review(&root);
    let key = HubKey::load_or_create(&data.join("hub.key")).unwrap();
    let agent = publish_channel(&mut root, &key, &data, NOW, SnapshotChannel::Agent)
        .unwrap()
        .manifest;
    let backup = publish_backup.then(|| {
        publish_channel(&mut root, &key, &data, NOW + 1, SnapshotChannel::Backup)
            .unwrap()
            .manifest
    });
    let plain = seed_device(&mut root, NOW, false);
    let replicator = seed_device(&mut root, NOW, true);
    let cfg = HubConfig {
        bind: "127.0.0.1:7420".into(),
        endpoints: vec!["http://127.0.0.1:7420".into()],
        max_snapshot_bps: 4096,
    };
    let state = HubState::new(root, key, cfg, &data, Box::new(|| NOW));
    Channels {
        _dir: dir,
        state,
        plain,
        replicator,
        agent,
        backup,
    }
}

/// 以某台设备的身份发一条签名的 GET（`state`/`snapshot` 两条路由的签名体都是空）。
fn get_as(state: &mut HubState, dev: &Device, target: &str) -> Response {
    let doc = challenge(state, &dev.key, &dev.token);
    let nonce = doc["nonce"].as_str().unwrap().to_string();
    let headers = signed_headers(&dev.key, &dev.token, &nonce, "GET", target, &[]);
    state.handle(&Request::new("GET", target, headers, Vec::new()))
}

/// 把下发来的字节按 standby 的路子整替进一个本地库，返回落地后的本地库。
fn apply_to_local(tmp: &std::path::Path, name: &str, bytes: &[u8], sha: &str) -> Root {
    let snapshot = tmp.join(format!("{name}-served.db"));
    std::fs::write(&snapshot, bytes).unwrap();
    let mut local = Root::open(&tmp.join(name)).unwrap();
    hub::replicate::apply_memory(&mut local, &snapshot, sha, 0, NOW).unwrap();
    local
}

fn local_rows(root: &Root, table: &str) -> i64 {
    root.memory()
        .conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// 同一时刻两条通道各发一次：普通设备拿 agent（审查两表已清空），
/// `replicate` 设备拿 backup（审查两表保留）；两通道的 sha 与字节都不同。
#[test]
fn channels_route_by_replicate_capability() {
    let mut c = channels_fixture(true);
    let agent = c.agent.clone();
    let backup = c.backup.clone().expect("备份通道没发布");
    assert_ne!(agent.sha256, backup.sha256, "两条通道的 sha 相同");
    // 字节数不参与判定：删几行之后 VACUUM 出来的文件仍是页整数倍，两条通道可能一样大。

    // `/v1/state`：一行摘要就足以看出走的是哪条通道。
    let plain = get_as(&mut c.state, &c.plain, "/v1/state");
    assert_eq!(plain.status, 200, "{}", String::from_utf8_lossy(&plain.body));
    let plain_doc: Value = serde_json::from_slice(&plain.body).unwrap();
    assert_eq!(plain_doc["sha256"], agent.sha256, "普通设备没拿到记忆通道");

    let standby = get_as(&mut c.state, &c.replicator, "/v1/state");
    assert_eq!(standby.status, 200, "{}", String::from_utf8_lossy(&standby.body));
    let standby_doc: Value = serde_json::from_slice(&standby.body).unwrap();
    assert_eq!(standby_doc["sha256"], backup.sha256, "replicate 设备没拿到备份通道");

    // `/v1/snapshot/memory`：字节也跟着通道走。
    let plain_bytes = get_as(&mut c.state, &c.plain, "/v1/snapshot/memory");
    assert_eq!(
        plain_bytes.status,
        200,
        "{}",
        String::from_utf8_lossy(&plain_bytes.body)
    );
    assert_eq!(rc_net::protocol::sha256_hex(&plain_bytes.body), agent.sha256);

    let standby_bytes = get_as(&mut c.state, &c.replicator, "/v1/snapshot/memory");
    assert_eq!(
        standby_bytes.status,
        200,
        "{}",
        String::from_utf8_lossy(&standby_bytes.body)
    );
    assert_eq!(
        rc_net::protocol::sha256_hex(&standby_bytes.body),
        backup.sha256
    );
    assert_ne!(plain_bytes.body, standby_bytes.body, "两条通道下发了同一份字节");

    // 落进本地库之后受众差异才是可读的：agent 副本审查两表为空，backup 保留。
    let tmp = c._dir.path();
    let local_agent = apply_to_local(tmp, "local-agent", &plain_bytes.body, &agent.sha256);
    assert_eq!(local_rows(&local_agent, "review_item"), 0, "agent 副本带了审查队列");
    assert_eq!(
        local_rows(&local_agent, "review_decision"),
        0,
        "agent 副本带了判定台账"
    );

    let local_backup = apply_to_local(tmp, "local-backup", &standby_bytes.body, &backup.sha256);
    assert_eq!(
        local_rows(&local_backup, "review_item"),
        2,
        "备份通道的副本丢了审查队列"
    );
    assert_eq!(
        local_rows(&local_backup, "review_decision"),
        1,
        "备份通道的副本丢了判定台账"
    );
}

/// 备份通道还没发布时，`replicate` 设备拿到 404，**绝不回落** agent：
/// standby 若把 agent 副本当成完整记忆应用下去，本地审查队列会被静默清空。
#[test]
fn replicate_device_without_a_backup_snapshot_gets_404_not_the_agent_copy() {
    let mut c = channels_fixture(false);
    assert!(c.backup.is_none());

    // 同一时刻普通设备是拿得到的——说明 404 来自通道选择，不是根本没有快照。
    let plain = get_as(&mut c.state, &c.plain, "/v1/state");
    assert_eq!(plain.status, 200, "记忆通道本身没有快照");

    let state = get_as(&mut c.state, &c.replicator, "/v1/state");
    assert_eq!(state.status, 404, "replicate 设备回落到了 agent 快照");
    assert_eq!(
        serde_json::from_slice::<Value>(&state.body).unwrap()["error"],
        "no_snapshot"
    );

    let bytes = get_as(&mut c.state, &c.replicator, "/v1/snapshot/memory");
    assert_eq!(bytes.status, 404, "replicate 设备拿到了 agent 的字节");
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes.body).unwrap()["error"],
        "no_snapshot"
    );
    assert!(
        bytes.body.len() < 64,
        "404 响应里带上了文件字节：{} 字节",
        bytes.body.len()
    );
}

/// 备份通道目录与记忆通道目录是两个目录（`snapshots-backup` 不覆盖 `snapshots`）。
#[test]
fn the_two_channels_write_to_their_own_directories() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        channel_dir(dir.path(), SnapshotChannel::Agent),
        dir.path().join("snapshots")
    );
    assert_eq!(
        channel_dir(dir.path(), SnapshotChannel::Backup),
        dir.path().join("snapshots-backup")
    );
    assert_eq!(
        hub::snapshot::snapshots_dir(dir.path()),
        channel_dir(dir.path(), SnapshotChannel::Agent)
    );
}
