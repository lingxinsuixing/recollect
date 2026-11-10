//! v4 A1 契约：锚 / 提及 / 别名的语义草稿与校验。
//!
//! 这一层先独立存在，不接 v3 管线：A1 parser、A1 writer 与双跑 stable 过滤过闸之后，
//! 才把生产入口从 v3 切到 v4。这样 1176 条既有测试在切换前一条都不动。
//!
//! 模型只给**批内别名**（`ref`）与表面名；数据库 id、scope、device、时间戳全部由程序补。
//! 错误载荷只带下标与字段名，不回显模型正文（守卫 12）。

use crate::batch::Batch;
use rc_contract::{is_valid_anchor_kind, normalize_body};
use rc_memory::writers_v4::{A1Anchor as WriteAnchor, A1Batch, A1Issue as WriteIssue,
                            A1Mention as WriteMention, A1Unresolved as WriteUnresolved};
use rc_memory::{AnchorRole, Provenance, PRONOUNS, PRONOUNS_ASCII};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

pub use rc_contract::{DISCOURSE_ROLES, MENTION_RESOLUTIONS, REALITY_STATUSES};

pub const V4_SCHEMA_VERSION: u32 = 4;
pub const PASS_A1: &str = "a1";
pub const A1_KEYS: [&str; 6] = [
    "schema_version",
    "pass",
    "anchors",
    "mentions",
    "issues",
    "unresolved_surfaces",
];

