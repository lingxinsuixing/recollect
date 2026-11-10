//! fact/event 两个 pass 共用的**候选锚池**（S2e-7 的位置契约 + 归属可见性过滤）。
//!
//! 原先两个 pass 各有一份 `load_candidates`（逐行相同的 SQL），于是同一条隐私缺陷
//! 也有两份：`SELECT id,kind,canonical FROM anchor WHERE status='active'` **不按归属过滤**，
//! 任何会话的私有锚都会进入下一批的 `a1..aN`，模型可以把它选成主语而闸门只校验行 owner。
//! 这里把候选池收成一份实现，`pass_fact`/`pass_event` 只调用、不再各写一条 SQL。
//!
//! 两条口径：
//! ① **顺序契约不变**：仍是"最近创建的在前、同刻按 id"，`a1..aN` 按位置编号；
//!    会话自锚仍由调用方补在最后一格（`append_agent_anchor`），过滤只删行、不改序。
//! ② **fail-closed**：锚必须有 `resource_owner` 行、owner 已解决，且 owner 是本人或
//!    visibility 非 `private_agent` 且 scope 全等；谓词文本住在
//!    [`rc_memory::owner::anchor_visible_pred`]，与程序闸的单锚检查**同一份文本**。
//!
//! `CANDIDATE_LIMIT` 仍是"最近创建的 30 条**可见**锚"：`LIMIT` 挂在过滤之后（同一条 SQL 的
//! WHERE 里），不是先取 30 条再筛——否则一批私有锚就能把共享锚挤出候选表。
//!
//! C3 起多一层 [`order_by_evidence`]：名字出现在本批目标轮正文的候选排到前面，其余保持原序。
//! 它**只重排、不删**（过滤仍是上面那一条谓词的事），重排后按新位置重编 `a1..aN`。
//!
//! P4a-1 起再补一层 [`insert_mentioned_candidates`]（只给 event pass 调）：候选池是"最近
//! 创建的 30 条"，正文点名的人可能落在池外——池里没有他，模型就没有 `a*` 可写，actor 只能
//! 塌回说话人。这一层把本批正文里出现的**库内已有锚**补进候选，来源仍只用 `anchor`/`alias`
//! 两表，不新造锚、不调模型；补位上限 6 个，插在两格程序锚（自锚/主人锚）之前。没有点名、
//! 或名字在库里没有对应锚时一格都不补——候选列表与改动前逐字一致。
//!
//! D 起同一位置规则再服务一层 [`insert_speaker_candidates`]（也只给 event pass 调）：
//! 子 agent 的轮级说话人锚（`mention.discourse_role='speaker'`）可能在池外，池里没有它，
//! TARGET 行就只能写成主 agent 自锚的 `a*`——子 agent 的实体归属当场丢失。

use crate::batch::TurnRow;
use crate::pass_prompt::FactCandidate;
use crate::prepare::prepare;
use crate::DistillError;
use rc_contract::normalize_body;
use rc_memory::owner;
use rc_memory::Scope;
use rc_store::MemoryHandle;
use rusqlite::named_params;
use std::collections::{HashMap, HashSet};

/// 候选锚上限：只作"主语在哪"的提示，不含任何 id（`FactCandidate` 的 `anchor_id` 不进 prompt）。
const CANDIDATE_LIMIT: i64 = 30;

/// 本批正文点名的第三方锚最多**新补**几格（P4a-1）。池里已有的命中不占这个名额：
/// 它们本来就在候选里，补位层对它们只做 `anchor_id` 去重。
const MENTIONED_LIMIT: usize = 6;

/// 候选池的一行。`scope_*` 与 `created_at_ms` 是池内去重要用的两样，
/// `id` 只用于赢家规则的最后一格（同刻同组时取字典序小的）。
struct PoolAnchor {
    anchor_id: String,
    kind: String,
    canonical: String,
    scope_kind: String,
    scope_key: String,
    created_at_ms: i64,
}

