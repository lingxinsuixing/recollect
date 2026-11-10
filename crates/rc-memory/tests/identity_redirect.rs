//! W15c：两条身份查询（会话自锚 / 主人锚）把候选 id 过 `reader::redirect_terminal`。
//!
//! D8 的合并是**读侧指向**（旧锚不重键、不删，`anchor_merge` 一行就是指向），读侧八处已经
//! 走解析，而写入侧这两条身份查询原先是"算 id → 直接查活锚"：旧自锚/旧主人锚一被合并，
//! 新写的 `fact.speaker_anchor_id` / `mention.speaker_anchor_id` 就静默变 `None`。这一片钉
//! 五件事：
//! 1. 自锚被合并 ⇒ 返回幸存锚（退场那一态走生产 `merge_redirect`）；
//! 2. 主人锚被合并 ⇒ 返回幸存锚；旧锚分叉到两个活终点 ⇒ `None`（不猜），两旧锚同指一格 ⇒ 那一格；
//! 3. 没有合并时逐字不回归；
//! 4. 链式 A→B→C 与读侧同一个终端（返回 C）；
//! 5. 终端自己退场 ⇒ `None`（不返回死锚）。
//!
//! 夹具取向：自锚/主人锚走**生产的** `ensure_*_anchor`（写库层比的正是这两条内容寻址算法算
//! 出来的格）；幸存锚与"旧锚带归属行"的那一态直插——D8 零搬运，归属行本来就留在旧锚上，
//! 只有直插才复现得出来。

use rc_memory::keys::{AnchorRole, Scope, ScopeKind};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::redirect::merge_redirect;
use rc_memory::writers_fact::{put_fact_rows, FactWriteItem};
use rc_memory::writers_v4::{
    ensure_agent_anchor, ensure_agent_anchor_for, ensure_principal_anchor, session_agent_anchor_id,
    session_principal_anchor_id, write_a1_with_owner, A1Anchor, A1Batch, A1Mention,
};
use rc_memory::{compute_mem_id, BatchCtx, MemTable, Provenance};
use rc_store::{MemoryHandle, Root};

const NOW: i64 = 1_700_002_000_000;
const SRC_MS: i64 = 1_700_000_000_000;
const SOURCE_AGENT: &str = "agent-ir";
const PRINCIPAL: &str = "p-ir";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-ir".into() }
}

fn ctx() -> BatchCtx {
    BatchCtx { source_agent: SOURCE_AGENT.into(), device_id: "dev-ir".into() }
}

fn owner(principal: Option<&str>) -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: principal.map(str::to_string),
        owner_agent_id: SOURCE_AGENT.into(),
        visibility: Visibility::Project,
        retention: "default".into(),
    }
}

fn uid(index: usize) -> String {
    format!("{:064x}", 0x71 + index)
}

fn seeded() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    (dir, root)
}

/// 会话自锚（生产写入口；canonical = `SOURCE_AGENT`）。
fn ensure_self(m: &MemoryHandle) -> String {
    ensure_agent_anchor(m, &ctx(), &owner(None), &scope(), NOW).unwrap().id
}

/// 另一个 `kind='agent'` 锚（幸存锚/链中段）。
fn ensure_agent_named(m: &MemoryHandle, canonical: &str) -> String {
    ensure_agent_anchor_for(m, &ctx(), &owner(None), &scope(), canonical, NOW).unwrap().id
}

/// 直插一格锚（幸存锚不该被迫带归属行，旧锚的退场态也不该由生产路径代劳）。
fn seed_anchor(m: &MemoryHandle, kind: &str, canonical: &str, status: &str) -> String {
    let id = compute_mem_id(MemTable::Anchor, kind, canonical, "", &scope());
    m.conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind,
                                scope_key, status, profile_text, created_at_ms, updated_at_ms,
                                norm_version)
             VALUES(?1,?2,?3,'real',NULL,'project','proj-ir',?4,NULL,?5,?5,?6)",
            rusqlite::params![id, kind, canonical, status, NOW,
                              rc_store::capability::NORM_VERSION as i64],
        )
        .unwrap();
    id
}

