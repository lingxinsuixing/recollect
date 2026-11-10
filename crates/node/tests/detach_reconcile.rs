//! B4b 双进程 E2E：detached 节点回并到原中心、确认后解锁保留窗口、detached 读数（判据 3⑮–3⑰）。
//!
//! 夹具照 `e2e_two_process.rs` / `reconcile_e2e.rs`：真 `recollect-hub` 进程（`serve` +
//! `--dev-loopback`）与真 `recollect-node` 二进制，配对、发布快照、节点先下行一代副本。
//! **回并那一轮必须走进程边界**：`reconcile_confirmed_at_ms` 的写点在 `main::run_sync` 的
//! 成功路径上（库不取钟，判据 12a），库面 `sync_once` 不碰那一格。
//!
//! 暂存层由测试直接播种：`staging::prepare` 从底座建层，`Root::open_with_memory` 里用
//! `put_card` 写一张新卡 + 它的 provenance，再往 `staging_new_row` 写两个行标。
//! **不跑真 LLM**（派发档 §二）。

use node::dr::rotate;
use node::staging;
use node::state::{self, SelfDistill, SyncState};
use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_store::Root;
use serde_json::Value;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};

/// 暂存层里那张新卡的记号：断言中心收到的是这一行。
const CANARY: &str = "DETACH-RECONCILE-CANARY-9183";
/// 新卡的出处。回并后它必须出现在中心的 `pre_distilled_turn` 里。
const TURN_UID: &str = "turn-detach-reconcile-1";
/// 建层与播种的时刻：库代码不取钟；同步的时间由调用方（`main.rs`）给。
const SEED_MS: i64 = 10_000;

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

/// 中心记忆库里先放一张卡：快照发布因此有内容，节点的第一次下行也确有其物。
fn seed_hub_memory(data: &Path) {
    let root = Root::open(data).unwrap();
    put_card(
        root.memory(),
        &FactContent {
            kind: "decision".into(),
            title: "中心底座里的卡".into(),
            body: "the base row the hub already had".into(),
            scope: Scope { kind: ScopeKind::Project, key: "detach-reconcile".into() },
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: "turn-base-detach".into(), anchor_role: AnchorRole::Human }],
        10,
    )
    .unwrap();
}

struct Fixture {
    /// 先声明：drop 顺序 = 声明顺序，hub 必须先被杀掉再删临时目录。
    _hub: HubProc,
    /// 只活到用例结束（hub/node 的 data_dir 都在它下面）。
    _dir: tempfile::TempDir,
    hub_data: PathBuf,
    node_data: PathBuf,
    node_cfg: PathBuf,
    /// 端口是"绑一下再放"拿到的：同一个二进制里的真进程用例并行会撞端口，配对打到
    /// 另一台的 hub 上（`not_paired`）。最后声明 ⇒ 最后释放。
    _serial: std::sync::MutexGuard<'static, ()>,
}

static PORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Fixture {
    /// 中心播种一张卡并发布一代快照；节点配对之后即可下行拿到副本。
    fn new() -> Fixture {
        let serial = PORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let hub_data = dir.path().join("hub");
        let node_data = dir.path().join("node");
        std::fs::create_dir_all(&hub_data).unwrap();
        std::fs::create_dir_all(&node_data).unwrap();

        rc_net::key::HubKey::load_or_create(&hub_data.join("hub.key")).unwrap();
        seed_hub_memory(&hub_data);

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

        let hub_err = dir.path().join("hub.err");
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

        Fixture { _hub: hub, _dir: dir, hub_data, node_data, node_cfg, _serial: serial }
    }
}

