//! 派发档 I-3a · episode 存储层（写侧 + 读侧 + 句柄）。
//!
//! 这一层钉八件事（母档 §0 裁定 + §2 落点 + 派发档 I-3a 交付清单 + I-3d 两条必修 + I-3e 两条必修）：
//! 1. **句柄**：`episode.id` = sha256(scope + **有序成员 fact id**)（D3）⇒ 哈希含序，成员换个序
//!    就是另一个 preimage；而 I-3e · FIX-2 改判了**谁给那个序**：它是写入口**准入之后**按
//!    [`rc_memory::writers_x::event_order`] 排好的那一批 fact id，不是调用方递进来的那一串
//!    ⇒ 成员少一条就是另一个句柄（`句柄取准入后按事件序的成员_少一个成员就换一个句柄`），
//!    调用方换交入序不再造第二段经历；镜像卡句柄与它同源（同一组成员必配成一对）
//!    且不碰撞（两侧 preimage 差一段侧标）；
//! 2. **一次写入落四族行**：`episode` + `episode_fact` + 镜像 `card(kind='episode')` +
//!    每成员一行 `ref_kind='fact', role='basis'` 出处，全在**一个事务**里；
//! 3. **幂等**：同一批重跑计数全 0、行数不变（内容寻址 id + `INSERT OR IGNORE`）；
//! 4. **时间那一格**（D4 + D4-补）：`episode.start_ms/end_ms` 只从成员 `fact.occurred_at_ms`/
//!    `occurred_end_ms` 取；镜像卡 `as_of_ms` 取序 `start_ms`（⇒ `'anchored'`）→ 最早成员的
//!    `fact.created_at_ms`（⇒ `'speech'`，本轮新增的一支）；**执行钟 `now_ms` 与导入钟一个字都不
//!    进库**；成员全未知时 `start/end` 保持 NULL；
//! 5. **成员准入**：无出处（既无 `ref_kind='turn'` 行也无 `evidence_turn_uid`）的 fact 不进、
//!    **已经判死的 fact 也不进**（成员那一行自己 + 它那张镜像卡的死活，I-3d MF-2）、
//!    不跨 scope（跨了整批报错、零行）；
//! 6. **镜像卡结构性进不了审核游标**：它一条 `ref_kind='turn'` 出处都没有（母档 §2.5 那道拦点），
//!    这一条**真断**，不靠注释；
//! 7. **superseded 那一形**：成员集变化 ⇒ 新 episode + 新卡，旧 episode 与旧卡被 `superseded_by`
//!    指过去（前驱按"共享成员"认，不靠重算旧句柄）。
//!
//! 夹具用真生产者 `put_fact_rows_with_extras` 落 fact 行（这样"成员 100% 有出处"是真的而不是手搓的）；
//! 事件时间那一格按 `rc-distill::pass_time` 的同一形状 UPDATE 补——本 crate 不许反向依赖解析器，
//! 而 `FACT_INSERT` 把七列时间全写 NULL 正是"等回填"的形状。
//! 正反例只用通用占位名，不含任何语料专名或 holdout。
use rc_memory::identity::{compute_episode_card_id, compute_episode_id, compute_fact_id};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::reader::{self, FactRow};
use rc_memory::writers_x::{put_episodes, EpisodeContent, EpisodeWriteOutcome};
use rc_memory::{
    put_fact_rows_with_extras, retire, AnchorRole, BatchCtx, FactExtras, FactWriteItem, MemError,
    MemKey, MemTable, Scope, ScopeKind,
};
use rc_store::{MemoryHandle, Root};

/// 执行钟：本层**不许**用它写任何一格。它故意与源时刻差得很远，落进去了就红。
const NOW: i64 = 1_700_000_999_000;
/// 发话钟基准（`fact.created_at_ms` 由它来，I3 之后它就是源轮时刻）。
const SRC0: i64 = 1_700_000_000_000;
const MIN: i64 = 60_000;

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-episode".into() }
}

fn ctx() -> BatchCtx {
    BatchCtx { source_agent: "agent-episode-w".into(), device_id: "dev-episode-1".into() }
}

fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-episode".into()),
        owner_agent_id: "agent-owner".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}

/// 锚行的 canonical：`seed_anchor` 的 INSERT 与句柄派生共用这一串。
const NAMES: [&str; 4] = ["甲某", "乙某", "丙某", "丁某"];

fn anchor(index: usize) -> String {
    rc_memory::compute_mem_id(MemTable::Anchor, "person", NAMES[index], "", &scope())
}

fn turn(index: usize) -> String {
    format!("{:064x}", 0x31 + index)
}

fn seed_anchor(m: &MemoryHandle, id: &str, canonical: &str, scope_key: &str) {
    m.conn().execute(
        "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                            status, created_at_ms, updated_at_ms, norm_version)
         VALUES(?1, 'person', ?2, 'real', 'project', ?3, 'active', ?4, ?4, 1)",
        rusqlite::params![id, canonical, scope_key, SRC0],
    ).unwrap();
}

fn seeded() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    for (index, name) in NAMES.iter().enumerate() {
        seed_anchor(root.memory(), &anchor(index), name, "proj-episode");
    }
    (dir, root)
}

/// 一条真 fact 行（走真生产者 ⇒ 它自带 `ref_kind='turn'` 出处、参与者与镜像卡）。
/// 返回它的 id（内容寻址，夹具自己算一遍，顺带把"句柄是算出来的"这一格也钉住）。
/// `extra` 是主语以外的参与者锚下标（`writers_fact` 会把 subject 自动并进去）。
fn fact(m: &MemoryHandle, index: usize, value: &str, speech_ms: i64, extra: &[usize]) -> String {
    fact_and_card(m, index, value, speech_ms, extra).0
}

/// 同 [`fact`]，另外把那条 fact 的**镜像消费卡** id 一起交回来（死活夹具要消音的就是这张卡）。
/// 卡 id 从真生产者的账本里拿（`FactWriteOutcome::card_ids`），**不**在夹具里重算一遍哈希：
/// 重算就是拿夹具自己担保"这张卡确实指回这条 fact"，而要证的恰恰是生产者那一条。
fn fact_and_card(m: &MemoryHandle, index: usize, value: &str, speech_ms: i64, extra: &[usize])
    -> (String, String) {
    let text = format!("甲某{value}");
    let item = FactWriteItem {
        subject_anchor_id: anchor(0),
        object_anchor_id: None,
        predicate: "做了".into(),
        value: Some(value.into()),
        category: "event".into(),
        mode: "stated".into(),
        time_text: String::new(),
        quote: value.into(),
        evidence: vec![(turn(index), AnchorRole::Human)],
        text: text.clone(),
        scope: scope(),
        source_ms: speech_ms,
    };
    let id = compute_fact_id(&anchor(0), "做了", None, Some(value), "event", "", &text, &scope());
    let extras = FactExtras {
        participants: extra.iter().map(|i| anchor(*i)).collect(),
        ..Default::default()
    };
    let out = put_fact_rows_with_extras(m, &[item], &[extras], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(out.facts, 1, "夹具的 fact 必须真的落库：{value}");
    assert_eq!(out.card_ids.len(), 1, "夹具的 fact 必须自带一张镜像卡：{value}");
    (id, out.card_ids[0].id.clone())
}

/// 事件时间回填（`rc-distill::pass_time` 的同一形状）。
fn set_occurred(m: &MemoryHandle, id: &str, start: Option<i64>, end: Option<i64>) {
    m.conn().execute(
        "UPDATE fact SET occurred_at_ms=?1, occurred_end_ms=?2 WHERE id=?3",
        rusqlite::params![start, end, id],
    ).unwrap();
}

/// 一形**没有生产者**的 fact：既无 `ref_kind='turn'` 出处行、`evidence_turn_uid` 又是空串。
/// 它正是"成员准入"要拦的那一形（抽取入口必带证据，真库里今天不会有它），
/// 于是夹具只能手搭——手搭的这条也不该被归组层收进成员表。
fn unproven_fact(m: &MemoryHandle, value: &str, speech_ms: i64) -> String {
    let id = format!("{:064x}", 0xe0 + value.chars().count());
    m.conn().execute(
        "INSERT INTO fact(id, subject_anchor_id, predicate, category, participants_json,
                          mode, confidence, text, evidence_turn_uid, scope_kind, scope_key,
                          source_agent, device_id, created_at_ms, norm_version)
         VALUES(?1,?2,'做了','event','[]','stated',0.8,?3,'','project','proj-episode',
                'agent-episode-w','dev-episode-1',?4,1)",
        rusqlite::params![id, anchor(0), format!("甲某{value}"), speech_ms],
    ).unwrap();
    id
}

