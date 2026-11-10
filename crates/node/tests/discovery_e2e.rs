//! S4c2：`recollect-node --discover` 的**真进程** E2E。
//!
//! 起真 `recollect-hub serve --dev-loopback --tls --discovery-port <free>`，用
//! `hub invite --json` 取 `hub_fp/tls_fp`（只出票，不配对、不耗票），再让节点扫本机。
//!
//! 四件事：候选与中心签的那一份逐字段对得上、`--expect-fp` 收得准/放得准（错指纹退 4）、
//! `--discover` 不需要 `node.toml`、坏的 `--expect-fp` 是退 3 的静态用法错。

use serde_json::Value;
use std::net::{SocketAddr, TcpListener, UdpSocket};
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

fn node(args: &[&str]) -> Output {
    let mut cmd = Command::new(node_bin());
    for a in args {
        cmd.arg(*a);
    }
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

/// HTTP 监听通了不代表发现线程已经绑上 UDP（两条独立线程）。用 `rc-net` 自己的探针
/// 等它，而不是猜一个 sleep：这条用例的判据是"扫得到"，不是"大概起来了"。
/// 探针要有一个真钟面：`discover` 不取钟（判据 12a），窗口得由调用方给。
fn wall_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after the unix epoch")
        .as_millis() as i64
}

fn wait_discovery(port: u16) {
    let target = SocketAddr::from(([127, 0, 0, 1], port));
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

struct Hub {
    /// 先声明：drop 顺序 = 声明顺序，hub 必须先被杀掉再删临时目录。
    _hub: HubProc,
    dir: tempfile::TempDir,
    port: u16,
    discovery_port: u16,
    hub_fp: String,
    tls_fp: String,
    /// 最后声明、最后释放：这几条用例动同一个端口族与同一份 LAN，
    /// 一个接一个跑比并行抢端口稳。
    _serial: MutexGuard<'static, ()>,
}

static PORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Hub {
    fn new() -> Hub {
        let serial = PORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("hub");
        std::fs::create_dir_all(&data).unwrap();
        let port = free_tcp_port();
        let discovery_port = free_udp_port();
        let cfg = dir.path().join("hub.toml");
        std::fs::write(
            &cfg,
            format!(
                "[hub]\nbind = \"127.0.0.1:{port}\"\nendpoints = [\"https://127.0.0.1:{port}\"]\nmax_snapshot_bps = 1048576\n"
            ),
        )
        .unwrap();

        let hub_err = dir.path().join("hub.err");
        let hub_out = dir.path().join("hub.out");
        let child = Command::new(hub_bin())
            .arg("serve")
            .arg("--hub-config")
            .arg(&cfg)
            .arg("--data-dir")
            .arg(&data)
            .arg("--dev-loopback")
            .arg("--tls")
            .arg("--discovery-port")
            .arg(discovery_port.to_string())
            .stdout(Stdio::from(std::fs::File::create(&hub_out).unwrap()))
            .stderr(Stdio::from(std::fs::File::create(&hub_err).unwrap()))
            .spawn()
            .unwrap();
        let hub = HubProc(child);
        wait_ready(port, &hub_err);
        wait_discovery(discovery_port);

        // 指纹的唯一来源是操作员也会跑的那条命令（只出票，不配对、不耗票）。
        let out = Command::new(hub_bin())
            .arg("invite")
            .arg("--json")
            .arg("--hub-config")
            .arg(&cfg)
            .arg("--data-dir")
            .arg(&data)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "hub invite: {}", stderr(&out));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        let hub_fp = v["hub_fp"].as_str().unwrap().to_string();
        let tls_fp = v["tls_fp"].as_str().unwrap().to_string();
        assert_eq!(hub_fp.len(), 64, "hub 指纹是 64 hex：{hub_fp}");

        Hub {
            _hub: hub,
            dir,
            port,
            discovery_port,
            hub_fp,
            tls_fp,
            _serial: serial,
        }
    }

    fn discover_args(&self) -> Vec<String> {
        vec![
            "--discover".into(),
            "--discovery-port".into(),
            self.discovery_port.to_string(),
            "--discovery-target".into(),
            "127.0.0.1".into(),
            // 扫描窗口取 1s：真实 LAN 的应答在毫秒级，这里要的是"扫完就走"，
            // 不是让每条用例替默认的 10s 窗口付账。
            "--timeout-s".into(),
            "1".into(),
        ]
    }

    fn run(&self, extra: &[&str]) -> Output {
        let args = self.discover_args();
        let mut all: Vec<&str> = args.iter().map(String::as_str).collect();
        all.extend_from_slice(extra);
        node(&all)
    }
}