/// 已有活锚候选，**只含对这批的归属上下文可见的那些**。
///
/// `owner_agent_id`/`scope` 取会话行（`select_*_batch` 已按 COALESCE 归一）；
/// 无归属行或 owner 未解决的锚一条都不返回（见模块文档）。
///
/// W15b2：读回来的池先按 `(归一正名, scope)` 去重，同一格只留一格——落库侧已经堵住
/// 新草案撞旧锚的第二格，但存量里同一正名挂两个 kind 的锚仍在，照着原样渲染就是
/// 把"这两个格子是同一个物"这件事交给模型猜。去重只删行、不新增、不改 kind/别名，
/// 编号仍按最终位置重编。
pub(crate) fn load_candidates(
    m: &MemoryHandle,
    owner_agent_id: &str,
    scope: &Scope,
) -> Result<Vec<FactCandidate>, DistillError> {
    let sql = format!(
        "SELECT a.id, a.kind, a.canonical, a.scope_kind, a.scope_key, a.created_at_ms
           FROM anchor AS a
          WHERE a.status='active' AND {}
          ORDER BY a.created_at_ms DESC, a.id LIMIT :limit",
        owner::anchor_visible_pred("a.id")
    );
    let anchors: Vec<PoolAnchor> = {
        let conn = m.conn();
        let mut st = conn.prepare(&sql)?;
        let rows = st.query_map(
            named_params! {
                ":viewer_agent": owner_agent_id,
                ":vis_person": owner::visibility_slug(owner::Visibility::Person),
                ":vis_project": owner::visibility_slug(owner::Visibility::Project),
                ":vis_org": owner::visibility_slug(owner::Visibility::Org),
                ":vis_public": owner::visibility_slug(owner::Visibility::Public),
                ":viewer_scope_kind": scope.kind.as_str(),
                ":viewer_scope_key": scope.key,
                ":limit": CANDIDATE_LIMIT,
            },
            |r| {
                Ok(PoolAnchor {
                    anchor_id: r.get(0)?,
                    kind: r.get(1)?,
                    canonical: r.get(2)?,
                    scope_kind: r.get(3)?,
                    scope_key: r.get(4)?,
                    created_at_ms: r.get(5)?,
                })
            },
        )?;
        rows.collect::<Result<_, _>>()?
    };
    Ok(dedupe_pool(anchors)
        .into_iter()
        .enumerate()
        .map(|(index, row)| FactCandidate {
            reference: format!("a{}", index + 1),
            kind: row.kind,
            // 池里的锚显示**它自己的正名**：语料里的张三就是张三——别名是"这个名字也指同一格"
            // 的证据，不是来替换正名的（把别名当显示名会让 `张三/老张` 在同批里漂，
            // 也会把预筛的命中判据改样）。唯一需要换可读名的是**主人锚**：它的正名是账号主键，
            // 由 `pass_event::append_principal_anchor` 单独取 `reader::display_name`。
            canonical: row.canonical,
            anchor_id: row.anchor_id,
            // 池里的锚没有身份章：`self`/`principal` 只由调用方的 `append_*_anchor` 盖。
            mark: None,
        })
        .collect())
}

/// 池内按 `(归一正名, scope_kind, scope_key)` 去重，每组只留一格。
///
/// 赢家规则与写侧跨 kind 合流**同一条**：`active` 优先（池的 WHERE 已经把状态钉成
/// active，所以这一格在这里恒成立）、然后入库时刻最小的、再然后 id 字典序最小的。
/// 池的 SQL 给的是 DESC 序，**不能**顺手取"每组第一格"——那是最新的一格。
///
/// 位置纪律：删的是每个组里的非赢家，留下的行仍按 SQL 给的原序排列（去重后是原序列的
/// 子序列，顺序契约不变）；不同 scope 的同名锚各留一格——scope 是身份的第三维，跨 scope
/// 去重会把两个不同归属的实体并成一格。
fn dedupe_pool(rows: Vec<PoolAnchor>) -> Vec<PoolAnchor> {
    let mut winners: HashMap<(String, String, String), usize> = HashMap::new();
    for (index, row) in rows.iter().enumerate() {
        let key =
            (normalize_body(&row.canonical), row.scope_kind.clone(), row.scope_key.clone());
        match winners.get(&key) {
            Some(&best) if pool_winner_precedes(row, &rows[best]) => {
                winners.insert(key, index);
            }
            Some(_) => {}
            None => {
                winners.insert(key, index);
            }
        }
    }
    let keep: HashSet<usize> = winners.into_values().collect();
    rows.into_iter()
        .enumerate()
        .filter(|(index, _)| keep.contains(index))
        .map(|(_, row)| row)
        .collect()
}

/// `a` 是否比同组的 `b` 更该当那唯一一格（见 [`dedupe_pool`] 的赢家规则）。
fn pool_winner_precedes(a: &PoolAnchor, b: &PoolAnchor) -> bool {
    (a.created_at_ms, a.anchor_id.as_str()) < (b.created_at_ms, b.anchor_id.as_str())
}

// ── 证据内预筛（C3）：只重排，不删 ───────────────────────────────────────────
//
// 候选表此前只有 `created_at DESC` 一条序：证据轮正文里明明写着"李四拆开清理"，
// 李四却可能排在 `a7`，模型要在 30 格短引用里逐个比对名字。这一层把"名字出现在本批
// **目标轮**正文"的候选提到前面，其余保持原序——**不裁任何一格**：候选池的 owner/scope
// 过滤已在 `27e7386` 落地，这里只改顺序。
//
// 三条纪律：
// 1. **命中判据是名字**：canonical 或该锚的任一 `alias`（`alias` 表）逐字出现在
//    任一目标轮正文里。两边都先过 `prepare`——模型看到的是剪过的正文，判断同一个字面。
// 2. **稳定**：命中的候选之间、未命中的候选之间各自保持原相对顺序（池内即
//    `created_at DESC, id`；自锚/主人锚按 `append_*_anchor` 的顺序接在池后，被点名时
//    同样前移）。不做二次排序，模型看到的顺序可复算。
// 3. **位置即契约**：重排后按新位置重编 `a1..aN`，与渲染/parser 同源；同一批内编号唯一。

/// 这段正文里是否出现这个名字（两侧都已 `prepare`）。
///
/// 空名字不命中：`canonical` 是空串时 `contains("")` 恒真，会把整表都判成命中。
fn name_in_bodies(name: &str, bodies: &[String]) -> bool {
    !name.is_empty() && bodies.iter().any(|body| body.contains(name))
}