fn episode(title: &str, narrative: &str, fact_ids: Vec<String>) -> EpisodeContent {
    EpisodeContent { title: title.into(), narrative: narrative.into(), fact_ids }
}

/// 副本真跑（T-6）从库里读出的成员行形状。字段名比七元组好读，也过得了 clippy 的
/// `type_complexity`（那条告警是门禁：`cargo clippy --workspace --all-targets` 要零告警）。
struct LiveMember {
    id: String,
    text: String,
    source_agent: String,
    device_id: String,
    scope: Scope,
    occurred_at_ms: Option<i64>,
    created_at_ms: i64,
}

// ── 读数面（行不在 ⇒ 直接炸：夹具的每条断言都建在"行确实落了"之上，
//    不许把"没行"读成"值是 NULL"）───────────────────────────────────────────────

fn cell_text(m: &MemoryHandle, table: &str, id: &str, column: &str) -> String {
    m.conn().query_row(&format!("SELECT {column} FROM {table} WHERE id=?1"), [id],
                       |r| r.get::<_, String>(0)).unwrap()
}

fn cell_num(m: &MemoryHandle, table: &str, id: &str, column: &str) -> i64 {
    m.conn().query_row(&format!("SELECT {column} FROM {table} WHERE id=?1"), [id],
                       |r| r.get::<_, i64>(0)).unwrap()
}

fn cell_ms(m: &MemoryHandle, table: &str, id: &str, column: &str) -> Option<i64> {
    m.conn().query_row(&format!("SELECT {column} FROM {table} WHERE id=?1"), [id],
                       |r| r.get::<_, Option<i64>>(0)).unwrap()
}

fn cell_slug(m: &MemoryHandle, table: &str, id: &str, column: &str) -> Option<String> {
    m.conn().query_row(&format!("SELECT {column} FROM {table} WHERE id=?1"), [id],
                       |r| r.get::<_, Option<String>>(0)).unwrap()
}

fn ep(m: &MemoryHandle, id: &str, column: &str) -> String {
    cell_text(m, "episode", id, column)
}

fn ep_ms(m: &MemoryHandle, id: &str, column: &str) -> Option<i64> {
    cell_ms(m, "episode", id, column)
}

fn ep_slug(m: &MemoryHandle, id: &str, column: &str) -> Option<String> {
    cell_slug(m, "episode", id, column)
}

fn cd(m: &MemoryHandle, id: &str, column: &str) -> String {
    cell_text(m, "card", id, column)
}

fn cd_ms(m: &MemoryHandle, id: &str, column: &str) -> Option<i64> {
    cell_ms(m, "card", id, column)
}

fn cd_slug(m: &MemoryHandle, id: &str, column: &str) -> Option<String> {
    cell_slug(m, "card", id, column)
}

fn episode_ids(m: &MemoryHandle) -> Vec<String> {
    let mut st = m.conn().prepare("SELECT id FROM episode ORDER BY id").unwrap();
    st.query_map([], |r| r.get::<_, String>(0)).unwrap().map(|x| x.unwrap()).collect()
}

fn member_ids(m: &MemoryHandle, episode_id: &str) -> Vec<String> {
    let mut st = m.conn().prepare(
        "SELECT fact_id FROM episode_fact WHERE episode_id=?1 ORDER BY ord, fact_id").unwrap();
    st.query_map([episode_id], |r| r.get::<_, String>(0)).unwrap().map(|x| x.unwrap()).collect()
}

fn member_ords(m: &MemoryHandle, episode_id: &str) -> Vec<i64> {
    let mut st = m.conn().prepare(
        "SELECT ord FROM episode_fact WHERE episode_id=?1 ORDER BY ord").unwrap();
    st.query_map([episode_id], |r| r.get::<_, i64>(0)).unwrap().map(|x| x.unwrap()).collect()
}

/// 库里 `episode.status` 出现过的值（`SELECT DISTINCT status` 的读数，排序后交回才有确定形状）。
/// 这一格是 I-3d MF-3 的钉子：DDL 允许四值而实现只产两值，值域可达性只能这样从库里读。
fn distinct_status(m: &MemoryHandle) -> Vec<String> {
    let mut st = m.conn().prepare("SELECT DISTINCT status FROM episode ORDER BY status").unwrap();
    st.query_map([], |r| r.get::<_, String>(0)).unwrap().map(|x| x.unwrap()).collect()
}

/// 某个句柄的出处行 `(ref_kind, role, 被引句柄, ord)`，按 `(ref_kind, 被引句柄)` 序。
fn prov_rows(m: &MemoryHandle, mem_table: &str, mem_id: &str)
    -> Vec<(String, String, String, Option<i64>)> {
    let mut st = m.conn().prepare(
        "SELECT ref_kind, role, turn_uid, ord FROM provenance
         WHERE mem_table=?1 AND mem_id=?2 ORDER BY ref_kind, turn_uid").unwrap();
    st.query_map(rusqlite::params![mem_table, mem_id], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    }).unwrap().map(|x| x.unwrap()).collect()
}

