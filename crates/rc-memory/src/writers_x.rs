//! X（抽取作业）的写入口：X 递得进来的只有内容列与归属，四个 P 独占列全部由本模块盖章。
//! `source_agent` / `device_id` 的**唯一入口是 `BatchCtx`**——那是 P 侧数据的载体、不是模型数据：
//! 三个内容形状 `EntityContent` / `FactContent` / `WriteItem` 里根本没有这两格。真批次经
//! `put_batch` 的那一个 ctx 形参位进来（`PStamps::from_ctx`），旧四把 `put_*` 走
//! `BatchCtx::synthetic()`。`created_at_ms` 只有 `now_ms` 这一个**时钟**形参位（P 侧的钟，
//! 三个内容形状与模型侧都递不进它），`norm_version` 连时钟位都没有——它是
//! `rc_store::capability::NORM_VERSION` 常量。§2 那句"`assert_writable(列, actor)` 降级成
//! 写不出这个函数"说的就是这种为真：不是运行时报错，是内容侧根本没有那个形参位。

// 链头行的 id 与边 id 同一家（`identity.rs`）：**都不走 `compute_mem_id`**，理由写在那一格的注释里。
use crate::identity::{
    compute_chain_id, compute_edge_id, compute_episode_card_id, compute_episode_id,
    compute_event_link_id, compute_mem_id,
};
use crate::keys::{MemKey, MemTable, Scope, ScopeKind};
use crate::owner::{self, OwnerCtx, Visibility};
use crate::{MemError, Provenance};
use rc_contract::normalize_body;
use rc_store::MemoryHandle;
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// 实体那一行的内容形状。`aliases` 是"一句话指到一个人"的唯一入口，因此它是守卫 10 的门。
#[derive(Debug, Clone)]
pub struct EntityContent {
    pub kind: String,
    pub title: String,
    pub scope: Scope,
    pub aliases: Vec<String>,
}

/// card 的内容形状。旧三张候选表（card/event/assertion）合并成 canonical 的 `card` 一张，
/// 三条公开入口（`put_card`/`put_event`/`put_assertion`）都落这里，差别只剩调用方的 `kind`。
#[derive(Debug, Clone)]
pub struct FactContent {
    pub kind: String,
    pub title: String,
    pub body: String,
    pub scope: Scope,
    /// 处境类记忆保质期的唯一载体（D10）。None = 不设保质期。
    pub valid_to_ms: Option<i64>,
}

/// 守卫 10。这张表只收一类东西：人称与指示代词。判据是"整串等于"，不是"含"——
/// 否则"我们的项目"被误伤，而误拦比漏拦贵（真实体进不了库）。
///
/// 为什么闸门在写入侧而不是检索侧：别名表是"一句话指到一个人"的唯一入口。'我' 一旦进表，
/// 每一句人话都能命中一条记忆（洪水），于是处境类召回会被"顺手"改成加代词别名——
/// 那是 §8 明令禁止的那条修法（它同时防两个方向）。
pub const PRONOUNS: &[&str] = &["我", "你", "他", "她", "它", "我们", "你们", "他们", "她们",
                               "它们", "自己", "别人", "这", "那", "这个", "那个", "谁"];
pub const PRONOUNS_ASCII: &[&str] = &["i", "me", "my", "mine", "you", "your", "he", "him", "she",
                                      "her", "it", "they", "them", "we", "us", "our", "this", "that",
                                      "who", "someone", "anyone"];

pub fn check_alias(s: &str) -> Result<(), MemError> {
    let t = s.trim();
    if t.is_empty() { return Err(MemError::ContentEmpty); }
    if PRONOUNS.contains(&t) { return Err(MemError::AliasIsPronoun(t.to_string())); }
    let low = t.to_ascii_lowercase();
    if PRONOUNS_ASCII.contains(&low.as_str()) { return Err(MemError::AliasIsPronoun(t.to_string())); }
    Ok(())
}

/// 机制类 relation（派发档 I-2 的 P-4）：`fact_link.relation` 九值闭集（`ddl.rs` 的 `fact_link` 建表那条 CHECK）里
/// "必须说得出机制"的那四支。设计对"无证据不落"的要求针对的就是这一支。
///
/// 为什么这个闭集的家在 `rc-memory` 而不是 `rc-derive`：`rc-derive` 依赖 `rc-memory`，
/// 闸的两半（模型面住 parse、门口住本文件）因此**共用一份定义**——同一个闭集在两个 crate
/// 各写一遍就是守卫 3 点名的"改一处漏一处"，而漂的那一处不报错，只让闸少拦一支。
///
/// 另外五支（`part_of` / `depends_on` / `owns` / `located_in` / `role`）是与关系层重叠的
/// **结构归属**：对它们要求机制句得到的是一批假拒（P-4 原文），所以这一支闭集只准入闸用，
/// 不进任何"所有 relation 都要机制句"的循环。
pub const MECHANISM_RELATIONS: &[&str] = &["causes", "enables", "prevents", "responds_to"];

/// 说不出事情的机制句**套话**（闭集）。判据是"整串等于"，不是"含"——与上面 `PRONOUNS` /
/// `check_alias` 同一把尺子，理由也同一条：误拦比漏拦贵。"含"会把
/// "两个阈值时间相邻，但挪动的其实是那条封顶决定"这种**真机制句**吃掉，
/// 而一句真机制不会整串等于"时间相邻"。
///
/// 空白串也算套话（`is_placeholder_mechanism` 的第一支）：`fact_link.basis` 是 NOT NULL，
/// 空串**落得进库、也读得回来**，库自己不会红 ⇒ 值域这一半只能由写侧把守（同 `ROLE_BASIS`
/// 那一格的纪律：跨列 CHECK 不管值域）。
pub const PLACEHOLDER_MECHANISMS: &[&str] = &[
    // 中文：三型阴性的"原话形状"（共现 / 时间相邻 / 同实体）＋说不出事情的占位词
    "无", "暂无", "待定", "未知", "相关", "有关系", "有关联", "相互关联", "因为它们相关",
    "同一实体", "同一个实体", "同现实体", "同一个东西", "同一轮", "同轮", "同一轮里",
    "同一周", "同周", "同一会话", "同时出现", "一起出现", "共现", "时间相邻", "时间相邻导致",
    "相邻", "先后发生", "时间顺序", "顺带提及", "一起被提到",
    // 英文：同一批形状 + 占位词（模型答英文时用的就是这几个词）
    "none", "n/a", "na", "todo", "tbd", "unknown", "co_occurs", "co-occurrence", "cooccurrence",
    "related", "they are related", "because they are related", "same entity", "same thing",
    "same turn", "same week", "same session", "mentioned together", "appear together",
    "adjacent in time", "time adjacency", "temporal adjacency", "in order", "before", "after",
];

/// 一句"机制"是不是套话（含空白）。整串比对，英文按 `to_ascii_lowercase` 归一（`check_alias` 同写法）。
pub fn is_placeholder_mechanism(raw: &str) -> bool {
    let t = raw.trim();
    if t.is_empty() { return true; }
    if PLACEHOLDER_MECHANISMS.contains(&t) { return true; }
    let low = t.to_ascii_lowercase();
    PLACEHOLDER_MECHANISMS.contains(&low.as_str())
}

/// 机制类关系的"证据"缺不缺——**两道判据共用的那一句**（模型面 `rc-derive::parse` 与门口
/// `put_edges_inner` 都调它，两层不许各写一套）：
/// ① 交不出不是套话的机制句，或 ② 一条依据行都没点名（`basis_ids` 为空）。
/// 端点那两行 `role='basis'` 出处**不算依据**：它们说的是"这条边连着谁"，不是"这条边凭什么"。
pub fn lacks_mechanism_evidence(relation: &str, mechanism: &str, named_basis_rows: usize) -> bool {
    MECHANISM_RELATIONS.contains(&relation)
        && (is_placeholder_mechanism(mechanism) || named_basis_rows == 0)
}

/// 批次上下文：P 独占列里那两列的唯一来源。它存在的原因是守卫 11——模型递不进这两列，
/// 而抽取作业必须能把"这批来自哪台设备、哪个 agent"递进写入口。
#[derive(Debug, Clone)]
pub struct BatchCtx {
    pub source_agent: String,
    pub device_id: String,
}

impl BatchCtx {
    /// 今天 `p_stamps` 里那两个常量的家。合成夹具与 `synth.rs` 用它，语义与阶段 1 逐字节相同。
    pub fn synthetic() -> Self {
        BatchCtx { source_agent: "synthetic-agent".into(), device_id: "dev-synthetic-1".into() }
    }
}

/// 一批待写的行。`table` 而不是两个变体：两张表的写入骨架在 `write_fact` 里本来就是一份。
#[derive(Debug, Clone)]
pub struct WriteItem {
    pub table: MemTable,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub scope: Scope,
    pub valid_to_ms: Option<i64>,
    pub aliases: Vec<String>,
    pub prov: Vec<Provenance>,
}

/// P 独占列的唯一来源。X 的入参结构体里根本没有这些字段——这就是 §2 那句
/// "`assert_writable(列, actor)` 降级成写不出这个函数"。
/// 合成期与真批次的两条路：真批次从 `put_batch` 的 `BatchCtx` 形参位经 `from_ctx` 带进
/// source_agent / device_id；合成夹具与旧四把 `put_*` 走 `BatchCtx::synthetic()`。
struct PStamps {
    source_agent: String,
    device_id: String,
    norm_version: i64,
}

impl PStamps {
    fn from_ctx(ctx: &BatchCtx) -> PStamps {
        PStamps {
            source_agent: ctx.source_agent.clone(),
            device_id: ctx.device_id.clone(),
            norm_version: rc_store::capability::NORM_VERSION as i64,
        }
    }
}

fn p_stamps(_now_ms: i64) -> PStamps {
    PStamps::from_ctx(&BatchCtx::synthetic())
}

/// `card` 的唯一插入形状。抽取行的 `causal_type` 与三锚列都写空值：抽取事件**没有**
/// 可重算的锚（模型只引用 turn，不产出时刻）⇒ 三锚列写 `NULL, NULL, 'unknown'`
/// （裁定 13 的"无锚就写 unknown"，I3：不许拿导入钟冒充事件时刻），`causal_type` 留 NULL。
/// 时间线/链头那两族需要真锚，走各自的 INSERT 字面量（同一张表、同一组列）。
/// 分层可信度（Task 7）：那一族各自的列单里带 `mode='inferred'`，本形状**不写 `mode`**、
/// 吃列的 DEFAULT `'stated'`（`ddl.rs:487`）⇒ 原文直述与推导产物在库里可以被读者分辨。
fn card_sql() -> String {
    "INSERT INTO card(id, kind, title, text, scope_kind, scope_key, as_of_ms, valid_to_ms,
                      source_agent, device_id, created_at_ms, norm_version,
                      causal_type, time_anchor_ms, time_anchor_id, time_confidence)
     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,NULL,NULL,NULL,'unknown')
     ON CONFLICT(id) DO NOTHING".to_string()
}

/// 写侧校验（在任何一个事务开始**之前**）：ReadOnly 由调用方查句柄，其余四项只查内容。
/// 判据顺序与阶段 1 逐字节相同：出处 → 标题非空 → 别名闸门 → 标题即别名。
/// `ProvenanceMissing` 的载荷仍是表前缀（`&'static str`），不改错误形状（守卫 12 的下游读者）。
fn validate_fields(table: MemTable, title: &str, aliases: &[String], prov: &[Provenance])
    -> Result<(), MemError> {
    if prov.is_empty() { return Err(MemError::ProvenanceMissing(table.prefix())); }
    if normalize_body(title).is_empty() { return Err(MemError::ContentEmpty); }
    for a in aliases.iter() { check_alias(a)?; }
    check_alias(title)?;                       // 标题本身就是一条别名（守卫 10 的第二入口）
    Ok(())
}

/// 单条的 SQL 段：行 + 出处 + 别名 + **归属**，收 `&Transaction`（**不自己开事务**）。
/// 为什么必须收事务而不是在外面套一层：rusqlite 在同一连接上开第二个事务会走到 SQLite 的
/// `BEGIN` 重入错误——"外层整批事务 + 逐条 `put_*`"这个形状根本不存在，逐条写会全红。
/// 校验不住在这里：§2.13 定死"校验在前、SQL 收 `&Transaction`"两半。
///
/// 归属行（S1b-C3）与内容三件事**同一个事务**：任何一步失败整批回滚，不存在"行在、归属不在"。
/// owner/principal/visibility/retention 四格从 `OwnerCtx`（会话）来，但 `created_by_agent` 与
/// `source_agent`/`device_id` 仍是 P 独占列（`PStamps`）的副本——模型与内容形状都递不进它们。
/// `insert` 返回 false（已有归属行）**不报错、不覆盖**：先写者赢，冲突审计留给 C4。
#[allow(clippy::too_many_arguments)]
fn write_item_in(tx: &rusqlite::Transaction, table: MemTable, kind: &str, title: &str,
                 body: &str, scope: &Scope, valid_to_ms: Option<i64>, aliases: &[String],
                 prov: &[Provenance], p: &PStamps, row_ms: i64,
                 owner: &OwnerCtx) -> Result<MemKey, MemError> {
    let id = compute_mem_id(table, kind, title, body, scope);
    let key = MemKey { table, id: id.clone() };
    if table == MemTable::Anchor {
        // I3：`created_at_ms` / `updated_at_ms` 都是源轮时刻（`row_ms`），不是导入钟。
        // 锚没有 `as_of` 列；canonical 就是标题，reality_status/status 取下界。
        tx.execute("INSERT INTO anchor(id, kind, canonical, reality_status, world_id,
                    scope_kind, scope_key, status, profile_text, created_at_ms, updated_at_ms,
                    norm_version) VALUES(?1,?2,?3,'unknown',NULL,?4,?5,'active',NULL,?6,?6,?7)
                    ON CONFLICT(id) DO NOTHING",
            rusqlite::params![id, kind, title, scope.kind.as_str(), scope.key,
                              row_ms, p.norm_version])?;
    } else {
        // I3：两列都取源轮时刻（`row_ms`）。`as_of_ms` 的语义是"这条何时成立"，
        // `created_at_ms` 的语义是"这条何时入库"——本任务按裁定统一成源轮时间，
        // 不再用导入钟（审计 I3 的 9 天偏移）。
        tx.execute(&card_sql(), rusqlite::params![id, kind, title, body,
            scope.kind.as_str(), scope.key, row_ms, valid_to_ms,
            p.source_agent, p.device_id, row_ms, p.norm_version])?;
    }
    for pv in prov {
        tx.execute("INSERT OR IGNORE INTO provenance(mem_table, mem_id, turn_uid, anchor_role)
                    VALUES(?1,?2,?3,?4)",
            rusqlite::params![table.prefix(), id, pv.turn_uid, pv.anchor_role.as_str()])?;
    }
    for a in aliases.iter().filter(|_| table == MemTable::Anchor) {
        tx.execute("INSERT OR IGNORE INTO alias(anchor_id, alias) VALUES(?1,?2)",
                   rusqlite::params![id, a])?;
    }
    // 归属：行 + 出处 + 别名之后、返回句柄之前。visibility 再过一次上界（行 scope 是天花板）。
    owner::insert(tx, &owner::OwnerRow {
        mem_table: table.prefix().to_string(),
        mem_id: id.clone(),
        owner_principal_id: owner.owner_principal_id.clone(),
        owner_agent_id: owner.owner_agent_id.clone(),
        created_by_agent: p.source_agent.clone(),
        visibility: owner::effective(owner.visibility, scope.kind),
        retention: owner::normalize_retention(&owner.retention),
        scope: scope.clone(),
        source_agent: Some(p.source_agent.clone()),
        device_id: Some(p.device_id.clone()),
        created_at_ms: row_ms,
    })?;
    Ok(key)
}

/// 四个公开 `put_*` 的公共骨架。形参多到 10 个是简报给的形状，不是这里选的：
/// 内容与出处、别名、时间戳各是一组，拆开成结构体反而让"哪个字段进哪一列"变成读不出来的事。
/// 归属走 `OwnerCtx::synthetic(署名 agent)`（C3）：旧四把单条写没有会话可问，
/// 默认 owner=署名 agent、private、default。
#[allow(clippy::too_many_arguments)]
fn write_fact(m: &MemoryHandle, table: MemTable, kind: &str, title: &str, body: &str,
              scope: &Scope, valid_to_ms: Option<i64>, aliases: &[String],
              prov: &[Provenance], now_ms: i64) -> Result<MemKey, MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    validate_fields(table, title, aliases, prov)?;
    let p = p_stamps(now_ms);
    let owner = OwnerCtx::synthetic(&p.source_agent);
    let tx = m.conn().unchecked_transaction()?;   // 一个事务：行 + 出处 + 别名 + 归属，任何一步失败整条回滚
    let key = write_item_in(&tx, table, kind, title, body, scope, valid_to_ms,
                            aliases, prov, &p, now_ms, &owner)?;
    tx.commit()?;
    Ok(key)
}

