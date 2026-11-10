//! 派发档 I-3e · FIX-1：派生行**继承归属**，而不是放宽读闸。
//!
//! 断线（台账「Phase 2.5 现网验收」）的形状是：derive 写下的因果链卡 `source_agent='rc-derive'`
//! 而 `resource_owner` 零行 ⇒ `gate.rs` 的 `GateIndex::decide` 第 3 条只剩"行的署名恰等于读者"那一支 ⇒ 对真实读者
//! （现网是 `qoder`）一律 `Deny::NoOwnerRow`，句柄臂把它渲染成 `empty:row_absent`——**库里有这一行**。
//! 所以本文件的判据一律走**读者面**（`gate::lookup_visible` 就是 `rc_agent` 的句柄臂
//! `tools.rs` 的 `vote_with_viewer` 与 `recall.rs` 的 `gate_allows` 调的那一支），库里查得到一行只当**阳性对照**（"行在"），
//! 不当"读得到"。
//!
//! 七件事：
//! 1. 三族派生卡（因果链 / 时间线 / episode 镜像卡）的归属继承自**依据行**，读者是依据行的
//!    主人时读得到；
//! 2. 继承的是 `owner_agent_id`（归属），**不是** `source_agent`（署名）——夹具故意让两者不同，
//!    拿署名当归属的那一种错法当场红；
//! 3. 反例一：依据行属于别的 agent ⇒ 派生行对本站读者仍然不可见；
//! 4. 反例二：两条依据分属不同 agent ⇒ 一行归属都不写（fail-closed），两个主人都读不到；
//! 5. `visibility` 取依据行中**最受限**的那一档；
//! 6. 再过一次 scope 天花板（`owner::effective`）：派生行落在 entity scope 时，依据行的
//!    `project` 会被压成 `person`；
//! 7. `fact_link`（边）**没有归属面**：本族一行归属都不写，边的可见性照旧跟随两端。
//!
//! 夹具纪律：种子行一律走真写入路径（`put_batch_with_owner` / `put_fact_rows_with_extras`），
//! 归属行因此是生产形状的归属行，不是手搓的一行字节。
use rc_memory::gate::{lookup_visible, Viewer};
use rc_memory::keys::{AnchorRole, MemKey, MemTable, Scope, ScopeKind};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::reader;
use rc_memory::writers_x::{
    put_batch_with_owner, put_chains_merged, put_edges_anchored, put_entity, put_episodes,
    put_timeline_rows_counted, BatchCtx, ChainContent, EdgeContent, EntityContent, EpisodeContent,
    TimelineContent, WriteItem,
};
use rc_memory::{
    compute_fact_id, compute_mem_id, put_fact_rows_with_extras, FactExtras, FactWriteItem,
    Provenance,
};
use rc_store::{MemoryHandle, Root};

/// 执行钟。派生行的 `created_at_ms` 由源侧时刻来，这一格只当读闸的 `now_ms`。
const NOW: i64 = 1_700_000_000_000;
/// 依据行的主人（= 现网的 `qoder` 那一格）。
const OWNER_A: &str = "owner-甲";
/// 另一个主人（两条反例的靶）。
const OWNER_B: &str = "owner-乙";
/// 派生行的署名——**故意与依据行的主人不同名**，钉住"继承的是归属不是署名"。
const DERIVE: &str = "rc-derive";
/// 依据行自己的生产者。归属（`owner-甲`）与署名（这一格）在夹具里是三串不同的字。
const EXTRACT: &str = "extract-agent";

fn scope() -> Scope { Scope { kind: ScopeKind::Project, key: "proj-derived".into() } }
fn entity_scope() -> Scope { Scope { kind: ScopeKind::Entity, key: "ent-derived".into() } }
fn derive_ctx() -> BatchCtx { BatchCtx { source_agent: DERIVE.into(), device_id: "unknown".into() } }
fn extract_ctx() -> BatchCtx { BatchCtx { source_agent: EXTRACT.into(), device_id: "dev-extract".into() } }

fn owner_of(agent: &str, visibility: Visibility) -> OwnerCtx {
    OwnerCtx { owner_principal_id: None, owner_agent_id: agent.into(), visibility,
               retention: "default".into() }
}

