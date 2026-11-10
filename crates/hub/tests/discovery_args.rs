// crates/hub/tests/discovery_args.rs
//! S4c1 / S4fix-A R12：`hub serve` 的发现旗标形状与启动行事实性。
//!
//! 发现应答的行为面在 `rc-net/tests/discovery.rs`（真 UDP socket）；这一份钉 CLI 那一侧：
//! 两个旗标存在、默认端口就是 `rc_net::discovery::DISCOVERY_PORT`、越界端口是用法错误；
//! 以及启动行 `discovery=on` 只在 UDP 端口真的绑上之后才出现。
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin()).args(args).output().expect("hub binary")
}

#[test]
fn serve_help_lists_both_discovery_flags_and_the_shared_default_port() {
    let out = run(&["serve", "--help"]);
    assert!(out.status.success(), "--help 不该失败：{:?}", out.status);
    let text = String::from_utf8_lossy(&out.stdout);
    for needle in ["--no-discovery", "--discovery-port"] {
        assert!(text.contains(needle), "serve --help 里没有 {needle}：{text}");
    }
    // 默认值必须与 `rc_net::discovery::DISCOVERY_PORT` 是同一个数：两处各写一遍就是
    // 下一次"节点问 37420、中心答 37421"的来源。
    let default = rc_net::discovery::DISCOVERY_PORT.to_string();
    assert!(
        text.contains(&format!("default: {default}")),
        "serve --help 的默认端口不是 {default}：{text}"
    );
}

/// `--discovery-port` 是 u16：越界值由 clap 拦成用法错误，不是"绑到某个被截断的端口上"。
/// 这一仓把 clap 的用法错误统一改写成退出码 3 + 一行 `invalid invocation`（静态 USAGE，
/// 不回显输入），所以断言是这个约定，不是 clap 的 2。
#[test]
fn an_out_of_range_discovery_port_is_a_usage_error() {
    let out = run(&["serve", "--discovery-port", "99999"]);
    assert_eq!(out.status.code(), Some(3), "越界端口该由 clap 拦下");
    assert!(out.stdout.is_empty(), "用法错误不该往 stdout 写东西");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("invalid invocation"), "类别要报得出来：{err}");
}

/// 一台真起的 `hub serve`：把 stdout/stderr 落到文件里，退出时杀掉。
///
/// 不做成管道：管道缓冲区满时子进程会卡在写 stdout 上，而这里只是要读回几行启动输出。
struct ServeProc {
    child: Child,
    out: PathBuf,
    err: PathBuf,
}

impl ServeProc {
    fn start(data: &Path, port: u16, discovery_port: u16) -> ServeProc {
        let cfg = data.join("hub.toml");
        std::fs::write(
            &cfg,
            format!("[hub]\nbind = \"127.0.0.1:{port}\"\nendpoints = [\"https://127.0.0.1:{port}\"]\n"),
        )
        .unwrap();
        let out = data.join("serve.out");
        let err = data.join("serve.err");
        let child = Command::new(bin())
            .arg("serve")
            .arg("--hub-config")
            .arg(&cfg)
            .arg("--data-dir")
            .arg(data)
            .arg("--dev-loopback")
            .arg("--discovery-port")
            .arg(discovery_port.to_string())
            .stdout(Stdio::from(std::fs::File::create(&out).unwrap()))
            .stderr(Stdio::from(std::fs::File::create(&err).unwrap()))
            .spawn()
            .expect("hub binary");
        ServeProc { child, out, err }
    }

    /// 等到 HTTP 端口可连：启动行都写在 `serve_loop` 之前，所以连得上就说明该印的都印了。
    fn wait_ready(&self, port: u16) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return;
            }
            if Instant::now() > deadline {
                panic!("hub 没起来：{}", self.stderr());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn stdout(&self) -> String {
        std::fs::read_to_string(&self.out).unwrap_or_default()
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.err).unwrap_or_default()
    }
}

impl Drop for ServeProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn ports() -> (u16, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    (addr.port(), addr)
}

fn free_udp_port() -> u16 {
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.local_addr().unwrap().port()
}

/// 正常：UDP 端口绑得上 ⇒ `discovery=on port=<那一个>`。
#[test]
fn a_serving_hub_prints_discovery_on_only_after_binding_udp() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let (port, _) = ports();
    let discovery_port = free_udp_port();
    let proc = ServeProc::start(&data, port, discovery_port);
    proc.wait_ready(port);

    let out = proc.stdout();
    assert!(
        out.contains(&format!("discovery=on port={discovery_port}")),
        "启动行里没有发现端口：{out}"
    );
    assert!(!out.contains("discovery=unavailable"), "端口明明绑上了：{out}");
}

/// 端口被占用：`discovery=on` 一个字节都不许出现（它必须是事实），
/// 而 serve 本身照常服务——发现失效不该让一台已经在服务的中心退出。
#[test]
fn a_hub_that_cannot_bind_udp_never_prints_discovery_on() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let (port, _) = ports();
    // 先占住一个 UDP 端口，再把同一个端口交给 serve。占位 socket 也绑通配地址：
    // serve 绑的就是 `0.0.0.0:<port>`，两个通配绑定才会真的撞上。
    let squat = UdpSocket::bind("0.0.0.0:0").unwrap();
    let taken = squat.local_addr().unwrap().port();
    let proc = ServeProc::start(&data, port, taken);
    proc.wait_ready(port);

    let out = proc.stdout();
    let err = proc.stderr();
    assert!(!out.contains("discovery=on"), "端口没绑上却印了 discovery=on：{out}");
    assert!(
        err.contains("discovery unavailable — udp port is not bindable"),
        "没有一条说明发现为什么没开：{err}"
    );
    // HTTP 面照常（wait_ready 已经连上过），进程还活着。
    assert!(out.contains("tls=off"), "启动行不完整：{out}");
}
