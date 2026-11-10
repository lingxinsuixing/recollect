//! 主人锚的**身份**只认稳定主键，昵称是别名。（用户 11-01 拍板：改名不换锚）
//!
//! 这一片先钉两件事（显示名那一半见 `principal_display_name.rs`）：
//! 1. **身份一格**：`canonical` 恒等于 `principal_id`。旧行为是"名字非空就用名字当 canonical"，
//!    于是同一个人在"有的 runtime 给得出显示名、有的给不出"时算出**两个 id** ⇒ 两格锚、
//!    两条互不相干的 `state_assertion` 时间线；而 `status='merged'` 全仓**没有生产者**
//!    ⇒ 裂开了没有任何机制能合回来。
//! 2. **可读名进 `alias`**：每次带的称呼都追加成同一格锚的别名（`alias` 本就带
//!    `valid_from_ms/valid_to_ms`，"称呼分段生效"是它的本职；旧称呼不删，历史正文里还是那个名字）。
//!
//! 夹具取向与 `speaker_bind.rs` 一致：主人锚走**生产的** `ensure_principal_anchor`，不自拼 id。
use rc_memory::keys::{AnchorRole, Scope, ScopeKind};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_v4::{
    ensure_principal_anchor, write_a1_with_owner, A1Anchor, A1Batch,
};
use rc_memory::{BatchCtx, MemError, Provenance};
use rc_store::{MemoryHandle, Root};

const NOW: i64 = 1_700_000_500_000;
const PRINCIPAL: &str = "acct-7";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-principal".into() }
}

fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some(PRINCIPAL.into()),
        owner_agent_id: "agent-principal".into(),
        visibility: Visibility::Person,
        retention: "keep".into(),
    }
}

fn seeded() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    (dir, root)
}

fn ensure(m: &MemoryHandle, name: Option<&str>) -> Result<String, MemError> {
    ensure_at(m, name, NOW)
}

fn ensure_at(m: &MemoryHandle, name: Option<&str>, at_ms: i64) -> Result<String, MemError> {
    ensure_principal_anchor(m, PRINCIPAL, name, &owner(), &scope(), at_ms)
        .map(|key| key.expect("身份已知时必须建主人锚").id)
}

/// 一格锚的两副面孔：身份那格 `canonical`，以及挂在它下面的别名集合（升序）。
fn faces(m: &MemoryHandle, id: &str) -> (String, Vec<String>) {
    let conn = m.conn();
    let canonical: String = conn
        .query_row("SELECT canonical FROM anchor WHERE id=?1", [id], |r| r.get(0))
        .unwrap();
    let mut st = conn
        .prepare("SELECT alias FROM alias WHERE anchor_id=?1 ORDER BY alias")
        .unwrap();
    let aliases = st
        .query_map([id], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|row| row.unwrap())
        .collect();
    (canonical, aliases)
}

/// 1 + 2：改昵称 ⇒ **同一格锚**，两个称呼都挂在它身上。
#[test]
fn a_renamed_principal_is_the_same_anchor_not_a_second_one() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let first = ensure(m, Some("小林")).unwrap();
    let (canonical, aliases) = faces(m, &first);
    assert_eq!(canonical, PRINCIPAL, "身份那一格必须是稳定主键，不是可读名");
    assert_eq!(aliases, vec!["小林".to_string()], "可读名进 alias 表");

    // 同一个 principal 换了称呼（改名 / 另一台设备报上来的名字 / 工作号）
    let second = ensure(m, Some("小林（工作号）")).unwrap();
    assert_eq!(first, second, "改名不许换锚——`merged` 没有生产者，裂开就合不回来");
    let (canonical, aliases) = faces(m, &first);
    assert_eq!(canonical, PRINCIPAL);
    assert_eq!(
        aliases,
        vec!["小林".to_string(), "小林（工作号）".to_string()],
        "每次带的称呼都追加成同一格锚的别名，旧称呼不删"
    );

    // 锚表只有一格 person：别退化成"两格都留着、靠读者自己认"
    let persons: i64 = m
        .conn()
        .query_row("SELECT count(*) FROM anchor WHERE kind='person'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(persons, 1, "同一个人只能有一格主人锚");
}

/// 不带名字的章（有的 runtime 只给得出主键）⇒ 同一格锚，且不写空别名。
#[test]
fn a_claim_without_a_display_name_still_lands_on_the_same_anchor() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let with_name = ensure(m, Some("小林")).unwrap();
    let without = ensure(m, None).unwrap();
    assert_eq!(with_name, without, "有没有可读名都不许换锚");
    assert_eq!(faces(m, &with_name).1, vec!["小林".to_string()], "没名字时不写空别名");
}