/// 在暂存层里播种一张离线期新蒸的卡 + 它的 provenance，并把两行的句柄写进
/// `staging_new_row`。键表达式与 `selfdistill::KEY_SQL` 同一套：card 取句柄，
/// provenance 取 `mem_table:mem_id:turn_uid`。返回那张卡的 id（中心侧按它查行）。
fn seed_new_card(data: &Path) -> String {
    let pending = staging::path(data);
    let key = {
        let root = Root::open_with_memory(data, &pending).expect("暂存层可写打开");
        let key = put_card(
            root.memory(),
            &FactContent {
                kind: "lesson".into(),
                title: "离线第五天新蒸出来的卡".into(),
                body: format!("{CANARY} 原中心回来后这一行必须先回传"),
                scope: Scope { kind: ScopeKind::Project, key: "detach-reconcile".into() },
                valid_to_ms: None,
            },
            &[Provenance { turn_uid: TURN_UID.into(), anchor_role: AnchorRole::Human }],
            500,
        )
        .expect("播种一张卡");
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
            (format!("card:{}:{TURN_UID}", key.id), "provenance"),
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
    };
    key
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

/// 判据 3⑮：detached 的节点连回原中心 ⇒ 行到中心、层丢、确认时刻写入、状态回 normal。
#[test]
fn a_detached_node_reconciles_with_the_original_centre() {
    let fx = Fixture::new();

    // 先正常下行一代：回并要有一份"原中心的底座"可对，`staging::prepare` 才有得复制。
    let out = node_run(&fx.node_cfg, &["--once"]);
    assert_eq!(out.status.code(), Some(0), "第一次同步：{}", stderr(&out));
    assert!(
        state::current_replica_path(&fx.node_data).is_some_and(|p| p.is_file()),
        "第一次同步之后必须有一代可读副本"
    );

    // 离线满五天：detached，且中心还没确认收下任何回并批次。
    let path = state::state_path(&fx.node_data);
    let mut st = state::load(&path).unwrap().unwrap();
    st.self_distill = SelfDistill::DetachedDistilling;
    st.reconcile_confirmed_at_ms = 0;
    state::save(&path, &st).unwrap();

    // 暂存层：从底座复制一份（带索引与行标表），再播种一张离线期新蒸的卡。
    staging::prepare(&fx.node_data, &st.current_manifest, SEED_MS).expect("暂存层要建得起来");
    let card_id = seed_new_card(&fx.node_data);

    // 原中心回来了：真二进制跑一轮 `--once`（push → rows → 确认 → pull → 丢层）。
    let out = node_run(&fx.node_cfg, &["--once"]);
    assert_eq!(out.status.code(), Some(0), "回并这一轮：{}", stderr(&out));

    assert_eq!(hub_card_count(&fx.hub_data, &card_id), 1, "中心记忆库要有那张卡");
    assert_eq!(marked_turns(&fx.hub_data, TURN_UID), 1, "pre_distilled_turn 要有对应 turn");
    assert!(!staging::path(&fx.node_data).exists(), "回并成功后整层丢弃");

    let st = state::load(&path).unwrap().unwrap();
    assert_eq!(st.self_distill, SelfDistill::Normal, "成功回并之后状态回 normal");
    assert!(
        st.reconcile_confirmed_at_ms > 0,
        "中心确认收下这一批 ⇒ 保留锚必须写下来：{}",
        st.reconcile_confirmed_at_ms
    );
}

/// 判据 3⑯：确认时刻才解锁保留窗口；未确认时一份都不删。
///
/// 8 份空目录、确认时刻 5、keep=1：1–4 早于确认且超出窗口 ⇒ 删；5–8 不早于确认 ⇒ 全在。
#[test]
fn the_confirmation_time_unlocks_pruning() {
    let dir = tempfile::tempdir().unwrap();
    let dr = dir.path().join("dr");
    for i in 1..=8 {
        std::fs::create_dir_all(dr.join(format!("backup-{i}"))).unwrap();
    }

    assert_eq!(
        rotate(dir.path(), 5, 1).unwrap(),
        4,
        "确认之后只许删早于确认且超出窗口的 1–4"
    );
    for i in 1..=4 {
        assert!(!dr.join(format!("backup-{i}")).exists(), "第 {i} 份早于确认且超窗，必须删掉");
    }
    for i in 5..=8 {
        assert!(dr.join(format!("backup-{i}")).is_dir(), "第 {i} 份不早于确认，一份都不许删");
    }

    // 未确认（0）：保留锚是事件不是时钟，一份都不许再少。
    assert_eq!(rotate(dir.path(), 0, 1).unwrap(), 0, "未确认时 rotate 必须为 0");
    for i in 5..=8 {
        assert!(dr.join(format!("backup-{i}")).is_dir(), "未确认不许删：第 {i} 份不见了");
    }
    let remaining = std::fs::read_dir(&dr).unwrap().flatten().count();
    assert_eq!(remaining, 4, "未确认的 rotate 必须一份不少地留下 5–8");
}

/// 判据 3⑰：detached 的节点只自称 detached，不许把读数写成"已同步"。
#[test]
fn a_detached_node_never_claims_it_is_in_sync() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("node");
    std::fs::create_dir_all(&data).unwrap();
    let cfg = dir.path().join("node.toml");
    write_node_cfg(&cfg, &data, free_port());

    // 离线满五天、中心还没确认：状态文件就停在这两格上。
    let st = SyncState {
        self_distill: SelfDistill::DetachedDistilling,
        reconcile_confirmed_at_ms: 0,
        ..SyncState::default()
    };
    state::save(&state::state_path(&data), &st).unwrap();
    seed_new_card(&data);

    // 无旗标 = status：只读状态文件与暂存层，不联网。
    let out = node_run(&cfg, &[]);
    assert_eq!(out.status.code(), Some(0), "status：{}", stderr(&out));
    let v: Value = serde_json::from_slice(&out.stdout).expect("status 是一行 JSON");
    assert_eq!(v["detached"].as_bool(), Some(true), "{v}");
    assert_eq!(v["reconcile_confirmed_at_ms"].as_i64(), Some(0), "{v}");
    assert_eq!(v["self_distill"].as_str(), Some("detached_distilling"), "{v}");
    assert!(
        v["staging_rows"].as_u64().unwrap_or(0) >= 1,
        "暂存层至少一行标记：{v}"
    );
}