fn count(m: &MemoryHandle, sql: &str) -> i64 {
    m.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

/// 最小 JSON 字符串数组解析器。rc-memory 的依赖集里**没有** serde_json（dev 依赖只有
/// tempfile），而 `participants_json` 的写法本身就是一串裸 hex（母档 §2.4 的同一条约定），
/// 所以这里只数引号段，不引一个解析器进来。
fn json_strings(doc: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    for ch in doc.chars() {
        match ch {
            '"' if !in_string => { in_string = true; current.clear(); }
            '"' => { out.push(std::mem::take(&mut current)); in_string = false; }
            c if in_string => current.push(c),
            _ => {}
        }
    }
    out
}

/// 交付 1：id 的 preimage 是 scope + **有序**成员 fact id。
/// 变异形（T-5①）：把 `identity.rs` 的成员段换成无序集合、或改成"只哈希 scope + 标题"⇒
/// 本条在"换序必须换句柄"那句红。
#[test]
fn 句柄由归属与有序成员算出_换序即换句柄_卡与行同源不碰撞() {
    let ids = vec![format!("{:064x}", 1u8), format!("{:064x}", 2u8), format!("{:064x}", 3u8)];
    let row = compute_episode_id(&scope(), &ids);
    let mirrored = compute_episode_card_id(&scope(), &ids);
    assert_eq!((row.len(), mirrored.len()), (64, 64), "两串都是 sha256 hex");
    assert_ne!(row, mirrored, "结构行与镜像卡共用同一组输入，但侧标不同 ⇒ 句柄不可能互相冒充");
    assert_eq!(row, compute_episode_id(&scope(), &ids), "同输入同句柄（内容寻址）");
    let mut swapped = ids.clone();
    swapped.swap(0, 1);
    assert_ne!(row, compute_episode_id(&scope(), &swapped),
        "D3：成员顺序进 preimage ⇒ 换序就是换句柄（母档接受这一格，靠 superseded_by 收尾）");
    assert_ne!(mirrored, compute_episode_card_id(&scope(), &swapped), "镜像卡同步换句柄");
    let other = Scope { kind: ScopeKind::Project, key: "proj-episode-other".into() };
    assert_ne!(row, compute_episode_id(&other, &ids), "归属进句柄 ⇒ 不跨 scope 有物理根据");
    assert_ne!(row, compute_episode_id(&scope(), &ids[..2]), "成员少一个就是另一个句柄");
    // 拼接碰撞：两组不同成员不许算出同一个句柄（段间分隔那一格，与同族同一把尺子）。
    let a = vec!["ab".to_string(), "c".to_string()];
    let b = vec!["a".to_string(), "bc".to_string()];
    assert_ne!(compute_episode_id(&scope(), &a), compute_episode_id(&scope(), &b));
}

/// FIX-2（派发档 I-3e · 交付档 I-3e2 补的那一条）：**句柄取准入后按事件序的成员表**。
///
/// 修前 preimage 用的是调用方递进来的 `fact_ids`，里面**含着被准入拦掉的成员**，于是
/// "两成员的一段"与"其中一条被消音后的一段"算出**同一个** id（I-3d 的 T-7 实测：两遍的
/// `episode.id` 都是 `87bd79ca0b39…`）——同一个句柄对应两套内容，配 `INSERT OR IGNORE`
/// 就是第一遍的内容被永久冻住、重放不收敛，母档 D3 那句"成员变则句柄变"根本没有兑现。
///
/// 这一条把改判后的两面都读出来：
/// ① 同一批输入、一条成员被消音 ⇒ **新句柄 ≠ 旧句柄**，且旧行/旧卡被 `superseded_by` 指过去；
/// ② 换个交入序、成员一个没动 ⇒ **同一个句柄**（归组层换措辞/换序不再造出第二段经历）；
/// 两遍各自幂等：每一遍重跑都是全 0 读数、库里行数与句柄集都不变。
#[test]
fn 句柄取准入后按事件序的成员_少一个成员就换一个句柄() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let (muted, muted_card) = fact_and_card(m, 0, "调通了闸", SRC0, &[1]);
    let alive = fact(m, 1, "补了桩", SRC0 + 4 * MIN, &[2]);
    set_occurred(m, &muted, Some(SRC0 + MIN), None);
    set_occurred(m, &alive, Some(SRC0 + 3 * MIN), None);
    // 调用序恰好等于事件序，好让"消音前后差的那一格"只剩成员集本身（序的对照在下面那条）。
    let ids = vec![muted.clone(), alive.clone()];
    let group = || episode("两段都活的那一段", "先调通闸，再补桩。", ids.clone());

    // ── 第一遍：两条成员都活着 ⇒ 准入后的成员表就是那两条。
    let handle_two = compute_episode_id(&scope(), &ids);
    let card_two = compute_episode_card_id(&scope(), &ids);
    let out1 = put_episodes(m, &[group()], &ctx(), NOW).unwrap();
    assert_eq!((out1.episodes, out1.cards, out1.members, out1.dropped_members), (1, 1, 2, 0),
        "前置：两条成员都准入 ⇒ 句柄、卡与成员表都是这一批");
    assert_eq!(episode_ids(m), vec![handle_two.clone()], "落库的句柄 = hash(scope + 准入后事件序)");
    assert_eq!(member_ids(m, &handle_two), vec![muted.clone(), alive.clone()]);
    // 幂等一：同一批再跑一遍 ⇒ 全 0 读数、行数不变（`INSERT OR IGNORE` 命中自己的句柄）。
    assert_eq!(put_episodes(m, &[group()], &ctx(), NOW).unwrap(), EpisodeWriteOutcome::default(),
        "第一遍的重放收敛");
    assert_eq!(episode_ids(m), vec![handle_two.clone()]);
    assert_eq!(count(m, "SELECT count(*) FROM episode_fact"), 2);

    // ── 把一条成员消音（真 API，与 MF-2 同一支调用），然后交**同一批输入**。
    retire(m, &MemKey { table: MemTable::Card, id: muted_card }, "audit:vote1", NOW).unwrap();
    let handle_one = compute_episode_id(&scope(), std::slice::from_ref(&alive));
    assert_ne!(handle_one, handle_two,
        "FIX-2 的正面判据：成员集真的少了一行 ⇒ 句柄必须换（修前这一句红：两遍同一个 id）");
    let out2 = put_episodes(m, &[group()], &ctx(), NOW).unwrap();
    assert_eq!((out2.episodes, out2.members, out2.dropped_members,
                out2.superseded, out2.superseded_cards), (1, 1, 1, 1, 1),
        "新句柄落一条、被消音那条拦在成员表外、旧结构行与旧镜像卡各被顶掉一条");
    let mut both = vec![handle_two.clone(), handle_one.clone()];
    both.sort();
    assert_eq!(episode_ids(m), both, "库里两段：内容=两成员的那一段与内容=一成员的那一段");
    assert_eq!(member_ids(m, &handle_one), vec![alive.clone()]);
    assert_eq!(member_ids(m, &handle_two), vec![muted.clone(), alive.clone()],
        "旧成员关系不删（它是结构史）");
    assert_eq!(ep(m, &handle_two, "status"), "superseded");
    assert_eq!(ep_slug(m, &handle_two, "superseded_by").as_deref(), Some(handle_one.as_str()));
    assert_eq!(cd_slug(m, &card_two, "superseded_by").as_deref(),
               Some(compute_episode_card_id(&scope(), std::slice::from_ref(&alive)).as_str()),
        "镜像卡与结构行同源换句柄 ⇒ 卡的指针指到新卡");
    // 幂等二：消音后的这一批再跑一遍 ⇒ **行数**一格都不许多（新句柄已在库里，也不再顶掉任何
    // 东西）。`dropped_members` 那一格每遍都照计——它记的是"这一批里有几条没准入"（P-5：被闸
    // 挡下的条数不许蒸发），不是"库里多了几行"，所以它不是 0，其余八格必须是 0。
    let replay = put_episodes(m, &[group()], &ctx(), NOW).unwrap();
    assert_eq!(replay.dropped_members, 1, "拦读数每遍都在，但它是账本不是行数");
    assert_eq!((replay.episodes, replay.members, replay.cards, replay.mentions, replay.provenance,
                replay.superseded, replay.superseded_cards, replay.dropped_episodes,
                replay.owner_ambiguous), (0, 0, 0, 0, 0, 0, 0, 0, 0), "第二遍的重放收敛");
    assert_eq!(episode_ids(m), both);
    assert_eq!(count(m, "SELECT count(*) FROM episode_fact"), 3,
        "两段的成员行：旧的两行（结构史不删）+ 新的一行");

    // ── ② 的另一面：交入序不再决定句柄。倒着递同一批成员（其中一条已判死）⇒
    //    准入后剩下的还是那一条 ⇒ 命中的是 `handle_one` 那一行，不造第三段经历。
    //    （哈希**仍然含序**这一格由上面 `句柄由归属与有序成员算出_换序即换句柄_卡与行同源不碰撞`
    //    钉住；这里钉的是**写入口交进去的那一序由成员自己的时间列决定**，
    //    调用方那一序今天只剩一个作用：点名了哪几条、以及被拦掉几条。标题与叙事本来就不进 preimage。）
    let reversed = vec![alive.clone(), muted.clone()];
    assert_ne!(compute_episode_id(&scope(), &reversed), handle_two,
        "哈希含序没变：倒着递的 preimage 算的是另一个串（正因为如此，下面那句零行才只能由写入口的重排解释）");
    let swapped = put_episodes(m, &[episode("同一批成员倒着递", "同一段经历的另一种交入序。", reversed)],
                               &ctx(), NOW).unwrap();
    assert_eq!((swapped.episodes, swapped.members, swapped.cards, swapped.superseded,
                swapped.superseded_cards, swapped.dropped_episodes), (0, 0, 0, 0, 0, 0),
        "写入口把准入后的成员按事件序排好再算句柄 ⇒ 倒着递命中的仍是库里那一段");
    assert_eq!(episode_ids(m), both, "换序不造第三段经历");
}

