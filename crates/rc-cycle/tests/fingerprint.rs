//! S1：抽取指纹在 cycle 层的**盖章点**。
//!
//! 指纹的算法不在这里（唯一实现在 `rc_ledger::water::extraction_fingerprint`），这一层只负责
//! "在正确的位置盖一次、盖给这一批的每一轮、盖失败就按写失败处置"。所以这一文件的三条牙是：
//! ① 成功批次的每一轮都有一枚 64 位小写 hex；② 同一批 + 同一模型两次 ⇒ 同一指纹；
//! ③ 换一个 `slug()` ⇒ 换一枚指纹。外加一条负向的：LLM 故障轮**不盖章**。
//!
//! 夹具风格照 hub 的 `cycle_once.rs`：合成正文、正文短句、不碰真实导出；差别只在它是
//! 库内调用（`run_cycle_once` 是 `rc-cycle` 的公开面），所以时间与批次大小由用例直接给。

use rc_cycle::{run_cycle_once, CycleExit, CycleOut, Metered};
use rc_ledger::water;
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_store::Root;

const DEV: &str = "dev-fp";
const SESS: &str = "sess-fp";

/// 批内两轮的 uid。`64 位 hex` 是生产形状（`compute_turn_uid` 的产物），夹具不另立一套。
fn uids() -> [String; 2] {
    [format!("{:064x}", 1), format!("{:064x}", 2)]
}

/// 播种：一个会话、两条 turn。**同一份种子在另一个临时目录里逐字节复现**，
/// 于是"同一批输入"这条判据不需要在同一库上重放（那要先把状态行擦掉，反而人为）。
fn seeded() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let [u1, u2] = uids();
    root.transcript()
        .conn()
        .execute_batch(&format!(
            "INSERT INTO sessions(session_key, device_id, source_agent, machine, project_hint,
                                  last_saved_at_ms)
             VALUES('{SESS}', '{DEV}', 'agent-fp', NULL, NULL, NULL);
             INSERT INTO turns(turn_uid, session_key, device_id, role, body, body_fp,
                               turn_key_tier, project_hint, created_at_ms, ingested_at_ms)
             VALUES('{u1}', '{SESS}', '{DEV}', 'human', '先把测试跑绿再提交。', 'fp-1', 1, NULL, 1000, 1000),
                   ('{u2}', '{SESS}', '{DEV}', 'agent', '记住了：提交之前跑全量测试。', 'fp-2', 1, NULL, 1001, 1001);"
        ))
        .unwrap();
    (dir, root)
}

/// 五键齐全、一条卡片、出处指向整批（两轮都拿到 `extracted` 状态行）。
fn extract_reply() -> String {
    let [u1, u2] = uids();
    format!(
        r#"{{"entities":[],"events":[],"assertions":[],
             "cards":[{{"kind":"lesson","title":"指纹落柱",
                        "body":"每一次抽取都由一枚指纹记着它是谁产出的。",
                        "valid_to_ms":null,"provenance":["{u1}","{u2}"]}}],
             "worthless_turn_uids":[]}}"#
    )
}

/// 缺 `cards` 键 ⇒ `ContractError::MissingKey` ⇒ `reason_slug() == Some("schema")`，
/// 也就是 `run_cycle_once` 的"契约族丢弃"那一支（那一支**也**写过状态行）。
const CONTRACT_BAD: &str = r#"{"entities":[],"events":[],"assertions":[],"worthless_turn_uids":[]}"#;
/// 推导家族的"跑通但不落行"答复（单环节链 ⇒ 条目级拒收，见 hub 的 `DERIVE_QUIET` 同名注释）。
const DERIVE_QUIET: &str =
    r#"{"relationships":[],"chains":[{"from":["r1"],"mechanism":"requires","basis":["r1"]}],"timeline":[]}"#;

/// 按 system 段/`derive` 旗标认作业。判同（judge）在这一批没有候选，所以它不在分派里；
/// 真被问到就是 `BackendError`——那会让用例以 Fault 变红，而不是悄悄放过。
struct TestBackend {
    slug: &'static str,
    /// `Some` ⇒ 抽取那一格答这条；`None` ⇒ 抽取恒 `BackendError`（LLM 故障支的夹具）。
    extract: Option<String>,
}

impl TestBackend {
    fn ok(slug: &'static str) -> TestBackend {
        TestBackend { slug, extract: Some(extract_reply()) }
    }
    fn without_extract(slug: &'static str) -> TestBackend {
        TestBackend { slug, extract: None }
    }
}