fn prov(c: char) -> Vec<Provenance> {
    vec![Provenance { turn_uid: c.to_string().repeat(64), anchor_role: AnchorRole::Human }]
}

fn item(title: &str, body: &str) -> WriteItem {
    WriteItem {
        table: MemTable::Card, kind: "decision".into(), title: title.into(), body: body.into(),
        scope: scope(), valid_to_ms: None, aliases: vec![], prov: prov('a'),
    }
}

/// 一张有归属行的种子卡：归属 = `agent`（可见性 = `visibility`），署名 = `EXTRACT`。
fn owned_card(m: &MemoryHandle, title: &str, body: &str, agent: &str,
              visibility: Visibility) -> String {
    let keys = put_batch_with_owner(m, &[item(title, body)], &extract_ctx(),
                                    &owner_of(agent, visibility), NOW, &[]).unwrap();
    keys.into_iter().next().flatten().expect("种子卡不被抑制").id
}

/// 默认的阳性种子：两张同主人、同 `private_agent` 的卡。
fn two_cards(m: &MemoryHandle) -> [String; 2] {
    [owned_card(m, "先立的口径", "这一行说容量按并发数算。", OWNER_A, Visibility::PrivateAgent),
     owned_card(m, "后来的改动", "这一行说那条阈值被换掉了。", OWNER_A, Visibility::PrivateAgent)]
}

/// 真写入路径落一条链（机制句不是任何一行正文的子串 ⇒ 不会被 I5 的复述降权挡下）。
fn write_chain(m: &MemoryHandle, links: &[String], bases: &[String]) -> (usize, usize) {
    put_chains_merged(m, &[ChainContent {
        mechanism: "前一次改动把旧的容量口径顶掉了，于是回退不再按那条阈值触发".into(),
        link_ids: links.to_vec(), basis_ids: bases.to_vec(),
        causal_type: "causes".into(), scope: scope(),
    }], &derive_ctx(), NOW).unwrap()
}

/// 库里那张唯一的因果链头行（阳性对照：行**在**，读得到与否是另一问）。
fn only_causal_head(m: &MemoryHandle) -> String {
    let mut st = m.conn().prepare("SELECT id FROM card WHERE kind='causal' ORDER BY id").unwrap();
    let got: Vec<String> = st.query_map([], |r| r.get::<_, String>(0)).unwrap()
        .map(|x| x.unwrap()).collect();
    assert_eq!(got.len(), 1, "夹具应当恰好落一条链头行：{got:?}");
    got.into_iter().next().unwrap()
}

fn only_timeline_card(m: &MemoryHandle) -> String {
    let mut st = m.conn().prepare("SELECT id FROM card WHERE kind='timeline' ORDER BY id").unwrap();
    let got: Vec<String> = st.query_map([], |r| r.get::<_, String>(0)).unwrap()
        .map(|x| x.unwrap()).collect();
    assert_eq!(got.len(), 1, "夹具应当恰好落一行时间线：{got:?}");
    got.into_iter().next().unwrap()
}

fn only_episode_card(m: &MemoryHandle) -> String {
    let mut st = m.conn()
        .prepare("SELECT id FROM card WHERE kind='episode' ORDER BY id").unwrap();
    let got: Vec<String> = st.query_map([], |r| r.get::<_, String>(0)).unwrap()
        .map(|x| x.unwrap()).collect();
    assert_eq!(got.len(), 1, "夹具应当恰好落一张镜像卡：{got:?}");
    got.into_iter().next().unwrap()
}

/// 某个句柄的归属行（`(主人, 产出者, 可见性)`）；没有归属行 ⇒ `None`。
/// 这一格是**读回**用的对照，不是判据：判据走 [`lookup_visible`]（读者面）。
fn owner_row(m: &MemoryHandle, table: &str, id: &str) -> Option<(Option<String>, String, String, String)> {
    m.conn()
        .query_row(
            "SELECT owner_principal_id, owner_agent_id, created_by_agent, visibility
               FROM resource_owner WHERE mem_table=?1 AND mem_id=?2",
            rusqlite::params![table, id],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?, r.get::<_, String>(3)?)),
        )
        .ok()
}

