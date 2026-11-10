//! B2b-3：1 天闸的触发接线与 status 读数（判据 2㊵–2㊸）。
//!
//! 夹具照 `crates/node/tests/offline_cycle.rs`（底座副本 + manifest + 两条 turn + 脚本后端 + `Metered`），
//! 差别只有一处：**不预先建暂存层**——触发点（`run_one_offline_batch`）负责建。
//!
//! 判据 2㊵ 走**真进程**：`recollect-node --once` 打不通死端点（`endpoint_unreachable`），
//! 失败路径按 1 天闸开箱、就地跑一个批次。那条路上"脚本后端"的等价物是本机的脚本端点
//! （`llm.toml` 的 custom 通道 + `rc_net::http::serve`）：批次经 `Live` 真出网到这里，
//! 于是"脚本被问过"这一格在端到端路径上仍有生产读者——把失败路径的 `cfg.auto_unlock`
//! 换回写死的 `false`，它当场变红（`observe_failure` 拿到 false ⇒ `OfflineWaiting` ⇒ 不建层）。

use node::offline::{observe_failure, observe_success, OFFLINE_GATE_MS};
use node::selfdistill::{run_one_offline_batch, OfflineFault};
use node::staging;
use node::state::{self, CurrentManifest, SelfDistill, SyncState};
use node::sync::SyncError;
use rc_contract::{compute_body_fp, compute_turn_uid};
use rc_cycle::Metered;
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::{put_card, set_verdict, AnchorRole, FactContent, Provenance, Scope, ScopeKind,
                Verdict};
use rc_store::Root;
use std::cell::RefCell;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

const DEV: &str = "dev-trigger";
const SESS: &str = "sess-trigger";
const TURN_A_BODY: &str = "the base turn that the centre already distilled";
const TURN_B_BODY: &str = "the new turn written while the centre was away";
const A_AT: i64 = 1_000;
const B_AT: i64 = 2_000;
const NOW_MS: i64 = 9_000;
/// 判据 2㊵ 的离线起点：写死的**未来**纪元毫秒，让墙钟差恒为 0。
///
/// **为什么不是 `now - OFFLINE_GATE_MS`**：判据 12a 是全仓规则（取钟只许住五个 bin 的
/// `main.rs`，测试也算库代码），测试一读钟那条闸当场红。起点放在未来 ⇒ `now - since` 被
/// `saturating_sub` 折成 0；开闸的生产者改成状态里持久化的 `offline_elapsed_ms = 2 天`
/// （`observe_failure` 的三取 max）。这样既零钟，也不会随真实日历跨过第 5 天后变成 detached。
const OFFLINE_SINCE_MS: i64 = 9_000_000_000_000;
/// 持久化的离线时长：一天 < 2 天 < 五天，落在一档的窗口里。
const TWO_DAYS_MS: i64 = 2 * 86_400_000;
/// 新卡正文里的记号：断言"这条行是新蒸出来的"靠它，不靠标题。
const CANARY: &str = "TRIGGER-CANARY-7731";
/// 审核三票全过。逐字抄 `hub/tests/cycle_once.rs` 的 `YES`。
const YES: &str = r#"{"holds":true}"#;
/// 推导的"跑通但不落行"答复。逐字抄 `cycle_once.rs` 的 `DERIVE_QUIET`。
const DERIVE_QUIET: &str = r#"{"relationships":[],"chains":[{"from":["r1"],"mechanism":"requires","basis":["r1"]}],"timeline":[]}"#;
/// 读箱判据用的口令。值只经过文件，不进任何一条错误面。
const PASS: &[u8] = b"offline-trigger-passphrase-77";

/// 五键齐全、一条卡片、出处指向本轮那条新 turn。逐字抄 `offline_cycle.rs` 的 `extract_ok`
/// （只有卡片正文里的记号换了 `CANARY`）。
fn extract_ok(u: &str) -> String {
    serde_json::json!({"entities": [], "events": [], "assertions": [],
           "cards": [{"kind": "lesson", "title": "上线前先跑测试",
                      "body": format!("{CANARY} 每次合并之前先跑全量测试"),
                      "valid_to_ms": null, "provenance": [u]}],
           "worthless_turn_uids": []}).to_string()
}