/// 给一格锚补归属行。`device` 为 `None` ⇒ 主人锚契约那一格（`ensure_principal_anchor` 的形状）。
fn bind_owner(m: &MemoryHandle, id: &str, principal: Option<&str>, device: Option<&str>) {
    m.conn()
        .execute(
            "INSERT INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                        created_by_agent, visibility, retention, scope_kind,
                                        scope_key, source_agent, device_id, created_at_ms)
             VALUES('anchor', ?1, ?2, ?3, ?3, 'project', 'default', 'project', 'proj-ir', ?3, ?4, ?5)",
            rusqlite::params![id, principal, SOURCE_AGENT, device, NOW],
        )
        .unwrap();
}

/// 手写一行指向（`merge_redirect` 会把 from 退场；"from 仍是活锚"那一态只能直插）。
fn seed_pointer(m: &MemoryHandle, from: &str, to: &str) {
    m.conn()
        .execute(
            "INSERT INTO anchor_merge(from_id, to_id, run_id, reason, created_at_ms)
             VALUES(?1,?2,'run-ir','bad-data',?3)",
            rusqlite::params![from, to, NOW],
        )
        .unwrap();
}

fn merge(m: &MemoryHandle, from: &str, to: &str) {
    merge_redirect(m, from, to, "dedupe", "user", NOW).unwrap();
}

fn status_of(m: &MemoryHandle, id: &str) -> String {
    m.conn().query_row("SELECT status FROM anchor WHERE id=?1", [id], |r| r.get(0)).unwrap()
}

// ── ① 自锚被合并 ⇒ 幸存锚 ───────────────────────────────────────────────────

#[test]
fn merged_self_anchor_resolves_to_the_survivor() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let old = ensure_self(m);
    let survivor = ensure_agent_named(m, "agent-survivor");

    merge(m, &old, &survivor);
    assert_eq!(status_of(m, &old), "merged", "生产合并必须把旧锚退场（D8 的指向行形状）");

    assert_eq!(
        session_agent_anchor_id(m, SOURCE_AGENT, &scope()).unwrap().as_deref(),
        Some(survivor.as_str()),
        "旧自锚被合并后，新写的说话人必须落在幸存锚上（此前静默成 None）"
    );
}

/// 档与树不一致的取证：派发档说"`A` 可保持 active"也应返回 `B`。可 `redirect_terminal` 的
/// 行为里**活锚不沿链走**（`redirect::resolve` 对活锚直接返回 `Alive`，读侧八处同一个语义），
/// 而两条生产合并路径（`merge_redirect` / `merge_apply`）都必然把 from 退场——所以
/// "活锚 + 指向行"只是坏数据，解析出的终端就是它自己。这一条把该行为钉住，不去改解析器。
#[test]
fn an_alive_self_anchor_is_its_own_terminal() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let old = ensure_self(m);
    let survivor = ensure_agent_named(m, "agent-survivor");

    seed_pointer(m, &old, &survivor);
    assert_eq!(status_of(m, &old), "active");

    assert_eq!(
        session_agent_anchor_id(m, SOURCE_AGENT, &scope()).unwrap().as_deref(),
        Some(old.as_str()),
        "活锚的终端是它自己：与读侧同一个解析，不越活锚"
    );
}

#[test]
fn chained_self_anchor_merge_reaches_the_last_survivor() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let a = ensure_self(m);
    let b = ensure_agent_named(m, "agent-middle");
    let c = ensure_agent_named(m, "agent-last");

    merge(m, &b, &c);
    merge(m, &a, &b);

    assert_eq!(
        session_agent_anchor_id(m, SOURCE_AGENT, &scope()).unwrap().as_deref(),
        Some(c.as_str()),
        "A→B→C 的终端是 C（与读侧同一个链解析）"
    );
}

#[test]
fn a_retired_terminal_gives_none_not_the_old_anchor() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let old = ensure_self(m);
    let survivor = ensure_agent_named(m, "agent-survivor");
    merge(m, &old, &survivor);

    m.conn().execute("UPDATE anchor SET status='retired' WHERE id=?1", [&survivor]).unwrap();

    assert_eq!(
        session_agent_anchor_id(m, SOURCE_AGENT, &scope()).unwrap(),
        None,
        "终端已退场 ⇒ None（不返回死锚，也不回落到被合并的旧锚）"
    );
}

// ── ② 主人锚被合并 ⇒ 幸存锚；分叉 ⇒ None ─────────────────────────────────────

