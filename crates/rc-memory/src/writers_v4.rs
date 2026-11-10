//! v4 A1 写入口：锚 / 别名 / 轮级提及 / 问题。
//!
//! 与 `writers_x` 分开的理由：v3 的 `put_entity` 只有"标题 + 别名"两个内容列，而 A1
//! 还要 `reality_status`、轮级 `mention`、双跑 `stable` 与 `attention`。把它们塞回 v3 的
//! `WriteItem` 会把 1176 条既有用例的语义一起搅动，所以 v4 自己成文件。
//!
//! 与 `writers_x` 相同的纪律：模型只递语义草稿；id / scope / device / source_agent /
//! created_at / norm_version 全部由这里盖章（守卫 11），错误载荷只带静态字段名。

use crate::identity::{compute_mem_id, compute_mention_id};
use crate::issues::{UNRESOLVED_SURFACE_CODE, UNRESOLVED_SURFACE_SHAPE_CODE};
use crate::keys::{AnchorRole, AttentionKind, MemKey, MemTable, Scope};
use crate::owner::{self, OwnerCtx};
use crate::reader;
use crate::writers_x::{anchor_keys, check_alias, BatchCtx};
use crate::{MemError, Provenance};
use rc_contract::{
    is_valid_anchor_kind, normalize_body, DISCOURSE_ROLES, MENTION_RESOLUTIONS, REALITY_STATUSES,
};
use rc_store::MemoryHandle;
use std::collections::{BTreeMap, BTreeSet};

/// A1 在 `extraction_issue.pass` 里的落点。那一列的 CHECK 只认 v3 的八段
///（`role|entity|time|event|state|utterance|relation|assemble`），A1 的语义段就是 entity；
/// 等 v4 管线正式接管时再随迁移扩值域，不在这一轮偷偷写一个 CHECK 外的词。
const A1_PASS_DB: &str = "entity";

/// 一条锚草稿。`evidence` 已经是 (轮, 角色) 对，角色只能来自批次行（模型递不进）。
#[derive(Debug, Clone)]
pub struct A1Anchor {
    pub reference: String,
    pub kind: String,
    pub canonical: String,
    pub aliases: Vec<String>,
    pub reality_status: String,
    pub evidence: Vec<Provenance>,
    /// 双跑都出现 ⇒ `active`；只出现一次 ⇒ `provisional`（入库但不进 A2）。
    pub stable: bool,
}

#[derive(Debug, Clone)]
pub struct A1Mention {
    pub anchor_ref: String,
    pub surface: String,
    pub turn_uid: String,
    pub speaker_ref: Option<String>,
    pub discourse_role: String,
    pub resolution: String,
    pub confidence: f64,
}