impl LlmBackend for TestBackend {
    fn complete(&self, prompt: &Prompt) -> Result<String, LlmFailure> {
        if prompt.derive {
            return Ok(DERIVE_QUIET.to_owned());
        }
        if prompt.system.contains("extraction job") {
            return match &self.extract {
                Some(reply) => Ok(reply.clone()),
                None => Err(LlmFailure::BackendError),
            };
        }
        if prompt.system.contains("review job") {
            return Ok(r#"{"holds":true}"#.to_owned());
        }
        Err(LlmFailure::BackendError)
    }

    fn slug(&self) -> Option<&str> { Some(self.slug) }
}

fn run_once(root: &Root, backend: &dyn LlmBackend, now_ms: i64) -> (CycleOut, CycleExit) {
    let meter = Metered::new();
    run_cycle_once(root, &meter, backend, now_ms, 10)
}

fn stamped(root: &Root, uid: &str) -> Option<String> {
    water::extract_record(root.ledger(), uid).unwrap().unwrap().fingerprint
}

#[test]
fn every_turn_of_a_successful_batch_carries_a_64_hex_fingerprint() {
    let (_dir, root) = seeded();
    let backend = TestBackend::ok("model-a");
    let (out, exit) = run_once(&root, &backend, 5_000);
    assert!(matches!(exit, CycleExit::Ran),
            "这一轮必须跑通；落到别的判定说明答复形状或 prompt 装配对不上");
    assert_eq!(out.turns_scanned, 2);
    for uid in uids() {
        let fp = stamped(&root, &uid).unwrap_or_else(|| panic!("{uid} 没有指纹"));
        assert_eq!(fp.len(), 64, "指纹是 64 位 hex，得到 {fp}");
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "只许小写 hex：{fp}");
    }
    // 同一批的两轮拿的是**同一枚**指纹（同一批、同一 prompt、同一模型、同一输入）。
    assert_eq!(stamped(&root, &uids()[0]), stamped(&root, &uids()[1]));
}

#[test]
fn the_same_batch_and_model_stamp_the_same_fingerprint_twice() {
    // 两个临时目录、同一份种子：`select_batch` 的序由 SQL 给（ingested_at_ms, turn_uid），
    // `extract_user` 的渲染逐字节可复现 ⇒ "同一输入"这件事成立。
    let (_d1, r1) = seeded();
    let (_d2, r2) = seeded();
    let (_, e1) = run_once(&r1, &TestBackend::ok("model-a"), 5_000);
    let (_, e2) = run_once(&r2, &TestBackend::ok("model-a"), 6_000);
    assert!(matches!(e1, CycleExit::Ran) && matches!(e2, CycleExit::Ran));
    assert_eq!(stamped(&r1, &uids()[0]), stamped(&r2, &uids()[0]),
               "同批同模型两次的指纹必须逐字节相同（时间戳不进指纹）");
}

#[test]
fn a_different_model_slug_stamps_a_different_fingerprint() {
    let (_da, ra) = seeded();
    let (_db, rb) = seeded();
    run_once(&ra, &TestBackend::ok("model-a"), 5_000);
    run_once(&rb, &TestBackend::ok("model-b"), 5_000);
    assert_ne!(stamped(&ra, &uids()[0]), stamped(&rb, &uids()[0]),
               "换了模型指纹却不变 ⇒ 指纹没有记下'是谁产出的'");
}

#[test]
fn a_contract_discarded_batch_is_stamped_too() {
    // 契约族丢弃的那一支**写过状态行**（整批 `rejected`），所以它也必须盖章：
    // 否则"这批被谁产出的"在运维面上只剩下一半。
    let (_dir, root) = seeded();
    let backend = TestBackend { slug: "model-a", extract: Some(CONTRACT_BAD.to_owned()) };
    let (_, exit) = run_once(&root, &backend, 5_000);
    assert!(matches!(exit, CycleExit::BatchDropped), "契约族错误必须是整批丢弃");
    for uid in uids() {
        let r = water::extract_record(root.ledger(), &uid).unwrap().unwrap();
        assert_eq!(r.reason.as_deref(), Some("schema"));
        assert!(r.fingerprint.is_some(), "{uid} 的状态行被写了，指纹却没盖");
    }
}

#[test]
fn an_llm_failure_round_stamps_nothing() {
    // LLM 故障支没有状态行（"恢复即续"），所以它不盖章——盖章在 return 之前就退出。
    let (_dir, root) = seeded();
    let (_, exit) = run_once(&root, &TestBackend::without_extract("model-a"), 5_000);
    assert!(matches!(exit, CycleExit::Fault(_)), "LLM 故障必须落到 Fault");
    for uid in uids() {
        assert_eq!(water::extract_record(root.ledger(), &uid).unwrap(), None,
                   "故障轮不许留下状态行，也不许留下指纹");
    }
}

#[test]
fn the_backend_trait_defaults_to_no_slug() {
    // 默认实现是 None：其它后端（回放座位、mock）零改动，调用方据此写 "unknown"。
    struct Bare;
    impl LlmBackend for Bare {
        fn complete(&self, _p: &Prompt) -> Result<String, LlmFailure> { Err(LlmFailure::Timeout) }
    }
    assert_eq!(Bare.slug(), None);
    assert_eq!(TestBackend::ok("model-a").slug(), Some("model-a"));
}