#[test]
fn merged_principal_anchor_resolves_to_the_survivor() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let old = ensure_principal_anchor(m, PRINCIPAL, Some("小明"), &owner(Some(PRINCIPAL)),
                                      &scope(), NOW)
        .unwrap()
        .expect("身份已知时必须建主人锚")
        .id;
    // 幸存锚**不带归属行**：D8 零搬运，合并不会把 `resource_owner` 搬到新锚上。
    let survivor = seed_anchor(m, "person", "幸存主人", "active");

    merge(m, &old, &survivor);

    assert_eq!(
        session_principal_anchor_id(m, PRINCIPAL, &scope()).unwrap().as_deref(),
        Some(survivor.as_str()),
        "旧主人锚退场后，归属行留在旧锚上；沿链走到幸存锚才是'谁说的'"
    );
}

#[test]
fn two_stale_candidates_pointing_to_one_survivor_dedupe() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let first = seed_anchor(m, "person", "旧主人甲", "active");
    let second = seed_anchor(m, "person", "旧主人乙", "active");
    bind_owner(m, &first, Some(PRINCIPAL), None);
    bind_owner(m, &second, Some(PRINCIPAL), None);
    let survivor = seed_anchor(m, "person", "幸存主人", "active");

    merge(m, &first, &survivor);
    merge(m, &second, &survivor);

    assert_eq!(
        session_principal_anchor_id(m, PRINCIPAL, &scope()).unwrap().as_deref(),
        Some(survivor.as_str()),
        "两个旧锚都指向同一格 ⇒ 去重后恰好一个终端"
    );
}

#[test]
fn two_live_terminals_are_none_not_a_guess() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let first = seed_anchor(m, "person", "旧主人甲", "active");
    let second = seed_anchor(m, "person", "旧主人乙", "active");
    bind_owner(m, &first, Some(PRINCIPAL), None);
    bind_owner(m, &second, Some(PRINCIPAL), None);
    let left = seed_anchor(m, "person", "幸存甲", "active");
    let right = seed_anchor(m, "person", "幸存乙", "active");

    merge(m, &first, &left);
    merge(m, &second, &right);

    assert_eq!(
        session_principal_anchor_id(m, PRINCIPAL, &scope()).unwrap(),
        None,
        "旧锚分叉到两个活终点 ⇒ None（两个终点就是两个主张，不替数据挑一个）"
    );
}

#[test]
fn a_retired_principal_terminal_gives_none() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let old = ensure_principal_anchor(m, PRINCIPAL, Some("小明"), &owner(Some(PRINCIPAL)),
                                      &scope(), NOW)
        .unwrap()
        .expect("身份已知时必须建主人锚")
        .id;
    let survivor = seed_anchor(m, "person", "幸存主人", "active");
    merge(m, &old, &survivor);

    m.conn().execute("UPDATE anchor SET status='retired' WHERE id=?1", [&survivor]).unwrap();

    assert_eq!(
        session_principal_anchor_id(m, PRINCIPAL, &scope()).unwrap(),
        None,
        "终端退场 ⇒ 0 个活终端 ⇒ None（不返回死锚）"
    );
}

// ── ③ 无合并不回归 ───────────────────────────────────────────────────────────

#[test]
fn without_a_merge_both_queries_are_unchanged() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let self_anchor = ensure_self(m);
    let principal_anchor =
        ensure_principal_anchor(m, PRINCIPAL, Some("小明"), &owner(Some(PRINCIPAL)), &scope(), NOW)
            .unwrap()
            .expect("身份已知时必须建主人锚")
            .id;

    assert_eq!(
        session_agent_anchor_id(m, SOURCE_AGENT, &scope()).unwrap().as_deref(),
        Some(self_anchor.as_str()),
        "普通自锚逐字不变"
    );
    assert_eq!(
        self_anchor,
        compute_mem_id(MemTable::Anchor, "agent", SOURCE_AGENT, "", &scope()),
        "返回的就是内容寻址那一格"
    );
    assert_eq!(
        session_principal_anchor_id(m, PRINCIPAL, &scope()).unwrap().as_deref(),
        Some(principal_anchor.as_str()),
        "普通主人锚逐字不变"
    );
    // 身份未知/空：两条查询都是 None（不造无名用户/无名 agent）。
    assert_eq!(session_principal_anchor_id(m, "unknown", &scope()).unwrap(), None);
    assert_eq!(session_principal_anchor_id(m, "   ", &scope()).unwrap(), None);
    assert_eq!(session_agent_anchor_id(m, "   ", &scope()).unwrap(), None);
    // 没有合并过的陌生 agent 名下也没有自锚：查不到 ⇒ None（不造锚）。
    assert_eq!(session_agent_anchor_id(m, "agent-never-seen", &scope()).unwrap(), None);
}