fn owner_rows(m: &MemoryHandle, table: &str, id: &str) -> i64 {
    m.conn().query_row(
        "SELECT count(*) FROM resource_owner WHERE mem_table=?1 AND mem_id=?2",
        rusqlite::params![table, id], |r| r.get(0)).unwrap()
}

/// 任意一条 `SELECT count(*) …` 的读数（本文件的"非空集判据"用它，别让手搓 SQL 散在用例里）。
fn count(m: &MemoryHandle, sql: &str) -> i64 {
    m.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

/// 归属面（`resource_owner`）的**总行数**。"这一族一行归属都不写"只有在这种读法上才有牙：
/// 按 `mem_table='fact_link'` 单问，任何"写到别的表名底下"的错法都拦不住。
fn owner_row_count(m: &MemoryHandle) -> i64 {
    count(m, "SELECT count(*) FROM resource_owner")
}

/// **读者面**：这一行对 `agent` 是否读得到。`lookup_visible` 是 `rc_agent` 句柄臂调的那一支
/// （`tools.rs` 的 `vote_with_viewer` / `recall.rs` 的 `gate_allows`），被拒与查不到在这里同形（`None`）。
fn readable(m: &MemoryHandle, agent: &str, id: &str) -> bool {
    let key = MemKey { table: MemTable::Card, id: id.to_string() };
    lookup_visible(m, Some(&Viewer::agent(agent)), NOW, &key).unwrap().is_some()
}

/// 阳性对照：行**在库里**（`reader::lookup` 是读者自己取行事实的那一支，不过闸）。
fn row_is_in_store(m: &MemoryHandle, id: &str) -> bool {
    let key = MemKey { table: MemTable::Card, id: id.to_string() };
    reader::lookup(m, &key).unwrap().is_some()
}

// ── 1/2. 三族派生卡继承归属 ─────────────────────────────────────────────────────

#[test]
fn 因果链卡的归属继承自依据行_依据行的主人读得到() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let [a, b] = two_cards(m);
    assert_eq!(write_chain(m, &[a.clone(), b.clone()], std::slice::from_ref(&a)), (1, 0), "前置：链真落库");
    let head = only_causal_head(m);

    // 阳性对照：库里有这一行。它单独不构成"读得到"——断线正是这个形状（行在、读者读不到）。
    assert!(row_is_in_store(m, &head), "链头行必须真的在库里（否则下面的断言是在空集上成立）");

    // FIX-1 的**正向**判据（派发档 I-3e2 把这一格从"修前形状"翻过来）：归属行现在被写着，
    // 而且写的是从依据行**继承**来的那一行。修前这一格断的是 `count == 0`
    // （"派生族不写归属行"＝断线的根因），它在修后必然转红 ⇒ 那条前置断言本身没有判别力，
    // 现在它换成四条有判别力的读数：行数、主人、产出者、可见性取值。
    let o = owner_row(m, "card", &head).expect("派生卡必须有一行继承来的归属行（FIX-1 的落点）");
    assert_eq!(owner_rows(m, "card", &head), 1, "一次写入只许留下一行归属");
    assert_eq!(o.1, OWNER_A, "继承的是依据行的**归属**，不是署名（署名是 {DERIVE}）");
    assert_eq!(o.2, DERIVE, "`created_by_agent` 是产出者，取 `BatchCtx` 的署名（照 writers_fact 的写法）");
    assert_eq!(o.0, None, "依据行没有登记自然人 ⇒ 不发明一个 principal");
    // 可见性那两格在这里也一起钉住：两条依据行同为 `private_agent` ⇒ 最受限的一档就是它；
    // 链头落在 project scope，天花板是 `project`，`owner::effective` 只压不抬 ⇒ 取值不变。
    // 取最受限与天花板**各自会红**的形状见后面两条（两档取低、entity 把 project 压回 person）。
    assert_eq!(o.3, "private_agent",
        "visibility = 依据行中最受限的一档，再过派生行自己 scope 的天花板（`owner::effective`）");

    // 判据：依据行的主人现在读得到。
    assert!(readable(m, OWNER_A, &head),
        "派生卡对依据行的主人不可见 ⇒ FIX-1 没落地（修前红在这里：`row_absent` 的形状）");

    // 同一条判据的**反装**：这一格是"修前库里就在的那一形"——行在、归属行不在。
    // 把刚继承来的归属行删掉，同一位读者对同一张卡**重新读不到** ⇒ 放行它的是那行继承来的
    // 归属证据，不是被放宽的读闸（`gate.rs`/`reader.rs` 本轮零改动，这一句就是它的可观测面）。
    m.conn().execute(
        "DELETE FROM resource_owner WHERE mem_table='card' AND mem_id=?1", [&head],
    ).unwrap();
    assert!(!readable(m, OWNER_A, &head),
        "没有归属行就读不到 ⇒ 断线的根因确实是归属行缺失，闸本身没有被改宽");
}

