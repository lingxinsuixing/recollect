//! B6：一整条生命周期 —— 同步 → 断连 1 天 → 回并 → 再断连 5 天 → 自升 + 自备份 →
//! 原中心回来 → 回并 → 不重蒸（判据 3㉓）。
//!
//! 夹具照 `detach_reconcile.rs` / `e2e_two_process.rs`：**真 `recollect-hub` 与真
//! `recollect-node` 二进制**，配对、发布快照；hub 用「杀进程 / 同数据目录同端口重启」模拟
//! 断连与原中心回来。暂存层的新行由测试直接播种（`staging::prepare` + `put_card` + 行标），
//! **不跑真 LLM**：离线蒸馏已由 B2b 的脚本后端单独证明，这里只证明状态与回并的整条链。
//!
//! 时间全部注入：离线天数写进状态文件（`offline_elapsed_ms`），暂存层与卡用固定时刻播种。
//! 唯一由真进程取的钟是 `main.rs` 入口的 `now_ms`（判据 12a 的白名单），它只用来给
//! `reconcile_confirmed_at_ms` 与 `dr/backup-<ts>` 落一个真实时刻，判定不读它。
//!
//! 两个 turn 同时是**回并的 provenance 出处**与**hub 转录层里的真 turn**：只有后者成立，
//! 「第 6 步不重蒸」才有牙——把 `pre_distilled_uids` 的排除面删掉，中心会把这两条重新选中。

use node::offline::OFFLINE_GATE_MS;
use node::staging;
use node::state::{self, SelfDistill};
use rc_ledger::water::Cursor;
use rc_memory::{put_card, set_verdict, AnchorRole, FactContent, Provenance, Scope, ScopeKind, Verdict};
use rc_store::Root;
use serde_json::Value;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};

/// 两次断连各播种一张新卡；两段文本不同 ⇒ 内容寻址的 id 不同。
const CARD_A_CANARY: &str = "LIFECYCLE-CARD-A-1743";
const CARD_B_CANARY: &str = "LIFECYCLE-CARD-B-9016";
/// 两条离线期自蒸轮的原文。它们先真上行到中心转录层，再作为 provenance 的出处被回并标记。
const TURN_A_BODY: &str = "the turn distilled into card A while the centre was gone";
const TURN_B_BODY: &str = "the turn distilled into card B on the fifth offline day";
const SESSION_KEY: &str = "lifecycle-session-1";
/// 建层与播种的注入时刻（库代码不取钟）。
const SEED_A_MS: i64 = 10_000;
const SEED_B_MS: i64 = 20_000;

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

/// `auto_unlock=true` 是同一条链的一部分：第 5 天闸要开得起来。箱不放 —— 第 6 天的
/// 状态是 `detached`，离线 cycle 那一支根本不进，正好只走 DR 触发。
fn write_node_cfg(path: &Path, data: &Path, port: u16) {
    let text = format!(
        "data_dir = \"{}\"\ndevice_key_path = \"{}\"\nauto_unlock = true\n[[endpoints]]\nlabel = \"loopback\"\nhost = \"127.0.0.1\"\nport = {port}\n",
        toml_path(data),
        toml_path(&data.join("device.key")),
    );
    std::fs::write(path, text).unwrap();
}

