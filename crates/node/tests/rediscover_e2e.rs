//! S4d1：自动重连的**真进程** E2E。
//!
//! 起真 `recollect-hub serve --dev-loopback --tls --discovery-port DP`，凭票配对一台节点，
//! 然后把中心停掉、换地址再起，看节点能不能**自己跟过去**。三件事：
//!
//! 1. 换端口能自己跟过去：新地址上的中心真的答了请求（`no_memory` 只能由它答），
//!    `status` 里 `rediscover_ok=1`、`paired_endpoints` 指向新端口；
//! 2. 外来中心不被采纳：另起一台**新数据目录**的中心，节点仍然报 `endpoint_unreachable`，
//!    端点一条不变；
//! 3. 同钥匙换证书要响：同一数据目录、空 `tls` 目录再起（hub key 相同、证书不同）⇒
//!    `identity_mismatch`，端点一条不变。
//!
//! 三条用例都会起两个进程并抢同一段回环端口，所以用一把静态锁串起来。

use node::state;
use rc_store::Root;
use serde_json::Value;
use std::net::{TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::MutexGuard;
use std::time::{Duration, Instant, SystemTime};

fn node_bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-node")
}

/// 同 `invite_e2e.rs`：cargo 只给本包的 bin 定义 `CARGO_BIN_EXE_*`，hub 按邻居找。
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

fn free_tcp_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// 发现端口是 UDP：用 UDP 现绑现放找空位，别拿 TCP 的端口去猜。
fn free_udp_port() -> u16 {
    let u = UdpSocket::bind("127.0.0.1:0").unwrap();
    u.local_addr().unwrap().port()
}

fn toml_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
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

/// HTTP 监听通了不代表发现线程已经绑上 UDP（两条独立线程）。用 `rc-net` 自己的探针等它，
/// 而不是猜一个 sleep：这几条用例的判据全落在"扫得到"上。
/// 探针要有一个真钟面：`discover` 不取钟（判据 12a），窗口得由调用方给。
fn wall_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after the unix epoch")
        .as_millis() as i64
}

fn wait_discovery(port: u16) {
    let target = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(v) = rc_net::discovery::discover(&[target], Duration::from_millis(200), wall_now_ms()) {
            if !v.is_empty() {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("hub 的发现应答没起来（UDP {port}）");
}

/// 公告的端点表与 bind 是**两件事**（中心按 `hub.toml` 的 `endpoints` 应答，不看真实
/// 监听的地址）。重试失败的用例靠这个造"公告了一个没人听的地址"。
fn write_hub_cfg_with(path: &Path, port: u16, endpoints: &[String]) {
    let list = endpoints.iter().map(|e| format!("\"{e}\"")).collect::<Vec<_>>().join(", ");
    std::fs::write(
        path,
        format!(
            "[hub]\nbind = \"127.0.0.1:{port}\"\nendpoints = [{list}]\nmax_snapshot_bps = 1048576\n"
        ),
    )
    .unwrap();
}

/// 节点配置：只有 `data_dir`、钥匙与**发现目标**。没有端点、没有 `hub_tls_fp`——
/// 地址与指纹必须全部来自票，于是"节点自己跟过去"这件事才没有被配置帮过忙。
fn write_node_cfg(path: &Path, data: &Path, discovery_port: u16) {
    std::fs::write(
        path,
        format!(
            "data_dir = \"{}\"\ndevice_key_path = \"{}\"\n\
             discovery_target = \"127.0.0.1\"\ndiscovery_port = {discovery_port}\n",
            toml_path(data),
            toml_path(&data.join("device.key")),
        ),
    )
    .unwrap();
}

/// 带一条**手写**端点的节点配置（R7.6 的合并用例）：发现目标仍是本机、随机端口，
/// 端点指向一个（可能已经死掉的）回环地址。
fn write_node_cfg_with_endpoint(
    path: &Path,
    data: &Path,
    discovery_port: u16,
    label: &str,
    port: u16,
) {
    std::fs::write(
        path,
        format!(
            "data_dir = \"{}\"\ndevice_key_path = \"{}\"\n\
             discovery_target = \"127.0.0.1\"\ndiscovery_port = {discovery_port}\n\
             [[endpoints]]\nlabel = \"{label}\"\nhost = \"127.0.0.1\"\nport = {port}\n",
            toml_path(data),
            toml_path(&data.join("device.key")),
        ),
    )
    .unwrap();
}

fn seed_turns(data: &Path, device_id: &str) -> usize {    let root = rc_store::NodeRoot::open_node(data, None).unwrap();
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
    let n = turns.len();
    rc_ledger::insert_batch(root.transcript(), &sid, &turns, 500).unwrap();
    n
}

fn hub_turns(data: &Path) -> i64 {
    let root = Root::open(data).unwrap();
    root.transcript()
        .conn()
        .query_row("SELECT count(*) FROM turns", [], |r| r.get(0))
        .unwrap()
}

fn hub_devices(data: &Path) -> i64 {
    let root = Root::open(data).unwrap();
    root.ledger()
        .conn()
        .query_row("SELECT count(*) FROM device", [], |r| r.get(0))
        .unwrap()
}

fn status_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("status 不是一行 JSON（{e}）：{}", stdout(out)))
}