/// 脚本后端（进程内用；判据 2㊵ 走真进程的那一份是下面的 [`ScriptSeat`]）：
/// 按序回答，并记下被问过的次数。它也替账本记账（`enter` → 回答 → `record_call`），
/// 与 `offline_cycle.rs` 的同一件东西逐字同形。
struct Script {
    replies: RefCell<Vec<String>>,
    prompts: RefCell<Vec<String>>,
    meter: Rc<Metered>,
}

impl Script {
    fn new(replies: Vec<String>, meter: Rc<Metered>) -> Script {
        Script { replies: RefCell::new(replies), prompts: RefCell::new(Vec::new()), meter }
    }

    fn calls(&self) -> usize {
        self.prompts.borrow().len()
    }
}

impl LlmBackend for Script {
    fn complete(&self, prompt: &Prompt) -> Result<String, LlmFailure> {
        self.prompts.borrow_mut().push(prompt.user.clone());
        let next = self.replies.borrow_mut().remove(0);
        self.meter.record_call();
        Ok(next)
    }
}

/// 夹具：中心下发过一代副本（一张已审过的卡 + turn A 的 provenance），
/// 节点转录里同时有 turn A（副本 provenance 里已有）与 turn B（新的）。
/// **暂存层不在这里建**：触发点在开闸那一轮负责建，这正是本批要测的那件事。
struct Fixture {
    /// 只是把临时目录活到用例结束。
    _dir: tempfile::TempDir,
    data: PathBuf,
    b_uid: String,
}

impl Fixture {
    /// 状态：中心下发过一代副本（epoch 1/seq 1）；其余从零开始。
    fn state(&self) -> SyncState {
        SyncState {
            current_manifest: CurrentManifest {
                epoch: 1,
                seq: 1,
                sha256: "0".repeat(64),
                created_at_ms: 1,
            },
            ..SyncState::default()
        }
    }
}

fn synth_turn(t: &rc_store::TranscriptHandle, uid: &str, role: &str, body: &str, at: i64) {
    // 形状照 `crates/hub/tests/backup_cli.rs` 的 `synth_turn`；uid 由调用方算好传进来。
    t.conn()
        .execute(
            "INSERT INTO turns(turn_uid, session_key, device_id, role, body, body_fp,
                               turn_key_tier, project_hint, created_at_ms, ingested_at_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, 1, NULL, ?7, ?7)",
            (uid, SESS, DEV, role, body, compute_body_fp(body).as_str(), at),
        )
        .unwrap();
    t.conn()
        .execute(
            "INSERT INTO sessions(session_key, device_id, source_agent, machine,
                                  project_hint, last_saved_at_ms)
             VALUES(?1, ?2, 'agent', NULL, NULL, ?3)
             ON CONFLICT(session_key, device_id)
             DO UPDATE SET last_saved_at_ms=excluded.last_saved_at_ms",
            (SESS, DEV, at),
        )
        .unwrap();
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(data.join("replica")).unwrap();
    let a_uid = compute_turn_uid(DEV, SESS, "human", TURN_A_BODY, A_AT);
    let b_uid = compute_turn_uid(DEV, SESS, "human", TURN_B_BODY, B_AT);

    {
        let centre = tempfile::tempdir().unwrap();
        let root = Root::open(centre.path()).unwrap();
        let key = put_card(
            root.memory(),
            &FactContent {
                kind: "decision".into(),
                title: "fixture base row".into(),
                body: "the base replica already carries this row".into(),
                scope: Scope { kind: ScopeKind::Project, key: SESS.into() },
                valid_to_ms: None,
            },
            &[Provenance { turn_uid: a_uid.clone(), anchor_role: AnchorRole::Human }],
            10,
        )
        .unwrap();
        // 底座卡片是中心**审过**的：留着 unreviewed 会把脚本里给新行的那三条 `YES` 先吃掉。
        set_verdict(root.memory(), &key, Verdict::Passed, 11).unwrap();
        // WAL 的提交在最后一个连接关闭时落回主文件；先关再拷，拷贝才是完整的。
        drop(root);
        std::fs::copy(centre.path().join("memory.db"), data.join("replica/snapshot-1-1.db"))
            .unwrap();
    }

    {
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), &a_uid, "human", TURN_A_BODY, A_AT);
        synth_turn(root.transcript(), &b_uid, "human", TURN_B_BODY, B_AT);
    }

    Fixture { _dir: dir, data, b_uid }
}

