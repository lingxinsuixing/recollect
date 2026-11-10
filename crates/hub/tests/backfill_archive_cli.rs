//! `hub distill backfill-archive` 的真进程面：存量卡（无章）回填盖章、重跑零新增。
//!
//! 夹具合成（守卫 12）：一条施工行（`run_tests` + agent 出处）在写点会盖程序章，
//! 测试先删章模拟"功能上线前的老库"，再走 CLI 回填。
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_fact::{put_fact_rows, FactWriteItem};
use rc_memory::{compute_mem_id, AnchorRole, BatchCtx, MemTable, Scope, ScopeKind};
use rc_store::Root;
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

const WRITE_NOW: i64 = 1_700_000_999_000;
const SRC_MS: i64 = 1_700_000_000_000;
const AGENT: &str = "agent-hub-bf";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}
fn run(args: &[&str]) -> Output {
    Command::new(bin()).args(args).output().expect("hub binary")
}
fn data_dir(dir: &Path) -> String {
    dir.to_str().unwrap().to_string()
}
fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-hub-bf".into() }
}
fn ctx() -> BatchCtx {
    BatchCtx { source_agent: AGENT.into(), device_id: "dev-hub-bf".into() }
}
fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-hub-bf".into()),
        owner_agent_id: "agent-owner".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}
fn uid(index: usize) -> String {
    format!("{:064x}", 0x41 + index)
}
fn self_id() -> String {
    compute_mem_id(MemTable::Anchor, "agent", AGENT, "", &scope())
}
fn seed_self_anchor(m: &rc_store::MemoryHandle) {
    m.conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind,
                                scope_key, status, profile_text, created_at_ms, updated_at_ms,
                                norm_version)
             VALUES(?1,'agent',?2,'unknown',NULL,'project','proj-hub-bf','active',NULL,?3,?3,?4)",
            rusqlite::params![self_id(), AGENT, SRC_MS, rc_store::capability::NORM_VERSION as i64],
        )
        .unwrap();
}
fn construction_item() -> FactWriteItem {
    FactWriteItem {
        subject_anchor_id: self_id(),
        object_anchor_id: None,
        predicate: "run_tests".into(),
        value: Some("已执行".into()),
        category: "observation".into(),
        mode: "stated".into(),
        time_text: String::new(),
        quote: "回填用例：跑了一遍测试。".into(),
        evidence: vec![(uid(1), AnchorRole::Agent)],
        text: "回填用例：跑了一遍测试。".into(),
        scope: scope(),
        source_ms: SRC_MS,
    }
}

#[test]
fn backfill_stamps_pre_existing_cards_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_self_anchor(m);
    put_fact_rows(m, &[construction_item()], &ctx(), &owner(), WRITE_NOW).unwrap();
    // 模拟上线前的老库：写点盖的章不存在。
    m.conn().execute("DELETE FROM card_sink", []).unwrap();
    drop(root);

    let first = run(&["distill", "backfill-archive", "--data-dir", &data_dir(dir.path()), "--json"]);
    assert_eq!(first.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&first.stderr));
    let v: Value = serde_json::from_slice(&first.stdout).expect("stdout 必须是单个 JSON");
    assert_eq!(v["scanned"], 1, "{v}");
    assert_eq!(v["hits"], 1, "{v}");
    assert_eq!(v["newly_marked"], 1, "{v}");

    let root = Root::open(dir.path()).unwrap();
    let marked: i64 = root
        .memory()
        .conn()
        .query_row("SELECT count(*) FROM card_sink WHERE sink='archive'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(marked, 1, "存量卡必须真的被盖章");
    drop(root);

    let second = run(&["distill", "backfill-archive", "--data-dir", &data_dir(dir.path()), "--json"]);
    assert_eq!(second.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(v["newly_marked"], 0, "重跑必须零新增：{v}");
}
