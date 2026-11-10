use node::state::SyncState;
use node::sync::{self, Mode, SyncPlan};
use rc_net::key::{DeviceKey, HubKey};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 一台假中心：答 pair / challenge / ingest / state / snapshot 五件事，
/// 并**数自己被叫了几次**（短路要用这个数）。
struct FakeCenter {
    key: HubKey,
    device_id: String,
    body: Vec<u8>,
    epoch: u64,
    seq: u64,
    /// 中心有没有快照。`false` 时 `/v1/state` 答 404 `no_snapshot`。
    has_snapshot: bool,
    /// `true` 时，凡是从偏移 0 以后开始的分片请求一律答 500（制造"下到一半断了"）。
    fail_snapshot: Arc<AtomicBool>,
    /// 每次分片请求的起点，按到达顺序记（续传要看第二轮的第一个起点）。
    ranges: Arc<Mutex<Vec<u64>>>,
    hits: Arc<AtomicUsize>,
    ingest_hits: Arc<AtomicUsize>,
    snapshot_hits: Arc<AtomicUsize>,
    /// `/v1/ingest` 的应答码。非 200 时只回错误，不装成功。
    ingest_status: u16,
    /// `/v1/challenge` 的应答码。非 200 时只回错误（制造“中心拒绝这份身份”）。
    challenge_status: u16,
    /// `/v1/state` 的应答码。非 200 时直接回错误（制造"中心坏了"）。
    state_status: u16,
    /// `true` 时 `/v1/state` 回一段不是 JSON 的字节（制造"答非契约"）。
    bad_state_json: bool,
    /// `true` 时清单签名对不上：签的是别的字节，形状仍是合法 base64。
    bad_manifest_sig: bool,
    /// `true` 时清单里的 sha 是 64 个 'a'：形状合法、内容不对 ⇒ 整份终验红。
    corrupt_sha: bool,
    /// `/v1/pair` 应答里的 `schema` 区间。`None` = 旧中心，不印这一格。
    pair_schema: Option<serde_json::Value>,
}