/// **整批一个事务**：任何一条失败 ⇒ 一条都不写（`Err` 时库里没有半批内容）。
/// 这把 `write_fact` 的"行 + 出处 + 别名"单事务扩成"一批"，是 §7"缺一整批丢弃"在写入侧的兑现。
///
/// **写前查账**（守卫 13 在本阶段唯一的牙）：每条在插入前读 `reader::dead_by_id_expr`
/// 那一支三支齐全的表达式（未解决退场账 / 未被取代 / 未解决否决，P-31），命中 ⇒ 跳过不写。
/// 内容寻址的 id 在这里才算得出来（抽取侧手上只有 `Draft`），所以这条查账不可能上移到 rc-distill；
/// 但条件文本一家都不在这里写——三支分别住 `reader::retirement_pred` / `superseded_pred` /
/// `rejection_pred`，与候选筛、审核游标筛同一个家（守卫 3）。
/// 写侧要的是 `dead_by_id_expr` 而不是读者的 `alive_where`：两本账按 id 查、与行在不在无关，
/// 理由与代价都写在 `reader.rs` 那一格的注释里。
/// 返回的是**与 `items` 逐格对齐的账本**：写成了给 `Some(key)`、被抑制给 `None`——
/// 不是"短的 Vec"，短的会让调用方的 `zip` 静默错位，把 A 条的出处记到 B 条上。
pub fn put_batch(m: &MemoryHandle, items: &[WriteItem], ctx: &BatchCtx, now_ms: i64)
    -> Result<Vec<Option<MemKey>>, MemError> {
    put_batch_at(m, items, ctx, now_ms, &[])
}

/// `put_batch` 的带源时刻版本（I3）：`item_times` 与 `items` **逐格对齐**，
/// `Some(ms)` = 该行的 as_of/created_at 用这个源轮时刻；`None` 或越界 = 回落 `now_ms`。
/// 为什么不是给 `WriteItem` 加一格：那个形状的调用者不止抽取作业（agent 侧夹具也造它），
/// 加必填字段会把"时间"这件事摊到每个内容构造点；写成旁路切片则"谁有源时刻"只在抽取层回答。
///
/// C3 起它是 [`put_batch_with_owner`] 的**薄包装**：没有会话可问的调用者拿合成默认
/// （owner=ctx.source_agent、private、default）。中心抽取走带 owner 的那一支。
pub fn put_batch_at(m: &MemoryHandle, items: &[WriteItem], ctx: &BatchCtx, now_ms: i64,
                    item_times: &[Option<i64>]) -> Result<Vec<Option<MemKey>>, MemError> {
    let owner = OwnerCtx::synthetic(&ctx.source_agent);
    put_batch_with_owner(m, items, ctx, &owner, now_ms, item_times)
}

/// 带归属上下文的整批写（S1b-C3 的唯一新入口）：主体与 [`put_batch_at`] 逐行相同，
/// 差别只在每一行多传一个 `owner`。归属与行/出处/别名**同一个事务**，
/// 抑制（dead 命中）的行不写行、因此也不写归属。
pub fn put_batch_with_owner(m: &MemoryHandle, items: &[WriteItem], ctx: &BatchCtx,
                            owner: &OwnerCtx, now_ms: i64, item_times: &[Option<i64>])
    -> Result<Vec<Option<MemKey>>, MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    // 校验在任何一条写入、任何事务开始之前：一条坏标题 = 整批丢弃，而不是"遇到才停"。
    for it in items {
        validate_fields(it.table, &it.title, &it.aliases, &it.prov)?;
    }
    let p = PStamps::from_ctx(ctx);
    let tx = m.conn().unchecked_transaction()?;
    let mut slots: Vec<Option<MemKey>> = Vec::with_capacity(items.len());
    for (i, it) in items.iter().enumerate() {
        let row_ms = item_times.get(i).copied().flatten().unwrap_or(now_ms);
        let id = compute_mem_id(it.table, &it.kind, &it.title, &it.body, &it.scope);
        // 从 `tx` 上查，不从 `m.conn()` 另开一条读：那会把查账移出这个"整批一个事务"，
        // 批内前一条写下的行与后一条的查账就会看到两个不同快照。
        // 命中=该死：幽灵账（有账无行）1、活行 0、死行 1、什么都没有 0，只有读到 1 才跳过。
        // 数反了就是复活；把两本账改成"与行同查"照样能编译，只是查不到幽灵账那一种（见 reader 那一格）。
        let sql = format!("SELECT {}", crate::reader::dead_by_id_expr(it.table));
        let dead: bool = tx.query_row(&sql, rusqlite::params![&id], |r| r.get(0))?;
        if dead {
            slots.push(None);                    // 命中抑制账：不写行，也不追加出处（因此也不写归属）
            continue;
        }
        let key = write_item_in(&tx, it.table, &it.kind, &it.title, &it.body, &it.scope,
                                it.valid_to_ms, &it.aliases, &it.prov, &p, row_ms, owner)?;
        slots.push(Some(key));
    }
    tx.commit()?;
    Ok(slots)
}

pub fn put_entity(m: &MemoryHandle, c: &EntityContent, prov: &[Provenance], now_ms: i64)
    -> Result<MemKey, MemError> {
    // anchor 表没有正文 / as_of / valid_to 这些列，所以正文传空串、保质期传 None：
    // 它们在 reader 那一侧读回来就是 "" 与 None。
    write_fact(m, MemTable::Anchor, &c.kind, &c.title, "", &c.scope, None, &c.aliases, prov, now_ms)
}

/// 候选行共用同一个骨架，差别只有一个 `kind`（表已经是同一张 `card`）。
fn put_fact(m: &MemoryHandle, table: MemTable, c: &FactContent, prov: &[Provenance], now_ms: i64)
    -> Result<MemKey, MemError> {
    write_fact(m, table, &c.kind, &c.title, &c.body, &c.scope, c.valid_to_ms, &[], prov, now_ms)
}

pub fn put_card(m: &MemoryHandle, c: &FactContent, prov: &[Provenance], now_ms: i64)
    -> Result<MemKey, MemError> {
    put_fact(m, MemTable::Card, c, prov, now_ms)
}

/// 波三刀 A：**带会话归属**的直接写卡入口（`kind='lesson'` 的家）。
///
/// 为什么不是复用 [`put_card`]：那一把走 `OwnerCtx::synthetic`（owner=署名 agent、private），
/// 而 lesson pass 手里真有会话的归属上下文（`sessions` 的 owner/visibility/retention/scope）。
/// 走合成默认会把"这条经验属于谁"写成 `rc-distill` 的私有行，读闸会把它对真读者拒掉——
/// 与派生行断线那次（`writers_x` 里 BasisOwner 的派发档 I-3e 注释）同一形状的缺陷。
///
/// 为什么不是复用 fact 写口（`put_fact_rows*`）：那一族必然同时写 `fact` 行与镜像卡，
/// 而 lesson 不是 fact 类事实（它没有断言、没有宾语、没有时间锚），借它的表等于把
/// "经验卡"伪装成"事实行"。这里只写 `card` + `provenance` + `resource_owner` 三表，
/// 与 [`put_card`] 的落点逐表相同，差别只有归属与 P 独占列的来源。
///
/// 返回 `(卡句柄, 真插入行数)`：`0` = 同一内容寻址 id 已在库里（幂等命中，不是错误）。
pub fn put_card_with_owner(
    m: &MemoryHandle,
    c: &FactContent,
    prov: &[Provenance],
    ctx: &BatchCtx,
    owner: &OwnerCtx,
    now_ms: i64,
) -> Result<(MemKey, i64), MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    validate_fields(MemTable::Card, &c.title, &[], prov)?;
    let p = PStamps::from_ctx(ctx);
    let tx = m.conn().unchecked_transaction()?;
    // 幂等命中要在**写入之前**读：`ON CONFLICT(id) DO NOTHING` 的返回值不区分
    // "刚插入"与"已有"，而写库读数（`cards=`）必须只数真插入的那些。
    let id = compute_mem_id(MemTable::Card, &c.kind, &c.title, &c.body, &c.scope);
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM card WHERE id=?1)",
        rusqlite::params![&id],
        |r| r.get(0),
    )?;
    let key = write_item_in(&tx, MemTable::Card, &c.kind, &c.title, &c.body, &c.scope,
                            c.valid_to_ms, &[], prov, &p, now_ms, owner)?;
    tx.commit()?;
    Ok((key, if exists { 0 } else { 1 }))
}

/// 旧 `event` 消费行：canonical 之后与 card 同落一张表（`kind` 保留调用方给的值）。
pub fn put_event(m: &MemoryHandle, c: &FactContent, prov: &[Provenance], now_ms: i64)
    -> Result<MemKey, MemError> {
    put_fact(m, MemTable::Card, c, prov, now_ms)
}

/// 旧 `assertion` 消费行：同上，落 `card`。
pub fn put_assertion(m: &MemoryHandle, c: &FactContent, prov: &[Provenance], now_ms: i64)
    -> Result<MemKey, MemError> {
    put_fact(m, MemTable::Card, c, prov, now_ms)
}

// 只补 `AnchorRole` 一个名字：本文件顶部的 import 已经把
// `MemKey` / `MemTable` / `Scope`（`use crate::keys::{..}`）与 `MemError`（`use crate::{..}`）导进来了，
// 重复 import 是 E0252 而不是"多写无害"。
use crate::keys::AnchorRole;

/// `provenance.role` 的两个值。值域的家是 `crate::keys::PROVENANCE_ROLES`（两个 slug 逐字取自它，
/// DDL 侧的字面量与它对拍见 rc-memory 的 `ddl_shape`），写侧**只许**插这两个词。
/// 为什么必须钉成常量而不是裸字符串：那一列的跨列 CHECK 只管 role↔ord 的耦合、**不管值域**
/// ⇒ 拼错的词（`"inks"`）落得进库、也读得回来，库自己不会红；所以值域这一半只能由写侧把守，
/// 执法用例见 rc-memory 的 `put_chains`（`SELECT DISTINCT role` 那一格）。
const ROLE_BASIS: &str = "basis";
const ROLE_LINK: &str = "link";

// ── 派发档 I-3e · FIX-1：派生行的归属**继承**自依据行 ──────────────────────────────
//
// 断线的形状（台账「Phase 2.5 现网验收」）：derive 写下的链卡 `source_agent='rc-derive'`、
// `resource_owner` 零行 ⇒ `gate.rs` 的 `GateIndex::decide` 第 3 条只剩"行的署名恰等于读者"那一支 ⇒ 对任何真实
// 读者（`qoder`）一律 `Deny::NoOwnerRow`，句柄臂把它渲染成 `empty:row_absent`（库里有这一行）。
// 修法按控制器裁定：**继承归属，不发明归属，也不放宽闸**——本轮 `gate.rs` 与 `reader.rs`
// 零改动，派生行只是带上它本来就该有的归属证据，方向与 `export.rs` 的 `render_export`（边的可见性
// 跟随两端）同一条。

/// 依据行在 `resource_owner` 里被继承要用到的那几格。身份是 `(owner_principal_id,
/// owner_agent_id)` 这一对——那正是 `owner::existing_owner` 用来判"是不是同一个 owner"的
/// 比较面（visibility/scope 不参与判等，只参与取值），拿别的面判等就是另造一套归属身份。
struct BasisOwner {
    principal: Option<String>,
    agent: String,
    visibility: Visibility,
    retention: String,
}

/// `Visibility` 的保守序（closed set）：`private_agent < person < project < org < public`。
/// 序的**唯一实现点**是 `owner::rank`，而它是 `owner.rs` 的私有函数、`owner.rs` 不在本轮允许集里，
/// 所以这里只在"取最保守的一档"这一处按枚举顺序过一遍——形状与分寸逐字照
/// `hub/src/ownership.rs` 的 `VISIBILITY_BY_CONSERVATISM` + `most_conservative`
/// （同一条理由：不复制解析与落库字面量，那才是 `owner` 的事）。
const VISIBILITY_BY_CONSERVATISM: [Visibility; 5] = [
    Visibility::PrivateAgent,
    Visibility::Person,
    Visibility::Project,
    Visibility::Org,
    Visibility::Public,
];

/// 取最保守的一档。`parse_visibility` 只产出上面那五个值 ⇒ 恒能命中一格；兜底
/// `private_agent` 挡的是"日后有人加变体"，方向仍是收紧而不是放宽。
fn most_conservative(values: &[Visibility]) -> Visibility {
    for candidate in VISIBILITY_BY_CONSERVATISM {
        if values.contains(&candidate) {
            return candidate;
        }
    }
    Visibility::PrivateAgent
}

/// 一条派生行的归属继承结果。**没有第三个值**：要么继承到并写上归属行，要么整条不写
/// （fail-closed，与"归属分歧"同一格），后者由写入口计进 `owner_ambiguous`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Inheritance {
    /// 依据行的归属唯一 ⇒ `resource_owner` 已按继承写上（或先写者的行本来就在）。
    Inherited,
    /// 依据行的归属不唯一，**或**取不到归属证据（依据行没有归属行 / id 根本不在有归属面的
    /// 那三张表里）⇒ **不写**归属行，读者照旧看不见这一行。
    Ambiguous,
}

