//! 真链路：真 `HubState` 起在真实 TCP 上，客户端是 `recollect-hub` 真进程。
//!
//! `replicate_pull_cli` 的假中心不验签，`replicate_ack` 的调用在进程内；
//! 这一条把两者接起来：签名、epoch 头、HTTP 状态、CLI 退出码都在同一条链上。

use hub::hub_cfg::HubConfig;
use hub::serve::HubState;
use rc_contract::{compute_body_fp, compute_turn_uid, NormalizedTurn, SessionId, TurnTier};
use rc_ledger::transcript::insert_batch_committed;
use rc_net::http::Limits;
use rc_net::key::{DeviceKey, HubKey};
use rc_store::Root;
use serde_json::Value;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

const NOW: i64 = 1_000;

struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Server {
    fn start(mut state: HubState) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let join = std::thread::spawn(move || {
            rc_net::http::serve(listener, &mut state, &stop_thread, Limits::default(), 0);
        });
        Server {
            addr,
            stop,
            join: Some(join),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn center(data: &Path) -> Option<HubState> {
    let root = Root::open(data).ok()?;
    let key = HubKey::load_or_create(&data.join("hub.key")).ok()?;
    let cfg = HubConfig {
        bind: "127.0.0.1:0".into(),
        endpoints: vec!["http://127.0.0.1:0".into()],
        max_snapshot_bps: 4096,
    };
    Some(HubState::new(root, key, cfg, data, Box::new(|| NOW)))
}

/// 直接写账本配对（不经 HTTP），返回设备凭据文件与 `device_id`。
fn pair_device(data: &Path, dir: &Path, tag: &str) -> (std::path::PathBuf, std::path::PathBuf, String) {
    let dev = DeviceKey::generate().unwrap();
    let device_id = dev.device_id();
    let key_path = dir.join(format!("{tag}.key"));
    dev.save(&key_path).unwrap();
    let (code, _) = rc_net::pair::new_pair_code().unwrap();
    let hash = rc_net::pair::hash_pair_code(&code).unwrap();
    let (token, token_hash) = rc_net::pair::new_token().unwrap();
    let token_path = dir.join(format!("{tag}.token"));
    std::fs::write(&token_path, format!("{token}\n")).unwrap();
    let mut root = Root::open(data).unwrap();
    rc_ledger::device::issue_pair_code(root.ledger_mut(), &hash, NOW, NOW + 300_000).unwrap();
    rc_ledger::device::pair(
        root.ledger_mut(),
        &hash,
        &device_id,
        &dev.public_b64(),
        &token_hash,
        NOW,
    )
    .unwrap();
    (key_path, token_path, device_id)
}

fn run(
    server: &Server,
    hub_pub: &Path,
    key: &Path,
    token: &Path,
    job: &[&str],
    extra: &[&str],
) -> Output {
    let mut args: Vec<String> = vec!["replicate".into()];
    args.extend(job.iter().map(|s| s.to_string()));
    args.extend([
        "--endpoint".into(),
        format!("127.0.0.1:{}", server.addr.port()),
        "--token-file".into(),
        token.to_str().unwrap().into(),
        "--device-key-file".into(),
        key.to_str().unwrap().into(),
        "--hub-pub-file".into(),
        hub_pub.to_str().unwrap().into(),
    ]);
    args.extend(extra.iter().map(|s| s.to_string()));
    Command::new(env!("CARGO_BIN_EXE_recollect-hub"))
        .args(&args)
        .output()
        .unwrap()
}

#[test]
fn real_center_and_real_client_agree_on_pull_ack_and_read() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("center");
    Root::open(&data).unwrap();
    // 一条已提交的 turn：中心有东西可复制，replica_ack 才有可回执的水位。
    let mut root = Root::open(&data).unwrap();
    let (session, turn) = (
        SessionId {
            session_key: "s-1".into(),
            device_id: "dev-1".into(),
            source_agent: "qoder".into(),
            machine: None,
            project_hint: None,
            last_saved_at_ms: None,
            owner_principal_id: None,
            principal_name: None,
            principal_source: None,
        },
        {
            let body = "alpha";
            NormalizedTurn {
                tier: TurnTier::ContentOnly,
                turn_uid: compute_turn_uid("dev-1", "s-1", "human", body, 100),
                body: body.into(),
                body_fp: compute_body_fp(body),
                role: "human".into(),
                created_at_ms: 100,
            }
        },
    );
    insert_batch_committed(root.transcript_mut(), &session, &[turn], 100, "import").unwrap();
    drop(root);

    let hub_key = HubKey::load_or_create(&data.join("hub.key")).unwrap();
    let hub_pub_path = dir.path().join("hub.pub");
    std::fs::write(&hub_pub_path, format!("{}\n", hub_key.public_b64())).unwrap();
    let server = Server::start(center(&data).expect("center state"));

    let (backup_key, backup_token, backup_id) = pair_device(&data, dir.path(), "backup");
    let (writer_key, writer_token, _writer_id) = pair_device(&data, dir.path(), "writer");
    rc_ledger::device::grant_capability(
        Root::open(&data).unwrap().ledger_mut(),
        &backup_id,
        rc_ledger::device::CAPABILITY_REPLICATE,
        NOW,
    )
    .unwrap();
    let backup_dir = dir.path().join("backup-data");

    let out = run(
        &server,
        &hub_pub_path,
        &backup_key,
        &backup_token,
        &["pull"],
        &[
            "--data-dir",
            backup_dir.to_str().unwrap(),
            "--limit",
            "10",
            "--now-ms",
            "500",
            "--ack",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let lines: Vec<&str> = std::str::from_utf8(&out.stdout).unwrap().lines().collect();
    assert_eq!(lines.len(), 2, "{:?}", lines);
    let pulled: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(pulled["applied"], 1);
    assert_eq!(pulled["lag"], 0);
    let acked: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(acked["acked"], true);
    assert_eq!(acked["replicated_seq"], 1);

    // 真中心侧回了什么：回执行在中心库里，水位就是这一格。
    let root = Root::open(&data).unwrap();
    let (seq, epoch): (i64, i64) = root
        .transcript()
        .conn()
        .query_row(
            "SELECT applied_seq, epoch FROM replica_ack WHERE device_id=?1",
            [&backup_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((seq, epoch), (1, 1));
    drop(root);

    // 普通设备（writer）也能读合并水位。
    let out = run(&server, &hub_pub_path, &writer_key, &writer_token, &["acked"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let seen: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(seen["epoch"], 1);
    assert_eq!(seen["last_commit_seq"], 1);
    assert_eq!(seen["replicated_seq"], 1);

    // 操作面能看到逐备份回执，以及“在编且新鲜”。
    let out = run(&server, &hub_pub_path, &backup_key, &backup_token, &["acks"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let acks: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(acks["replicated_seq"], 1);
    assert_eq!(acks["acks"][0]["device_id"].as_str(), Some(backup_id.as_str()));
    assert_eq!(acks["acks"][0]["applied_seq"], 1);
    assert_eq!(acks["acks"][0]["active"], true);
    assert_eq!(acks["acks"][0]["fresh"], true);

    // 超前中心必须被真服务端拒绝，CLI 以 3 退出且 stdout 为空。
    let out = run(
        &server,
        &hub_pub_path,
        &backup_key,
        &backup_token,
        &["ack"],
        &["--applied-seq", "2"],
    );
    assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty());

    // 撤销备份后，它的旧回执不再替设备作保。
    rc_ledger::device::revoke(
        Root::open(&data).unwrap().ledger_mut(),
        &backup_id,
        NOW + 1,
    )
    .unwrap();
    let out = run(&server, &hub_pub_path, &writer_key, &writer_token, &["acked"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let seen: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(seen["replicated_seq"], 0);
}
