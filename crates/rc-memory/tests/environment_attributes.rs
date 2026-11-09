//! W14c：环境/账号属性（`attribute` 表）的生产者与解析（真库，零网络）。
//!
//! 用户 2026-11-06 的两句话是这一片的判据：
//! "以上所有信息都归属于示例主人这个实体，不过指定名字是归属于哪个实体环境/设备在使用。"
//! "agent 查询『甲某随行』，那么能获取的信息是示例主人在 B 站的账户的昵称。"
//!
//! 所以这一片钉四件事：
//! 1. **一个环境一格锚**：`kind='object'`、canonical=主机名、幂等；归属行写主人与设备；
//! 2. **属性是值不是实体**：同 `(锚, 键, 同值)` 重导不增行；值变了写新行并关旧行；
//!    值变回旧值重开同一格（内容寻址的代价与它的补法）；
//! 3. **解析带来源**：`resolve_surface` 报 `canonical` / `alias` / `attribute:<key>` 与角色名；
//!    同一值命中多个环境时**全部报出**，不选一个；
//! 4. **`entity_ids_for_name` 并入属性值**（`entity=` 过滤因此"属性名命中也算命中该锚"）。
//!
//! 夹具取向与 `principal_identity.rs` 一致：主人锚走**生产的** `ensure_principal_anchor`，
//! 环境锚与属性走本片的生产入口，不自拼 id。
use rc_memory::keys::{Scope, ScopeKind};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::reader::{attribute_owners, entity_ids_for_name, resolve_surface};
use rc_memory::writers_v4::ensure_principal_anchor;
use rc_memory::{
    ensure_environment_anchor, put_attribute_rows, AttrWriteOutcome, BatchCtx, MemError,
    ATTR_BILIBILI_ACCOUNT, ATTR_BRAND, ATTR_DEVICE_ID, ATTR_HOSTNAME, ATTR_OS_USER,
    ATTR_QODER_ACCOUNT,
};
use rc_store::{MemoryHandle, Root};

const NOW: i64 = 1_700_000_600_000;
const LATER: i64 = 1_700_000_900_000;
const PRINCIPAL: &str = "示例主人";
const HOST: &str = "example-host";
const BILI: &str = "甲某随行";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "recollect".into() }
}

fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some(PRINCIPAL.into()),
        owner_agent_id: "qoder".into(),
        visibility: Visibility::Person,
        retention: "keep".into(),
    }
}

fn ctx() -> BatchCtx {
    BatchCtx { source_agent: "qoder".into(), device_id: "local-qoder".into() }
}

fn seeded() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    (dir, root)
}

/// 生产形态：主人锚（canonical=示例主人、别名=示例主人）+ 一格环境锚。
fn with_environment(m: &MemoryHandle) -> String {
    ensure_principal_anchor(m, PRINCIPAL, Some(PRINCIPAL), &owner(), &scope(), NOW)
        .unwrap()
        .expect("身份已知 ⇒ 主人锚必建");
    ensure_environment_anchor(m, HOST, &ctx(), &owner(), &scope(), NOW).unwrap()
}

/// 声明的五格 + 设备标识一条（与 `hub::ingest::write_declared_environment` 同一组）。
fn declared() -> Vec<(&'static str, String)> {
    vec![
        (ATTR_HOSTNAME, HOST.to_string()),
        (ATTR_BRAND, "Acer".to_string()),
        (ATTR_OS_USER, "u".to_string()),
        (ATTR_BILIBILI_ACCOUNT, BILI.to_string()),
        (ATTR_QODER_ACCOUNT, "example-account".to_string()),
        (ATTR_DEVICE_ID, "local-qoder".to_string()),
    ]
}