/// 读一个依据句柄的归属行。归属表的主键是 `(mem_table, mem_id)`，而依据行的**表名**不在
/// `ChainContent`/`TimelineContent`/`EpisodeContent` 里（程序现算列，模型与调用方都递不进），
/// 所以按 id 一次问过"有归属面的那三张表"：`card` + `anchor`（`hub/src/ownership.rs` 的 `mem_rows`
/// 钉住的行集，也是 `MemTable` 的两张可指认表）与 `fact`（episode 成员所在的表，
/// `views.rs` 的 `visible` 就按 `allows("fact", …)` 读它）。
/// 零行 ⇒ 这一条依据**没有归属证据**。表名写成字面量清单与本文件既有写法同族
/// （`insert_derived_provenance(&tx, "card", …)`、`"fact"` 作为 `ref_kind`），
/// `state_assertion` 不在列内：它今天不可能是这三族的依据行，写进清单就是一句没有生产者的话。
fn basis_owners(tx: &rusqlite::Transaction, basis_id: &str)
    -> Result<Vec<BasisOwner>, MemError> {
    let mut st = tx.prepare(
        "SELECT owner_principal_id, owner_agent_id, visibility, retention
           FROM resource_owner
          WHERE mem_table IN ('card', 'anchor', 'fact') AND mem_id=?1")?;
    let rows = st.query_map([basis_id], |r| {
        Ok(BasisOwner {
            principal: r.get::<_, Option<String>>(0)?,
            agent: r.get::<_, String>(1)?,
            visibility: owner::parse_visibility(&r.get::<_, String>(2)?),
            retention: r.get::<_, String>(3)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 把一张派生卡（恒为 `card` 表，`mem_id` 是它的句柄）的归属**继承**自 `basis_ids` 点名的
/// 依据行。三条判据按控制器裁定逐字落：
/// ① 所有依据行的身份 `(principal, agent)` **全等**才继承；
/// ② 不唯一 ⇒ `Inheritance::Ambiguous`，一行归属都不写（调用方计 `owner_ambiguous`）；
/// ③ `visibility` 取依据行中最受限的一档，再照 `writers_fact.rs` 的 `put_fact_rows_with_extras` 的既有做法过一次
///    scope 天花板（`owner::effective(...)`，天花板按**派生行自己**的 scope）。
/// `retention` 沿用 `hub/src/ownership.rs` 的 `derived_row` 那条分寸：各依据行一致才继承、
/// 否则 `default`——它不是可见性轴，这里不发明新规则。
/// `scope` / `created_at_ms` 取派生行自己的值，`created_by_agent` 与 producer 两列取
/// [`BatchCtx`]（守卫 11：P 独占列递不进内容形状）。
///
/// 写入只有 `owner::insert` 一个出口（`INSERT OR IGNORE`、先写者赢、永不覆盖既有归属），
/// 所以这里既不新增出处 SQL 的家（判据 52③ 把 src 面钉成恰好 7 处），也不会把谁已有的归属改宽。
///
/// 为什么"依据行不存在"也走 `Ambiguous` 而不是整批 `Err`：本文件今天对
/// `TimelineContent::basis_ids` 不做存在性校验（它只进 `evidence_json`），把这一格升级成
/// 报错是**改判据**，不是修归属；取不到证据 ⇒ 不写归属 ⇒ 读者读不到，与分歧同一形状。
///
/// **只在"本次真的写下了这一行"的那条路径上调用**。三族的重放分支都是
/// `INSERT OR IGNORE` 命中既有句柄 ⇒ `continue`，那里**不**补归属行：把补写既有行的活儿
/// 塞进写入口，等于让"重跑一个字节都不许多"那三条既有幂等判据换口径，而它并解决不了实际
/// 要迁移的那一形（链族在 `n_ins` 之前就被 I5 的去重判据挡掉，根本走不到补写那一支）。
/// 库里已有的派生行怎么补上归属，登记在交付报告的「未解决」第一条。
fn inherit_card_owner(
    tx: &rusqlite::Transaction, ctx: &BatchCtx, mem_id: &str, basis_ids: &[String],
    scope: &Scope, row_ms: i64,
) -> Result<Inheritance, MemError> {
    // 零条依据行 ⇒ 没有任何归属可以继承（不是"继承到一个空串"）。链/时间线/镜像卡三族
    // 的合法输入都至少点名一行依据，所以这一支挡的是绕过上游判据的直接调用者。
    if basis_ids.is_empty() {
        return Ok(Inheritance::Ambiguous);
    }
    let mut identity: Option<(Option<String>, String)> = None;
    let mut visibilities: Vec<Visibility> = Vec::new();
    let mut retentions: BTreeSet<String> = BTreeSet::new();
    for basis_id in basis_ids {
        let owners = basis_owners(tx, basis_id)?;
        if owners.is_empty() {
            return Ok(Inheritance::Ambiguous);
        }
        for owner in owners {
            // 空串不是任何一个 agent 的名字（`producer_is_viewer` 同一条判据）。
            if owner.agent.is_empty() {
                return Ok(Inheritance::Ambiguous);
            }
            let this = (owner.principal.clone(), owner.agent.clone());
            match &identity {
                None => identity = Some(this),
                Some(prev) if *prev != this => return Ok(Inheritance::Ambiguous),
                Some(_) => {}
            }
            visibilities.push(owner.visibility);
            retentions.insert(owner::normalize_retention(&owner.retention));
        }
    }
    // 走到这里 identity 必是 `Some`（每条依据行都取到了至少一行归属）。留这一支是把
    // "没有归属证据"与"归属分歧"合到同一格出口，而不是新增第三种结果。
    let Some((owner_principal_id, owner_agent_id)) = identity else {
        return Ok(Inheritance::Ambiguous);
    };
    let retention = if retentions.len() == 1 {
        retentions.into_iter().next().unwrap_or_else(|| "default".to_string())
    } else {
        "default".to_string()
    };
    owner::insert(tx, &owner::OwnerRow {
        mem_table: MemTable::Card.prefix().to_string(),
        mem_id: mem_id.to_string(),
        owner_principal_id,
        owner_agent_id,
        created_by_agent: ctx.source_agent.clone(),
        visibility: owner::effective(most_conservative(&visibilities), scope.kind),
        retention,
        scope: scope.clone(),
        source_agent: Some(ctx.source_agent.clone()),
        device_id: Some(ctx.device_id.clone()),
        created_at_ms: row_ms,
    })?;
    Ok(Inheritance::Inherited)
}

/// 边内容（与 FactContent 平行，不入 MemTable 变体——边不是可指认句柄，裁定 3 原文）。
/// **没有 `source_agent` / `device_id` 两个字段**：本模块的文件头写明"P 独占列里那两列的
/// 唯一入口是 `BatchCtx`"，`FactContent`/`EntityContent`/`WriteItem` 因此都不带这两列；
/// 边与时间行也不能例外，否则守卫 11 在这一处是纸面的。`from_kind`/`to_kind` 同理不在这里——
/// 它们是程序现算列（`table_of()`），模型与调用方都递不进。
pub struct EdgeContent {
    pub from_id: String,
    pub to_id: String,
    pub kind: String,       // ∈ 五项封闭枚举
    /// 这条边"凭什么"的那一句话，**逐字进 `fact_link.basis`**（P-1：证据落那一格——
    /// 它 NOT NULL、有人读；`evidence_json` 那一格被下面的 INSERT 写死 `'[]'`，
    /// 而读侧 `reader::edges`/`EdgeFacts` 根本不读它，把闸建在那一格就是第二次 `card.time_json`）。
    /// 机制类 relation（[`MECHANISM_RELATIONS`]）由证据闸要求它非空白且不是套话；
    /// 结构归属那一支不要求（P-4）。
    pub reason: String,
    pub scope: Scope,
    /// 这条边凭什么（parse 已解析成真实行 id）。每条都写一行 provenance（自己的角色），
    /// 与两端一起构成"边可回读的依据"；同一句柄撞主键时 `INSERT OR IGNORE` 吞掉。
    /// 机制类 relation 还要求它**非空**（[`lacks_mechanism_evidence`] 那一支）。
    pub basis_ids: Vec<String>,
}

/// 时间线内容（落 fact 行 kind='timeline'，三锚列同事务）
pub struct TimelineContent {
    pub title: String,
    pub body: String,
    pub scope: Scope,
    pub time_anchor_ms: Option<i64>,
    pub time_anchor_id: Option<String>,  // 被锚行 id（跨表）
    pub time_confidence: String,         // 'anchored' | 'unknown'
    /// 这条时间行凭什么（parse 已解析成真实行 id）。它进 `event_link.evidence_json`；
    /// **不写 provenance**——无锚时间行的"出处零行"是被既有用例钉住的形状（裁定 13），
    /// 依据行的署名由软边的 evidence 承载，两件事不许混。
    pub basis_ids: Vec<String>,
}

/// 因果链内容（批次③ · 落 `event(kind='causal')` 一行 + `provenance` 的环节/依据行）。
/// `link_ids` 是**有序**环节列表：顺序本身就是那条链的语义（`A→B` 与 `B→A` 是两条链），
/// 所以它不许是集合、也不许在写侧重排。`basis_ids` 是"这条链凭什么"，不带序。
/// 与 `EdgeContent`/`TimelineContent` 同一条纪律：**没有 `source_agent`/`device_id` 两格**
/// （守卫 11——P 独占列只能从 `BatchCtx` 进写入口），`ref_kind` 与 `anchor_role` 也不在这里
/// （它们是程序现算的读数，模型与调用方都递不进）。
pub struct ChainContent {
    pub mechanism: String,
    pub link_ids: Vec<String>,
    pub basis_ids: Vec<String>,
    /// 链头类型：三值 ∈ causes/enables/prevents。parse 已验值域；写侧不再判，
    /// 库里的 `CHECK(causal_type IN (...))` 是最后一道。
    pub causal_type: String,
    pub scope: Scope,
}

/// 一条 mention（行提到实体）。`row_id` 是事实行 id，`name` 是模型逐字摘的名字；
/// 名字→实体 id 的解析归本入口（多义落全部，零匹配不计写入并计 unresolved）。
/// 与 `EdgeContent`/`TimelineContent`/`ChainContent` 同一条纪律：**没有 `source_agent`/`device_id`**
/// （守卫 11——P 独占列只能从 `BatchCtx` 进写入口），`from_kind` 也不在这里（`table_of()` 现算）。
pub struct MentionContent { pub row_id: String, pub name: String }

/// 一段经历的 X 内容（派发档 I-3a 交付 2）。
///
/// **只装三格**：标题、叙事、有序成员 fact id。其余都不在这儿，因为它们是程序读数而不是内容：
/// `status`（新行恒 `'open'`；DDL 允许四值，另两值今天零生产者——登记与"缺什么证据才能填"
/// 都写在 [`put_episodes`] 的文档里，别在这一格当"生命周期已纳管"读）、`superseded_by`（本次顶掉了谁）、`start_ms`/`end_ms`
/// （成员 `occurred_at_ms`/`occurred_end_ms` 的包络，母档 D4-补：不从镜像卡 `as_of_ms` 反取）、
/// `participants_json`（成员 `participants_json` 的并集）、`as_of_ms`/`time_confidence`
/// （[`episode_time_arm`] 那一格）、`scope_*`/`world_id`（成员行的归属，也是"不跨 scope"的执法处）、
/// `source_agent`/`device_id`（[`BatchCtx`]，守卫 11）、`norm_version`（常量）。
/// 这条分工与 `EdgeContent`/`TimelineContent`/`ChainContent` 同一个形状：调用方递不进程序现算列。
///
/// **为什么这里没有 `episode_id`**：句柄是算出来的（[`compute_episode_id`]），递一个 id 进来就是
/// 给"同一段经历的两个 id"开口子（P-44 那一族"读数没有生产者"的镜像）。
pub struct EpisodeContent {
    pub title: String,
    pub narrative: String,
    /// 成员 fact id：它们经**准入**与**事件序**之后整个进句柄的 preimage（派发档 I-3e · FIX-2：
    /// 修前这里是"调用方那一序直接进 preimage"，于是被拦掉的成员也照样进了句柄 ⇒ 同一个 id 对应
    /// 两套内容，见 [`put_episodes`] 的 ① 那一格）。
    /// 顺序仍然有意义，但意义换了：它决定"点名了哪几条"（以及哪几条被拦后计进
    /// `dropped_members`），**不**决定句柄——句柄跟着库里的内容走（D3 的"成员变则句柄变"）。
    /// 用例见 `tests/episode.rs` 的 `成员排序已知在前未知在后且两次一致`、
    /// `句柄由归属与有序成员算出_换序即换句柄_卡与行同源不碰撞`（那一条断的是哈希本身含序）与
    /// `句柄取准入后按事件序的成员_少一个成员就换一个句柄`（FIX-2 的那一条）。
    pub fact_ids: Vec<String>,
}

/// 一条链批次（[`put_chains_merged_with_owner_account`]）的账本。**三格都是本次真的插进去的
/// 行数/条数**，不是计划条数（重放全 0），与 [`EpisodeWriteOutcome`] 同一条口径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChainWriteAccount {
    /// 落库的链头行数（= 既有 `put_chains_merged` 的第一格）。
    pub chains: usize,
    /// 被 I5 的重复/复述判据合并掉的条数（= 既有 `put_chains_merged` 的第二格）。
    pub merged: usize,
    /// I-3e · FIX-1：本次写下的链头卡里，依据行归属不唯一（或取不到归属证据）
    /// 因而**没有**写上归属行的条数。
    pub owner_ambiguous: usize,
}

/// 一批时间线（[`put_timeline_rows_with_owner_account`]）的账本，同上口径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimelineWriteAccount {
    /// 落库的时间线卡行数（= 既有 `put_timeline_rows_counted` 的第一格）。
    pub rows: usize,
    /// 本次插入的 `event_link` 旁路行数（= 既有 `put_timeline_rows_counted` 的第二格，R3）。
    pub links: usize,
    /// I-3e · FIX-1：本次写下的时间线卡里没有写上归属行的条数。
    pub owner_ambiguous: usize,
}

/// 一次 episode 写入的账本。**每一格都是插入/更新的行数**，不是计划条数（重放全 0）。
///
/// 两道闸各有出口（P-5：被闸挡下的条数不许在库里蒸发）：
/// `dropped_members` = 被准入拦下的成员数——**两种挡因共用这一格**（无出处 / 已判死，
/// I-3d MF-2），新开一格就是把"拦了几条"变成两个口径；`dropped_episodes` = 成员被拦空了而没有落成的
/// episode 数。`superseded`/`superseded_cards` 是本次顶掉的旧结构行与旧镜像卡数（D3 的收尾，
/// 旁路写入必须有读数出口，R3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EpisodeWriteOutcome {
    pub episodes: usize,
    pub members: usize,
    pub cards: usize,
    pub mentions: usize,
    pub provenance: usize,
    pub superseded: usize,
    pub superseded_cards: usize,
    pub dropped_members: usize,
    pub dropped_episodes: usize,
    /// I-3e · FIX-1：本次写下的镜像卡里，成员行的归属不唯一（或成员根本没有归属行）
    /// 因而**没有**写上归属行的条数。归属不唯一 ⇒ 不写（fail-closed），不发明一个主人。
    /// 口径与其余格一致：**本次真的写下的**才计，重放全 0。
    pub owner_ambiguous: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MentionOutcome {
    /// 实际插入的 mention 行数（多义名字落多行时按行计；`INSERT OR IGNORE` 命中已有 ⇒ 0）。
    pub written: usize,
    /// 零匹配的 mention 条数（名字解析不到任何实体；**不造实体**）。
    pub unresolved: usize,
    /// I6：实体行指向自己的 mention 条数（`from_id == entity_id`）。这些是纯噪声，
    /// 不落库、单列一格，供将来读数（当前 `DeriveOutcome` 不搬这一格）。
    pub self_skipped: usize,
}

/// `basis_ids` 的 JSON 数组。rc-memory 的依赖集里没有 serde_json（守卫 10 的红线），
/// 而依据 id 是内容哈希（小写 hex）⇒ 这里只做最小引号包裹，不假装自己是一个通用 JSON 编码器。
fn json_array(ids: &[String]) -> String {
    let mut out = String::from("[");
    for (i, id) in ids.iter().enumerate() {
        if i > 0 { out.push(','); }
        out.push('"'); out.push_str(id); out.push('"');
    }
    out.push(']');
    out
}

/// 沿 `to_id = 当前端点` 反向走**未撤销**的软边，返回最大深度（无入边 ⇒ 0）。
/// 端点是 `"<table>:<id>"` 键；时间行那一端永远是 `card:<id>`。
/// 上限 2 的执法点：深度 0/1 允许再连一条，深度 2 不再连（链就此封顶）。
fn live_link_depth(conn: &rusqlite::Connection, to_key: &str) -> Result<i64, MemError> {
    let depth: i64 = conn.query_row(
        "WITH RECURSIVE up(cur, depth) AS (
             SELECT ?1, 0
             UNION ALL
             SELECT l.from_id, up.depth + 1
               FROM fact_link AS l JOIN up ON l.to_id = up.cur
              WHERE up.depth < 2
                AND NOT EXISTS (SELECT 1 FROM retirement AS r
                                WHERE r.mem_table='fact_link' AND r.mem_id=l.id
                                  AND r.resolved_at_ms IS NULL)
         )
         SELECT coalesce(max(depth), 0) FROM up",
        [to_key], |r| r.get(0))?;
    Ok(depth)
}

/// I6：给一行正文补铸别名 mention——标题/别名归一后出现在正文里就落一行。
/// 判据与 `shared_text_anchor` 同一把尺子（`norm_anchor_key`），所以"关系能靠别名锚上"
/// 与"mention 铸得出来"是同一件事。已存在的 `(mem_table, mem_id, anchor_id)` 由主键
/// 吞掉（`INSERT OR IGNORE`）；锚行的自指在这里不被过滤（由 `put_mentions` 负责），
/// 因为派生行（card）不可能是锚本身。
fn insert_auto_mentions(
    tx: &rusqlite::Transaction, ctx: &BatchCtx, row_id: &str, row_kind: &str,
    text: &str, now_ms: i64, index: &[(String, Vec<String>)],
) -> Result<usize, MemError> {
    let key = norm_anchor_key(text);
    let mut written = 0usize;
    for (anchor_id, keys) in index {
        if row_kind == "anchor" && row_id == anchor_id { continue; }   // 自指不铸
        if !keys.iter().any(|k| key.contains(k.as_str())) { continue; }
        written += tx.execute(
            "INSERT OR IGNORE INTO memory_mention
                 (mem_table, mem_id, anchor_id, source_agent, device_id, created_at_ms, norm_version)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![row_kind, row_id, anchor_id,
                              ctx.source_agent, ctx.device_id, now_ms,
                              rc_store::capability::NORM_VERSION as i64],
        )?;
    }
    Ok(written)
}

/// I6 的补锚入口：给一批 `(行 id, 正文)` 铸别名 mention，返回实际插入行数。
/// `rc_derive::derive_pass` 在关系建边**之前**调用它，所以 I4 的"共享 mention"判据
/// 看到的是本批刚铸出来的那批边（审计的 3 条应铸未铸因此有落点）。
pub fn link_row_mentions(
    m: &MemoryHandle, rows: &[(String, String)], ctx: &BatchCtx, now_ms: i64,
) -> Result<usize, MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    let conn = m.conn();
    let tx = conn.unchecked_transaction()?;
    let index = entity_anchor_index(&tx)?;
    let mut written = 0usize;
    for (row_id, text) in rows {
        let table = table_of(&tx, row_id)?;
        written += insert_auto_mentions(&tx, ctx, row_id, table.prefix(), text, now_ms, &index)?;
    }
    tx.commit()?;
    Ok(written)
}

/// 写边：id = compute_edge_id(from, kind, to)；`INSERT OR IGNORE` 幂等。
/// 落库走单一事务：一条 fact_link 行 + **三族 provenance 行**——
/// from 端、to 端（角色都取两端最强，与列上的 `anchor_role` 同源）、以及每条 `basis_ids`
/// （各取自己的 `anchor_role_of`）。三族合起来让两跳溯源从"只看得见 from 端"变成
/// "两端与第三条依据都回读得到"（C20）。边仍不是可指认句柄（裁定 3）：不进 `MemTable`、
/// 不进 `ROW_TABLES`、不进 `facts()`；读侧走 `reader::edges` / `edge_provenance`。
///
/// 证据闸（I-2）在这一格里被 `put_edges_inner` 的门口语句执行：机制类 relation 交不出机制句
/// 或点不出一条依据行 ⇒ 不落。**这一格的后果要如实登记**：`put_edges` 只有一个返回格（写入数），
/// 被闸挡下的条数在这一支**今天没有读数出口**（`put_edges_anchored` 那一支有，第二格就是它）。
/// 今天它没有生产调用者（derive 走 `put_edges_anchored`），所以这一格是"门口少拦一条"而不是
/// "拦了没人知道"；哪天有人接进来，第一件要补的事就是把这一格也交出去。
pub fn put_edges(
    m: &MemoryHandle, edges: &[EdgeContent], ctx: &BatchCtx, now_ms: i64,
) -> Result<usize, MemError> {
    let (written, _dropped) = put_edges_inner(m, edges, ctx, now_ms, None)?;
    Ok(written)
}

/// **I4 的关系锚门槛**：derive 建边必须先有一条真锚，纯同轮共现不建边。
///
/// 判据（满足任一）：
/// ① **共享 mention**：两端在 `mention` 表里指向同一个实体（本批的 mention 必须先落库——
///    `rc_derive::derive_pass` 因此把 mention 写在 edge 之前）；
/// ② **逐字同实体**：两端正文都出现同一个实体的标题/别名（`norm_anchor_key` 归一后包含，
///    别名文本锚因此可识别）。
///
/// `part_of` 额外收紧：它只认 ①（共享 mention）。审计的 `c18cd965`（定价卡 part_of 文档实体）
/// 正是"目标标题包含源标题里的名词、但两端没有共同提及"的那一型；放宽到 ② 会把它放回来。
///
/// **本文件里它不是唯一那道闸**：机制类 relation 还要过证据闸（见 [`lacks_mechanism_evidence`]），
/// 两道闸共用第二格计数 ⇒ 运维面看到的是"被挡下的条数"，分不出是哪一道（本轮不新增读数结构，
/// P-5）。要分诊就看 relation：结构归属那一支只可能被 I4 挡，机制类那一支两道都可能。
///
/// 归属统一成 `project`（I4）：端点 scope 可能是 `entity/<标题>`，边却属于项目面；
/// 传进来的 `scope.key` 保留（derive 侧按"首个 project scope"选），`kind` 在这里强制为 Project。
/// 未过门槛的边**不落库**，计入返回的第二格（derive 侧把它并进 `rejected` 读数）。
pub fn put_edges_anchored(
    m: &MemoryHandle, edges: &[EdgeContent], ctx: &BatchCtx, now_ms: i64,
) -> Result<(usize, usize), MemError> {
    put_edges_inner(m, edges, ctx, now_ms, Some(()))
}

fn put_edges_inner(
    m: &MemoryHandle, edges: &[EdgeContent], ctx: &BatchCtx, now_ms: i64, anchored: Option<()>,
) -> Result<(usize, usize), MemError> {
    // 只读闸门：库内九处写入口（`writers_a` 1 + `writers_c` 4 + `writers_p` 2 +
    // `writers_x` 的 `write_fact`/`put_batch` 2）每一处开头都是这一行，写面就长这样。
    // 少它不是"引擎会兜住"——引擎确实会拒写（副本走只读打开），但错出来的是
    // `MemError::Sql("attempt to write a readonly database")`，而 T5 按族分诊时
    // 读数要念的是 `ReadOnly`；sqlite 文本不是运维能据以判断的那句话（I-3）。
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    let conn = m.conn();
    let tx = conn.unchecked_transaction()?;
    // 实体锚索引只读一次；未启用锚门槛的旧入口不付这笔读。
    let index = match anchored {
        Some(()) => entity_anchor_index(&tx)?,
        None => Vec::new(),
    };
    let mut n = 0usize;
    let mut dropped = 0usize;
    for e in edges {
        // 证据闸（派发档 I-2 · Phase 2.4 / 计划 D3「无证据不落」）：机制类 relation 必须
        // ① 交得出一句不是套话的机制（那句话就是下面 `?5`，逐字进 `fact_link.basis`），
        // ② 点名至少一条依据行（`role='basis'` 的出处行；两端那两行说的是"连着谁"，不是"凭什么"）。
        // 只拦 `MECHANISM_RELATIONS` 那四支（P-4）：把结构归属也要求机制句会得到一批假拒。
        // **为什么门口也要有一道**（模型面那道住 `rc-derive::parse`）：与 `put_chains_merged`
        // 的 `ContentEmpty` 同一处分工——它的价值是"绕过 parse 的调用者同样被拦"，不是"判据更强"。
        // 两道闸的先后有意：这一道不查库，先跑；I4 那道要读 mention，后跑。一条边只计一次
        // `dropped`（两支各自 `continue`，都在 `compute_edge_id` 与任何字节写进库之前）。
        if lacks_mechanism_evidence(&e.kind, &e.reason, e.basis_ids.len()) {
            dropped += 1; continue;
        }
        let id = compute_edge_id(&e.from_id, &e.kind, &e.to_id);
        let from_table = table_of(&tx, &e.from_id)?;
        let to_table = table_of(&tx, &e.to_id)?;
        if anchored.is_some() {
            let from_mentions = mention_ids_of(&tx, &e.from_id)?;
            let shared_mention = !from_mentions.is_disjoint(&mention_ids_of(&tx, &e.to_id)?);
            let text_anchor = if shared_mention {
                true
            } else {
                let (a, b) = (row_anchor_text(&tx, &e.from_id)?, row_anchor_text(&tx, &e.to_id)?);
                shared_text_anchor(&a, &b, &index).is_some()
            };
            // `part_of` 只认共享 mention（见函数文档）；其余 kind 允许 ① 或 ②。
            // 再收紧一层：目标是实体行时，`from` 必须**直接提到**它——审计 `c18cd965`
            // 的定价卡只提到 `Agnes 3.0 Flash`（另一个实体），目标却是「…官方文档」；
            // 共享 mention 会由那个被共提的实体给出，单靠交集拦不住这一型。
            let ok = if e.kind == "part_of" {
                shared_mention && (to_table != MemTable::Anchor || from_mentions.contains(&e.to_id))
            } else {
                shared_mention || text_anchor
            };
            if !ok { dropped += 1; continue; }
        }
        let strongest = strongest(anchor_role_of(&tx, &e.from_id)?, anchor_role_of(&tx, &e.to_id)?);
        // 依据先验：任何一条不存在 ⇒ `table_of` 报错、事务回滚 ⇒ 不落一条半截边。
        let mut bases = Vec::with_capacity(e.basis_ids.len());
        for b in &e.basis_ids {
            bases.push((b.clone(), table_of(&tx, b)?, anchor_role_of(&tx, b)?));
        }
        // I4：归属统一 project。key 由调用方给（derive 侧取首个 project scope 的 key）。
        let scope = Scope { kind: ScopeKind::Project, key: e.scope.key.clone() };
        // canonical：端点存 "<table>:<id>" 键；`relation` 就是调用方给的 kind，`basis` 是理由。
        let from_key = format!("{}:{}", from_table.prefix(), e.from_id);
        let to_key = format!("{}:{}", to_table.prefix(), e.to_id);
        let n_ins: usize = tx.execute(
            "INSERT OR IGNORE INTO fact_link
                 (id, from_id, to_id, relation, basis, evidence_json, decided_by,
                  scope_kind, scope_key, world_id, source_agent, device_id, created_at_ms, norm_version)
             VALUES (?1,?2,?3,?4,?5,'[]',NULL,?6,?7,NULL,?8,?9,?10,?11)",
            rusqlite::params![id, from_key, to_key, e.kind, e.reason,
                              scope.kind.as_str(), scope.key,
                              ctx.source_agent, ctx.device_id, now_ms,
                              // `norm_version` 不写字面量：本文件头那句"它是
                              // `rc_store::capability::NORM_VERSION` 常量、连时钟位都没有"
                              // 说的就是它只能从常量取，否则常量升版时这两张新表悄悄停在旧值。
                              rc_store::capability::NORM_VERSION as i64],
        )?;
        if n_ins == 0 { continue; }  // 已存在 ⇒ 不重复插 provenance
        insert_derived_provenance(&tx, "fact_link", &id, &e.from_id,
                                  strongest.as_str(), from_table.prefix(),
                                  ROLE_BASIS, None)?;
        insert_derived_provenance(&tx, "fact_link", &id, &e.to_id,
                                  strongest.as_str(), to_table.prefix(),
                                  ROLE_BASIS, None)?;
        for (b, b_table, b_role) in bases {
            insert_derived_provenance(&tx, "fact_link", &id, &b,
                                      b_role.as_str(), b_table.prefix(),
                                      ROLE_BASIS, None)?;
        }
        n += 1;
    }
    tx.commit()?;
    Ok((n, dropped))
}

/// 写时间线：每条落一个 fact 行（kind='timeline'、mode='inferred'）+ 三锚列 + 1 条 provenance。
/// 落库走单一事务。`source_agent` / `device_id` 同样只从 `ctx` 取（守卫 11）。
///
/// I3：`as_of_ms` / `created_at_ms` 都取**源侧**时刻，不取导入钟 `now_ms`：
/// 有 `time_anchor_ms` ⇒ 它就是源时刻；无锚但有被锚行/依据行 ⇒ 取那条记忆行的
/// `created_at_ms`（I3 之后 = 源轮时刻）；两者都没有才回落 `now_ms`（parse 拒收那种形状，
/// 生产路径不可达）。`time_confidence` 仍由调用方决定（无锚 = `unknown`，锚列保持 NULL）。
///
/// 返回**实际写入的时间行数**；`event_link` 旁路写入（A44 软边）单独计数，由
/// [`put_timeline_rows_counted`] 交给 derive 侧并进 `derive_rows`（R3：旁路写入必须有读数出口）。
pub fn put_timeline_rows(
    m: &MemoryHandle, rows: &[TimelineContent], ctx: &BatchCtx, now_ms: i64,
) -> Result<usize, MemError> {
    let (written, _links) = put_timeline_rows_counted(m, rows, ctx, now_ms)?;
    Ok(written)
}

/// 与 [`put_timeline_rows`] 同一条写路径，额外返回本次插入的 `event_link` 行数（R3）。
///
/// I-3e · FIX-1 起这一族还会为落下的每张时间线卡写一行**继承**归属（[`inherit_card_owner`]），
/// 而那一格的计数**不在这两格里**：derive 侧按 `(rows, links)` 解构，`owner_ambiguous` 在
/// [`put_timeline_rows_with_owner_account`] 返回的 [`TimelineWriteAccount`] 上。
pub fn put_timeline_rows_counted(
    m: &MemoryHandle, rows: &[TimelineContent], ctx: &BatchCtx, now_ms: i64,
) -> Result<(usize, usize), MemError> {
    let a = put_timeline_rows_with_owner_account(m, rows, ctx, now_ms)?;
    Ok((a.rows, a.links))
}

/// 与 [`put_timeline_rows_counted`] **同一条实现**（本族唯一的实现，判据 52 那条律没动），
/// 账本多一格 FIX-1 新增的 `owner_ambiguous`：本次**真的写下的**时间线卡里，依据行归属不唯一
/// （或取不到归属证据）而**没有**写上归属行的条数。
///
/// 为什么这一格开在新入口上而不是塞进原来那两格：原第二格的意思是"本次插入的 `event_link`
/// 行数"，`rc-derive/src/derive.rs` 的 `derive_pass` 按二元组解构它并把第二格并进 `rows_written`，而那个
/// 文件在本轮允许集之外 ⇒ 改形状就是改派生面的读数口径。落法照本文件既有先例
/// （`put_timeline_rows` 是 [`put_timeline_rows_counted`] 的薄包装、`put_chains` 是
/// [`put_chains_merged`] 的薄包装），同一条实现只多一个名字。
/// `owner_ambiguous` 今天在生产侧**没有读者**（`DeriveOutcome` 不搬这一格），与
/// [`MentionOutcome::self_skipped`] 同一格处境；它的消费者是用例
/// （`crates/rc-memory/tests/derived_visibility.rs`）。
pub fn put_timeline_rows_with_owner_account(
    m: &MemoryHandle, rows: &[TimelineContent], ctx: &BatchCtx, now_ms: i64,
) -> Result<TimelineWriteAccount, MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }  // 同 `put_edges` 的只读闸门（I-3）
    let conn = m.conn();
    let tx = conn.unchecked_transaction()?;
    let index = entity_anchor_index(&tx)?;
    let mut n = 0usize;
    let mut links = 0usize;
    let mut owner_ambiguous = 0usize;
    for t in rows {
        // 沿 FactContent + WriteItem 形状：用 SQL 直插（id = sha256 by content — 走 compute_mem_id）
        let id = crate::identity::compute_mem_id(MemTable::Card, "timeline", &t.title, &t.body, &t.scope);
        // I3：源时刻解析（见函数文档）。`now_ms` 只在"连依据行都没有"时兜底。
        let source_ms = match t.time_anchor_ms {
            Some(ms) => ms,
            None => match (&t.time_anchor_id, t.basis_ids.first()) {
                (Some(aid), _) => created_at_of(&tx, aid)?,
                (None, Some(bid)) => created_at_of(&tx, bid)?,
                (None, None) => now_ms,
            },
        };
        // 三锚列：本任务内层只决定"是否写 NULL"；置信已在调用方决定
        // 层间可信度（Task 7）：这一族是**推导产物**，`mode` 显式落 `'inferred'`，
        // 不吃列的 DEFAULT（`ddl.rs:487` = `'stated'`）——同形落库就等于读者分不出「推出来的」。
        // 写成字面量而不是加第 15 个参数：本文件的 `?13`/`?14` 是**故意排在参数列表末尾**的
        // （`NORM_VERSION`/`causal_type` 复用同一批编号），插参数会移位；而 `mode` 在这一层
        // 根本没有可递的值——它是「这条路径是谁」的常量答案。
        let n_ins: usize = tx.execute(
            "INSERT OR IGNORE INTO card
                 (id, kind, title, text, scope_kind, scope_key, as_of_ms,
                  source_agent, device_id, created_at_ms, norm_version,
                  causal_type, time_anchor_ms, time_anchor_id, time_confidence, mode)
             VALUES (?1,'timeline',?2,?3,?4,?5,?6,?7,?8,?9,?13,NULL,?10,?11,?12,'inferred')",
            rusqlite::params![id, t.title, t.body, t.scope.kind.as_str(), t.scope.key,
                              source_ms,
                              ctx.source_agent, ctx.device_id, source_ms,
                              t.time_anchor_ms, t.time_anchor_id, t.time_confidence,
                              // 同 `put_edges`：`norm_version` 只从常量取（?13 排在末位是
                              // 编号参数的写法，不是列序）。
                              rc_store::capability::NORM_VERSION as i64],
        )?;
        if n_ins == 0 { continue; }
        // I5：派生事件补 mention（正文/标题里逐字出现的锚，含别名锚）。导数不参与 `n`：
        // 它不是时间行，`derive_rows` 另行累加。
        let text = format!("{} {}", t.title, t.body);
        insert_auto_mentions(&tx, ctx, &id, "card", &text, now_ms, &index)?;
        // provenance(ref_kind=被锚行表, turn_uid=被锚行 id) ⇒ 两级溯源
        if let Some(anchor_id) = &t.time_anchor_id {
            let anchor_table = table_of(&tx, anchor_id)?;
            // 锚角色 = 被锚行自己 provenance 的最强值（裁定 11）。`anchor_role_of` 内部已取最强，
            // 所以这里直接用它；曾经硬写 `'agent'` 是"读数没有生产者"那一族（P-44）。
            let role = anchor_role_of(&tx, anchor_id)?;
            insert_derived_provenance(&tx, "card", &id, anchor_id,
                                      role.as_str(), anchor_table.prefix(),
                                      ROLE_BASIS, None)?;
            // A44 软边：只有"锚到一条消费行"才连。锚到锚行时没有事件对，
            // 不造边（软边的两端按设计都落在 card）。上限 2：深度 2 起只落时间行。
            //
            // 证据闸在这一支也要长一遍（I-2）：`responds_to` 是**今天唯一在生产里出现的机制类
            // `fact_link`**，而它的 `basis` 就是这一行时间线的正文（`?4`）。正文空白或套话 ⇒
            // 这条边说不出自己凭什么 ⇒ 不连。**分寸沿用既有那一格**（链长上限二那句
            // "上限只挡边不挡行"）：时间行与它的三锚列照落，`links` 读数自然少一——
            // 所以这一道既不是"数据丢弃"也不需要新的计数出口。
            // 依据行那一半在这里恒成立：上面那句 `insert_derived_provenance(..., ROLE_BASIS, None)`
            // 已经把**被锚行**署成了一条 `role='basis'` 出处，那是软边独有的一条真依据（时间行
            // 锚在谁身上就是凭什么），不需要 `t.basis_ids` 再点一次名。
            let anchor_key = format!("{}:{}", anchor_table.prefix(), anchor_id);
            if anchor_table == MemTable::Card && !is_placeholder_mechanism(&t.body)
                && live_link_depth(&tx, &anchor_key)? < 2 {
                let link_id = compute_event_link_id(anchor_id, &id);
                let self_key = format!("card:{id}");
                let ins = tx.execute(
                    "INSERT OR IGNORE INTO fact_link
                         (id, from_id, to_id, relation, basis, evidence_json, decided_by,
                          scope_kind, scope_key, world_id, source_agent, device_id,
                          created_at_ms, norm_version)
                     VALUES (?1,?2,?3,'responds_to',?4,?5,NULL,?6,?7,NULL,?8,?9,?10,?11)",
                    rusqlite::params![link_id, anchor_key, self_key, t.body,
                                      json_array(&t.basis_ids),
                                      t.scope.kind.as_str(), t.scope.key,
                                      ctx.source_agent, ctx.device_id, now_ms,
                                      rc_store::capability::NORM_VERSION as i64],
                )?;
                links += ins;   // R3：旁路写入的计数出口（derive 侧并进 rows_written）
            }
        }
        // FIX-1：这张时间线卡的归属继承自它点名的依据行（被锚行 + `basis_ids`）。
        // 依据行的表名不在这里问：`basis_owners` 按 id 过"有归属面的那三张表"，
        // 所以这一支不会像 `table_of` 那样把"依据行不存在"变成整批 `Err`
        // （本族今天对 `basis_ids` 不做存在性校验，它只进 `evidence_json`）。
        let mut basis: Vec<String> = Vec::with_capacity(1 + t.basis_ids.len());
        if let Some(anchor_id) = &t.time_anchor_id {
            basis.push(anchor_id.clone());
        }
        basis.extend(t.basis_ids.iter().cloned());
        if inherit_card_owner(&tx, ctx, &id, &basis, &t.scope, source_ms)?
            == Inheritance::Ambiguous
        {
            owner_ambiguous += 1;
        }
        n += 1;
    }
    tx.commit()?;
    Ok(TimelineWriteAccount { rows: n, links, owner_ambiguous })
}

