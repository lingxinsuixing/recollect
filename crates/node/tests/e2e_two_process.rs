//! 双进程 E2E：pair → ingest → snapshot → sync → 两端逐 id 相同 → 两条短路 → 署名不符 → 倒退。
//!
//! 它起**真进程**（`recollect-hub` 与 `recollect-node` 两个 bin），不是线程里的两半。
//! 蒸馏链路由 rc-distill 的用例守：本文件在 hub 侧直接 `put_card` 播种记忆，
//! **传输的真实性**另由 hub 转录层行数与 `import_log` 行数证明。

use node::state;
use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_store::Root;
use serde_json::Value;
use std::collections::BTreeSet;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};

const HUB_CARDS: usize = 3;

fn node_bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-node")
}

/// cargo 只给**本包**的 bin 定义 `CARGO_BIN_EXE_*`。hub 与 node 同在一个 target 目录，
/// 所以按邻居找；并断它不比 hub 源码旧（`cargo test -p node` 不会重建 hub）。
fn hub_bin() -> PathBuf {
    let exe = Path::new(node_bin())
        .parent()
        .unwrap()
        .join(format!("recollect-hub{}", std::env::consts::EXE_SUFFIX));
    assert!(exe.is_file(), "找不到同目录的 recollect-hub 产物：请跑 cargo test --workspace");
    let built = exe.metadata().unwrap().modified().unwrap();
    let hub_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../hub");
    if let Some(newest) = newest_under(&hub_dir) {
        assert!(newest <= built, "hub 产物早于 hub 源码改动：先跑 cargo test --workspace");
    }
    exe
}

fn newest_under(dir: &Path) -> Option<SystemTime> {
    let mut best = None::<SystemTime>;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        let t = if p.is_dir() {
            newest_under(&p)
        } else {
            p.metadata().ok().and_then(|m| m.modified().ok())
        };
        if let Some(t) = t {
            best = Some(best.map_or(t, |b| b.max(t)));
        }
    }
    best
}

struct HubProc(Child);

impl Drop for HubProc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// TOML 里写 Windows 路径：反斜杠是转义符，统一换成 `/`。
fn toml_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn node_run(cfg: &Path, extra: &[&str]) -> Output {
    let mut cmd = Command::new(node_bin());
    cmd.arg("--config").arg(cfg).arg("--timeout-s").arg("10");
    for a in extra {
        cmd.arg(*a);
    }
    cmd.output().unwrap()
}

fn hub_run(data: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(hub_bin());
    for a in args {
        cmd.arg(*a);
    }
    cmd.arg("--data-dir").arg(data);
    cmd.output().unwrap()
}