/// 交付 2 的主形状：一条 episode 落四族行，逐列都是程序现算的形状。
#[test]
fn 一次写入落结构行成员关系镜像卡与依据出处() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let a = fact(m, 0, "调通了闸", SRC0, &[1]);
    let b = fact(m, 1, "补了桩", SRC0 + 4 * MIN, &[2]);
    set_occurred(m, &a, Some(SRC0 + MIN), Some(SRC0 + 2 * MIN));
    set_occurred(m, &b, Some(SRC0 + 3 * MIN), Some(SRC0 + 9 * MIN));
    let ids = vec![a.clone(), b.clone()];

    let out = put_episodes(m, &[episode("把闸接通", "甲某先调通闸，乙某补了桩。", ids.clone())],
                           &ctx(), NOW).unwrap();
    let id = compute_episode_id(&scope(), &ids);
    let card_id = compute_episode_card_id(&scope(), &ids);
    assert_eq!((out.episodes, out.members, out.cards, out.superseded), (1, 2, 1, 0));
    assert_eq!((out.dropped_members, out.dropped_episodes), (0, 0));
    assert_eq!(episode_ids(m), vec![id.clone()]);
    assert_eq!(member_ids(m, &id), vec![a.clone(), b.clone()]);
    assert_eq!(member_ords(m, &id), vec![0, 1], "ord 从 0 连续");

    assert_eq!((ep(m, &id, "title").as_str(), ep(m, &id, "narrative").as_str()),
               ("把闸接通", "甲某先调通闸，乙某补了桩。"));
    assert_eq!(ep_ms(m, &id, "start_ms"), Some(SRC0 + MIN));
    assert_eq!(ep_ms(m, &id, "end_ms"), Some(SRC0 + 9 * MIN),
        "start/end 只从成员 occurred_at_ms/occurred_end_ms 取（D4-补：不从镜像卡 as_of 反取）");
    assert_eq!(ep(m, &id, "status"), "open", "生命周期归程序：新行恒 open");
    assert_eq!(ep_slug(m, &id, "superseded_by"), None);
    assert_eq!((ep(m, &id, "scope_kind").as_str(), ep(m, &id, "scope_key").as_str()),
               ("project", "proj-episode"), "归属从成员行现算，不跟调用方递的串");
    assert_eq!(ep_slug(m, &id, "world_id"), None, "成员的 world_id 全空 ⇒ 不编一个世界");
    assert_eq!(cell_num(m, "episode", &id, "created_at_ms"), SRC0 + MIN,
        "created_at_ms 取本条的源时刻（= as_of_ms），不取执行钟");
    assert!(cell_num(m, "episode", &id, "norm_version") > 0);
    assert!(!json_strings(&ep(m, &id, "participants_json")).is_empty(),
        "participants_json 是 JSON 数组：{}", ep(m, &id, "participants_json"));

    assert_eq!(cd(m, &card_id, "kind"), "episode");
    assert_eq!((cd(m, &card_id, "title").as_str(), cd(m, &card_id, "text").as_str()),
               ("把闸接通", "甲某先调通闸，乙某补了桩。"));
    assert_eq!(cd_ms(m, &card_id, "as_of_ms"), Some(SRC0 + MIN),
        "as_of = episode.start_ms（事件时间）");
    assert_eq!(cd(m, &card_id, "time_confidence"), "anchored");
    assert_eq!(cd_ms(m, &card_id, "time_anchor_ms"), None,
        "时间锚两列留 NULL：这一族的时刻是成员聚合出来的，不是锚在某一行上");
    assert_eq!(cd_slug(m, &card_id, "time_anchor_id"), None);
    assert_eq!(cd(m, &card_id, "mode"), "inferred", "推导产物显式写 mode，不吃 DEFAULT 'stated'");
    assert_eq!(cd(m, &card_id, "verdict"), "unreviewed", "不冒充已审");
    assert_eq!(cd_slug(m, &card_id, "time_json"), None,
        "time_json 是零读者那一族，本轮不写它（不造第四个实例）");
    let pointers = cd(m, &card_id, "pointers_json");
    assert!(pointers.contains(&format!("fact:{a}")) && pointers.contains(&format!("fact:{b}")),
        "镜像卡的指针指向成员 fact：{pointers}");

    // 出处：每成员一行 ref_kind='fact' + role='basis'，一条 turn 都不写。
    let prov = prov_rows(m, "card", &card_id);
    assert_eq!(prov.len(), 2);
    for (ref_kind, role, target, ord) in &prov {
        assert_eq!((ref_kind.as_str(), role.as_str()), ("fact", "basis"));
        assert_eq!(*ord, None, "basis 行不带 ord（ord 是 link 那一族的格子）");
        assert!(*target == a || *target == b, "依据行该点名成员：{target}");
    }
    assert_eq!(count(m, &format!("SELECT count(*) FROM provenance WHERE mem_table='card' \
                                 AND mem_id='{card_id}' AND ref_kind='turn'")),
               0, "镜像卡一条 turn 出处都不许写（结构性拦点）");
    assert!(count(m, &format!("SELECT count(*) FROM memory_mention WHERE mem_table='card' \
                              AND mem_id='{card_id}'")) >= 1,
        "派生行照模板补 mention（正文里逐字出现的锚）");
}

/// 交付 2 的幂等格：同一批重跑，计数全 0、四族行数一字不变。
#[test]
fn 同一批重跑计数全零且行数不变() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let a = fact(m, 0, "调通了闸", SRC0, &[1]);
    let b = fact(m, 1, "补了桩", SRC0 + 4 * MIN, &[2]);
    set_occurred(m, &a, Some(SRC0 + MIN), None);
    let rows = [episode("把闸接通", "先调通闸，再补桩。", vec![a.clone(), b.clone()])];
    let first = put_episodes(m, &rows, &ctx(), NOW).unwrap();
    assert_eq!((first.episodes, first.members, first.cards), (1, 2, 1));
    let before = (count(m, "SELECT count(*) FROM episode"),
                  count(m, "SELECT count(*) FROM episode_fact"),
                  count(m, "SELECT count(*) FROM card WHERE kind='episode'"),
                  count(m, "SELECT count(*) FROM provenance WHERE ref_kind='fact'"),
                  count(m, "SELECT count(*) FROM memory_mention"));
    let again = put_episodes(m, &rows, &ctx(), NOW + 7_000).unwrap();
    assert_eq!(again, EpisodeWriteOutcome::default(),
        "重跑必须整格全 0：非 0 就是把「计划条数」当成了「写入行数」");
    let after = (count(m, "SELECT count(*) FROM episode"),
                 count(m, "SELECT count(*) FROM episode_fact"),
                 count(m, "SELECT count(*) FROM card WHERE kind='episode'"),
                 count(m, "SELECT count(*) FROM provenance WHERE ref_kind='fact'"),
                 count(m, "SELECT count(*) FROM memory_mention"));
    assert_eq!(before, after, "重跑不许在任何一族里长出第二行");
}

/// 交付 2 的第六件（母档 §2.5）：镜像卡**不进审核游标**——真断，不靠注释。
/// 变异形：把出处那一句的 `ref_kind` 实参递成 `'turn'` ⇒ 本条在游标那句红
/// （`unreviewed_rows` 里 `EXISTS (… ref_kind='turn')` 是唯一拦住它的东西）。
#[test]
fn 镜像卡不进审核游标因为它一条_turn_出处都没有() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let a = fact(m, 0, "调通了闸", SRC0, &[1]);
    let b = fact(m, 1, "补了桩", SRC0 + 4 * MIN, &[2]);
    set_occurred(m, &a, Some(SRC0 + MIN), None);
    put_episodes(m, &[episode("把闸接通", "先调通闸，再补桩。", vec![a.clone(), b.clone()])],
                 &ctx(), NOW).unwrap();
    let card_id = compute_episode_card_id(&scope(), &[a.clone(), b.clone()]);
    // 夹具前提：卡确实落库了、verdict 确实是未审的默认值 ⇒ 看不见它才是谓词在筛，不是行没落。
    assert_eq!((cd(m, &card_id, "kind").as_str(), cd(m, &card_id, "verdict").as_str()),
               ("episode", "unreviewed"));
    let cursor: Vec<String> = reader::unreviewed_rows(m, 200).unwrap()
        .into_iter().map(|r| r.key.id).collect();
    assert!(!cursor.contains(&card_id), "episode 镜像卡不该再花一次判定额度");
    assert!(!cursor.is_empty(), "阳性对照：同一张表里抽取写的卡确实在游标里 ⇒ 游标不是恒空");
    assert_eq!(count(m, &format!("SELECT count(*) FROM provenance WHERE mem_table='card' \
                                 AND mem_id='{card_id}'")), 2, "两行依据出处仍在");
}

/// 交付 2 的准入规则一：无出处的成员不进，且被挡下的条数有读数出口（P-5 那一格）。
/// 变异形（T-5②）：把 `writers_x.rs` 里"成员必须有出处"那一句短路 ⇒ 本条在 `dropped_members`
/// 那句红（无出处那条会被收进成员表）。
#[test]
fn 无出处的成员被准入拦下_全被拦则整条不落() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let good = fact(m, 0, "调通了闸", SRC0, &[1]);
    let bad = unproven_fact(m, "没有出价的行", SRC0 + MIN);
    set_occurred(m, &bad, Some(SRC0 + 2 * MIN), None);

    let ids = vec![good.clone(), bad.clone()];
    let out = put_episodes(m, &[episode("混合成员", "一条有出处一条没有。", ids.clone())],
                           &ctx(), NOW).unwrap();
    assert_eq!(out.dropped_members, 1, "无出处那条必须被拦在成员表之外，并计进读数");
    assert_eq!(out.episodes, 1, "还剩一条有出处的成员 ⇒ episode 照落");
    // FIX-2（派发档 I-3e）：句柄的 preimage 是**准入后**的那一批成员，被拦下的 `bad` 不再进
    // preimage。修前这里递 `ids`（含被拦的那条）也算得出来，因为落库的 id 用的正是调用方那一序；
    // 修后两者分家，"句柄说的就是库里那一组成员"才是这一格要钉的东西。
    let id = compute_episode_id(&scope(), std::slice::from_ref(&good));
    assert_eq!(member_ids(m, &id), vec![good.clone()], "成员表里只有有出处的那一条");
    assert_eq!(out.members, 1);
    assert_eq!(count(m, &format!("SELECT count(*) FROM provenance WHERE mem_table='card' \
                                 AND mem_id='{}' AND turn_uid='{}'",
                                 compute_episode_card_id(&scope(), std::slice::from_ref(&good)), bad)),
               0, "被拦下的成员连依据出处都不该有");

    // 成员全被拦 ⇒ 整条不落：空成员的 episode 没有语义，也不该有一张空卡。
    let out2 = put_episodes(m, &[episode("全是空的", "谁都没有出处。", vec![bad.clone()])],
                            &ctx(), NOW).unwrap();
    assert_eq!((out2.episodes, out2.cards, out2.dropped_episodes, out2.dropped_members),
               (0, 0, 1, 1), "成员全被拦时整条不落，并给出可观测的丢弃数");
    assert_eq!(episode_ids(m), vec![id.clone()], "库里仍只有那一条 episode");
    assert!(!member_ids(m, &id).contains(&bad));
}