/// 显示面（`reader::display_name`）：最新的有效别名优先、关段的不再露出、没有别名才回落主键。
/// 身份那一格是账号主键，给人/给 LLM 看的那一面必须是名字——否则消费句里出现一串 id，
/// 违反"LLM 面用自然语言"那条消费契约。
#[test]
fn the_display_name_is_the_newest_live_alias_and_only_falls_back_to_the_key() {
    use rc_memory::reader::display_name;
    let (_dir, root) = seeded();
    let m = root.memory();

    let principal = ensure_at(m, Some("小林"), NOW).unwrap();
    assert_eq!(display_name(m, &principal).unwrap(), "小林");

    // 后来盖了一个新称呼 ⇒ 显示名跟着走，锚还是那一格
    assert_eq!(ensure_at(m, Some("小林（工作号）"), NOW + 60_000).unwrap(), principal);
    assert_eq!(display_name(m, &principal).unwrap(), "小林（工作号）");

    // 新称呼**关段**了（换回原名 / 那个称呼不再用）⇒ 不許再露出，退回还在用的那一个
    m.conn()
        .execute(
            "UPDATE alias SET valid_to_ms=?1 WHERE anchor_id=?2 AND alias=?3",
            rusqlite::params![NOW + 120_000, principal.clone(), "小林（工作号）"],
        )
        .unwrap();
    assert_eq!(display_name(m, &principal).unwrap(), "小林", "关段的别名不算显示名");

    // 全关段 ⇒ 才回落身份那一格（账号主键），不是返回空串或报错
    m.conn()
        .execute(
            "UPDATE alias SET valid_to_ms=?1 WHERE anchor_id=?2",
            rusqlite::params![NOW + 180_000, principal.clone()],
        )
        .unwrap();
    assert_eq!(display_name(m, &principal).unwrap(), PRINCIPAL);

    // 没有别名的普通锚（语料里的人名/概念）⇒ 一个字符都不多
    let plain = anchor_direct(m, "concept", "砍价");
    assert_eq!(display_name(m, &plain).unwrap(), "砍价");
}

/// B 的另一半：同名第三方**不并入**主人，但也不许**无声地遮住**主人——
/// 新锚的正名如果已经是"同 kind、同 scope"里某格活锚的别名 ⇒ 两格都挂 `alias_ambiguity`，
/// 让"这句话到底说谁"变成一件可见、待裁的事，而不是靠读者猜。
#[test]
fn a_new_anchor_shadowing_the_principals_display_name_raises_ambiguity_on_both_sides() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let principal = ensure(m, Some("小林")).unwrap();
    let ctx = BatchCtx {
        source_agent: "agent-principal".into(),
        device_id: "dev-principal".into(),
    };
    let batch = A1Batch {
        anchors: vec![A1Anchor {
            reference: "a1".into(),
            kind: "person".into(),
            canonical: "小林".into(),
            aliases: Vec::new(),
            reality_status: "real".into(),
            evidence: vec![Provenance {
                turn_uid: format!("{:064x}", 0x93u64),
                anchor_role: AnchorRole::Human,
            }],
            stable: true,
        }],
        mentions: Vec::new(),
        issues: Vec::new(),
        unresolved: Vec::new(),
    };
    let out = write_a1_with_owner(m, &batch, &ctx, &owner(), &scope(), "run-shadow", NOW).unwrap();

    let persons: i64 = m
        .conn()
        .query_row("SELECT count(*) FROM anchor WHERE kind='person'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(persons, 2, "B：主人格与同名第三方各留一格，不许静默合并");
    let mut st = m.conn().prepare(
        "SELECT mem_id FROM attention WHERE kind='alias_ambiguity' AND mem_table='anchor'
         AND resolved_at_ms IS NULL ORDER BY mem_id",
    ).unwrap();
    let flagged: Vec<String> = st
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(flagged.len(), 2, "新格与主人格都要挂上歧义告警：{flagged:?}");
    assert!(flagged.contains(&principal), "主人那一格必须在告警名单里");
    assert!(out.attention >= 1, "告警计数也要带上，调用方才看得见");
}

/// 直插一格锚：只给"没有别名的普通锚"这一类对照用（id 走生产的同一套内容寻址算法）。
fn anchor_direct(m: &MemoryHandle, kind: &str, canonical: &str) -> String {
    use rc_memory::identity::compute_mem_id;
    use rc_memory::keys::MemTable;

    let id = compute_mem_id(MemTable::Anchor, kind, canonical, "", &scope());
    m.conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind, scope_key,
                              status, profile_text, created_at_ms, updated_at_ms, norm_version)
             VALUES(?1,?2,?3,'real',NULL,?4,?5,'active',NULL,?6,?6,1)",
            rusqlite::params![id, kind, canonical, scope().kind.as_str(), scope().key, NOW],
        )
        .unwrap();
    id
}