impl rc_net::http::Handler for FakeCenter {
    fn handle(&mut self, req: &rc_net::http::Request) -> rc_net::http::Response {
        use rc_net::http::Response;
        match (req.method.as_str(), req.path.as_str()) {
            ("POST", "/v1/pair") => {
                self.hits.fetch_add(1, Ordering::SeqCst);
                let body: serde_json::Value =
                    serde_json::from_slice(&req.body).unwrap_or(serde_json::Value::Null);
                let code = body.get("code").and_then(|v| v.as_str()).unwrap_or("");
                if code != "good-code" {
                    return Response::error(403, "pair_code_unknown");
                }
                let mut doc = serde_json::json!({
                    "api_version": 1,
                    "token": "tok-1",
                    "hub_pub": self.key.public_b64(),
                    "endpoints": ["lan-ipv6", "lan-ipv4"],
                });
                // 缺省照真中心的形状印 schema；`None` 用来造"旧中心不印这一格"。
                if let Some(schema) = &self.pair_schema {
                    doc["schema"] = schema.clone();
                }
                Response::json(200, doc)
            }
            ("POST", "/v1/challenge") => {
                self.hits.fetch_add(1, Ordering::SeqCst);
                if self.challenge_status != 200 {
                    return Response::error(self.challenge_status, "fixture_challenge_status");
                }
                let nonce = format!("n-{}", self.hits.load(Ordering::SeqCst));
                let expires_at_ms = 1i64;
                let sig = self.key.sign_b64(
                    rc_net::protocol::challenge_canonical(&self.device_id, &nonce, expires_at_ms)
                        .as_bytes(),
                );
                Response::json(
                    200,
                    serde_json::json!({
                        "api_version": 1, "nonce": nonce,
                        "expires_at_ms": expires_at_ms, "sig": sig,
                        "epoch": 1,
                    }),
                )
            }
            ("POST", "/v1/ingest") => {
                self.hits.fetch_add(1, Ordering::SeqCst);
                self.ingest_hits.fetch_add(1, Ordering::SeqCst);
                if self.ingest_status != 200 {
                    return Response::error(self.ingest_status, "fixture_ingest_status");
                }
                let lines = String::from_utf8_lossy(&req.body).lines().count();
                Response::json(
                    200,
                    serde_json::json!({
                        "api_version": 1, "lines_in": lines, "lines_accepted": lines,
                        "lines_rejected": 0, "turns_accepted": lines, "skipped_duplicate": 0,
                        "turns_rejected": 0, "rejects": [],
                    }),
                )
            }
            ("GET", "/v1/state") => {
                self.hits.fetch_add(1, Ordering::SeqCst);
                if self.state_status != 200 {
                    return Response::error(self.state_status, "fixture_state_status");
                }
                if !self.has_snapshot {
                    return Response::error(404, "no_snapshot");
                }
                if self.bad_state_json {
                    return Response::bytes(200, "application/json", b"{".to_vec());
                }
                let sha = if self.corrupt_sha {
                    "a".repeat(64)
                } else {
                    rc_net::protocol::sha256_hex(&self.body)
                };
                let canonical = rc_net::protocol::manifest_canonical(
                    self.epoch,
                    self.seq,
                    &sha,
                    rc_store::ddl::MEMORY_VERSION,
                    1,
                );
                let sig = if self.bad_manifest_sig {
                    self.key.sign_b64(b"not-the-manifest")
                } else {
                    self.key.sign_b64(canonical.as_bytes())
                };
                Response::json(
                    200,
                    serde_json::json!({
                        "api_version": 1, "epoch": self.epoch, "seq": self.seq,
                        "sha256": sha, "bytes": self.body.len(),
                        "schema_version": rc_store::ddl::MEMORY_VERSION,
                        "norm_version": 1, "created_at_ms": 1,
                        "signature": sig, "endpoint": "/v1/snapshot/memory",
                        "max_chunk_bytes": 10 * 1024 * 1024,
                    }),
                )
            }
            ("GET", "/v1/snapshot/memory") => {
                self.hits.fetch_add(1, Ordering::SeqCst);
                self.snapshot_hits.fetch_add(1, Ordering::SeqCst);
                use rc_net::http::Response;
                let total = self.body.len() as u64;
                let range = req.header("range").unwrap_or("");
                let (start, end) = rc_net::http::parse_range(range, total, 10 * 1024 * 1024)
                    .expect("the fixture serves the range it was asked for");
                self.ranges.lock().unwrap().push(start);
                if self.fail_snapshot.load(Ordering::SeqCst) && start > 0 {
                    return Response::error(500, "io");
                }
                let chunk = self.body[start as usize..=end as usize].to_vec();
                let mut resp = Response::bytes(206, "application/octet-stream", chunk);
                resp.headers
                    .push(("content-range".into(), format!("bytes {start}-{end}/{total}")));
                resp.headers.push((
                    "x-recollect-chunk-sha256".into(),
                    rc_net::protocol::sha256_hex(&resp.body),
                ));
                resp
            }
            _ => Response::error(404, "not_found"),
        }
    }
}

fn start(center: FakeCenter) -> (node::node_cfg::Endpoint, Arc<AtomicUsize>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let hits = center.hits.clone();
    let ingest_hits = center.ingest_hits.clone();
    let snapshot_hits = center.snapshot_hits.clone();
    std::thread::spawn(move || {
        let mut center = center;
        rc_net::http::serve(
            listener,
            &mut center,
            &flag,
            rc_net::http::Limits::default(),
            0,
        );
    });
    (
        node::node_cfg::Endpoint {
            label: "lan-ipv6".into(),
            host: "127.0.0.1".into(),
            port,
            tls_fp: None,
        },
        hits,
        ingest_hits,
        snapshot_hits,
    )
}

/// `HubKey` 不实现 `Clone`：公钥串在把钥匙交给假中心**之前**取出来，
/// 测试拿串去验签，中心拿钥匙去签。
fn fixture() -> (tempfile::TempDir, DeviceKey, String, node::node_cfg::Endpoint,
                 Arc<AtomicUsize>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    fixture_with(|_| {})
}