fn count(m: &MemoryHandle, sql: &str) -> i64 {
    m.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

/// 一个 `(锚, 键)` 的活行 `(id, value_text, superseded_by)`。
fn live_rows(m: &MemoryHandle, anchor_id: &str, key: &str) -> Vec<(String, String, Option<String>)> {
    let mut st = m
        .conn()
        .prepare(
            "SELECT id, value_text, superseded_by FROM attribute
              WHERE anchor_id=?1 AND key=?2 AND valid_to_ms IS NULL ORDER BY id",
        )
        .unwrap();
    st.query_map([anchor_id, key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

// ── ① 环境锚：一格、幂等、归属是主人 ─────────────────────────────────────

#[test]
fn 环境锚只建一格_重写不长第二行_归属是主人且带设备() {
    let (_d, root) = seeded();
    let m = root.memory();
    let first = with_environment(m);
    let second = ensure_environment_anchor(m, HOST, &ctx(), &owner(), &scope(), LATER).unwrap();
    assert_eq!(first, second, "同 (canonical, scope) 必须落在同一格");
    assert_eq!(
        count(m, "SELECT count(*) FROM anchor WHERE kind='object' AND canonical='example-host'"),
        1,
        "重写不长第二格锚"
    );
    let (kind, canonical): (String, String) = m
        .conn()
        .query_row("SELECT kind, canonical FROM anchor WHERE id=?1", [&first], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((kind.as_str(), canonical.as_str()), ("object", HOST), "环境是 object 锚、名字是主机名");
    let (principal, device): (Option<String>, Option<String>) = m
        .conn()
        .query_row(
            "SELECT owner_principal_id, device_id FROM resource_owner
              WHERE mem_table='anchor' AND mem_id=?1",
            [&first],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(principal.as_deref(), Some(PRINCIPAL), "环境的归属是主人，不是它自己");
    assert_eq!(device.as_deref(), Some("local-qoder"), "环境锚带设备；device_id IS NULL 是主人锚的契约");
}

// ── ② 属性：幂等 / 换值关旧行 / 值回来重开同一格 ─────────────────────────

#[test]
fn 六条属性写一次_同值重导不增行() {
    let (_d, root) = seeded();
    let m = root.memory();
    let anchor = with_environment(m);
    let first = put_attribute_rows(m, &anchor, &declared(), &scope(), &ctx(), NOW).unwrap();
    assert_eq!(first, AttrWriteOutcome { inserted: 6, identical: 0, superseded: 0, reopened: 0 });
    assert_eq!(count(m, "SELECT count(*) FROM attribute"), 6, "声明五格 + 设备标识一条");
    let again = put_attribute_rows(m, &anchor, &declared(), &scope(), &ctx(), LATER).unwrap();
    assert_eq!(again, AttrWriteOutcome { inserted: 0, identical: 6, superseded: 0, reopened: 0 });
    assert_eq!(count(m, "SELECT count(*) FROM attribute"), 6, "同值重导不长第二行");
    // 写侧盖章的四格：mode/type/source/device 都来自程序，不来自环境变量。
    let (mode, vtype, agent, device): (String, String, String, String) = m
        .conn()
        .query_row(
            "SELECT mode, value_type, source_agent, device_id FROM attribute WHERE key=?1",
            [ATTR_BILIBILI_ACCOUNT],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!((mode.as_str(), vtype.as_str()), ("stated", "string"));
    assert_eq!((agent.as_str(), device.as_str()), ("qoder", "local-qoder"));
}

#[test]
fn 值变了写新行并关旧行_旧行指认替代者() {
    let (_d, root) = seeded();
    let m = root.memory();
    let anchor = with_environment(m);
    put_attribute_rows(m, &anchor, &[(ATTR_BILIBILI_ACCOUNT, BILI.into())], &scope(), &ctx(), NOW)
        .unwrap();
    let changed = put_attribute_rows(
        m, &anchor, &[(ATTR_BILIBILI_ACCOUNT, "新昵称".into())], &scope(), &ctx(), LATER,
    )
    .unwrap();
    assert_eq!(changed, AttrWriteOutcome { inserted: 1, identical: 0, superseded: 1, reopened: 0 });
    let live = live_rows(m, &anchor, ATTR_BILIBILI_ACCOUNT);
    assert_eq!(live.len(), 1, "同一 (锚, 键) 只留一条活行");
    assert_eq!(live[0].1, "新昵称");
    let (closed_value, closed_to, closed_by): (String, Option<i64>, Option<String>) = m
        .conn()
        .query_row(
            "SELECT value_text, valid_to_ms, superseded_by FROM attribute WHERE value_text=?1",
            [BILI],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(closed_value, BILI, "旧行不删（历史仍在）");
    assert_eq!(closed_to, Some(LATER), "旧行关在本次批次钟上");
    assert_eq!(closed_by.as_deref(), Some(live[0].0.as_str()), "旧行指认替代者");
}

#[test]
fn 值变回旧值时重开同一格_不新增第三行() {
    let (_d, root) = seeded();
    let m = root.memory();
    let anchor = with_environment(m);
    for (value, at) in [(BILI, NOW), ("新昵称", LATER), (BILI, LATER + 1)] {
        put_attribute_rows(m, &anchor, &[(ATTR_BILIBILI_ACCOUNT, value.into())], &scope(), &ctx(), at)
            .unwrap();
    }
    assert_eq!(count(m, "SELECT count(*) FROM attribute"), 2, "同值复用同一格：只有 B 与 新昵称 两行");
    let live = live_rows(m, &anchor, ATTR_BILIBILI_ACCOUNT);
    assert_eq!(live.len(), 1, "回到旧值 ⇒ 恰好一条活行");
    assert_eq!(live[0].1, BILI);
    // 另一格（新昵称）此时是关着的，且指认 B。
    let (closed, by): (Option<i64>, Option<String>) = m
        .conn()
        .query_row(
            "SELECT valid_to_ms, superseded_by FROM attribute WHERE value_text='新昵称'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(closed.is_some(), "被替下去的那一行必须关着");
    assert_eq!(by.as_deref(), Some(live[0].0.as_str()));
}

#[test]
fn 写侧拒收不认识的键与空白值() {
    let (_d, root) = seeded();
    let m = root.memory();
    let anchor = with_environment(m);
    let bad_key = put_attribute_rows(m, &anchor, &[("nickname", "甲某随行".into())], &scope(), &ctx(), NOW);
    assert!(matches!(bad_key, Err(MemError::BadA1Field("key"))), "{bad_key:?}");
    let bad_val = put_attribute_rows(m, &anchor, &[(ATTR_HOSTNAME, "  ".into())], &scope(), &ctx(), NOW);
    assert!(matches!(bad_val, Err(MemError::BadA1Field("value_text"))), "{bad_val:?}");
    assert_eq!(count(m, "SELECT count(*) FROM attribute"), 0, "坏批一条都不写");
}

// ── ③ 解析：来源 + 角色 + 歧义全报 ───────────────────────────────────────

#[test]
fn 账号昵称解析到环境锚_来源与角色都在() {
    let (_d, root) = seeded();
    let m = root.memory();
    let anchor = with_environment(m);
    put_attribute_rows(m, &anchor, &declared(), &scope(), &ctx(), NOW).unwrap();
    let hits = resolve_surface(m, BILI).unwrap();
    assert_eq!(hits.len(), 1, "只有一条属性命中：{hits:?}");
    assert_eq!(hits[0].anchor_id, anchor);
    assert_eq!(hits[0].via, "attribute:bilibili_account");
    assert_eq!(hits[0].role, Some("B站账号昵称"));
    assert_eq!(entity_ids_for_name(m, BILI).unwrap(), vec![anchor.clone()], "entity= 过滤也认这条");
}

#[test]
fn 同一个值命中两个环境时两个锚都报出来() {
    let (_d, root) = seeded();
    let m = root.memory();
    let a = with_environment(m);
    // 第二台机器：另一个 scope，同一个品牌值。
    let other_scope = Scope { kind: ScopeKind::Project, key: "other-proj".into() };
    let b = ensure_environment_anchor(m, "另一台", &ctx(), &owner(), &other_scope, NOW).unwrap();
    assert_ne!(a, b);
    put_attribute_rows(m, &a, &[(ATTR_BRAND, "Acer".into())], &scope(), &ctx(), NOW).unwrap();
    put_attribute_rows(m, &b, &[(ATTR_BRAND, "Acer".into())], &other_scope, &ctx(), NOW).unwrap();
    let mut hits = resolve_surface(m, "Acer").unwrap();
    hits.sort_by(|x, y| x.anchor_id.cmp(&y.anchor_id));
    assert_eq!(hits.len(), 2, "歧义不许选一个：{hits:?}");
    assert!(hits.iter().all(|h| h.via == "attribute:brand" && h.role == Some("品牌")));
    let mut ids = entity_ids_for_name(m, "Acer").unwrap();
    ids.sort();
    let mut want = vec![a, b];
    want.sort();
    assert_eq!(ids, want);
}

/// 同一格锚既被 canonical 命中又被 alias 命中：**来源各自一行、角色都为空**，
/// 而 `entity_ids_for_name` 那一侧按**锚**去重 ⇒ 仍只回一格。
///
/// 去重的粒度是 `(锚, 来源)` 而不是只有锚：canonical 与 alias 说的不是一件事
/// （"它现在就叫这个" vs "它也叫过这个"），读侧要能分辨；要 id 集的调用方自己按锚收。
#[test]
fn 锚名与别名两行同锚_角色都为空_id_集仍一格() {
    let (_d, root) = seeded();
    let m = root.memory();
    with_environment(m);
    // 主人锚 canonical=示例主人，别名也是示例主人 ⇒ 两条来源同一个锚。
    let hits = resolve_surface(m, PRINCIPAL).unwrap();
    assert_eq!(hits.len(), 2, "{hits:?}");
    assert!(hits.iter().all(|h| h.anchor_id == hits[0].anchor_id), "两条来源同一个锚");
    assert_eq!(hits[0].via, "canonical", "canonical 是最高优先的那一档");
    assert_eq!(hits[1].via, "alias");
    assert!(hits.iter().all(|h| h.role.is_none()), "非属性命中没有角色名");
    assert_eq!(entity_ids_for_name(m, PRINCIPAL).unwrap().len(), 1, "id 集按锚去重");
}

#[test]
fn 属性行不存在时解析为空() {
    let (_d, root) = seeded();
    let m = root.memory();
    with_environment(m);
    assert!(resolve_surface(m, BILI).unwrap().is_empty());
    assert!(entity_ids_for_name(m, BILI).unwrap().is_empty());
    assert!(attribute_owners(m, BILI).unwrap().is_empty());
    assert!(resolve_surface(m, "   ").unwrap().is_empty(), "空白词不是任何一个名字");
}

// ── ④ 归属答句的原料：谁 + 哪个环境 + 什么角色 ───────────────────────────

#[test]
fn attribute_owners_答出主人与环境名() {
    let (_d, root) = seeded();
    let m = root.memory();
    let anchor = with_environment(m);
    put_attribute_rows(m, &anchor, &declared(), &scope(), &ctx(), NOW).unwrap();
    let owners = attribute_owners(m, BILI).unwrap();
    assert_eq!(owners.len(), 1);
    assert_eq!(owners[0].surface, BILI);
    assert_eq!(owners[0].role, "B站账号昵称");
    assert_eq!(owners[0].environment, HOST);
    assert_eq!(owners[0].owner_principal_id.as_deref(), Some(PRINCIPAL));
    assert_eq!(owners[0].owner_name.as_deref(), Some(PRINCIPAL), "显示名取主人锚的别名");
}

#[test]
fn 没有章的主人锚时归属名回落_principal_id() {
    let (_d, root) = seeded();
    let m = root.memory();
    // 只建环境锚与属性，**不建主人锚**：归属列仍在，显示名只能回落稳定键。
    let anchor = ensure_environment_anchor(m, HOST, &ctx(), &owner(), &scope(), NOW).unwrap();
    put_attribute_rows(m, &anchor, &[(ATTR_BILIBILI_ACCOUNT, BILI.into())], &scope(), &ctx(), NOW)
        .unwrap();
    let owners = attribute_owners(m, BILI).unwrap();
    assert_eq!(owners[0].owner_principal_id.as_deref(), Some(PRINCIPAL));
    assert_eq!(owners[0].owner_name.as_deref(), Some(PRINCIPAL), "没有主人锚 ≠ 没有主人");
}
