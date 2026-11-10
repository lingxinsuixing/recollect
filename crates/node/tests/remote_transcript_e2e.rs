//! W39 刀 D：边缘**远端转录读**的真两进程 E2E（细案 `2026-11-09-w39-remote-transcript` §3.2/§五）。
//!
//! 夹具形状照 `invite_e2e.rs`：起真 `recollect-hub serve --dev-loopback --tls`，
//! 凭 `hub invite` 出的票让一台边缘节点配对（**地址与 TLS 指纹从票落进状态文件**，
//! 正是 `remote_transcript` 要读的那三格），然后在**中心数据目录**用
//! `rc_store::Root::open` + `rc_ledger::insert_batch` 种一条两轮的会话，
//! 最后用**边缘数据目录**调 `node::remote_transcript` 的四个函数读数。
//!
//! **没有 mock**：四个函数都经真签名请求（challenge + 五道鉴权）打到真中心。
//! 断言：会话清单含该会话、回放正文逐字、search 命中、turn 取到目标轮与前一邻居。

use node::state;
use rc_contract::{NormalizedTurn, SessionId, TurnTier};
use rc_store::Root;
use serde_json::Value;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};

/// 中心会话的两轮共享的命中词（自造合成词，不引用任何真实语料）。
const MARKER: &str = "zephyrquokka";
const SESSION_KEY: &str = "ses-remote-transcript";

fn node_bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-node")
}

/// 同 `e2e_two_process.rs`：cargo 只给本包的 bin 定义 `CARGO_BIN_EXE_*`，hub 按邻居找。
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

/// 64 位小写 hex 的 turn_uid（`/v1/transcript/turn` 的形状门要求）。
fn uid(n: u8) -> String {
    format!("{n:064x}")
}

/// 中心数据目录里的一条两轮会话（归属自造：agent / principal 与设备都不同）。
/// `insert_batch` 落 `sessions` 行 + `turns` 行 + FTS 影子行，三个读数都读得到。
fn seed_center_session(hub_data: &Path) {
    let root = Root::open(hub_data).unwrap();
    let sid = SessionId {
        session_key: SESSION_KEY.to_string(),
        device_id: "dev-centre".to_string(),
        source_agent: "agent-centre".to_string(),
        machine: None,
        project_hint: Some("proj-w39".to_string()),
        last_saved_at_ms: None,
        owner_principal_id: Some("p-synthetic".to_string()),
        principal_name: None,
        principal_source: Some("agent_runtime".to_string()),
    };
    let bodies = [
        format!("{MARKER} amber opening words"),
        format!("{MARKER} amber closing words"),
    ];
    let turns: Vec<NormalizedTurn> = bodies
        .iter()
        .enumerate()
        .map(|(i, body)| {
            let at = 1_000 + i as i64;
            NormalizedTurn {
                turn_uid: uid(0x31 + i as u8),
                body_fp: rc_contract::compute_body_fp(body),
                tier: TurnTier::ContentOnly,
                body: body.clone(),
                role: if i == 0 { "human".into() } else { "agent".into() },
                created_at_ms: at,
            }
        })
        .collect();
    rc_ledger::insert_batch(root.transcript(), &sid, &turns, 1_100).unwrap();
}

struct Fixture {
    /// 先声明：drop 顺序 = 声明顺序，hub 必须先被杀掉再删临时目录。
    _hub: HubProc,
    _dir: tempfile::TempDir,
    node_data: PathBuf,
    _node_cfg: PathBuf,
    /// 端口是"绑一下再放"拿到的：同一二进制里的真进程用例由此串起来。
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

        // 种子先落盘、hub 后起来：serve 起来之后不再从测试进程写中心库。
        seed_center_session(&hub_data);

        let device = rc_net::key::DeviceKey::generate().unwrap();
        device.save(&node_data.join("device.key")).unwrap();

        let port = free_port();
        let hub_cfg = dir.path().join("hub.toml");
        std::fs::write(
            &hub_cfg,
            format!(
                "[hub]\nbind = \"127.0.0.1:{port}\"\nendpoints = [\"https://127.0.0.1:{port}\"]\nmax_snapshot_bps = 1048576\n"
            ),
        )
        .unwrap();
        // 凭票加入的节点配置：**没有端点、没有指纹**——地址与信任只能来自票，
        // 也正是 `remote_transcript` 之后要读的那三格。
        let node_cfg = dir.path().join("node.toml");
        std::fs::write(
            &node_cfg,
            format!(
                "data_dir = \"{}\"\ndevice_key_path = \"{}\"\n",
                toml_path(&node_data),
                toml_path(&node_data.join("device.key")),
            ),
        )
        .unwrap();