/// 同一台假中心，先让用例改它的应答形状再启动。
fn fixture_with(tune: impl FnOnce(&mut FakeCenter))
    -> (tempfile::TempDir, DeviceKey, String, node::node_cfg::Endpoint,
        Arc<AtomicUsize>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let dir = tempfile::tempdir().unwrap();
    let key = DeviceKey::generate().unwrap();
    let hub_key = HubKey::load_or_create(&dir.path().join("hub.key")).unwrap();
    let hub_pub = hub_key.public_b64();
    let body: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();
    let hits = Arc::new(AtomicUsize::new(0));
    let ingest_hits = Arc::new(AtomicUsize::new(0));
    let snapshot_hits = Arc::new(AtomicUsize::new(0));
    let mut center = FakeCenter {
        key: hub_key,
        device_id: key.device_id(),
        body,
        epoch: 7,
        seq: 3,
        has_snapshot: true,
        fail_snapshot: Arc::new(AtomicBool::new(false)),
        ranges: Arc::new(Mutex::new(Vec::new())),
        hits: hits.clone(),
        ingest_hits: ingest_hits.clone(),
        snapshot_hits: snapshot_hits.clone(),
        ingest_status: 200,
        challenge_status: 200,
        state_status: 200,
        bad_state_json: false,
        bad_manifest_sig: false,
        corrupt_sha: false,
        // 与真中心同一形状：本代 schema 区间。
        pair_schema: Some(serde_json::json!({"min": 6, "max": 6})),
    };
    tune(&mut center);
    let (ep, h, ih, sh) = start(center);
    (dir, key, hub_pub, ep, h, ih, sh)
}

/// **同一把钥匙**既签名又出设备 id（`SyncPlan.key` 是借的，理由见 `sync.rs` 里的注释）。
fn plan_for<'a>(dir: &tempfile::TempDir, key: &'a DeviceKey, hub_pub: &str,
                ep: &node::node_cfg::Endpoint) -> SyncPlan<'a> {
    SyncPlan {
        endpoints: vec![rc_net::node::Endpoint {
            label: ep.label.clone(),
            host: ep.host.clone(),
            port: ep.port,
            tls_fp: ep.tls_fp.clone(),
        }],
        token: "tok-1".into(),
        device_id: key.device_id(),
        key,
        hub_pub: hub_pub.to_string(),
        data_dir: dir.path().to_path_buf(),
        // 空表 = 本计划不发现（S4d1 的关闭开关）。
        discovery: Vec::new(),
        discovery_timeout: Duration::from_secs(1),
        // 纯 HTTP 假中心：没有 TLS 指纹可钉；发现表为空时这一格没有读者。
        hub_tls_fp: None,
    }
}

/// 往节点本机的转录层里写两轮（上行源）。
fn seed_turns(dir: &tempfile::TempDir, device_id: &str) {
    let root = rc_store::NodeRoot::open_node(dir.path(), None).unwrap();
    let sid = rc_contract::SessionId {
        session_key: "s1".into(),
        device_id: device_id.to_string(),
        source_agent: "agent".into(),
        machine: None,
        project_hint: None,
        last_saved_at_ms: None,
        owner_principal_id: None,
        principal_name: None,
        principal_source: None,
    };
    let turns: Vec<rc_contract::NormalizedTurn> = ["one", "two"]
        .iter()
        .enumerate()
        .map(|(i, body)| rc_contract::NormalizedTurn {
            turn_uid: format!("t-{}", i),
            body_fp: rc_contract::compute_body_fp(body),
            tier: rc_contract::TurnTier::ContentOnly,
            body: body.to_string(),
            role: "agent".into(),
            created_at_ms: 100 + i as i64,
        })
        .collect();
    rc_ledger::insert_batch(root.transcript(), &sid, &turns, 500).unwrap();
}