/// 往节点本机转录层写两条真 turn（uid 用中心会重算的那个口径），返回它们的 uid。
/// 它们在第 1 步真上行到中心：第 6 步「不重蒸」的排除面必须能在这两条上咬合。
fn seed_turns(data: &Path, device_id: &str) -> (String, String) {
    let root = rc_store::NodeRoot::open_node(data, None).unwrap();
    let sid = rc_contract::SessionId {
        session_key: SESSION_KEY.into(),
        device_id: device_id.to_string(),
        source_agent: "agent".into(),
        machine: None,
        project_hint: None,
        last_saved_at_ms: None,
        owner_principal_id: None,
        principal_name: None,
        principal_source: None,
    };
    let turns: Vec<rc_contract::NormalizedTurn> = [(TURN_A_BODY, 100i64), (TURN_B_BODY, 101i64)]
        .iter()
        .map(|(body, at)| rc_contract::NormalizedTurn {
            turn_uid: rc_contract::compute_turn_uid(device_id, SESSION_KEY, "agent", body, *at),
            body_fp: rc_contract::compute_body_fp(body),
            tier: rc_contract::TurnTier::ContentOnly,
            body: (*body).to_string(),
            role: "agent".into(),
            created_at_ms: *at,
        })
        .collect();
    let uids: Vec<String> = turns.iter().map(|t| t.turn_uid.clone()).collect();
    rc_ledger::insert_batch(root.transcript(), &sid, &turns, 500).unwrap();
    (uids[0].clone(), uids[1].clone())
}

struct Fixture {
    /// 先声明：drop 顺序 = 声明顺序，hub 必须先被杀掉再删临时目录。
    hub: Option<HubProc>,
    dir: tempfile::TempDir,
    hub_data: PathBuf,
    node_data: PathBuf,
    node_cfg: PathBuf,
    hub_cfg: PathBuf,
    hub_err: PathBuf,
    port: u16,
    turn_a: String,
    turn_b: String,
    /// 端口是"绑一下再放"拿到的：同一个二进制里的真进程用例并行会撞端口，配对打到
    /// 另一台的 hub 上（`not_paired`）。最后声明 ⇒ 最后释放。
    _serial: std::sync::MutexGuard<'static, ()>,
}

static PORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Fixture {
    /// 中心发布一代空快照（库是新建的，读面自有其表）；节点配对、写两条本机 turn，
    /// 等第一次 `--once` 上行。
    fn new() -> Fixture {
        let serial = PORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let hub_data = dir.path().join("hub");
        let node_data = dir.path().join("node");
        std::fs::create_dir_all(&hub_data).unwrap();
        std::fs::create_dir_all(&node_data).unwrap();

        rc_net::key::HubKey::load_or_create(&hub_data.join("hub.key")).unwrap();

        // 配对码的签发不联网：先在 serve 起来之前做，少一个"两个进程同时打开账本"的窗口。
        let out = hub_run(&hub_data, &["pair"]);
        assert_eq!(out.status.code(), Some(0), "hub pair: {}", stderr(&out));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        let code = v["code"].as_str().unwrap().to_string();

        let out = hub_run(&hub_data, &["snapshot", "publish"]);
        assert_eq!(out.status.code(), Some(0), "hub snapshot publish: {}", stderr(&out));

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

        // 设备钥匙先落盘再播种 turn：uid 的口径里有 device_id，配对时用的是同一把。
        let device = rc_net::key::DeviceKey::generate().unwrap();
        let device_id = device.device_id();
        device.save(&node_data.join("device.key")).unwrap();
        let (turn_a, turn_b) = seed_turns(&node_data, &device_id);

        let hub_err = dir.path().join("hub.err");
        let mut fx = Fixture {
            hub: None,
            dir,
            hub_data,
            node_data,
            node_cfg,
            hub_cfg,
            hub_err,
            port,
            turn_a,
            turn_b,
            _serial: serial,
        };
        fx.start_hub();

        let out = node_run(&fx.node_cfg, &["--code", &code]);
        assert_eq!(out.status.code(), Some(0), "node pair: {}", stderr(&out));

        fx
    }

    fn start_hub(&mut self) {
        let err = std::fs::File::create(&self.hub_err).unwrap();
        let child = Command::new(hub_bin())
            .arg("serve")
            .arg("--hub-config")
            .arg(&self.hub_cfg)
            .arg("--data-dir")
            .arg(&self.hub_data)
            .arg("--dev-loopback")
            .stdout(Stdio::null())
            .stderr(Stdio::from(err))
            .spawn()
            .unwrap();
        self.hub = Some(HubProc(child));
        wait_ready(self.port, &self.hub_err);
    }

    fn stop_hub(&mut self) {
        // `take` 后立刻丢：`HubProc::drop` 会 kill + wait，端口与库锁都放干净。
        if let Some(h) = self.hub.take() {
            drop(h);
        }
    }

    fn node(&self, extra: &[&str]) -> Output {
        node_run(&self.node_cfg, extra)
    }
}