fn wait_ready(port: u16, err_path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        if Instant::now() > deadline {
            let e = std::fs::read_to_string(err_path).unwrap_or_default();
            panic!("hub 没起来（端口 {port}）：{e}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn write_node_cfg(path: &Path, data: &Path, port: u16) {
    let text = format!(
        "data_dir = \"{}\"\ndevice_key_path = \"{}\"\n[[endpoints]]\nlabel = \"loopback\"\nhost = \"127.0.0.1\"\nport = {port}\n",
        toml_path(data),
        toml_path(&data.join("device.key")),
    );
    std::fs::write(path, text).unwrap();
}

/// 往节点本机转录层写两轮。uid 用**中心会重算的那个口径**（ContentOnly），
/// 否则中心落库的是另一个 uid，出处会悬空。
fn seed_turns(data: &Path, device_id: &str) -> Vec<String> {
    let root = rc_store::NodeRoot::open_node(data, None).unwrap();
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
        .map(|(i, body)| {
            let at = 100 + i as i64;
            rc_contract::NormalizedTurn {
                turn_uid: rc_contract::compute_turn_uid(device_id, "s1", "agent", body, at),
                body_fp: rc_contract::compute_body_fp(body),
                tier: rc_contract::TurnTier::ContentOnly,
                body: (*body).to_string(),
                role: "agent".into(),
                created_at_ms: at,
            }
        })
        .collect();
    let uids = turns.iter().map(|t| t.turn_uid.clone()).collect();
    rc_ledger::insert_batch(root.transcript(), &sid, &turns, 500).unwrap();
    uids
}

fn seed_hub_memory(data: &Path, turn_uid: &str) {
    let root = Root::open(data).unwrap();
    for (i, title) in ["decisions are captured", "local first", "no daemon"]
        .iter()
        .enumerate()
    {
        put_card(
            root.memory(),
            &FactContent {
                kind: "decision".into(),
                title: (*title).into(),
                body: format!("body {i}"),
                scope: Scope { kind: ScopeKind::Project, key: "proj-e2e".into() },
                valid_to_ms: None,
            },
            &[Provenance {
                turn_uid: turn_uid.to_string(),
                anchor_role: AnchorRole::Human,
            }],
            1_000 + i as i64,
        )
        .unwrap();
    }
}

fn hub_turns(data: &Path) -> i64 {
    let root = Root::open(data).unwrap();
    root.transcript()
        .conn()
        .query_row("SELECT count(*) FROM turns", [], |r| r.get(0))
        .unwrap()
}

fn import_log_rows(data: &Path) -> i64 {
    let root = Root::open(data).unwrap();
    root.ledger()
        .conn()
        .query_row("SELECT count(*) FROM import_log", [], |r| r.get(0))
        .unwrap()
}

fn read_ids(db: &Path) -> BTreeSet<String> {
    let m = rc_store::open_replica(db).unwrap();
    rc_memory::reader::facts(&m)
        .unwrap()
        .iter()
        .map(|r| r.key.key_string())
        .collect()
}

struct Fixture {
    /// 先声明：drop 顺序 = 声明顺序，hub 必须先被杀掉再删临时目录。
    _hub: HubProc,
    dir: tempfile::TempDir,
    hub_data: PathBuf,
    node_data: PathBuf,
    node_cfg: PathBuf,
    port: u16,
    hub_pub: String,
    turn_uids: Vec<String>,
    /// 端口是"绑一下再放"拿到的：两个用例并行时可能拿到同一个临时端口，配对就会打到
    /// 另一台的 hub 上（症状是 `not_paired`）。这一把锁把同一个测试二进制里的真进程
    /// 用例串起来；最后声明 ⇒ 最后释放，hub 与临时目录先退场。
    _serial: std::sync::MutexGuard<'static, ()>,
}

static PORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Fixture {
    fn new() -> Fixture {
        let serial = PORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let hub_data = dir.path().join("hub");
        let node_data = dir.path().join("node");
        std::fs::create_dir_all(&hub_data).unwrap();
        std::fs::create_dir_all(&node_data).unwrap();

        let hub_pub = rc_net::key::HubKey::load_or_create(&hub_data.join("hub.key"))
            .unwrap()
            .public_b64();

        let device = rc_net::key::DeviceKey::generate().unwrap();
        let device_id = device.device_id();
        device.save(&node_data.join("device.key")).unwrap();
        let turn_uids = seed_turns(&node_data, &device_id);
        seed_hub_memory(&hub_data, &turn_uids[0]);

        let port = free_port();
        let hub_cfg = dir.path().join("hub.toml");
        std::fs::write(
            &hub_cfg,
            format!(
                "[hub]\nbind = \"127.0.0.1:{port}\"\nendpoints = [\"http://127.0.0.1:{port}\"]\nmax_snapshot_bps = 1048576\n"
            ),
        )
        .unwrap();
        let node_cfg = dir.path().join("node.toml");
        write_node_cfg(&node_cfg, &node_data, port);

        let hub_err = dir.path().join("hub.err");

        // 配对码的签发不联网：先在 serve 起来之前做，少一个"两个进程同时打开账本"的窗口。
        let out = hub_run(&hub_data, &["pair"]);
        assert_eq!(out.status.code(), Some(0), "hub pair: {}", stderr(&out));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        let code = v["code"].as_str().unwrap().to_string();

        let err = std::fs::File::create(&hub_err).unwrap();
        let child = Command::new(hub_bin())
            .arg("serve")
            .arg("--hub-config")
            .arg(&hub_cfg)
            .arg("--data-dir")
            .arg(&hub_data)
            .arg("--dev-loopback")
            .stdout(Stdio::null())
            .stderr(Stdio::from(err))
            .spawn()
            .unwrap();
        let hub = HubProc(child);
        wait_ready(port, &hub_err);

        let out = node_run(&node_cfg, &["--code", &code]);
        assert_eq!(out.status.code(), Some(0), "node pair: {}", stderr(&out));

        Fixture {
            _hub: hub,
            dir,
            hub_data,
            node_data,
            node_cfg,
            port,
            hub_pub,
            turn_uids,
            _serial: serial,
        }
    }

    fn node(&self, extra: &[&str]) -> Output {
        node_run(&self.node_cfg, extra)
    }

    fn hub(&self, args: &[&str]) -> Output {
        hub_run(&self.hub_data, args)
    }
}

#[test]
fn both_ends_agree_on_every_row_id() {
    let f = Fixture::new();
    let out = f.node(&["--once"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["pushed"].as_i64(), Some(f.turn_uids.len() as i64), "{v}");
    assert_eq!(v["no_memory"].as_bool(), Some(true), "{v}");
    assert_eq!(hub_turns(&f.hub_data), f.turn_uids.len() as i64);
    assert_eq!(import_log_rows(&f.hub_data), 1);

    let out = f.hub(&["snapshot", "publish"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let published: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(published["published"].as_bool(), Some(true), "{published}");

    let out = f.node(&["--once"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["pulled"].as_bool(), Some(true), "{v}");
    assert_eq!(v["no_memory"].as_bool(), Some(false), "{v}");

    let replica = state::current_replica_path(&f.node_data).expect("下行之后必须有当前副本");
    let hub_ids = read_ids(&f.hub_data.join("memory.db"));
    let node_ids = read_ids(&replica);
    assert_eq!(hub_ids.len(), HUB_CARDS, "hub 侧播种的卡都应在：{hub_ids:?}");
    assert_eq!(hub_ids, node_ids, "两端必须逐 id 相同");
}

#[test]
fn a_second_sync_sends_nothing_again() {
    let f = Fixture::new();
    let out = f.node(&["--once"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["pushed"].as_i64(), Some(f.turn_uids.len() as i64), "{v}");
    let before = import_log_rows(&f.hub_data);

    let out = f.node(&["--once"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["pushed"].as_i64(), Some(0), "{v}");
    assert_eq!(v["push_skipped"].as_bool(), Some(true), "{v}");
    assert_eq!(import_log_rows(&f.hub_data), before, "没有新轮次就不许再记一次导入");
}

#[test]
fn a_second_sync_downloads_nothing_again() {
    let f = Fixture::new();
    let out = f.node(&["--once"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let out = f.hub(&["snapshot", "publish"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let out = f.node(&["--once"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["pulled"].as_bool(), Some(true), "{v}");

    let replica = state::current_replica_path(&f.node_data).expect("第一代副本");
    let sha_before = rc_net::protocol::sha256_hex(&std::fs::read(&replica).unwrap());
    let mtime_before = std::fs::metadata(&replica).unwrap().modified().unwrap();

    let out = f.node(&["--once"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["pulled"].as_bool(), Some(false), "{v}");
    assert_eq!(v["pull_skipped"].as_bool(), Some(true), "{v}");

    let sha_after = rc_net::protocol::sha256_hex(&std::fs::read(&replica).unwrap());
    let mtime_after = std::fs::metadata(&replica).unwrap().modified().unwrap();
    assert_eq!(sha_before, sha_after, "同一份清单不许重下");
    assert_eq!(mtime_before, mtime_after, "重下同一份字节也不许——只有 mtime 看得出它重下了");
}

#[test]
fn a_request_signed_by_another_key_is_refused() {
    let f = Fixture::new();
    // 先让合法节点同步一轮：中心有行，"被拒的批次一行都不许落"才有阳性对照。
    let out = f.node(&["--once"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(hub_turns(&f.hub_data), f.turn_uids.len() as i64);
    let other_data = f.dir.path().join("node2");
    std::fs::create_dir_all(&other_data).unwrap();
    let other = rc_net::key::DeviceKey::generate().unwrap();
    let other_id = other.device_id();
    other.save(&other_data.join("device.key")).unwrap();
    std::fs::copy(
        state::token_path(&f.node_data),
        state::token_path(&other_data),
    )
    .unwrap();
    let st = state::SyncState {
        paired_hub_pub: f.hub_pub.clone(),
        ..Default::default()
    };
    state::save(&state::state_path(&other_data), &st).unwrap();
    seed_turns(&other_data, &other_id);
    let cfg = f.dir.path().join("node2.toml");
    write_node_cfg(&cfg, &other_data, f.port);

    let out = node_run(&cfg, &["--once"]);
    assert_eq!(out.status.code(), Some(3), "stderr={}", stderr(&out));
    let st = state::load(&state::state_path(&other_data)).unwrap().unwrap();
    assert_eq!(st.last_error, "not_paired");
    assert_eq!(
        hub_turns(&f.hub_data),
        f.turn_uids.len() as i64,
        "被拒的批次一行都不许落"
    );
}

#[test]
fn an_older_manifest_is_refused() {
    let f = Fixture::new();
    let out = f.node(&["--once"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let out = f.hub(&["snapshot", "publish"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let published: Value = serde_json::from_slice(&out.stdout).unwrap();
    let epoch = published["epoch"].as_u64().expect("publish 要报 epoch");
    let seq = published["seq"].as_u64().expect("publish 要报 seq");
    let out = f.node(&["--once"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let replica = state::current_replica_path(&f.node_data).expect("第一代副本");

    let path = state::state_path(&f.node_data);
    let mut st = state::load(&path).unwrap().unwrap();
    // 中心的 epoch 是随机 63 位：要造"节点以为自己的清单更新"，只能拿实际号 +1。
    st.current_manifest.epoch = epoch + 1;
    st.current_manifest.sha256 = "a".repeat(64);
    state::save(&path, &st).unwrap();

    let out = f.node(&["--once"]);
    assert_eq!(out.status.code(), Some(3), "stderr={}", stderr(&out));
    let st = state::load(&path).unwrap().unwrap();
    assert_eq!(st.last_error, "bad_response");
    assert!(replica.is_file(), "拒收不许删掉已经落地的那一代");
    assert_eq!(st.current_manifest.epoch, epoch + 1);
    assert_eq!(st.current_manifest.seq, seq);
}