/// 两会话交错：s1 的轮落在 100/300，s2 的轮落在 200。
/// 全局最大轮在**先出现的那个会话**里——游标该落在 300，不是 s2 的 200。
fn seed_interleaved_turns(dir: &tempfile::TempDir, device_id: &str) {
    let root = rc_store::NodeRoot::open_node(dir.path(), None).unwrap();
    let sid_of = |sk: &str| rc_contract::SessionId {
        session_key: sk.into(),
        device_id: device_id.to_string(),
        source_agent: "agent".into(),
        machine: None,
        project_hint: None,
        last_saved_at_ms: None,
        owner_principal_id: None,
        principal_name: None,
        principal_source: None,
    };
    let turn_of = |uid: &str, at: i64| rc_contract::NormalizedTurn {
        turn_uid: uid.into(),
        body_fp: rc_contract::compute_body_fp(uid),
        tier: rc_contract::TurnTier::ContentOnly,
        body: uid.into(),
        role: "agent".into(),
        created_at_ms: at,
    };
    let s1 = sid_of("s1");
    let s2 = sid_of("s2");
    rc_ledger::insert_batch(root.transcript(), &s1, &[turn_of("a1", 100)], 100).unwrap();
    rc_ledger::insert_batch(root.transcript(), &s2, &[turn_of("b1", 200)], 200).unwrap();
    rc_ledger::insert_batch(root.transcript(), &s1, &[turn_of("a2", 300)], 300).unwrap();
}

#[test]
fn a_pull_downloads_the_snapshot_and_writes_the_replica() {
    let (dir, key, hub_pub, ep, _hits, _ingest, snapshot_hits) = fixture();
    let plan = plan_for(&dir, &key, &hub_pub, &ep);
    let mut st = SyncState::default();
    let out = sync::sync_once(&plan, &mut st, Mode::Pull, 1, Duration::from_secs(5)).unwrap();
    assert!(out.pulled, "{out:?}");
    assert!(!out.no_memory);
    assert_eq!(snapshot_hits.load(Ordering::SeqCst), 1);
    let replica = dir.path().join("replica").join("snapshot-7-3.db");
    assert_eq!(std::fs::metadata(&replica).unwrap().len(), 1024);
    assert_eq!(st.current_manifest.epoch, 7);
    assert_eq!(st.current_manifest.seq, 3);
}

#[test]
fn a_second_pull_of_the_same_manifest_downloads_nothing() {
    let (dir, key, hub_pub, ep, _hits, _ingest, snapshot_hits) = fixture();
    let plan = plan_for(&dir, &key, &hub_pub, &ep);
    let mut st = SyncState::default();
    sync::sync_once(&plan, &mut st, Mode::Pull, 1, Duration::from_secs(5)).unwrap();
    let after_first = snapshot_hits.load(Ordering::SeqCst);
    let out = sync::sync_once(&plan, &mut st, Mode::Pull, 2, Duration::from_secs(5)).unwrap();
    assert!(!out.pulled, "同一份清单不该再下：{out:?}");
    assert!(out.pull_skipped);
    assert_eq!(snapshot_hits.load(Ordering::SeqCst), after_first, "不该再发一次分片请求");
}

#[test]
fn a_push_with_nothing_pending_sends_no_request() {
    let (dir, key, hub_pub, ep, _hits, ingest_hits, _sh) = fixture();
    let plan = plan_for(&dir, &key, &hub_pub, &ep);
    let mut st = SyncState::default();
    let out = sync::sync_once(&plan, &mut st, Mode::Push, 1, Duration::from_secs(5)).unwrap();
    assert!(out.push_skipped, "{out:?}");
    assert_eq!(ingest_hits.load(Ordering::SeqCst), 0, "没有待发就不该发请求");
}

#[test]
fn a_push_sends_the_pending_turns_and_advances_the_cursor() {
    let (dir, key, hub_pub, ep, _hits, ingest_hits, _sh) = fixture();
    seed_turns(&dir, &key.device_id());
    let plan = plan_for(&dir, &key, &hub_pub, &ep);
    let mut st = SyncState::default();
    let out = sync::sync_once(&plan, &mut st, Mode::Push, 1, Duration::from_secs(5)).unwrap();
    assert_eq!(out.pushed, 2, "{out:?}");
    assert_eq!(ingest_hits.load(Ordering::SeqCst), 1);
    assert_eq!(st.push_cursor.at_ms, 500, "游标推到发出去那批的最大值");
}

#[test]
fn a_second_push_sends_nothing_again() {
    let (dir, key, hub_pub, ep, _hits, ingest_hits, _sh) = fixture();
    seed_turns(&dir, &key.device_id());
    let plan = plan_for(&dir, &key, &hub_pub, &ep);
    let mut st = SyncState::default();
    sync::sync_once(&plan, &mut st, Mode::Push, 1, Duration::from_secs(5)).unwrap();
    let after_first = ingest_hits.load(Ordering::SeqCst);
    let out = sync::sync_once(&plan, &mut st, Mode::Push, 2, Duration::from_secs(5)).unwrap();
    assert!(out.push_skipped, "{out:?}");
    assert_eq!(ingest_hits.load(Ordering::SeqCst), after_first);
}