/// 在暂存层里播种一张离线期新蒸的卡 + 它的 provenance，并把两行的句柄写进
/// `staging_new_row`。键表达式与 `selfdistill::KEY_SQL` 同一套。返回那张卡的 id。
fn seed_new_card(data: &Path, canary: &str, turn_uid: &str, now_ms: i64) -> String {
    let pending = staging::path(data);
    let root = Root::open_with_memory(data, &pending).expect("暂存层可写打开");
    let key = put_card(
        root.memory(),
        &FactContent {
            kind: "lesson".into(),
            title: "离线期新蒸出来的卡".into(),
            body: format!("{canary} 原中心回来后这一行必须先回传"),
            scope: Scope { kind: ScopeKind::Project, key: "lifecycle".into() },
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: turn_uid.into(), anchor_role: AnchorRole::Human }],
        now_ms,
    )
    .expect("播种一张卡");
    // 这两张卡在夹具里是"已经审过"的行：第 6 步要读的是**抽取族**的不重蒸，
    // 不是审核族（`rows_http` 的卡行同样直接写 `verdict=passed`）。
    set_verdict(root.memory(), &key, Verdict::Passed, now_ms).expect("把播种的卡标成已审");
    root.memory()
        .conn()
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS staging_new_row (
               id TEXT PRIMARY KEY,
               table_name TEXT NOT NULL
             )",
        )
        .unwrap();
    for (id, table) in [
        (format!("card:{}", key.id), "card"),
        (format!("card:{}:{turn_uid}", key.id), "provenance"),
    ] {
        root.memory()
            .conn()
            .execute(
                "INSERT OR REPLACE INTO staging_new_row(id, table_name) VALUES(?1, ?2)",
                (id, table),
            )
            .unwrap();
    }
    key.id
}

fn hub_card_count(data: &Path, id: &str) -> i64 {
    let root = Root::open(data).unwrap();
    root.memory()
        .conn()
        .query_row("SELECT count(*) FROM card WHERE id=?1", [id], |r| r.get(0))
        .unwrap()
}

fn marked_turns(data: &Path, uid: &str) -> i64 {
    let root = Root::open(data).unwrap();
    root.ledger()
        .conn()
        .query_row(
            "SELECT count(*) FROM pre_distilled_turn WHERE turn_uid=?1",
            [uid],
            |r| r.get(0),
        )
        .unwrap()
}

fn hub_turns(data: &Path) -> i64 {
    let root = Root::open(data).unwrap();
    root.transcript()
        .conn()
        .query_row("SELECT count(*) FROM turns", [], |r| r.get(0))
        .unwrap()
}

/// 节点**当前读面**（索引副本优先）里有没有这张卡。
fn card_in_current_replica(data: &Path, id: &str) -> bool {
    let Some(replica) = state::current_replica_path(data) else { return false };
    let Ok(memory) = rc_store::open_replica(&replica) else { return false };
    memory
        .conn()
        .query_row("SELECT count(*) FROM card WHERE id=?1", [id], |r| r.get::<_, i64>(0))
        .map(|n| n == 1)
        .unwrap_or(false)
}

fn backup_dirs(data: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(data.join("dr"))
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_dir()
                        && p.file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with("backup-"))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// 一份自备份的两段清单：下行段与随备份落地的中心签名清单原文本同代，本机段点名三个文件。