#[derive(Debug, Clone)]
pub struct A1Issue {
    pub kind: String,
    pub refs: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct A1Unresolved {
    pub surface: String,
    pub turn_uid: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct A1Batch {
    pub anchors: Vec<A1Anchor>,
    pub mentions: Vec<A1Mention>,
    pub issues: Vec<A1Issue>,
    pub unresolved: Vec<A1Unresolved>,
}

/// 一批 A1 落库后的读数 + A2 需要的 `ref → 句柄` 映射。
/// 注意读数的口径：`anchors` 是**本批处理的去重锚数**（重放同值），
/// `aliases`/`mentions` 是**本次真正新插入的行数**（重放为 0）。
#[derive(Debug, Clone, Default)]
pub struct A1Outcome {
    pub anchors: usize,
    pub aliases: usize,
    pub aliases_withheld: usize,
    pub mentions: usize,
    pub attention: usize,
    pub issues: usize,
    pub unresolved: usize,
    pub stable: usize,
    pub provisional: usize,
    /// P3-5：名字到锚的程序解析改了模型给的 `anchor_ref` 的提及数（唯一命中且不同）。
    pub mentions_rebound: usize,
    /// P3-5：命中多个锚 ⇒ 强制 `unresolved` 的提及数。
    pub mentions_ambiguous: usize,
    /// P3-5：一个锚都没命中 ⇒ 保持模型 resolution、只计数的提及数。
    pub mentions_unmatched: usize,
    /// P0-2：`kind='person'` 而身份未知 ⇒ `real` 降成 `unknown` 的锚数。
    pub reality_downgrades: usize,
    /// W14d：证据轮全是 agent 轮的 `kind='person'` 锚数（现实性据此落成未知；锚照写）。
    pub person_agent_reported: usize,
    /// W14e：形状类悬空表面（哈希/路径/UUID/…）被判掉、只进诊断桶的条数。
    /// 它们**不进** `unresolved`：那一桶留给"是个名字、但这一批里落不到实体"。
    pub unresolved_shape_dropped: usize,
    /// W15b1：正名撞上同 scope 另一个 kind 的活锚、因而改从旧锚那条身份的草案数。
    /// 每一条都在 `extraction_issue` 留一行（同一 run 重放不追加）。
    pub cross_kind_coerced: usize,
    pub by_ref: BTreeMap<String, MemKey>,
}

/// 写一批 A1。校验在任何事务开始之前完成：一条坏输入 ⇒ 整批不落库。
///
/// 归属走合成默认（owner=署名 agent、private、default）：没有会话可问的调用方（夹具、离线闭环）
/// 用它。中心 naming pass 走 [`write_a1_with_owner`]，把会话的归属声明递进来。
pub fn write_a1(
    m: &MemoryHandle,
    batch: &A1Batch,
    ctx: &BatchCtx,
    scope: &Scope,
    run_id: &str,
    now_ms: i64,
) -> Result<A1Outcome, MemError> {
    let owner = OwnerCtx::synthetic(&ctx.source_agent);
    write_a1_with_owner(m, batch, ctx, &owner, scope, run_id, now_ms)
}

/// 带归属上下文的 A1 写入口（S2a-2）。主体与 [`write_a1`] 逐行相同，差别只在每个锚落库后
/// 紧跟一行 `resource_owner`——它与锚/别名/提及/出处**同一个事务**，任何一步失败整批回滚。
///
/// 只给 `anchor` 写归属：`alias`/`mention`/`provenance` 不是可指认句柄（没有 `MemKey`），
/// 它们的受众由所属锚那一行承载。`owner::insert` 是 `INSERT OR IGNORE`（先写者赢）：
/// 同 id 重放不覆盖已有归属，也不刷新它的 `created_at_ms`。
#[allow(clippy::too_many_arguments)]
pub fn write_a1_with_owner(
    m: &MemoryHandle,
    batch: &A1Batch,
    ctx: &BatchCtx,
    owner: &OwnerCtx,
    scope: &Scope,
    run_id: &str,
    now_ms: i64,
) -> Result<A1Outcome, MemError> {
    if m.is_read_only() {
        return Err(MemError::ReadOnly);
    }

    // W15b1（审计 P1-1 的第一刀，只止血不回溯）：锚的身份是 (kind, 归一正名, scope)，所以同一个
    // 名字先后被判成两种 kind 时会各落一格锚，主体解析跟着分叉。这里在**校验之前**把草案改从
    // 库里已有的那一格：`validate` 之后 id 已经由 (kind, canonical) 算定，那时再改会让 id 与列
    // 对不上。改的是本地副本，调用方的批次不动。
    let mut batch = batch.clone();
    let folded = fold_cross_kind_duplicates(m.conn(), &mut batch, scope)?;

    let mut by_ref = validate(&batch, scope)?;

    // 说话人（C1）：mention 的 `speaker_anchor_id` 由**程序**盖章，不听模型递的 `speaker_ref`。
    // 每轮的转录角色从批内锚证据里读（`Provenance.anchor_role` 只由 pass 从 `turns.role` 填，
    // 模型递交不进这一格），两格说话人在事务前各查一次（各 ≤1 条只读查询）。
    let turn_roles = turn_roles_of(&batch);
    let (agent_speaker, principal_speaker) = if batch.mentions.is_empty() {
        (None, None)
    } else {
        let agent = session_agent_anchor_id(m, &ctx.source_agent, scope)?;
        let principal = match owner.owner_principal_id.as_deref() {
            Some(principal_id) => session_principal_anchor_id(m, principal_id, scope)?,
            None => None,
        };
        (agent, principal)
    };

    let tx = m.conn().unchecked_transaction()?;
    let mut out = A1Outcome { cross_kind_coerced: folded.len(), ..A1Outcome::default() };

    // 同一 (kind, 归一 canonical) 的两条草稿是同一个锚：合并别名与证据，任一跑稳定即位 stable。
    let mut order: Vec<MemKey> = Vec::new();
    let mut merged: BTreeMap<String, A1Anchor> = BTreeMap::new();
    for draft in &batch.anchors {
        let id = compute_mem_id(MemTable::Anchor, &draft.kind, &draft.canonical, "", scope);
        let key = MemKey { table: MemTable::Anchor, id: id.clone() };
        match merged.get_mut(&id) {
            Some(seen) => {
                for alias in &draft.aliases {
                    if !seen.aliases.contains(alias) {
                        seen.aliases.push(alias.clone());
                    }
                }
                for ev in &draft.evidence {
                    if !seen.evidence.iter().any(|p| p.turn_uid == ev.turn_uid) {
                        seen.evidence.push(ev.clone());
                    }
                }
                seen.stable |= draft.stable;
                if seen.reality_status == "unknown" && draft.reality_status != "unknown" {
                    seen.reality_status = draft.reality_status.clone();
                }
            }
            None => {
                order.push(key.clone());
                merged.insert(id.clone(), draft.clone());
            }
        }
        by_ref.insert(draft.reference.clone(), key);
    }

    // 批内撞名要先算清楚：一个锚的别名撞上另一个锚的 canonical 时，判定不能取决于谁先落库。
    // 撞名的串对两个认领方都挂 `alias_ambiguity`；别名本身不落库（不硬挂）。
    let mut claims: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (id, draft) in &merged {
        claims
            .entry(normalize_body(&draft.canonical))
            .or_default()
            .insert(id.clone());
        for alias in &draft.aliases {
            claims
                .entry(normalize_body(alias))
                .or_default()
                .insert(id.clone());
        }
    }
    let mut contested: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (claim, ids) in &claims {
        if ids.len() < 2 {
            continue;
        }
        for id in ids {
            contested.entry(id.clone()).or_default().insert(claim.clone());
        }
    }
    for id in contested.keys() {
        if raise_attention(&tx, AttentionKind::AliasAmbiguity, id, now_ms)? {
            out.attention += 1;
        }
    }

    // W14d（用户裁定）：agent 轮转述的人名"有来源、有去处"，应当通过——这条锚照写，
    // 提及、出处、说话人一格不少；不知道的只是"这个人在现实中是否真实存在"，所以现实性
    // 落成未知，而不是拒收。拒收会把整条转述路径断在闸门，降级只动一格现实性。
    //
    // 判据只看证据轮的角色：`Provenance.anchor_role` 由 pass 从 `turns.role` 抄，模型递不进。
    // 必须放在合并之后：合并是"未知让位给非未知"，先降会被另一跑的自报值掀回来。
    let mut agent_reported: Vec<String> = Vec::new();
    let mut agent_reported_from: BTreeMap<String, String> = BTreeMap::new();
    for (id, draft) in merged.iter_mut() {
        if draft.kind != PRINCIPAL_KIND {
            continue;
        }
        let human = draft.evidence.iter().any(|p| p.anchor_role == AnchorRole::Human);
        if human {
            continue;
        }
        agent_reported.push(id.clone());
        // `from` 记降级**前**的模型自报值（不写库、只进审计行），供读数分辨来源。
        agent_reported_from.insert(id.clone(), draft.reality_status.clone());
        if draft.reality_status != "unknown" {
            draft.reality_status = "unknown".to_string();
        }
    }
    out.person_agent_reported = agent_reported.len();
    // 审计行按批内短引用指认（与身份未知那一族同一套），不带正文；同一 run 重放不追加。
    let agent_reported_refs: Vec<(&str, &str)> = by_ref
        .iter()
        .filter_map(|(reference, key)| {
            agent_reported_from
                .get(&key.id)
                .map(|from| (reference.as_str(), from.as_str()))
        })
        .collect();

    // P0-2（写侧的第三格）：身份未知 ⇒ person 不许自报 `real`。
    //
    // 判据只有一列：本批的归属有没有一个**已知**的 principal。没有（空/`unknown`/干脆没登记）
    // 就意味着"我们不知道这条人名的锚对应哪个自然人"，此时 `real` 是模型替程序下的结论，
    // 降成 `unknown`（`insert_anchor` 的 unknown→已知 单向覆盖仍照旧：以后身份清楚了能升回来）。
    // 降级不许静默：每个被降的锚一行 `extraction_issue`（同一 run 重放不追加），读数是
    // [`A1Outcome::reality_downgrades`] 与那一行 `code='person_reality_downgraded'`。
    // 不走 `attention`：它的 `kind` 是五值 CHECK 闭集，没有"身份未知"这一族（加值要动 DDL）。
    // 与上面那一族互斥：一个锚只算一次，转述证据那族优先（它已经降过，不重复计数）。
    let principal_known = owner
        .owner_principal_id
        .as_deref()
        .is_some_and(principal_is_known);
    let mut downgraded: Vec<String> = Vec::new();
    if !principal_known {
        for (id, draft) in merged.iter_mut() {
            if agent_reported_from.contains_key(id) {
                continue;
            }
            if draft.kind == PRINCIPAL_KIND && draft.reality_status == "real" {
                draft.reality_status = "unknown".to_string();
                downgraded.push(id.clone());
            }
        }
    }
    out.reality_downgrades = downgraded.len();
    // 审计行按批内短引用指认（与 `review_decision.detail` 的 `ref=` 同一套），不带正文。
    let downgraded_refs: Vec<&str> = by_ref
        .iter()
        .filter(|(_, key)| downgraded.contains(&key.id))
        .map(|(reference, _)| reference.as_str())
        .collect();

    // P3-5：名字到锚的程序解析。模型的 `anchor_ref` 只是候选，唯一命中时被程序结果纠正。
    let resolved = resolve_mentions(&batch, &merged, &by_ref);
    out.mentions_rebound = resolved.rebound;
    out.mentions_ambiguous = resolved.ambiguous;
    out.mentions_unmatched = resolved.unmatched;

    for key in &order {
        let draft = &merged[&key.id];
        insert_anchor(&tx, key, draft, scope, now_ms)?;
        // 归属：锚行之后、别名/出处之前，与整批同一个事务。visibility 过一遍行 scope 的天花板
        // （`owner::effective`），与 `writers_x::write_item_in` 是同一个实现点。
        owner::insert(
            &tx,
            &owner::OwnerRow {
                mem_table: MemTable::Anchor.prefix().to_string(),
                mem_id: key.id.clone(),
                owner_principal_id: owner.owner_principal_id.clone(),
                owner_agent_id: owner.owner_agent_id.clone(),
                created_by_agent: ctx.source_agent.clone(),
                visibility: owner::effective(owner.visibility, scope.kind),
                retention: owner::normalize_retention(&owner.retention),
                scope: scope.clone(),
                source_agent: Some(ctx.source_agent.clone()),
                device_id: Some(ctx.device_id.clone()),
                created_at_ms: now_ms,
            },
        )?;
        // B（用户 11-01 拍板："同名第三方不并入主人，但也不许无声遮住"）：新锚的正名如果
        // 已经是**同 kind、同 scope** 里某格活锚的别名 ⇒ 两格都挂 `alias_ambiguity`。
        // 不合并（合并会把别人的状态记到主人头上，正是 D19/D7 一路在拆的形状），但
        // "这句到底说谁"从此是一件可见、待裁的事，不是靠读者猜。
        for other in alias_shadowing_anchors(&tx, &draft.kind, &draft.canonical, scope, &key.id)? {
            if raise_attention(&tx, AttentionKind::AliasAmbiguity, &key.id, now_ms)? {
                out.attention += 1;
            }
            if raise_attention(&tx, AttentionKind::AliasAmbiguity, &other, now_ms)? {
                out.attention += 1;
            }
        }
        out.anchors += 1;
        if draft.stable {
            out.stable += 1;
        } else {
            out.provisional += 1;
        }

        for pv in &draft.evidence {
            tx.execute(
                "INSERT OR IGNORE INTO provenance(mem_table, mem_id, turn_uid, anchor_role)
                 VALUES('anchor',?1,?2,?3)",
                rusqlite::params![key.id, pv.turn_uid, pv.anchor_role.as_str()],
            )?;
        }

        let source_turn = draft.evidence.first().map(|p| p.turn_uid.as_str());
        for alias in &draft.aliases {
            let normalized = normalize_body(alias);
            if normalized == normalize_body(&draft.canonical) {
                continue; // canonical 本身已经能被 entity_ids_for_name 命中
            }
            if contested.get(&key.id).is_some_and(|set| set.contains(&normalized)) {
                out.aliases_withheld += 1;
                continue; // 告警已在批内预扫时挂过
            }
            let claimants = alias_claimants(&tx, alias, &key.id)?;
            if !claimants.is_empty() {
                out.aliases_withheld += 1;
                if raise_attention(&tx, AttentionKind::AliasAmbiguity, &key.id, now_ms)? {
                    out.attention += 1;
                }
                for other in claimants {
                    if raise_attention(&tx, AttentionKind::AliasAmbiguity, &other, now_ms)? {
                        out.attention += 1;
                    }
                }
                continue;
            }
            let n = tx.execute(
                "INSERT OR IGNORE INTO alias(anchor_id, alias, source_turn_uid) VALUES(?1,?2,?3)",
                rusqlite::params![key.id, alias, source_turn],
            )?;
            if n == 1 {
                out.aliases += 1;
            } else if let Some(uid) = source_turn {
                // 重放：只在原来没记来源时补一次，已有来源不覆盖。
                tx.execute(
                    "UPDATE alias SET source_turn_uid=?3
                     WHERE anchor_id=?1 AND alias=?2 AND source_turn_uid IS NULL",
                    rusqlite::params![key.id, alias, uid],
                )?;
            }
        }
    }

    for (index, draft) in batch.mentions.iter().enumerate() {
        // P3-5：这一条提及挂哪个锚由程序解析决定（[`resolve_mentions`]），不再直接听
        // `anchor_ref`；`resolution` 同理（多命中时已被强制成 `unresolved`）。
        let plan = &resolved.plans[index];
        // 说话人（C1）：程序列，按"这一轮是谁在说"取，不取模型递的 `speaker_ref`。
        // - human 轮 + 身份已知 ⇒ 主人锚；主人锚不在库里就是 NULL（**不回落到模型值**：
        //   有身份时这一格必须只剩程序值，闸第 3 支只信它）。
        // - human 轮 + 身份未知 ⇒ 保持既有行为（模型值）。这一格不影响闸：身份未知的
        //   human 轮在 6c 第一步就 fail-open 了（[`crate::writers_v4`] 之外的 `KnownPrincipal`）。
        // - agent/system 轮 ⇒ 自锚；自锚不在库里（夹具/旧入口）才退回模型值。
        // - 这一轮在批内没有任何锚证据（角色读不到）⇒ 保持既有行为，不猜。
        let model_speaker = draft.speaker_ref.as_ref().map(|s| by_ref[s].id.clone());
        let speaker = match turn_roles.get(draft.turn_uid.as_str()) {
            Some(AnchorRole::Human) => match owner.owner_principal_id.as_deref() {
                Some(principal_id) if principal_is_known(principal_id) => {
                    principal_speaker.clone()
                }
                _ => model_speaker,
            },
            Some(AnchorRole::Agent) | Some(AnchorRole::System) => {
                agent_speaker.clone().or(model_speaker)
            }
            None => model_speaker,
        };
        let id = compute_mention_id(&draft.turn_uid, &plan.anchor_id, &draft.surface, None, None);
        let n = tx.execute(
            "INSERT OR IGNORE INTO mention(id, anchor_id, turn_uid, surface, span_start, span_end,
                 speaker_anchor_id, discourse_role, resolution, confidence,
                 source_agent, device_id, created_at_ms, norm_version)
             VALUES(?1,?2,?3,?4,NULL,NULL,?5,?6,?7,?8,?9,?10,?11,?12)",
            rusqlite::params![
                id,
                plan.anchor_id,
                draft.turn_uid,
                draft.surface,
                speaker,
                draft.discourse_role,
                plan.resolution,
                draft.confidence,
                ctx.source_agent,
                ctx.device_id,
                now_ms,
                rc_store::capability::NORM_VERSION as i64
            ],
        )?;
        out.mentions += n;
    }

    for issue in &batch.issues {
        let refs: Vec<String> = issue
            .refs
            .iter()
            .filter_map(|r| by_ref.get(r).map(|k| k.id.clone()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let kind = match issue.kind.as_str() {
            "ambiguous_anchor" => Some(AttentionKind::AliasAmbiguity),
            "conflict" => Some(AttentionKind::Contradiction),
            _ => None,
        };
        if let Some(kind) = kind {
            for id in &refs {
                if raise_attention(&tx, kind, id, now_ms)? {
                    out.attention += 1;
                }
            }
        }
        let detail = format!("refs={}; reason={}", issue.refs.join(","), issue.reason);
        log_issue(&tx, run_id, &issue.kind, &detail, now_ms)?;
        out.issues += 1;
    }

    // W14e（审计 P0-3）：悬空表面在落库之前分桶。
    //
    // 形状类（哈希、路径、URL、UUID、反引号令牌、纯数字）**不进**真名桶：它们是模板的
    // 排除条款明确说"不是实体"的东西，进真名桶只会变成永久悬空的噪声。它们进诊断桶，
    // `shape=` 说明是哪一类——这一桶不是待人工解决队列，它是"模型又照旧条款塞了一遍"
    // 的读数。两桶都走 `log_issue`，幂等语义不变；存量旧行不回溯。
    for u in &batch.unresolved {
        if let Some(shape) = surface_shape(&u.surface) {
            let detail = format!(
                "turn={}; surface={}; shape={}",
                u.turn_uid,
                u.surface,
                shape.as_str()
            );
            log_issue(&tx, run_id, UNRESOLVED_SURFACE_SHAPE_CODE, &detail, now_ms)?;
            out.unresolved_shape_dropped += 1;
            continue;
        }
        // 真名/真歧义：语义与 W14e 之前逐字相同，只在尾巴补程序侧候选数。
        let cand = candidate_anchor_count(&u.surface, &merged);
        let detail = format!(
            "turn={}; surface={}; reason={}; cand={cand}",
            u.turn_uid, u.surface, u.reason
        );
        log_issue(&tx, run_id, UNRESOLVED_SURFACE_CODE, &detail, now_ms)?;
        out.unresolved += 1;
    }

    // 降级的确定性读数（不许静默）：一个被降的锚一行，同一 run 重放不追加。
    for reference in &downgraded_refs {
        log_issue(
            &tx,
            run_id,
            "person_reality_downgraded",
            &format!("ref={reference}; principal=unknown"),
            now_ms,
        )?;
    }
    // 转述降级的读数：一行一个锚，`from` 是降级前的模型自报值（重放不追加）。
    for (reference, from) in &agent_reported_refs {
        log_issue(
            &tx,
            run_id,
            "person_agent_reported",
            &format!("ref={reference}; evidence=agent_only; from={from}"),
            now_ms,
        )?;
    }
    // 程序改写过模型的提及映射时才留一行（干净批次不产噪声）：`rebound` 是纠正，
    // `ambiguous` 是强制 `unresolved`，`unmatched` 只计数（规则见 [`resolve_mentions`]）。
    if resolved.rebound + resolved.ambiguous > 0 {
        log_issue(
            &tx,
            run_id,
            "mention_resolution",
            &format!(
                "rebound={}; ambiguous={}; unmatched={}",
                resolved.rebound, resolved.ambiguous, resolved.unmatched
            ),
            now_ms,
        )?;
    }

    // 跨 kind 合流的读数：一行一条被改写的草案，指认用批内短引用（与上面几族同一套）。
    // `candidates=<n>` 是同一正名的候选旧锚数；`from`/`to` 是两个 kind 的原文。
    for fold in &folded {
        log_issue(
            &tx,
            run_id,
            CROSS_KIND_CANONICAL,
            &format!(
                "ref={}; from={}; to={}; candidates={}",
                fold.reference, fold.from_kind, fold.to_kind, fold.candidates
            ),
            now_ms,
        )?;
    }

    tx.commit()?;
    out.by_ref = by_ref;
    Ok(out)
}

/// 跨 kind 合流的审计行 `code`。正文里带批内短引用与两个 kind，不带锚的正文。
const CROSS_KIND_CANONICAL: &str = "cross_kind_canonical";

/// 一次合流：哪条草案、从哪个 kind 改到哪个 kind、同一正名有几个候选旧锚。
#[derive(Debug, Clone)]
struct CrossKindFold {
    reference: String,
    from_kind: String,
    to_kind: String,
    candidates: usize,
}

/// 库里一条活锚的合流候选面（只要赢家规则用得到的那几列）。
#[derive(Debug, Clone)]
struct AliveAnchor {
    id: String,
    kind: String,
    canonical: String,
    status: String,
    created_at_ms: i64,
}

/// W15b1：草案的正名在同 scope 里已经有**另一个 kind** 的活锚时，把草案改从那一格。
///
/// 为什么只在写侧做：锚 id 由 (kind, 正名, scope) 算出来，同一个名字被两批判成两种 kind 就
/// 必然两行；`contract_v4` 的合并只在同一批内生效（跨批不并），写侧的歧义检查又只看同 kind。
/// 于是同一实体在两批里各建一格、主体解析分叉（审计 P1-1 的 10 对）。已存在的那些对**不**
/// 在这里重键、合并或删除——合并是数据动作，另排；本函数只保证新批次不再添第二格。
///
/// 方向单向且确定：**已存在的锚赢**，草案改从它的 kind 与正名，绝不反向改旧行。旧正名若与
/// 赢家不同，收进草案的别名（去重，且不收一个只差归一化形态的"同名"）。
///
/// 赢家规则（写死，不随查询顺序漂移）：`active` 优先于 `provisional`；其次 `created_at_ms`
/// 最小；再其次 `id` 字典序最小。同一正名有多个候选时按此取第一名，候选数进审计行。
fn fold_cross_kind_duplicates(
    conn: &rusqlite::Connection,
    batch: &mut A1Batch,
    scope: &Scope,
) -> Result<Vec<CrossKindFold>, MemError> {
    if batch.anchors.is_empty() {
        return Ok(Vec::new());
    }
    let mut by_name: BTreeMap<String, Vec<AliveAnchor>> = BTreeMap::new();
    for alive in alive_anchors_in_scope(conn, scope)? {
        by_name.entry(normalize_body(&alive.canonical)).or_default().push(alive);
    }

    let mut folds = Vec::new();
    for draft in batch.anchors.iter_mut() {
        let norm = normalize_body(&draft.canonical);
        if norm.is_empty() {
            continue; // 空正名由 `validate` 拒收，这里不参与分组
        }
        let candidates: Vec<&AliveAnchor> = by_name
            .get(&norm)
            .map(|alive| alive.iter().filter(|a| a.kind != draft.kind).collect())
            .unwrap_or_default();
        let Some(winner) = fold_winner(&candidates) else {
            continue;
        };
        let from_kind = std::mem::replace(&mut draft.kind, winner.kind.clone());
        let renamed = draft.canonical != winner.canonical;
        if renamed {
            let old = std::mem::replace(&mut draft.canonical, winner.canonical.clone());
            let old_norm = normalize_body(&old);
            let already = draft.aliases.iter().any(|a| normalize_body(a) == old_norm);
            if old_norm != normalize_body(&winner.canonical) && !already {
                draft.aliases.push(old);
            }
        }
        folds.push(CrossKindFold {
            reference: draft.reference.clone(),
            from_kind,
            to_kind: winner.kind.clone(),
            candidates: candidates.len(),
        });
    }
    Ok(folds)
}

/// 同 scope 里所有活锚的合流候选面。活 = `alive_where` 的三支（未退场、未否决、未被顶掉）
/// 加上状态列只认 `active`/`provisional`——退场/合并掉的锚不许把新草案吸过去。
///
/// 一次查询取全 scope 再在内存里按归一正名分组：草案数与锚数都是批级规模，逐条查会把
/// "一条草案一次全表扫"的代价乘上去。
fn alive_anchors_in_scope(
    conn: &rusqlite::Connection,
    scope: &Scope,
) -> Result<Vec<AliveAnchor>, MemError> {
    let alive = reader::alive_where(MemTable::Anchor);
    let sql = format!(
        "SELECT f.id, f.kind, f.canonical, f.status, f.created_at_ms FROM anchor f
          WHERE f.scope_kind=?1 AND f.scope_key=?2
            AND f.status IN ('active','provisional') AND ({alive})
         ORDER BY 1"
    );
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map(rusqlite::params![scope.kind.as_str(), scope.key], |r| {
        Ok(AliveAnchor {
            id: r.get(0)?,
            kind: r.get(1)?,
            canonical: r.get(2)?,
            status: r.get(3)?,
            created_at_ms: r.get(4)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 合流的赢家（规则只有一处，见 [`fold_cross_kind_duplicates`] 的文档）。
///
/// 比的是"哪一格更该留住"：已经 `active` 的那格是两次跑都认下来的，比仍待定的 `provisional`
/// 更该当主；同状态里打平就看谁先入库，再看 id——最后一格存在的唯一目的是让结果**恒定**，
/// 不随查询返回顺序漂移。
fn fold_winner<'a>(candidates: &[&'a AliveAnchor]) -> Option<&'a AliveAnchor> {
    candidates
        .iter()
        .copied()
        .min_by(|a, b| {
            let rank = |s: &str| u8::from(s != "active");
            rank(&a.status)
                .cmp(&rank(&b.status))
                .then(a.created_at_ms.cmp(&b.created_at_ms))
                .then_with(|| a.id.cmp(&b.id))
        })
}

/// 一条提及落库时的程序解析结果（P3-5）。
struct MentionPlan {
    /// 这一条提及最终挂在哪个锚上（唯一命中时可纠正模型给的 `anchor_ref`）。
    anchor_id: String,
    /// 最终写进 `mention.resolution` 的值。
    resolution: String,
}

/// 一批提及的程序解析读数 + 逐条落库计划。
#[derive(Default)]
struct MentionResolution {
    plans: Vec<MentionPlan>,
    /// 唯一命中且与模型给的锚不同 ⇒ 采纳了程序结果。
    rebound: usize,
    /// 命中多个锚 ⇒ 强制 `unresolved`。
    ambiguous: usize,
    /// 一个锚都没命中 ⇒ 保持模型的 resolution，只计数。
    unmatched: usize,
}

/// 名字到锚的程序解析（P3-5）：用 `writers_x::anchor_keys` 这把**已有的**尺子，把
/// mention 的 `surface` 与同批每个锚（canonical ∪ aliases）的锚键各算一次，取交集。
///
/// 三支口径：
/// 1. **唯一命中** ⇒ 以程序结果为准（模型把名字配给了别的锚时纠正 `anchor_ref`）；
///    `resolution` 仍是模型给的值——程序只证明"这个名字落在哪个锚上"，没有替模型
///    把"这段指代解没解出来"改强。
/// 2. **多命中** ⇒ `unresolved`：同一串名字能对上两个以上的锚，程序不替模型选。
///    行的 `anchor_id` 仍用模型给的那一格（它是批内合法引用，也是模型的判断），
///    只是这一列不再声称"已解析"。
/// 3. **不命中** ⇒ 保持模型的 `resolution` 原样（现有行为：短名/代词本来就没有锚键），
///    只计数——"程序没认出来"不是改判的理由。
fn resolve_mentions(
    batch: &A1Batch,
    merged: &BTreeMap<String, A1Anchor>,
    by_ref: &BTreeMap<String, MemKey>,
) -> MentionResolution {
    let mut keysets: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for (id, anchor) in merged {
        let mut keys: BTreeSet<String> = anchor_keys(&anchor.canonical).into_iter().collect();
        for alias in &anchor.aliases {
            keys.extend(anchor_keys(alias));
        }
        keysets.insert(id.as_str(), keys);
    }
    let mut out = MentionResolution::default();
    for mention in &batch.mentions {
        let model_id = by_ref[&mention.anchor_ref].id.clone();
        let surface: BTreeSet<String> = anchor_keys(&mention.surface).into_iter().collect();
        let hits: Vec<&str> = if surface.is_empty() {
            Vec::new()
        } else {
            keysets
                .iter()
                .filter(|(_, keys)| keys.iter().any(|key| surface.contains(key)))
                .map(|(id, _)| *id)
                .collect()
        };
        let (anchor_id, resolution) = match hits.as_slice() {
            [only] => {
                if *only != model_id.as_str() {
                    out.rebound += 1;
                }
                ((*only).to_string(), mention.resolution.clone())
            }
            [] => {
                out.unmatched += 1;
                (model_id, mention.resolution.clone())
            }
            _ => {
                out.ambiguous += 1;
                (model_id, "unresolved".to_string())
            }
        };
        out.plans.push(MentionPlan { anchor_id, resolution });
    }
    out
}

/// W14e：悬空表面的"形状类"。六个字节级的封闭类别，判定**只读表面串**。
///
/// 为什么需要它：模板一边要求"名字落不到实体就写进悬空列表"，一边又说哈希、
/// 文件路径、URL、反引号里的代码令牌不是实体；模型照前者做，程序照单全收，
/// 这一桶于是被倾倒物占满、真歧义读不出来。prompt 的排除条款必须配程序闸（项目规矩④）。
///
/// 为什么只有这六类、且顺序固定：宁可漏杀不可误杀。真名（`老张`、`测试组的李伟`、
/// `批次 M`、`阶段 4`）一个都不许被当成路径或数字丢掉——认不出的表面原样留在真名桶，
/// 读侧把两份计数分开看，越界的那一类再补一条闸，不在这里放宽。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SurfaceShape {
    Hex,
    Uuid,
    Url,
    Path,
    Code,
    Number,
}

impl SurfaceShape {
    const fn as_str(self) -> &'static str {
        match self {
            SurfaceShape::Hex => "hex",
            SurfaceShape::Uuid => "uuid",
            SurfaceShape::Url => "url",
            SurfaceShape::Path => "path",
            SurfaceShape::Code => "code",
            SurfaceShape::Number => "number",
        }
    }
}

/// 表面串的形状分类。`None` = 这六类都不命中（**默认留在真名桶**）。
///
/// 顺序按"判别力从强到弱"排：先判整串形态（UUID/URL/路径/哈希），再判包含关系
/// （反引号），最后判纯数字——一个串同时像两类时，取前一个标签，标签只用于诊断。
fn surface_shape(surface: &str) -> Option<SurfaceShape> {
    let text = surface.trim();
    if text.is_empty() {
        return None;
    }
    if is_uuid(text) {
        return Some(SurfaceShape::Uuid);
    }
    if text.contains("://") {
        return Some(SurfaceShape::Url);
    }
    if is_path(text) {
        return Some(SurfaceShape::Path);
    }
    // 纯数字先于哈希：一串只有数字的七位以上表面，标签应该是 number 而不是 hex。
    if is_bare_number(text) {
        return Some(SurfaceShape::Number);
    }
    if text.len() >= 7 && text.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(SurfaceShape::Hex);
    }
    if text.contains('`') {
        return Some(SurfaceShape::Code);
    }
    None
}

/// 标准 8-4-4-4-12：长度 36、四个连字符在固定的四位上、其余全是十六进制。
fn is_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(i, b)| match i {
        8 | 13 | 18 | 23 => *b == b'-',
        _ => b.is_ascii_hexdigit(),
    })
}