// ── ④ 合并之后写进去的说话人 ─────────────────────────────────────────────────

fn fact_item(subject: &str, text: &str, evidence: Vec<(String, AnchorRole)>) -> FactWriteItem {
    FactWriteItem {
        subject_anchor_id: subject.to_string(),
        object_anchor_id: None,
        predicate: "decide".into(),
        value: Some("把发布推迟一天".into()),
        category: "decision".into(),
        mode: "stated".into(),
        time_text: String::new(),
        quote: String::new(),
        evidence,
        text: text.to_string(),
        scope: scope(),
        source_ms: SRC_MS,
    }
}

fn mention_speaker(m: &MemoryHandle, canonical: &str) -> Option<String> {
    m.conn()
        .query_row(
            "SELECT m.speaker_anchor_id FROM mention m JOIN anchor a ON a.id=m.anchor_id
              WHERE a.canonical=?1",
            [canonical],
            |r| r.get::<_, Option<String>>(0),
        )
        .unwrap()
}

/// 审计点名的那一格：合并之后**新写**的 fact/mention 说话人必须落在幸存锚上。
#[test]
fn facts_written_after_a_merge_keep_their_speaker() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let old_self = ensure_self(m);
    let survivor = ensure_agent_named(m, "agent-survivor");
    merge(m, &old_self, &survivor);

    let old_principal =
        ensure_principal_anchor(m, PRINCIPAL, Some("小明"), &owner(Some(PRINCIPAL)), &scope(), NOW)
            .unwrap()
            .expect("身份已知时必须建主人锚")
            .id;
    let principal_survivor = seed_anchor(m, "person", "幸存主人", "active");
    merge(m, &old_principal, &principal_survivor);

    let subject = seed_anchor(m, "person", "发布", "active");
    put_fact_rows(
        m,
        &[
            fact_item(&subject, "agent 轮记下的一条", vec![(uid(1), AnchorRole::Agent)]),
            fact_item(&subject, "human 轮记下的一条", vec![(uid(2), AnchorRole::Human)]),
        ],
        &ctx(),
        &owner(Some(PRINCIPAL)),
        NOW,
    )
    .unwrap();

    let speakers: Vec<(Option<String>, String)> = {
        let mut st = m
            .conn()
            .prepare("SELECT speaker_anchor_id, text FROM fact WHERE subject_anchor_id=?1 ORDER BY text")
            .unwrap();
        let rows = st
            .query_map([&subject], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?)))
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    };
    assert_eq!(
        speakers,
        vec![
            (Some(survivor.clone()), "agent 轮记下的一条".to_string()),
            (Some(principal_survivor.clone()), "human 轮记下的一条".to_string()),
        ],
        "agent 轮 ⇒ 自锚的幸存锚；human 轮 ⇒ 主人锚的幸存锚（都不是 None）"
    );

    // 轮级提及同一格：agent 轮的 mention 说话人也是自锚的幸存锚。
    write_a1_with_owner(
        m,
        &A1Batch {
            anchors: vec![A1Anchor {
                reference: "a1".into(),
                kind: "person".into(),
                canonical: "老张".into(),
                aliases: Vec::new(),
                reality_status: "real".into(),
                evidence: vec![Provenance { turn_uid: uid(3), anchor_role: AnchorRole::Agent }],
                stable: true,
            }],
            mentions: vec![A1Mention {
                anchor_ref: "a1".into(),
                surface: "老张".into(),
                turn_uid: uid(3),
                speaker_ref: None,
                discourse_role: "mentioned".into(),
                resolution: "resolved".into(),
                confidence: 0.9,
            }],
            issues: Vec::new(),
            unresolved: Vec::new(),
        },
        &ctx(),
        &owner(Some(PRINCIPAL)),
        &scope(),
        "run-ir",
        NOW,
    )
    .unwrap();
    assert_eq!(
        mention_speaker(m, "老张").as_deref(),
        Some(survivor.as_str()),
        "合并后新写的 mention 说话人同样落在幸存锚上"
    );
}