pub const ISSUE_KINDS: [&str; 7] = [
    "ambiguous_anchor",
    "unresolved_surface",
    "missing_subject",
    "ambiguous_time",
    "conflict",
    "quote_mismatch",
    "other",
];

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAnchor {
    #[serde(rename = "ref")]
    reference: String,
    kind: String,
    canonical: String,
    #[serde(default)]
    aliases: Vec<String>,
    reality_status: String,
    evidence: Vec<String>,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMention {
    anchor_ref: String,
    surface: String,
    turn_uid: String,
    #[serde(default)]
    speaker_ref: Option<String>,
    discourse_role: String,
    resolution: String,
    confidence: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIssue {
    kind: String,
    #[serde(default)]
    refs: Vec<String>,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUnresolved {
    surface: String,
    turn_uid: String,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawA1 {
    schema_version: Option<u32>,
    pass: Option<String>,
    anchors: Option<Vec<RawAnchor>>,
    mentions: Option<Vec<RawMention>>,
    issues: Option<Vec<RawIssue>>,
    unresolved_surfaces: Option<Vec<RawUnresolved>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnchorDraft {
    pub reference: String,
    pub kind: String,
    pub canonical: String,
    pub aliases: Vec<String>,
    pub reality_status: String,
    pub evidence: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MentionDraft {
    pub anchor_ref: String,
    pub surface: String,
    pub turn_uid: String,
    pub speaker_ref: Option<String>,
    pub discourse_role: String,
    pub resolution: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IssueDraft {
    pub kind: String,
    pub refs: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnresolvedSurface {
    pub surface: String,
    pub turn_uid: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct A1Output {
    pub anchors: Vec<AnchorDraft>,
    pub mentions: Vec<MentionDraft>,
    pub issues: Vec<IssueDraft>,
    pub unresolved: Vec<UnresolvedSurface>,
}

/// 错误载荷只许是下标、静态字段名与形状过关的 uid；模型正文一个字都不进。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum A1Error {
    #[error("output is not one json object")]
    NotAnObject,
    /// 对象形状对、字段类型不对（例如 `confidence` 写成 `"high"`）。
    /// 不与 `NotAnObject` 混用：那一支会让"JSON 是好的、只是某格类型错"被读成"模型没给 JSON"，
    /// 排障时两件事的下一步完全不同。载荷不带模型正文（守卫 12）。
    #[error("output has a field of the wrong type")]
    BadShape,
    #[error("missing key {0}")]
    MissingKey(&'static str),
    #[error("schema_version must be 4")]
    WrongSchemaVersion,
    #[error("pass must be a1")]
    WrongPass,
    #[error("anchor {index} has a duplicate ref")]
    DuplicateRef { index: usize },
    #[error("anchor {index} has an invalid kind")]
    BadAnchorKind { index: usize },
    #[error("anchor {index} has empty {field}")]
    EmptyField { index: usize, field: &'static str },
    #[error("anchor {index} evidence ref is not in this batch: {uid}")]
    UnlistedEvidence { index: usize, uid: String },
    #[error("anchor {index} has an invalid reality_status")]
    BadRealityStatus { index: usize },
    #[error("mention {index} references an unknown anchor")]
    UnknownAnchorRef { index: usize },
    #[error("mention {index} has empty {field}")]
    EmptyMentionField { index: usize, field: &'static str },
    #[error("mention {index} ref is not in this batch: {uid}")]
    UnlistedMentionTurn { index: usize, uid: String },
    #[error("mention {index} has an invalid {field}")]
    BadMentionField { index: usize, field: &'static str },
    #[error("mention {index} confidence is not in 0..=1")]
    BadConfidence { index: usize },
    #[error("issue {index} has an invalid kind")]
    BadIssueKind { index: usize },
    #[error("unresolved surface {index} ref is not in this batch: {uid}")]
    UnlistedUnresolvedTurn { index: usize, uid: String },
    #[error("unresolved surface {index} has empty surface")]
    EmptyUnresolvedSurface { index: usize },
}

pub fn parse_a1(json: &str, batch: &Batch) -> Result<A1Output, A1Error> {
    let doc: serde_json::Value =
        serde_json::from_str(json).map_err(|_| A1Error::NotAnObject)?;
    if !doc.is_object() {
        return Err(A1Error::NotAnObject);
    }
    for key in A1_KEYS {
        if !doc.get(key).is_some() {
            return Err(A1Error::MissingKey(key));
        }
    }
    let raw: RawA1 = serde_json::from_value(doc).map_err(|_| A1Error::BadShape)?;
    if raw.schema_version != Some(V4_SCHEMA_VERSION) {
        return Err(A1Error::WrongSchemaVersion);
    }
    if raw.pass.as_deref() != Some(PASS_A1) {
        return Err(A1Error::WrongPass);
    }
    let raw_anchors = raw.anchors.ok_or(A1Error::MissingKey("anchors"))?;
    let raw_mentions = raw.mentions.ok_or(A1Error::MissingKey("mentions"))?;
    let raw_issues = raw.issues.ok_or(A1Error::MissingKey("issues"))?;
    let raw_unresolved = raw
        .unresolved_surfaces
        .ok_or(A1Error::MissingKey("unresolved_surfaces"))?;

    // 模型只看见批内短引用；这里映射回真实 turn_uid，PlanView/gold 仍用后者。
    let turn_refs: std::collections::HashMap<String, String> =
        crate::prompt_v4::a1_refs(batch)
            .into_iter()
            .map(|(reference, row)| (reference, row.turn_uid.clone()))
            .collect();

    let mut anchors: Vec<AnchorDraft> = Vec::with_capacity(raw_anchors.len());
    for (index, a) in raw_anchors.into_iter().enumerate() {
        if a.reference.trim().is_empty() {
            return Err(A1Error::EmptyField { index, field: "ref" });
        }
        if anchors.iter().any(|seen| seen.reference == a.reference) {
            return Err(A1Error::DuplicateRef { index });
        }
        if !is_valid_anchor_kind(&a.kind) {
            return Err(A1Error::BadAnchorKind { index });
        }
        if normalize_body(&a.canonical).is_empty() {
            return Err(A1Error::EmptyField { index, field: "canonical" });
        }
        if a.aliases.iter().any(|alias| alias.trim().is_empty()) {
            return Err(A1Error::EmptyField { index, field: "aliases" });
        }
        if a.evidence.is_empty() {
            return Err(A1Error::EmptyField { index, field: "evidence" });
        }
        let mut evidence = Vec::with_capacity(a.evidence.len());
        for reference in &a.evidence {
            match turn_refs.get(reference) {
                Some(uid) => evidence.push(uid.clone()),
                None => return Err(A1Error::UnlistedEvidence { index, uid: safe_uid(reference) }),
            }
        }
        if !REALITY_STATUSES.contains(&a.reality_status.as_str()) {
            return Err(A1Error::BadRealityStatus { index });
        }
        anchors.push(AnchorDraft {
            reference: a.reference,
            kind: a.kind,
            canonical: a.canonical,
            aliases: a.aliases,
            reality_status: a.reality_status,
            evidence,
            reason: a.reason,
        });
    }

    let mut mentions: Vec<MentionDraft> = Vec::with_capacity(raw_mentions.len());
    for (index, m) in raw_mentions.into_iter().enumerate() {
        if !anchors.iter().any(|a| a.reference == m.anchor_ref) {
            return Err(A1Error::UnknownAnchorRef { index });
        }
        if m.surface.trim().is_empty() {
            return Err(A1Error::EmptyMentionField { index, field: "surface" });
        }
        let turn_uid = match turn_refs.get(&m.turn_uid) {
            Some(uid) => uid.clone(),
            None => {
                return Err(A1Error::UnlistedMentionTurn {
                    index,
                    uid: safe_uid(&m.turn_uid),
                })
            }
        };
        if let Some(speaker) = &m.speaker_ref {
            if !anchors.iter().any(|a| a.reference == *speaker) {
                return Err(A1Error::UnknownAnchorRef { index });
            }
        }
        if !DISCOURSE_ROLES.contains(&m.discourse_role.as_str()) {
            return Err(A1Error::BadMentionField { index, field: "discourse_role" });
        }
        if !MENTION_RESOLUTIONS.contains(&m.resolution.as_str()) {
            return Err(A1Error::BadMentionField { index, field: "resolution" });
        }
        if !(0.0..=1.0).contains(&m.confidence) || !m.confidence.is_finite() {
            return Err(A1Error::BadConfidence { index });
        }
        mentions.push(MentionDraft {
            anchor_ref: m.anchor_ref,
            surface: m.surface,
            turn_uid,
            speaker_ref: m.speaker_ref,
            discourse_role: m.discourse_role,
            resolution: m.resolution,
            confidence: m.confidence,
        });
    }

    let mut issues: Vec<IssueDraft> = Vec::with_capacity(raw_issues.len());
    for (index, issue) in raw_issues.into_iter().enumerate() {
        if !ISSUE_KINDS.contains(&issue.kind.as_str()) {
            return Err(A1Error::BadIssueKind { index });
        }
        issues.push(IssueDraft { kind: issue.kind, refs: issue.refs, reason: issue.reason });
    }

    let mut unresolved: Vec<UnresolvedSurface> = Vec::with_capacity(raw_unresolved.len());
    for (index, u) in raw_unresolved.into_iter().enumerate() {
        if u.surface.trim().is_empty() {
            return Err(A1Error::EmptyUnresolvedSurface { index });
        }
        let turn_uid = match turn_refs.get(&u.turn_uid) {
            Some(uid) => uid.clone(),
            None => {
                return Err(A1Error::UnlistedUnresolvedTurn {
                    index,
                    uid: safe_uid(&u.turn_uid),
                })
            }
        };
        unresolved.push(UnresolvedSurface {
            surface: u.surface,
            turn_uid,
            reason: u.reason,
        });
    }

    Ok(A1Output { anchors, mentions, issues, unresolved })
}

/// 锚的身份键：`(kind, 归一 canonical)`。双跑取交集、跨批次去重、程序 ref 的排序都用它。
/// 归一化必须与 `compute_mem_id` 用同一把尺子，否则"两次其实是同一个锚"会算成两个。
pub fn anchor_key(a: &AnchorDraft) -> (String, String) {
    (a.kind.clone(), normalize_body(&a.canonical))
}

/// 双跑 stable：两次都出现的锚。只在一次出现的进库但标 `provisional`，A2 不挂它。
pub fn stable_anchor_keys(first: &A1Output, second: &A1Output) -> BTreeSet<(String, String)> {
    let second_keys: BTreeSet<(String, String)> = second.anchors.iter().map(anchor_key).collect();
    first
        .anchors
        .iter()
        .map(anchor_key)
        .filter(|k| second_keys.contains(k))
        .collect()
}

/// 把两次 A1 合成一份可落库计划：
/// - 锚取并集（同名合并别名/证据）；一方的 canonical 出现在另一方的 aliases 里 ⇒ 同一实体，
///   合成一个锚（双跑常给同一台设备换名字：老电脑 / ThinkPad）。模型显式报
///   `ambiguous_anchor` 的那几对不链接——它已经说了"同名不同人"。
/// - 两次都认出的实体（不管用的是哪个名字）⇒ `stable`；
/// - 提及/问题/未解析表面取并集并去重，模型 ref 重映射成程序 ref（a1、a2…）；
/// - 排序与合并都只依赖内容，不依赖两次运行的顺序 ⇒ 交换两次输入得到同一份计划。
pub fn plan_a1(first: &A1Output, second: &A1Output, batch: &Batch) -> A1Batch {
    let first_refs: BTreeMap<String, (String, String)> =
        first.anchors.iter().map(|a| (a.reference.clone(), anchor_key(a))).collect();
    let second_refs: BTreeMap<String, (String, String)> =
        second.anchors.iter().map(|a| (a.reference.clone(), anchor_key(a))).collect();
    let first_present: BTreeSet<(String, String)> = first_refs.values().cloned().collect();
    let second_present: BTreeSet<(String, String)> = second_refs.values().cloned().collect();

    let mut merged: BTreeMap<(String, String), AnchorDraft> = BTreeMap::new();
    for draft in first.anchors.iter().chain(second.anchors.iter()) {
        let key = anchor_key(draft);
        match merged.get_mut(&key) {
            Some(seen) => {
                for alias in &draft.aliases {
                    if !seen.aliases.contains(alias) {
                        seen.aliases.push(alias.clone());
                    }
                }
                for uid in &draft.evidence {
                    if !seen.evidence.contains(uid) {
                        seen.evidence.push(uid.clone());
                    }
                }
                if seen.reality_status == "unknown" && draft.reality_status != "unknown" {
                    seen.reality_status = draft.reality_status.clone();
                }
            }
            None => {
                merged.insert(key, draft.clone());
            }
        }
    }

    // 模型明说"同名不同人"的那些 ref 对：不许被别名链接合并。
    let mut forbidden: BTreeSet<((String, String), (String, String))> = BTreeSet::new();
    for (run, refs) in [(first, &first_refs), (second, &second_refs)] {
        for issue in run.issues.iter().filter(|i| i.kind == "ambiguous_anchor") {
            let keys: Vec<(String, String)> =
                issue.refs.iter().filter_map(|r| refs.get(r).cloned()).collect();
            for i in 0..keys.len() {
                for j in (i + 1)..keys.len() {
                    forbidden.insert(ordered(&keys[i], &keys[j]));
                }
            }
        }
    }

    // 并查集：同 kind 且"一方的 canonical 是另一方的 alias"才链接。
    let keys: Vec<(String, String)> = merged.keys().cloned().collect();
    let mut parent: BTreeMap<(String, String), (String, String)> =
        keys.iter().map(|k| (k.clone(), k.clone())).collect();
    for i in 0..keys.len() {
        for j in (i + 1)..keys.len() {
            let (a, b) = (&keys[i], &keys[j]);
            if forbidden.contains(&ordered(a, b)) {
                continue;
            }
            let same_kind = a.0 == b.0;
            let same_canonical =
                normalize_body(&merged[a].canonical) == normalize_body(&merged[b].canonical);
            // 同 kind 走别名链；不同 kind 但 canonical 完全相同（模型把同一个实体
            // 同时标成 event 和 work）也合并成一条。跨 kind 不按 alias 相交合并，
            // 避免把两个共享泛称别名的不同物体误并。
            if (same_kind && alias_linked(&merged[a], &merged[b])) || (!same_kind && same_canonical) {
                union(&mut parent, a, b);
            }
        }
    }
    let mut components: BTreeMap<(String, String), Vec<(String, String)>> = BTreeMap::new();
    for key in &keys {
        let root = find(&mut parent, key);
        components.entry(root).or_default().push(key.clone());
    }

    struct Planned {
        kind: String,
        canonical: String,
        aliases: Vec<String>,
        reality_status: String,
        evidence: Vec<String>,
        stable: bool,
        members: Vec<(String, String)>,
    }

    let mut planned: Vec<Planned> = Vec::with_capacity(components.len());
    for members in components.values() {
        let chosen = members
            .iter()
            .max_by_key(|k| {
                let in_first = first_present.contains(*k);
                let in_second = second_present.contains(*k);
                (in_first && in_second, in_first as u8 + in_second as u8,
                 k.1.chars().count(), (*k).clone())
            })
            .expect("component is never empty");
        let draft = &merged[chosen];
        // 代词永远不是名字：canonical 是代词就丢掉该锚；alias 是代词就剔除。
        // 这是写侧守卫 10 在计划层的同一条不变量，不让坏计划流到 writer。
        if is_pronoun(&draft.canonical) {
            continue;
        }
        let mut aliases = Vec::new();
        let mut evidence = Vec::new();
        let mut reality = "unknown".to_string();
        for m in members {
            for alias in &merged[m].aliases {
                if !is_pronoun(alias) && !aliases.contains(alias) {
                    aliases.push(alias.clone());
                }
            }
            // 被链接掉的 canonical 也是这个实体的别名；最终 canonical 自己不必再写一遍。
            if m != chosen
                && !is_pronoun(&merged[m].canonical)
                && !aliases.contains(&merged[m].canonical)
            {
                aliases.push(merged[m].canonical.clone());
            }
            for uid in &merged[m].evidence {
                if !evidence.contains(uid) {
                    evidence.push(uid.clone());
                }
            }
            if reality == "unknown" && merged[m].reality_status != "unknown" {
                reality = merged[m].reality_status.clone();
            }
        }
        planned.push(Planned {
            kind: draft.kind.clone(),
            canonical: draft.canonical.clone(),
            aliases,
            reality_status: reality,
            evidence,
            stable: members.iter().any(|m| first_present.contains(m))
                && members.iter().any(|m| second_present.contains(m)),
            members: members.clone(),
        });
    }
    planned.sort_by(|a, b| {
        (a.kind.clone(), normalize_body(&a.canonical))
            .cmp(&(b.kind.clone(), normalize_body(&b.canonical)))
            .then_with(|| a.canonical.cmp(&b.canonical))
    });

    let mut ref_by_key: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut anchors = Vec::with_capacity(planned.len());
    for (index, p) in planned.iter().enumerate() {
        let reference = format!("a{}", index + 1);
        for member in &p.members {
            ref_by_key.insert(member.clone(), reference.clone());
        }
        let evidence = p
            .evidence
            .iter()
            .map(|uid| Provenance {
                turn_uid: uid.clone(),
                // 角色只能从批次行读；role 值异常时退到 System，而不是把整批拒掉。
                anchor_role: batch.role_of(uid).unwrap_or(AnchorRole::System),
            })
            .collect();
        anchors.push(WriteAnchor {
            reference,
            kind: p.kind.clone(),
            canonical: p.canonical.clone(),
            aliases: p.aliases.clone(),
            reality_status: p.reality_status.clone(),
            evidence,
            stable: p.stable,
        });
    }

    let map_first = ref_map(first, &ref_by_key);
    let map_second = ref_map(second, &ref_by_key);
    let mut mentions: BTreeMap<(String, String, String, String), WriteMention> = BTreeMap::new();
    for (run, map) in [(first, &map_first), (second, &map_second)] {
        for m in &run.mentions {
            let Some(anchor_ref) = map.get(&m.anchor_ref) else { continue };
            let speaker_ref = m.speaker_ref.as_ref().and_then(|s| map.get(s).cloned());
            let key = (
                anchor_ref.clone(),
                m.turn_uid.clone(),
                normalize_body(&m.surface),
                m.discourse_role.clone(),
            );
            match mentions.get_mut(&key) {
                Some(seen) => {
                    seen.confidence = seen.confidence.max(m.confidence);
                    if resolution_rank(&m.resolution) > resolution_rank(&seen.resolution) {
                        seen.resolution = m.resolution.clone();
                    }
                    match (&speaker_ref, &seen.speaker_ref) {
                        (Some(new), Some(old)) if new < old => {
                            seen.speaker_ref = speaker_ref.clone();
                        }
                        (Some(_), None) => seen.speaker_ref = speaker_ref.clone(),
                        _ => {}
                    }
                }
                None => {
                    mentions.insert(
                        key,
                        WriteMention {
                            anchor_ref: anchor_ref.clone(),
                            surface: m.surface.clone(),
                            turn_uid: m.turn_uid.clone(),
                            speaker_ref,
                            discourse_role: m.discourse_role.clone(),
                            resolution: m.resolution.clone(),
                            confidence: m.confidence,
                        },
                    );
                }
            }
        }
    }

    let mut issues: BTreeMap<(String, Vec<String>, String), WriteIssue> = BTreeMap::new();
    for (run, map) in [(first, &map_first), (second, &map_second)] {
        for issue in &run.issues {
            let refs: Vec<String> = issue
                .refs
                .iter()
                .filter_map(|r| map.get(r).cloned())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let key = (issue.kind.clone(), refs.clone(), issue.reason.clone());
            issues.entry(key).or_insert_with(|| WriteIssue {
                kind: issue.kind.clone(),
                refs,
                reason: issue.reason.clone(),
            });
        }
    }

    let mut unresolved: BTreeMap<(String, String, String), WriteUnresolved> = BTreeMap::new();
    for run in [first, second] {
        for u in &run.unresolved {
            // 同一轮同一 surface 的 unresolved 只保留一条：reason 不同不构成第二条。
            let key = (normalize_body(&u.surface), u.turn_uid.clone(), String::new());
            unresolved.entry(key).or_insert_with(|| WriteUnresolved {
                surface: u.surface.clone(),
                turn_uid: u.turn_uid.clone(),
                reason: u.reason.clone(),
            });
        }
    }

    // 同一锚同一轮的嵌套表面只留最长的一条（“最大表面”规则）。
    let mut by_turn: BTreeMap<(String, String), Vec<WriteMention>> = BTreeMap::new();
    for m in mentions.into_values() {
        by_turn.entry((m.anchor_ref.clone(), m.turn_uid.clone())).or_default().push(m);
    }
    let mut kept_mentions = Vec::new();
    for (_, mut group) in by_turn {
        group.sort_by(|a, b| {
            normalize_body(&b.surface)
                .chars()
                .count()
                .cmp(&normalize_body(&a.surface).chars().count())
                .then_with(|| a.surface.cmp(&b.surface))
        });
        let mut kept: Vec<WriteMention> = Vec::new();
        for m in group {
            let surface = normalize_body(&m.surface);
            let nested = kept.iter().any(|k| {
                let outer = normalize_body(&k.surface);
                outer != surface && outer.contains(&surface)
            });
            if !nested {
                kept.push(m);
            }
        }
        kept_mentions.extend(kept);
    }

    A1Batch {
        anchors,
        mentions: kept_mentions,
        issues: issues.into_values().collect(),
        unresolved: unresolved.into_values().collect(),
    }
}

fn is_pronoun(s: &str) -> bool {
    PRONOUNS.contains(&s) || PRONOUNS_ASCII.contains(&s.to_lowercase().as_str())
}

/// 同一实体的两个名字：一方的 canonical 出现在另一方的 aliases 里。
/// 只认这个方向；两条不同 canonical 共享同一个别名是歧义，不是同义（那交给 issue/attention）。
fn alias_linked(a: &AnchorDraft, b: &AnchorDraft) -> bool {
    let ac = normalize_body(&a.canonical);
    let bc = normalize_body(&b.canonical);
    b.aliases.iter().any(|x| normalize_body(x) == ac)
        || a.aliases.iter().any(|x| normalize_body(x) == bc)
}

fn ordered(
    a: &(String, String),
    b: &(String, String),
) -> ((String, String), (String, String)) {
    if a <= b {
        (a.clone(), b.clone())
    } else {
        (b.clone(), a.clone())
    }
}

fn find(
    parent: &mut BTreeMap<(String, String), (String, String)>,
    key: &(String, String),
) -> (String, String) {
    let current = parent.get(key).cloned().unwrap_or_else(|| key.clone());
    if &current == key {
        return current;
    }
    let root = find(parent, &current);
    parent.insert(key.clone(), root.clone());
    root
}

fn union(
    parent: &mut BTreeMap<(String, String), (String, String)>,
    a: &(String, String),
    b: &(String, String),
) {
    let ra = find(parent, a);
    let rb = find(parent, b);
    if ra != rb {
        parent.insert(rb, ra);
    }
}

fn ref_map(
    run: &A1Output,
    ref_by_key: &BTreeMap<(String, String), String>,
) -> BTreeMap<String, String> {
    run.anchors
        .iter()
        .filter_map(|a| ref_by_key.get(&anchor_key(a)).map(|r| (a.reference.clone(), r.clone())))
        .collect()
}

fn resolution_rank(value: &str) -> u8 {
    match value {
        "resolved" => 2,
        "provisional" => 1,
        _ => 0,
    }
}

/// 形状过关的 uid 才原样回显；否则只回显长度（与 v3 的 `uid_echo` 同一纪律）。
fn safe_uid(uid: &str) -> String {
    if uid.len() == 64
        && uid
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        uid.to_string()
    } else {
        format!("<not-a-uid {} chars>", uid.chars().count())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::TurnRow;

    fn uid(c: char) -> String {
        c.to_string().repeat(64)
    }

    fn batch() -> Batch {
        Batch {
            session_key: "s".into(),
            device_id: "d".into(),
            source_agent: "a".into(),
            project_hint: None,
            rows: vec![
                TurnRow {
                    turn_uid: uid('a'),
                    role: "human".into(),
                    body: String::new(),
                    created_at_ms: 1,
                    ingested_at_ms: 1,
                    speaker: None,
                    speaker_anchor_id: None,
                },
                TurnRow {
                    turn_uid: uid('b'),
                    role: "agent".into(),
                    body: String::new(),
                    created_at_ms: 2,
                    ingested_at_ms: 2,
                    speaker: None,
                    speaker_anchor_id: None,
                },
            ],
        }
    }

    fn valid() -> String {
        r#"{"schema_version":4,"pass":"a1",
          "anchors":[{"ref":"a1","kind":"person","canonical":"张三","aliases":["小张"],
                      "reality_status":"real","evidence":["t1"],"reason":"原文明确"}],
          "mentions":[{"anchor_ref":"a1","surface":"她","turn_uid":"t2","speaker_ref":null,
                       "discourse_role":"mentioned","resolution":"resolved","confidence":0.9}],
          "issues":[],"unresolved_surfaces":[]}"#.to_string()
    }

    #[test]
    fn a1_valid_output_parses_and_refs_map_to_full_uids() {
        let out = parse_a1(&valid(), &batch()).unwrap();
        assert_eq!(out.anchors.len(), 1);
        assert_eq!(out.anchors[0].reference, "a1");
        assert_eq!(out.anchors[0].kind, "person");
        assert_eq!(out.anchors[0].evidence, vec![uid('a')], "t1 必须映射回第 1 行");
        assert_eq!(out.mentions.len(), 1);
        assert_eq!(out.mentions[0].surface, "她");
        assert_eq!(out.mentions[0].turn_uid, uid('b'), "t2 必须映射回第 2 行");
        assert!(out.issues.is_empty());
        assert!(out.unresolved.is_empty());
    }

    #[test]
    fn a1_compact_summary_rows_do_not_consume_a_ref() {
        let mut b = batch();
        b.rows[0].body = format!("摘要\n{}", crate::prompt::COMPACT_SUMMARY_MARKER);
        let json = valid().replace(r#""turn_uid":"t2""#, r#""turn_uid":"t1""#);
        let out = parse_a1(&json, &b).unwrap();
        assert_eq!(out.anchors[0].evidence, vec![uid('b')], "t1 应指向第 2 个可见行");
        assert_eq!(out.mentions[0].turn_uid, uid('b'));
    }

    #[test]
    fn a1_missing_key_is_rejected() {
        let json = valid().replace(r#""issues":[],"#, "");
        assert_eq!(parse_a1(&json, &batch()).unwrap_err(), A1Error::MissingKey("issues"));
    }

    #[test]
    fn a1_out_of_batch_evidence_echoes_only_a_safe_uid() {
        let bad = "a".repeat(65);
        let json = valid().replace(r#""evidence":["t1"]"#, &format!(r#""evidence":["{bad}"]"#));
        let err = parse_a1(&json, &batch()).unwrap_err();
        assert_eq!(
            err,
            A1Error::UnlistedEvidence { index: 0, uid: "<not-a-uid 65 chars>".into() }
        );
    }

    #[test]
    fn a1_unknown_anchor_ref_is_rejected() {
        let json = valid().replace(r#""anchor_ref":"a1""#, r#""anchor_ref":"zz""#);
        assert_eq!(
            parse_a1(&json, &batch()).unwrap_err(),
            A1Error::UnknownAnchorRef { index: 0 }
        );
    }

    #[test]
    fn a1_bad_kind_and_confidence_are_rejected() {
        let json = valid().replace(r#""kind":"person""#, r#""kind":"human""#);
        assert_eq!(parse_a1(&json, &batch()).unwrap_err(), A1Error::BadAnchorKind { index: 0 });

        let json = valid().replace(r#""confidence":0.9"#, r#""confidence":1.2"#);
        assert_eq!(parse_a1(&json, &batch()).unwrap_err(), A1Error::BadConfidence { index: 0 });
    }

    #[test]
    fn a1_duplicate_ref_is_rejected() {
        let dup = r#"{"ref":"a1","kind":"person","canonical":"另一个","aliases":[],"reality_status":"real","evidence":["t1"],"reason":"x"},"#;
        let json = valid().replace(r#""anchors":["#, &format!(r#""anchors":[{dup}"#));
        assert_eq!(parse_a1(&json, &batch()).unwrap_err(), A1Error::DuplicateRef { index: 1 });
    }

    #[test]
    fn a1_bad_issue_and_unresolved_turn_are_rejected() {
        let json = valid().replace(
            r#""issues":[]"#,
            r#""issues":[{"kind":"ghost","refs":[],"reason":""}]"#,
        );
        assert_eq!(parse_a1(&json, &batch()).unwrap_err(), A1Error::BadIssueKind { index: 0 });

        let json = valid().replace(
            r#""unresolved_surfaces":[]"#,
            r#""unresolved_surfaces":[{"surface":"那台打印机","turn_uid":"t9","reason":"信息不足"}]"#,
        );
        assert_eq!(
            parse_a1(&json, &batch()).unwrap_err(),
            A1Error::UnlistedUnresolvedTurn { index: 0, uid: "<not-a-uid 2 chars>".into() }
        );
    }

    #[test]
    fn a1_wrong_pass_and_non_object_are_rejected() {
        let json = valid().replace(r#""pass":"a1""#, r#""pass":"a2""#);
        assert_eq!(parse_a1(&json, &batch()).unwrap_err(), A1Error::WrongPass);
        assert_eq!(parse_a1("[]", &batch()).unwrap_err(), A1Error::NotAnObject);
    }

    #[test]
    fn a1_field_type_error_is_not_reported_as_a_non_object() {
        let json = valid().replace(r#""confidence":0.9"#, r#""confidence":"high""#);
        assert_eq!(parse_a1(&json, &batch()).unwrap_err(), A1Error::BadShape);
    }
}