///
/// 拿**备份自己的**签名清单原文对，而不是拿测试当下的状态对：旧份天生属于旧代，
/// 保留它们正是"确认只解锁窗口"的意思。
fn assert_two_segment_backup(dir: &Path) {
    let doc: Value =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(
        doc["downlink"]["signed_manifest"].as_str(),
        Some("center-manifest.json"),
        "{doc}"
    );
    let signed: Value =
        serde_json::from_slice(&std::fs::read(dir.join("center-manifest.json")).unwrap()).unwrap();
    assert_eq!(doc["downlink"]["epoch"], signed["epoch"], "下行段的代际与签名清单不符：{doc}");
    assert_eq!(doc["downlink"]["seq"], signed["seq"], "下行段的序号与签名清单不符：{doc}");
    assert_eq!(
        doc["downlink"]["sha256"], signed["sha256"],
        "下行段的 sha 与签名清单不符：{doc}"
    );
    let files = doc["local"]["files"].as_object().expect("本机段必须带 files 表");
    for name in ["transcript.db", "pending.db", "node-state.json"] {
        assert!(files.contains_key(name), "本机段缺 {name}：{files:?}");
        assert!(dir.join(name).is_file(), "本机段点名的 {name} 必须真的在");
    }
}

/// 判据 3㉓：同步 → 断连 1 天 → 回并 → 再断连 5 天 → 自升 + 自备份 → 原中心回来 → 回并 →
/// 不重蒸，全程真进程。
#[test]
fn the_whole_lifecycle_from_offline_to_detached_and_back() {
    let mut fx = Fixture::new();

    // ---- 1. 初始同步：两条 turn 上行；一代快照下行 ----
    let out = fx.node(&["--once"]);
    assert_eq!(out.status.code(), Some(0), "初始同步：{}", stderr(&out));
    let v: Value = serde_json::from_slice(&out.stdout).expect("sync 是一行 JSON");
    assert_eq!(v["pushed"].as_i64(), Some(2), "两条本机 turn 都要上行：{v}");
    assert_eq!(v["pulled"].as_bool(), Some(true), "第一次下行必须拿到副本：{v}");
    assert_eq!(hub_turns(&fx.hub_data), 2, "中心转录层要有这两条 turn");
    let path = state::state_path(&fx.node_data);
    let st = state::load(&path).unwrap().expect("第一次同步之后状态文件必须在");
    let first_epoch = st.current_manifest.epoch;
    let first_seq = st.current_manifest.seq;
    assert!(first_epoch > 0 && first_seq > 0, "{st:?}");
    assert!(
        state::current_replica_path(&fx.node_data).is_some_and(|p| p.is_file()),
        "第一次同步之后必须有一代可读副本"
    );

    // ---- 2. 第一次断连：离线一天档；播种卡 A ----
    fx.stop_hub();
    let mut st = state::load(&path).unwrap().unwrap();
    st.self_distill = SelfDistill::OfflineDistilling;
    st.offline_elapsed_ms = 2 * OFFLINE_GATE_MS;
    state::save(&path, &st).unwrap();
    staging::prepare(&fx.node_data, &st.current_manifest, SEED_A_MS).expect("暂存层要建得起来");
    let card_a = seed_new_card(&fx.node_data, CARD_A_CANARY, &fx.turn_a, SEED_A_MS);
    assert!(staging::is_active(&fx.node_data), "断连期的新行必须住在暂存层");

    // ---- 3. 第一次回并：中心吸收后立刻发布新代，节点拉到新代 ----
    fx.start_hub();
    let out = fx.node(&["--once"]);
    assert_eq!(out.status.code(), Some(0), "第一次回并：{}", stderr(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["pulled"].as_bool(), Some(true), "吸收后必须发布新快照并被拉下：{v}");
    assert_eq!(hub_card_count(&fx.hub_data, &card_a), 1, "中心记忆库要有卡 A");
    assert_eq!(marked_turns(&fx.hub_data, &fx.turn_a), 1, "pre_distilled_turn 要有 A 的 turn");
    assert!(!staging::path(&fx.node_data).exists(), "回并成功后整层丢弃");
    let st = state::load(&path).unwrap().unwrap();
    assert_eq!(st.self_distill, SelfDistill::Normal, "成功回并之后状态回 normal");
    assert!(st.reconcile_confirmed_at_ms > 0, "中心确认收下 ⇒ 保留锚必须写下来");
    assert!(
        st.current_manifest.seq > first_seq,
        "节点当前副本必须换到吸收后发布的新代：{} -> {}",
        first_seq,
        st.current_manifest.seq
    );
    assert_eq!(st.current_manifest.epoch, first_epoch, "同一次安装的 epoch 不变");
    let confirmed_a = st.reconcile_confirmed_at_ms;
    assert!(card_in_current_replica(&fx.node_data, &card_a), "节点当前副本里也要有卡 A");

    // ---- 4. 第二次断连：离线六天；自升 detached 并自备份 ----
    fx.stop_hub();
    let mut st = state::load(&path).unwrap().unwrap();
    st.self_distill = SelfDistill::DetachedDistilling;
    st.offline_elapsed_ms = 6 * OFFLINE_GATE_MS;
    st.offline_since_ms = 0;
    state::save(&path, &st).unwrap();
    staging::prepare(&fx.node_data, &st.current_manifest, SEED_B_MS).expect("暂存层要重建得起来");
    let card_b = seed_new_card(&fx.node_data, CARD_B_CANARY, &fx.turn_b, SEED_B_MS);

    let out = fx.node(&["--once"]);
    assert_eq!(out.status.code(), Some(3), "中心不在：{}", stderr(&out));
    let mut st = state::load(&path).unwrap().unwrap();
    assert_eq!(st.last_error, "endpoint_unreachable", "{st:?}");
    assert_eq!(st.self_distill, SelfDistill::DetachedPromoted, "自备份成功 ⇒ detached_promoted");
    assert!(st.dr_backup_count >= 1, "{st:?}");
    assert!(st.dr_backup_last_at_ms > 0, "{st:?}");
    let first_backups = backup_dirs(&fx.node_data);
    assert_eq!(first_backups.len(), 1, "第一次失败同步要落一份自备份");
    assert_two_segment_backup(&first_backups[0]);
    let doc: Value = serde_json::from_slice(
        &std::fs::read(first_backups[0].join("manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        doc["downlink"]["seq"].as_u64(),
        Some(st.current_manifest.seq),
        "这一份备份的下行段必须是回并之后的那一代：{doc}"
    );

    // 时间注入：把"上次备份"清零 ⇒ 下一次失败同步再次到期（不等待、不取钟）。
    st.dr_backup_last_at_ms = 0;
    state::save(&path, &st).unwrap();
    let out = fx.node(&["--once"]);
    assert_eq!(out.status.code(), Some(3), "中心仍然不在：{}", stderr(&out));
    let st = state::load(&path).unwrap().unwrap();
    assert_eq!(st.self_distill, SelfDistill::DetachedPromoted);
    assert_eq!(st.dr_backup_count, 2, "第二轮到期的自备份也要计上");
    let backups = backup_dirs(&fx.node_data);
    assert_eq!(backups.len(), 2, "两份 dr/backup-* 都要在");
    assert!(first_backups[0].is_dir(), "第二份不许把第一份挤掉");

    // ---- 5. 原中心回来：卡 B 回并；确认时刻更新；两份自备份一份不删 ----
    fx.start_hub();
    let out = fx.node(&["--once"]);
    assert_eq!(out.status.code(), Some(0), "第二次回并：{}", stderr(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["pulled"].as_bool(), Some(true), "卡 B 吸收后同样要下发新代：{v}");
    assert_eq!(hub_card_count(&fx.hub_data, &card_a), 1, "卡 A 仍在中心");
    assert_eq!(hub_card_count(&fx.hub_data, &card_b), 1, "中心记忆库要有卡 B");
    assert_eq!(marked_turns(&fx.hub_data, &fx.turn_a), 1, "A 的 turn 标记仍在");
    assert_eq!(marked_turns(&fx.hub_data, &fx.turn_b), 1, "pre_distilled_turn 要有 B 的 turn");
    assert!(!staging::path(&fx.node_data).exists(), "第二次回并后整层也要丢弃");
    let st = state::load(&path).unwrap().unwrap();
    assert_eq!(st.self_distill, SelfDistill::Normal, "原中心回来 ⇒ 状态回 normal");
    assert!(
        st.reconcile_confirmed_at_ms > confirmed_a,
        "确认时刻要更新：{confirmed_a} -> {}",
        st.reconcile_confirmed_at_ms
    );
    assert_eq!(st.dr_backup_count, 2, "自备份份数不许被回并抹掉");
    let after = backup_dirs(&fx.node_data);
    assert_eq!(after.len(), 2, "确认只解锁保留窗口，不立即删任何一份");
    for dir in &after {
        assert_two_segment_backup(dir);
    }
    assert!(card_in_current_replica(&fx.node_data, &card_a), "节点当前副本里要有卡 A");
    assert!(card_in_current_replica(&fx.node_data, &card_b), "节点当前副本里也要有卡 B");

    // 节点只有一个说法：normal，不是 detached，暂存层没有新行。
    let out = fx.node(&[]);
    assert_eq!(out.status.code(), Some(0), "status：{}", stderr(&out));
    let v: Value = serde_json::from_slice(&out.stdout).expect("status 是一行 JSON");
    assert_eq!(v["self_distill"].as_str(), Some("normal"), "{v}");
    assert_eq!(v["detached"].as_bool(), Some(false), "{v}");
    assert_eq!(v["staging_rows"].as_u64(), Some(0), "{v}");
    assert_eq!(v["dr_backup_count"].as_u64(), Some(2), "{v}");

    // ---- 6. 不重蒸：中心下一轮一条都不许再选 ----
    // 先把 serve 停下：cycle 是批作业，独占同一个数据目录，不留两进程写同一本账的窗口。
    fx.stop_hub();
    // 推导族不在这条用例的断言面：两张卡是推导池的合法输入，`--replies []` 若让 derive
    // 起批，脚本会在那里耗尽、把这一轮读成 3，第 6 步就不再只是「抽取不重蒸」的读数。
    // 夹具把推导游标放到这两张卡之后（等价于"中心已经推过这一批"），让这一轮只剩抽取族可读。
    {
        let root = Root::open(&fx.hub_data).unwrap();
        rc_ledger::water::advance(
            root.ledger(),
            "derive",
            "",
            &Cursor { at_ms: SEED_B_MS, turn_uid: format!("card:{card_b}") },
            SEED_B_MS,
        )
        .unwrap();
    }
    let replies = fx.dir.path().join("empty-replies.json");
    std::fs::write(&replies, "[]").unwrap();
    let out = Command::new(hub_bin())
        .env_remove("RECOLLECT_DEVICE_ID")
        .arg("cycle")
        .arg("once")
        .arg("--replies")
        .arg(&replies)
        .arg("--data-dir")
        .arg(&fx.hub_data)
        .output()
        .expect("hub binary");
    let v: Value = serde_json::from_slice(&out.stdout).expect("cycle 是一行 JSON");
    assert_eq!(
        v["turns_scanned"].as_u64(),
        Some(0),
        "已回并的 turn 被重新选中了：{v}"
    );
    assert_eq!(out.status.code(), Some(0), "cycle：{}", stderr(&out));
    assert_eq!(v["rows_written"].as_u64(), Some(0), "{v}");
    let root = Root::open(&fx.hub_data).unwrap();
    // S3a-fix（fix round 1）：与 `hub/tests/rows_http.rs` 同一条夹具同一条口径——这一句断的是
    // "这一轮一次 LLM 口都没开"，只数 `llm_calls`。S3a 之后 cycle 收尾无条件写
    // `llm_job:cycle:quota_stopped`（运行记录，不是调用记录），按主语整表数会把它读成"开过口"。
    let llm_job = rc_ledger::counters::read_all(root.ledger())
        .unwrap()
        .into_iter()
        .filter(|r| r.subject_kind == "llm_job" && r.metric == "llm_calls")
        .count();
    assert_eq!(llm_job, 0, "回并后的下一轮不该再开 LLM 口");
}