#[test]
fn the_cursor_is_the_batch_maximum_even_when_sessions_interleave() {
    let (dir, key, hub_pub, ep, _hits, ingest_hits, _sh) = fixture();
    seed_interleaved_turns(&dir, &key.device_id());
    let plan = plan_for(&dir, &key, &hub_pub, &ep);
    let mut st = SyncState::default();
    sync::sync_once(&plan, &mut st, Mode::Push, 1, Duration::from_secs(5)).unwrap();
    assert_eq!(st.push_cursor.at_ms, 300, "游标是这批的最大值，不是最后那个会话的末轮");
    let after_first = ingest_hits.load(Ordering::SeqCst);
    let out = sync::sync_once(&plan, &mut st, Mode::Push, 2, Duration::from_secs(5)).unwrap();
    assert!(out.push_skipped, "游标退回去会让先出现的会话重推：{out:?}");
    assert_eq!(ingest_hits.load(Ordering::SeqCst), after_first);
}

#[test]
fn a_failed_download_leaves_a_resume_point_for_the_next_run() {
    let dir = tempfile::tempdir().unwrap();
    let key = DeviceKey::generate().unwrap();
    let hub_key = HubKey::load_or_create(&dir.path().join("hub.key")).unwrap();
    let hub_pub = hub_key.public_b64();
    // 两个分片：第二片在整个第一轮里都失败 ⇒ 盘上留下第一片的字节。
    let body: Vec<u8> = (0..(rc_net::protocol::MAX_SNAPSHOT_CHUNK as u32 + 1))
        .map(|i| (i % 251) as u8)
        .collect();
    let total = body.len() as u64;
    let fail = Arc::new(AtomicBool::new(true));
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let center = FakeCenter {
        key: hub_key,
        device_id: key.device_id(),
        body,
        epoch: 7,
        seq: 3,
        has_snapshot: true,
        fail_snapshot: fail.clone(),
        ranges: ranges.clone(),
        hits: Arc::new(AtomicUsize::new(0)),
        ingest_hits: Arc::new(AtomicUsize::new(0)),
        snapshot_hits: Arc::new(AtomicUsize::new(0)),
        ingest_status: 200,
        challenge_status: 200,
        state_status: 200,
        bad_state_json: false,
        bad_manifest_sig: false,
        corrupt_sha: false,
        // 与真中心同一形状：本代 schema 区间。
        pair_schema: Some(serde_json::json!({"min": 6, "max": 6})),
    };
    let (ep, _h, _ih, _sh) = start(center);
    let plan = plan_for(&dir, &key, &hub_pub, &ep);
    let mut st = SyncState::default();
    let e = sync::sync_once(&plan, &mut st, Mode::Pull, 1, Duration::from_secs(5)).unwrap_err();
    assert_eq!(e, sync::SyncError::DownloadFailed);
    assert_eq!(st.partial.epoch, 7);
    assert_eq!(st.partial.received, rc_net::protocol::MAX_SNAPSHOT_CHUNK as u64, "盘上那段没留给下一轮");

    fail.store(false, Ordering::SeqCst);
    ranges.lock().unwrap().clear();
    let out = sync::sync_once(&plan, &mut st, Mode::Pull, 2, Duration::from_secs(5)).unwrap();
    assert!(out.pulled, "{out:?}");
    assert_eq!(
        ranges.lock().unwrap().first().copied(),
        Some(rc_net::protocol::MAX_SNAPSHOT_CHUNK as u64),
        "第二轮必须从盘上那段续，不是从头来"
    );
    let replica = dir.path().join("replica").join("snapshot-7-3.db");
    assert_eq!(std::fs::metadata(&replica).unwrap().len(), total);
}

