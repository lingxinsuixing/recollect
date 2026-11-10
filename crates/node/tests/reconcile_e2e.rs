//! B3b 双进程 E2E：暂存层 → `POST /v1/rows` → 中心确认 → pull → 丢层（判据 3⑤–3⑨）。
//!
//! 夹具照 `e2e_two_process.rs`：真 `recollect-hub` 进程（`serve` + `--dev-loopback`）与真
//! `recollect-node` 配对；节点的**同步**走库面 [`node::sync::sync_once`]——工单 §二.3 的
//! `SyncOutcome.rows_uploaded` / `reconciled` 只有库面读得到（`main.rs` 不在本批可改范围），
//! 网络、签名与吸收仍是真 HTTP 对真中心。
//!
//! 暂存层由测试直接播种：一张新卡 + 它的 `provenance`，两行都进 `staging_new_row`。
//! **不跑真 LLM**（派发档 §三）。
//!
//! 状态机那两格按 `run_sync` 的同一条接缝观察：成功路径里 `observe_success` 把
//! `Reconciling` 清回 `normal`（判据 3⑤），没走完的回并则停在 `Reconciling`（判据 3⑧）。

use node::state::{self, SelfDistill, SyncState};
use node::sync::{self, Mode, SyncOutcome, SyncPlan};
use node::staging;
use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_store::Root;
use serde_json::Value;
use std::cell::RefCell;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};

/// 暂存层里那张新卡的记号：断言中心收到的是这一行，而不是底座里的行。
const CANARY: &str = "RECONCILE-CANARY-7742";
/// 新卡的出处。回并后它必须出现在中心的 `pre_distilled_turn` 里。
const TURN_UID: &str = "turn-reconcile-e2e-1";
/// 判据 12a：库代码不读钟；同步的时间由调用方给。
const NOW_MS: i64 = 10_000;

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

/// 中心记忆库里先放一张卡：`with_snapshot=true` 时它会被发布成第一代快照，
/// 节点的 pull 因此有东西可下（判据 3⑤/3⑦ 需要"pull 成功"；3⑧ 刻意不要）。
fn seed_hub_memory(data: &Path) {
    let root = Root::open(data).unwrap();
    put_card(
        root.memory(),
        &FactContent {
            kind: "decision".into(),
            title: "中心底座里的卡".into(),
            body: "the base row the hub already had".into(),
            scope: Scope { kind: ScopeKind::Project, key: "reconcile-e2e".into() },
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: "turn-base-e2e".into(), anchor_role: AnchorRole::Human }],
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
    port: u16,
    /// 端口是"绑一下再放"拿到的：同一个二进制里的真进程用例并行会撞端口，配对打到
    /// 另一台的 hub 上（`not_paired`）。最后声明 ⇒ 最后释放。
    _serial: std::sync::MutexGuard<'static, ()>,
}

static PORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Fixture {
    /// `with_snapshot`：中心是否在同步之前就发布过一代快照。
    fn new(with_snapshot: bool) -> Fixture {
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

        if with_snapshot {
            let out = hub_run(&hub_data, &["snapshot", "publish"]);
            assert_eq!(
                out.status.code(),
                Some(0),
                "hub snapshot publish: {}",
                stderr(&out)
            );
        }

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

        Fixture { _hub: hub, _dir: dir, hub_data, node_data, port, _serial: serial }
    }

    fn endpoint(&self) -> rc_net::node::Endpoint {
        rc_net::node::Endpoint {
            label: "loopback".into(),
            host: "127.0.0.1".into(),
            port: self.port,
            tls_fp: None,
        }
    }
}