/// 把一个锚的名字面读全：`canonical`（调用方已有）+ `alias` 表（升序）。
/// naming 的候选表也要这一份（它要判"这些表面是不是同一个物"），所以与池同一文件、同一把尺。
pub(crate) fn aliases_of(m: &MemoryHandle, anchor_id: &str) -> Result<Vec<String>, DistillError> {
    let mut st = m.conn().prepare("SELECT alias FROM alias WHERE anchor_id=?1 ORDER BY alias")?;
    let rows = st.query_map([anchor_id], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// 纯重排：`hits[i]` 为真的候选在前，各自保持原相对顺序；重排后按位置重编 `a1..aN`。
///
/// 与"算命中"分开，是为了让顺序本身可被单测钉住（不必造库）；生产路径见
/// [`order_by_evidence`]。
pub(crate) fn promote_evidence_hits(candidates: &mut Vec<FactCandidate>, hits: &[bool]) {
    debug_assert_eq!(hits.len(), candidates.len(), "命中位与候选必须同长");
    let mut kept: Vec<FactCandidate> = Vec::with_capacity(candidates.len());
    let mut rest: Vec<FactCandidate> = Vec::new();
    for (candidate, hit) in std::mem::take(candidates).into_iter().zip(hits.iter().copied()) {
        if hit {
            kept.push(candidate);
        } else {
            rest.push(candidate);
        }
    }
    kept.extend(rest);
    renumber(&mut kept);
    *candidates = kept;
}

/// 证据内预筛（C3）：名字（canonical / alias）出现在本批任一目标轮正文的候选排到前面，
/// 其余保持 `created_at DESC` 的原序。**只重排，不删**，也不新增候选。
///
/// `rows` 是本批的目标轮（`batch.rows`，不含 CTX 邻轮）：只有证据正文才算"本批证据"。
pub(crate) fn order_by_evidence(
    m: &MemoryHandle,
    candidates: &mut Vec<FactCandidate>,
    rows: &[crate::batch::TurnRow],
) -> Result<(), DistillError> {
    let bodies: Vec<String> = rows.iter().map(|row| prepare(&row.body)).collect();
    let mut hits = Vec::with_capacity(candidates.len());
    for candidate in candidates.iter() {
        let canonical = prepare(&candidate.canonical);
        if name_in_bodies(&canonical, &bodies) {
            hits.push(true);
            continue;
        }
        // canonical 不中才去读别名：多数候选一次就命中（或一次都不中）。
        let aliases = aliases_of(m, &candidate.anchor_id)?;
        hits.push(aliases.iter().any(|alias| name_in_bodies(&prepare(alias), &bodies)));
    }
    promote_evidence_hits(candidates, &hits);
    Ok(())
}

// ── 第三方锚补位（P4a-1） ────────────────────────────────────────────────────
//
// 候选池是"最近创建的 30 条可见锚"：一轮里点名的人只要不在这个窗口里（或命名 pass 还没把
// 他排到前面），CANDIDATES 段就没有他的 `a*`。P1 的"actor 必须是 quote 点名的 a-ref"堵住了
// 乱写，却没有给出可写的格子——actor 于是恒落回说话人（a1）。这一层只做一件事：把本批正文
// 里**字面出现**的库内已有锚补进候选。
//
// 三条纪律：
// 1. **来源只用库里已有的锚**：`anchor.canonical` 与 `alias.alias`，不新造锚、不调模型；
//    可见性沿用候选池同一条谓词（别人 owner 的私有锚一格都不补）。
// 2. **排序与上限**：先按"出现在几轮里"降序，再按 canonical 字典序；最多新补 `MENTIONED_LIMIT`
//    个。已在候选里的锚按 `anchor_id` 去重（主人锚被点名时不会出现第二格，I2 不变）。
// 3. **位置纪律**：补位插在两格程序锚（自锚/主人锚）之前，插完按最终位置重编 `a1..aN`。

/// 本批正文点名、但还不在候选里的可见活锚（P4a-1）。
///
/// `taken` 是现有候选（池 + 两格程序锚）：按 `anchor_id` 去重，再按 `(归一正名, scope)` 与池
/// 同一条规则去重（W15b2b，见 [`drop_shadowed_extras`]）——池里已删掉的同名输家不许借点名回来。
/// 匹配尺子与 [`order_by_evidence`] 同源（两侧都过 [`prepare`]，`name_in_bodies` 判字面子串）；
/// 名字按**轮**计数，同一锚在一轮里出现多次只算一轮。空 canonical 不参与匹配
/// （`contains("")` 恒真，会把整张表都补进来）。
pub(crate) fn mentioned_candidates(
    m: &MemoryHandle,
    owner_agent_id: &str,
    scope: &Scope,
    taken: &[FactCandidate],
    rows: &[TurnRow],
) -> Result<Vec<FactCandidate>, DistillError> {
    let bodies: Vec<String> = rows.iter().map(|row| prepare(&row.body)).collect();
    let taken_ids: HashSet<&str> = taken.iter().map(|candidate| candidate.anchor_id.as_str()).collect();
    let sql = format!(
        "SELECT a.id, a.kind, a.canonical, COALESCE(al.alias, '') FROM anchor AS a
           LEFT JOIN alias AS al ON al.anchor_id = a.id
          WHERE a.status='active' AND {}
          ORDER BY a.created_at_ms DESC, a.id, al.alias",
        owner::anchor_visible_pred("a.id")
    );
    let mut anchors: Vec<(String, String, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut aliases: HashMap<String, Vec<String>> = HashMap::new();
    {
        let conn = m.conn();
        let mut st = conn.prepare(&sql)?;
        let got = st.query_map(
            named_params! {
                ":viewer_agent": owner_agent_id,
                ":vis_person": owner::visibility_slug(owner::Visibility::Person),
                ":vis_project": owner::visibility_slug(owner::Visibility::Project),
                ":vis_org": owner::visibility_slug(owner::Visibility::Org),
                ":vis_public": owner::visibility_slug(owner::Visibility::Public),
                ":viewer_scope_kind": scope.kind.as_str(),
                ":viewer_scope_key": scope.key,
            },
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )?;
        // 一条锚在 alias 表里有 n 行：canonical 只留第一行，别名按 id 收拢。
        for row in got {
            let (anchor_id, kind, canonical, alias) = row?;
            if seen.insert(anchor_id.clone()) {
                anchors.push((anchor_id.clone(), kind, canonical));
            }
            if !alias.is_empty() {
                aliases.entry(anchor_id).or_default().push(alias);
            }
        }
    }

    let mut hits: Vec<(usize, FactCandidate)> = Vec::new();
    for (anchor_id, kind, canonical) in anchors {
        if taken_ids.contains(anchor_id.as_str()) || canonical.is_empty() {
            continue;
        }
        let mut names: Vec<String> = vec![prepare(&canonical)];
        if let Some(extra) = aliases.get(&anchor_id) {
            names.extend(extra.iter().map(|alias| prepare(alias)));
        }
        names.retain(|name| !name.is_empty());
        let turns = bodies
            .iter()
            .filter(|body| names.iter().any(|name| body.contains(name)))
            .count();
        if turns == 0 {
            continue;
        }
        hits.push((
            turns,
            FactCandidate {
                // 编号由 `insert_mentioned_candidates` 按最终位置重编（位置即契约）。
                reference: String::new(),
                kind,
                canonical,
                anchor_id,
                // 第三方锚没有身份章：`[self]`/`[principal]` 只由两格程序锚盖。
                mark: None,
            },
        ));
    }
    // 出现在几轮里降序；同轮数的按 canonical 字典序（再同则按 id，保证可复算）。
    hits.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.canonical.cmp(&b.1.canonical))
            .then_with(|| a.1.anchor_id.cmp(&b.1.anchor_id))
    });
    hits.truncate(MENTIONED_LIMIT);
    Ok(hits.into_iter().map(|(_, candidate)| candidate).collect())
}