#[test]
fn a_center_without_a_snapshot_is_reported_as_no_memory() {
    let dir = tempfile::tempdir().unwrap();
    let key = DeviceKey::generate().unwrap();
    let hub_key = HubKey::load_or_create(&dir.path().join("hub.key")).unwrap();
    let hub_pub = hub_key.public_b64();
    let hits = Arc::new(AtomicUsize::new(0));
    let center = FakeCenter {
        key: hub_key,
        device_id: key.device_id(),
        body: vec![],
        epoch: 0,
        seq: 0,
        has_snapshot: false,
        fail_snapshot: Arc::new(AtomicBool::new(false)),
        ranges: Arc::new(Mutex::new(Vec::new())),
        hits: hits.clone(),
        ingest_hits: Arc::new(AtomicUsize::new(0)),
        snapshot_hits: Arc::new(AtomicUsize::new(0)),
        ingest_status: 200,
        challenge_status: 200,
        state_status: 200,
        bad_state_json: false,
        bad_manifest_sig: false,
        corrupt_sha: false,
        // 与真中心同一形状：本代 schema 区间。
        pair_schema: Some(serde_json::json!({"min": 6, "max": 6})),
    };
    let (ep, _h, _ih, sh) = start(center);
    let plan = plan_for(&dir, &key, &hub_pub, &ep);
    let mut st = SyncState::default();
    let out = sync::sync_once(&plan, &mut st, Mode::Pull, 1, Duration::from_secs(5)).unwrap();
    assert!(out.no_memory, "没有快照要说'无可用记忆'，不是失败：{out:?}");
    assert!(!out.pulled);
    assert_eq!(sh.load(Ordering::SeqCst), 0, "不该发分片请求");
}

#[test]
fn a_node_without_a_token_is_no_pairing_not_not_paired() {
    let (dir, key, hub_pub, ep, _h, _ih, _sh) = fixture();
    let mut plan = plan_for(&dir, &key, &hub_pub, &ep);
    plan.token.clear();
    let mut st = SyncState::default();
    let e = sync::sync_once(&plan, &mut st, Mode::Pull, 1, Duration::from_secs(5)).unwrap_err();
    assert_eq!(e.code(), "no_pairing", "本机没有 token 是'还没配对'：{e:?}");
}

#[test]
fn a_pairing_code_the_hub_rejects_is_not_paired() {
    let (_dir, key, _hub_pub, ep, _h, _ih, _sh) = fixture();
    let ep = rc_net::node::Endpoint { label: ep.label, host: ep.host, port: ep.port, tls_fp: ep.tls_fp };
    let e = sync::pair(&ep, "wrong-code", &key, Duration::from_secs(5)).unwrap_err();
    assert_eq!(e.code(), "not_paired", "配对码错不是'无可用记忆'：{e:?}");
}

/// R10：配对应答里的 `schema: {min, max}` 与本地区间不相交 ⇒ 就地响亮失败。
/// 不判这一格的话，版本偏斜会一路配对成功，之后每轮 pull 都读成 `bad_response`。
#[test]
fn a_pair_reply_with_a_disjoint_schema_is_schema_unsupported() {
    let (_dir, key, _hub_pub, ep, _h, _ih, _sh) =
        fixture_with(|c| c.pair_schema = Some(serde_json::json!({"min": 99, "max": 100})));
    let ep = rc_net::node::Endpoint { label: ep.label, host: ep.host, port: ep.port, tls_fp: ep.tls_fp };
    let e = sync::pair(&ep, "good-code", &key, Duration::from_secs(5)).unwrap_err();
    assert_eq!(e, sync::SyncError::SchemaUnsupported);
    assert_eq!(e.code(), "schema_unsupported");
}

/// 旧中心不印 `schema` 这一格：缺省**不是**"不兼容"，配对照常成功。
#[test]
fn a_pair_reply_without_a_schema_field_still_pairs() {
    let (_dir, key, _hub_pub, ep, _h, _ih, _sh) = fixture_with(|c| c.pair_schema = None);
    let ep = rc_net::node::Endpoint { label: ep.label, host: ep.host, port: ep.port, tls_fp: ep.tls_fp };
    let paired = sync::pair(&ep, "good-code", &key, Duration::from_secs(5)).unwrap();
    assert_eq!(paired.token, "tok-1");
}