/// 写因果链（批次③ · 段 B，这一族**唯一**的生产写入口）：一条链 = 一行 `event(kind='causal')`
/// 的头行 + `provenance` 里 `role='link'` 的环节行（带 `ord = 0..`）与 `role='basis'` 的依据行
/// （不带 `ord`）。**整批一个事务**：头行与它的出处行要么全成要么全无（同 `put_edges`）。
/// `source_agent` / `device_id` 同样只从 `ctx` 取（守卫 11），`norm_version` 只从
/// `rc_store::capability::NORM_VERSION` 取（不写字面量，常量升版时这一族不许停在旧值）。
///
/// **头行 id 不走 `compute_mem_id`**，走 `identity::compute_chain_id`（工单 C-1，承重墙）：
/// 前者的 preimage 里没有环节列表，而链头的正文按 C-2 就是机制句本身 ⇒ "同一句机制、两组环节"的
/// 两条链算出同一个 id，第二条被头行那句 `INSERT OR IGNORE` 静默吞掉（环节全丢、不报错、不红）。
/// 也**不走 `write_item_in` / `fact_sql`**：那两条内部各算一次 `compute_mem_id`，正是这里要防的
/// 那次碰撞，且 `fact_sql` 的列清单根本没有三锚列。SQL 形状照 `put_timeline_rows` 那条 16 列
/// `INSERT OR IGNORE INTO card(...)` 逐字走（含 `mode='inferred'` 那一格，Task 7：两族都是推导产物），
/// 差别只有三格：`kind` 写 `'causal'`、
/// 标题是 `derive-` + **完整**机制句（I5：不截断）、三锚列取**最后一环的源侧时刻**
/// （`created_at_ms`，I3 后 = 源轮时刻；锚到最后一环 ⇒ `time_confidence='anchored'`；
/// 没有环节才退 `NULL/NULL/'unknown'`）。`valid_to_ms` 不进列清单——那一列在 `event` 表可空。
///
/// **两段出处行的先后是 C-3 的承重墙**：先按 `ord = 0..n` 插环节，**再**插依据。原因：输出契约的
/// 示例本身就是 `basis ⊆ from`（依据几乎总是环节的某个子集），而 `provenance` 的主键是
/// `(mem_table, mem_id, turn_uid)` ⇒ 同一句柄既是环节又是依据时必撞主键。这个次序让撞掉的恰是
/// 后到的那一跳（它已经以环节的身份在库里了，信息没丢）；反序会把环节降级成依据 ⇒ `ord` 缺口，
/// 而"顺序读得回来"是本批唯一的价值。
///
/// 返回**写入的链条数**：不是行数之和，也不是计划条数。判据是既有两个推导入口的同一律
/// ——头行 `INSERT OR IGNORE` 命中已有 ⇒ `continue`、这一条不计 ⇒ **同一条链重跑报 0**
/// （裁定"幂等 = 内容寻址 id + INSERT OR IGNORE"要的那格可观测）。
///
/// 空机制的两道判据分工（本函数这一道在任何字节写进库**之前**）：模型面住 parse 层
/// （`mechanism.trim().is_empty()` ⇒ 计 `rejected`），那里只能用 `trim`——rc-derive 不依赖
/// rc-contract，`normalize_body` 递不进去，加依赖就撞本批红线；本函数用
/// `normalize_body(&c.mechanism).is_empty()` ⇒ `ContentEmpty`，**价值是守在库门口**（绕过 parse 的
/// 调用者同样被拦），不是"判据更强"：`normalize_body` = LF 折叠 + NFC + `trim()`，比 `trim()` 多的
/// 两对操作都不增加空判覆盖面 ⇒ 空白之外仍有噪声形残值，那与 `validate_fields`、agent 侧、distill
/// 侧用的是同一把尺子，本批不单独收紧（收紧要新造判据，会让这个入口与既有每个写入口不一致）。
///
/// **不调 `validate_fields`**——与既有两个推导入口（`put_edges`、`put_timeline_rows`）同律，不是漏了
/// 一道闸门：那一条查的三格对推导行另有归宿——出处非空由本函数自己插（环节就是出处）、
/// 标题非空由 `"derive-"` 前缀保证、别名闸门管的是 `alias` 那张表而链头行不进它。
pub fn put_chains(
    m: &MemoryHandle, rows: &[ChainContent], ctx: &BatchCtx, now_ms: i64,
) -> Result<usize, MemError> {
    let (written, _merged) = put_chains_merged(m, rows, ctx, now_ms)?;
    Ok(written)
}

