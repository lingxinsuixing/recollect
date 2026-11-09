//! S4b2：节点凭邀请票加入的**真进程** E2E。
//!
//! 起真 `recollect-hub serve --dev-loopback --tls`，用 `hub invite --json` 出票，
//! 节点配置里**一个端点都不写**（连 `hub_tls_fp` 都没有）：地址与指纹只能来自票。
//!
//! 五件事：
//! 1. 凭票加入 → 状态文件里落指纹与票的端点 → `hub snapshot publish` 后 `--once`
//!    靠状态里的端点拉下快照（不靠 `node.toml`）；
//! 2. 过期票 ⇒ 退出 3、`invite_expired`、没有 token；
//! 3. 作废票 ⇒ `invite_revoked`；
//! 4. 篡改票 ⇒ `invite_invalid`；未配对且无端点时 `sync` ⇒ `no_endpoint`；
//! 5. 旧 `--code` 配对仍可用（票是码的后继，不是替代）。

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

/// 同 `tls_e2e.rs`：cargo 只给本包的 bin 定义 `CARGO_BIN_EXE_*`，hub 按邻居找。
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

/// 凭票加入的节点配置：**只有 data_dir 与钥匙**。没有 `endpoints`、没有 `hub_tls_fp`——
/// 地址与指纹必须全部来自票，否则这个 E2E 什么也没证明。
fn write_invite_node_cfg(path: &Path, data: &Path) {
    std::fs::write(
        path,
        format!(
            "data_dir = \"{}\"\ndevice_key_path = \"{}\"\n",
            toml_path(data),
            toml_path(&data.join("device.key")),
        ),
    )
    .unwrap();
}

/// 旧路（`--code`）的配置：手写端点 + 钉死指纹。
fn write_code_node_cfg(path: &Path, data: &Path, port: u16, tls_fp: &str) {
    std::fs::write(
        path,
        format!(
            "data_dir = \"{}\"\ndevice_key_path = \"{}\"\nhub_tls_fp = \"{tls_fp}\"\n\
             [[endpoints]]\nlabel = \"loopback\"\nhost = \"127.0.0.1\"\nport = {port}\n",
            toml_path(data),
            toml_path(&data.join("device.key")),
        ),
    )
    .unwrap();
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
                scope: Scope { kind: ScopeKind::Project, key: "proj-invite".into() },
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
    hub_cfg: PathBuf,
    hub_data: PathBuf,
    node_data: PathBuf,
    node_cfg: PathBuf,
    port: u16,
    tls_fp: String,
    turn_uids: Vec<String>,
    _serial: std::sync::MutexGuard<'static, ()>,
}

static PORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Fixture {
    /// TLS 中心 + 一个**没有端点**的节点配置。票在每条用例里自己出——
    /// 出票要读账本，而 serve 正开着账本，多一张少一张都影响"用没用过"的读数。
    fn new() -> Fixture {
        let serial = PORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let hub_data = dir.path().join("hub");
        let node_data = dir.path().join("node");
        std::fs::create_dir_all(&hub_data).unwrap();
        std::fs::create_dir_all(&node_data).unwrap();

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
                "[hub]\nbind = \"127.0.0.1:{port}\"\nendpoints = [\"https://127.0.0.1:{port}\"]\nmax_snapshot_bps = 1048576\n"
            ),
        )
        .unwrap();
        let node_cfg = dir.path().join("node.toml");
        write_invite_node_cfg(&node_cfg, &node_data);

        let hub_err = dir.path().join("hub.err");
        let hub_out = dir.path().join("hub.out");
        let child = Command::new(hub_bin())
            .arg("serve")
            .arg("--hub-config")
            .arg(&hub_cfg)
            .arg("--data-dir")
            .arg(&hub_data)
            .arg("--dev-loopback")
            .arg("--tls")
            .stdout(Stdio::from(std::fs::File::create(&hub_out).unwrap()))
            .stderr(Stdio::from(std::fs::File::create(&hub_err).unwrap()))
            .spawn()
            .unwrap();
        let hub = HubProc(child);
        wait_ready(port, &hub_err);

        // 指纹的唯一来源就是操作员会跑的那条命令（证书在 accept 之前已落盘）。
        let out = hub_run(&hub_data, &["tls", "show", "--json"]);
        assert_eq!(out.status.code(), Some(0), "tls show: {}", stderr(&out));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        let tls_fp = v["tls_fingerprint"].as_str().unwrap().to_string();
        assert_eq!(tls_fp.len(), 64, "指纹是 64 hex：{tls_fp}");

        Fixture {
            _hub: hub,
            dir,
            hub_cfg,
            hub_data,
            node_data,
            node_cfg,
            port,
            tls_fp,
            turn_uids,
            _serial: serial,
        }
    }

    fn node(&self, extra: &[&str]) -> Output {
        node_run(&self.node_cfg, extra)
    }

    /// `hub invite --json --hub-config …`：票 + id（作废要用 id）。
    fn invite(&self, extra: &[&str]) -> Value {
        let out = Command::new(hub_bin())
            .arg("invite")
            .arg("--json")
            .arg("--hub-config")
            .arg(&self.hub_cfg)
            .arg("--data-dir")
            .arg(&self.hub_data)
            .args(extra)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "hub invite: {}", stderr(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn hub(&self, args: &[&str]) -> Output {
        hub_run(&self.hub_data, args)
    }

    /// 一台全新的、还没配对过的节点（独立 data-dir 与钥匙）。
    fn fresh_node(&self, name: &str) -> (PathBuf, PathBuf) {
        let data = self.dir.path().join(name);
        std::fs::create_dir_all(&data).unwrap();
        let key = rc_net::key::DeviceKey::generate().unwrap();
        key.save(&data.join("device.key")).unwrap();
        let cfg = self.dir.path().join(format!("{name}.toml"));
        write_invite_node_cfg(&cfg, &data);
        (data, cfg)
    }
}

#[test]
fn invite_joins_and_syncs_using_only_the_state_endpoints() {
    let f = Fixture::new();
    let invite = f.invite(&[]);
    let ticket = invite["ticket"].as_str().unwrap();
    assert!(ticket.starts_with("rc1."), "不是文本票：{ticket}");

    // 配置里确实没有端点：这个用例证明的是"票带来地址"，不是"配置里碰巧有"。
    let cfg_text = std::fs::read_to_string(&f.node_cfg).unwrap();
    assert!(!cfg_text.contains("endpoints"), "本用例的 node.toml 不许有端点：{cfg_text}");
    assert!(!cfg_text.contains("hub_tls_fp"), "本用例连指纹都不许手写：{cfg_text}");

    let out = f.node(&["--invite", ticket]);
    assert_eq!(out.status.code(), Some(0), "凭票加入必须成功：{}", stderr(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["paired"].as_bool(), Some(true), "{v}");
    let want_endpoints = rc_net::invite::decode_ticket(ticket).unwrap().0;
    assert_eq!(
        v["announced_endpoints"].as_u64(),
        Some(want_endpoints.endpoints.len() as u64),
        "{v}"
    );
    let text = stdout(&out);
    assert!(!text.contains("https://"), "加入的输出不许印地址：{text}");

    // 状态文件：指纹 = 票里的、端点 = 票里的（顺序同票）。
    let st = state::load(&state::state_path(&f.node_data))
        .unwrap()
        .expect("配对成功必须有状态文件");
    assert_eq!(
        st.paired_tls_fp.as_deref(),
        Some(want_endpoints.tls_fp.as_str()),
        "状态里的 TLS 指纹必须就是票里那一枚"
    );
    assert_eq!(st.paired_endpoints, want_endpoints.endpoints, "状态里的端点必须就是票里那一份");
    assert_eq!(st.paired_tls_fp.as_deref(), Some(f.tls_fp.as_str()));
    assert!(!st.paired_hub_pub.is_empty(), "中心公钥落了盘：{:?}", st.paired_hub_pub);
    assert!(state::token_path(&f.node_data).exists(), "配对成功必须留下 token");

    // 第一轮：只上行，中心还没有快照。
    let out = f.node(&["--once"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["pushed"].as_i64(), Some(f.turn_uids.len() as i64), "{v}");
    assert_eq!(v["no_memory"].as_bool(), Some(true), "{v}");
    // 路名来自状态里的端点（`paired-1`），不是配置里的任何 label。
    assert_eq!(v["endpoint"].as_str(), Some("paired-1"), "端点必须来自状态：{v}");
    assert_eq!(hub_turns(&f.hub_data), f.turn_uids.len() as i64);

    // 中心出快照，节点只靠状态里的地址/指纹再连一次并拉下来。
    let out = f.hub(&["snapshot", "publish"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
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
fn an_expired_invite_is_refused_with_invite_expired() {
    let f = Fixture::new();
    // 1ms 有效期：出票那一刻之后就已经过期。
    let invite = f.invite(&["--ttl-ms", "1"]);
    let ticket = invite["ticket"].as_str().unwrap().to_string();
    std::thread::sleep(Duration::from_millis(30));

    let (data, cfg) = f.fresh_node("node-expired");
    let out = node_run(&cfg, &["--invite", &ticket]);
    assert_eq!(out.status.code(), Some(3), "过期票必须退 3：{}", stderr(&out));
    assert!(stderr(&out).contains("invite_expired"), "错误码必须是 invite_expired：{}", stderr(&out));
    assert!(!state::token_path(&data).exists(), "过期票不许留下 token");
    assert!(
        state::load(&state::state_path(&data)).unwrap().is_none(),
        "连一次请求都没发，就不该有状态文件"
    );
}

#[test]
fn a_revoked_invite_is_refused_with_invite_revoked() {
    let f = Fixture::new();
    let invite = f.invite(&[]);
    let ticket = invite["ticket"].as_str().unwrap().to_string();
    let id = invite["id"].as_str().unwrap().to_string();

    let out = f.hub(&["invite", "revoke", &id]);
    assert_eq!(out.status.code(), Some(0), "revoke: {}", stderr(&out));

    let (data, cfg) = f.fresh_node("node-revoked");
    let out = node_run(&cfg, &["--invite", &ticket]);
    assert_eq!(out.status.code(), Some(3), "作废票必须退 3：{}", stderr(&out));
    assert!(stderr(&out).contains("invite_revoked"), "错误码必须是 invite_revoked：{}", stderr(&out));
    assert!(!state::token_path(&data).exists(), "作废票不许留下 token");

    // 阳性对照：同一台中心另出一张票，同一台机器照样能加入——卡住的是这张票，不是服务。
    let ok = f.invite(&[]);
    let out = node_run(&cfg, &["--invite", ok["ticket"].as_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(state::token_path(&data).exists());
}

#[test]
fn a_tampered_ticket_is_invite_invalid_and_an_unpaired_node_has_no_endpoint() {
    let f = Fixture::new();
    let invite = f.invite(&[]);
    let ticket = invite["ticket"].as_str().unwrap();
    // 翻载荷的第一个字符：base64 仍能解，但首个字节不再是 `{` ⇒ 拆信封就失败。
    let (prefix, rest) = ticket.split_once('.').unwrap();
    let mut chars: Vec<char> = rest.chars().collect();
    chars[0] = if chars[0] == 'e' { 'f' } else { 'e' };
    let tampered = format!("{prefix}.{}", chars.into_iter().collect::<String>());
    assert_ne!(tampered, ticket);

    let (data, cfg) = f.fresh_node("node-tampered");
    let out = node_run(&cfg, &["--invite", &tampered]);
    assert_eq!(out.status.code(), Some(3), "篡改票必须退 3：{}", stderr(&out));
    assert!(stderr(&out).contains("invite_invalid"), "错误码必须是 invite_invalid：{}", stderr(&out));
    assert!(!state::token_path(&data).exists(), "篡改票不许留下 token");

    // 只改签名段：信封能拆开，但本地拿到 hub_pub 后必须先验票签名，不能发到中心再报 bad_response。
    let (head, sig) = ticket.rsplit_once('.').unwrap();
    let mut sig_chars: Vec<char> = sig.chars().collect();
    let last = sig_chars.len() - 1;
    sig_chars[last] = if sig_chars[last] == 'A' { 'B' } else { 'A' };
    let bad_sig = format!("{head}.{}", sig_chars.into_iter().collect::<String>());
    let (sig_data, sig_cfg) = f.fresh_node("node-badsig");
    let out = node_run(&sig_cfg, &["--invite", &bad_sig]);
    assert_eq!(out.status.code(), Some(3), "签名篡改必须退 3：{}", stderr(&out));
    assert!(
        stderr(&out).contains("invite_invalid"),
        "签名篡改必须在本地报 invite_invalid：{}",
        stderr(&out)
    );
    assert!(!state::token_path(&sig_data).exists());

    // 未配对、配置里也没有端点：同步只能报 `no_endpoint`（"先去配对"）。
    let out = node_run(&cfg, &["--once"]);
    assert_eq!(out.status.code(), Some(3), "无端点必须退 3：{}", stderr(&out));
    assert!(stderr(&out).contains("no_endpoint"), "错误码必须是 no_endpoint：{}", stderr(&out));
    let st = state::load(&state::state_path(&data)).unwrap().expect("失败码要落盘");
    assert_eq!(st.last_error, "no_endpoint");

    // 阳性对照：坏票不行、好票行。
    let out = node_run(&cfg, &["--invite", invite["ticket"].as_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(state::token_path(&data).exists());
}

#[test]
fn the_old_pair_code_path_still_works() {
    let f = Fixture::new();
    let out = f.hub(&["pair"]);
    assert_eq!(out.status.code(), Some(0), "hub pair: {}", stderr(&out));
    let code = serde_json::from_slice::<Value>(&out.stdout).unwrap()["code"]
        .as_str()
        .unwrap()
        .to_string();

    let data = f.dir.path().join("node-code");
    std::fs::create_dir_all(&data).unwrap();
    let key = rc_net::key::DeviceKey::generate().unwrap();
    key.save(&data.join("device.key")).unwrap();
    let cfg = f.dir.path().join("node-code.toml");
    write_code_node_cfg(&cfg, &data, f.port, &f.tls_fp);

    let out = node_run(&cfg, &["--code", &code]);
    assert_eq!(out.status.code(), Some(0), "旧 --code 必须照旧能用：{}", stderr(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["paired"].as_bool(), Some(true), "{v}");
    assert!(state::token_path(&data).exists());

    // `--code` 与 `--invite` 同给是**用法错误**（互斥），不是"先试一个再试另一个"。
    let invite = f.invite(&[]);
    let out = node_run(
        &cfg,
        &["--code", &code, "--invite", invite["ticket"].as_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(3), "两个凭证同给必须退 3：{}", stderr(&out));
}