// ── 补位去重（W15b2b） ───────────────────────────────────────────────────────
//
// 池只留同名组里的赢家，但上面两层补位都只按 `anchor_id` 去重：库里的输家不在候选里，
// 正文一点名（或它正是本批说话人）就又补回来，模型照样看到两个 kind 的同名格。这一层把
// 池那把 `(归一正名, scope)` 尺子接到补位上：候选里已有同组 ⇒ 这一格不补；两格都是补的
// ⇒ 按同一条赢家规则留一格。身份章（自锚/主人锚）除外——程序锚不是名字候选，删任何一格
// 都会改主语面。

/// 补位去重时按 id 读回的一格：只要赢家规则与分组用得上的四样。
struct BackfillAnchor {
    scope_kind: String,
    scope_key: String,
    created_at_ms: i64,
    status: String,
}

/// 补位层（正文点名 / 轮级说话人）的共享去重：把与 `taken` 同组的 extra 丢掉，
/// 再把 extra 之间同组的按赢家规则收敛成一格。
///
/// 三条口径：
/// 1. 分组键 `(归一正名, scope_kind, scope_key)` 与池 [`dedupe_pool`] 同一条，**跨 scope 不合并**；
///    正名的归一化用调用方手里那格 candidate 的 `canonical`（池与补位都取自锚行本身）。
/// 2. 已存在的 `taken` 优先：池已经按赢家规则留过一格，输家不该借补位回来。
/// 3. 任一侧带身份章时两格都留（身份章只由两格程序锚盖，它们不是名字候选）。
///
/// 只读 `anchor` 的 id 列（id 全部已知，`IN (...)` 动态占位），不走可见性谓词——可见性
/// 是两层补位各自入口的事。读不回来的 id（库外造的候选）不判、照原样保留。
fn drop_shadowed_extras(
    m: &MemoryHandle,
    taken: &[FactCandidate],
    extras: Vec<FactCandidate>,
) -> Result<Vec<FactCandidate>, DistillError> {
    if extras.is_empty() {
        return Ok(extras);
    }
    let mut ids: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for candidate in taken.iter().chain(extras.iter()) {
        if seen.insert(candidate.anchor_id.as_str()) {
            ids.push(candidate.anchor_id.clone());
        }
    }
    let holes = vec!["?"; ids.len()].join(",");
    let sql = format!(
        "SELECT id, scope_kind, scope_key, created_at_ms, status
           FROM anchor WHERE id IN ({holes})"
    );
    let mut facts: HashMap<String, BackfillAnchor> = HashMap::new();
    {
        let conn = m.conn();
        let mut st = conn.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(ids.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                BackfillAnchor {
                    scope_kind: r.get(1)?,
                    scope_key: r.get(2)?,
                    created_at_ms: r.get(3)?,
                    status: r.get(4)?,
                },
            ))
        })?;
        for row in rows {
            let (id, anchor) = row?;
            facts.insert(id, anchor);
        }
    }
    let key_of = |candidate: &FactCandidate| -> Option<(String, String, String)> {
        let anchor = facts.get(&candidate.anchor_id)?;
        Some((
            normalize_body(&candidate.canonical),
            anchor.scope_kind.clone(),
            anchor.scope_key.clone(),
        ))
    };

    // 候选里已有的组；带身份章的那几组单独记：任一侧有章 ⇒ 两格都留。
    let mut taken_keys: HashSet<(String, String, String)> = HashSet::new();
    let mut marked_taken_keys: HashSet<(String, String, String)> = HashSet::new();
    for candidate in taken {
        if let Some(key) = key_of(candidate) {
            if candidate.mark.is_some() {
                marked_taken_keys.insert(key.clone());
            }
            taken_keys.insert(key);
        }
    }

    // extra 之间同组只留一格：状态活的优先，然后入库时刻最小，再然后 id 字典序最小
    // （与池 [`dedupe_pool`]、写侧合流同一条规则）。
    let mut winners: HashMap<(String, String, String), usize> = HashMap::new();
    let mut marked_extra_keys: HashSet<(String, String, String)> = HashSet::new();
    for (index, candidate) in extras.iter().enumerate() {
        let Some(key) = key_of(candidate) else { continue };
        if candidate.mark.is_some() {
            marked_extra_keys.insert(key);
            continue;
        }
        let here = facts.get(&candidate.anchor_id).expect("key_of 已确认这一格在场");
        match winners.get(&key) {
            Some(&best) => {
                let below = facts.get(&extras[best].anchor_id).expect("同上");
                if backfill_winner_precedes(
                    (&candidate.anchor_id, here),
                    (&extras[best].anchor_id, below),
                ) {
                    winners.insert(key, index);
                }
            }
            None => {
                winners.insert(key, index);
            }
        }
    }

    Ok(extras
        .into_iter()
        .enumerate()
        .filter(|(index, candidate)| {
            let Some(key) = key_of(candidate) else { return true };
            if candidate.mark.is_some()
                || marked_extra_keys.contains(&key)
                || marked_taken_keys.contains(&key)
            {
                return true;
            }
            if taken_keys.contains(&key) {
                return false;
            }
            winners.get(&key) == Some(index)
        })
        .map(|(_, candidate)| candidate)
        .collect())
}