/// 派发档 §三给的脚本：新 turn 的抽取 + 三票审核 + 一轮推导，后三条是多余的备用
/// （避免管线多问一句就 `Timeout`）。脚本与 cycle 共享**同一本**账（`Rc`）。
fn scripted(fx: &Fixture, meter: &Rc<Metered>) -> Script {
    Script::new(
        vec![
            extract_ok(&fx.b_uid),
            YES.to_string(),
            YES.to_string(),
            YES.to_string(),
            DERIVE_QUIET.to_string(),
            YES.to_string(),
            YES.to_string(),
            YES.to_string(),
        ],
        Rc::clone(meter),
    )
}

fn meter() -> Rc<Metered> {
    Rc::new(Metered::new())
}

/// 本机脚本端点（判据 2㊵ 的"脚本后端"在真进程里的那一半）：`llm.toml` 的 custom 通道
/// 指向它，按序端出脚本回复。出网协议就是 custom 的 openai-chat 形。
struct ScriptSeat {
    replies: Vec<String>,
    asked: Arc<AtomicUsize>,
}

impl rc_net::http::Handler for ScriptSeat {
    fn handle(&mut self, _req: &rc_net::http::Request) -> rc_net::http::Response {
        let n = self.asked.fetch_add(1, Ordering::SeqCst);
        match self.replies.get(n) {
            Some(reply) => rc_net::http::Response::json(
                200,
                serde_json::json!({
                    "model": "script-model",
                    "choices": [{"finish_reason": "stop",
                                 "message": {"role": "assistant", "content": reply}}],
                }),
            ),
            // 脚本耗尽 ⇒ 让这一轮响亮失败：404 不在可重试名单 ⇒ `SchemaMismatch`，不重发、不换家。
            None => rc_net::http::Response::error(404, "script_exhausted"),
        }
    }
}

/// 起脚本端点。返回 `(端口, 被问次数)`；监听活到测试进程结束（`serve` 的单线程 accept 循环）。
fn start_script_seat(replies: Vec<String>) -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let asked = Arc::new(AtomicUsize::new(0));
    let mut seat = ScriptSeat { replies, asked: Arc::clone(&asked) };
    let stop = AtomicBool::new(false);
    std::thread::spawn(move || {
        rc_net::http::serve(listener, &mut seat, &stop, rc_net::http::Limits::default(), 0);
    });
    (port, asked)
}

fn node_bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-node")
}

/// 跑真节点进程。`--config` 之后按调用点给的旗标走（无旗标 = status）。
fn node_run(cfg: &Path, extra: &[&str]) -> Output {
    let mut cmd = Command::new(node_bin());
    cmd.arg("--config").arg(cfg);
    for a in extra {
        cmd.arg(*a);
    }
    cmd.output().unwrap()
}

/// TOML 里写 Windows 路径：反斜杠是转义符，统一换成 `/`。
fn toml_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// 一个没人听的端口（先绑住拿到号，再放掉）——**死端点**是同步失败的生产者。
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// `node.toml`：`llm_config` 只在给了的时候写（status 的 locked 读数不需要它）。
fn write_node_cfg(
    dir: &Path,
    data: &Path,
    port: u16,
    auto_unlock: bool,
    llm: Option<&Path>,
) -> PathBuf {
    let path = dir.join("node.toml");
    let mut text = format!(
        "data_dir = \"{}\"\ndevice_key_path = \"{}\"\nauto_unlock = {auto_unlock}\n",
        toml_path(data),
        toml_path(&data.join("device.key")),
    );
    if let Some(llm) = llm {
        text.push_str(&format!("llm_config = \"{}\"\n", toml_path(llm)));
    }
    text.push_str("[[endpoints]]\nlabel = \"lan\"\nhost = \"127.0.0.1\"\nport = ");
    text.push_str(&port.to_string());
    text.push('\n');
    std::fs::write(&path, text).unwrap();
    path
}

