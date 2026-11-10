//! B2b-2：节点离线自蒸的五条判据（派发档 §三）。
//!
//! 夹具的形状是"中心已经下发过一代副本、节点离线后又攒了一条新 turn"：
//! - 底座副本 = 中心库的一份拷贝（一张已再审过的卡 + turn A 的 provenance）；
//! - `staging_new_row` 由 B1b 的 `staging::prepare` 建好（本批不负责建层）；
//! - 节点 `transcript.db` 里同时有 turn A（副本 provenance 里已有）与 turn B（新的）；
//! - 后端是脚本：**文案逐字抄 `hub/tests/cycle_once.rs`**（`extract_ok` / `YES` / `DERIVE_QUIET`），
//!   一次都不出网。uid 只从 `rc_contract::compute_turn_uid` 来，不许手抄哈希（守卫 3）。

use node::node_cfg::{Endpoint, NodeConfig};
use node::selfdistill::{build_live_backend, run_offline_cycle, OfflineFault};
use node::state::CurrentManifest;
use node::staging;
use rc_contract::{compute_body_fp, compute_turn_uid};
use rc_cycle::Metered;
use rc_llm::{JobFamily, LlmBackend, LlmFailure, Prompt};
use rc_memory::{put_card, set_verdict, AnchorRole, FactContent, Provenance, Scope, ScopeKind,
                Verdict};
use rc_store::Root;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

const DEV: &str = "dev-offline";
const SESS: &str = "sess-offline";
const TURN_A_BODY: &str = "the base turn that the centre already distilled";
const TURN_B_BODY: &str = "the new turn written while the centre was away";
const A_AT: i64 = 1_000;
const B_AT: i64 = 2_000;
const NOW_MS: i64 = 9_000;
/// 新卡正文里的记号：断言"这条行是新蒸出来的"靠它，不靠标题。
const CANARY: &str = "OFFLINE-CANARY-5521";
/// 审核三票全过。逐字抄 `hub/tests/cycle_once.rs` 的 `YES`。
const YES: &str = r#"{"holds":true}"#;
/// 推导的"跑通但不落行"答复：单环节链在 parse 的第一道就按条目拒收，
/// 于是读数照旧（`derive_rows = 0`）、游标照推。逐字抄 `cycle_once.rs` 的 `DERIVE_QUIET`。
const DERIVE_QUIET: &str = r#"{"relationships":[],"chains":[{"from":["r1"],"mechanism":"requires","basis":["r1"]}],"timeline":[]}"#;
/// 读箱判据用的口令。值只经过文件，不进任何一条错误面。
const PASS: &[u8] = b"offline-cycle-passphrase-42";

/// 五键齐全、一条卡片、出处指向本轮那条新 turn。逐字抄 `hub/tests/cycle_once.rs` 的 `extract_ok`
/// （只有卡片正文里的记号换成 `CANARY`）。
fn extract_ok(u: &str) -> String {
    serde_json::json!({"entities": [], "events": [], "assertions": [],
           "cards": [{"kind": "lesson", "title": "上线前先跑测试",
                      "body": format!("{CANARY} 每次合并之前先跑全量测试"),
                      "valid_to_ms": null, "provenance": [u]}],
           "worthless_turn_uids": []}).to_string()
}

/// 脚本后端：按序回答，并**记下每一次被问的 user 段**（第一条是不是抽取 B 靠它证明）。
///
/// 它也替**账本**记账（`enter` → 回答 → `record_call`），与 hub 的 `replay::Seat`、
/// 与生产的 `MeteredBackend` 同一条契约：`rc_cycle::run_cycle_once` 只递来源与账本两个角色，
/// 「这一次调用属于哪个家族」的计数发生在来源那一侧。少了这一层，
/// `charge_counters` 读到的是零（P-33 的算术在测试里就被绕过去了）。
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

/// 夹具：中心副本 + 暂存层 + 两条 turn。字段 `_dir` 只是把临时目录活到用例结束。
struct Fixture {
    _dir: tempfile::TempDir,
    data: PathBuf,
    base_card: String,
    b_uid: String,
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

    let base_card = {
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
        // 底座卡片是中心**审过**的。审核游标一轮最多问 `MAX_PER_PASS = 4` 行，
        // 留着 `unreviewed` 会把脚本里给新行的那三条 `YES` 先吃掉，本用例就测不到
        // "新行被审"这件事；这同时是 `Verdict` 为 `Passed` 的行不进审核游标的阳性对照。
        set_verdict(root.memory(), &key, Verdict::Passed, 11).unwrap();
        // WAL 的提交在最后一个连接关闭时落回主文件；先关再拷，拷贝才是完整的。
        drop(root);
        std::fs::copy(centre.path().join("memory.db"), data.join("replica/snapshot-1-1.db"))
            .unwrap();
        key.key_string()
    };

    let manifest =
        CurrentManifest { epoch: 1, seq: 1, sha256: "0".repeat(64), created_at_ms: 1 };
    staging::prepare(&data, &manifest, 1).unwrap();