/// 文件路径的三种形态：盘符（`X:\` / `X:/`）、以 `/` 开头、UNC（以两个反斜杠开头）。
/// **只认这三种**：反斜杠开头但只有一个（`\foo`）的串不判路径——那不是本节点名的形状。
fn is_path(text: &str) -> bool {
    if text.starts_with('/') || text.starts_with("\\\\") {
        return true;
    }
    let mut chars = text.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(drive), Some(':'), Some('\\' | '/')) => drive.is_ascii_alphabetic(),
        _ => false,
    }
}

/// 纯数字：允许千分位逗号、小数点与结尾百分号。
///
/// 收得紧是故意的：逗号只许当千分位（首段 1~3 位、其后每段恰好 3 位），小数点只许出现
/// 一次且两侧都要有数字，别的字符一个都不收——`批次 M`、`阶段 4`、`分片 7` 因此不受影响。
fn is_bare_number(text: &str) -> bool {
    let body = text.strip_suffix('%').unwrap_or(text);
    let mut digits = 0usize;
    let mut run = 0usize;
    let mut commas = 0usize;
    let mut dot = false;
    for c in body.chars() {
        match c {
            '0'..='9' => {
                digits += 1;
                run += 1;
            }
            ',' => {
                if dot || run == 0 {
                    return false;
                }
                let first_ok = commas == 0 && run <= 3;
                if !first_ok && run != 3 {
                    return false;
                }
                commas += 1;
                run = 0;
            }
            '.' => {
                if dot || run == 0 {
                    return false;
                }
                dot = true;
                run = 0;
            }
            _ => return false,
        }
    }
    digits > 0 && run > 0 && (commas == 0 || dot || run == 3)
}