#[test]
fn an_unauthorized_push_is_not_paired() {
    let (dir, key, hub_pub, ep, _h, _ih, _sh) =
        fixture_with(|c| c.ingest_status = 401);
    seed_turns(&dir, &key.device_id());
    let plan = plan_for(&dir, &key, &hub_pub, &ep);
    let mut st = SyncState::default();
    let e = sync::sync_once(&plan, &mut st, Mode::Push, 1, Duration::from_secs(5)).unwrap_err();
    assert_eq!(e.code(), "not_paired", "token 不被接受 ≠ 中心没有记忆：{e:?}");
    assert_eq!(st.push_cursor.at_ms, 0, "被拒的一批不许推游标");
}

#[test]
fn a_batch_the_hub_rejects_whole_is_ingest_failed() {
    let (dir, key, hub_pub, ep, _h, _ih, _sh) =
        fixture_with(|c| c.ingest_status = 422);
    seed_turns(&dir, &key.device_id());
    let plan = plan_for(&dir, &key, &hub_pub, &ep);
    let mut st = SyncState::default();
    let e = sync::sync_once(&plan, &mut st, Mode::Push, 1, Duration::from_secs(5)).unwrap_err();
    assert_eq!(e.code(), "ingest_failed");
    assert_eq!(st.push_cursor.at_ms, 0, "整批被拒 ⇒ 那批还在队列里");
}

#[test]
fn a_failing_hub_is_not_reported_as_no_memory() {
    let (dir, key, hub_pub, ep, _h, _ih, _sh) =
        fixture_with(|c| c.state_status = 500);
    let plan = plan_for(&dir, &key, &hub_pub, &ep);
    let mut st = SyncState::default();
    let e = sync::sync_once(&plan, &mut st, Mode::Pull, 1, Duration::from_secs(5)).unwrap_err();
    assert_eq!(e.code(), "bad_response", "中心坏了不是'没有记忆'：{e:?}");
}

#[test]
fn an_unparseable_hub_response_is_bad_response() {
    let (dir, key, hub_pub, ep, _h, _ih, _sh) =
        fixture_with(|c| c.bad_state_json = true);
    let plan = plan_for(&dir, &key, &hub_pub, &ep);
    let mut st = SyncState::default();
    let e = sync::sync_once(&plan, &mut st, Mode::Pull, 1, Duration::from_secs(5)).unwrap_err();
    assert_eq!(e.code(), "bad_response", "{e:?}");
}

#[test]
fn a_manifest_signature_that_does_not_verify_is_hub_key_mismatch() {
    let (dir, key, hub_pub, ep, _h, _ih, _sh) =
        fixture_with(|c| c.bad_manifest_sig = true);
    let plan = plan_for(&dir, &key, &hub_pub, &ep);
    let mut st = SyncState::default();
    let e = sync::sync_once(&plan, &mut st, Mode::Pull, 1, Duration::from_secs(5)).unwrap_err();
    assert_eq!(e.code(), "hub_key_mismatch", "签名对不上就是钉死的公钥对不上：{e:?}");
}

#[test]
fn a_download_that_does_not_match_the_manifest_is_chunk_mismatch() {
    let (dir, key, hub_pub, ep, _h, _ih, _sh) =
        fixture_with(|c| c.corrupt_sha = true);
    let plan = plan_for(&dir, &key, &hub_pub, &ep);
    let mut st = SyncState::default();
    let e = sync::sync_once(&plan, &mut st, Mode::Pull, 1, Duration::from_secs(5)).unwrap_err();
    assert_eq!(e.code(), "chunk_mismatch", "拼起来的字节不是清单那一份：{e:?}");
    assert!(!dir.path().join("replica").join("snapshot-7-3.db").exists());
}

#[test]
fn a_missing_replica_is_downloaded_again_even_when_the_manifest_is_current() {
    let (dir, key, hub_pub, ep, _h, _ih, snapshot_hits) = fixture();
    let plan = plan_for(&dir, &key, &hub_pub, &ep);
    let mut st = SyncState::default();
    sync::sync_once(&plan, &mut st, Mode::Pull, 1, Duration::from_secs(5)).unwrap();
    let first = snapshot_hits.load(Ordering::SeqCst);
    std::fs::remove_file(dir.path().join("replica").join("snapshot-7-3.db")).unwrap();
    let out = sync::sync_once(&plan, &mut st, Mode::Pull, 2, Duration::from_secs(5)).unwrap();
    assert!(out.pulled, "清单没变但副本没了：必须再下一次，不是永远跳过：{out:?}");
    assert!(
        snapshot_hits.load(Ordering::SeqCst) > first,
        "副本丢了却一次分片都没请求"
    );
    assert_eq!(
        std::fs::metadata(dir.path().join("replica").join("snapshot-7-3.db")).unwrap().len(),
        1024
    );
}

