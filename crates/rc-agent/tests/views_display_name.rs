//! W20（P3-1）：视图主语念**显示名**（`reader::display_name`）——最新有效别名优先，
//! 取不到才回落正名；过闸语义一字不动（解析发生且被拒 ⇒ 与"锚不存在"同形）。
//!
//! 夹具与 `tests/views_redirect_gate.rs` 同族：真库 + 真 SQL（锚行 / 别名行 / 归属行 /
//! fact 行 / `rc_memory::redirect::merge_redirect`），断的是视图自己的渲染规则。
use rc_agent::views::{self, ViewRequest};
use rc_memory::gate::Viewer;
use rc_memory::redirect::merge_redirect;
use rc_memory::{compute_mem_id, MemTable, Scope, ScopeKind};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;
const SCOPE: &str = "proj-view-display-name";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: SCOPE.into() }
}

/// 锚行 + 它的句柄：id 按生产算法算（`kind`/canonical/scope 就是这一行写进去的三格），
/// 铺下的行与手上那串 hex 同源。
fn anchor_of_kind(root: &Root, kind: &str, canonical: &str, status: &str) -> String {
    let id = compute_mem_id(MemTable::Anchor, kind, canonical, "", &scope());
    root.memory()
        .conn()
        .execute(
            "INSERT INTO anchor(id,kind,canonical,reality_status,scope_kind,scope_key,status,
                                created_at_ms,updated_at_ms,norm_version)
             VALUES(?1,?2,?3,'real','project',?4,?5,?6,?6,1)",
            (&id, kind, canonical, SCOPE, status, NOW),
        )
        .unwrap();
    id
}

fn anchor(root: &Root, canonical: &str, status: &str) -> String {
    anchor_of_kind(root, "person", canonical, status)
}

/// 一条**活**别名（`valid_to_ms IS NULL`）：`display_name` 只认这一支。
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

/// 过闸之后念**别名**：person 锚的正名是"主人"，活别名是"示例主人"，视图主语必须是后者。
///
/// 这一条是 P3-1 的正身：把 `display_name` 换回 `anchor_names(..).canonical`（旧形状）
/// 时它当场红，而下面两条不动——所以它证明的正是"显示名"这一刀，不是别的。
#[test]
fn the_view_shows_the_live_alias_not_the_canonical_name() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let id = anchor(&root, "主人", "active");
    alias(&root, &id, "示例主人");
    anchor_owner(&root, &id, "agent-a");
    fact(&root, "f1", &id, "agent-a");

    let out = timeline_json(&root, "agent-a");
    assert_eq!(out["items"][0]["subject"], "示例主人",
               "过闸后要念别名（最新有效别名），不是正名：{out}");
    assert_ne!(out["items"][0]["subject"], "主人", "还在念正名 ⇒ 这一刀没接上：{out}");
}

/// 指向不绕闸：解析发生（Merged）且幸存锚被拒 ⇒ 与 Missing 同形（id 前缀），
/// **别名也一个字不泄**。名字面换了来源，闸的那一道不许因此松掉。
#[test]
fn a_denied_viewer_sees_the_missing_shape_even_when_the_survivor_has_an_alias() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let to = anchor(&root, "幸存者", "active");
    alias(&root, &to, "示例主人");
    let from = anchor(&root, "旧名", "active");
    // 幸存锚归 agent-b 私有；fact 行归 agent-a（读者能看见这一行本身）。
    anchor_owner(&root, &to, "agent-b");
    fact(&root, "f1", &from, "agent-a");

    merge_redirect(root.memory(), &from, &to, "同一人", "user", NOW).unwrap();
    let out = timeline_json(&root, "agent-a");
    let fallback: String = from.chars().take(8).collect();
    assert_eq!(out["items"][0]["subject"], fallback,
               "被拒 ⇒ 与锚不存在同形（id 前缀兜底）：{out}");
    assert_ne!(out["items"][0]["subject"], "示例主人", "别名经指向行泄露了：{out}");
    assert_ne!(out["items"][0]["subject"], "幸存者", "正名也不许泄露：{out}");
}

/// 没有别名 ⇒ 显示面与旧形状逐字相同（正名）。这一条是"换名字面"那刀的分界：
/// 别名优先不等于别名必需。
#[test]
fn an_anchor_without_an_alias_still_shows_the_canonical_name() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let id = anchor(&root, "无别名锚", "active");
    anchor_owner(&root, &id, "agent-a");
    fact(&root, "f1", &id, "agent-a");

    let out = timeline_json(&root, "agent-a");
    assert_eq!(out["items"][0]["subject"], "无别名锚", "取不到别名要回落正名：{out}");
}

/// 合并之后念的是**幸存锚**的别名：合并把旧正名并进了 to 锚的 `alias`，而
/// `display_name` 按 `COALESCE(valid_from_ms,0) DESC` 取名 —— 并入的那一行
/// `valid_from_ms` 是 NULL，所以幸存锚自己的活别名照旧赢。这条把两件事一起钉住：
/// 解析到终点 + 终点念别名。
#[test]
fn a_merged_subject_shows_the_survivors_alias() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let to = anchor(&root, "幸存者", "active");
    alias(&root, &to, "示例主人");
    let from = anchor(&root, "旧名", "active");
    anchor_owner(&root, &to, "agent-a");
    fact(&root, "f1", &from, "agent-a");

    merge_redirect(root.memory(), &from, &to, "同一人", "user", NOW).unwrap();
    let out = timeline_json(&root, "agent-a");
    assert_eq!(out["items"][0]["subject"], "示例主人",
               "合并后要念幸存锚的别名：{out}");
}

/// W20c（用户 2026-11-06 拍板 B）：**非 person 锚即使有别名也用正名**。
/// `display_name` 的别名是"最新有效别名"而不是被挑过的显示名；work/agent 这类锚的
/// 正名才是稳定标识，用别名替换会让标签随别名史漂。
#[test]
fn a_non_person_alias_does_not_replace_the_canonical() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let id = anchor_of_kind(&root, "work", "稳定正名", "active");
    alias(&root, &id, "某个旧版本别名");
    anchor_owner(&root, &id, "agent-a");
    fact(&root, "f1", &id, "agent-a");

    let out = timeline_json(&root, "agent-a");
    assert_eq!(out["items"][0]["subject"], "稳定正名",
               "非 person 锚必须用正名，别名不许替换：{out}");
}