/// I5 的"重复因果事件合并/降权"：同一 `causal_type` + **同一顺序**的环节序列只留一条链。
/// 审计的六条因果事件里有几条只是把既有 card 复述成事件；其中真正重复的那些（同一组环节、
/// 换个说法）在这里被合并，`rejected` 侧拿到第二格读数。C-1/P1/P2 不受影响：
/// 同机制不同环节、A→B 与 B→A 的环节序列不同 ⇒ 仍是两条链。
///
/// 另加一条**文本降权**：机制句与任一环节行正文的字符二元组 Jaccard ≥ 0.7 时，
/// 这条"因果"只是把那张 card 抄了一遍 ⇒ 不落头行（计入合并数）。
///
/// I-3e · FIX-1 起这一族会为落下的每张链头卡写一行**继承**归属（[`inherit_card_owner`]），
/// 而那一格的计数**不在这两格里**：derive 侧按 `(rows, merged)` 解构，`owner_ambiguous` 在
/// [`put_chains_merged_with_owner_account`] 返回的 [`ChainWriteAccount`] 上。
pub fn put_chains_merged(
    m: &MemoryHandle, rows: &[ChainContent], ctx: &BatchCtx, now_ms: i64,
) -> Result<(usize, usize), MemError> {
    let a = put_chains_merged_with_owner_account(m, rows, ctx, now_ms)?;
    Ok((a.chains, a.merged))
}

/// 与 [`put_chains_merged`] **同一条实现**（这一族唯一的实现，判据 52② 两处计数一字未动），
/// 账本多一格 FIX-1 新增的 `owner_ambiguous`：本次**真的写下的**链头卡里，依据行归属不唯一
/// （或取不到归属证据）而**没有**写上归属行的条数。
///
/// 为什么这一格开在新入口上而不是塞进原来那两格：`rc-derive/src/derive.rs` 的 `derive_pass` 按
/// `(chain_rows, chain_merged)` 二元组解构并把第二格并进 `rejected` 读数，而那个文件在本轮
/// 允许集之外 ⇒ 改它的形状就是改派生面的读数口径。落法照本文件既有先例（`put_chains` 是
/// [`put_chains_merged`] 的薄包装、`put_timeline_rows` 是 [`put_timeline_rows_counted`] 的
/// 薄包装）：同一条实现只多一个名字，实现仍然只有一份。
/// `owner_ambiguous` 在生产侧今天**没有读者**（`DeriveOutcome` 不搬这一格），与
/// [`MentionOutcome::self_skipped`] 同一格处境；它的消费者是用例
/// （`crates/rc-memory/tests/derived_visibility.rs`）。
pub fn put_chains_merged_with_owner_account(
    m: &MemoryHandle, rows: &[ChainContent], ctx: &BatchCtx, now_ms: i64,
) -> Result<ChainWriteAccount, MemError> {
    // 只读闸门（I-3，与 `put_edges` 那一句逐字同形）。少它不是"引擎会兜住"：引擎确实拒写，
    // 但错出来的是 `MemError::Sql("attempt to write a readonly database")`，而 T5 按族分诊时
    // 读数要念的是 `ReadOnly`——sqlite 文本不是运维能据以判断的那句话。
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    // 空内容判据在任何一条写入、任何事务开始之前（`put_batch` 的"先查后写"同律）。
    for c in rows {
        if normalize_body(&c.mechanism).is_empty() { return Err(MemError::ContentEmpty); }
    }
    let conn = m.conn();
    let tx = conn.unchecked_transaction()?;
    let index = entity_anchor_index(&tx)?;
    // 已落库的因果链：mem_id → (causal_type, 有序环节)。一次读全表，批内不查第二遍。
    let mut existing: BTreeMap<String, (String, Vec<String>)> = BTreeMap::new();
    {
        let mut st = tx.prepare(
            "SELECT p.mem_id, p.turn_uid, e.causal_type
               FROM provenance p JOIN card e ON e.id = p.mem_id
              WHERE p.mem_table='card' AND p.role='link' AND e.kind='causal'
              ORDER BY p.mem_id, p.ord")?;
        let got = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?,
                                           r.get::<_, Option<String>>(2)?)))?;
        for row in got {
            let (id, link, ty) = row?;
            let entry = existing.entry(id).or_insert((ty.unwrap_or_default(), Vec::new()));
            entry.1.push(link);
        }
    }
    let mut batch_seqs: BTreeSet<(String, Vec<String>)> = BTreeSet::new();
    let mut n = 0usize;
    let mut merged = 0usize;
    let mut owner_ambiguous = 0usize;
    for c in rows {
        let mechanism = normalize_body(&c.mechanism);
        // I5：标题 = `derive-` 前缀 + **完整**机制句，不再按 20 字符截断
        // （英文机制句在词中间被切开是审计 I5 的原话）。
        let title = format!("derive-{mechanism}");
        let seq_key = (c.causal_type.clone(), c.link_ids.clone());
        if batch_seqs.contains(&seq_key) { merged += 1; continue; }
        if existing.values().any(|(ty, links)| ty == &c.causal_type && links == &c.link_ids) {
            merged += 1; continue;
        }
        // 文本降权：机制句几乎就是某条环节行的正文 ⇒ 复述，不是新事实。
        let mech_bg = bigrams(&norm_anchor_key(&mechanism));
        let mut restates = false;
        for link_id in &c.link_ids {
            let (t, b) = row_title_and_body(&tx, link_id)?;
            let jt = jaccard(&mech_bg, &bigrams(&norm_anchor_key(&t)));
            let jb = jaccard(&mech_bg, &bigrams(&norm_anchor_key(&b)));
            if jt.max(jb) >= 0.7 {
                restates = true; break;
            }
        }
        if restates { merged += 1; continue; }
        let id = compute_chain_id(&c.scope, &mechanism, &c.link_ids);
        // I5：链头时间锚 = 最后一环（链的落点）的源侧时刻；I3 后 `created_at_ms` 就是源轮时刻。
        let anchor = c.link_ids.last()
            .map(|last| created_at_of(&tx, last).map(|ms| (last.clone(), ms)))
            .transpose()?;
        let (anchor_ms, anchor_id) = match &anchor {
            Some((aid, ms)) => (Some(*ms), Some(aid.clone())),
            None => (None, None),
        };
        let as_of_ms = anchor_ms.unwrap_or(now_ms);
        let confidence = if anchor_ms.is_some() { "anchored" } else { "unknown" };
        // 同 timeline 那一族：链头是推导产物 ⇒ `mode` 显式 `'inferred'`，不吃 DEFAULT（Task 7）。
        let n_ins: usize = tx.execute(
            "INSERT OR IGNORE INTO card
                 (id, kind, title, text, scope_kind, scope_key, as_of_ms,
                  source_agent, device_id, created_at_ms, norm_version,
                  causal_type, time_anchor_ms, time_anchor_id, time_confidence, mode)
             VALUES (?1,'causal',?2,?3,?4,?5,?6,?7,?8,?9,?13,?14,?10,?11,?12,'inferred')",
            rusqlite::params![id, title, mechanism, c.scope.kind.as_str(), c.scope.key,
                              as_of_ms,                      // ?6 = as_of_ms
                              ctx.source_agent, ctx.device_id, as_of_ms,
                              anchor_ms, anchor_id, confidence,
                              rc_store::capability::NORM_VERSION as i64,
                              // ?14 = causal_type（批次 M，列尾）：parse 已验三值；同一条链的
                              // 类型变化在 `INSERT OR IGNORE` 下首写者胜（id 的 preimage 不含它）。
                              c.causal_type],
        )?;
        if n_ins == 0 { continue; }   // 头行已在 = 整条链已落 ⇒ 不重复插出处、不计这一条
        batch_seqs.insert(seq_key);
        // I5：派生事件补 mention（链头正文里出现的锚/别名）。
        insert_auto_mentions(&tx, ctx, &id, "card", &title, now_ms, &index)?;
        // 环节行（先）：`ref_kind` 与 `anchor_role` 都是**读出来的**——被引行在哪张表、
        // 它自己 provenance 的最强锚角色（P-44：读数必须有真生产者）。
        for (ord, link_id) in c.link_ids.iter().enumerate() {
            let link_table = table_of(&tx, link_id)?;
            let role = anchor_role_of(&tx, link_id)?;
            insert_derived_provenance(&tx, "card", &id, link_id, role.as_str(),
                                      link_table.prefix(), ROLE_LINK, Some(ord as i64))?;
        }
        // 依据行（后）：`ord` 递 `None`——依据不带序是那条跨列 CHECK 的另一半（`ROLE_BASIS` 配 NULL）。
        for basis_id in &c.basis_ids {
            let basis_table = table_of(&tx, basis_id)?;
            let role = anchor_role_of(&tx, basis_id)?;
            insert_derived_provenance(&tx, "card", &id, basis_id, role.as_str(),
                                      basis_table.prefix(), ROLE_BASIS, None)?;
        }
        // FIX-1：链头卡的归属继承自它点名的依据行——**环节 ∪ 依据**，与上面两段出处行同一批
        // 句柄（出处行说了"这条链连着谁、凭什么"，归属就跟着谁）。归属不唯一 ⇒ 一行都不写。
        let mut basis: Vec<String> = Vec::with_capacity(c.link_ids.len() + c.basis_ids.len());
        basis.extend(c.link_ids.iter().cloned());
        basis.extend(c.basis_ids.iter().cloned());
        if inherit_card_owner(&tx, ctx, &id, &basis, &c.scope, as_of_ms)?
            == Inheritance::Ambiguous
        {
            owner_ambiguous += 1;
        }
        n += 1;
    }
    tx.commit()?;
    Ok(ChainWriteAccount { chains: n, merged, owner_ambiguous })
}

/// 字符二元组集合（I5 的文本降权尺子）。归一后的串 ⇒ 空格/标点差异不影响比较。
fn bigrams(s: &str) -> BTreeSet<String> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 2 { return BTreeSet::new(); }
    chars.windows(2).map(|w| w.iter().collect()).collect()
}

fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() { return 0.0; }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    inter / union
}

/// 一条被收进成员表的事实行，只带本层要用到的那几格。
/// 其余列一律**从库里读**（母档落点约束：`RowFacts` 不加字段、`EpisodeContent` 不装读数）。
struct MemberFact {
    id: String,
    world_id: Option<String>,
    scope: Scope,
    occurred_at_ms: Option<i64>,
    occurred_end_ms: Option<i64>,
    /// 发话钟（I3 之后它就是源轮时刻）——时间那一格的第二支从这里取，不是 `now_ms`。
    created_at_ms: i64,
    /// `fact.participants_json` 里的裸 anchor id（写侧已把 subject/object 并进去）。
    participants: Vec<String>,
}