    {
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), &a_uid, "human", TURN_A_BODY, A_AT);
        synth_turn(root.transcript(), &b_uid, "human", TURN_B_BODY, B_AT);
    }

    Fixture { _dir: dir, data, base_card, b_uid }
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

fn marked_ids(data: &Path) -> Vec<String> {
    let memory = rc_store::open_replica(&staging::path(data)).unwrap();
    let mut st = memory
        .conn()
        .prepare("SELECT id FROM staging_new_row ORDER BY id")
        .unwrap();
    let got = st.query_map([], |r| r.get::<_, String>(0)).unwrap();
    got.collect::<Result<Vec<_>, _>>().unwrap()
}

#[test]
fn already_distilled_turns_are_not_selected_again() {
    let fx = fixture();
    let m = meter();
    let script = scripted(&fx, &m);
    let run = run_offline_cycle(&fx.data, &script, &m, NOW_MS).expect("离线 cycle 必须跑通");

    assert_eq!(
        run.out.turns_scanned, 1,
        "只该蒸新 turn B：turn A 已经在副本的 provenance 里（播种那一步把它标成 Extracted）"
    );
    let prompts = script.prompts.borrow();
    assert!(!prompts.is_empty(), "一轮 cycle 至少要问一次抽取");
    // W23a：抽取 prompt 的方括号里改成批内位置别名，身份值不再进 prompt ⇒ "问的是哪一轮"
    // 这一格搬到**正文**上判（它本来就是这条用例真正要认的东西，比哈希更直接）。
    assert!(
        prompts[0].contains(TURN_B_BODY),
        "第一条问的必须是 turn B：{}",
        prompts[0]
    );
    assert!(
        !prompts[0].contains(TURN_A_BODY),
        "已经蒸过的 turn A 不许再进抽取批次：{}",
        prompts[0]
    );
    drop(prompts);
    assert_eq!(
        script.calls(),
        5,
        "1 抽取 + 0 判同 + 3 审核 + 1 推导 = 5（§2.15 的配额算术；多一条说明有行被重蒸/重审）"
    );
    assert!(
        !marked_ids(&fx.data).iter().any(|id| id == &fx.base_card),
        "底座已有的卡片不是离线新行：{}",
        fx.base_card
    );
}

#[test]
fn the_offline_cycle_writes_only_the_staging_layer_and_marks_new_rows() {
    let fx = fixture();
    let base = fx.data.join("replica").join("snapshot-1-1.db");
    let before = std::fs::read(&base).unwrap();
    let m = meter();
    let script = scripted(&fx, &m);
    let run = run_offline_cycle(&fx.data, &script, &m, NOW_MS).expect("离线 cycle 必须跑通");
    let after = std::fs::read(&base).unwrap();

    assert_eq!(before, after, "底座副本必须逐字节不动（写面只许是暂存层）");
    assert_eq!(
        rc_net::protocol::sha256_hex(&before),
        rc_net::protocol::sha256_hex(&after),
        "底座的 sha256 跑前跑后必须相同"
    );

    let marked = marked_ids(&fx.data);
    let new_card = marked
        .iter()
        .find(|id| id.starts_with("card:") && **id != fx.base_card)
        .unwrap_or_else(|| panic!("暂存标记里必须出现新的 card 句柄：{marked:?}"));
    assert!(run.staging_rows >= 1, "本批至少标了一行，读数却报 {}", run.staging_rows);

    // 标记点名的行真的在暂存层里（不是只写了一行字符串）。
    let staged = rc_store::open_replica(&staging::path(&fx.data)).unwrap();
    let bare = new_card.strip_prefix("card:").unwrap();
    let hits: i64 = staged
        .conn()
        .query_row("SELECT count(*) FROM card WHERE id=?1", [bare], |r| r.get(0))
        .unwrap();
    assert_eq!(hits, 1, "行标 {new_card} 指向的卡片必须在暂存层里");
    assert!(
        staging::new_rows(&fx.data).contains(new_card),
        "读面（`staging::new_rows`）必须看见同一个句柄"
    );
}

#[test]
fn the_offline_cycle_charges_the_node_ledger() {
    let fx = fixture();
    let m = meter();
    let script = scripted(&fx, &m);
    let run = run_offline_cycle(&fx.data, &script, &m, NOW_MS).expect("离线 cycle 必须跑通");
    assert_eq!(run.out.turns_scanned, 1);

    // 重开一次：计数必须落在节点自己的 `ledger.db` 里（不是暂存层、不是中心账）。
    let root =
        Root::open_with_memory(&fx.data, &staging::path(&fx.data)).expect("节点三库必须开得起来");
    let rows = rc_ledger::counters::read_all(root.ledger()).unwrap();
    let extract = rows
        .iter()
        .find(|r| {
            r.subject_kind == "llm_job"
                && r.subject_id == JobFamily::Extract.slug()
                && r.metric == rc_llm::ALERT_METRIC
        })
        .unwrap_or_else(|| panic!("节点账本里没有 extract 家族的 llm_calls：{rows:?}"));
    assert!(extract.count >= 1, "抽取至少开过一次口，计数器却报 {}", extract.count);
}