/// 一条悬空表面在**本批**的锚里能对上几个（canonical ∪ aliases，尺子仍是
/// [`anchor_keys`]——与 [`resolve_mentions`] 同一把，不新写第三把）。
///
/// 只读数、不改判：`0` 是"这一批里确实没有它的落点"（真悬空），大于 `0` 是
/// "批内有同名的锚，但这段表面没被解析到它身上"（真歧义）——两种处置不同，
/// 所以这个数要跟着审计行走。
fn candidate_anchor_count(surface: &str, merged: &BTreeMap<String, A1Anchor>) -> usize {
    let probe: BTreeSet<String> = anchor_keys(surface).into_iter().collect();
    if probe.is_empty() {
        return 0;
    }
    merged
        .values()
        .filter(|anchor| {
            let mut keys = anchor_keys(&anchor.canonical);
            for alias in &anchor.aliases {
                keys.extend(anchor_keys(alias));
            }
            keys.iter().any(|key| probe.contains(key))
        })
        .count()
}

/// 会话自锚的 `kind` 字面量（`anchor.kind` 的 CHECK 里有它，见 `rc-store` 的 DDL）。
const AGENT_KIND: &str = "agent";

/// 主人锚的 `kind` 字面量。人的锚就是 `person`——与 `ensure_agent_anchor` 的 `agent` 对称。
const PRINCIPAL_KIND: &str = "person";