#[test]
fn a_reader_holding_the_old_name_does_not_block_the_new_generation() {
    let (dir, key, hub_pub, ep, _h, _ih, _sh) = fixture();
    let plan = plan_for(&dir, &key, &hub_pub, &ep);
    let mut st = SyncState::default();
    let legacy = dir.path().join("replica").join("memory.db");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(&legacy, vec![0u8; 1024]).unwrap();
    // 模拟 mcp：只读打开旧名并一直握着。Windows 上带句柄的文件不能被 rename 覆盖。
    let held = std::fs::File::open(&legacy).unwrap();
    let out = sync::sync_once(&plan, &mut st, Mode::Pull, 1, Duration::from_secs(5)).unwrap();
    assert!(out.pulled, "{out:?}");
    let versioned = dir.path().join("replica").join("snapshot-7-3.db");
    assert_eq!(std::fs::metadata(&versioned).unwrap().len(), 1024, "新副本要落在代际名下");
    assert!(legacy.is_file(), "旧名不许被替换或删除：读者还开着它");
    drop(held);
}

#[test]
fn an_upgraded_node_migrates_the_legacy_replica_instead_of_downloading_again() {
    let (dir, key, hub_pub, ep, _h, _ih, snapshot_hits) = fixture();
    let plan = plan_for(&dir, &key, &hub_pub, &ep);
    let mut st = SyncState::default();
    sync::sync_once(&plan, &mut st, Mode::Pull, 1, Duration::from_secs(5)).unwrap();
    let versioned = dir.path().join("replica").join("snapshot-7-3.db");
    let legacy = dir.path().join("replica").join("memory.db");
    // 升级前的形状：状态说已是最新，字节只在旧名下。
    std::fs::rename(&versioned, &legacy).unwrap();
    let hits = snapshot_hits.load(Ordering::SeqCst);
    let out = sync::sync_once(&plan, &mut st, Mode::Pull, 2, Duration::from_secs(5)).unwrap();
    assert!(!out.pulled, "升级前的副本还在，不该重下：{out:?}");
    assert_eq!(snapshot_hits.load(Ordering::SeqCst), hits, "一条分片都不该再请求");
    assert!(versioned.is_file(), "旧名要迁到代际名");
    assert!(!legacy.exists(), "迁完之后旧名不该还在");
}

/// 设备地址簿里有两条路：一条不可达、一条被中心拒绝。
/// “答过”比“没通”更重要——不能因为死路把整轮读成断网，否则离线窗口会错误累积。
#[test]
fn an_answered_endpoint_beats_an_unreachable_one() {
    let (dir, key, hub_pub, ep, _h, _ih, _sh) = fixture_with(|c| c.challenge_status = 401);
    let dead = rc_net::node::Endpoint {
        label: "lan".into(),
        host: "127.0.0.1".into(),
        port: 1,
        tls_fp: None,
    };
    let live = rc_net::node::Endpoint {
        label: "mesh".into(),
        host: ep.host.clone(),
        port: ep.port,
        tls_fp: ep.tls_fp.clone(),
    };
    let plan = SyncPlan {
        endpoints: vec![dead, live],
        token: "tok-1".into(),
        device_id: key.device_id(),
        key: &key,
        hub_pub,
        data_dir: dir.path().to_path_buf(),
        // 空表 = 本计划不发现（S4d1 的关闭开关）。
        discovery: Vec::new(),
        discovery_timeout: Duration::from_secs(1),
        hub_tls_fp: None,
    };
    let mut st = SyncState::default();
    let err = sync::sync_once(&plan, &mut st, Mode::Pull, 1, Duration::from_secs(2))
        .expect_err("两条路都不成");
    assert_eq!(
        err,
        sync::SyncError::AuthRejected,
        "一条路答了（哪怕是拒绝），就不能把另一条路的不可达读成断网"
    );
}