fn candidates(out: &Output) -> Vec<Value> {
    let v: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout 不是一行 JSON（{e}）：{}", stdout(out)));
    v["candidates"].as_array().cloned().expect("候选必须是数组")
}

#[test]
fn discover_lists_the_hub_that_answered() {
    let h = Hub::new();
    let out = h.run(&["--json"]);
    assert_eq!(out.status.code(), Some(0), "扫到中心必须退 0：{}", stderr(&out));

    let cands = candidates(&out);
    assert_eq!(cands.len(), 1, "只起了一台中心，只该有一个候选：{}", stdout(&out));
    let c = &cands[0];
    assert_eq!(c["hub_fp"].as_str(), Some(h.hub_fp.as_str()));
    assert_eq!(c["tls_fp"].as_str(), Some(h.tls_fp.as_str()));
    assert_eq!(c["epoch"].as_u64().map(|e| e >= 1), Some(true), "center epoch 至少是 1：{c}");
    assert_eq!(
        c["endpoints"],
        serde_json::json!([format!("https://127.0.0.1:{}", h.port)]),
        "端点必须就是中心配置里的那一份：{c}"
    );
    assert!(!c["hub_pub"].as_str().unwrap().is_empty(), "公告里的 hub_pub 应被带出来：{c}");

    let text = stdout(&out);
    assert!(!text.contains("seen_from"), "候选表不许印本机网络细节：{text}");
    assert!(!text.contains("rc1."), "发现这条路不碰票：{text}");
}

#[test]
fn expect_fp_selects_the_hub_and_a_miss_exits_4() {
    let h = Hub::new();
    let out = h.run(&["--expect-fp", &h.hub_fp]);
    assert_eq!(out.status.code(), Some(0), "指定的指纹在场必须退 0：{}", stderr(&out));
    let cands = candidates(&out);
    assert_eq!(cands.len(), 1, "{}", stdout(&out));
    assert_eq!(cands[0]["hub_fp"].as_str(), Some(h.hub_fp.as_str()));

    // 一个与中心无关的指纹：全 0 的 64 hex。
    let wrong = "0".repeat(64);
    assert_ne!(wrong, h.hub_fp);
    let out = h.run(&["--expect-fp", &wrong]);
    assert_eq!(out.status.code(), Some(4), "要的中心不在必须退 4：{}", stderr(&out));
    assert!(candidates(&out).is_empty(), "退 4 时候选表必须为空：{}", stdout(&out));
    assert!(stdout(&out).contains("\"candidates\":[]"), "空表仍是一份可解析的文档：{}", stdout(&out));
}

#[test]
fn discover_needs_no_node_config() {
    let h = Hub::new();
    let missing = h.dir.path().join("no-such-node.toml");
    let out = h.run(&["--config", &toml_path(&missing)]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "没有 node.toml 也必须能发现（发现是配对之前的事）：{}",
        stderr(&out)
    );
    let cands = candidates(&out);
    assert_eq!(cands.len(), 1, "{}", stdout(&out));
    assert_eq!(cands[0]["hub_fp"].as_str(), Some(h.hub_fp.as_str()));
    assert!(!missing.exists(), "发现不许顺手造一个配置文件：{}", missing.display());
}

/// 用法错是**静态的**：退 3、 stdout 一个字节都不写、stderr 不回声用户输入。
/// 这一条不需要 hub，也不该等一个扫描窗口。
#[test]
fn a_bad_expect_fp_or_target_is_a_static_usage_error() {
    let bad_fps = [
        "not-hex".to_string(),
        "a".repeat(63),
        "a".repeat(65),
        "g".repeat(64),
    ];
    for bad in &bad_fps {
        let out = node(&["--discover", "--expect-fp", bad, "--timeout-s", "0"]);
        assert_eq!(out.status.code(), Some(3), "{bad} 该是用法错：{}", stderr(&out));
        assert!(out.stdout.is_empty(), "用法错不该往 stdout 写东西：{}", stdout(&out));
        let err = stderr(&out);
        assert!(err.contains("invalid invocation"), "类别要报得出来：{err}");
        assert!(!err.contains(bad.as_str()), "用法错不许回声输入：{err}");
    }

    // 目标只收 IP：主机名要先 DNS，而发现是"连 DNS 都没有时也能用"的那条路。
    let out = node(&["--discover", "--discovery-target", "hub.local", "--timeout-s", "0"]);
    assert_eq!(out.status.code(), Some(3), "非 IP 目标该是用法错：{}", stderr(&out));
    assert!(out.stdout.is_empty());
    assert!(stderr(&out).contains("invalid invocation"));
}