/// 没有身份时的占位 id（设计 `2026-10-27-principal-identity` §2.1 第 3 级）。
/// 它**不是**一个人的名字，也不是可建锚的身份：写进 claim 表可以（"Agent 明确答了不知道"），
/// 但不能拿它造一个"无名用户"。
pub const UNKNOWN_PRINCIPAL_ID: &str = "unknown";

/// 这个 `principal_id` 算不算"身份已知"：空白与 [`UNKNOWN_PRINCIPAL_ID`] 都不算。
///
/// 只有这一处实现，两个读者共用（后者经 [`principal_anchor_canonical`]）：主人锚不建
/// 无名用户；程序闸（`rc_distill::review::gate`）只对身份已知的会话收紧 human 轮的主语硬检。
/// 把"未知"读成"已知"会硬拒用户自己的决定，读反了则让编出来的主语过关——两个方向都有代价，
/// 所以口径只有一条：空/`unknown` = 不知道。
pub fn principal_is_known(principal_id: &str) -> bool {
    let id = principal_id.trim();
    !id.is_empty() && id != UNKNOWN_PRINCIPAL_ID
}

/// 主人锚的 `canonical`：**恒等于稳定的 `principal_id`**（用户 2026-11-01 拍板："改名不换锚"）。
/// 没有身份 ⇒ `None`。
///
/// 旧规则是"可读名非空就用名字"，代价不是审美问题而是**数据分裂**：同一个人在
/// "有的 runtime 给得出显示名、有的给不出"时算出两个 id ⇒ 两格锚、两条互不相干的
/// `state_assertion` 时间线；而 `status='merged'` 在全仓**没有生产者**，一旦裂开就合不回来。
/// 可读名现在进 `alias` 表（同一格锚的别名，带 `valid_from_ms` 分段），显示面走
/// [`crate::reader::display_name`]。
///
/// `canonical` 是锚的身份字形，闸门判"主语锚 == 主人锚"时也从这里取，不另写一份规则——
/// 所以改这一格就够了：建锚、判锚、重放三条路径共用同一个算法。
pub fn principal_anchor_canonical<'a>(
    principal_id: &'a str,
    _principal_name: Option<&'a str>,
) -> Option<&'a str> {
    if !principal_is_known(principal_id) {
        return None;
    }
    Some(principal_id.trim())
}