fn load_state(data: &Path) -> state::SyncState {
    state::load(&state::state_path(data)).unwrap().expect("配对之后必须有状态文件")
}

/// 一局：一份临时目录、一台中心的数据目录、一台节点的数据目录与配置。
///
/// 中心可以被杀掉再换地址起起来，所以 `HubProc` 不挂在这个结构上——每条用例自己拿。
struct Env {
    _serial: MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
    hub_data: PathBuf,
    node_data: PathBuf,
    node_cfg: PathBuf,
    device_id: String,
}

static PORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Env {
    fn new(node_discovery_port: u16) -> Env {
        let serial = PORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let hub_data = dir.path().join("hub");
        let node_data = dir.path().join("node");
        std::fs::create_dir_all(&hub_data).unwrap();
        std::fs::create_dir_all(&node_data).unwrap();
        let key = rc_net::key::DeviceKey::generate().unwrap();
        key.save(&node_data.join("device.key")).unwrap();
        let device_id = key.device_id();
        let node_cfg = dir.path().join("node.toml");
        write_node_cfg(&node_cfg, &node_data, node_discovery_port);
        Env { _serial: serial, dir, hub_data, node_data, node_cfg, device_id }
    }

    /// 起一台中心，等 HTTP 与发现两条线程都就绪。返回进程句柄与 stderr 路径。
    fn start_hub(&self, data: &Path, cfg: &Path, port: u16, discovery_port: u16, tag: &str) -> (HubProc, PathBuf) {
        let announced = vec![format!("https://127.0.0.1:{port}")];
        self.start_hub_announcing(data, cfg, port, &announced, discovery_port, tag)
    }

    /// 同上，但**公告的端点表**由调用方给（与真实 bind 无关）。
    fn start_hub_announcing(
        &self,
        data: &Path,
        cfg: &Path,
        port: u16,
        announced: &[String],
        discovery_port: u16,
        tag: &str,
    ) -> (HubProc, PathBuf) {
        write_hub_cfg_with(cfg, port, announced);
        let err = self.dir.path().join(format!("hub-{tag}.err"));
        let out = self.dir.path().join(format!("hub-{tag}.out"));
        let child = Command::new(hub_bin())
            .arg("serve")
            .arg("--hub-config")
            .arg(cfg)
            .arg("--data-dir")
            .arg(data)
            .arg("--dev-loopback")
            .arg("--tls")
            .arg("--discovery-port")
            .arg(discovery_port.to_string())
            .stdout(Stdio::from(std::fs::File::create(&out).unwrap()))
            .stderr(Stdio::from(std::fs::File::create(&err).unwrap()))
            .spawn()
            .unwrap();
        let hub = HubProc(child);
        wait_ready(port, &err);
        wait_discovery(discovery_port);
        (hub, err)
    }

    fn invite(&self, cfg: &Path, data: &Path) -> Value {
        let out = Command::new(hub_bin())
            .arg("invite")
            .arg("--json")
            .arg("--hub-config")
            .arg(cfg)
            .arg("--data-dir")
            .arg(data)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "hub invite: {}", stderr(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn node(&self, extra: &[&str]) -> Output {
        node_run(&self.node_cfg, extra)
    }

    fn pair(&self, hub_cfg: &Path, hub_data: &Path) {
        let invite = self.invite(hub_cfg, hub_data);
        let ticket = invite["ticket"].as_str().unwrap();
        let out = self.node(&["--invite", ticket]);
        assert_eq!(out.status.code(), Some(0), "凭票加入必须成功：{}", stderr(&out));
    }
}

#[test]
fn the_node_follows_the_center_to_a_new_port() {
    let dp2 = free_udp_port();
    let env = Env::new(dp2);
    assert_eq!(seed_turns(&env.node_data, &env.device_id), 2);

    let p = free_tcp_port();
    let dp1 = free_udp_port();
    let cfg_p = env.dir.path().join("hub-p.toml");
    let (hub_p, _) = env.start_hub(&env.hub_data, &cfg_p, p, dp1, "p");
    env.pair(&cfg_p, &env.hub_data);
    let before = status_json(&env.node(&[]));
    let st = load_state(&env.node_data);
    assert_eq!(st.paired_endpoints, vec![format!("https://127.0.0.1:{p}")], "配对时地址来自票");

    // 中心换端口：同一个数据目录（同一把 hub key、同一枚证书），新的 hub.toml 只改地址。
    drop(hub_p);
    let q = free_tcp_port();
    assert_ne!(q, p, "新地址必须真的不同");
    let cfg_q = env.dir.path().join("hub-q.toml");
    let (_hub_q, _) = env.start_hub(&env.hub_data, &cfg_q, q, dp2, "q");
    let mid = status_json(&env.node(&[]));
    assert_eq!(mid["rediscover_ok"].as_u64(), Some(0), "还没跑同步：{mid}");
    assert_eq!(
        load_state(&env.node_data).paired_endpoints,
        vec![format!("https://127.0.0.1:{p}")],
        "同步之前端点不许自己变"
    );

    // 一次同步：旧端点一条都不通 ⇒ 问一句 ⇒ 换到新地址重试**这一轮**。
    let out = env.node(&["--once"]);
    assert_eq!(out.status.code(), Some(0), "自动重连必须自己跟上：{}", stderr(&out));
    let sync: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(sync["pushed"].as_i64(), Some(2), "重试那一轮要把上行真发到新地址：{sync}");
    assert_eq!(sync["no_memory"].as_bool(), Some(true), "404 只能由新地址上的中心答：{sync}");
    assert_eq!(sync["endpoint"].as_str(), Some("paired-1"), "{sync}");

    let after = status_json(&env.node(&[]));
    assert_eq!(after["rediscover_ok"].as_u64(), Some(1), "{after}");
    assert_eq!(after["rediscover_miss"].as_u64(), Some(0), "{after}");
    assert!(after["last_rediscover_at_ms"].as_i64().unwrap() > 0, "{after}");
    assert_eq!(after["endpoint"].as_str(), Some("paired-1"), "{after}");
    // R9：读数面要说清"我钉的是谁、走不走 TLS、有几条路"。地址不印全文。
    let paired_state = load_state(&env.node_data);
    assert_eq!(after["paired"].as_bool(), Some(true), "配对过就是在：{after}");
    assert_eq!(after["hub_fp"].as_str(), Some(paired_state.hub_fp().as_str()), "{after}");
    assert_eq!(after["hub_fp"].as_str().map(str::len), Some(64), "{after}");
    assert_eq!(
        after["tls_fp"].as_str(),
        paired_state.paired_tls_fp.as_deref(),
        "有效指纹 = 状态里那一枚（配置没写）：{after}"
    );
    assert_eq!(after["transport"].as_str(), Some("tls"), "{after}");
    assert_eq!(after["endpoints"].as_u64(), Some(1), "只印条数：{after}");
    assert!(after.get("paired_endpoints").is_none(), "地址清单不许整段印出来：{after}");
    assert_eq!(after["last_error"].as_str(), Some(""), "成功一轮必须清掉上次的码：{after}");
    assert_eq!(load_state(&env.node_data).paired_endpoints, vec![format!("https://127.0.0.1:{q}")]);
    assert_eq!(hub_turns(&env.hub_data), 2, "上行落在了这台中心上");

    // 再跑一轮：地址已经是新的，发现不该再发生（计数不涨）——一轮最多发现一次。
    let out = env.node(&["--once"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let again = status_json(&env.node(&[]));
    assert_eq!(again["rediscover_ok"].as_u64(), Some(1), "发现只发生在换地址那一轮：{again}");
    assert_eq!(again["last_rediscover_at_ms"], after["last_rediscover_at_ms"], "{again}");

    println!("S4d1 before={before}");
    println!("S4d1 after={after}");
    println!("S4d1 again={again}");
}

#[test]
fn a_foreign_center_is_not_adopted() {
    let dp2 = free_udp_port();
    let env = Env::new(dp2);
    let p = free_tcp_port();
    let dp1 = free_udp_port();
    let cfg_a = env.dir.path().join("hub-a.toml");
    let (hub_a, _) = env.start_hub(&env.hub_data, &cfg_a, p, dp1, "a");
    env.pair(&cfg_a, &env.hub_data);
    let paired = load_state(&env.node_data).paired_endpoints;
    assert_eq!(paired, vec![format!("https://127.0.0.1:{p}")]);

    // A 停掉；另起一台**全新数据目录**的中心到 Q（不同 hub key、不同证书）。
    drop(hub_a);
    let b_data = env.dir.path().join("hub-b");
    std::fs::create_dir_all(&b_data).unwrap();
    let q = free_tcp_port();
    let cfg_b = env.dir.path().join("hub-b.toml");
    let (_hub_b, _) = env.start_hub(&b_data, &cfg_b, q, dp2, "b");

    let out = env.node(&["--once"]);
    assert_eq!(out.status.code(), Some(3), "外来中心不该被采纳：{}", stderr(&out));
    assert!(
        stderr(&out).contains("endpoint_unreachable"),
        "错误码必须是 endpoint_unreachable：{}",
        stderr(&out)
    );
    let st = load_state(&env.node_data);
    assert_eq!(st.last_error, "endpoint_unreachable");
    assert_eq!(st.paired_endpoints, paired, "端点一条都不许变");
    assert_eq!(st.rediscover_ok, 0, "没有换过地址");
    assert!(st.rediscover_miss >= 1, "问过一句但没找到我那台：{}", st.rediscover_miss);

    // 旁证：B 的库里一条上行、一台设备都没有。
    // 严格证明"一个包都没落到 B"要在中间架一个计数代理，那不在本轮的边界里；
    // 真正的牙在 `rediscover_pick.rs` 的"外来中心被跳过"与"只撞指纹不算信任"两条。
    assert_eq!(hub_turns(&b_data), 0, "外来中心不该收到任何上行");
    assert_eq!(hub_devices(&b_data), 0, "外来中心不该出现这台设备");
}

#[test]
fn the_same_key_with_a_new_certificate_is_a_loud_failure() {
    let dp2 = free_udp_port();
    let env = Env::new(dp2);
    let p = free_tcp_port();
    let dp1 = free_udp_port();
    let cfg_p = env.dir.path().join("hub-p.toml");
    let (hub_p, _) = env.start_hub(&env.hub_data, &cfg_p, p, dp1, "p");
    env.pair(&cfg_p, &env.hub_data);
    let old_state = load_state(&env.node_data);
    let old_fp = old_state.paired_tls_fp.clone().expect("凭票配对必须钉下证书指纹");
    let paired = old_state.paired_endpoints.clone();

    // 停中心、把 TLS 身份挪走（key 留下）：同一个数据目录里 `<data-dir>/tls` 空了，
    // `ensure_identity` 会在新地址上重新生成一对**新证书**，hub key 还是原来那把。
    drop(hub_p);
    std::fs::rename(env.hub_data.join("tls"), env.hub_data.join("tls-before-rotation")).unwrap();
    let q = free_tcp_port();
    let cfg_q = env.dir.path().join("hub-q.toml");
    let (_hub_q, _) = env.start_hub(&env.hub_data, &cfg_q, q, dp2, "q");
    let shown = hub_run(&env.hub_data, &["tls", "show", "--json"]);
    assert_eq!(shown.status.code(), Some(0), "tls show: {}", stderr(&shown));
    let now_fp = serde_json::from_slice::<Value>(&shown.stdout).unwrap()["tls_fingerprint"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(now_fp, old_fp, "换一个空 tls 目录之后证书必须真的换了");

    let out = env.node(&["--once"]);
    assert_eq!(out.status.code(), Some(3), "同钥匙换证书必须响亮失败：{}", stderr(&out));
    assert!(
        stderr(&out).contains("identity_mismatch"),
        "错误码必须是 identity_mismatch：{}",
        stderr(&out)
    );
    let st = load_state(&env.node_data);
    assert_eq!(st.last_error, "identity_mismatch");
    assert_eq!(st.paired_endpoints, paired, "响亮失败时端点一条都不许变");
    assert_eq!(st.rediscover_ok, 0, "不许把新地址当成可重试的地址");
}

/// R7.3：发现回来的地址**没跑通**时，旧地址簿一条都不许换，只记一次 miss，
/// 并原样返回重试的那个错误（折成别的码会把"新地址不通"读成"没发现"）。
#[test]
fn a_retry_that_fails_keeps_the_old_book_and_counts_a_miss() {
    let dp2 = free_udp_port();
    let env = Env::new(dp2);
    let p = free_tcp_port();
    let dp1 = free_udp_port();
    let cfg_p = env.dir.path().join("hub-p.toml");
    let (hub_p, _) = env.start_hub(&env.hub_data, &cfg_p, p, dp1, "p");
    env.pair(&cfg_p, &env.hub_data);
    let book = load_state(&env.node_data).paired_endpoints;
    assert_eq!(book, vec![format!("https://127.0.0.1:{p}")]);

    // 中心换到一个新端口，但公告的端点表指向一个**没人听**的地址（`endpoints` 与
    // 真实 bind 脱钩）。发现会采纳它，重试却连不上。
    drop(hub_p);
    let dead = free_tcp_port();
    let q = free_tcp_port();
    let cfg_q = env.dir.path().join("hub-q.toml");
    let (_hub_q, _) = env.start_hub_announcing(
        &env.hub_data,
        &cfg_q,
        q,
        &[format!("https://127.0.0.1:{dead}")],
        dp2,
        "q",
    );

    let out = env.node(&["--once"]);
    assert_eq!(out.status.code(), Some(3), "新地址不通就是这一轮失败：{}", stderr(&out));
    assert!(
        stderr(&out).contains("endpoint_unreachable"),
        "返回的必须是重试那个错误：{}",
        stderr(&out)
    );
    let st = load_state(&env.node_data);
    assert_eq!(st.paired_endpoints, book, "没跑通的新地址不许换掉地址簿");
    assert_eq!(st.rediscover_ok, 0, "没跑通不算重发现成功");
    assert_eq!(st.rediscover_miss, 1, "问过一句、采纳了一个不通的地址：只记一次 miss");
    assert!(st.last_rediscover_at_ms > 0, "真的发过 probe ⇒ 时刻要写");
}

/// R7.6：配置端点与状态端点**合并**（配置在前），发现结果下一次不再重复发现。
/// 配置里写的是旧地址、状态里是发现来的新地址：两条都试，计数不涨。
#[test]
fn a_configured_endpoint_and_a_discovered_one_merge_and_converge() {
    let dp2 = free_udp_port();
    let env = Env::new(dp2);
    let p = free_tcp_port();
    let dp1 = free_udp_port();
    let cfg_p = env.dir.path().join("hub-p.toml");
    let (hub_p, _) = env.start_hub(&env.hub_data, &cfg_p, p, dp1, "p");
    env.pair(&cfg_p, &env.hub_data);

    // 运维把**当时的**地址手写进 node.toml；稍后中心换端口。
    write_node_cfg_with_endpoint(&env.node_cfg, &env.node_data, dp2, "lan-old", p);
    let cfg = node::node_cfg::load(&env.node_cfg).unwrap().expect("配置可读");
    assert_eq!(cfg.endpoints.len(), 1);

    drop(hub_p);
    let q = free_tcp_port();
    let cfg_q = env.dir.path().join("hub-q.toml");
    let (_hub_q, _) = env.start_hub(&env.hub_data, &cfg_q, q, dp2, "q");

    // 第一轮：配置里的旧地址不通 ⇒ 发现 ⇒ 换到新地址重试成功。
    let out = env.node(&["--once"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let first = status_json(&env.node(&[]));
    assert_eq!(first["rediscover_ok"].as_u64(), Some(1), "{first}");
    assert_eq!(first["rediscover_miss"].as_u64(), Some(0), "{first}");
    assert_eq!(load_state(&env.node_data).paired_endpoints, vec![format!("https://127.0.0.1:{q}")]);
    // 有效端点 = 配置 1 条 + 状态里没出现过的那 1 条。
    assert_eq!(first["endpoints"].as_u64(), Some(2), "{first}");

    // 第二轮：旧地址仍在前（配置优先），试过之后走状态里那条新地址；**不再发现**。
    let out = env.node(&["--once"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let again = status_json(&env.node(&[]));
    assert_eq!(again["rediscover_ok"].as_u64(), Some(1), "发现只发生在换地址那一轮：{again}");
    assert_eq!(again["rediscover_miss"].as_u64(), Some(0), "{again}");
    assert_eq!(
        again["last_rediscover_at_ms"], first["last_rediscover_at_ms"],
        "第二轮根本没问一句：{again}"
    );
    println!("R7.6 round1={first}");
    println!("R7.6 round2={again}");
}

/// R16：`--discover` 的发现层只绑 `0.0.0.0`，IPv6 目标是**用法错**（exit 3），
/// 不是"中心不可达"。不读配置：`--discover` 在配对之前就可以跑。
#[test]
fn an_ipv6_discovery_target_is_a_usage_error() {
    let out = Command::new(node_bin())
        .args(["--discover", "--discovery-target", "::1"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "IPv6 目标必须退 3：{}", stderr(&out));
    assert!(stderr(&out).contains("IPv4 only"), "错误串要写明 IPv4 only：{}", stderr(&out));
}

/// R16：空表时在 stderr 印一行"问了几处、等了多久、没人答"，**不印地址**；
/// stdout 仍是那唯一一行 JSON（exit 0：一片没有中心的网是正常读数）。
#[test]
fn an_empty_discovery_scan_prints_a_reading_on_stderr() {
    let dead = free_udp_port();
    let out = Command::new(node_bin())
        .args([
            "--discover",
            "--discovery-target",
            "127.0.0.1",
            "--discovery-port",
            &dead.to_string(),
            "--timeout-s",
            "1",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "空表不是失败：{}", stderr(&out));
    let e = stderr(&out);
    assert!(e.contains("discovery: 1 target, 1000ms, no candidate"), "读数行：{e}");
    assert!(!e.contains("127.0.0.1"), "不许印地址：{e}");
    let v: Value = serde_json::from_slice(&out.stdout).expect("stdout 仍是一行 JSON");
    assert_eq!(v["candidates"].as_array().map(Vec::len), Some(0), "{v}");
}