/// MF-2 的先红用例（V-4 读数控件 #4 的形状，夹具按它给的构造方法搭）。
/// 打的是**今天可达**的那一形：成员的镜像卡被审核面消音（`rc-distill::audit` 票 1 那一支
/// `retire`，账写在 `mem_table='card'` 上），成员本身却还被收进新 episode 的成员表，
/// 再由一张结构上永不可审的 `mode='inferred'` 镜像卡端回召回面 ⇒ 消音被绕过、新卡无法二次消音。
/// 破坏的是"用户能撤回一条记忆"这条闭环，所以这一格断的是**成员表里没有它**（可观测），
/// 不是注释里的一句承诺。
/// 变异形：把 `writers_x.rs` 的 `member_cards_are_silenced` 短路成 `Ok(false)` ⇒ 本条在
/// `dropped_members == 1` 那句红（`episode_fact` 会多出被消音那一行）。
#[test]
fn 成员的镜像卡被消音后_新_episode_不再收它为成员() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let (silenced, card) = fact_and_card(m, 0, "调通了闸", SRC0, &[1]);
    let alive = fact(m, 1, "补了桩", SRC0 + 4 * MIN, &[2]);
    set_occurred(m, &silenced, Some(SRC0 + MIN), None);
    set_occurred(m, &alive, Some(SRC0 + 2 * MIN), None);
    // 消音走真 API（与 audit 票 1 同一支调用），不手搭 INSERT：这一形今天就是生产者可及的。
    retire(m, &MemKey { table: MemTable::Card, id: card }, "audit:vote1", NOW).unwrap();

    let ids = vec![silenced.clone(), alive.clone()];
    let out = put_episodes(m, &[episode("一段被撤回的经历", "一条已被消音一条还活着。", ids.clone())],
                           &ctx(), NOW).unwrap();
    assert_eq!(out.dropped_members, 1,
        "被消音的成员必须拦在成员表之外并计进读数（修前：准入只问出处 ⇒ 这一句红）");
    assert_eq!(out.episodes, 1, "还剩一条活成员 ⇒ episode 照落");
    // FIX-2：消音那条既不进成员表、也不进句柄的 preimage ⇒ 这一段的身份就是"活着的这一条"。
    let id = compute_episode_id(&scope(), std::slice::from_ref(&alive));
    assert_eq!(member_ids(m, &id), vec![alive.clone()],
        "可观测断言：成员被消音后新 episode 不再收它（episode_fact 里就没有那一行）");

    // 拦下的成员连依据出处都不许写（与"无出处"那一族同一律：被拦 = 这一条在这段经历里不存在）。
    let card_id = compute_episode_card_id(&scope(), std::slice::from_ref(&alive));
    assert_eq!(count(m, &format!("SELECT count(*) FROM provenance WHERE mem_table='card' \
                                 AND mem_id='{card_id}' AND turn_uid='{silenced}'")),
               0, "被拦下的成员连依据出处都不该有");

    // 成员全被消音 ⇒ 整条不落（空成员的 episode 没有语义，也不该有一张把死内容端出来的卡）。
    let out2 = put_episodes(m, &[episode("全是死成员", "唯一一条已被消音。", vec![silenced.clone()])],
                            &ctx(), NOW).unwrap();
    assert_eq!((out2.episodes, out2.cards, out2.dropped_episodes, out2.dropped_members),
               (0, 0, 1, 1), "成员全被消音时整条不落，并给出可观测的丢弃数");
    assert_eq!(episode_ids(m), vec![id.clone()], "库里仍只有那一条 episode");
}

/// MF-2 的第二支：成员**那一行 fact 自己**的死活——`superseded_by` 指针与 `('fact', id)` 的
/// 未解决退场账。口径与 `reader::fact_rows` 的活行三支同源（派发档 I-3d 要求 1）。
///
/// 这两形**今天没有生产者**（`MemTable` 只有 `Anchor|Card` ⇒ `retire`/`supersede` 落不到 `'fact'`；
/// 中心实测 `fact.superseded_by` 非空 0 行、`retirement` 0 行），所以夹具按 `unproven_fact`
/// 同一族手搭一条库里形状。它钉的是"口径已经钉死、将来真出现生产者时这一格有人守"，
/// 不是"库里已经有死 fact"。第三支（`attestation` 落在 `'fact'`）本层**判不到**，
/// 理由写在 `writers_x::admitted_member` 的文档里，这里不假装它被拦。
#[test]
fn 成员那一行自己带着退场账或被顶掉指针时也不准入() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let retired = fact_and_card(m, 0, "调通了闸", SRC0, &[1]).0;
    let superseded = fact(m, 1, "补了桩", SRC0 + 4 * MIN, &[2]);
    set_occurred(m, &retired, Some(SRC0 + MIN), None);
    set_occurred(m, &superseded, Some(SRC0 + 2 * MIN), None);
    // 手搭两形：一条带未解决的退场账（`mem_table='fact'`），一条行上挂着 superseded_by 指针。
    m.conn().execute(
        "INSERT INTO retirement(mem_table, mem_id, reason, created_at_ms, resolved_at_ms)
         VALUES('fact',?1,'fixture:no-producer',?2,NULL)",
        rusqlite::params![retired, SRC0],
    ).unwrap();
    m.conn().execute("UPDATE fact SET superseded_by=?1 WHERE id=?2",
                     rusqlite::params![format!("{:064x}", 0xfdu8), superseded]).unwrap();

    let out = put_episodes(m, &[episode("两条都判死了", "退场账与被顶掉指针各一条。",
                                        vec![retired.clone(), superseded.clone()])],
                           &ctx(), NOW).unwrap();
    assert_eq!((out.dropped_members, out.dropped_episodes, out.episodes), (2, 1, 0),
        "两支都要拦：一支恒不命中就把整条口径放掉，将来有生产者时没人红");
    assert_eq!(count(m, "SELECT count(*) FROM episode"), 0, "成员全拦 ⇒ 零行");
    assert_eq!(count(m, "SELECT count(*) FROM episode_fact"), 0);
}

/// 交付 2 的准入规则二：成员跨 scope ⇒ 调用方的错，整批 `Err` 且零行（不许静默切一半）。
#[test]
fn 成员跨_scope_时整批报错且库里零行() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let a = fact(m, 0, "调通了闸", SRC0, &[1]);
    let other_scope = Scope { kind: ScopeKind::Project, key: "proj-elsewhere".into() };
    let other_text = "丙某别处的行";
    let other = FactWriteItem {
        subject_anchor_id: anchor(2),
        object_anchor_id: None,
        predicate: "做了".into(),
        value: Some("别处的行".into()),
        category: "event".into(),
        mode: "stated".into(),
        time_text: String::new(),
        quote: "别处的行".into(),
        evidence: vec![(turn(9), AnchorRole::Human)],
        text: other_text.into(),
        scope: other_scope.clone(),
        source_ms: SRC0 + MIN,
    };
    let other_id = compute_fact_id(&anchor(2), "做了", None, Some("别处的行"), "event", "",
                                   other_text, &other_scope);
    put_fact_rows_with_extras(m, &[other], &[FactExtras::default()], &ctx(), &owner(), NOW).unwrap();

    let err = put_episodes(m, &[episode("跨了归属", "两条 fact 不在同一个 scope。",
                                        vec![a.clone(), other_id.clone()])], &ctx(), NOW);
    assert!(matches!(err, Err(MemError::Sql(_))), "跨 scope 是调用方的错：{err:?}");
    assert_eq!(count(m, "SELECT count(*) FROM episode"), 0, "报错时零行");
    assert_eq!(count(m, "SELECT count(*) FROM episode_fact"), 0);
    assert_eq!(count(m, "SELECT count(*) FROM card WHERE kind='episode'"), 0);
    assert_eq!(count(m, "SELECT count(*) FROM provenance WHERE ref_kind='fact'"), 0);
}