#[test]
fn 时间线卡的归属继承的被锚行() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let anchor = owned_card(m, "季度会", "会上定了发布排在季度会之后。", OWNER_A, Visibility::PrivateAgent);
    put_timeline_rows_counted(m, &[TimelineContent {
        title: "derive-发布在季度会之后".into(), body: "发布排在季度会之后。".into(),
        scope: scope(), time_anchor_ms: Some(NOW + 1), time_anchor_id: Some(anchor.clone()),
        time_confidence: "anchored".into(), basis_ids: vec![],
    }], &derive_ctx(), NOW).unwrap();
    let tl = only_timeline_card(m);
    assert!(row_is_in_store(m, &tl), "时间线行必须在库里");
    assert!(readable(m, OWNER_A, &tl), "时间线卡的归属必须继承自被锚行");
    assert_eq!(owner_row(m, "card", &tl).map(|o| o.1).as_deref(), Some(OWNER_A));
}

#[test]
fn episode_镜像卡的归属继承自成员事实行() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let ids = [member_fact(m, 0, "调通了闸"), member_fact(m, 1, "补了桩")];
    put_episodes(m, &[EpisodeContent {
        title: "把闸接通".into(), narrative: "先调通闸，再补桩。".into(), fact_ids: ids.to_vec(),
    }], &derive_ctx(), NOW).unwrap();
    let card_id = only_episode_card(m);
    assert!(row_is_in_store(m, &card_id), "镜像卡必须在库里");
    assert!(readable(m, OWNER_A, &card_id),
        "镜像卡的归属必须继承自成员 fact 行（I-3d 交回里点到的是同一格）");
    assert_eq!(owner_row(m, "card", &card_id).map(|o| o.1).as_deref(), Some(OWNER_A));
}

/// 一条真成员 fact（走真生产者 ⇒ 自带出处与归属行），归属 = `OWNER_A`。
fn member_fact(m: &MemoryHandle, index: usize, value: &str) -> String {
    member_fact_owned(m, index, value, OWNER_A)
}

/// 同 [`member_fact`]，但把**归属**交出来选（反例用例要的是"成员属于别人"那一形）。
/// 署名仍是 `EXTRACT`、走同一条真写入路径，所以这一形与正向用例只差主人一格。
fn member_fact_owned(m: &MemoryHandle, index: usize, value: &str, agent: &str) -> String {
    // 主体锚的句柄按生产算法算：kind/canonical/scope 就是下面那行 INSERT 写进去的三格
    // （`person` / `甲某{index}` / `project/proj-derived` = [`scope()`]）。
    let canonical = format!("甲某{index}");
    let anchor = compute_mem_id(MemTable::Anchor, "person", &canonical, "", &scope());
    m.conn().execute(
        "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                            status, created_at_ms, updated_at_ms, norm_version)
         VALUES(?1, 'person', ?2, 'real', 'project', 'proj-derived', 'active', ?3, ?3, 1)",
        rusqlite::params![anchor, canonical, NOW],
    ).unwrap();
    let text = format!("甲某{value}");
    let item = FactWriteItem {
        subject_anchor_id: anchor.clone(), object_anchor_id: None, predicate: "做了".into(),
        value: Some(value.into()), category: "event".into(), mode: "stated".into(),
        time_text: String::new(), quote: value.into(),
        evidence: vec![(format!("{:064x}", 0x31 + index), AnchorRole::Human)],
        text: text.clone(), scope: scope(), source_ms: NOW + index as i64,
    };
    let id = compute_fact_id(&anchor, "做了", None, Some(value), "event", "", &text, &scope());
    let out = put_fact_rows_with_extras(m, &[item], &[FactExtras::default()],
                                        &extract_ctx(), &owner_of(agent, Visibility::PrivateAgent),
                                        NOW).unwrap();
    assert_eq!(out.facts, 1, "夹具的成员必须真的落库：{value}");
    // 成员行的归属是这一族继承的**唯一**来源；它不在，下面那句断言就是在空集上成立。
    assert_eq!(owner_rows(m, "fact", &id), 1, "成员 fact 行必须自带归属行");
    assert_eq!(owner_row(m, "fact", &id).map(|o| o.1).as_deref(), Some(agent),
        "成员行的归属就是夹具交出去的那一位（反例的那一半靠它才是'属于别人'）");
    id
}