/// `a` 是否比同组的 `b` 更该当补位里那唯一一格（见 [`drop_shadowed_extras`]）。
fn backfill_winner_precedes(a: (&str, &BackfillAnchor), b: (&str, &BackfillAnchor)) -> bool {
    let rank = |status: &str| u8::from(status != "active");
    let left = (rank(&a.1.status), a.1.created_at_ms, a.0);
    let right = (rank(&b.1.status), b.1.created_at_ms, b.0);
    left < right
}

/// 把 [`mentioned_candidates`] 补进候选，插在两格程序锚**之前**（P4a-1 的位置纪律）。
///
/// `tail` 是两格程序锚占的格数（自锚 + 主人锚，0..=2）：补位从 `len - tail` 处插入，
/// 自锚/主人锚的原有位置不动。插完按最终位置重编 `a1..aN`——解析层只按位置认编号。
/// 没有可补的锚时一个字节都不改（调用方随后仍会跑 [`order_by_evidence`]）。
pub(crate) fn insert_mentioned_candidates(
    m: &MemoryHandle,
    candidates: &mut Vec<FactCandidate>,
    tail: usize,
    owner_agent_id: &str,
    scope: &Scope,
    rows: &[TurnRow],
) -> Result<(), DistillError> {
    let extras = mentioned_candidates(m, owner_agent_id, scope, candidates, rows)?;
    // W15b2b：池已按赢家规则留过一格，输家不许借"正文点名"回来；两格都是补的才收敛成一格。
    let extras = drop_shadowed_extras(m, candidates, extras)?;
    if extras.is_empty() {
        return Ok(());
    }
    let at = candidates.len().saturating_sub(tail);
    candidates.splice(at..at, extras);
    renumber(candidates);
    Ok(())
}

// ── 轮级说话人补位（D 子 agent 接线） ───────────────────────────────────────
//
// 子 agent 的报告在库里已有自己的锚与轮级 `mention`（`record_turn_speakers` 写的
// `discourse_role='speaker'`），但候选池只有"最近创建的 30 条可见锚"——它常在池外。
// 池里没有它，抽取侧就只能按 role 推说话人：TARGET 行写成主 agent 自锚的 `a*`，
// 子 agent 的实体归属当场丢失。这一层只做一件事：把**本批目标轮的说话人锚**补进候选。
//
// 纪律与 P4a-1 同源：
// 1. **来源只用库里已有的锚**（`mention` 指向的那一格），不新造锚、不调模型；
// 2. **位置**：插在两格程序锚（自锚/主人锚）之前，插完按最终位置重编 `a1..aN`；
// 3. **去重与可见性**：`anchor_id` 去重（同一子 agent 说两轮只占一格），再按
//    `(归一正名, scope)` 与池同一条规则去重（W15b2b）；可见性沿用候选池
//    同一条谓词（别人 owner 的私有锚一格都不补）。