/// 交付 2 的时间格一（D4）：成员有事件时间 ⇒ `as_of_ms` = `start_ms`、`time_confidence='anchored'`。
#[test]
fn 有事件时间_as_of_取起点并标_anchored() {
    let (_dir, root) = seeded();
    let m = root.memory();
    // 发话时刻故意**早于**事件时间：错取发话钟的话 as_of 就会等于 SRC0。
    let a = fact(m, 0, "开会", SRC0, &[1]);
    let b = fact(m, 1, "散会", SRC0 + MIN, &[2]);
    let ids = vec![a.clone(), b.clone()];
    set_occurred(m, &a, Some(SRC0 + 10 * MIN), Some(SRC0 + 20 * MIN));
    set_occurred(m, &b, Some(SRC0 + 30 * MIN), None);
    put_episodes(m, &[episode("那次会议", "开了会并散了会。", ids.clone())], &ctx(), NOW).unwrap();
    let id = compute_episode_id(&scope(), &ids);
    let card_id = compute_episode_card_id(&scope(), &ids);
    assert_eq!(ep_ms(m, &id, "start_ms"), Some(SRC0 + 10 * MIN));
    assert_eq!(ep_ms(m, &id, "end_ms"), Some(SRC0 + 30 * MIN),
        "occurred_end_ms 缺省的那条退自己的 occurred_at_ms；end 取成员里的最大值");
    assert_eq!(cd_ms(m, &card_id, "as_of_ms"), Some(SRC0 + 10 * MIN));
    assert_eq!(cd(m, &card_id, "time_confidence"), "anchored");
    assert_eq!(cell_num(m, "card", &card_id, "created_at_ms"), SRC0 + 10 * MIN,
        "卡的 created_at_ms 与 as_of 同一把源钟（同 timeline/causal 两族那一格）");
}

/// 交付 2 的时间格二（D4 新增的一支）：成员全**无**事件时间 ⇒ `start/end` 保持 NULL，
/// `as_of_ms` 退到**最早成员的发话时刻**并标 `'speech'`（不是 `'anchored'`）。
#[test]
fn 成员全无事件时间_起点保持空_as_of_退到发话时刻并标_speech() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let a = fact(m, 0, "说了甲", SRC0 + 7 * MIN, &[1]);
    let b = fact(m, 1, "说了乙", SRC0 + 2 * MIN, &[2]);   // 最早的发话时刻
    let ids = vec![a.clone(), b.clone()];
    put_episodes(m, &[episode("一段没有日期的经历", "只记得说过。", ids.clone())],
                 &ctx(), NOW).unwrap();
    // FIX-2：成员全无事件时间时，事件序退到**发话时刻升序** ⇒ preimage 用的是 `[b, a]`
    // 而不是调用方递进来的 `[a, b]`（这一格把"谁决定那个序"读得最直白：库里那一行说的序）。
    let event_order = vec![b.clone(), a.clone()];
    let id = compute_episode_id(&scope(), &event_order);
    let card_id = compute_episode_card_id(&scope(), &event_order);
    assert_eq!((ep_ms(m, &id, "start_ms"), ep_ms(m, &id, "end_ms")), (None, None),
        "成员全未知 ⇒ episode.start/end 保持 NULL");
    assert_eq!(cell_num(m, "episode", &id, "created_at_ms"), SRC0 + 2 * MIN,
        "created_at_ms 也取源侧时刻，不取 now_ms");
    assert_eq!(cd_ms(m, &card_id, "as_of_ms"), Some(SRC0 + 2 * MIN));
    assert_eq!(cd(m, &card_id, "time_confidence"), "speech",
        "'speech' = as_of 取的是发话时刻（最早成员的 created_at_ms），不是事件时间");
    assert_eq!(cd_ms(m, &card_id, "time_anchor_ms"), None, "退到发话时刻时不假称有时刻锚");
    assert_ne!(cd_ms(m, &card_id, "as_of_ms"), Some(NOW), "任何情况不许用执行钟");
    assert_eq!(cd(m, &card_id, "mode"), "inferred");
}

/// 混合形：已知的在前、未知的在后；同输入两次读回的 ord 序列一致（母档 §3 第三条）。
/// 未知那一格的"标 unknown"由 `fact.occurred_at_ms IS NULL` 自己说（不许为它加列）。
#[test]
fn 成员排序已知在前未知在后且两次一致() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let late = fact(m, 0, "晚发生的", SRC0, &[1]);
    let early = fact(m, 1, "早发生的", SRC0 + 30 * MIN, &[2]);
    let undated = fact(m, 2, "没日期的", SRC0 + 40 * MIN, &[3]);
    set_occurred(m, &late, Some(SRC0 + 10 * MIN), None);
    set_occurred(m, &early, Some(SRC0 + MIN), None);
    let ids = vec![late.clone(), undated.clone(), early.clone()];
    put_episodes(m, &[episode("三段经历", "两段时间已知一段未知。", ids.clone())],
                 &ctx(), NOW).unwrap();
    // FIX-2：进 preimage 的那一序**就是**事件序（调用方那一序今天只决定"点名了哪几条"）。
    // 于是这一句同时钉两件事：句柄与成员表同源（同一批成员、同一个序），
    // 以及"归组层换个交入序再交同一段经历"不再换句柄。
    let event_order = vec![early.clone(), late.clone(), undated.clone()];
    let id = compute_episode_id(&scope(), &event_order);
    let ordered = reader::episode_members(m, &id).unwrap();
    assert_eq!(ordered.iter().map(|f| f.id.clone()).collect::<Vec<_>>(), event_order,
        "ord 是**事件序**：已知按 occurred_at_ms 升序在前，未知的按发话时刻升序排在后面");
    assert_eq!(member_ords(m, &id), vec![0, 1, 2]);
    assert_eq!(ordered.last().unwrap().occurred_at_ms, None, "未知那一格自己会说话");
    let again = reader::episode_members(m, &id).unwrap();
    assert_eq!(again.iter().map(|f| f.id.clone()).collect::<Vec<_>>(),
               ordered.iter().map(|f| f.id.clone()).collect::<Vec<_>>(),
        "同输入两次读回的顺序必须一致（排序不许跟着查询计划抖）");
    assert_eq!((ep_ms(m, &id, "start_ms"), ep_ms(m, &id, "end_ms")),
               (Some(SRC0 + MIN), Some(SRC0 + 10 * MIN)), "start/end 只数已知的那两条");
}

/// 交付 2 的第七件（D3 的收尾）：成员集变化 ⇒ 新句柄 + 旧行与旧卡被 `superseded_by` 指过去。
/// 前驱按"共享成员"认，不靠重算旧句柄（旧句柄用的是调用方给的那一序，库里没有存它）。
#[test]
fn 成员集变大时旧行与旧卡被指过去而新行是独立句柄() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let a = fact(m, 0, "调通了闸", SRC0, &[1]);
    let b = fact(m, 1, "补了桩", SRC0 + 4 * MIN, &[2]);
    let c = fact(m, 2, "结了案", SRC0 + 9 * MIN, &[3]);
    set_occurred(m, &a, Some(SRC0 + MIN), None);
    set_occurred(m, &b, Some(SRC0 + 3 * MIN), None);
    put_episodes(m, &[episode("把闸接通", "先调通闸。", vec![a.clone(), b.clone()])],
                 &ctx(), NOW).unwrap();
    let old_row = compute_episode_id(&scope(), &[a.clone(), b.clone()]);
    let old_card = compute_episode_card_id(&scope(), &[a.clone(), b.clone()]);
    let new_ids = vec![a.clone(), b.clone(), c.clone()];
    let out = put_episodes(m, &[episode("把闸接通结案", "调通闸、补桩、结案。", new_ids.clone())],
                           &ctx(), NOW).unwrap();
    let new_row = compute_episode_id(&scope(), &new_ids);
    let new_card = compute_episode_card_id(&scope(), &new_ids);
    assert_eq!((out.episodes, out.superseded, out.superseded_cards), (1, 1, 1),
        "新行落一条、旧结构行与旧镜像卡各被顶掉一条（旁路写入也有读数出口，R3）");
    assert_ne!(old_row, new_row);
    assert_eq!(ep(m, &old_row, "status"), "superseded");
    assert_eq!(ep_slug(m, &old_row, "superseded_by").as_deref(), Some(new_row.as_str()),
        "旧 episode 走 status='superseded' + superseded_by=新行 id（D3）");
    assert_eq!(cd_slug(m, &old_card, "superseded_by").as_deref(), Some(new_card.as_str()),
        "旧镜像卡指向新卡（D3 要的那一格）");
    assert_eq!(cd(m, &new_card, "kind"), "episode");
    assert_eq!(ep(m, &new_row, "status"), "open");
    assert_eq!(ep_slug(m, &new_row, "superseded_by"), None);
    let mut want = vec![old_row.clone(), new_row.clone()];
    want.sort();
    assert_eq!(episode_ids(m), want);
    // 顶掉之后旧成员关系**不删**（它是结构史），反向查只认活着的那条。
    assert_eq!(member_ids(m, &old_row), vec![a.clone(), b.clone()]);
    assert_eq!(reader::episode_of_fact(m, &a).unwrap().as_deref(), Some(new_row.as_str()),
        "反向查给的是活着的 episode，不是被顶掉的那一条");
    // 重跑第二批：新行已存在 ⇒ 整格全 0、不再顶掉任何东西。
    assert_eq!(put_episodes(m, &[episode("把闸接通结案", "调通闸、补桩、结案。", new_ids)],
                            &ctx(), NOW).unwrap(), EpisodeWriteOutcome::default());
}