/// 直接播种暂存层：一张新卡（带 provenance）+ 两行 `staging_new_row`。
///
/// 底座用一份**新建的**记忆库：B1b 允许"离线时副本不存在 ⇒ 暂存层从空记忆库开始"，
/// 本用例关心的就是这层里的新行。返回那张卡的 id（中心侧按它查行）。
fn seed_staging(fx: &Fixture, canary: &str, turn_uid: &str) -> String {
    let scratch = tempfile::tempdir().unwrap();
    let key = {
        let root = Root::open(scratch.path()).unwrap();
        let key = put_card(
            root.memory(),
            &FactContent {
                kind: "lesson".into(),
                title: "离线期新蒸出来的卡".into(),
                body: format!("{canary} 中心回来之后这一行必须先回传"),
                scope: Scope { kind: ScopeKind::Project, key: "reconcile-e2e".into() },
                valid_to_ms: None,
            },
            &[Provenance { turn_uid: turn_uid.into(), anchor_role: AnchorRole::Human }],
            500,
        )
        .unwrap();
        // WAL 的提交在最后一个连接关闭时落回主文件；先关再拷，拷贝才是完整的。
        drop(root);
        key
    };
    let pending = staging::path(&fx.node_data);
    std::fs::copy(scratch.path().join("memory.db"), &pending).unwrap();
    {
        let root = rc_store::AgentRoot::open_replica_rw(&fx.node_data, &pending).unwrap();
        root.memory()
            .conn()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS staging_new_row (
                   id TEXT PRIMARY KEY,
                   table_name TEXT NOT NULL
                 )",
            )
            .unwrap();
        // 键表达式与 `selfdistill::KEY_SQL` 同一套：card 取句柄，provenance 取三段拼接。
        let markers = [
            (format!("card:{}", key.id), "card"),
            (format!("card:{}:{turn_uid}", key.id), "provenance"),
        ];
        for (id, table) in markers {
            root.memory()
                .conn()
                .execute(
                    "INSERT INTO staging_new_row(id, table_name) VALUES(?1, ?2)",
                    (id, table),
                )
                .unwrap();
        }
    }
    key.id
}

/// 库面跑一轮同步：真签名、真 HTTP、真中心；节点状态照 `run_sync` 的读法从盘上取回。
fn sync_round(
    fx: &Fixture,
    mode: Mode,
    now_ms: i64,
) -> Result<(SyncOutcome, SyncState), sync::SyncError> {
    let key = rc_net::key::DeviceKey::load(&fx.node_data.join("device.key")).unwrap();
    let token = std::fs::read_to_string(state::token_path(&fx.node_data))
        .unwrap()
        .trim()
        .to_string();
    let mut st = state::load(&state::state_path(&fx.node_data))
        .unwrap()
        .unwrap_or_default();
    let plan = SyncPlan {
        endpoints: vec![fx.endpoint()],
        token,
        device_id: key.device_id(),
        key: &key,
        hub_pub: st.paired_hub_pub.clone(),
        data_dir: fx.node_data.clone(),
        // 空表 = 本计划不发现（S4d1 的关闭开关）。
        discovery: Vec::new(),
        discovery_timeout: Duration::from_secs(1),
        // 与 `run_sync` 同源：这一格在发现关闭时没有读者。
        hub_tls_fp: st.paired_tls_fp.clone(),
    };
    let out = sync::sync_once(&plan, &mut st, mode, now_ms, Duration::from_secs(10))?;
    state::save(&state::state_path(&fx.node_data), &st).unwrap();
    Ok((out, st))
}