/// 把 [`crate::batch::TurnSpeaker`] 补进候选，插在两格程序锚**之前**（位置规则同 P4a-1）。
///
/// `tail` 是两格程序锚占的格数（自锚 + 主人锚，0..=2）：补位从 `len - tail` 处插入，
/// 自锚/主人锚的原有位置不动（仍倒数第二、最后），插完按最终位置重编 `a1..aN`。
/// 没有说话人、说话人锚已在候选里、或它对这批不可见时一个字节都不改。
pub(crate) fn insert_speaker_candidates(
    m: &MemoryHandle,
    candidates: &mut Vec<FactCandidate>,
    tail: usize,
    owner_agent_id: &str,
    scope: &Scope,
    speakers: &[crate::batch::TurnSpeaker],
) -> Result<(), DistillError> {
    let mut extras: Vec<FactCandidate> = Vec::new();
    {
        let mut taken: HashSet<&str> =
            candidates.iter().map(|candidate| candidate.anchor_id.as_str()).collect();
        for speaker in speakers {
            if speaker.canonical.is_empty() || taken.contains(speaker.anchor_id.as_str()) {
                continue;
            }
            if !owner::anchor_visible(m, &speaker.anchor_id, owner_agent_id, scope)? {
                continue;
            }
            taken.insert(speaker.anchor_id.as_str());
            extras.push(FactCandidate {
                // 编号由最终位置重编（位置即契约）。
                reference: String::new(),
                kind: speaker.kind.clone(),
                canonical: speaker.canonical.clone(),
                anchor_id: speaker.anchor_id.clone(),
                // 说话人锚不是程序补的两格身份锚：不盖 `[self]`/`[principal]`。
                mark: None,
            });
        }
    }
    if extras.is_empty() {
        return Ok(());
    }
    // W15b2b：说话人补位与正文点名同一条尺子——候选里已有同组，或两格都是补的，都不能让
    // 池已经去重掉的同名格再进来。
    let extras = drop_shadowed_extras(m, candidates, extras)?;
    if extras.is_empty() {
        return Ok(());
    }
    let at = candidates.len().saturating_sub(tail);
    candidates.splice(at..at, extras);
    renumber(candidates);
    Ok(())
}

