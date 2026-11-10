//! S4a2：TLS 接线的**真进程** E2E。
//!
//! 起真 `recollect-hub serve --dev-loopback --tls`（临时 data-dir、随机端口），
//! 从 `hub tls show --json` 取指纹写进 node.toml，然后在 TLS 上跑完配对、上行、快照下行。
//! 另两件：错指纹必须 `tls_failed` 且中心数据面一行不动；没有证书时 `hub tls show` 退 3。
//!
//! 既有回环 E2E（`e2e_two_process.rs`，无 `--tls`）仍走明文，本文件不碰它。

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

fn issue_code(data: &Path) -> String {
    let out = hub_run(data, &["pair"]);
    assert_eq!(out.status.code(), Some(0), "hub pair: {}", stderr(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    v["code"].as_str().unwrap().to_string()
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

/// `hub_tls_fp` 只在给了的时候写：它的**缺席**就是"明文"（既有回环 E2E 的形状）。
fn write_node_cfg(path: &Path, data: &Path, port: u16, hub_tls_fp: Option<&str>) {
    let mut text = format!(
        "data_dir = \"{}\"\ndevice_key_path = \"{}\"\n",
        toml_path(data),
        toml_path(&data.join("device.key")),
    );
    if let Some(fp) = hub_tls_fp {
        text.push_str(&format!("hub_tls_fp = \"{fp}\"\n"));
    }
    text.push_str(&format!(
        "[[endpoints]]\nlabel = \"loopback\"\nhost = \"127.0.0.1\"\nport = {port}\n"
    ));
    std::fs::write(path, text).unwrap();
}

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
                scope: Scope { kind: ScopeKind::Project, key: "proj-tls".into() },
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

fn tls_show(data: &Path, json: bool) -> Output {
    let mut args = vec!["tls", "show"];
    if json {
        args.push("--json");
    }
    hub_run(data, &args)
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
    /// serve 的 stdout：启动那一行指纹/`tls=off` 的证词。
    startup: String,
    spare_code: String,
    _serial: std::sync::MutexGuard<'static, ()>,
}

static PORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Fixture {
    /// `tls=true` ⇒ `serve --dev-loopback --tls`，并从 `hub tls show --json` 取指纹；
    /// `tls=false` ⇒ 既有回环默认（明文），node.toml 里没有 `hub_tls_fp`。
    fn new(tls: bool) -> Fixture {
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
        let scheme = if tls { "https" } else { "http" };
        let hub_cfg = dir.path().join("hub.toml");
        std::fs::write(
            &hub_cfg,
            format!(
                "[hub]\nbind = \"127.0.0.1:{port}\"\nendpoints = [\"{scheme}://127.0.0.1:{port}\"]\nmax_snapshot_bps = 1048576\n"
            ),
        )
        .unwrap();
        let node_cfg = dir.path().join("node.toml");
        write_node_cfg(&node_cfg, &node_data, port, None);

        let hub_err = dir.path().join("hub.err");
        let hub_out = dir.path().join("hub.out");
        // 两张码都在 serve 起来之前签发：配对码的签发不联网，少一个"两进程同开账本"的窗口。
        let code = issue_code(&hub_data);
        let spare_code = issue_code(&hub_data);

        let mut cmd = Command::new(hub_bin());
        cmd.arg("serve").arg("--hub-config").arg(&hub_cfg).arg("--data-dir").arg(&hub_data);
        cmd.arg("--dev-loopback");
        if tls {
            cmd.arg("--tls");
        }
        let child = cmd
            .stdout(Stdio::from(std::fs::File::create(&hub_out).unwrap()))
            .stderr(Stdio::from(std::fs::File::create(&hub_err).unwrap()))
            .spawn()
            .unwrap();
        let hub = HubProc(child);
        wait_ready(port, &hub_err);
        let startup = std::fs::read_to_string(&hub_out).unwrap_or_default();

        if tls {
            // 指纹的唯一来源就是操作员会跑的那条命令；证书在 accept 之前已经落盘。
            let out = tls_show(&hub_data, true);
            assert_eq!(out.status.code(), Some(0), "tls show: {}", stderr(&out));
            let v: Value = serde_json::from_slice(&out.stdout).unwrap();
            let fp = v["tls_fingerprint"].as_str().expect("tls_fingerprint 是字符串");
            assert_eq!(fp.len(), 64, "full 指纹是 64 hex：{fp}");
            assert!(fp.bytes().all(|b| b.is_ascii_hexdigit()), "指纹只含 hex：{fp}");
            assert_eq!(fp.to_ascii_lowercase(), fp, "指纹必须是小写");
            assert_eq!(
                v["tls_short"].as_str(),
                Some(&fp[..16]),
                "short 指纹是 full 的前 16 位"
            );
            write_node_cfg(&node_cfg, &node_data, port, Some(fp));
        }

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
            startup,
            spare_code,
            _serial: serial,
        }
    }

    fn node(&self, extra: &[&str]) -> Output {
        node_run(&self.node_cfg, extra)
    }

    fn hub(&self, args: &[&str]) -> Output {
        hub_run(&self.hub_data, args)
    }

    fn fingerprint(&self) -> (String, String) {
        let out = tls_show(&self.hub_data, true);
        assert_eq!(out.status.code(), Some(0), "tls show: {}", stderr(&out));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        (
            v["tls_fingerprint"].as_str().unwrap().to_string(),
            v["tls_short"].as_str().unwrap().to_string(),
        )
    }
}

#[test]
fn tls_end_to_end_pair_sync_and_snapshot() {
    let f = Fixture::new(true);
    let (fp, short) = f.fingerprint();

    // 启动那一行就是操作员抄 `hub_tls_fp` 的地方，与 `hub tls show` 必须逐字一致。
    let want = format!("tls_fingerprint={fp} tls_short={short}");
    assert!(
        f.startup.lines().any(|l| l == want),
        "serve 启动行 {want:?} 未出现：{:?}",
        f.startup
    );
    let human = tls_show(&f.hub_data, false);
    assert_eq!(human.status.code(), Some(0), "{}", stderr(&human));
    assert_eq!(stdout(&human).trim(), want, "人读与 JSON 的指纹必须同值");

    // 上行：TLS 上完成一次签名请求。
    let out = f.node(&["--once"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["pushed"].as_i64(), Some(f.turn_uids.len() as i64), "{v}");
    assert_eq!(v["endpoint"].as_str(), Some("loopback"), "{v}");
    assert_eq!(v["no_memory"].as_bool(), Some(true), "{v}");
    assert_eq!(hub_turns(&f.hub_data), f.turn_uids.len() as i64);
    assert_eq!(import_log_rows(&f.hub_data), 1);

    // 快照下行：TLS 上的分片下载 + 整份 sha 终验。
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
    assert_eq!(hub_ids, node_ids, "TLS 两端必须逐 id 相同");
}

#[test]
fn a_wrong_fingerprint_fails_with_tls_failed_and_writes_nothing() {
    let f = Fixture::new(true);
    let (good_fp, _) = f.fingerprint();
    // 只翻一个 hex 字符：形状合法、内容不是那枚证书。
    let wrong_fp = {
        let first = if good_fp.starts_with('0') { '1' } else { '0' };
        format!("{first}{}", &good_fp[1..])
    };

    let bad_data = f.dir.path().join("node-bad");
    std::fs::create_dir_all(&bad_data).unwrap();
    let bad_key = rc_net::key::DeviceKey::generate().unwrap();
    let bad_id = bad_key.device_id();
    bad_key.save(&bad_data.join("device.key")).unwrap();
    let bad_turns = seed_turns(&bad_data, &bad_id);
    assert_eq!(bad_turns.len(), 2, "错指纹用例要有行可推，才证明中心一行没收到");
    let bad_cfg = f.dir.path().join("node-bad.toml");
    write_node_cfg(&bad_cfg, &bad_data, f.port, Some(&wrong_fp));

    // 配对阶段同样映射：错指纹不是"中心没开"，是 `tls_failed`。
    let out = node_run(&bad_cfg, &["--code", &f.spare_code]);
    assert_eq!(out.status.code(), Some(3), "错指纹配对必须非 0：{}", stderr(&out));
    assert!(
        stderr(&out).contains("tls_failed"),
        "配对错误码必须是 tls_failed：{}",
        stderr(&out)
    );
    assert!(
        !state::token_path(&bad_data).exists(),
        "握手没成才没有 token：错指纹不许留下一张有效身份"
    );

    // 同步阶段：把好节点的 token 搬过来，仍然必须止步于 TLS。
    std::fs::copy(state::token_path(&f.node_data), state::token_path(&bad_data)).unwrap();
    {
        // 中心公钥照好节点的写（配置里钉死），状态文件留空——这一轮只该卡在 TLS。
        let text = std::fs::read_to_string(&bad_cfg).unwrap();
        std::fs::write(
            &bad_cfg,
            format!("hub_pub = \"{}\"\n{text}", f.hub_pub),
        )
        .unwrap();
    }
    let out = node_run(&bad_cfg, &["--once"]);
    assert_eq!(out.status.code(), Some(3), "错指纹同步必须非 0：{}", stderr(&out));
    assert!(
        stderr(&out).contains("tls_failed"),
        "同步错误码必须是 tls_failed：{}",
        stderr(&out)
    );
    let st = state::load(&state::state_path(&bad_data))
        .unwrap()
        .expect("失败也要把 last_error 落盘");
    assert_eq!(st.last_error, "tls_failed");
    assert!(
        st.current_manifest.epoch == 0 && st.current_manifest.seq == 0,
        "没有一次握手成功，就不该有清单"
    );

    // 中心数据面读数：一行都没收到、一条导入都没记。
    assert_eq!(hub_turns(&f.hub_data), 0, "错指纹的那批行一行都不许落");
    assert_eq!(import_log_rows(&f.hub_data), 0);

    // 阳性对照：同一台中心，好指纹的节点照常能推——上面卡住的是身份，不是服务。
    let out = f.node(&["--once"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["pushed"].as_i64(), Some(f.turn_uids.len() as i64), "{v}");
    assert_eq!(hub_turns(&f.hub_data), f.turn_uids.len() as i64);
}

#[test]
fn tls_show_without_a_certificate_exits_3() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("empty-data");
    for json in [false, true] {
        let out = tls_show(&data, json);
        assert_eq!(out.status.code(), Some(3), "没有证书必须退 3（json={json}）");
        assert!(
            stderr(&out).contains("no tls certificate"),
            "静态话要点名缺什么：{}",
            stderr(&out)
        );
        assert!(stdout(&out).trim().is_empty(), "没有读数就没有 stdout");
    }
    assert!(!data.join("tls").exists(), "查询不许顺手生成一对证书");
}

#[test]
fn loopback_serve_without_the_tls_flag_stays_plaintext() {
    let serial = PORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let hub_data = dir.path().join("hub");
    std::fs::create_dir_all(&hub_data).unwrap();
    let port = free_port();
    let hub_cfg = dir.path().join("hub.toml");
    std::fs::write(
        &hub_cfg,
        format!(
            "[hub]\nbind = \"127.0.0.1:{port}\"\nendpoints = [\"http://127.0.0.1:{port}\"]\n"
        ),
    )
    .unwrap();
    let hub_out = dir.path().join("hub.out");
    let hub_err = dir.path().join("hub.err");
    let child = Command::new(hub_bin())
        .arg("serve")
        .arg("--hub-config")
        .arg(&hub_cfg)
        .arg("--data-dir")
        .arg(&hub_data)
        .arg("--dev-loopback")
        .stdout(Stdio::from(std::fs::File::create(&hub_out).unwrap()))
        .stderr(Stdio::from(std::fs::File::create(&hub_err).unwrap()))
        .spawn()
        .unwrap();
    let _hub = HubProc(child);
    wait_ready(port, &hub_err);
    let startup = std::fs::read_to_string(&hub_out).unwrap_or_default();
    assert!(startup.contains("tls=off"), "回环缺省必须是明文：{startup:?}");
    assert!(
        !hub_data.join("tls").exists(),
        "明文模式不该顺手生成一对没人用的证书"
    );
    drop(serial);
}

#[test]
fn tls_and_insecure_http_together_are_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let hub_data = dir.path().join("hub");
    std::fs::create_dir_all(&hub_data).unwrap();
    let hub_cfg = dir.path().join("hub.toml");
    std::fs::write(
        &hub_cfg,
        "[hub]\nbind = \"127.0.0.1:0\"\nendpoints = [\"https://127.0.0.1:0\"]\n",
    )
    .unwrap();
    let out = Command::new(hub_bin())
        .arg("serve")
        .arg("--hub-config")
        .arg(&hub_cfg)
        .arg("--data-dir")
        .arg(&hub_data)
        .arg("--dev-loopback")
        .arg("--tls")
        .arg("--insecure-http")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("invalid invocation"),
        "两个相反的旗标同给是用法错误：{}",
        stderr(&out)
    );
}