        let hub_err = dir.path().join("hub.err");
        let child = Command::new(hub_bin())
            .arg("serve")
            .arg("--hub-config")
            .arg(&hub_cfg)
            .arg("--data-dir")
            .arg(&hub_data)
            .arg("--dev-loopback")
            .arg("--tls")
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&hub_err).unwrap()))
            .spawn()
            .unwrap();
        let hub = HubProc(child);
        wait_ready(port, &hub_err);

        // 出票（证书已由 serve 落盘）→ 节点凭票加入：状态文件里从此有
        // `paired_endpoints` / `paired_tls_fp` / `paired_hub_pub` 与 token。
        let out = Command::new(hub_bin())
            .arg("invite")
            .arg("--json")
            .arg("--hub-config")
            .arg(&hub_cfg)
            .arg("--data-dir")
            .arg(&hub_data)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "hub invite: {}", stderr(&out));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        let ticket = v["ticket"].as_str().unwrap().to_string();
        let out = node_run(&node_cfg, &["--invite", &ticket]);
        assert_eq!(out.status.code(), Some(0), "node pair: {}", stderr(&out));
        assert!(state::token_path(&node_data).is_file(), "配对成功要落 token");

        Fixture { _hub: hub, _dir: dir, node_data, _node_cfg: node_cfg, _serial: serial }
    }
}

#[test]
fn the_edge_reads_the_centre_transcript_over_the_lan() {
    let f = Fixture::new();
    let (first, second) = (uid(0x31), uid(0x32));
    let dir = f.node_data.as_path();
    let extra: &[(String, String)] = &[];

    // ① 会话清单：中心全部会话（这里只有种下的那一条），信封已带 `source:"center"`。
    let list = node::remote_transcript::sessions(dir, None, extra).expect("sessions");
    assert_eq!(list["source"], serde_json::json!("center"), "{list}");
    assert!(list["epoch"].as_u64().is_some(), "中心信封要带 epoch：{list}");
    let sessions = list["sessions"].as_array().expect("sessions:[]");
    let row = sessions
        .iter()
        .find(|s| s["session_key"] == serde_json::json!(SESSION_KEY))
        .unwrap_or_else(|| panic!("会话清单里要有中心那条：{list}"));
    assert_eq!(row["owner_agent_id"], serde_json::json!("agent-centre"), "{row}");
    assert_eq!(row["owner_principal_id"], serde_json::json!("p-synthetic"), "{row}");
    assert_eq!(row["turns"], serde_json::json!(2), "{row}");

    // ② 回放：正文逐字，归属随原文走。
    let replay =
        node::remote_transcript::session(dir, SESSION_KEY, None, None, extra).expect("session");
    assert_eq!(replay["source"], serde_json::json!("center"), "{replay}");
    assert_eq!(replay["session_key"], serde_json::json!(SESSION_KEY), "{replay}");
    assert_eq!(replay["device_id"], serde_json::json!("dev-centre"), "{replay}");
    let turns = replay["turns"].as_array().expect("turns:[]");
    assert_eq!(turns.len(), 2, "{replay}");
    assert_eq!(turns[0]["turn_uid"], serde_json::json!(first), "{replay}");
    assert_eq!(turns[0]["body"], serde_json::json!(format!("{MARKER} amber opening words")));
    assert_eq!(turns[1]["body"], serde_json::json!(format!("{MARKER} amber closing words")));

    // ③ search：中心 FTS 上命中（命中词只在种下的那两轮里）。
    let found = node::remote_transcript::search(dir, MARKER, Some(10), extra).expect("search");
    assert_eq!(found["source"], serde_json::json!("center"), "{found}");
    let hits = found["hits"].as_array().expect("hits:[]");
    assert!(
        hits.iter().any(|h| h["turn_uid"] == serde_json::json!(second)
            && h["session_key"] == serde_json::json!(SESSION_KEY)),
        "命中里要有第二轮：{found}"
    );

    // ④ turn：目标轮 + 同会话前一邻居。
    let window =
        node::remote_transcript::turn(dir, &second, 1, 0, extra).expect("turn window");
    assert_eq!(window["session_key"], serde_json::json!(SESSION_KEY), "{window}");
    let turns = window["turns"].as_array().expect("turns:[]");
    assert_eq!(turns.len(), 2, "before=1 要带回前一邻居：{window}");
    assert_eq!(turns[0]["turn_uid"], serde_json::json!(first), "{window}");
    assert_eq!(turns[1]["turn_uid"], serde_json::json!(second), "{window}");
    assert_eq!(window["truncated_before"], serde_json::json!(false), "{window}");
    assert_eq!(window["owner_principal_id"], serde_json::json!("p-synthetic"), "{window}");

    // 会话数据目录就是**边缘**的：`sessions` 里的行不是从本机转录层读的
    // （边缘的转录层一行都没有），所以上面的读数只可能来自中心。
    let edge = rc_store::NodeRoot::open_node(dir, None).unwrap();
    let local: i64 = edge
        .transcript()
        .conn()
        .query_row("SELECT count(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(local, 0, "远端读数不许靠边缘本地转录层");
}