/// 会话自锚的 `anchor_id`：`source_agent` 对应的 `kind='agent'` 锚，**只回库里已有的那一格**。
///
/// 四条口径：
/// 1. **身份只有一条算法**：与 [`ensure_agent_anchor`] 同一个 `compute_mem_id`，同 scope 同
///    canonical 落在同一格；写库层不另算一份 id。
/// 2. **不造锚**：这一格是只读查询——没有 `ensure_agent_anchor` 的调用方（夹具、旧入口、
///    没跑过选批的路径）拿到的就是 `None`。`fact.speaker_anchor_id` 有
///    `REFERENCES anchor(id)`，悬空 id 连写都写不进去，所以"查不到就不写"是唯一诚实的形状。
/// 3. **候选先归到链终点**（W15c）：自锚被 D8 合并/退场之后，算出来的仍是被合并的旧 id；
///    不解析就等于让合并后新写的"谁说的"静默变 `None`。解析入口与读侧同一个
///    （[`reader::redirect_terminal`]），写进去的指向与读回来的渲染才是一条链。
/// 4. **kind/活跃再核一遍，canonical 不再逐字比对**：id 相同已蕴含后两格，但夹具/手写行可以
///    把别的锚塞进这一格——说话人这一列由程序盖章，不接受"id 撞上就算"。正名反而不比了：
///    合并后的幸存锚正名可能已经不同（旧正名并进了 alias），终端就是权威。
pub fn session_agent_anchor_id(
    m: &MemoryHandle,
    source_agent: &str,
    scope: &Scope,
) -> Result<Option<String>, MemError> {
    if normalize_body(source_agent).is_empty() {
        return Ok(None);
    }
    let candidate = compute_mem_id(MemTable::Anchor, AGENT_KIND, source_agent, "", scope);
    let id = reader::redirect_terminal(m, &candidate)?;
    let hit = m.conn().query_row(
        "SELECT 1 FROM anchor
          WHERE id=?1 AND kind=?2 AND status IN ('active','provisional')",
        rusqlite::params![id, AGENT_KIND],
        |r| r.get::<_, i64>(0),
    );
    match hit {
        Ok(_) => Ok(Some(id)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 主人锚的 `anchor_id`：`kind='person'`、本 scope、归属行把 principal 绑在
/// `owner_principal_id` 上，且**归属行不带设备**的那一格。
///
/// 为什么靠 `resource_owner` 而不靠 canonical：写库层（fact/event/mention 的写入点）手里只有
/// `OwnerCtx.owner_principal_id`，没有 `principal_name`——主人锚的 canonical 是"名字优先、
/// 回落到 id"（[`principal_anchor_canonical`]），写库层复算不出来。归属行是 I1 设计里
/// "锚与 principal 绑定"的**唯一一处**落库形状（`2026-10-27-principal-identity` §3.2）。
///
/// `ro.device_id IS NULL` 不是随手加的：naming 建出来的 person 锚（正文里的人名）同样带着
/// 会话的 `owner_principal_id`，只有 [`ensure_principal_anchor`] 写归属时**不写设备**
/// （主人锚不是某一台设备的一次入库产物）。没有这一格，同 scope 里"主人"与"正文里的人名"
/// 就分不开，只能猜——所以那一格是 **`ensure_principal_anchor` 的契约，不许改**。
///
/// 身份未知（空/`unknown`）⇒ `None`；候选不是唯一（数据脏）⇒ `None`：不猜。
///
/// W15c：候选按上面的身份谓词取（`ORDER BY a.id`），**逐个过 [`reader::redirect_terminal`]
/// 再验终端**。D8 的合并是读侧指向、零搬运——归属行留在旧锚上，所以候选里会出现已退场的
/// 旧主人锚；只有沿链走到终点、且终点仍是**活 `person` 锚**，那个 id 才算"谁说的"。
/// 状态谓词因此落在**终端**上而不是候选上：活锚的终端永远是它自己，把状态过滤留在候选步
/// 会让这一处的解析恒为空转（已退场的旧锚一律进不了候选）。
///
/// 去重后的活终端集合必须是**恰好一个**：0 个（一条都没走到活锚）或多个（旧锚分叉到两个
/// 不同的幸存锚）都是 `None`——两个终点就是两个主张，这里不替数据挑一个。
pub fn session_principal_anchor_id(
    m: &MemoryHandle,
    principal_id: &str,
    scope: &Scope,
) -> Result<Option<String>, MemError> {
    let principal_id = principal_id.trim();
    if !principal_is_known(principal_id) {
        return Ok(None);
    }
    let mut st = m.conn().prepare(
        "SELECT a.id FROM anchor a
           JOIN resource_owner ro ON ro.mem_table='anchor' AND ro.mem_id=a.id
          WHERE a.kind=?1
            AND a.scope_kind=?2 AND a.scope_key=?3
            AND ro.owner_principal_id=?4 AND ro.device_id IS NULL
          ORDER BY a.id",
    )?;
    let rows = st.query_map(
        rusqlite::params![PRINCIPAL_KIND, scope.kind.as_str(), scope.key, principal_id],
        |r| r.get::<_, String>(0),
    )?;
    let mut candidates = Vec::new();
    for row in rows {
        candidates.push(row?);
    }
    drop(st);

    let mut terminals: Vec<String> = Vec::new();
    for candidate in candidates {
        let id = reader::redirect_terminal(m, &candidate)?;
        // 解析失败（Broken/Missing）回落的是候选自己，挡在这一格里：不返回死锚。
        let alive = m.conn().query_row(
            "SELECT 1 FROM anchor
              WHERE id=?1 AND kind=?2 AND status IN ('active','provisional')",
            rusqlite::params![id, PRINCIPAL_KIND],
            |r| r.get::<_, i64>(0),
        );
        match alive {
            Ok(_) => {}
            Err(rusqlite::Error::QueryReturnedNoRows) => continue,
            Err(e) => return Err(e.into()),
        }
        if !terminals.contains(&id) {
            terminals.push(id);
        }
    }
    Ok(if terminals.len() == 1 { terminals.pop() } else { None })
}

/// 一条 fact/event 行的**程序说话人**（`fact.speaker_anchor_id` 的唯一算法，写库层与读侧
/// 共用这一处）：
///
/// - 证据轮**自带说话人**（D 子 agent 接线）⇒ 那个 `kind='agent'` 锚。这一档**优先于**
///   下面按 `role` 推导的两档：`Agent` 工具结果的正文是子 agent 的结论，轮级 `speaker`
///   就是"谁说的"的权威值，不能再被"role=agent ⇒ 会话自锚"盖掉。
/// - 证据里**任一轮是 human** ⇒ 会话主人锚（[`session_principal_anchor_id`]）。human 说的
///   话，主语默认是"我"；混证据（human 陈述 + agent 回执）的事实也是 human 提出的，所以
///   判据是"任一"而不是"全是"（与闸 6c 的 human 判据同一口径）。
/// - 没有 human 证据 ⇒ 会话自锚（[`session_agent_anchor_id`]）。agent/system 轮都是本 agent
///   这一侧在说话。
/// - 身份未知（`principal_id` 为 `None`/空/`unknown`）的 human 行 ⇒ `None`：不造无名用户，
///   也不拿 agent 锚顶替（那会把"用户说的"记成"代理说的"）。
pub fn speaker_anchor_id(
    m: &MemoryHandle,
    source_agent: &str,
    principal_id: Option<&str>,
    evidence: &[(String, AnchorRole)],
    scope: &Scope,
) -> Result<Option<String>, MemError> {
    // 轮级说话人先判：`text` 证据按出现顺序，第一条有 speaker 的轮说了算（与
    // `fact.evidence_turn_uid` 取第一条同一取向——主证据决定"这是谁的话"）。
    for (turn_uid, _) in evidence {
        if let Some(id) = turn_speaker_anchor_id(m, turn_uid)? {
            return Ok(Some(id));
        }
    }
    let human = evidence.iter().any(|(_, role)| *role == AnchorRole::Human);
    if !human {
        return session_agent_anchor_id(m, source_agent, scope);
    }
    match principal_id {
        Some(principal_id) => session_principal_anchor_id(m, principal_id, scope),
        None => Ok(None),
    }
}

/// 这一轮**自带**的说话人锚：`mention.discourse_role='speaker'`、锚是 `kind='agent'`。
///
/// 只有导入侧的 [`record_turn_speakers`] 会写这种行；naming pass 的 `speaker` 提及若指向
/// person 锚（"某句话的说话人是张三"），不参与这里——fact 的说话人只认 agent 锚。
/// 只读，不建锚（`fact.speaker_anchor_id` 有外键，悬空 id 不该由本层造出来）。
fn turn_speaker_anchor_id(m: &MemoryHandle, turn_uid: &str) -> Result<Option<String>, MemError> {
    let hit = m.conn().query_row(
        "SELECT m.speaker_anchor_id FROM mention m
           JOIN anchor a ON a.id=m.anchor_id
          WHERE m.turn_uid=?1 AND m.speaker_anchor_id IS NOT NULL
            AND m.discourse_role='speaker' AND a.kind=?2
          ORDER BY m.created_at_ms, m.id LIMIT 1",
        rusqlite::params![turn_uid, AGENT_KIND],
        |r| r.get::<_, Option<String>>(0),
    );
    match hit {
        Ok(id) => Ok(id),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 把一批 turn 的**轮级说话人**落库（D 子 agent 接线的写入面）。
///
/// 每一格做两件事，同一个事务的前后：先为 `speaker` 确保一个 `kind='agent'` 锚
/// （canonical = speaker 字符串、scope 取会话 scope、`INSERT OR IGNORE` 幂等），
/// 再写一条 `discourse_role='speaker'` 的 `mention`（`turn_uid` + `speaker_anchor_id`
/// 都指向它）。`mention` 是库里唯一写着"某一轮是谁说的"的轮级形状；后续
/// [`speaker_anchor_id`] 与闸的 `turn_spoken_by` 都从这一列读，不另开表。
///
/// 口径：
/// 1. **只建 agent 锚**：子 agent 没有自己的 claim/主人锚；主人锚仍取父会话（`owner` 参数）。
/// 2. **幂等**：同一 speaker 第二次写入命中原锚（`INSERT OR IGNORE`），同一轮重放命中原
///    mention id（内容寻址）⇒ 不增行。
/// 3. **空白 = 没给**：空串/纯空白跳过，不造无名 agent。
/// 4. 返回**本次真正新插入**的 mention 行数（锚的幂等命中不计）。
pub fn record_turn_speakers(
    m: &MemoryHandle,
    ctx: &BatchCtx,
    owner: &OwnerCtx,
    scope: &Scope,
    turns: &[(String, String)],
    now_ms: i64,
) -> Result<usize, MemError> {
    if m.is_read_only() {
        return Err(MemError::ReadOnly);
    }
    // 先建锚（每格一个事务；既有行不覆盖），再一次性写 mention。
    let mut anchors: BTreeMap<&str, String> = BTreeMap::new();
    for (_, speaker) in turns {
        let name = speaker.trim();
        if name.is_empty() || anchors.contains_key(name) {
            continue;
        }
        let key = ensure_agent_anchor_for(m, ctx, owner, scope, name, now_ms)?;
        anchors.insert(name, key.id);
    }
    let mut inserted = 0usize;
    let tx = m.conn().unchecked_transaction()?;
    for (turn_uid, speaker) in turns {
        let name = speaker.trim();
        let Some(anchor_id) = anchors.get(name) else {
            continue;
        };
        let id = compute_mention_id(turn_uid, anchor_id, name, None, None);
        inserted += tx.execute(
            "INSERT OR IGNORE INTO mention(id, anchor_id, turn_uid, surface, span_start, span_end,
                 speaker_anchor_id, discourse_role, resolution, confidence,
                 source_agent, device_id, created_at_ms, norm_version)
             VALUES(?1,?2,?3,?4,NULL,NULL,?2,'speaker','resolved',1.0,?5,?6,?7,?8)",
            rusqlite::params![
                id,
                anchor_id,
                turn_uid,
                name,
                ctx.source_agent,
                ctx.device_id,
                now_ms,
                rc_store::capability::NORM_VERSION as i64
            ],
        )?;
    }
    tx.commit()?;
    Ok(inserted)
}

/// human 轮正文里的第一人称代词（D31）→ 一条**轮级 mention**，指向本会话的主人锚。
///
/// 口径：
/// 1. **「我」是权威说话人**：`speaker` / `resolved` / `1.0` / `speaker_anchor_id = 主人锚`
///    （它确实是这一轮在说话的那个人）。
/// 2. **「我们」是集合**：`speaker` / `provisional` / `0.7` / `speaker_anchor_id = NULL`。
///    不能把它当权威说话人（`load_turn_speakers` 只认 `resolved`），也避免放宽闸 6c 第 3 支
///    （`turn_spoken_by` 不看 `resolution`，只有这一列留空才是保险）。
/// 3. `surface` 存原文代词；`span_start/span_end` **一律 NULL**——span 进 id
///    （[`compute_mention_id`]），填了会把同一轮的模型 mention 变成第二行而不是幂等命中。
/// 4. 同一轮两种表面都出现 ⇒ **两行都写**（surface 不同、id 不同）；同一表面重复出现
///    ⇒ 仍只有一行（`compute_mention_id` 按 turn+anchor+surface 去重）。
/// 5. **只写 mention**：不建锚、不判断 claim、不读 transcript——这些归调用方。主人锚必须由
///    调用方先 `ensure_principal_anchor` 落库（`mention.anchor_id` 有外键）。
/// 6. 返回本次**真正新插入**的行数（与 [`record_turn_speakers`] 同口径）。
///
/// 检测不引分词器：`我们` 里的 `我` 不算独立自称，所以
/// `has_me = 我出现次数 > 我们出现次数`。`reader::name_occurs` 不能用来判「我」——
/// 它对单字非 ASCII 名恒 `false`。
pub fn record_principal_mentions(
    m: &MemoryHandle,
    ctx: &BatchCtx,
    turns: &[(String, String)],
    principal_anchor_id: &str,
    now_ms: i64,
) -> Result<usize, MemError> {
    if m.is_read_only() {
        return Err(MemError::ReadOnly);
    }
    let mut inserted = 0usize;
    let tx = m.conn().unchecked_transaction()?;
    for (turn_uid, body) in turns {
        let us = body.matches("我们").count();
        let me = body.matches('我').count();
        if us > 0 {
            let id = compute_mention_id(turn_uid, principal_anchor_id, "我们", None, None);
            inserted += tx.execute(
                "INSERT OR IGNORE INTO mention(id, anchor_id, turn_uid, surface, span_start, span_end,
                     speaker_anchor_id, discourse_role, resolution, confidence,
                     source_agent, device_id, created_at_ms, norm_version)
                 VALUES(?1,?2,?3,?4,NULL,NULL,NULL,'speaker','provisional',0.7,?5,?6,?7,?8)",
                rusqlite::params![
                    id,
                    principal_anchor_id,
                    turn_uid,
                    "我们",
                    ctx.source_agent,
                    ctx.device_id,
                    now_ms,
                    rc_store::capability::NORM_VERSION as i64
                ],
            )?;
        }
        if me > us {
            let id = compute_mention_id(turn_uid, principal_anchor_id, "我", None, None);
            inserted += tx.execute(
                "INSERT OR IGNORE INTO mention(id, anchor_id, turn_uid, surface, span_start, span_end,
                     speaker_anchor_id, discourse_role, resolution, confidence,
                     source_agent, device_id, created_at_ms, norm_version)
                 VALUES(?1,?2,?3,?4,NULL,NULL,?2,'speaker','resolved',1.0,?5,?6,?7,?8)",
                rusqlite::params![
                    id,
                    principal_anchor_id,
                    turn_uid,
                    "我",
                    ctx.source_agent,
                    ctx.device_id,
                    now_ms,
                    rc_store::capability::NORM_VERSION as i64
                ],
            )?;
        }
    }
    tx.commit()?;
    Ok(inserted)
}

/// 为一批的 `source_agent` 确保一个 agent 锚（程序产物）：`kind='agent'`、
/// `canonical=ctx.source_agent`、`status='active'`、`reality_status='real'`、
/// `scope=参数`、`created/updated=now`；`resource_owner` 用传入 `owner`（effective 上界）。
///
/// 为什么要有它：真实的 fact/event 抽取里，"我/本 agent 决定/计划/做了…"这类行的主体是
/// **会话自己**，而候选表此前只有命名 pass 从正文里认出来的实体。没有自锚时模型只能把
/// 主语写成"项目记忆/阶段台账"这类来源（冒烟发现 1），审核要么放错、要么整批拒。
///
/// 三条纪律：
/// 1. **身份只有一条算法**：id = `compute_mem_id(Anchor, "agent", source_agent, "", scope)`，
///    同 `(canonical, scope)` 落在同一格 ⇒ `INSERT OR IGNORE` 天然幂等，重放不长第二行。
/// 2. **既有行不覆盖**：锚的内容/时间、归属的 owner 都按"先写者赢"（`owner::insert` 是
///    `INSERT OR IGNORE`）——命名 pass 已经建过同名 agent 锚时，这里只回它的 id。
/// 3. **不猜人名、不造 principal**：canonical 只取 `ctx.source_agent`；它为空（会话没自报
///    主语）就是 `BadA1Field("source_agent")`，不拿设备号/默认 agent 兜底。
///
/// 锚与归属同一个事务：宁可两行都没有，也不要一行锚配不上归属。
pub fn ensure_agent_anchor(
    m: &MemoryHandle,
    ctx: &BatchCtx,
    owner: &OwnerCtx,
    scope: &Scope,
    now_ms: i64,
) -> Result<MemKey, MemError> {
    // 会话自锚就是"canonical = 会话 source_agent"的那一格：与轮级子 agent 锚同一条算法，
    // 只差 canonical 参数（[`ensure_agent_anchor_for`]）。
    ensure_agent_anchor_for(m, ctx, owner, scope, &ctx.source_agent, now_ms)
}

/// [`ensure_agent_anchor`] 的通用形：任意 `kind='agent'` 锚（canonical = `canonical`）。
///
/// 会话自锚（[`ensure_agent_anchor`]）与子 agent 锚（[`record_turn_speakers`]）共用这一处
/// id 算法与"先写者赢"的归属：同 `(canonical, scope)` 只有一格，重放不长第二行。
pub fn ensure_agent_anchor_for(
    m: &MemoryHandle,
    ctx: &BatchCtx,
    owner: &OwnerCtx,
    scope: &Scope,
    canonical: &str,
    now_ms: i64,
) -> Result<MemKey, MemError> {
    if m.is_read_only() {
        return Err(MemError::ReadOnly);
    }
    if normalize_body(canonical).is_empty() {
        return Err(MemError::BadA1Field("source_agent"));
    }
    let id = compute_mem_id(MemTable::Anchor, AGENT_KIND, canonical, "", scope);
    let tx = m.conn().unchecked_transaction()?;
    tx.execute(
        "INSERT OR IGNORE INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind,
                                      scope_key, status, profile_text, created_at_ms,
                                      updated_at_ms, norm_version)
         VALUES(?1,?2,?3,'real',NULL,?4,?5,'active',NULL,?6,?6,?7)",
        rusqlite::params![
            id,
            AGENT_KIND,
            canonical,
            scope.kind.as_str(),
            scope.key,
            now_ms,
            rc_store::capability::NORM_VERSION as i64
        ],
    )?;
    owner::insert(
        &tx,
        &owner::OwnerRow {
            mem_table: MemTable::Anchor.prefix().to_string(),
            mem_id: id.clone(),
            owner_principal_id: owner.owner_principal_id.clone(),
            owner_agent_id: owner.owner_agent_id.clone(),
            created_by_agent: ctx.source_agent.clone(),
            visibility: owner::effective(owner.visibility, scope.kind),
            retention: owner::normalize_retention(&owner.retention),
            scope: scope.clone(),
            source_agent: Some(ctx.source_agent.clone()),
            device_id: Some(ctx.device_id.clone()),
            created_at_ms: now_ms,
        },
    )?;
    tx.commit()?;
    Ok(MemKey { table: MemTable::Anchor, id })
}

/// 为一个**已知**主人确保一个 `kind='person'` 的主人锚（设计 §3.2，与
/// [`ensure_agent_anchor`] 并列）。三条与 agent 锚逐条同构：
///
/// 1. **身份只有一条算法**：canonical = [`principal_anchor_canonical`] ⇒ **恒等于稳定主键**
///    `principal_id`，可读名走 `alias`，
///    id = `compute_mem_id(Anchor, "person", canonical, "", scope)`；
///    同 `(canonical, scope)` 落在同一格 ⇒ `INSERT OR IGNORE` 幂等，重放不长第二行，
///    **换称呼也不长第二格**（改名不换锚——`merged` 没有生产者，裂开合不回来）。
/// 2. **既有行不覆盖**：锚的内容/时间与归属都按"先写者赢"（`owner::insert` 是
///    `INSERT OR IGNORE`）——命名 pass 已经建过同名 person 锚时，这里只回它的 id。
/// 3. **没有身份就不造"无名用户"**：`principal_id` 空或 [`UNKNOWN_PRINCIPAL_ID`]
///    ⇒ `Ok(None)`，锚与归属一行都不写。
///
/// `resource_owner` 写的就是调用方给的这个主人 id（不是 `OwnerCtx` 里可能为空的同名列），
/// 与锚同一个事务：宁可两行都没有，也不要一行锚配不上归属。
///
/// **契约（C1 起）**：这一行的 `device_id` 必须是 `NULL`——它是"主人锚"与"正文里的人名锚"
/// 在库里的唯一区别（两者都带同一个 `owner_principal_id`）。写库层与闸门靠
/// [`session_principal_anchor_id`] 的 `ro.device_id IS NULL` 找回主人锚；改这一格等于
/// 让"谁说的"整列失效。`source_agent` 同理写会话的 effective agent（不是设备）。
pub fn ensure_principal_anchor(
    m: &MemoryHandle,
    principal_id: &str,
    principal_name: Option<&str>,
    owner: &OwnerCtx,
    scope: &Scope,
    now_ms: i64,
) -> Result<Option<MemKey>, MemError> {
    if m.is_read_only() {
        return Err(MemError::ReadOnly);
    }
    let Some(canonical) = principal_anchor_canonical(principal_id, principal_name) else {
        return Ok(None);
    };
    let id = compute_mem_id(MemTable::Anchor, PRINCIPAL_KIND, canonical, "", scope);
    let tx = m.conn().unchecked_transaction()?;
    tx.execute(
        "INSERT OR IGNORE INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind,
                                      scope_key, status, profile_text, created_at_ms,
                                      updated_at_ms, norm_version)
         VALUES(?1,?2,?3,'real',NULL,?4,?5,'active',NULL,?6,?6,?7)",
        rusqlite::params![
            id,
            PRINCIPAL_KIND,
            canonical,
            scope.kind.as_str(),
            scope.key,
            now_ms,
            rc_store::capability::NORM_VERSION as i64
        ],
    )?;
    // 可读名进**别名**（同一格锚），不进身份。空名字不写空别名；`valid_from_ms` 用盖章时刻，
    // 显示面（`reader::display_name`）据此取"现在被叫的那个名字"。旧称呼**不删**——
    // 历史正文里出现的就是它，闸门"名字在正文里"那一支读的正是 canonical ∪ aliases。
    if let Some(name) = principal_name.map(str::trim).filter(|name| !name.is_empty()) {
        tx.execute(
            "INSERT OR IGNORE INTO alias(anchor_id, alias, valid_from_ms) VALUES(?1,?2,?3)",
            rusqlite::params![id, name, now_ms],
        )?;
    }
    owner::insert(
        &tx,
        &owner::OwnerRow {
            mem_table: MemTable::Anchor.prefix().to_string(),
            mem_id: id.clone(),
            owner_principal_id: Some(principal_id.trim().to_string()),
            owner_agent_id: owner.owner_agent_id.clone(),
            // `created_by_agent` 是"谁写的这一行"，仍是 agent（本函数签名里没有 source_agent，
            // 会话行的 effective owner_agent_id 是它最接近的读法）；主人只占 owner 那一列。
            created_by_agent: owner.owner_agent_id.clone(),
            visibility: owner::effective(owner.visibility, scope.kind),
            retention: owner::normalize_retention(&owner.retention),
            scope: scope.clone(),
            source_agent: Some(owner.owner_agent_id.clone()),
            device_id: None,
            created_at_ms: now_ms,
        },
    )?;
    tx.commit()?;
    Ok(Some(MemKey { table: MemTable::Anchor, id }))
}