/// 镜像卡时间那一格的三支判据（母档 D4 + 派发档 I-3a "时间那一格"）。返回 `(as_of_ms, time_confidence)`：
///  · `start_ms` 在 ⇒ `(Some(start_ms), 'anchored')`——它是成员 `occurred_at_ms`/`occurred_end_ms`
///    的包络，**事件时间**；
///  · 只有发话时刻 ⇒ `(Some(最早成员的 created_at_ms), 'speech')`——**本轮新增的一支**，它说的是
///    "这一格的钟是说话那口钟，不是事情发生那口钟"。加它就是为了让"只用事件时间、缺了写时间未知"
///    那条契约不撒谎（旧词表只有 `anchored`/`unknown` 两支，退到发话时刻只能谎称 anchored 或 unknown）；
///  · 两者都无 ⇒ `(None, 'unknown')`——调用方**不落卡**（D4 原文"前两者都无才失败"）。
///
/// **两支钟都不许出场**：`now_ms` 是执行钟、`turns.ingested_at_ms` 是导入钟，把它们写进历史就是
/// I3 那次 9 天偏移的重演。D4 的实测依据在这里成立：36/36 `fact.created_at_ms` 与证据轮发话时刻
/// 逐字节相等 ⇒ "永远要兜底"那种格根本不存在。
///
/// **诚实登记（`'unknown'` 一支今天不可达）**：成员准入要求"至少有一条有出处的 fact"，而
/// `fact.created_at_ms` 是 NOT NULL 列 ⇒ 只要落得下成员，发话时刻就一定取得到。所以第三支今天
/// 唯一的作用是"让兜底那一格在代码里没有"（写成回落 `now_ms` 才是会红的那一形）。
/// 库里今天只会出现 `'anchored'` 与 `'speech'` 两个值，这条登记交 I-3b / Phase 4。
fn episode_time_arm(start_ms: Option<i64>, earliest_speech_ms: Option<i64>)
    -> (Option<i64>, &'static str) {
    match (start_ms, earliest_speech_ms) {
        (Some(ms), _) => (Some(ms), "anchored"),
        (None, Some(ms)) => (Some(ms), "speech"),
        (None, None) => (None, "unknown"),
    }
}

/// 成员按**事件序**排：`occurred_at_ms` 已知的在前（升序），未知的在后（按发话时刻升序）；
/// 同刻并列一律按 id 定序，这样"同输入两次一致"不靠查询计划（派发档 I-3a "时间那一格"）。
/// 未知那一格的"标 unknown"由 `fact.occurred_at_ms IS NULL` 自己说——`episode_fact` 只有三列，
/// 母档 §1 明令不改 DDL，所以这里不加第四格。
fn event_order(members: &[MemberFact]) -> Vec<&MemberFact> {
    let mut out: Vec<&MemberFact> = members.iter().collect();
    out.sort_by(|a, b| match (a.occurred_at_ms, b.occurred_at_ms) {
        (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.id.cmp(&b.id)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.created_at_ms.cmp(&b.created_at_ms).then_with(|| a.id.cmp(&b.id)),
    });
    out
}

/// 成员参与者的并集（按事件序首次出现定序、去重）。存**裸 anchor id 数组**，
/// 与 `fact.participants_json` 同一条约定（母档 §2.4：现网 36/36 都是裸 64-hex，
/// `reader::row_participants` 就是按裸 id 去查 `anchor.canonical`）。
fn union_participants(ordered: &[&MemberFact]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for member in ordered {
        for id in &member.participants {
            if !out.iter().any(|seen| seen == id) { out.push(id.clone()); }
        }
    }
    out
}

/// 写 episodes（派发档 I-3a 交付 2 · `episode`/`episode_fact` 这两张表**唯一**的生产写入口）。
///
/// 一段经历 = 一行 `episode` + 逐成员一行 `episode_fact`（`ord` = 事件序）+ 一行镜像消费卡
/// `card(kind='episode')` + **每成员一行** `provenance(ref_kind='fact', role='basis')` + 卡的别名
/// mention；成员集变化时再各一条 UPDATE 把旧行/旧卡指过去。**整批一个事务**：任何一步失败
/// ⇒ 一条都不写（同 [`put_edges`] / [`put_chains_merged`]）。
///
/// **为什么这一族必须住在本文件**（母档 §2.1，不是风格）：唯一合法的出处写法
/// [`insert_derived_provenance`] 是本文件的**私有 fn**，而判据 52③（`scripts/check-phase4.sh`）
/// 把 src 面的出处 INSERT 字面量钉成恰好 7 处 + 白名单四家 ⇒ 另起 `writers_episode.rs` 要么调不到
/// 这一格、要么逼本族自己写第八处字面量（当场红）。SQL 形状照 [`put_timeline_rows_counted`]：
/// 单事务、内容寻址 id、`mode='inferred'` 显式写（不吃 `ddl.rs` 的 `card.mode` 那条 DEFAULT `'stated'`）。
///
/// 四件事按母档/派发档钉死，改动任何一件都要先改这两份档：
/// ① **句柄**：`episode.id` 与镜像卡 id 由 `identity::compute_episode_id`/`compute_episode_card_id`
///    算，preimage = scope + **准入后、按事件序排好的**成员 fact id（I-3e · FIX-2；D3 的
///    "成员变则句柄变"由此真正成立：少一个成员就是另一个句柄，旧行走 `superseded_by` 收尾）。
///    `MemTable` 不扩（`keys.rs` 只有 `Anchor|Card`），所以 `episode` 的稳定消费句柄**就是这张卡**。
///    修前那一格用的是调用方递进来的 `ep.fact_ids`（含被准入拦掉的成员），于是"同一批输入、
///    成员被消音一个"前后两段**内容不同**的经历共享同一个 id ⇒ 同一个句柄对应两套内容，
///    配 `INSERT OR IGNORE` 就是第一遍的内容被永久冻住、重放不收敛（I-3d 的 T-7 实测：
///    两成员与一成员的 `episode.id` 都是 `87bd79ca0b39…`）。用例
///    `句柄取准入后按事件序的成员_少一个成员就换一个句柄`。
/// ② **时间那一格**：见 [`episode_time_arm`] 与 [`event_order`]；`start/end` 只从成员
///    `occurred_at_ms`/`occurred_end_ms` 取（D4-补：`card.as_of_ms` 与 `fact.occurred_at_ms` 在现网
///    4/4 全不等、两行差 8.04h/8.06h，反取就把时区偏移写进历史）。`episode.created_at_ms` 与
///    卡的 `created_at_ms` 都取本条的源时刻（`as_of_ms`），与 `put_timeline_rows` 同律。
/// ③ **成员准入的拦点**（母档 §3 那三条硬规则里"不进"的那两支 + 写入侧自己的两条 `Err`）：
///    无出处（既无 `ref_kind='turn'` 出处行、`evidence_turn_uid` 又为空）的 fact
///    不进并计 `dropped_members`；**已判死的 fact 也不进**（成员那一行自己 + 它那张陈述面消费卡，
///    I-3d MF-2 那条撤回权；同一格计数，见 [`admitted_member`] 的两支谓词）；
///    不跨 scope（跨了整批 `Err`、零行，因为那是调用方的错而不是数据
///    的形状）；成员 id 在库里不存在也是 `Err`——**不许**把"没这条行"读成"少一个成员"，
///    那正是"读数没有生产者"那一族（P-44）在写入侧的镜像。
///    （第三条硬规则"未知事件时间的成员 `ord` 排后"不在这里，它走 ② 那一格。）
/// ④ **出处只写 `ref_kind='fact'`、一条 `ref_kind='turn'` 都不写** ⇒ 它结构性进不了
///    `reader::unreviewed_rows`（那道 `EXISTS (… ref_kind='turn')` 就是母档说的三重拦点之一），
///    也不回喂推导池（`reader::turn_basis_pred` 同一支谓词）。用例真断，不靠注释。
///
/// **诚实登记（`episode.status` 的四值今天只有两值有生产者 · I-3d MF-3）**：DDL 那一格
/// （`rc-store/src/ddl.rs` 的 `episode` 建表那条 status CHECK）的值域是 `open`/`resolved`/`dormant`/`superseded` 四值，
/// 本入口只有两个生产者——新落的一行恒 `open`（上面 INSERT 的那个字面量），
/// 被同归属、共享成员的新行顶掉的那一行走 `superseded`（[`superseded_predecessors`] 那一支 UPDATE）。
/// 另两个值 `resolved`/`dormant` **零生产者、零读者**：本文件写它们的字面量一个都没有，
/// `src` 面也没有第二处能落到那一列的写点（同一个 commit 对 `time_confidence` 的不可达那一支
/// 做了同样的登记，见 [`episode_time_arm`]；同一把尺子在这里也该量到）。
///
/// 为什么留空、而不是这一轮把判定补上（控制器裁定：本轮**不发明判定规则**）：判据来源
/// `investigations/l2-episode.md:124` 给的前两格是"显式结束标记 ⇒ `resolved`"与
/// "`end_ms` 早于窗口且无更新 ⇒ `dormant`"，这两条在现网**都没有证据**：
/// ① 库里没有任何一格承载"这段经历结束了"（抽取、归组、审核三面都不产那个标记，
///    `episode` 表也没有那一列，母档 §1 明令本轮不改 DDL）；
/// ② "窗口"从未被量过——`investigations/episode-s0-measure.md` 量的是 Δt≤10min 与块内跨度≤15min
///    那两条**成对**闸门（D9 的归组判据），它们既不是"结束"的阈值，也搬不到生命周期上。
/// 缺这两格证据就发明一个判据，产出的是一条没人能验证的读数，还会让人以为"经历的生命周期已经纳管"
/// （V-4 A5 的"若判错会怎样"写的正是这一形：Phase 4 照着四值分支，其中两支是死码）。
///
/// **要填这一格缺什么**（不是"以后再说"）：一是 S0 补量"一段经历最后一次成员更新距今"的分布
/// 并给出阈值（`dormant` 的窗口判据），二是现网真出现一个"显式结束"的输入面（谁写、写哪一列、
/// 重放怎么算幂等）。**两条都在场**才谈得上那两个值；缺任一条就继续留空并保留这条登记。
/// 现状有格子钉着：`tests/episode.rs` 的 `episode_status_今天只有_open_与_superseded_两值`
/// 拿 `SELECT DISTINCT status FROM episode` 断这两值——将来任何人（含旁路 INSERT）写第三值都会被看见。
///
/// 卡面上**不写**的两格要说明：`time_anchor_ms`/`time_anchor_id` 留 NULL（这一族的时刻是成员聚合
/// 出来的，不是锚在某一行上——写成"锚在最早那条成员"会让 `export` 的链形状读法多出一个假锚），
/// `time_json` 留 NULL（它是台账点名的"写了没人读"族第一个实例，本轮不造第四个）。
///
/// `resource_owner` 那一格（I-3e · FIX-1 改判）：I-3a/I-3b 交回时写的是"派生族都不写归属行，
/// 可见性因此只由'署名 agent 恰等于读者'那一格放行"——那句话当时是真的，而它正是台账
/// 「Phase 2.5 现网验收」点名的断线：镜像卡与链卡对任何真实读者都渲染成 `row_absent`。
/// 现在这一族按 [`inherit_card_owner`] **继承**成员行的归属（成员分属不同主人 ⇒ 不写、
/// 计 `owner_ambiguous`），`episode` 结构行自己仍然没有归属面（它不是可指认句柄，
/// 读它的两面是 `reader::episode_members`/`episode_of_fact`，都不过归属闸）。
///
/// `now_ms` 是形参位（与其余写入口同形），本层**不使用**：见 [`episode_time_arm`] 那条纪律。
pub fn put_episodes(
    m: &MemoryHandle, rows: &[EpisodeContent], ctx: &BatchCtx, _now_ms: i64,
) -> Result<EpisodeWriteOutcome, MemError> {
    // 只读闸门（I-3，与其余九处写入口逐字同形）：少它不是"引擎会兜住"——引擎确实拒写，
    // 但错出来的是 `MemError::Sql("attempt to write a readonly database")`，而运维要念的是 ReadOnly。
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    validate_episodes(rows)?;
    let mut out = EpisodeWriteOutcome::default();
    if rows.is_empty() { return Ok(out); }
    let norm_version = rc_store::capability::NORM_VERSION as i64;
    let conn = m.conn();
    let tx = conn.unchecked_transaction()?;
    let index = entity_anchor_index(&tx)?;
    for ep in rows {
        // ③ 成员准入：逐条从库里读，被拦的计数、不落成员表。
        let mut members: Vec<MemberFact> = Vec::with_capacity(ep.fact_ids.len());
        for fact_id in &ep.fact_ids {
            match admitted_member(&tx, fact_id)? {
                Some(member) => members.push(member),
                None => out.dropped_members += 1,
            }
        }
        if members.is_empty() {
            out.dropped_episodes += 1;
            continue;
        }
        let (scope, world_id) = shared_member_scope(&members)?;
        // ① 时间那一格先算：FIX-2 之后**句柄与它同一把尺子**（都走成员的事件序）。
        let ordered = event_order(&members);
        // ② 句柄（FIX-2 · 派发档 I-3e）：preimage = **准入后**、**按事件序排好的**成员列表。
        //    修前用的是调用方递进来的 `ep.fact_ids`（含被准入拦掉的成员），于是"两成员"与
        //    "消音一个后只剩一成员"这两段**内容不同**的经历算出**同一个** id（I-3d 的 T-7 实测：
        //    两次都是 `87bd79ca0b39…`），而写出的成员行与卡上指针用的是准入后的另一套 ⇒
        //    同一个 id 对应两套内容，配 `INSERT OR IGNORE` 就是第一遍的内容被永久冻住、
        //    重放不收敛。母档 D3 那句"成员变则句柄变、靠 superseded_by 收尾"要的正是这一格。
        //    调用方那一序今天只决定"哪些成员被点名"（以及被拦掉几条），不再决定句柄。
        let ordered_ids: Vec<String> = ordered.iter().map(|f| f.id.clone()).collect();
        let id = compute_episode_id(&scope, &ordered_ids);
        let card_id = compute_episode_card_id(&scope, &ordered_ids);
        let start_ms = ordered.iter().filter_map(|f| f.occurred_at_ms).min();
        let end_ms = ordered.iter()
            .filter_map(|f| f.occurred_at_ms.map(|s| f.occurred_end_ms.unwrap_or(s)))
            .max();
        let earliest_speech = ordered.iter().map(|f| f.created_at_ms).min();
        let (as_of_ms, confidence) = episode_time_arm(start_ms, earliest_speech);
        // 'unknown' 那一支在这里就是"不落这一条"（D4 的"失败"）：卡上的 as_of_ms 是 NOT NULL 列，
        // 填任何别的东西都是编一个时刻出来。这一支在成员准入下不可达，见 [`episode_time_arm`]。
        let Some(as_of_ms) = as_of_ms else {
            return Err(MemError::Sql("episode has no usable source-side time".to_string()));
        };
        let participants = json_array(&union_participants(&ordered));
        let pointers = json_array(&ordered.iter().map(|f| format!("fact:{}", f.id)).collect::<Vec<_>>());
        let n_ins: usize = tx.execute(
            "INSERT OR IGNORE INTO episode(
                 id, title, narrative, start_ms, end_ms, participants_json, status,
                 world_id, scope_kind, scope_key, source_agent, device_id,
                 created_at_ms, norm_version, superseded_by)
             VALUES(?1,?2,?3,?4,?5,?6,'open',?7,?8,?9,?10,?11,?12,?13,NULL)",
            rusqlite::params![id, ep.title, ep.narrative, start_ms, end_ms, participants,
                              world_id, scope.kind.as_str(), scope.key,
                              ctx.source_agent, ctx.device_id, as_of_ms, norm_version],
        )?;
        if n_ins == 0 { continue; }   // 同一段经历已在 = 整段已落 ⇒ 不重复插成员/出处、不计这一条
        out.episodes += n_ins;
        for (ord, member) in ordered.iter().enumerate() {
            out.members += tx.execute(
                "INSERT OR IGNORE INTO episode_fact(episode_id, fact_id, ord) VALUES(?1,?2,?3)",
                rusqlite::params![id, member.id, ord as i64],
            )?;
        }
        // 镜像卡（④ 的另一半）：`mode='inferred'` 显式写，出处只走下面的 basis 行。
        let ins_card: usize = tx.execute(
            "INSERT OR IGNORE INTO card(
                 id, kind, title, text, pointers_json, scope_kind, scope_key, world_id,
                 as_of_ms, source_agent, device_id, created_at_ms, norm_version,
                 time_anchor_ms, time_anchor_id, time_confidence, mode)
             VALUES(?1,'episode',?2,?3,?4,?5,?6,?7,?8,?9,?10,?8,?11,NULL,NULL,?12,'inferred')",
            rusqlite::params![card_id, ep.title, ep.narrative, pointers,
                              scope.kind.as_str(), scope.key, world_id, as_of_ms,
                              ctx.source_agent, ctx.device_id, norm_version, confidence],
        )?;
        out.cards += ins_card;
        // mention 照模板（I5/I6）：标题 + 正文里逐字出现的锚（含别名锚）。
        let text = format!("{} {}", ep.title, ep.narrative);
        out.mentions += insert_auto_mentions(&tx, ctx, &card_id, "card", &text, as_of_ms, &index)?;
        // ④ 出处：每成员一行 `ref_kind='fact', role='basis'`，角色取那一行**自己**出处里的最强
        //     锚角色（`anchor_role_of` 内部已取最强；硬写 `'agent'` 是 P-44 那一族）。
        for member in &members {
            let role = anchor_role_of(&tx, &member.id)?;
            insert_derived_provenance(&tx, "card", &card_id, &member.id, role.as_str(),
                                      "fact", ROLE_BASIS, None)?;
            out.provenance += 1;
        }
        // FIX-1：镜像卡的归属继承自**准入后的成员行**——与上面算句柄用的是同一批 fact id，
        // 出处点名的也是这批。归属不唯一（成员分属不同主人，或某条成员根本没有归属行）
        // ⇒ 一行都不写，这张卡照旧只有"署名恰等于读者"那一格放行（fail-closed）。
        // `episode` 结构行本身**没有归属面**（`hub/src/ownership.rs` 与 `gate.rs` 的行集是
        // card/anchor/fact/state_assertion，`MemTable` 也没有第四张表），所以这里只写卡那一行。
        if inherit_card_owner(&tx, ctx, &card_id, &ordered_ids, &scope, as_of_ms)?
            == Inheritance::Ambiguous
        {
            out.owner_ambiguous += 1;
        }
        // D3 的收尾：同归属、还活着、与自己共享至少一个成员的旧行/旧卡被指过去。
        // **按"共享成员"认前驱，不按句柄认**：前驱的句柄只存在于它自己那一行里（库里没有
        // 存 preimage），拿本批的成员重算只能算出**自己**的句柄；而 D3 要的"成员集变了就顶掉
        // 旧的"这件事，可观测的根据是 `episode_fact`/`provenance` 里那批成员是否重叠。
        // （FIX-2 之前这里另有一句"重算会算出第三个 id"，那是句柄含未 admitted 成员那一格的
        // 症状；修后 preimage 与落库内容同源，那一句不再成立，但按共享成员认前驱这条没变。）
        let member_ids: Vec<String> = members.iter().map(|f| f.id.clone()).collect();
        let (rows_out, cards_out) = supersede_predecessors(
            &tx, &scope, &id, &card_id, &member_ids)?;
        out.superseded += rows_out;
        out.superseded_cards += cards_out;
    }
    tx.commit()?;
    Ok(out)
}

