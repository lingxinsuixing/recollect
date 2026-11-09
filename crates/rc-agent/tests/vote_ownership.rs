// crates/rc-agent/tests/vote_ownership.rs
//! S1b-C4b1：`recollect_vote` 的**句柄校验**过读闸（设计 ownership-persistence §4.2）。
//!
//! 两条不许漂的口径：
//! ① **不泄露存在性**：被拒的句柄与"库里根本没有"的句柄，返回的三段
//!    （`kind`/`code`/`hint`）逐字相同；被拒的那一次一行表态都不许落；
//! ② owner 自己投得进去，`viewer=None`（中心 CLI/旧测试）下旧行为一个字不变。
//!
//! 规则矩阵本身住在 `rc-memory/tests/gate.rs`，这里只断"接线接上了、形状对不对"。
//! 全部用真库：A 的卡由 `put_batch` 以 agent-A 署名写入（private_agent 归属随行落库）。

use rc_agent::tools::{vote, vote_with_viewer, VoteStance};
use rc_agent::AgentErrorKind;
use rc_memory::gate::Viewer;
use rc_memory::keys::MemTable;
use rc_memory::writers_x::{put_batch, BatchCtx, WriteItem};
use rc_memory::{AnchorRole, Provenance, Scope, ScopeKind};
use rc_store::{MemoryHandle, Root};

const NOW: i64 = 1_700_000_000_000;

struct Fixture {
    _dir: tempfile::TempDir,
    root: Root,
    /// agent-A 的那张卡（句柄是 `<table>:<id>` 的整串，vote 的入参形状）。
    a: String,
}

fn seeded() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let item = WriteItem {
        table: MemTable::Card,
        kind: "note".into(),
        title: "表态归属甲卡".into(),
        body: "表态归属这一行只有 agent-A 读得见。".into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-vote-own".into() },
        valid_to_ms: None,
        aliases: vec![],
        prov: vec![Provenance { turn_uid: "e".repeat(64), anchor_role: AnchorRole::Human }],
    };
    let ctx = BatchCtx { source_agent: "agent-A".into(), device_id: "dev-agent-A".into() };
    let k = put_batch(root.memory(), &[item], &ctx, NOW).unwrap()[0]
        .clone()
        .expect("A 的行不被抑制");
    Fixture { _dir: dir, root, a: k.key_string() }
}

fn attestations(m: &MemoryHandle) -> i64 {
    m.conn().query_row("SELECT count(*) FROM attestation", [], |r| r.get(0)).unwrap()
}

/// B 的 agent 面点名 A 的卡 ⇒ `row_absent`，且与"这一行根本不存在"逐字同形。
#[test]
fn a_denied_vote_looks_exactly_like_an_absent_row() {
    let fx = seeded();
    let before = attestations(fx.root.memory());
    let denied = vote_with_viewer(
        fx.root.memory(),
        Some(&Viewer::agent("agent-B")),
        &fx.a,
        VoteStance::Rejected,
        None,
        Some("别人的行"),
        NOW,
    )
    .unwrap_err();
    assert_eq!(denied.kind, AgentErrorKind::Empty, "被拒不在 Empty 这一族：{denied}");
    assert_eq!(denied.code, "row_absent", "{denied}");

    // 同形的另一半：库里根本没有的句柄走**同一支**。
    let ghost = format!("card:{}", "f".repeat(64));
    let absent = vote_with_viewer(
        fx.root.memory(),
        Some(&Viewer::agent("agent-B")),
        &ghost,
        VoteStance::Rejected,
        None,
        Some("别人的行"),
        NOW,
    )
    .unwrap_err();
    assert_eq!(denied, absent, "被拒与不存在两种缺席的返回不同 ⇒ 形状本身泄露了存在性");
    assert_eq!(attestations(fx.root.memory()), before, "被拒的表态写进了库");
}

/// owner 自己投得进去：表态行 +1，回执指向那一行。
#[test]
fn the_owner_can_vote_on_its_own_row() {
    let fx = seeded();
    let before = attestations(fx.root.memory());
    let out = vote_with_viewer(
        fx.root.memory(),
        Some(&Viewer::agent("agent-A")),
        &fx.a,
        VoteStance::Rejected,
        None,
        Some("自己的行"),
        NOW,
    )
    .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out.text()).unwrap();
    assert_eq!(v["id"], serde_json::json!(fx.a.clone()), "{v}");
    assert_eq!(v["suppresses"], serde_json::json!(true), "{v}");
    assert_eq!(attestations(fx.root.memory()), before + 1, "owner 的表态没落库");
}

/// `viewer=None`（中心 CLI/旧测试）下旧行为不变；旧的 `vote` 入口就是这一支的薄包装。
#[test]
fn without_a_viewer_the_vote_keeps_its_old_behaviour() {
    let fx = seeded();
    let before = attestations(fx.root.memory());
    vote_with_viewer(
        fx.root.memory(),
        None,
        &fx.a,
        VoteStance::Confirmed,
        // 正面表态的锚必须解析到该行的一条人工轮出处（`attest` 的 V6 核对）；
        // 夹具的出处是 `"e"*64`，所以锚只能是它。
        Some(&"e".repeat(64)),
        None,
        NOW,
    )
    .unwrap();
    assert_eq!(attestations(fx.root.memory()), before + 1, "viewer=None 被读闸挡住了");

    vote(fx.root.memory(), &fx.a, VoteStance::Corroborated, Some(&"e".repeat(64)), None, NOW)
        .unwrap();
    assert_eq!(attestations(fx.root.memory()), before + 2, "旧 `vote` 入口漂了");
}