/// 批内每轮的**程序角色**：锚证据里出现的 `(turn_uid, anchor_role)`，同一轮先到先得。
///
/// 为什么从锚证据取：`A1Mention` 自己没有 role 列，而锚证据的 `anchor_role` 是 pass 从
/// `turns.role` 抄进来的程序值（模型递不进这一格）。一条 mention 所在的轮通常正是它指向
/// 的锚被命名出来的那一轮，所以这一格覆盖得住；覆盖不到时调用方保持既有行为，不猜。
fn turn_roles_of(batch: &A1Batch) -> BTreeMap<&str, AnchorRole> {
    let mut out: BTreeMap<&str, AnchorRole> = BTreeMap::new();
    for anchor in &batch.anchors {
        for pv in &anchor.evidence {
            out.entry(pv.turn_uid.as_str()).or_insert(pv.anchor_role);
        }
    }
    out
}

/// 校验 + 建立 ref 索引。ref 唯一性在这里复查一次：调用方可能绕过 parser 直接构造批次。
fn validate(
    batch: &A1Batch,
    scope: &Scope,
) -> Result<BTreeMap<String, MemKey>, MemError> {
    let mut refs = BTreeSet::new();
    let mut by_ref = BTreeMap::new();
    for a in &batch.anchors {
        if a.reference.trim().is_empty() {
            return Err(MemError::BadA1Field("reference"));
        }
        if !refs.insert(a.reference.clone()) {
            return Err(MemError::BadA1Field("reference"));
        }
        if !is_valid_anchor_kind(&a.kind) {
            return Err(MemError::BadA1Field("kind"));
        }
        if normalize_body(&a.canonical).is_empty() {
            return Err(MemError::ContentEmpty);
        }
        if !REALITY_STATUSES.contains(&a.reality_status.as_str()) {
            return Err(MemError::BadA1Field("reality_status"));
        }
        if a.evidence.is_empty() {
            return Err(MemError::ProvenanceMissing(MemTable::Anchor.prefix()));
        }
        check_alias(&a.canonical)?;
        for alias in &a.aliases {
            check_alias(alias)?;
        }
        let id = compute_mem_id(MemTable::Anchor, &a.kind, &a.canonical, "", scope);
        by_ref.insert(a.reference.clone(), MemKey { table: MemTable::Anchor, id });
    }
    for m in &batch.mentions {
        if !by_ref.contains_key(&m.anchor_ref) {
            return Err(MemError::UnknownAnchorRef);
        }
        if let Some(speaker) = &m.speaker_ref {
            if !by_ref.contains_key(speaker) {
                return Err(MemError::UnknownAnchorRef);
            }
        }
        if m.surface.trim().is_empty() {
            return Err(MemError::BadA1Field("surface"));
        }
        if !DISCOURSE_ROLES.contains(&m.discourse_role.as_str()) {
            return Err(MemError::BadA1Field("discourse_role"));
        }
        if !MENTION_RESOLUTIONS.contains(&m.resolution.as_str()) {
            return Err(MemError::BadA1Field("resolution"));
        }
        if !(0.0..=1.0).contains(&m.confidence) || !m.confidence.is_finite() {
            return Err(MemError::BadA1Field("confidence"));
        }
    }
    Ok(by_ref)
}

