//! 真实签名客户端 + 假中心：`hub replicate pull` / `ack` / `acked` 的端到端。

use rc_contract::{compute_body_fp, compute_turn_uid, NormalizedTurn, SessionId, TurnTier};
use rc_ledger::transcript::insert_batch_committed;
use rc_memory::writers_x::put_card;
use rc_memory::{AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_net::http::{Handler, Limits, Request, Response};
use rc_net::key::{DeviceKey, HubKey};
use rc_net::protocol::challenge_canonical;
use rc_net::snapshot::SnapshotState;
use rc_store::Root;
use serde_json::{json, Value};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// 假中心收到的回执。测试拿它的克隆在客户端跑完后对账。
#[derive(Clone, Default)]
struct Acks(Arc<Mutex<Vec<u64>>>);

impl Acks {
    fn push(&self, seq: u64) {
        self.0.lock().unwrap().push(seq);
    }

    fn max(&self) -> u64 {
        self.0.lock().unwrap().iter().copied().max().unwrap_or(0)
    }

    fn seen(&self) -> Vec<u64> {
        self.0.lock().unwrap().clone()
    }
}

struct FakeCenter {
    hub_key: HubKey,
    status: Vec<u8>,
    records: Vec<u8>,
    ledger: Vec<u8>,
    state: Vec<u8>,
    memory: Vec<u8>,
    acks: Acks,
}

impl Handler for FakeCenter {
    fn handle(&mut self, req: &Request) -> Response {
        match (req.method.as_str(), req.path.as_str()) {
            ("POST", "/v1/challenge") => {
                let device = req.header("x-recollect-device").unwrap_or("dev").to_string();
                let nonce = "n1".to_string();
                let expires_at_ms = 10_000i64;
                let sig = self.hub_key.sign_b64(
                    challenge_canonical(&device, &nonce, expires_at_ms).as_bytes(),
                );
                Response::json(
                    200,
                    json!({"nonce": nonce, "expires_at_ms": expires_at_ms, "sig": sig, "epoch": 1}),
                )
            }
            ("GET", "/v1/replicate/status") => Response::bytes(200, "application/json", self.status.clone()),
            ("GET", "/v1/replicate") => {
                if req.query_get("since") == Some("0") {
                    Response::bytes(200, "application/json", self.records.clone())
                } else {
                    Response::bytes(200, "application/json", br#"{"records":[]}"#.to_vec())
                }
            }
            ("GET", "/v1/replicate/ledger") => Response::bytes(200, "application/json", self.ledger.clone()),
            ("POST", "/v1/replicate/ack") => {
                let applied = serde_json::from_slice::<Value>(&req.body)
                    .ok()
                    .and_then(|doc| doc.get("applied_seq").and_then(Value::as_u64));
                match applied {
                    Some(seq) => {
                        self.acks.push(seq);
                        Response::json(
                            200,
                            json!({"epoch": 1, "applied_seq": seq, "replicated_seq": self.acks.max()}),
                        )
                    }
                    None => Response::error(400, "bad_request"),
                }
            }
            ("GET", "/v1/replicate/acked") => {
                let last: u64 = serde_json::from_slice::<Value>(&self.status)
                    .ok()
                    .and_then(|doc| doc.get("last_commit_seq").and_then(Value::as_u64))
                    .unwrap_or(0);
                Response::json(
                    200,
                    json!({"epoch": 1, "last_commit_seq": last, "replicated_seq": self.acks.max()}),
                )
            }
            ("GET", "/v1/state") => Response::bytes(200, "application/json", self.state.clone()),
            ("GET", "/v1/snapshot/memory") => {
                let range = req.header("range").unwrap_or("bytes=0-0");
                let raw = range.strip_prefix("bytes=").unwrap_or("0-0");
                let (start, end) = raw.split_once('-').unwrap_or(("0", "0"));
                let start: usize = start.parse().unwrap_or(0);
                let end: usize = end.parse().unwrap_or(0);
                let end = end.min(self.memory.len().saturating_sub(1));
                let body = if start <= end {
                    self.memory[start..=end].to_vec()
                } else {
                    Vec::new()
                };
                let sha = rc_net::protocol::sha256_hex(&body);
                let mut resp = Response::bytes(206, "application/octet-stream", body);
                resp.headers.push(("X-Recollect-Chunk-Sha256".into(), sha));
                resp
            }
            _ => Response::error(404, "not_found"),
        }
    }
}

struct FakeServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl FakeServer {
    fn start(mut fake: FakeCenter) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let join = std::thread::spawn(move || {
            rc_net::http::serve(listener, &mut fake, &stop_thread, Limits::default(), 0);
        });
        FakeServer {
            addr,
            stop,
            join: Some(join),
        }
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

type CenterFixture = (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, HubKey);

fn center_fixture(dir: &std::path::Path) -> CenterFixture {
    let center_dir = dir.join("center");
    let mut center = Root::open(&center_dir).unwrap();
    let session = SessionId {
        session_key: "s-1".into(),
        device_id: "dev-1".into(),
        source_agent: "qoder".into(),
        machine: None,
        project_hint: None,
        last_saved_at_ms: None,
        owner_principal_id: None,
        principal_name: None,
        principal_source: None,
    };
    let body = "alpha";
    let turn = NormalizedTurn {
        tier: TurnTier::ContentOnly,
        turn_uid: compute_turn_uid("dev-1", "s-1", "human", body, 100),
        body: body.into(),
        body_fp: compute_body_fp(body),
        role: "human".into(),
        created_at_ms: 100,
    };
    insert_batch_committed(center.transcript_mut(), &session, &[turn], 100, "import").unwrap();
    let dev = DeviceKey::generate().unwrap();
    let (code, _) = rc_net::pair::new_pair_code().unwrap();
    let hash = rc_net::pair::hash_pair_code(&code).unwrap();
    rc_ledger::device::issue_pair_code(center.ledger_mut(), &hash, 100, 10_000).unwrap();
    let (_token, token_hash) = rc_net::pair::new_token().unwrap();
    rc_ledger::device::pair(
        center.ledger_mut(),
        &hash,
        &dev.device_id(),
        &dev.public_b64(),
        &token_hash,
        101,
    )
    .unwrap();
    rc_ledger::device::grant_capability(
        center.ledger_mut(),
        &dev.device_id(),
        rc_ledger::device::CAPABILITY_REPLICATE,
        102,
    )
    .unwrap();
    put_card(
        center.memory(),
        &FactContent {
            kind: "note".into(),
            title: "pull 卡".into(),
            body: "pull 正文".into(),
            scope: Scope {
                kind: ScopeKind::Project,
                key: "proj".into(),
            },
            valid_to_ms: None,
        },
        &[Provenance {
            turn_uid: "u-1".into(),
            anchor_role: AnchorRole::Human,
        }],
        100,
    )
    .unwrap();
    let hub_key = HubKey::load_or_create(&center_dir.join("hub.key")).unwrap();
    // H7b：standby 从备份通道取记忆（完整记忆）。
    let report = hub::snapshot::publish_channel(
        &mut center,
        &hub_key,
        &center_dir,
        1_000,
        hub::snapshot::SnapshotChannel::Backup,
    )
    .unwrap();
    let snaps = hub::snapshot::channel_dir(&center_dir, hub::snapshot::SnapshotChannel::Backup);
    let state: SnapshotState =
        serde_json::from_slice(&std::fs::read(snaps.join("CURRENT")).unwrap()).unwrap();
    let snapshot_file = snaps.join(rc_net::snapshot::snapshot_name(
        report.manifest.epoch,
        report.manifest.seq,
    ));
    let memory = std::fs::read(snapshot_file).unwrap();
    let status = json!({"last_commit_seq": 1, "epoch": 1}).to_string().into_bytes();
    let records = serde_json::to_vec(&json!({
        "records": hub::replicate::records_since(center.transcript(), 0, 10).unwrap()
    }))
    .unwrap();
    let ledger = serde_json::to_vec(&hub::replicate::export_ledger(&center).unwrap()).unwrap();
    let state = serde_json::to_vec(&json!({
        "epoch": state.manifest.epoch,
        "seq": state.manifest.seq,
        "sha256": state.manifest.sha256,
        "bytes": state.manifest.bytes,
        "schema_version": state.manifest.schema_version,
        "norm_version": state.manifest.norm_version,
        "created_at_ms": state.manifest.created_at_ms,
        "signature": state.manifest.signature,
        "center_commit_seq": state.center_commit_seq,
    }))
    .unwrap();
    (status, records, ledger, state, memory, hub_key)
}

/// 每个用例一份设备身份与三件凭据；`backup` 目录留给 pull 落地。
fn client_files(
    dir: &Path,
    hub_pub: &str,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let dev = DeviceKey::generate().unwrap();
    let key_path = dir.join("device.key");
    dev.save(&key_path).unwrap();
    let token_path = dir.join("token");
    std::fs::write(&token_path, "token\n").unwrap();
    let hub_pub_path = dir.join("hub.pub");
    std::fs::write(&hub_pub_path, format!("{hub_pub}\n")).unwrap();
    (key_path, token_path, hub_pub_path)
}

fn center_args(
    server: &FakeServer,
    files: &(std::path::PathBuf, std::path::PathBuf, std::path::PathBuf),
) -> Vec<String> {
    vec![
        "--endpoint".into(),
        format!("127.0.0.1:{}", server.addr.port()),
        "--token-file".into(),
        files.1.to_str().unwrap().into(),
        "--device-key-file".into(),
        files.0.to_str().unwrap().into(),
        "--hub-pub-file".into(),
        files.2.to_str().unwrap().into(),
    ]
}

fn run_pull(dir: &Path, server: &FakeServer, hub_pub: &str, extra: &[&str]) -> Output {
    let files = client_files(dir, hub_pub);
    let backup = dir.join("backup");
    let mut args: Vec<String> = vec!["replicate".into(), "pull".into()];
    args.extend(center_args(server, &files));
    args.extend([
        "--data-dir".into(),
        backup.to_str().unwrap().into(),
        "--limit".into(),
        "10".into(),
        "--now-ms".into(),
        "500".into(),
    ]);
    args.extend(extra.iter().map(|s| s.to_string()));
    Command::new(env!("CARGO_BIN_EXE_recollect-hub"))
        .args(&args)
        .output()
        .unwrap()
}

fn run_center_cmd(dir: &Path, server: &FakeServer, hub_pub: &str, job: &str, extra: &[&str]) -> Output {
    let files = client_files(dir, hub_pub);
    let mut args: Vec<String> = vec!["replicate".into(), job.into()];
    args.extend(center_args(server, &files));
    args.extend(extra.iter().map(|s| s.to_string()));
    Command::new(env!("CARGO_BIN_EXE_recollect-hub"))
        .args(&args)
        .output()
        .unwrap()
}

fn fake(
    status: Vec<u8>,
    records: Vec<u8>,
    ledger: Vec<u8>,
    state: Vec<u8>,
    memory: Vec<u8>,
    hub_key: HubKey,
    acks: Acks,
) -> FakeCenter {
    FakeCenter {
        hub_key,
        status,
        records,
        ledger,
        state,
        memory,
        acks,
    }
}

#[test]
fn replicate_pull_cli_pulls_transcript_ledger_and_memory() {
    let dir = tempfile::tempdir().unwrap();
    let (status, records, ledger, state, memory, hub_key) = center_fixture(dir.path());
    let hub_pub = hub_key.public_b64();
    let server = FakeServer::start(fake(status, records, ledger, state, memory, hub_key, Acks::default()));
    let out = run_pull(dir.path(), &server, &hub_pub, &[]);

    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["pulled"], true);
    assert_eq!(doc["applied"], 1);
    assert_eq!(doc["applied_seq"], 1);
    assert_eq!(doc["lag"], 0);
    assert_eq!(doc["memory_applied"], true);
    let root = Root::open(&dir.path().join("backup")).unwrap();
    let turns: i64 = root
        .transcript()
        .conn()
        .query_row("SELECT count(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    let cards: i64 = root
        .memory()
        .conn()
        .query_row("SELECT count(*) FROM card", [], |r| r.get(0))
        .unwrap();
    let devices: i64 = root
        .ledger()
        .conn()
        .query_row("SELECT count(*) FROM device", [], |r| r.get(0))
        .unwrap();
    assert_eq!((turns, cards, devices), (1, 1, 1));
}

/// 中心说还有第 2 个 commit，但这一轮只拿到第 1 个：拉完 lag=1。
/// `--max-lag 0` 是调度器的告警口径——拉成功也要以退出 4 叫醒人。
#[test]
fn replicate_pull_cli_exits_4_when_lag_exceeds_max() {
    let dir = tempfile::tempdir().unwrap();
    let (_status, records, ledger, state, memory, hub_key) = center_fixture(dir.path());
    let hub_pub = hub_key.public_b64();
    let server = FakeServer::start(fake(
        json!({"last_commit_seq": 2, "epoch": 1}).to_string().into_bytes(),
        records,
        ledger,
        state,
        memory,
        hub_key,
        Acks::default(),
    ));
    let out = run_pull(dir.path(), &server, &hub_pub, &["--max-lag", "0"]);

    assert_eq!(
        out.status.code(),
        Some(4),
        "lag 超过阈值必须退出 4：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<&str> = std::str::from_utf8(&out.stdout).unwrap().lines().collect();
    let report: Value = serde_json::from_str(lines[0]).expect("第一行仍是这一轮的回执");
    assert_eq!(report["lag"], 1);
    let alert: Value = serde_json::from_str(lines[1]).expect("第二行是告警");
    assert_eq!(alert["error"], "lag_exceeded");
    assert_eq!(alert["lag"], 1);
    assert_eq!(alert["max_lag"], 0);
}

/// 拉完自动回执：回执报的是**本地已应用水位**，不是中心水位。
#[test]
fn replicate_pull_cli_acks_the_local_waterline() {
    let dir = tempfile::tempdir().unwrap();
    let (status, records, ledger, state, memory, hub_key) = center_fixture(dir.path());
    let hub_pub = hub_key.public_b64();
    let acks = Acks::default();
    let server = FakeServer::start(fake(status, records, ledger, state, memory, hub_key, acks.clone()));
    let out = run_pull(dir.path(), &server, &hub_pub, &["--ack"]);

    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let lines: Vec<&str> = std::str::from_utf8(&out.stdout).unwrap().lines().collect();
    assert_eq!(lines.len(), 2, "回执与回执行各一行：{:?}", lines);
    let acked: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(acked["acked"], true);
    assert_eq!(acked["applied_seq"], 1);
    assert_eq!(acked["replicated_seq"], 1);
    assert_eq!(acks.seen(), vec![1], "中心收到的回执必须是本地水位");
}

#[test]
fn replicate_ack_and_acked_cli_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let (status, records, ledger, state, memory, hub_key) = center_fixture(dir.path());
    let hub_pub = hub_key.public_b64();
    let acks = Acks::default();
    let server = FakeServer::start(fake(status, records, ledger, state, memory, hub_key, acks.clone()));

    let out = run_center_cmd(dir.path(), &server, &hub_pub, "ack", &["--applied-seq", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["acked"], true);
    assert_eq!(doc["epoch"], 1);
    assert_eq!(doc["applied_seq"], 1);
    assert_eq!(doc["replicated_seq"], 1);

    let out = run_center_cmd(dir.path(), &server, &hub_pub, "acked", &[]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["epoch"], 1);
    assert_eq!(doc["last_commit_seq"], 1);
    assert_eq!(doc["replicated_seq"], 1);
    assert_eq!(acks.seen(), vec![1]);
}