/// I-3d MF-3 的钉现状用例：DDL（`rc-store/src/ddl.rs` 的 `episode` 建表那条 status CHECK）允许四个 status 值，实现今天只有
/// **两个生产者**——新落的一行（`open`）与被同归属、共享成员的新行顶掉的一行（`superseded`）。
/// 这条走两条真生产者把两值都长出来，再用 `SELECT DISTINCT status FROM episode` 钉住
/// "库里只可能出现这两值"，让任何新开的第三值写点（含旁路 INSERT）当场被看见。
/// `resolved`/`dormant` 为什么留空、以及**缺哪两格证据才能填**，登记在
/// `writers_x::put_episodes` 的文档里（控制器裁定：本轮不发明判定规则）。
/// 变异形（I-3d T-2 实跑过，红原文见 tasks/t-episode-mustfix-red-mf3-mutation.log）：
/// 在本条末尾手插一行 episode、status 取 DDL 允许而实现不产的第三值 ⇒ 本条红在最后那句
/// DISTINCT（读数从两值变三值）。
#[test]
fn episode_status_今天只有_open_与_superseded_两值() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let a = fact(m, 0, "调通了闸", SRC0, &[1]);
    let b = fact(m, 1, "补了桩", SRC0 + 4 * MIN, &[2]);
    let c = fact(m, 2, "结了案", SRC0 + 9 * MIN, &[3]);
    set_occurred(m, &a, Some(SRC0 + MIN), None);
    // 生产者一：新落的一行。
    put_episodes(m, &[episode("把闸接通", "先调通闸，再补桩。", vec![a.clone(), b.clone()])],
                 &ctx(), NOW).unwrap();
    assert_eq!(distinct_status(m), vec!["open"], "只有写入口那一句 INSERT 时，库里只可能出现 open");
    // 生产者二：同归属、共享成员的新行把旧行顶掉。
    put_episodes(m, &[episode("把闸接通结案", "调通闸、补桩、结案。",
                              vec![a.clone(), b.clone(), c.clone()])], &ctx(), NOW).unwrap();
    assert_eq!(count(m, "SELECT count(*) FROM episode"), 2, "两行都在库里：读数不是空表蒙出来的");
    assert_eq!(distinct_status(m), vec!["open", "superseded"],
        "四值里实现只产这两值；出现第三个值就是有人的旁路写点或新的生产者可及了");
}

/// 交付 3：正向按 ord 读成员（新写的 SELECT），反向按 fact 查句柄。
#[test]
fn 正向读成员按_ord_反向查句柄只认活行() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let a = fact(m, 0, "调通了闸", SRC0, &[1]);
    let b = fact(m, 1, "补了桩", SRC0 + 4 * MIN, &[2]);
    set_occurred(m, &a, Some(SRC0 + MIN), None);
    let ids = vec![a.clone(), b.clone()];
    put_episodes(m, &[episode("把闸接通", "先调通闸，再补桩。", ids.clone())], &ctx(), NOW).unwrap();
    let id = compute_episode_id(&scope(), &ids);
    let members: Vec<FactRow> = reader::episode_members(m, &id).unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(members.iter().map(|f| f.id.clone()).collect::<Vec<_>>(),
               vec![a.clone(), b.clone()]);
    // FactRow 的形状齐全（不是裸 id 列表）：正文/类别/归属/两把钟都在。
    let first = &members[0];
    assert_eq!((first.category.as_str(), first.text.as_str()), ("event", "甲某调通了闸"));
    assert_eq!((first.scope_kind.as_str(), first.scope_key.as_str()),
               ("project", "proj-episode"));
    assert_eq!((first.source_agent.as_str(), first.device_id.as_str()),
               ("agent-episode-w", "dev-episode-1"));
    assert_eq!(first.occurred_at_ms, Some(SRC0 + MIN));
    assert_eq!(first.created_at_ms, SRC0, "发话钟 = 源轮时刻（不是导入钟）");
    assert_eq!(first.at_ms, SRC0 + MIN, "视图序：occurred → valid_from → created");
    assert_eq!(reader::episode_of_fact(m, &b).unwrap().as_deref(), Some(id.as_str()));
    assert_eq!(reader::episode_of_fact(m, &format!("{:064x}", 0xfdu8)).unwrap(), None,
        "不属于任何 episode 的行给 None，不编造");
    assert!(reader::episode_members(m, &format!("{:064x}", 0xfeu8)).unwrap().is_empty(),
        "不存在的 episode 给空表，不报错");
}

/// 参与者那一格：成员涉及锚的**并集**、去重、裸 anchor id 数组（母档 §2.4 的同一条约定）。
#[test]
fn 参与者是成员锚的并集且存成裸数组() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let a = fact(m, 0, "调通了闸", SRC0, &[1, 2]);
    let b = fact(m, 1, "补了桩", SRC0 + 4 * MIN, &[2]);   // 与 a 共享丙
    let ids = vec![a.clone(), b.clone()];
    set_occurred(m, &a, Some(SRC0 + MIN), None);
    put_episodes(m, &[episode("把闸接通", "两条一起做。", ids.clone())], &ctx(), NOW).unwrap();
    let id = compute_episode_id(&scope(), &ids);
    let parsed = json_strings(&ep(m, &id, "participants_json"));
    let mut got = parsed.clone();
    got.sort();
    let mut want = vec![anchor(0), anchor(1), anchor(2)];
    want.sort();
    assert_eq!(got, want, "成员参与者的并集（含主语）去重后逐条是裸 anchor id：{parsed:?}");
    assert!(parsed.iter().all(|s| s.chars().all(|c| c.is_ascii_hexdigit()) && s.len() == 64),
        "裸 id 数组：不带 `anchor:` 前缀（与 fact.participants_json 同约定）");
}

/// 只读句柄在写入口第一行就拒（与其余写入口同一条闸门，I-3）。
#[test]
fn 只读句柄被写入口拒绝而不是留下半批() {
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("ro-copy.db");
    let a;
    {
        let root = Root::open(dir.path()).unwrap();
        for (index, name) in NAMES.iter().take(2).enumerate() {
            seed_anchor(root.memory(), &anchor(index), name, "proj-episode");
        }
        a = fact(root.memory(), 0, "调通了闸", SRC0, &[1]);
    }   // Root 必须先进块作用域关掉再拷：记忆库是 WAL，句柄没关就拷，拷出去的是空壳。
    std::fs::copy(dir.path().join("memory.db"), &snap).unwrap();
    let replica = rc_store::open_replica(&snap).unwrap();
    let err = put_episodes(&replica, &[episode("把闸接通", "读副本不许写。", vec![a])],
                           &ctx(), NOW);
    assert!(matches!(err, Err(MemError::ReadOnly)), "要念的是 ReadOnly：{err:?}");
}