fn insert_anchor(
    tx: &rusqlite::Transaction,
    key: &MemKey,
    draft: &A1Anchor,
    scope: &Scope,
    now_ms: i64,
) -> Result<(), MemError> {
    let status = if draft.stable { "active" } else { "provisional" };
    tx.execute(
        "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind, scope_key,
                            status, profile_text, created_at_ms, updated_at_ms, norm_version)
         VALUES(?1,?2,?3,?4,NULL,?5,?6,?7,NULL,?8,?8,?9)
         ON CONFLICT(id) DO NOTHING",
        rusqlite::params![
            key.id,
            draft.kind,
            draft.canonical,
            draft.reality_status,
            scope.kind.as_str(),
            scope.key,
            status,
            now_ms,
            rc_store::capability::NORM_VERSION as i64
        ],
    )?;
    // provisional → active 只升不降；merged/retired 不在 WHERE 的放行集里，因此不会被复活。
    tx.execute(
        "UPDATE anchor SET status='active', updated_at_ms=?1
         WHERE id=?2 AND status='provisional' AND ?3=1",
        rusqlite::params![now_ms, key.id, if draft.stable { 1 } else { 0 }],
    )?;
    // unknown 是一次真实的未知，不是"还没填"：已知值可以覆盖它，反过来不行。
    tx.execute(
        "UPDATE anchor SET reality_status=?1, updated_at_ms=?2
         WHERE id=?3 AND reality_status='unknown' AND ?1<>'unknown'",
        rusqlite::params![draft.reality_status, now_ms, key.id],
    )?;
    Ok(())
}

/// 这一串名字已经是哪些**同 kind、同 scope 活锚**的别名（排除自己）。
///
/// 与 [`alias_claimants`] 的两点差别：①方向是"新锚的**正名**遮住了别人的别名"；
/// ②按 `kind` + `scope` 收——跨 scope 的同名是隔离域里合法的另一个实体，不该报警。
/// 只读判定，不产生数据。
fn alias_shadowing_anchors(
    tx: &rusqlite::Transaction,
    kind: &str,
    name: &str,
    scope: &Scope,
    self_id: &str,
) -> Result<Vec<String>, MemError> {
    let alive = reader::alive_where(MemTable::Anchor);
    let sql = format!(
        "SELECT DISTINCT f.id FROM anchor f JOIN alias al ON al.anchor_id=f.id
          WHERE al.alias=?1 AND f.id<>?2 AND f.kind=?3
            AND f.scope_kind=?4 AND f.scope_key=?5 AND ({alive})
         ORDER BY 1"
    );
    let mut st = tx.prepare(&sql)?;
    let rows = st.query_map(
        rusqlite::params![name, self_id, kind, scope.kind.as_str(), scope.key],
        |r| r.get::<_, String>(0),
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 这串名字当前被哪些**活锚**认领（别名表 + 别的 canonical）。歧义别名不硬挂到新锚。
fn alias_claimants(
    tx: &rusqlite::Transaction,
    alias: &str,
    self_id: &str,
) -> Result<Vec<String>, MemError> {
    let alive = reader::alive_where(MemTable::Anchor);
    let sql = format!(
        "SELECT al.anchor_id FROM alias al JOIN anchor f ON f.id=al.anchor_id
          WHERE al.alias=?1 AND al.anchor_id<>?2 AND ({alive})
         UNION
         SELECT f.id FROM anchor f
          WHERE f.canonical=?1 AND f.id<>?2 AND ({alive})
         ORDER BY 1"
    );
    let mut st = tx.prepare(&sql)?;
    let rows = st.query_map(rusqlite::params![alias, self_id], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 与 `writers_c::raise_attention` 同一句 SQL，只是收事务而不是句柄：
/// A1 的锚、别名、提及与告警必须一起提交，事务外再开一条连接就会看到半批。
fn raise_attention(
    tx: &rusqlite::Transaction,
    kind: AttentionKind,
    id: &str,
    now_ms: i64,
) -> Result<bool, MemError> {
    let n = tx.execute(
        "INSERT INTO attention(kind, mem_table, mem_id, created_at_ms, resolved_at_ms, rechecked_at_ms)
         SELECT ?1, 'anchor', ?2, ?3, NULL, NULL
         WHERE NOT EXISTS (SELECT 1 FROM attention
                           WHERE kind=?1 AND mem_table='anchor' AND mem_id=?2
                             AND resolved_at_ms IS NULL)",
        rusqlite::params![kind.as_str(), id, now_ms],
    )?;
    if n > 1 {
        return Err(MemError::Sql(format!(
            "attention raise inserted {n} rows for {}:{id}",
            kind.as_str()
        )));
    }
    Ok(n == 1)
}

/// 问题审计行。同一 `(run_id, pass, code, detail)` 只留一条：重放不追加。
fn log_issue(
    tx: &rusqlite::Transaction,
    run_id: &str,
    code: &str,
    detail: &str,
    now_ms: i64,
) -> Result<(), MemError> {
    tx.execute(
        "INSERT INTO extraction_issue(run_id, pass, code, detail, created_at_ms)
         SELECT ?1,?2,?3,?4,?5
         WHERE NOT EXISTS (SELECT 1 FROM extraction_issue
                           WHERE run_id=?1 AND pass=?2 AND code=?3 AND detail=?4)",
        rusqlite::params![run_id, A1_PASS_DB, code, detail, now_ms],
    )?;
    Ok(())
}