/// 位置即契约：按当前位置重编 `a1..aN`（渲染与 parser 都按位置取）。
pub(crate) fn renumber(candidates: &mut [FactCandidate]) {
    for (index, candidate) in candidates.iter_mut().enumerate() {
        candidate.reference = format!("a{}", index + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, canonical: &str) -> FactCandidate {
        FactCandidate {
            reference: String::new(),
            kind: "person".into(),
            canonical: canonical.into(),
            anchor_id: id.into(),
            mark: None,
        }
    }

    /// 命中的在前、各自保持原相对顺序；重排后按位置重编 `a1..aN`（位置即契约）。
    #[test]
    fn promotion_is_stable_and_renumbers_by_position() {
        let mut candidates = vec![
            candidate("id-a", "张三"),
            candidate("id-b", "李四"),
            candidate("id-c", "王五"),
            candidate("id-d", "赵六"),
        ];
        // 李四、赵六命中；张三、王五不命中。
        promote_evidence_hits(&mut candidates, &[false, true, false, true]);
        let want: Vec<(&str, &str)> = vec![
            ("a1", "李四"),
            ("a2", "赵六"),
            ("a3", "张三"),
            ("a4", "王五"),
        ];
        let got: Vec<(&str, &str)> = candidates
            .iter()
            .map(|candidate| (candidate.reference.as_str(), candidate.canonical.as_str()))
            .collect();
        assert_eq!(got, want, "命中的在前且稳；未命中的相对顺序也稳；编号按新位置");
    }

    /// 空名字不命中：否则 `contains("")` 恒真，整张候选表都会被判成命中。
    #[test]
    fn an_empty_name_never_hits() {
        let bodies = vec!["李四拆开清理后恢复".to_string()];
        assert!(!name_in_bodies("", &bodies));
        assert!(name_in_bodies("李四", &bodies));
        assert!(!name_in_bodies("张三", &bodies));
    }

    // ── W15b2：候选池按 (归一正名, scope) 去重 ────────────────────────────────

    /// 池的读者：`resource_owner` 的 owner 是它自己，所以任何 scope 的锚都对它可见
    /// （谓词的第三支走 owner 相等那一半），跨 scope 用例才造得出来。
    const VIEWER: &str = "alice";

    /// 锚行 + 归属行。`id` 由调用方给：同 kind 同正名同 scope 的两格在内容寻址下算不出
    /// 两个 id，而这一组用例要造出"库里真有两行"的形状，所以直接写行。
    fn seed_anchor(
        m: &MemoryHandle,
        id: &str,
        kind: &str,
        canonical: &str,
        scope: &Scope,
        created_at_ms: i64,
    ) {
        m.conn()
            .execute(
                "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                                    status, created_at_ms, updated_at_ms, norm_version)
                 VALUES(?1, ?2, ?3, 'real', ?4, ?5, 'active', ?6, ?6, 1)",
                rusqlite::params![id, kind, canonical, scope.kind.as_str(), scope.key, created_at_ms],
            )
            .unwrap();
        m.conn()
            .execute(
                "INSERT INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                            created_by_agent, visibility, retention, scope_kind,
                                            scope_key, source_agent, device_id, created_at_ms)
                 VALUES('anchor', ?1, NULL, ?2, ?2, 'private_agent', 'default', ?3, ?4, ?2,
                        'dev-pool', ?5)",
                rusqlite::params![
                    id,
                    VIEWER,
                    scope.kind.as_str(),
                    scope.key,
                    created_at_ms
                ],
            )
            .unwrap();
    }

    fn project(key: &str) -> Scope {
        Scope { kind: rc_memory::ScopeKind::Project, key: key.into() }
    }

    fn ids(candidates: &[FactCandidate]) -> Vec<&str> {
        candidates.iter().map(|candidate| candidate.anchor_id.as_str()).collect()
    }

    fn references(candidates: &[FactCandidate]) -> Vec<&str> {
        candidates.iter().map(|candidate| candidate.reference.as_str()).collect()
    }

    /// 同 scope、同正名、两个 kind（work 较老、agent 较新）⇒ 只出一格，且是**较老**那格。
    ///
    /// 池的 SQL 是 `created_at DESC`：照原样渲染会先看到新的 agent 锚，顺手取"每组第一格"
    /// 就会留错。这一条同时钉住"留较老"与"去重后按位置重编 `a1`"。
    #[test]
    fn a_cross_kind_duplicate_renders_as_one_candidate_and_the_older_anchor_wins() {
        let dir = tempfile::tempdir().unwrap();
        let root = rc_store::Root::open(dir.path()).unwrap();
        let m = root.memory();
        let scope = project("proj-pool");
        seed_anchor(m, "anchor-old", "work", "老周", &scope, 1_000);
        seed_anchor(m, "anchor-new", "agent", "老周", &scope, 2_000);

        let got = load_candidates(m, VIEWER, &scope).unwrap();
        assert_eq!(ids(&got), vec!["anchor-old"], "同正名两个 kind 只许一格，且留较老的：{got:?}");
        assert_eq!(got[0].kind, "work", "留的是较老那格的 kind");
        assert_eq!(references(&got), vec!["a1"], "去重后按位置重编号");
    }

    /// 同正名分属两个 scope ⇒ 各留一格（scope 是身份的第三维，不许跨 scope 去重）。
    #[test]
    fn the_same_name_in_two_scopes_keeps_one_candidate_per_scope() {
        let dir = tempfile::tempdir().unwrap();
        let root = rc_store::Root::open(dir.path()).unwrap();
        let m = root.memory();
        let left = project("proj-left");
        let right = project("proj-right");
        seed_anchor(m, "anchor-left", "work", "老周", &left, 1_000);
        seed_anchor(m, "anchor-right", "agent", "老周", &right, 2_000);

        let got = load_candidates(m, VIEWER, &left).unwrap();
        assert_eq!(
            ids(&got),
            vec!["anchor-right", "anchor-left"],
            "两个 scope 各一格，顺序仍是 created_at DESC：{got:?}"
        );
        assert_eq!(references(&got), vec!["a1", "a2"]);
    }

    /// 同 kind 同正名同 scope 的两行（直插制造）⇒ 也只留一格；同刻时按 id 字典序小的那格。
    #[test]
    fn a_same_kind_duplicate_in_one_scope_keeps_the_smallest_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = rc_store::Root::open(dir.path()).unwrap();
        let m = root.memory();
        let scope = project("proj-pool");
        seed_anchor(m, "anchor-b", "work", "老周", &scope, 1_000);
        seed_anchor(m, "anchor-a", "work", "老周", &scope, 1_000);

        let got = load_candidates(m, VIEWER, &scope).unwrap();
        assert_eq!(ids(&got), vec!["anchor-a"], "同 kind 重复也只留一格：{got:?}");
    }

    // ── W15b2b：补位不许把池去重掉的同名格补回来 ─────────────────────────────

    fn entry(id: &str, kind: &str, canonical: &str) -> FactCandidate {
        FactCandidate {
            reference: String::new(),
            kind: kind.into(),
            canonical: canonical.into(),
            anchor_id: id.into(),
            mark: None,
        }
    }

    fn turn(body: &str) -> TurnRow {
        TurnRow {
            turn_uid: "t".repeat(64),
            role: "agent".into(),
            body: body.into(),
            created_at_ms: 1,
            ingested_at_ms: 1,
            speaker: None,
            speaker_anchor_id: None,
        }
    }

    /// 用一整池更近的诱饵把目标锚挤出候选池（池只取最近创建的那一批），而它们在库里仍可见：
    /// 这样"两格都是补的"才造得出来。
    fn crowd_out_pool(m: &MemoryHandle, scope: &Scope) {
        for i in 0..CANDIDATE_LIMIT {
            let name = format!("路人{i:02}");
            seed_anchor(m, &format!("anchor-decoy-{i:02}"), "person", &name, scope, 10_000 + i);
        }
    }

    /// 池里的赢家挡住补位：库里另有一格较新、同正名的活锚，池已按赢家规则删掉它——正文点名
    /// 不许把它补回来，否则模型照样看到两个 kind 的同名格。
    #[test]
    fn the_pool_winner_keeps_the_loser_out_of_the_backfill() {
        let dir = tempfile::tempdir().unwrap();
        let root = rc_store::Root::open(dir.path()).unwrap();
        let m = root.memory();
        let scope = project("proj-pool");
        seed_anchor(m, "anchor-old", "work", "d3", &scope, 1_000);
        seed_anchor(m, "anchor-new", "agent", "d3", &scope, 2_000);

        let mut candidates = load_candidates(m, VIEWER, &scope).unwrap();
        assert_eq!(ids(&candidates), vec!["anchor-old"], "池只留赢家：{candidates:?}");
        insert_mentioned_candidates(m, &mut candidates, 0, VIEWER, &scope, &[turn("d3 接手了")])
            .unwrap();
        assert_eq!(ids(&candidates), vec!["anchor-old"], "输家不许借正文点名回来：{candidates:?}");
        assert_eq!(references(&candidates), vec!["a1"]);
    }

    /// 两格都是补的（池里一格都没有）：按与池同一条赢家规则留**较老**那格。
    #[test]
    fn two_backfilled_extras_in_one_group_keep_the_older_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = rc_store::Root::open(dir.path()).unwrap();
        let m = root.memory();
        let scope = project("proj-pool");
        seed_anchor(m, "anchor-old", "work", "老周", &scope, 1_000);
        seed_anchor(m, "anchor-new", "agent", "老周", &scope, 2_000);
        crowd_out_pool(m, &scope);

        let mut candidates = load_candidates(m, VIEWER, &scope).unwrap();
        assert_eq!(candidates.len(), CANDIDATE_LIMIT as usize, "两格都被挤出池：{candidates:?}");
        insert_mentioned_candidates(m, &mut candidates, 0, VIEWER, &scope, &[turn("老周接手了")])
            .unwrap();
        assert_eq!(candidates.len(), CANDIDATE_LIMIT as usize + 1, "同组只补一格：{candidates:?}");
        assert_eq!(
            candidates.last().map(|c| c.anchor_id.as_str()),
            Some("anchor-old"),
            "两格都是补的 ⇒ 留较老那格：{candidates:?}"
        );
    }

    /// 同正名两个活锚分属两个 scope ⇒ 两格都在（scope 是身份的第三维，跨 scope 不合并）。
    #[test]
    fn two_backfilled_extras_in_different_scopes_are_both_kept() {
        let dir = tempfile::tempdir().unwrap();
        let root = rc_store::Root::open(dir.path()).unwrap();
        let m = root.memory();
        seed_anchor(m, "anchor-left", "work", "老周", &project("proj-left"), 1_000);
        seed_anchor(m, "anchor-right", "agent", "老周", &project("proj-right"), 2_000);

        let extras = vec![
            entry("anchor-right", "agent", "老周"),
            entry("anchor-left", "work", "老周"),
        ];
        let kept = drop_shadowed_extras(m, &[], extras).unwrap();
        assert_eq!(
            ids(&kept),
            vec!["anchor-right", "anchor-left"],
            "跨 scope 各留一格，顺序保持补位原序：{kept:?}"
        );
    }

    /// 身份章不动：候选里那一格带 mark 时，同正名的 extra 也保留——程序锚不是名字候选，
    /// 删任何一格都会改主语面。
    #[test]
    fn an_identity_mark_keeps_both_sides() {
        let dir = tempfile::tempdir().unwrap();
        let root = rc_store::Root::open(dir.path()).unwrap();
        let m = root.memory();
        let scope = project("proj-pool");
        seed_anchor(m, "anchor-self", "work", "老周", &scope, 1_000);
        seed_anchor(m, "anchor-other", "agent", "老周", &scope, 2_000);

        let mut marked = entry("anchor-self", "work", "老周");
        marked.mark = Some(crate::pass_prompt::MARK_SELF);
        let kept =
            drop_shadowed_extras(m, &[marked], vec![entry("anchor-other", "agent", "老周")]).unwrap();
        assert_eq!(ids(&kept), vec!["anchor-other"], "带身份章的那组两格都留：{kept:?}");
    }

    /// 说话人补位走同一条尺子：说话人锚是输家、候选里已有赢家 ⇒ 不再插入。
    #[test]
    fn a_speaker_backfill_does_not_undo_the_pool_dedupe() {
        let dir = tempfile::tempdir().unwrap();
        let root = rc_store::Root::open(dir.path()).unwrap();
        let m = root.memory();
        let scope = project("proj-pool");
        seed_anchor(m, "anchor-old", "work", "d3", &scope, 1_000);
        seed_anchor(m, "anchor-new", "agent", "d3", &scope, 2_000);

        let mut candidates = load_candidates(m, VIEWER, &scope).unwrap();
        assert_eq!(ids(&candidates), vec!["anchor-old"], "池只留赢家：{candidates:?}");
        let speakers = vec![crate::batch::TurnSpeaker {
            turn_uid: "t".repeat(64),
            anchor_id: "anchor-new".into(),
            kind: "agent".into(),
            canonical: "d3".into(),
        }];
        insert_speaker_candidates(m, &mut candidates, 0, VIEWER, &scope, &speakers).unwrap();
        assert_eq!(ids(&candidates), vec!["anchor-old"], "输家不许借说话人补位回来：{candidates:?}");
    }
}