/// 成员准入的执法点：读出那条 fact，并依次判它**有没有出处**、**还活着吗**。
/// 出处 = 有一行 `provenance(mem_table='fact', ref_kind='turn')`，**或**列上的
/// `evidence_turn_uid` 非空（母档 §3 那一条的两支，与 `reader::turn_basis_pred` 同一把尺子）。
/// 两支都没有 ⇒ `Ok(None)`（这一条不收，但整段经历照落——它是归组层递来的一条坏成员，
/// 不是库的故障；计数走 `dropped_members`）。
/// 行根本不在 ⇒ `Err`：不许把"没这条行"读成"少一个成员"。
///
/// **死活那一格（I-3d MF-2）**：出处齐了还要过 [`fact_row_is_dead`] 与 [`member_cards_are_silenced`]
/// 两支，任一命中 ⇒ 同样 `Ok(None)`。`dropped_members` 因此是"**被闸挡下的成员数**"，
/// 两种挡因（无出处 / 已判死）共享同一个读数出口——不许为后者新开一格，那会让
/// "拦了几条"变成两个口径。
/// 它挡的是这一条可达路径：成员那张**陈述面消费卡**被审核面消音（`rc-distill::audit` 票 1 →
/// `crate::writers_c::retire`，账落在 `mem_table='card'`）之后，那条 fact 仍然进新 episode 的
/// 成员表，再由一张 `mode='inferred'`、结构性进不了审核游标的镜像卡端回召回面
/// ⇒ **操作员的撤回被绕过，而新卡无法被二次消音**。这不是数据卫生，是撤回权，
/// 用例见 `tests/episode.rs` 的 `成员的镜像卡被消音后_新_episode_不再收它为成员`。
///
/// **明写判不到的那一族**：成员那一行 fact 自己的**否决账**
/// （`attestation.mem_table='fact'` 且 `stance='rejected'`）本层**不判**，两个理由都在场：
/// ① 它今天没有生产者——`MemTable` 只有 `Anchor|Card`（`keys.rs` 那一格），`attest` 与 `retire`
///    都要 `MemKey`，落不到 `'fact'`；② 那一支的谓词文本住 `reader::rejection_pred`，是
///    `reader` 模块私有而本文件不许动 `reader.rs`（派发档 I-3d §1）⇒ 在这里再写一份就是
///    "整段口径单源"破格。所以这一支**不声称判了**：真到有生产者那天，要做的是把那一支
///    导出来给两侧共用，而不是在这里抄一条 SQL。成员的否决面今天可达的形状走的是它的
///    镜像卡那一支（[`member_cards_are_silenced`] 里 `aj` 那半句覆盖的就是 `'card'`）。
fn admitted_member(conn: &rusqlite::Connection, fact_id: &str)
    -> Result<Option<MemberFact>, MemError> {
    let row = conn.query_row(
        "SELECT f.world_id, f.scope_kind, f.scope_key, f.occurred_at_ms, f.occurred_end_ms,
                f.created_at_ms, f.participants_json, f.evidence_turn_uid,
                EXISTS (SELECT 1 FROM provenance p
                         WHERE p.mem_table='fact' AND p.mem_id=f.id AND p.ref_kind='turn')
           FROM fact f WHERE f.id=?1",
        [fact_id],
        |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?, r.get::<_, Option<i64>>(4)?, r.get::<_, i64>(5)?,
                r.get::<_, String>(6)?, r.get::<_, String>(7)?, r.get::<_, i64>(8)?)),
    );
    let (world_id, kind, key, occurred_at_ms, occurred_end_ms, created_at_ms,
         participants_json, evidence_turn_uid, has_turn) = match row {
        Ok(v) => v,
        // 行不在 = 调用方递了一个不存在的成员（整批回滚，语义见函数文档）。
        Err(rusqlite::Error::QueryReturnedNoRows) =>
            return Err(MemError::Sql("episode member id is not a fact row".to_string())),
        Err(e) => return Err(MemError::from(e)),
    };
    if has_turn == 0 && evidence_turn_uid.trim().is_empty() { return Ok(None); }
    // 死活两支（顺序在出处之后：出处都不齐的行不必再去查账，两支各自都不写第二份谓词）。
    if fact_row_is_dead(conn, fact_id)? || member_cards_are_silenced(conn, fact_id)? {
        return Ok(None);
    }
    Ok(Some(MemberFact {
        id: fact_id.to_string(),
        world_id,
        // 脏 slug 落 `Project`：与 `reader::scope_of` 同一条下界策略（本族的 scope 只用来算句柄与
        // 判跨归属，认不出时不炸整批）。
        scope: Scope { kind: ScopeKind::parse(&kind).unwrap_or(ScopeKind::Project), key },
        occurred_at_ms,
        occurred_end_ms,
        created_at_ms,
        participants: crate::reader::json_array_strings(&participants_json),
    }))
}

/// 成员**那一行 fact 自己**的死活：`superseded_by` 指针（裸列）＋ 未解决的退场账。
/// 条件文本一家都不在这里写（守卫 3）：退场那一支逐字取 `reader::retirement_pred_on`，
/// 于是它与 `reader::fact_rows` 的活行三支、`reader::group_facts` 的 `fact_alive_where`
/// 是同一个家——"还活着的 fact"在两处必须是同一批行，否则视图少一行、归组多一段经历，
/// 那正是"库里有、下发看不见"那一族的镜像（I-3b 立的那条律）。
///
/// 这两形今天**没有生产者**（`MemTable` 无 `Fact` 变体 ⇒ `retire`/`supersede` 落不到 `'fact'`，
/// 中心实测 `retirement` 0 行、`fact.superseded_by` 非空 0 行）。写它不是凑数：派发档 I-3d
/// 要的是"口径与既有读者同源"，把口径钉死在这里，将来真有生产者那天分叉会当场红；
/// 用例 `成员那一行自己带着退场账或被顶掉指针时也不准入` 手搭两形把它钉住。
fn fact_row_is_dead(conn: &rusqlite::Connection, fact_id: &str) -> Result<bool, MemError> {
    let sql = format!(
        "SELECT EXISTS (SELECT 1 FROM fact f WHERE f.id=?1 \
           AND (f.superseded_by IS NOT NULL \
                OR EXISTS (SELECT 1 FROM retirement rt WHERE {})))",
        crate::reader::retirement_pred_on("rt", "'fact'", "f.id"));
    Ok(conn.query_row(&sql, [fact_id], |r| r.get::<_, i64>(0))? != 0)
}

