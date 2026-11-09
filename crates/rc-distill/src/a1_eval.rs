//! A1 双跑计划的评测器。
//!
//! 口径照 Hermes prompt loop 的“gold + 内容审计”两本账，在 A1 上落成可复算的数字：
//! 核心召回、全量召回、别名召回、kind 准确、同实体拆分、幻影锚、禁名命中、额外锚、
//! stable 覆盖，以及逐轮 mention / unresolved / 不建锚边界与结构闸。
//! 输入只有计划、gold 与（可选的）turn 上下文，不调模型；同一输入重放得到同一报告。

use crate::prompt::is_compact_summary;
use rc_contract::{compute_turn_uid, normalize_body};
use rc_memory::writers_v4::A1Batch;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// 双跑计划的落盘形状。`a1_probe --plan-out` 写它，`a1_eval` 读它。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanView {
    #[serde(default)]
    pub anchors: Vec<PlanAnchor>,
    #[serde(default)]
    pub mentions: Vec<PlanMention>,
    #[serde(default)]
    pub issues: Vec<PlanIssue>,
    #[serde(default)]
    pub unresolved: Vec<PlanUnresolved>,
}

impl PlanView {
    pub fn from_batch(batch: &A1Batch) -> PlanView {
        PlanView::from_batch_with_session(batch, "")
    }