/// `llm.toml`：一行 custom 座位指向本机的脚本端点（没有 preset 行可查，模型名只能来自配置）。
fn write_llm_toml(dir: &Path, seat_port: u16) -> PathBuf {
    let path = dir.join("llm.toml");
    std::fs::write(
        &path,
        format!(
            "[[providers]]\nmodel = \"script-model\"\nsecret_ref = \"secrets:offline-trigger\"\n\
             [providers.endpoint]\nbase_url = \"http://127.0.0.1:{seat_port}/v1\"\nprotocol = \"openai-chat\"\n"
        ),
    )
    .unwrap();
    path
}

/// 一个读得开的密文箱：`secrets.enc` + `secrets.pass` + 一格 `offline-trigger`。
fn write_offline_box(data: &Path) {
    {
        rc_secret::SecretsBox::create(&data.join("secrets.enc"), PASS).unwrap();
        let mut b = rc_secret::SecretsBox::open(&data.join("secrets.enc"), PASS).unwrap();
        b.put("offline-trigger", "sk-scripted-not-a-real-key").unwrap();
    }
    let mut pass = PASS.to_vec();
    pass.push(b'\n');
    std::fs::write(data.join("secrets.pass"), pass).unwrap();
}

/// 判据 2㊵：整整一天的失败后闸真开、并且**就地跑了一个批次**。
///
/// 全程走真进程：死端点让 `sync --once` 以 `endpoint_unreachable` 收场（退出码 3），
/// 失败路径按 `auto_unlock=true` + 墙钟差 ≥ 一天把状态推到 `OfflineDistilling`，
/// 接着建层、跑批次。断言全部落在**进程外可见的形状**上：状态文件、暂存层、脚本端点计数。
#[test]
fn a_full_day_of_failures_opens_the_gate_and_runs_one_batch() {
    let fx = fixture();
    let dir = fx._dir.path();
    let (seat_port, asked) = start_script_seat(vec![
        extract_ok(&fx.b_uid),
        YES.to_string(),
        YES.to_string(),
        YES.to_string(),
        DERIVE_QUIET.to_string(),
        YES.to_string(),
        YES.to_string(),
        YES.to_string(),
    ]);
    let llm = write_llm_toml(dir, seat_port);
    write_offline_box(&fx.data);
    rc_net::key::DeviceKey::generate().unwrap().save(&fx.data.join("device.key")).unwrap();
    std::fs::write(state::token_path(&fx.data), "scripted-token\n").unwrap();

    // 离线窗口早就开了（零钟夹具，见 `OFFLINE_SINCE_MS`）：开闸的生产者是持久化的 2 天。
    let st = SyncState {
        paired_hub_pub: "unverified-because-the-endpoint-is-dead".into(),
        offline_since_ms: OFFLINE_SINCE_MS,
        offline_elapsed_ms: TWO_DAYS_MS,
        self_distill: SelfDistill::OfflineWaiting,
        ..fx.state()
    };
    state::save(&state::state_path(&fx.data), &st).unwrap();

    let cfg = write_node_cfg(dir, &fx.data, free_port(), true, Some(&llm));
    let out = node_run(&cfg, &["--once"]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "远端打不通必须是运维失败码 3：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("endpoint_unreachable"),
        "错误面必须是类别串，不是栈：{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let st = state::load(&state::state_path(&fx.data)).unwrap().unwrap();
    assert_eq!(
        st.self_distill,
        SelfDistill::OfflineDistilling,
        "满一天 + auto_unlock ⇒ 闸必须开：{st:?}"
    );
    assert_eq!(
        st.llm_key.as_deref(),
        Some("unlocked"),
        "失败路径同样要记钥匙面（箱与格都在）"
    );
    assert!(staging::is_active(&fx.data), "闸开之后触发点必须把暂存层建出来");
    let rows = staging::new_rows(&fx.data);
    assert!(!rows.is_empty(), "这一批至少写一行行标：{rows:?}");
    assert!(asked.load(Ordering::SeqCst) >= 1, "批次必须真问过脚本端点");
}

/// 判据 2㊶：窗口满了一天、但 `auto_unlock=false` ⇒ 闸锁着：不建层、一次 LLM 调用都没有。
#[test]
fn a_locked_node_creates_no_staging_layer() {
    let fx = fixture();
    let m = meter();
    let script = scripted(&fx, &m);
    let mut st = fx.state();
    st.offline_since_ms = NOW_MS - OFFLINE_GATE_MS;
    // 挡着闸的是"锁"，不是"时长不够"：时长这一半在这里是满的。
    let gate = observe_failure(&mut st, SyncError::EndpointUnreachable, NOW_MS, 0, false);
    assert_eq!(gate, SelfDistill::OfflineWaiting);

    let locked = run_one_offline_batch(&fx.data, &st, &script, &m, NOW_MS);
    let Err(fault) = locked else {
        panic!("锁定节点必须拒绝跑批次");
    };
    assert_eq!(fault, OfflineFault::Locked);
    assert!(!staging::is_active(&fx.data), "锁定节点一次都不许建暂存层");
    assert_eq!(script.calls(), 0, "一次 LLM 调用都不许发生");
}

/// 判据 2㊷：中心回来（成功同步）后暂存层仍在——**不自动回并、不删层**（回并在 B3）。
#[test]
fn a_successful_sync_keeps_the_staging_layer() {
    let fx = fixture();
    let m = meter();
    let script = scripted(&fx, &m);
    let mut st = fx.state();
    st.offline_since_ms = NOW_MS - OFFLINE_GATE_MS;
    // 先走完离线那一段：闸开、批次建层并写行标。
    let gate = observe_failure(&mut st, SyncError::EndpointUnreachable, NOW_MS, 0, true);
    assert_eq!(gate, SelfDistill::OfflineDistilling);
    run_one_offline_batch(&fx.data, &st, &script, &m, NOW_MS).expect("闸开时批次必须跑起来");
    assert!(staging::is_active(&fx.data));
    let marked = staging::new_rows(&fx.data);
    assert!(!marked.is_empty(), "离线那一批必须留下行标：{marked:?}");

    // 中心回来了：状态机回 `normal`；暂存层与里面的行一个都不许动。
    observe_success(&mut st, NOW_MS + 10);
    assert_eq!(st.self_distill, SelfDistill::Normal);
    assert!(staging::is_active(&fx.data), "成功同步不许自动回并、不许删层");
    assert_eq!(staging::new_rows(&fx.data), marked, "层里的行标必须原样还在");
}

/// 判据 2㊸：`status`（无旗标）印出四格离线读数，其中 `staging_rows ≥ 1`、`llm_key == "locked"`。
///
/// 暂存层走**真实生产者**：触发点建的层 + 真批次写下的行标。状态文件里的 `llm_key` 刻意
/// 写着 `unlocked`（与配置相左）——status 那一格必须**现算**（`auto_unlock=false` ⇒ `locked`），
/// 照抄状态文件是唯一会得到 `unlocked` 的路径。
#[test]
fn status_prints_the_offline_readings() {
    let fx = fixture();
    let m = meter();
    let script = scripted(&fx, &m);
    let mut st = fx.state();
    st.offline_since_ms = NOW_MS - OFFLINE_GATE_MS;
    let gate = observe_failure(&mut st, SyncError::EndpointUnreachable, NOW_MS, 0, true);
    assert_eq!(gate, SelfDistill::OfflineDistilling);
    let run = run_one_offline_batch(&fx.data, &st, &script, &m, NOW_MS).expect("闸开时批次必须跑起来");
    assert!(run.staging_rows >= 1, "暂存层必须有真行标：{}", run.staging_rows);

    st.llm_key = Some("unlocked".into());
    state::save(&state::state_path(&fx.data), &st).unwrap();
    let cfg = write_node_cfg(fx._dir.path(), &fx.data, free_port(), false, None);

    let out = node_run(&cfg, &[]); // 无旗标 = status
    assert_eq!(
        out.status.code(),
        Some(0),
        "status 不该失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["self_distill"], "offline_distilling", "{v}");
    assert_eq!(v["offline_elapsed_ms"], OFFLINE_GATE_MS, "{v}");
    assert!(v["staging_rows"].as_u64().unwrap() >= 1, "暂存层行数必须报出来：{v}");
    assert_eq!(
        v["llm_key"], "locked",
        "这一格必须现算（配置锁着），不是抄状态文件里的 unlocked：{v}"
    );
}