/// 判据 3⑦ 的第一发：手工把同一载荷 POST 给中心（模拟"中心已吸收、ack 在路上丢了"）。
fn manual_upload(fx: &Fixture, body: &[u8]) -> rc_net::node::RowsAck {
    let key = rc_net::key::DeviceKey::load(&fx.node_data.join("device.key")).unwrap();
    let token = std::fs::read_to_string(state::token_path(&fx.node_data))
        .unwrap()
        .trim()
        .to_string();
    let st = state::load(&state::state_path(&fx.node_data)).unwrap().unwrap();
    let device_id = key.device_id();
    let auth = rc_net::node::NodeAuth {
        token: &token,
        device_id: &device_id,
        key: &key,
        hub_pub: &st.paired_hub_pub,
    };
    rc_net::node::upload_rows(&fx.endpoint(), &auth, body, Duration::from_secs(10)).unwrap()
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

/// 判据 3⑤：暂存层的新行到了中心、turn 被标记、层被丢、状态清回 `normal`。
#[test]
fn a_queued_batch_reaches_the_center_and_the_layer_is_dropped() {
    let fx = Fixture::new(true);
    let card_id = seed_staging(&fx, CANARY, TURN_UID);

    let (out, mut st) = sync_round(&fx, Mode::Both, NOW_MS).expect("一轮同步要成功");
    assert_eq!(out.rows_uploaded, 2, "卡 + 出处两行都要被吸收：{out:?}");
    assert!(out.reconciled, "rows 过了、pull 成功、层已丢：{out:?}");
    assert_eq!(hub_card_count(&fx.hub_data, &card_id), 1, "中心记忆库要有那张卡");
    assert_eq!(
        marked_turns(&fx.hub_data, TURN_UID),
        1,
        "pre_distilled_turn 要有对应 turn"
    );
    assert!(!staging::is_active(&fx.node_data), "回并成功后整层丢弃");
    // `run_sync` 的成功路径：同一句 `observe_success` 把回并态清回 normal。
    node::offline::observe_success(&mut st, NOW_MS + 1);
    assert_eq!(st.self_distill, SelfDistill::Normal, "成功回并之后状态回 normal");
}

/// 判据 3⑥：`reconcile_then_pull` 的调用顺序必须是 rows → pull。
///
/// 反序注入（实现里先 pull 再 rows，或在本用例里调换两个闭包）⇒ 下面的顺序断言当场红。
#[test]
fn rows_are_uploaded_before_the_snapshot() {
    let calls = RefCell::new(Vec::<&'static str>::new());
    let (uploaded, pulled) = sync::reconcile_then_pull(
        || {
            calls.borrow_mut().push("rows");
            Ok(3)
        },
        || {
            calls.borrow_mut().push("pull");
            Ok(sync::Pulled { pulled: true, no_memory: false, resumes: 0, rate_bps: 0 })
        },
    )
    .unwrap();
    assert_eq!(
        *calls.borrow(),
        vec!["rows", "pull"],
        "硬序是 rows → pull；反序注入必须在这里红"
    );
    assert_eq!(uploaded, 3, "rows 的计数要原样传出来");
    assert!(pulled.pulled, "pull 的结果要原样传出来");
}

/// 判据 3⑦：ack 丢失后重放同一载荷 ⇒ `absorbed=0`、中心仍一行、层被丢。
#[test]
fn a_lost_ack_replays_without_duplicating_rows() {
    let fx = Fixture::new(true);
    let card_id = seed_staging(&fx, CANARY, TURN_UID);

    // 第一发：中心已经吸收，节点却没看到应答。
    let body = node::reconcile::build_rows_payload(&fx.node_data)
        .unwrap()
        .expect("标记非空 ⇒ 必须有载荷");
    let ack = manual_upload(&fx, &body);
    assert_eq!(ack.rows_in, 2, "{ack:?}");
    assert_eq!(ack.absorbed, 2, "{ack:?}");
    assert_eq!(hub_card_count(&fx.hub_data, &card_id), 1);

    // 第二轮：同一载荷再发一次；中心全判重复，层照丢。
    let (out, _st) = sync_round(&fx, Mode::Both, NOW_MS).unwrap();
    assert_eq!(out.rows_uploaded, 0, "重放必须 absorbed=0：{out:?}");
    assert!(out.reconciled, "重复不等于失败：{out:?}");
    assert_eq!(hub_card_count(&fx.hub_data, &card_id), 1, "重放不许长出第二行");
    assert!(!staging::is_active(&fx.node_data), "确认之后层被丢");
}

/// 判据 3⑧：中心还没有快照 ⇒ rows 上传成功但层**保留**，状态停在 `Reconciling`。
#[test]
fn a_failed_snapshot_publish_keeps_the_layer() {
    let fx = Fixture::new(false);
    let card_id = seed_staging(&fx, CANARY, TURN_UID);
    // 让中心的 `snapshots` 目录建不出来：`create_dir_all` 撞上一个同名文件 ⇒ 发布失败。
    std::fs::write(fx.hub_data.join("snapshots"), b"blocked").unwrap();

    let err = sync_round(&fx, Mode::Both, NOW_MS).unwrap_err();
    assert_eq!(err, sync::SyncError::RowsRejected, "中心发布快照失败必须是 rows_failed");
    assert_eq!(hub_card_count(&fx.hub_data, &card_id), 1, "行已经吸收，不许丢");
    assert!(staging::is_active(&fx.node_data), "快照没发布成功 ⇒ 不许丢层");
}

/// 判据 3⑨：`--push` 只上行 turn，不发 rows、不丢层。
#[test]
fn a_push_only_run_does_not_upload_rows_or_drop_the_layer() {
    let fx = Fixture::new(false);
    let card_id = seed_staging(&fx, CANARY, TURN_UID);

    let (out, _st) = sync_round(&fx, Mode::Push, NOW_MS).unwrap();
    assert_eq!(out.rows_uploaded, 0, "--push 不许上行行载荷：{out:?}");
    assert!(!out.reconciled, "--push 不碰暂存层：{out:?}");
    assert!(staging::is_active(&fx.node_data), "--push 不许丢层");
    assert_eq!(hub_card_count(&fx.hub_data, &card_id), 0, "--push 不许把卡送到中心");
}