/// 成员那张**陈述面消费卡**的死活——MF-2 里今天唯一生产者可及的那一支（也是它必须修的理由：
/// 审核面消音写的就是 `'card'` 的账）。
///
/// 反查走 `card.pointers_json` 里的 `fact:<id>` 指针（`writers_fact` 每落一条 fact 配一张指回
/// 它的卡）。比对用 SQLite 自己的 `json_each` 逐格相等，不用 `LIKE` 的子串近似：指针是一串
/// 定长 hex，`fact:ab` 的子串能命中 `fact:abc…`，而那是一种不报错的错（读法同
/// `snapshot::check_card_pointers`）。
///
/// **`mode='stated'` 那一格是实测逼出来的，不是修饰**：指回成员 fact 的卡有两族——抽取写的陈述面
/// 卡，和本族自己写的 episode 镜像卡（[`put_episodes`] 那一句 `mode='inferred'`）。后者的
/// `superseded_by` 说的是"这段经历被下一段顶掉了"（D3 的结构史），把它读成"成员被判死"会自噬：
/// 每一段新经历都会把上一段经历的成员全判死。不加这一格时
/// `同一批重跑`/`成员集变大` 两条用例当场红在 `dropped_members: 2`。推导面卡的生死归那一族自己
/// 的结构史，成员的生死只看它的陈述面卡（全仓 `src` 面写 `fact:` 指针的就这两族，逐条数过）。
///
/// 死活表达式逐字取 `reader::dead_by_id_expr(MemTable::Card)`——三支（未解决退场账、未解决
/// 否决账、行被顶掉）一家都不在这里写，与 [`put_batch_with_owner`] 的写前查账同一个家、
/// 同一个形状。写侧要这一支而不是读者的 `alive_where`：两本账按 id 查、与行在不在无关，
/// 所以副本上"卡被 prune 掉而退场账还留着"那一形同样拦得住（reason 见 reader 那一格）。
/// 代价如实登记：成员那张陈述面卡被**判同顶掉**（`superseded_by`，不是撤回）时这里也会拦，
/// 那段经历因此少一个成员。方向选 fail-closed 而不是拆掉那一支——拆支就要在这里写第二份谓词，
/// 而判同之后的活卡不再把指针落回这一行 fact，指针面本来就断了。
///
/// 两个口径要说清：① **任一**张陈述面卡判死就不收这一条成员（宁可少一段经历，也不把操作员
/// 处置过的内容端回召回面），② 一条 fact 一张陈述面卡都没有时**不判死**（库里没有那张卡
/// 就没有那本账；这一形不是"已消音"，不许在这里当消音处理）。
fn member_cards_are_silenced(conn: &rusqlite::Connection, fact_id: &str)
    -> Result<bool, MemError> {
    let pointer = format!("fact:{fact_id}");
    let mut card_ids: Vec<String> = Vec::new();
    {
        let mut st = conn.prepare(
            "SELECT c.id FROM card c, json_each(c.pointers_json) p
              WHERE p.value=?1 AND c.mode='stated'")?;
        for id in st.query_map([&pointer], |r| r.get::<_, String>(0))? {
            card_ids.push(id?);
        }
    }   // 语句先离开 conn：下面逐张卡按 id 查账，两条读必须在同一个事务快照里排队。
    let sql = format!("SELECT {}", crate::reader::dead_by_id_expr(MemTable::Card));
    for card_id in &card_ids {
        let dead: i64 = conn.query_row(&sql, [card_id], |r| r.get(0))?;
        if dead != 0 { return Ok(true); }
    }
    Ok(false)
}

/// 成员必须共享同一个归属（母档 §3："不跨 scope"）。
/// `world_id` 一起比：`card_scope`/`fact_scope` 两条索引都是 `(world_id, scope_kind, scope_key)` 三元组，
/// 只比后两格就会把两个世界合成一段经历。
/// 跨了就是调用方（归组层）的错 ⇒ 整批 `Err`、零行，**不静默切一半**（`put_fact_rows_with_extras`
/// 里 `extras` 长度对不上走的正是这一支）。错误载荷只带一句静态话（守卫 12）。
fn shared_member_scope(members: &[MemberFact]) -> Result<(Scope, Option<String>), MemError> {
    let first = &members[0];
    if members[1..].iter().any(|m| {
        m.scope.kind != first.scope.kind || m.scope.key != first.scope.key
            || m.world_id != first.world_id
    }) {
        return Err(MemError::Sql("episode members do not share one scope".to_string()));
    }
    Ok((Scope { kind: first.scope.kind, key: first.scope.key.clone() }, first.world_id.clone()))
}

/// 把前驱指过去（D3 的收尾，本函数是这一族里唯一的 UPDATE 点）。
///
/// 判据是"**同归属、还活着、共享至少一个成员**"，两族各认各的句柄：
/// ① 旧 `episode` 行（`superseded_by IS NULL AND status<>'superseded'`）⇒
///    `status='superseded'` + `superseded_by=新行 id`；
/// ② 旧镜像卡（`kind='episode'` 且它的 basis 出处点到了同一个成员）⇒
///    `superseded_by=新卡 id`。
/// 为什么要**两族各查一遍**而不是由①算出②：两族的句柄各住在自己的表里（结构行的 id 在
/// `episode.id`、镜像卡的 id 在 `card.id`，同源但字面不同），①那一问给不出②那一串。
/// 而 D3 的规则是"**成员集变了** ⇒ 顶掉旧的"，不是"句柄差一个元素"，所以这里既不重算前驱
/// 句柄、也不依赖 FIX-2 之后"`episode` 句柄的 preimage 与 `episode_fact.ord` 同一把尺子"
/// 那格新性质（修前两者不同源，重算会算出第三个 id；修后同源，但那是可省的一次哈希，
/// 不是判据）。
/// `AND superseded_by IS NULL` 是并发位：同一对前后行被两台设备同时顶时，后提交那一边命中 0 行，
/// 不会把已经指过的再指一次。返回 `(顶掉的行数, 顶掉的卡数)`。
fn supersede_predecessors(
    tx: &rusqlite::Transaction, scope: &Scope, new_id: &str, new_card_id: &str,
    member_ids: &[String],
) -> Result<(usize, usize), MemError> {
    let mut rows: BTreeSet<String> = BTreeSet::new();
    let mut cards: BTreeSet<String> = BTreeSet::new();
    {
        let mut st = tx.prepare(
            "SELECT e.id FROM episode e JOIN episode_fact ef ON ef.episode_id = e.id
              WHERE ef.fact_id=?1 AND e.scope_kind=?2 AND e.scope_key=?3
                AND e.id<>?4 AND e.superseded_by IS NULL AND e.status<>'superseded'")?;
        for fact_id in member_ids {
            let got = st.query_map(
                rusqlite::params![fact_id, scope.kind.as_str(), scope.key, new_id],
                |r| r.get::<_, String>(0))?;
            for id in got { rows.insert(id?); }
        }
    }
    {
        let mut st = tx.prepare(
            "SELECT p.mem_id FROM provenance p JOIN card c ON c.id = p.mem_id
              WHERE p.mem_table='card' AND p.ref_kind='fact' AND p.turn_uid=?1
                AND c.kind='episode' AND c.id<>?2 AND c.superseded_by IS NULL")?;
        for fact_id in member_ids {
            let got = st.query_map(rusqlite::params![fact_id, new_card_id],
                                   |r| r.get::<_, String>(0))?;
            for id in got { cards.insert(id?); }
        }
    }
    let mut n_rows = 0usize;
    for old in &rows {
        n_rows += tx.execute(
            "UPDATE episode SET status='superseded', superseded_by=?1
             WHERE id=?2 AND superseded_by IS NULL",
            rusqlite::params![new_id, old],
        )?;
    }
    let mut n_cards = 0usize;
    for old in &cards {
        n_cards += tx.execute(
            "UPDATE card SET superseded_by=?1 WHERE id=?2 AND superseded_by IS NULL",
            rusqlite::params![new_card_id, old],
        )?;
    }
    Ok((n_rows, n_cards))
}

/// episode 写口的校验（在任何一个事务开始**之前**，同 `put_batch`/`put_state_rows` 的时点）。
/// 空白内容走 `ContentEmpty`（既有三族同形）；成员那一格的三种坏（空表、空串 id、批内重复）
/// 走 `BadA1Field("members")`——**复用**而不新开端：`MemError` 是
/// `rc-agent/src/error.rs` 那张逐变体点名的分型表的输入，新增一支会逼那个文件补 match 臂。
///
/// 批内重复必须报错而不是让主键吞掉：`episode_fact` 的 `PRIMARY KEY(episode_id, fact_id)`
/// 会把第二条 `INSERT OR IGNORE` 掉，而句柄的 preimage 里它是两条 ⇒ "计划的成员数"与
/// "落库的成员数"从此分叉，不报错也不红（"写了没人读"那一族的另一种形状）。
fn validate_episodes(rows: &[EpisodeContent]) -> Result<(), MemError> {
    for ep in rows {
        if normalize_body(&ep.title).is_empty() || normalize_body(&ep.narrative).is_empty() {
            return Err(MemError::ContentEmpty);
        }
        if ep.fact_ids.is_empty() || ep.fact_ids.iter().any(|id| id.trim().is_empty()) {
            return Err(MemError::BadA1Field("members"));
        }
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        if !ep.fact_ids.iter().all(|id| seen.insert(id.as_str())) {
            return Err(MemError::BadA1Field("members"));
        }
    }
    Ok(())
}



/// 写 mention（批次 M 合证面 · 这一族**唯一**的生产写入口）：一行 = "某事实行提到了某实体"。
/// **整批一个事务**：任何一步失败 ⇒ 一条都不写（同 `put_edges` / `put_chains`）。
/// `source_agent` / `device_id` 只从 `ctx` 取（守卫 11），`norm_version` 只从
/// `rc_store::capability::NORM_VERSION` 取（不写字面量）。`from_kind` 由 `table_of()` 现算
/// （模型与调用方都递不进）。
///
/// 名字→实体 id 的四条纪律：
/// ① **多义名字落全部匹配**（`entity_ids_for_name` 命中几条写几条）：近名不替模型选；
/// ② **零匹配不造实体**、只计 `unresolved`：合证标注是派生数据，缺实体就是没提到库里的实体，
///    凭空落一个实体行是"造新实体"（反例四支第 4 条）的形状；
/// ③ **幂等的前提是段 A 的 `PRIMARY KEY(from_kind, from_id, entity_id)`**（ddl_shape 已有用例）：
///    同一次调用重跑、或换一个解析到同一实体的名字（标题 vs 别名）重跑，`INSERT OR IGNORE` 都命中 0；
/// ④ `entity_ids_for_name` 在事务内只读**同一连接**：实体行早已提交，不存在可见性问题
///    （读的是本连接未提交事务之前的状态，不是另一条连接的快照）。
pub fn put_mentions(
    m: &MemoryHandle, mentions: &[MentionContent], ctx: &BatchCtx, now_ms: i64,
) -> Result<MentionOutcome, MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }   // 与其余九个写入口同一句
    let conn = m.conn();
    let tx = conn.unchecked_transaction()?;
    let mut out = MentionOutcome::default();
    for ment in mentions {
        let anchor_ids = crate::reader::entity_ids_for_name(m, &ment.name)?;
        if anchor_ids.is_empty() { out.unresolved += 1; continue; }
        let from_kind = table_of(&tx, &ment.row_id)?;
        for anchor_id in anchor_ids {
            // I6：锚行的自指 mention（行本身就是这个锚）是纯噪声，不落库。
            // 只拦"行本身就是这个锚"那一格；锚 A 提到锚 B 仍然是合法 mention。
            if from_kind == MemTable::Anchor && ment.row_id == anchor_id {
                out.self_skipped += 1;
                continue;
            }
            let n_ins: usize = tx.execute(
                "INSERT OR IGNORE INTO memory_mention
                     (mem_table, mem_id, anchor_id, source_agent, device_id, created_at_ms, norm_version)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                rusqlite::params![from_kind.prefix(), ment.row_id, anchor_id,
                                  ctx.source_agent, ctx.device_id, now_ms,
                                  rc_store::capability::NORM_VERSION as i64],
            )?;
            out.written += n_ins;
        }
    }
    tx.commit()?;
    Ok(out)
}

/// 推导行的出处行（依据 + 因果链的环节）——带 `ref_kind` 的那一条**唯一写入点**（守卫 3：同一条
/// SQL 在 `put_edges` 与 `put_timeline_rows` 各写一份，改一处漏一处就是 §8 五开关最容易悄悄失效的
/// 形状；批次③ 的环节行因此**扩展这一格**而不是新写第二条 `INSERT INTO provenance`）。
/// **私有**：本阶段没有库外调用者（T6/T7 只读 provenance，不写它），所以它不进 `lib.rs` 的
/// re-export——一个零读者的 `pub fn` 就是"新增读数必须有真实生产者"那条通则的反面（P-44）。
///
/// `role` / `ord` 两格（批次③ 因果链）由这一格一并盖章：调用方只许递 `ROLE_BASIS` 或 `ROLE_LINK`
/// 这两个常量，依据传 `ord = None`、环节传 `ord = Some(0..)`。`role='link' ⇔ ord IS NOT NULL` 那一半
/// 由库里的跨列 CHECK 执法，**值域那一半库不管**：域外 slug 配 NULL `ord` 落得进库也读得回来
/// （rc-store 的 phase3 用例把这一格钉成事实），所以拼错的词只能由写侧的常量与 rc-memory 的
/// `put_chains` 用例拦住。
// 参数个数是批次③ 段 B 加的 `role`/`ord` 两形参推上去的（⇒ 8 个）。不合并成结构体：那会把
// "唯一出处写入点"那条守卫（守卫 3）要读的东西——**哪一格进哪一列**——藏进一个只有这一格认识的结构里。
// 同文件先例：`write_item_in` 与 `write_fact` 上面各挂着同一句 allow。
#[allow(clippy::too_many_arguments)]
fn insert_derived_provenance(
    tx: &rusqlite::Transaction, mem_table: &str, mem_id: &str,
    basis_id: &str, anchor_role: &str, ref_kind: &str,
    role: &str, ord: Option<i64>,
) -> Result<(), MemError> {
    tx.execute(
        "INSERT OR IGNORE INTO provenance(mem_table, mem_id, turn_uid, anchor_role, ref_kind, role, ord)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![mem_table, mem_id, basis_id, anchor_role, ref_kind, role, ord],
    )?;
    Ok(())
}

// 助手：从行 id 找到 MemTable（两张可指认表各查一次）
fn table_of(conn: &rusqlite::Connection, id: &str) -> Result<MemTable, MemError> {
    for t in [MemTable::Card, MemTable::Anchor] {
        let p = t.prefix();
        let n: i64 = conn.query_row(&format!("SELECT 1 FROM {p} WHERE id=?1"), [id],
                                    |r| r.get(0)).unwrap_or(0);
        if n == 1 { return Ok(t); }
    }
    // 走到这里 = 选批与落库之间那一行没了（本阶段的唯一可能是并发退场）。
    // **不折进 `MemError::NotFound`**：那个变体的载荷是 `MemKey`，而这里恰恰不知道它在哪张表。
    // 用 `Sql(..)` 是既有先例——`rc-memory` 的 `decode_raw` 与 `all_titles` 里那两处
    // "reader got an unknown mem_table slug"、`rc-distill::dedupe` 里那句
    // "extract evidence holds an unknown table slug"都走这一支。
    Err(MemError::Sql("derived basis id is in no fact table".into()))
}
// 助手：取行的锚角色（provenance 行按 human<agent<system 排序取第一支 ⇒ 天然最强）
// **零出处行降级为 `System` 而不是炸整批**（I-1）：本模块自己会造出零出处的行——无锚的 timeline 行
// 落库时 provenance 为零行，而 `rows_since` 走全四表无 kind 过滤 ⇒ 它会在下一轮重新进批；
// 模型拿它当边的端点时，若这里 `?` 出去，整批 `put_edges` 回滚、`derive_pass` 返 `Store` 族错误，
// T5 的 `run_derive` 拿到它按"库故障"不推水位 ⇒ **同一批被反复重试、cycle 卡死**。
// 取最弱而不取最强：零出处 = 没有任何 actor 对它表过态，虚报 `Human` 会让
// "最强"这个读数开始说谎，而它是日后召回时判断"这条能不能信"的一列。
fn anchor_role_of(conn: &rusqlite::Connection, id: &str) -> Result<AnchorRole, MemError> {
    let s: Option<String> = match conn.query_row(
        "SELECT anchor_role FROM provenance WHERE mem_id=?1 ORDER BY
            CASE anchor_role WHEN 'human' THEN 0 WHEN 'agent' THEN 1 WHEN 'system' THEN 2 END
         LIMIT 1", [id],
        |r| r.get(0),
    ) {
        Ok(s) => Some(s),
        // 只这一支降级：I/O 错误与锁等待仍原样上抛，压成"没有出处锚"会让排障看到假原因（M-2）。
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(e.into()),
    };
    match s {
        Some(s) => AnchorRole::parse(&s)
            .ok_or_else(|| MemError::Sql(format!("provenance anchor_role unreadable: {s}"))),
        None => Ok(AnchorRole::System),
    }
}
// 助手：Human > Agent > System
fn strongest(a: AnchorRole, b: AnchorRole) -> AnchorRole {
    fn rank(r: AnchorRole) -> u8 { match r { AnchorRole::Human => 0, AnchorRole::Agent => 1, AnchorRole::System => 2 } }
    if rank(a) <= rank(b) { a } else { b }
}

// ── I4/I6 的文本锚：别名与标题共用一把尺子 ────────────────────────────────────────────────
//
// 判据是"归一后的字母数字串互相包含"，不是原样 `contains`：审计里的三处漏铸
// （`3.0-flash` ↔ `Agnes 3.0 Flash`、`agnes-ai` ↔ `Agnes AI`）差的只是大小写、空格与连字符。
// 归一 = 小写 + 只留 `char::is_alphanumeric`（CJK 也算 alnum）⇒ 空格、连字符、下划线、
// 括号全部消失。太短的键不参与锚定（ASCII 4 字符 / CJK 2 字符），否则 "ai" 这种子串会让
// 全库互相锚上。
fn norm_anchor_key(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn anchor_key_long_enough(k: &str) -> bool {
    let cjk = k.chars().any(|c| (c as u32) >= 0x2E80);
    if cjk { k.chars().count() >= 2 } else { k.chars().count() >= 4 }
}

/// 一个实体名的锚键集合：整串 + **相邻词元窗口**（2~3 个词元，按空白/标点切）。
/// 理由：审计的 `3.0-flash` ↔ `Agnes 3.0 Flash` 差的不是连字符，是标题比正文多了
/// `Agnes ` 这个前缀——整串包含判不出来；`3.0 Flash` 的窗口键 `30flash` 两边都有。
/// **不收单字词元**：`agnes` 这种单词元会把 `agnes-ai` 误锚到 `Agnes 3.0 Flash`。
///
/// 读者不止本模块：A1 写入口（`writers_v4`）用**同一把尺子**判 mention 的 surface 落在
/// 哪个锚上（P3-5）。两处共用这一份实现，不新写第三把——换尺子必须两处一起换。
pub(crate) fn anchor_keys(raw: &str) -> Vec<String> {
    let mut out = BTreeSet::new();
    let full = norm_anchor_key(raw);
    if anchor_key_long_enough(&full) { out.insert(full); }
    let tokens: Vec<String> = raw.split(|c: char| !c.is_alphanumeric())
        .map(norm_anchor_key)
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.len() >= 2 {
        for size in 2..=3 {
            for w in tokens.windows(size) {
                let k = w.concat();
                if anchor_key_long_enough(&k) { out.insert(k); }
            }
        }
    }
    out.into_iter().collect()
}

/// 实体标题 + 全部别名 ⇒ 归一锚键。**一次读全表**（每批几十行、实体数百行），
/// 调用方在事务外按需缓存；这里不做 N+1 查询。
fn entity_anchor_index(conn: &rusqlite::Connection)
    -> Result<Vec<(String, Vec<String>)>, MemError> {
    let mut by_entity: BTreeMap<String, Vec<String>> = BTreeMap::new();
    {
        let mut st = conn.prepare("SELECT id, canonical FROM anchor ORDER BY id")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (id, title) = row?;
            by_entity.entry(id).or_default().extend(anchor_keys(&title));
        }
    }
    {
        let mut st = conn.prepare("SELECT anchor_id, alias FROM alias ORDER BY anchor_id, alias")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (id, alias) = row?;
            by_entity.entry(id).or_default().extend(anchor_keys(&alias));
        }
    }
    Ok(by_entity.into_iter().map(|(id, keys)| {
        let uniq: BTreeSet<String> = keys.into_iter().collect();
        (id, uniq.into_iter().collect())
    }).collect())
}

/// 一行的可锚文本：标题 + 正文（anchor 没有正文列 ⇒ 只有 `canonical`）。
fn row_anchor_text(conn: &rusqlite::Connection, id: &str) -> Result<String, MemError> {
    let (title, body) = row_title_and_body(conn, id)?;
    Ok(if body.is_empty() { title } else { format!("{title} {body}") })
}

/// 标题与正文分读：I5 的复述判据要分别比（整串里标题的长尾会把 body 的 Jaccard 稀释）。
/// card 的正文列是 `text`；anchor 只读 `canonical`。
fn row_title_and_body(conn: &rusqlite::Connection, id: &str) -> Result<(String, String), MemError> {
    let table = table_of(conn, id)?;
    if table == MemTable::Anchor {
        return Ok((conn.query_row(
            "SELECT canonical FROM anchor WHERE id=?1",
            [id], |r| r.get::<_, String>(0))?, String::new()));
    }
    Ok(conn.query_row(
        "SELECT title, text FROM card WHERE id=?1",
        [id], |r| Ok((r.get(0)?, r.get(1)?)))?)
}

/// 两个正文是否**逐字共享同一个实体**（别名文本锚）。返回命中的实体 id（用于去重与计数）。
fn shared_text_anchor(a: &str, b: &str, index: &[(String, Vec<String>)]) -> Option<String> {
    let ka = norm_anchor_key(a);
    let kb = norm_anchor_key(b);
    for (entity_id, keys) in index {
        if keys.iter().any(|k| ka.contains(k.as_str()) && kb.contains(k.as_str())) {
            return Some(entity_id.clone());
        }
    }
    None
}

/// 某行在 `memory_mention` 表里指向的锚 id 集合（共享 mention 判据的读面）。
fn mention_ids_of(conn: &rusqlite::Connection, id: &str) -> Result<HashSet<String>, MemError> {
    let table = table_of(conn, id)?;
    let mut st = conn.prepare("SELECT anchor_id FROM memory_mention WHERE mem_table=?1 AND mem_id=?2")?;
    let rows = st.query_map(rusqlite::params![table.prefix(), id], |r| r.get::<_, String>(0))?;
    let mut out = HashSet::new();
    for r in rows { out.insert(r?); }
    Ok(out)
}

/// 行的 `created_at_ms`（I3：源轮时刻的唯一读法；derive 侧的时间锚从这一列取）。
fn created_at_of(conn: &rusqlite::Connection, id: &str) -> Result<i64, MemError> {
    let table = table_of(conn, id)?;
    Ok(conn.query_row(
        &format!("SELECT created_at_ms FROM {} WHERE id=?1", table.prefix()),
        [id], |r| r.get(0))?)
}


/// §13 A40 的闭包互挂写：给一对实体行补上**反向**那条别名，让两侧互查都命中。
///
/// 动作面只有这一件事：**不动行、不转 refs、不改归属**。A40 把"行级合并"整支留给后续阶段，
/// 所以这一支连 `anchor` 表的一列都不写——它是 `alias` 表除 `write_item_in` 之外的
/// 第二个写入口，也是唯一一个由维护作业触发的 X 族入口。
///
/// 三条纪律：
/// ① **闸门跟着写入口走**：写入的值取对侧的 `title`，所以本函数自己过一次 `check_alias`
///    （守卫 10）。不是因为上游可能漏，而是因为这里就是写入口——§2.13 那条"校验在任何一笔
///    写入之前"在这一支同样是先校验、后动手（两条 `check_alias` 都在 `tx` 里、`INSERT` 之前，
///    任一条拦下就整个事务不提交，不留半挂状态）。
/// ② **两条都 `INSERT OR IGNORE`** ⇒ 重复调用幂等（`t1` 断言第二遍插入 0 行、总数不变）。
///    主键 `(anchor_id, alias)` 是这条幂等的物理根据（`rc-store/src/ddl.rs` 的 CREATE TABLE）。
/// ③ **单事务**：两次写要么全成要么全无。helper 收 `&Transaction` 而不是自开一层，
///    与本文件的 `write_item_in` 同形（rusqlite 在同一连接上开第二个事务会走到 SQLite 的
///    `BEGIN` 重入错误）。
///
/// 对侧必须是锚行：`SELECT canonical FROM anchor WHERE id=?1` 查不到就是 `NotFound`——
/// 这一读本身就是那条约束的执法处，所以这里不再写第二遍表名判断（多写一遍就是两套口径）。
pub fn link_alias_pair(m: &MemoryHandle, a: &MemKey, b: &MemKey) -> Result<usize, MemError> {
    if m.is_read_only() { return Err(MemError::ReadOnly); }
    let tx = m.conn().unchecked_transaction()?;
    let read_title = |id: &str| -> Result<String, MemError> {
        tx.query_row("SELECT canonical FROM anchor WHERE id=?1", [id], |r| r.get::<_, String>(0))
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows =>
                    MemError::NotFound(MemKey { table: MemTable::Anchor, id: id.to_string() }),
                other => MemError::from(other),
            })
    };
    let at = read_title(&a.id)?;
    let bt = read_title(&b.id)?;
    check_alias(&at)?;
    check_alias(&bt)?;
    let mut linked = 0usize;
    linked += tx.execute("INSERT OR IGNORE INTO alias(anchor_id, alias) VALUES(?1,?2)",
                         rusqlite::params![b.id, at])?;
    linked += tx.execute("INSERT OR IGNORE INTO alias(anchor_id, alias) VALUES(?1,?2)",
                         rusqlite::params![a.id, bt])?;
    tx.commit()?;
    Ok(linked)
}