    /// 多 session 合并时，锚与提及都带上所属 session；`reference` 仍是模型给的批内别名，
    /// 合并器会把它加上 session 前缀，避免两个 session 的 `a1` 相撞。
    pub fn from_batch_with_session(batch: &A1Batch, session_key: &str) -> PlanView {
        PlanView {
            anchors: batch
                .anchors
                .iter()
                .map(|a| PlanAnchor {
                    session_key: session_key.to_string(),
                    reference: a.reference.clone(),
                    kind: a.kind.clone(),
                    canonical: a.canonical.clone(),
                    aliases: a.aliases.clone(),
                    reality_status: a.reality_status.clone(),
                    evidence: a.evidence.iter().map(|p| p.turn_uid.clone()).collect(),
                    stable: a.stable,
                })
                .collect(),
            mentions: batch
                .mentions
                .iter()
                .map(|m| PlanMention {
                    session_key: session_key.to_string(),
                    anchor_ref: m.anchor_ref.clone(),
                    surface: m.surface.clone(),
                    turn_uid: m.turn_uid.clone(),
                    speaker_ref: m.speaker_ref.clone(),
                    discourse_role: m.discourse_role.clone(),
                    resolution: m.resolution.clone(),
                    confidence: m.confidence,
                })
                .collect(),
            issues: batch
                .issues
                .iter()
                .map(|i| PlanIssue {
                    kind: i.kind.clone(),
                    refs: i.refs.clone(),
                    reason: i.reason.clone(),
                })
                .collect(),
            unresolved: batch
                .unresolved
                .iter()
                .map(|u| PlanUnresolved {
                    surface: u.surface.clone(),
                    turn_uid: u.turn_uid.clone(),
                    reason: u.reason.clone(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanAnchor {
    /// 单 session 计划为空；多 session 合并计划里是所属 session。
    #[serde(default)]
    pub session_key: String,
    pub reference: String,
    pub kind: String,
    pub canonical: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub reality_status: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub stable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanMention {
    #[serde(default)]
    pub session_key: String,
    pub anchor_ref: String,
    pub surface: String,
    pub turn_uid: String,
    #[serde(default)]
    pub speaker_ref: Option<String>,
    pub discourse_role: String,
    pub resolution: String,
    #[serde(default)]
    pub confidence: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanIssue {
    pub kind: String,
    #[serde(default)]
    pub refs: Vec<String>,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanUnresolved {
    pub surface: String,
    pub turn_uid: String,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GoldFile {
    pub entities: Vec<GoldEntity>,
    #[serde(default)]
    pub forbidden: Vec<String>,
    /// 已经判定“允许存在但不进 gold”的额外锚（例如项目里程碑）。空 = 其余额外锚都算膨胀。
    #[serde(default)]
    pub allowed_extra: Vec<String>,
    /// 逐轮 gold。每个 exchange 里的 turn 必须出现一次；没有提及的轮也要留一条空记录，
    /// 这样“整轮漏标”不会伪装成“这轮本来就没有提及”。
    #[serde(default)]
    pub turns: Vec<GoldTurn>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GoldEntity {
    pub id: String,
    pub kind: String,
    /// 默认 exact：任一名字等于 tokens/aliases 之一。`mode="all"`：名字必须包含全部 tokens，
    /// 用来区分共用同一个姓名的两个实体（李伟 + 测试组 / 大学同学）。
    pub tokens: Vec<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub core: bool,
    #[serde(default)]
    pub mode: Option<String>,
    /// 这个实体所属的 session。空 = 不限定（旧 gold）；填了之后只在对应 session 的锚上匹配，
    /// 这样两个会话可以各自用“打印机”这种语域别名而不互相误配。
    #[serde(default)]
    pub sessions: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GoldTurn {
    #[serde(alias = "session")]
    pub session_key: String,
    #[serde(alias = "turn")]
    pub turn_index: usize,
    #[serde(default)]
    pub mentions: Vec<GoldMention>,
    /// 应进 `unresolved_surfaces` 的表面名（无名泛称、无法落地的实例）。
    #[serde(default)]
    pub unresolved: Vec<String>,
    /// 不许建锚、也不要求进 unresolved 的串（日期、数量、路径、纯角色词等）。
    #[serde(default)]
    pub no_anchor: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GoldMention {
    pub surface: String,
    pub entity: String,
    /// 只在原文明确归因（“张三说……”）时填实体 id，否则 null；只做诊断，不进硬门。
    #[serde(default)]
    pub speaker: Option<String>,
    #[serde(default = "default_role")]
    pub role: String,
}

fn default_role() -> String {
    "mentioned".to_string()
}

/// exchange 文件里一条已归一化的 turn。`turn_index` 是 session 内原始下标（0 起）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnCtx {
    pub session_key: String,
    pub turn_index: usize,
    pub turn_uid: String,
    pub body: String,
    pub role: String,
}

/// 从 exchange JSON 解析 turn 上下文。turn_uid 优先用文件里给的（IdBearing），
/// 否则用与导入管线同一个 `compute_turn_uid` 重算；空正文轮跳过（导入也会拒收）。
pub fn turns_from_exchange(doc: &serde_json::Value) -> Vec<TurnCtx> {
    let mut out = Vec::new();
    let Some(sessions) = doc.get("sessions").and_then(serde_json::Value::as_array) else {
        return out;
    };
    for session in sessions {
        let session_key = session
            .get("session_key")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let device_id = session
            .get("device_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let Some(turns) = session.get("turns").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for (turn_index, turn) in turns.iter().enumerate() {
            let Some(raw_body) = turn.get("body").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let body = normalize_body(raw_body);
            if body.is_empty() {
                continue;
            }
            let role = turn
                .get("role")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("human");
            let created_at_ms = turn
                .get("created_at_ms")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            let turn_uid = turn
                .get("turn_uid")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    compute_turn_uid(device_id, session_key, role, &body, created_at_ms)
                });
            out.push(TurnCtx {
                session_key: session_key.to_string(),
                turn_index,
                turn_uid,
                body,
                role: role.to_string(),
            });
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ratio {
    pub hit: usize,
    pub total: usize,
}

impl Ratio {
    fn new(hit: usize, total: usize) -> Self {
        Ratio { hit, total }
    }
    pub fn value(&self) -> f64 {
        if self.total == 0 {
            1.0
        } else {
            self.hit as f64 / self.total as f64
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Split {
    pub id: String,
    pub session_key: String,
    pub anchors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EntityResult {
    pub id: String,
    pub core: bool,
    pub matched: Vec<String>,
    pub stable: bool,
    pub kind_ok: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EvalReport {
    pub anchors: usize,
    pub stable_anchors: usize,
    pub mentions: usize,
    pub unresolved: usize,
    pub issues: usize,
    pub core_recall: Ratio,
    pub all_recall: Ratio,
    pub core_stable: Ratio,
    pub kind_accuracy: Ratio,
    pub alias_recall: Ratio,
    pub splits: Vec<Split>,
    pub phantom: Vec<String>,
    pub forbidden_hits: Vec<String>,
    pub extras: Vec<String>,
    pub per_entity: Vec<EntityResult>,

    /// 逐轮 gold 的读数。`gold_mentions`/`gold_unresolved` 为 0 时对应分母不存在，
    /// 门禁不能把“没标”读成“满分”。
    pub gold_turns: usize,
    pub gold_mentions: usize,
    pub gold_unresolved: usize,
    /// 实际计分的 plan mention 数（扣除 allowed_extra 锚的提及）。
    pub mention_scored: usize,
    /// 实际计分的 plan unresolved 数（扣除 gold 明确归入 no_anchor、以及 gold 完全未标注的表面）。
    pub unresolved_scored: usize,
    /// gold 未标注、因而无法判定对错的 plan unresolved 表面数。
    pub unresolved_unlabelled: usize,
    pub mention_precision: Ratio,
    pub mention_recall: Ratio,
    pub unresolved_precision: Ratio,
    pub unresolved_recall: Ratio,
    /// speaker/role 只做诊断：原文没有明确归因时，模型无法可靠推断说话人。
    pub speaker_accuracy: Ratio,
    pub role_accuracy: Ratio,

    /// 结构闸。全为空才是绿。
    pub alias_not_in_source: Vec<String>,
    pub evidence_out_of_batch: Vec<String>,
    pub anchors_without_evidence: Vec<String>,
    pub pronoun_names: Vec<String>,
    pub summary_evidence: Vec<String>,
    pub injection_evidence: Vec<String>,
    pub boundary_hits: Vec<String>,
    pub gold_turns_missing: Vec<String>,
}

/// 一次评测。`source` 是这批 turns 的原文（所有轮拼起来），用于判“幻影锚”：
/// 锚的任何名字（canonical 或 alias）都没在原文出现过，就是无源之锚。
pub fn evaluate(plan: &PlanView, gold: &GoldFile, source: &str) -> EvalReport {
    evaluate_impl(plan, gold, source, &[])
}

/// 带 turn 上下文的评测：能解析逐轮 mention / unresolved，并跑结构闸。
pub fn evaluate_with_turns(plan: &PlanView, gold: &GoldFile, turns: &[TurnCtx]) -> EvalReport {
    let source = turns
        .iter()
        .map(|t| t.body.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    evaluate_impl(plan, gold, &source, turns)
}

fn evaluate_impl(
    plan: &PlanView,
    gold: &GoldFile,
    source: &str,
    turns: &[TurnCtx],
) -> EvalReport {
    let source = norm(source);
    let session_sources = session_sources(turns);
    let mut per_entity = Vec::with_capacity(gold.entities.len());
    let mut alias_hit = 0usize;
    let mut alias_total = 0usize;
    let mut splits = Vec::new();
    let mut matched_refs = std::collections::BTreeSet::new();

    for entity in &gold.entities {
        let matched: Vec<&PlanAnchor> =
            plan.anchors.iter().filter(|a| matches(entity, a)).collect();
        for a in &matched {
            matched_refs.insert(a.reference.as_str());
        }
        let stable = matched.iter().any(|a| a.stable);
        let kind_ok = matched.iter().any(|a| a.kind == entity.kind);
        // 同一个实体在两个 session 各有一个锚是正常的；只有同一 session 内拆成多个锚才算拆分。
        let mut by_session: BTreeMap<&str, Vec<&PlanAnchor>> = BTreeMap::new();
        for a in &matched {
            by_session.entry(a.session_key.as_str()).or_default().push(a);
        }
        for (session_key, anchors) in by_session {
            if anchors.len() > 1 {
                splits.push(Split {
                    id: entity.id.clone(),
                    session_key: session_key.to_string(),
                    anchors: anchors.iter().map(|a| a.canonical.clone()).collect(),
                });
            }
        }
        for alias in &entity.aliases {
            alias_total += 1;
            let target = norm(alias);
            if matched.iter().any(|a| {
                std::iter::once(&a.canonical)
                    .chain(a.aliases.iter())
                    .any(|n| norm(n) == target)
            }) {
                alias_hit += 1;
            }
        }
        per_entity.push(EntityResult {
            id: entity.id.clone(),
            core: entity.core,
            matched: matched.iter().map(|a| a.canonical.clone()).collect(),
            stable,
            kind_ok,
        });
    }

    let core: Vec<&EntityResult> = per_entity.iter().filter(|e| e.core).collect();
    let core_recall = Ratio::new(
        core.iter().filter(|e| !e.matched.is_empty()).count(),
        core.len(),
    );
    let all_recall = Ratio::new(
        per_entity.iter().filter(|e| !e.matched.is_empty()).count(),
        per_entity.len(),
    );
    let core_stable = Ratio::new(core.iter().filter(|e| e.stable).count(), core.len());
    let matched_entities: Vec<&EntityResult> =
        per_entity.iter().filter(|e| !e.matched.is_empty()).collect();
    let kind_accuracy = Ratio::new(
        matched_entities.iter().filter(|e| e.kind_ok).count(),
        matched_entities.len(),
    );

    let phantom: Vec<String> = plan
        .anchors
        .iter()
        .filter(|a| {
            let src = anchor_source(a, &source, &session_sources);
            !names(a).iter().any(|n| src.contains(n))
        })
        .map(|a| a.canonical.clone())
        .collect();
    let forbidden = gold.forbidden.iter().map(|f| norm(f)).collect::<Vec<_>>();
    let forbidden_hits: Vec<String> = plan
        .anchors
        .iter()
        .filter(|a| {
            names(a)
                .iter()
                .any(|n| forbidden.iter().any(|f| !f.is_empty() && n.contains(f)))
        })
        .map(|a| a.canonical.clone())
        .collect();
    let allowed: Vec<String> = gold.allowed_extra.iter().map(|s| norm(s)).collect();
    let extras: Vec<String> = plan
        .anchors
        .iter()
        .filter(|a| !matched_refs.contains(a.reference.as_str()))
        .filter(|a| !allowed.contains(&norm(&a.canonical)))
        // 幻影/禁名已经单独点名，不再重复算进“额外锚”的膨胀率。
        .filter(|a| !phantom.contains(&a.canonical) && !forbidden_hits.contains(&a.canonical))
        .map(|a| a.canonical.clone())
        .collect();

    let mut report = EvalReport {
        anchors: plan.anchors.len(),
        stable_anchors: plan.anchors.iter().filter(|a| a.stable).count(),
        mentions: plan.mentions.len(),
        unresolved: plan.unresolved.len(),
        issues: plan.issues.len(),
        core_recall,
        all_recall,
        core_stable,
        kind_accuracy,
        alias_recall: Ratio::new(alias_hit, alias_total),
        splits,
        phantom,
        forbidden_hits,
        extras,
        per_entity,
        gold_turns: gold.turns.len(),
        gold_mentions: 0,
        gold_unresolved: 0,
        mention_scored: 0,
        unresolved_scored: 0,
        unresolved_unlabelled: 0,
        mention_precision: Ratio::new(0, 0),
        mention_recall: Ratio::new(0, 0),
        unresolved_precision: Ratio::new(0, 0),
        unresolved_recall: Ratio::new(0, 0),
        speaker_accuracy: Ratio::new(0, 0),
        role_accuracy: Ratio::new(0, 0),
        alias_not_in_source: Vec::new(),
        evidence_out_of_batch: Vec::new(),
        anchors_without_evidence: Vec::new(),
        pronoun_names: Vec::new(),
        summary_evidence: Vec::new(),
        injection_evidence: Vec::new(),
        boundary_hits: Vec::new(),
        gold_turns_missing: Vec::new(),
    };

    if !turns.is_empty() {
        evaluate_turns(plan, gold, turns, &mut report);
        evaluate_structure(plan, gold, &source, &session_sources, turns, &mut report);
        return report;
    }

    // 没有 turn 上下文时仍能跑结构里不依赖 turn_uid 的两项（旧调用方）。
    report.alias_not_in_source = alias_not_in_source(plan, &source, &session_sources);
    report.pronoun_names = pronoun_names(plan);
    report.anchors_without_evidence = anchors_without_evidence(plan);
    report.boundary_hits = boundary_hits(plan, gold);
    report
}

fn evaluate_turns(
    plan: &PlanView,
    gold: &GoldFile,
    turns: &[TurnCtx],
    report: &mut EvalReport,
) {
    let uid_by_turn: BTreeMap<(&str, usize), &str> = turns
        .iter()
        .map(|t| ((t.session_key.as_str(), t.turn_index), t.turn_uid.as_str()))
        .collect();
    let body_by_uid: BTreeMap<&str, &str> = turns
        .iter()
        .map(|t| (t.turn_uid.as_str(), t.body.as_str()))
        .collect();
    let uid_session: BTreeMap<&str, &str> = turns
        .iter()
        .map(|t| (t.turn_uid.as_str(), t.session_key.as_str()))
        .collect();

    // 锚 ref → 命中的 gold 实体集合。mention 的 anchor_ref 先过这张表再与 gold 比。
    let mut ref_entities: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for a in &plan.anchors {
        let ents: Vec<&str> = gold
            .entities
            .iter()
            .filter(|e| matches(e, a))
            .map(|e| e.id.as_str())
            .collect();
        if !ents.is_empty() {
            ref_entities.insert(a.reference.as_str(), ents);
        }
    }

    let mut anchor_by_ref: BTreeMap<&str, &PlanAnchor> = BTreeMap::new();
    for a in &plan.anchors {
        anchor_by_ref.insert(a.reference.as_str(), a);
    }
    let allowed_extra: Vec<String> = gold.allowed_extra.iter().map(|s| norm(s)).collect();

    let mut gold_mentions: Vec<(String, String, String, Option<String>, String)> = Vec::new();
    let mut gold_unresolved: Vec<(String, String)> = Vec::new();
    let mut gold_no_anchor: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    let mut entity_names: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();
    for e in &gold.entities {
        let names: std::collections::BTreeSet<String> =
            e.tokens.iter().chain(e.aliases.iter()).map(|s| norm(s)).collect();
        if e.sessions.is_empty() {
            entity_names.entry(String::new()).or_default().extend(names);
        } else {
            for session in &e.sessions {
                entity_names.entry(session.clone()).or_default().extend(names.clone());
            }
        }
    }
    for gt in &gold.turns {
        let Some(uid) = uid_by_turn.get(&(gt.session_key.as_str(), gt.turn_index)) else {
            report
                .gold_turns_missing
                .push(format!("{}#{}", gt.session_key, gt.turn_index));
            continue;
        };
        for m in &gt.mentions {
            gold_mentions.push((
                uid.to_string(),
                norm(&m.surface),
                m.entity.clone(),
                m.speaker.clone(),
                m.role.clone(),
            ));
        }
        for surface in &gt.unresolved {
            gold_unresolved.push((uid.to_string(), norm(surface)));
        }
        for surface in &gt.no_anchor {
            gold_no_anchor.insert((uid.to_string(), norm(surface)));
        }
    }
    report.gold_mentions = gold_mentions.len();
    report.gold_unresolved = gold_unresolved.len();

    let mut used = vec![false; gold_mentions.len()];
    let mut correct = 0usize;
    let mut speaker_hit = 0usize;
    let mut speaker_total = 0usize;
    let mut role_hit = 0usize;
    let mut role_total = 0usize;
    for pm in &plan.mentions {
        // 允许存在但不进 gold 的额外锚：它的提及不计入 precision 分母，也不进召回。
        if let Some(anchor) = anchor_by_ref.get(pm.anchor_ref.as_str()) {
            if !ref_entities.contains_key(pm.anchor_ref.as_str())
                && allowed_extra.contains(&norm(&anchor.canonical))
            {
                continue;
            }
        }
        report.mention_scored += 1;
        let Some(ents) = ref_entities.get(pm.anchor_ref.as_str()) else {
            continue;
        };
        let surface = norm(&pm.surface);
        let found = gold_mentions.iter().enumerate().find(|(i, gm)| {
            !used[*i]
                && gm.0 == pm.turn_uid
                && gm.1 == surface
                && ents.iter().any(|e| *e == gm.2)
        });
        let Some((i, gm)) = found else {
            continue;
        };
        used[i] = true;
        correct += 1;
        if let Some(expected) = &gm.3 {
            speaker_total += 1;
            let got = pm
                .speaker_ref
                .as_ref()
                .and_then(|r| ref_entities.get(r.as_str()))
                .and_then(|v| v.first());
            if got == Some(&expected.as_str()) {
                speaker_hit += 1;
            }
        }
        role_total += 1;
        if pm.discourse_role == gm.4 {
            role_hit += 1;
        }
    }
    report.mention_precision = Ratio::new(correct, report.mention_scored);
    report.mention_recall = Ratio::new(correct, gold_mentions.len());
    report.speaker_accuracy = Ratio::new(speaker_hit, speaker_total);
    report.role_accuracy = Ratio::new(role_hit, role_total);

    let mut used_gold = vec![false; gold_unresolved.len()];
    let mut unresolved_hit = 0usize;
    let mut unresolved_scored = 0usize;
    for pu in &plan.unresolved {
        let surface = norm(&pu.surface);
        let found = gold_unresolved
            .iter()
            .enumerate()
            .find(|(i, gu)| !used_gold[*i] && gu.0 == pu.turn_uid && gu.1 == surface);
        if let Some((i, _)) = found {
            used_gold[i] = true;
            unresolved_hit += 1;
            unresolved_scored += 1;
            continue;
        }
        // PlanView 没有 no_anchor 输出通道；模型把 gold 明确归入 no_anchor 的表面放进
        // unresolved，是层间归类而非误报，按中性处理。
        if gold_no_anchor.contains(&(pu.turn_uid.clone(), surface.clone())) {
            continue;
        }
        // gold 没标过这个表面：无法判定它该进 unresolved 还是该被忽略，按中性处理。
        // 唯一该罚的是“把 gold 里已知的实体名放进 unresolved”——那才是真正的归属错误。
        let rel = uid_session.get(pu.turn_uid.as_str()).cloned().unwrap_or_default();
        let known_entity = entity_names
            .get(rel)
            .map(|s| s.contains(&surface))
            .unwrap_or(false)
            || entity_names
                .get("")
                .map(|s| s.contains(&surface))
                .unwrap_or(false);
        if !known_entity {
            report.unresolved_unlabelled += 1;
            continue;
        }
        unresolved_scored += 1;
    }
    report.unresolved_scored = unresolved_scored;
    report.unresolved_precision = Ratio::new(unresolved_hit, unresolved_scored);
    report.unresolved_recall = Ratio::new(unresolved_hit, gold_unresolved.len());

    for a in &plan.anchors {
        for uid in &a.evidence {
            if let Some(body) = body_by_uid.get(uid.as_str()) {
                if is_compact_summary(body) {
                    report.summary_evidence.push(uid.clone());
                }
                if body.contains("[recollect]") {
                    report.injection_evidence.push(uid.clone());
                }
            }
        }
    }
}

fn evaluate_structure(
    plan: &PlanView,
    gold: &GoldFile,
    source: &str,
    session_sources: &BTreeMap<String, String>,
    turns: &[TurnCtx],
    report: &mut EvalReport,
) {
    let all_uids: BTreeSet<&str> = turns.iter().map(|t| t.turn_uid.as_str()).collect();
    let mut uids_by_session: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for t in turns {
        uids_by_session
            .entry(t.session_key.as_str())
            .or_default()
            .insert(t.turn_uid.as_str());
    }
    let empty: BTreeSet<&str> = BTreeSet::new();
    report.evidence_out_of_batch = plan
        .anchors
        .iter()
        .flat_map(|a| {
            let allowed = if a.session_key.is_empty() {
                &all_uids
            } else {
                uids_by_session.get(a.session_key.as_str()).unwrap_or(&empty)
            };
            a.evidence
                .iter()
                .filter(move |uid| !allowed.contains(uid.as_str()))
        })
        .cloned()
        .collect();
    report.alias_not_in_source = alias_not_in_source(plan, source, session_sources);
    report.anchors_without_evidence = anchors_without_evidence(plan);
    report.pronoun_names = pronoun_names(plan);
    report.boundary_hits = boundary_hits(plan, gold);
}

fn alias_not_in_source(
    plan: &PlanView,
    source: &str,
    session_sources: &BTreeMap<String, String>,
) -> Vec<String> {
    plan.anchors
        .iter()
        .flat_map(|a| {
            let src = anchor_source(a, source, session_sources);
            a.aliases
                .iter()
                .filter(move |alias| !alias.is_empty() && !src.contains(&norm(alias)))
        })
        .cloned()
        .collect()
}

fn session_sources(turns: &[TurnCtx]) -> BTreeMap<String, String> {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for t in turns {
        let entry = map.entry(t.session_key.clone()).or_default();
        entry.push_str(&t.body);
        entry.push('\n');
    }
    map.into_iter().map(|(k, v)| (k, norm(&v))).collect()
}

/// 多 session 合并计划按锚的 session 取原文；拿不到该 session 时就给空串，
/// 这样“引用了一个不存在的 session”不会被全量原文放行。
fn anchor_source<'a>(
    anchor: &PlanAnchor,
    global: &'a str,
    session_sources: &'a BTreeMap<String, String>,
) -> &'a str {
    if anchor.session_key.is_empty() {
        return global;
    }
    session_sources
        .get(&anchor.session_key)
        .map(String::as_str)
        .unwrap_or("")
}

fn anchors_without_evidence(plan: &PlanView) -> Vec<String> {
    plan.anchors
        .iter()
        .filter(|a| a.evidence.is_empty())
        .map(|a| a.canonical.clone())
        .collect()
}

fn pronoun_names(plan: &PlanView) -> Vec<String> {
    const PRONOUNS: [&str; 15] = [
        "我", "你", "他", "她", "它", "我们", "你们", "他们", "她们", "它们", "this", "that",
        "they", "it", "them",
    ];
    plan.anchors
        .iter()
        .filter(|a| {
            names(a)
                .iter()
                .any(|n| PRONOUNS.iter().any(|p| n == p))
        })
        .map(|a| a.canonical.clone())
        .collect()
}

fn boundary_hits(plan: &PlanView, gold: &GoldFile) -> Vec<String> {
    let mut by_session: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut all: Vec<String> = Vec::new();
    for t in &gold.turns {
        for s in &t.no_anchor {
            let n = norm(s);
            if !n.is_empty() {
                by_session
                    .entry(t.session_key.as_str())
                    .or_default()
                    .push(n.clone());
                all.push(n);
            }
        }
    }
    let empty: Vec<String> = Vec::new();
    plan.anchors
        .iter()
        .filter(|a| {
            let boundaries = if a.session_key.is_empty() {
                &all
            } else {
                by_session.get(a.session_key.as_str()).unwrap_or(&empty)
            };
            names(a)
                .iter()
                .any(|n| boundaries.iter().any(|b| n.contains(b)))
        })
        .map(|a| a.canonical.clone())
        .collect()
}

fn names(a: &PlanAnchor) -> Vec<String> {
    std::iter::once(&a.canonical)
        .chain(a.aliases.iter())
        .map(|n| norm(n))
        .collect()
}

fn matches(entity: &GoldEntity, anchor: &PlanAnchor) -> bool {
    if !entity.sessions.is_empty()
        && !anchor.session_key.is_empty()
        && !entity.sessions.iter().any(|s| s == &anchor.session_key)
    {
        return false;
    }
    let names = names(anchor);
    if entity.mode.as_deref() == Some("all") {
        return names
            .iter()
            .any(|n| entity.tokens.iter().all(|t| n.contains(&norm(t))));
    }
    let targets: Vec<String> = entity
        .tokens
        .iter()
        .chain(entity.aliases.iter())
        .map(|s| norm(s))
        .collect();
    names.iter().any(|n| targets.contains(n))
}

/// 评测用的归一：大小写不敏感、空白折叠。它与 `compute_mem_id` 的归一不是同一件事，
/// 但两者都只做“同义判等”，改一个不会影响另一个的语义。
fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}
