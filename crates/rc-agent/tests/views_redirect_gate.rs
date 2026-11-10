//! 视图面的指向解析 + 不绕闸（D8 方案二，判据 6）：合并后 fact 的 subject 按**幸存锚**
//! 名字渲染（解析面）；无权读者对幸存锚被拒 ⇒ 与"锚不存在"同形（回落 id 前缀兜底）。
//!
//! 夹具用真库 + 真 SQL（与 `tests/views.rs` 同族）：锚行 + `resource_owner` + fact 行 +
//! `hub` 同款写侧 `rc_memory::redirect::merge_redirect`。
use rc_agent::views::{self, ViewRequest};
use rc_memory::gate::Viewer;
use rc_memory::redirect::merge_redirect;
use rc_memory::{compute_mem_id, MemTable, Scope, ScopeKind};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;
const SCOPE: &str = "proj-redirect-view";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: SCOPE.into() }
}

/// 锚行 + 它的句柄：id 按生产算法算（`kind`/canonical/scope 就是这一行写进去的三格），
/// 所以铺下的行与手上那串 hex 同源，写错一格也只会当场对不上、不会静默漂。
fn anchor(root: &Root, canonical: &str, status: &str) -> String {
    let id = compute_mem_id(MemTable::Anchor, "person", canonical, "", &scope());
    root.memory()
        .conn()
        .execute(
            "INSERT INTO anchor(id,kind,canonical,reality_status,scope_kind,scope_key,status,
                                created_at_ms,updated_at_ms,norm_version)
             VALUES(?1,'person',?2,'real','project',?3,?4,?5,?5,1)",
            (&id, canonical, SCOPE, status, NOW),
        )
        .unwrap();
    id
}

/// 一条**活**别名（`valid_to_ms IS NULL`、带 `valid_from_ms`）：`display_name` 认的那一支。
fn alias(root: &Root, id: &str, name: &str) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO alias(anchor_id,alias,valid_from_ms,valid_to_ms,source_turn_uid)
             VALUES(?1,?2,?3,NULL,NULL)",
            (id, name, NOW),
        )
        .unwrap();
}

/// 锚的归属行（读闸判"这格锚给谁看"的唯一凭据）。
fn anchor_owner(root: &Root, id: &str, agent: &str) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO resource_owner(mem_table,mem_id,owner_principal_id,owner_agent_id,
                                        created_by_agent,visibility,retention,scope_kind,scope_key,
                                        source_agent,device_id,created_at_ms)
             VALUES('anchor',?1,NULL,?2,?2,'private_agent','default','project',?3,?2,'dev',?4)",
            (id, agent, SCOPE, NOW),
        )
        .unwrap();
}

fn fact(root: &Root, id: &str, subject: &str, agent: &str) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO fact(id,subject_anchor_id,predicate,category,participants_json,mode,
                              confidence,text,evidence_turn_uid,scope_kind,scope_key,
                              source_agent,device_id,created_at_ms,norm_version)
             VALUES(?1,?2,'p','observation','[]','stated',0.8,'正文',?3,'project',?4,?5,'dev',?6,1)",
            (id, subject, "e".repeat(64), SCOPE, agent, NOW),
        )
        .unwrap();
    root.memory()
        .conn()
        .execute(
            "INSERT INTO resource_owner(mem_table,mem_id,owner_principal_id,owner_agent_id,
                                        created_by_agent,visibility,retention,scope_kind,scope_key,
                                        source_agent,device_id,created_at_ms)
             VALUES('fact',?1,NULL,?2,?2,'private_agent','default','project',?3,?2,'dev',?4)",
            (id, agent, SCOPE, NOW),
        )
        .unwrap();
}

fn merge(root: &Root, from: &str, to: &str) {
    merge_redirect(
        root.memory(), from, to, "同一人", "user", NOW,
    )
    .unwrap();
}