#[test]
fn a_missing_staging_layer_is_a_loud_fault() {
    let fx = fixture();
    staging::drop_layer(&fx.data).unwrap();
    assert!(!staging::is_active(&fx.data));
    let m = meter();
    let script = scripted(&fx, &m);
    let Err(fault) = run_offline_cycle(&fx.data, &script, &m, NOW_MS) else {
        panic!("没有暂存层必须响亮失败，不许静默跑一轮");
    };
    assert_eq!(fault, OfflineFault::StagingMissing);
    assert_eq!(script.calls(), 0, "缺暂存层时一次都不许开口");
}

fn write_llm_toml(dir: &Path) -> PathBuf {
    let path = dir.join("llm.toml");
    std::fs::write(
        &path,
        "[[providers]]\nprovider = \"deepseek\"\nmodel = \"deepseek-chat\"\n\
         secret_ref = \"secrets:deepseek\"\n",
    )
    .unwrap();
    path
}

fn node_cfg(data: &Path, llm_config: Option<PathBuf>, auto_unlock: bool) -> NodeConfig {
    NodeConfig {
        data_dir: data.to_path_buf(),
        device_key_path: data.join("device.key"),
        hub_pub: None,
        hub_tls_fp: None,
        endpoints: vec![Endpoint {
            label: "lan".into(),
            host: "127.0.0.1".into(),
            port: 8477,
            tls_fp: None,
        }],
        llm_config,
        auto_unlock,
        // 缺省 = 广播 + 固定发现端口（S4d1）；这条用例不碰发现。
        discovery_port: None,
        discovery_target: None,
        discovery: true,
        discovery_timeout_ms: 600,
        allow_plaintext: false,
    }
}

fn no_calls(m: &Metered) {
    for f in [JobFamily::Extract, JobFamily::Dedupe, JobFamily::Audit, JobFamily::Derive] {
        assert_eq!(m.calls(f), 0, "装配面一次 Live 调用都不许发生：{}", f.slug());
    }
}

#[test]
fn build_live_backend_refuses_locked_and_bad_boxes() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let llm = write_llm_toml(dir.path());

    // ① 锁定：`auto_unlock=false` 比"钥匙在哪"更靠前。
    let m = meter();
    let Err(fault) = build_live_backend(&node_cfg(&data, Some(llm.clone()), false), &data, m.clone())
    else {
        panic!("auto_unlock=false 必须拒绝装配");
    };
    assert_eq!(fault, OfflineFault::Locked);
    no_calls(&m);

    // ② `llm_config` 没给。
    let m = meter();
    let Err(fault) = build_live_backend(&node_cfg(&data, None, true), &data, m.clone()) else {
        panic!("没有 llm_config 必须拒绝装配");
    };
    assert_eq!(fault, OfflineFault::NoConfig);
    no_calls(&m);

    // ③ 配了但箱还没建。
    let m = meter();
    let Err(fault) =
        build_live_backend(&node_cfg(&data, Some(llm.clone()), true), &data, m.clone())
    else {
        panic!("缺箱必须拒绝装配");
    };
    assert_eq!(fault, OfflineFault::BoxMissing);
    no_calls(&m);

    // ④ 箱与口令都在、格缺。
    {
        rc_secret::SecretsBox::create(&data.join("secrets.enc"), PASS).unwrap();
    }
    std::fs::write(data.join("secrets.pass"), concat!("offline-cycle-passphrase-42", "\n")).unwrap();
    let m = meter();
    let Err(fault) =
        build_live_backend(&node_cfg(&data, Some(llm.clone()), true), &data, m.clone())
    else {
        panic!("缺格必须拒绝装配");
    };
    assert_eq!(fault, OfflineFault::SlotMissing);
    no_calls(&m);

    // ⑤ 箱在、格在，但口令文件写错。
    {
        let mut b = rc_secret::SecretsBox::open(&data.join("secrets.enc"), PASS).unwrap();
        b.put("deepseek", "sk-offline-test").unwrap();
    }
    std::fs::write(data.join("secrets.pass"), b"not-the-passphrase\n").unwrap();
    let m = meter();
    let Err(fault) =
        build_live_backend(&node_cfg(&data, Some(llm), true), &data, m.clone())
    else {
        panic!("错口令必须拒绝装配");
    };
    assert_eq!(fault, OfflineFault::BoxOpen);
    no_calls(&m);

    // 阳性对照：同一份目录改回正确口令之后装得起来（否则"五种失败"可能只是"永远失败"）。
    std::fs::write(data.join("secrets.pass"), b"offline-cycle-passphrase-42\n").unwrap();
    let m = meter();
    let built = build_live_backend(&node_cfg(&data, Some(write_llm_toml(dir.path())), true), &data,
                                   m.clone());
    assert!(built.is_ok(), "口令正确、格齐全时装配必须成功");
    no_calls(&m);
}