// ── 3/4. 两条反例（FIX-1 不是"放行"） ────────────────────────────────────────────

#[test]
fn 反例_依据行属于别的agent时派生行对本站读者不可见() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    // 依据行属于 OWNER_B，署名仍是 DERIVE ⇒ 继承过来的归属也是 B。
    let a = owned_card(m, "先立的口径", "这一行说容量按并发数算。", OWNER_B, Visibility::PrivateAgent);
    let b = owned_card(m, "后来的改动", "这一行说那条阈值被换掉了。", OWNER_B, Visibility::PrivateAgent);
    write_chain(m, &[a.clone(), b.clone()], std::slice::from_ref(&a));
    let head = only_causal_head(m);
    assert!(row_is_in_store(m, &head), "阳性对照：行在库里");
    assert!(!readable(m, OWNER_A, &head),
        "继承归属不是放宽闸：依据行属于 B ⇒ A 读不到");
    assert!(readable(m, OWNER_B, &head),
        "阳性对照：B 读得到 ⇒ 拦住 A 的是归属，不是闸被改坏了");
}

/// 派发档 I-3e2 要点 1 点名要补的那一条**反例**，换一族再验一遍：episode 镜像卡的依据行
/// 是成员 fact 行，成员属于别的 agent ⇒ 继承过来的归属也是那一位，本站读者（`OWNER_A`）
/// 照旧读不到。四条读数缺一不算验过：卡行在库里（阳性对照）、归属行确实是 B、
/// A 读不到、B 读得到（拦住 A 的那一格是归属，不是闸）。
#[test]
fn 反例_episode_成员属别的agent时镜像卡对本站读者仍不可见() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let ids = [member_fact_owned(m, 0, "调通了闸", OWNER_B),
               member_fact_owned(m, 1, "补了桩", OWNER_B)];
    let out = put_episodes(m, &[EpisodeContent {
        title: "把闸接通".into(), narrative: "先调通闸，再补桩。".into(), fact_ids: ids.to_vec(),
    }], &derive_ctx(), NOW).unwrap();
    assert_eq!((out.episodes, out.cards, out.owner_ambiguous), (1, 1, 0),
        "两条成员同属 B ⇒ 归属不歧义，镜像卡照落并写上继承来的归属");
    let card_id = only_episode_card(m);
    assert!(row_is_in_store(m, &card_id), "阳性对照：镜像卡在库里（行在 ≠ 读得到）");
    assert_eq!(owner_row(m, "card", &card_id).map(|o| o.1).as_deref(), Some(OWNER_B),
        "继承来的是成员那一行的主人 B，不是署名 rc-derive");
    assert!(!readable(m, OWNER_A, &card_id),
        "继承归属不是放宽闸：成员属于 B ⇒ A 读不到这张镜像卡");
    assert!(readable(m, OWNER_B, &card_id),
        "阳性对照：B 读得到 ⇒ 拦住 A 的那一格是归属，闸没有被动过");
}