fn timeline_json(root: &Root, agent: &str) -> serde_json::Value {
    let viewer = Viewer::agent(agent);
    let req = ViewRequest {
        view: "timeline",
        scope: None,
        from_ms: None,
        to_ms: None,
        limit: 10,
    };
    let out = views::render(root.memory(), &req, &viewer, NOW).unwrap();
    serde_json::from_str(&out).unwrap()
}

/// 合并后：有权读者看到 subject 渲成**幸存锚**的名字面（解析面）。
///
/// W20 起名字面取 `reader::display_name`（活别名优先）。`merge_redirect` 会把旧锚的正名
/// 并进幸存锚的 `alias`，而并入那一行的 `valid_from_ms` 是 NULL——所以这里给幸存锚自己
/// 一条**带日期**的活别名：显示名确定性地落在幸存锚这一侧，断言仍然证明"渲染归属 to 锚"，
/// 而不是"恰好念出了旧锚的名字"。
#[test]
fn merged_subject_renders_as_the_survivor_for_an_allowed_viewer() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let to = anchor(&root, "幸存者", "active");
    alias(&root, &to, "幸存者别名");
    let from = anchor(&root, "旧名", "active");
    anchor_owner(&root, &to, "agent-a");
    fact(&root, "f1", &from, "agent-a");

    // 合并前：subject 是旧锚自己的正名。
    let before = timeline_json(&root, "agent-a");
    assert_eq!(before["items"][0]["subject"], "旧名");

    merge(&root, &from, &to);
    let after = timeline_json(&root, "agent-a");
    assert_eq!(after["items"][0]["subject"], "幸存者别名",
               "解析发生且过闸：渲染归属幸存锚的显示名");
    assert_ne!(after["items"][0]["subject"], "旧名",
               "并入的旧正名（NULL 日期的那一行）不许盖掉幸存锚自己的活别名");
}

/// 指向不绕闸：无权读者（对幸存锚既非 owner 又无 grant）看到的是"锚不存在"的
/// 同形（id 前缀兜底），幸存锚的名字一个字不泄。
#[test]
fn a_denied_viewer_sees_the_nonexistent_shape_not_the_survivor() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let to = anchor(&root, "幸存者", "active");
    let from = anchor(&root, "旧名", "active");
    // 幸存锚归 agent-b 私有；fact 行归 agent-a（读者能看见这一行本身）。
    anchor_owner(&root, &to, "agent-b");
    fact(&root, "f1", &from, "agent-a");

    merge(&root, &from, &to);
    let out = timeline_json(&root, "agent-a");
    let fallback: String = from.chars().take(8).collect();
    assert_eq!(out["items"][0]["subject"], fallback,
               "被拒 ⇒ 与锚不存在同形（id 前缀兜底），不泄幸存锚名字");
    assert_ne!(out["items"][0]["subject"], "幸存者");
}

/// Broken（断链）回落旧锚自己的名字面：可见，不静默成"没有"。
#[test]
fn a_broken_chain_falls_back_to_the_old_anchors_own_name() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let to = anchor(&root, "幸存者", "active");
    let from = anchor(&root, "旧名", "active");
    anchor_owner(&root, &to, "agent-a");
    fact(&root, "f1", &from, "agent-a");
    merge(&root, &from, &to);
    // 变异：物理删掉幸存锚 ⇒ 断链。旧锚自己的行还在（canonical 未动）。
    root.memory().conn().execute("DELETE FROM anchor WHERE id=?1", [&to]).unwrap();
    let out = timeline_json(&root, "agent-a");
    assert_eq!(out["items"][0]["subject"], "旧名",
               "Broken 回落旧锚自己的行：可见，不是 id 前缀也不是空");
}

/// 零跳（Alive）不新增闸判定：既有形状逐位不变（本单不收紧既有面）。
#[test]
fn an_alive_anchor_keeps_the_preexisting_shape() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let subject = anchor(&root, "无归属锚", "active");
    // 锚没有归属行（旧库形状）：Alive 不走解析后的闸，名字面照旧渲出。
    fact(&root, "f1", &subject, "agent-a");
    let out = timeline_json(&root, "agent-a");
    assert_eq!(out["items"][0]["subject"], "无归属锚");
}