/// T-6 副本真跑（派发档 I-3a 验收）。**只在 `EPISODE_A_ACCEPT` 指着一份手工 cp 出来的副本时才动手**：
/// 平时这个环境变量不设 ⇒ 直接 return（不 `#[ignore]`、不 skip，为的是 `#[test]` 条数与
/// suite 的 passed 数仍然一格不差——本仓出过"编译进去但跑 0 条"的坑）。
///
/// 挑的成员是真库里唯一一对"非自锚参与者重叠且时间相邻"的（调查包 §3.3/§6.3 的阳性对照
/// `76db98675c23 × fe6520673893`，Δt=13.5min，共同参与者 = Qwen3.8-27B）。
/// 现网两行的 `occurred_at_ms` 都是 NULL（S0 §1.1：4/36 非空，这两条不在那 4 条里），
/// 所以这份副本真跑落在 `'speech'` 那一支——这正是本轮新加那一支的真数据形状。
/// **现网零写入**：写的全部动作都落在 `EPISODE_A_ACCEPT` 目录里那份副本上。
#[test]
fn 副本真跑_put_episodes_并在审核游标外面() {
    let Some(dir) = std::env::var_os("EPISODE_A_ACCEPT") else { return; };
    let root = Root::open(std::path::Path::new(&dir)).expect("副本打不开");
    let m = root.memory();
    // 成员、署名、归属、叙事**一律从副本里读**，不在源码里抄现网专名（本文件的夹具纪律）。
    // 挑的是调查包 §3.3/§6.3 那对唯一"非自锚参与者重叠且时间相邻"的行（Δt=13.5min）。
    let member = |prefix: &str| -> LiveMember {
        let mut st = m.conn().prepare(
            "SELECT id, text, source_agent, device_id, scope_kind, scope_key, occurred_at_ms, \
                    created_at_ms \
               FROM fact WHERE id LIKE ?1 || '%' ORDER BY id").unwrap();
        let hits: Vec<LiveMember> = st.query_map([prefix], |r| {
            let kind = r.get::<_, String>(4)?;
            Ok(LiveMember {
                id: r.get(0)?,
                text: r.get(1)?,
                source_agent: r.get(2)?,
                device_id: r.get(3)?,
                scope: Scope { kind: ScopeKind::parse(&kind).unwrap_or(ScopeKind::Project),
                               key: r.get(5)? },
                occurred_at_ms: r.get(6)?,
                created_at_ms: r.get(7)?,
            })
        }).unwrap().map(|x| x.unwrap()).collect();
        assert_eq!(hits.len(), 1, "现网句柄前缀 {prefix} 应恰命中一行，实际 {}", hits.len());
        hits.into_iter().next().unwrap()
    };
    let first = member("76db98675c23");
    let second = member("fe6520673893");
    let ids = vec![first.id.clone(), second.id.clone()];
    let real_ctx = BatchCtx { source_agent: first.source_agent.clone(),
                              device_id: first.device_id.clone() };
    let scope_real = first.scope.clone();
    assert_eq!((scope_real.kind.as_str(), scope_real.key.as_str()),
               (second.scope.kind.as_str(), second.scope.key.as_str()),
        "两条成员必须同归属，否则写入口会整批报错");
    // 叙事就是两条成员正文拼起来（真跑用；归组层的写法归 I-3b）。
    let narrative = format!("{}{}", first.text, second.text);
    let group = || episode("副本真跑：相邻的两条", &narrative, ids.clone());

    let before = (count(m, "SELECT count(*) FROM episode"),
                  count(m, "SELECT count(*) FROM episode_fact"),
                  count(m, "SELECT count(*) FROM card"),
                  count(m, "SELECT count(*) FROM provenance"),
                  count(m, "SELECT count(*) FROM memory_mention"));
    let out = put_episodes(m, &[group()], &real_ctx, NOW).unwrap();
    let id = compute_episode_id(&scope_real, &ids);
    let card_id = compute_episode_card_id(&scope_real, &ids);
    println!("EPISODE_A_ACCEPT={} 成员={}+{} 归属={}/{}",
             dir.to_string_lossy(), &ids[0][..12], &ids[1][..12],
             scope_real.kind.as_str(), scope_real.key);
    println!("成员事件时间={:?} 发话时刻={}+{}（相邻 {}s）",
             [first.occurred_at_ms, second.occurred_at_ms], first.created_at_ms,
             second.created_at_ms, (second.created_at_ms - first.created_at_ms) / 1000);
    println!("第一轮读数：{out:?}");
    println!("episode.id={id}");
    println!("镜像卡 id={card_id}");
    println!("episode 行：start_ms={:?} end_ms={:?} status={} created_at_ms={} participants={}",
             ep_ms(m, &id, "start_ms"), ep_ms(m, &id, "end_ms"), ep(m, &id, "status"),
             cell_num(m, "episode", &id, "created_at_ms"), ep(m, &id, "participants_json"));
    println!("镜像卡：as_of_ms={} time_confidence={} mode={} kind={} verdict={} pointers={}",
             cell_num(m, "card", &card_id, "as_of_ms"), cd(m, &card_id, "time_confidence"),
             cd(m, &card_id, "mode"), cd(m, &card_id, "kind"), cd(m, &card_id, "verdict"),
             cd(m, &card_id, "pointers_json"));
    println!("成员按 ord 读回={:?}", reader::episode_members(m, &id).unwrap()
             .iter().map(|f| (&f.id[..12], f.occurred_at_ms, f.created_at_ms)).collect::<Vec<_>>());
    println!("反向查={:?}", reader::episode_of_fact(m, &ids[0]).unwrap().map(|s| s[..12].to_string()));
    let after = (count(m, "SELECT count(*) FROM episode"),
                 count(m, "SELECT count(*) FROM episode_fact"),
                 count(m, "SELECT count(*) FROM card"),
                 count(m, "SELECT count(*) FROM provenance"),
                 count(m, "SELECT count(*) FROM memory_mention"));
    println!("五表行数 前={before:?} 后={after:?}");
    assert_eq!((after.0, after.1, after.2, after.3),
               (before.0 + 1, before.1 + 2, before.2 + 1, before.3 + 2),
        "真数据上应恰好多出 1 段经历、2 行成员、1 张卡、2 行依据出处");
    assert!(after.4 >= before.4, "mention 只增不减：{} → {}", before.4, after.4);
    println!("mention 增量={}（写入口报 {}）", after.4 - before.4, out.mentions);
    assert_eq!(out.provenance, 2);
    // 幂等在真数据上再走一遍。
    let again = put_episodes(m, &[group()], &real_ctx, NOW).unwrap();
    println!("第二轮读数（幂等）：{again:?}");
    assert_eq!(again, EpisodeWriteOutcome::default(), "同一批重跑必须整格全 0");
    // 真数据面上的拦点断言。这里的**阳性对照不是游标非空**——副本上 36 张抽取卡今天全部
    // `verdict='passed'`（已经过审），游标本就该是空的；换成"未审卡恰有一张、就是这张镜像卡，
    // 而游标仍然为 0"这一对读数：它拦住的原因只能是"它没有 turn 出处"，不是"limit 把它挤掉了"。
    // （这条是现网读数快照，只在手工设了 `EPISODE_A_ACCEPT` 时跑；合成夹具那一侧的阳性对照
    // 在 `镜像卡不进审核游标因为它一条_turn_出处都没有` 里。）
    let unreviewed_cards = count(m, "SELECT count(*) FROM card WHERE verdict='unreviewed'");
    let cursor: Vec<String> = reader::unreviewed_rows(m, 400).unwrap()
        .into_iter().map(|r| r.key.id).collect();
    println!("审核游标行数={} / 库里未审卡={} / 镜像卡在游标里={}",
             cursor.len(), unreviewed_cards, cursor.contains(&card_id));
    assert!(!cursor.contains(&card_id), "真数据上镜像卡也不许进审核游标");
    assert_eq!(unreviewed_cards, 1, "副本上唯一一张未审卡就是这张镜像卡");
    assert_eq!(cursor.len(), 0, "唯一那张未审卡被拦在游标外 ⇒ 游标空是拦住它的读数");
}

/// 空内容、批内重复成员、成员不在库里，都是调用方的错：整批 `Err` 且零行
/// （同 `put_batch` 的"先查后写"那一律）。空批次直接返全 0。
#[test]
fn 坏内容整批报错零行而空批次直接返全零() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let a = fact(m, 0, "调通了闸", SRC0, &[1]);
    let b = fact(m, 1, "补了桩", SRC0 + MIN, &[2]);
    let bad_cases = [
        episode("空成员", "一条成员都没有。", vec![]),
        episode("批内重复", "同一条给了两遍。", vec![a.clone(), a.clone()]),
        episode("   ", "标题是空白。", vec![a.clone()]),
        episode("空叙事", "   ", vec![a.clone()]),
        episode("成员不存在", "库里没有的那一条。", vec![format!("{:064x}", 0x7fu8)]),
    ];
    for bad in bad_cases {
        let err = put_episodes(m, &[bad, episode("第二条是好的一起给", "好的正文。",
                                                 vec![a.clone(), b.clone()])], &ctx(), NOW);
        assert!(err.is_err(), "坏内容必须在落库之前就被拦住");
        assert_eq!(count(m, "SELECT count(*) FROM episode"), 0, "整批零行：{err:?}");
        assert_eq!(count(m, "SELECT count(*) FROM episode_fact"), 0);
        assert_eq!(count(m, "SELECT count(*) FROM card WHERE kind='episode'"), 0);
    }
    assert_eq!(put_episodes(m, &[], &ctx(), NOW).unwrap(), EpisodeWriteOutcome::default(),
        "空批次直接返全 0，不报错");
}