#[test]
fn 反例_两条依据分属不同agent时一行归属都不写() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let a = owned_card(m, "先立的口径", "这一行说容量按并发数算。", OWNER_A, Visibility::PrivateAgent);
    let b = owned_card(m, "后来的改动", "这一行说那条阈值被换掉了。", OWNER_B, Visibility::PrivateAgent);
    assert_eq!(write_chain(m, &[a.clone(), b.clone()], std::slice::from_ref(&a)), (1, 0), "链照落");
    let head = only_causal_head(m);
    assert!(row_is_in_store(m, &head), "行照落：归属歧义不拦数据");
    assert_eq!(owner_rows(m, "card", &head), 0,
        "归属不唯一 ⇒ 不写归属行（fail-closed，控制器裁定的第二条）");
    assert!(!readable(m, OWNER_A, &head) && !readable(m, OWNER_B, &head),
        "没有归属行时两个主人都读不到：修的是断线，不是把闸打开");
}

// ── 5/6. visibility 的两道：取最受限 + scope 天花板 ──────────────────────────────

#[test]
fn 继承的可见性取依据行中最受限的那一档() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    // 同主人、两档可见性：`project` 与 `person` ⇒ 取 `person`。
    let a = owned_card(m, "先立的口径", "这一行说容量按并发数算。", OWNER_A, Visibility::Project);
    let b = owned_card(m, "后来的改动", "这一行说那条阈值被换掉了。", OWNER_A, Visibility::Person);
    write_chain(m, &[a.clone(), b.clone()], std::slice::from_ref(&a));
    let head = only_causal_head(m);
    assert_eq!(owner_row(m, "card", &head).map(|o| o.3), Some("person".to_string()),
        "派生卡不许比它的依据行更宽");
}

#[test]
fn 继承的可见性再过一次scope天花板_entity行压回person() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let a = owned_card(m, "先立的口径", "这一行说容量按并发数算。", OWNER_A, Visibility::Project);
    let b = owned_card(m, "后来的改动", "这一行说那条阈值被换掉了。", OWNER_A, Visibility::Project);
    // 链头落在 entity scope ⇒ `owner::ceiling(Entity) = Person` 把 `project` 压回 `person`。
    put_chains_merged(m, &[ChainContent {
        mechanism: "前一次改动把旧的容量口径顶掉了，于是回退不再按那条阈值触发".into(),
        link_ids: vec![a.clone(), b.clone()], basis_ids: vec![a.clone()],
        causal_type: "causes".into(), scope: entity_scope(),
    }], &derive_ctx(), NOW).unwrap();
    let head = only_causal_head(m);
    assert_eq!(owner_row(m, "card", &head).map(|o| o.3), Some("person".to_string()),
        "必须照 `writers_fact.rs` 的 `put_fact_rows_with_extras` 的既有做法再过一遍 `owner::effective`");
}

// ── 7. 边没有归属面 ─────────────────────────────────────────────────────────────

#[test]
fn fact_link_不是可指认句柄_本族一行归属都不写() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    // **I4 的锚门槛按真能过的那一形搭**（派发档 I-3e2 红②的处置）。判据②"逐字同实体"要的是
    // 库里**有一个实体行**、它的标题/别名归一后同时出现在两端的"标题+正文"里；上一版夹具只有
    // 两张卡、一个实体行都没有 ⇒ `entity_anchor_index` 是空表，两道判据全落空，
    // 边一条不落（实测 `(written,dropped) = (0,1)`），后面那句"本族不写归属"因此是在空集上读的。
    // 这里补的就是那个实体行，而且走真生产者（`put_entity` → `write_fact`），不是手搓 INSERT。
    // 判据①（共享 mention）今天对 card 端点在真库里恒空（`memory_mention` 只有 fact/derive/
    // state_assertion 的行，没有 card 行），所以本条验的是**判据②那一层**；
    // `put_edges`（不带锚门槛的旧入口）在这里不作对照，它的门槛缺失是另一格已登记的债。
    put_entity(m, &EntityContent {
        kind: "concept".into(), title: "Postgres".into(), scope: scope(), aliases: vec![],
    }, &prov('b'), NOW).unwrap();
    let from = owned_card(m, "ship behind a flag", "Postgres stays until the migration ends.",
                          OWNER_A, Visibility::PrivateAgent);
    let to = owned_card(m, "flagged rollout", "Same plan reworded, Postgres first.",
                        OWNER_A, Visibility::PrivateAgent);
    let owners_before = owner_row_count(m);
    let (written, dropped) = put_edges_anchored(m, &[EdgeContent {
        from_id: from.clone(), to_id: to.clone(), kind: "depends_on".into(),
        reason: "两份说法是同一个计划".into(), scope: scope(), basis_ids: vec![from.clone()],
    }], &derive_ctx(), NOW).unwrap();
    assert_eq!((written, dropped), (1, 0),
        "前置：边必须真落库（I4 判据②的逐字文本锚已在），否则下面的读数在空集上成立");
    assert_eq!(count(m, "SELECT count(*) FROM fact_link"), 1, "阳性对照：库里确实有这条边");

    // 判据（三半，都读在**非空集**上）：边不是可指认句柄（`keys.rs` 顶上的 §6 句柄形状文档、
    // `hub/src/ownership.rs` 的行集是 card+anchor+fact+state_assertion）⇒ 它没有归属面。
    // 只读 `mem_table='fact_link'` 那一问拦不住"往别的表名底下塞一行归属"这种错法，
    // 所以第二半问"归属面的**总行数**这一次一格都没多长"，第三半按边的 id 反查。
    assert_eq!(count(m, "SELECT count(*) FROM resource_owner WHERE mem_table='fact_link'"), 0,
        "`fact_link` 不在归属面的行集里");
    assert_eq!(owner_row_count(m), owners_before,
        "边这一族写归属面零行：写入前后归属面的总行数必须一格都没变");
    assert_eq!(count(m, "SELECT count(*) FROM resource_owner WHERE mem_id IN \
                         (SELECT id FROM fact_link)"), 0, "按边的句柄反查也一行都不许有");

    // 边的可见性照旧**跟随两端**（`export.rs` 的 `render_export`）：两端都是 A 的行 ⇒ 对 A 可见；
    // 把 from 端的归属改成 B ⇒ 同一位读者就看不见这条边了。
    let e = reader::edges(m).unwrap().into_iter().next().expect("读侧看得见这条边");
    let g = rc_memory::gate::GateIndex::load(m, &Viewer::agent(OWNER_A), NOW).unwrap();
    assert!(g.allows(&e.from_kind, &e.from_id) && g.allows(&e.to_kind, &e.to_id),
        "两端都归 A ⇒ 这条边的两端对 A 都放行（非空集上的读数）");
    m.conn().execute("UPDATE resource_owner SET owner_agent_id=?2 WHERE mem_table='card' AND mem_id=?1",
                     rusqlite::params![from, OWNER_B]).unwrap();
    let g = rc_memory::gate::GateIndex::load(m, &Viewer::agent(OWNER_A), NOW).unwrap();
    assert!(!g.allows(&e.from_kind, &e.from_id),
        "一端换了归属 ⇒ 那位读者看不见这一端 ⇒ 这条边在导出面自然不出现");
    assert!(g.allows(&e.to_kind, &e.to_id), "另一端照旧可见：换的是 from 那一端");
}

// ── 8. 重放幂等：归属行不许多长第二行 ───────────────────────────────────────────

#[test]
fn 同一批重跑归属行不增不改() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let [a, b] = two_cards(m);
    assert_eq!(write_chain(m, &[a.clone(), b.clone()], std::slice::from_ref(&a)), (1, 0));
    let head = only_causal_head(m);
    let first = owner_row(m, "card", &head).expect("第一次落库要写上归属行");
    // 重放：头行 `INSERT OR IGNORE` 命中已有 ⇒ 链数报 0，归属行既不多一行也不许被改写。
    let again = put_chains_merged(m, &[ChainContent {
        mechanism: "前一次改动把旧的容量口径顶掉了，于是回退不再按那条阈值触发".into(),
        link_ids: vec![a.clone(), b.clone()], basis_ids: vec![a.clone()],
        causal_type: "causes".into(), scope: scope(),
    }], &derive_ctx(), NOW).unwrap();
    assert_eq!(again, (0, 1), "同一 causal_type + 同一顺序环节 ⇒ 第二遍按既有判据计合并");
    assert_eq!(owner_rows(m, "card", &head), 1, "重放不许长第二行归属（`owner::insert` 是 INSERT OR IGNORE）");
    assert_eq!(owner_row(m, "card", &head).expect("归属行还在"), first,
        "先写者赢：重放不许覆盖既有归属行");
    assert!(readable(m, OWNER_A, &head));
}
