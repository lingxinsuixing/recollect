//! G1 刀 B 第 3 件：真模型下价值轴靶向小样（**环境变量 gated**）。
//!
//! 不设 `RECOLLECT_G1B_MEASURE_DB` 时这条用例打印一行跳过即 pass（CI/默认全量跑不碰现网，
//! 也不发任何模型调用）。
//!
//! 复现：
//! 1. 只读备份现网库到临时目录（SQLite backup API，绝不写现网；`memory.db` 与 `transcript.db`
//!    两件都要，命中与否由审核输入里的目标轮正文决定）：
//!    ```python
//!    import sqlite3, tempfile, os
//!    d = tempfile.mkdtemp(prefix="g1b_measure_")
//!    for name in ("memory.db", "transcript.db"):
//!        src = sqlite3.connect(f"file:D:/example/.local/node-data/{name}?mode=ro", uri=True)
//!        dst = sqlite3.connect(os.path.join(d, name))
//!        src.backup(dst); dst.close(); src.close()
//!    print(d)
//!    ```
//! 2. 供座位（值只走环境变量，绝不落盘/不打印）：
//!    `RECOLLECT_G1B_KEY`（密钥值）、`RECOLLECT_G1B_ENDPOINT`、`RECOLLECT_G1B_MODEL`、
//!    `RECOLLECT_G1B_TEMPERATURE`、`RECOLLECT_G1B_BUDGET`、`RECOLLECT_G1B_REASONING_EFFORT`。
//! 3. `RECOLLECT_G1B_MEASURE_DB=<那个临时目录> cargo test -p rc-distill --test g1b_live_value_axis -- --nocapture`
//!
//! 安全 belt：路径里含 `node-data`（现网目录名）直接 panic——测量只许在临时副本上跑。
//!
//! 口径：**最小可调用面**——直接复用 `pass_prompt` 的 fact/event 审核 proposer prompt
//! （`fact_review_system`/`fact_review_user`、`event_review_system`/`event_review_user`），
//! 每次调用只放一条 ITEM（`f1`/`e1`），解析用生产同一份契约（`parse_fact_review` /
//! `parse_event_review_full`）。目标轮正文与邻轮取自 transcript 副本，批头/说话人复用
//! `review::run::review_context`（与生产同一个函数）。不写库、不落判定。
//!
//! 抽样规则（派发档 §2）：
//! - A 残余流水：q02/q05 召回主循环里、镜像 fact 谓词 ∈ `CONSTRUCTION_PREDICATES` 的卡行，**全部**，
//!   期望按 `A_EXPECTATIONS` **逐行**给（瞬时快照 ⇒ demote / 持续约束 ⇒ keep），不再一律 demote；
//! - B 决策/承诺卡：`card.kind IN (decision,promise)` 对应的 fact 草稿，≥5 条，期望 keep；
//! - C 对照 keep：category ∈ (decision,promise,plan) 或谓词 ∈ 决策谓词闭集，≥3 条，期望 keep；
//! - D 对照 demote：施工谓词、category ∈ (event,observation,state_change)，≥3 条，期望 demote。

use rc_agent::recall::{recall, RecallRequest};
use rc_llm::{LlmBackend, Prompt};
use rc_llm_http::Live;
use rc_memory::owner::{parse_visibility, OwnerCtx};
use rc_memory::reader::RecallFilter;
use rc_memory::value_gate::{CONSTRUCTION_PREDICATES, DECISION_PREDICATES, EXEMPT_CATEGORIES};
use rc_memory::{Scope, ScopeKind};
use rc_store::AgentRoot;
use rc_distill::batch::{load_turn_speakers, Batch, TurnRow};
use rc_distill::pass_event_contract::{parse_event_review_full, EventDraft};
use rc_distill::pass_fact_contract::{parse_fact_review, FactDraft, ReviewTarget};
use rc_distill::pass_prompt::{
    event_review_system, event_review_user, fact_review_system, fact_review_user, FactFamily,
};
use rc_distill::review::run::review_context;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

fn norm(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

fn sha16(system: &str, user: &str) -> String {
    let mut h = Sha256::new();
    h.update(system.as_bytes());
    h.update(b"\n\x1e\n");
    h.update(user.as_bytes());
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    hex[..16].to_string()
}

fn cut(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out.replace('\n', " ")
}

/// 一张待测样本：来自哪一族、哪一条草稿、批与归属。
struct Sample {
    group: &'static str,
    fact_id: String,
    draft_ref: String,
    quote: String,
    expected: &'static str,
    family: Option<FactFamily>,
    system: String,
    user: String,
    /// 契约需要的那一份 target（fact 才有；event 用 refs 即可）。
    targets: Vec<ReviewTarget>,
}

fn family_of(slug: &str) -> Option<FactFamily> {
    FactFamily::ALL.iter().copied().find(|f| f.slug() == slug)
}

/// A 组（刀 A 残余 6 行）的**逐行**期望——按 2026-11-09 控制器裁定的收紧口径逐行分类
/// （`design/2026-11-09-value-axis-v06-experiment.md` §2）：一次性过程读数（瞬时快照）⇒
/// demote；持续约束/能力事实 ⇒ keep。**不是**旧口径的"残余一律 demote"：`11105ccfac64`
/// 记的是服务端下发的配额上限。
///
/// 该口径的 prompt 改写在 v0.6 试验里**未采纳**（A 组只命中 3/6，闸门按规则回滚，线上仍是
/// v0.5）；本表留下的是**判据分类**，与 prompt 版本无关，供下一次收紧时直接复用。
///
/// 表键是 `fact.id` 前 12 位（库内 id 是随机 hex，前缀足够指认）；没登记上的残余行记
/// `UNCLASSIFIED`——不算命中，并在输出里显形，绝不悄悄按 demote 计。
const A_EXPECTATIONS: &[(&str, &str, &str)] = &[
    ("11105ccfac64", "keep", "服务端下发的配额上限 $1/日 + 20 RPM = 持续约束"),
    ("1f96eb88b147", "demote", "轮询登录状态，等待结束即失效 = 瞬时快照"),
    ("76db98675c23", "demote", "一次性测试请求，被后续测试结论覆盖 = 非持续约束"),
    ("9fca7a29be02", "demote", "git diff --stat 为空 = 瞬时快照"),
    ("c6ef4b3b68c9", "demote", "今早两轮 calls/episodes 读数 = 瞬时快照"),
    ("fba7f2169c2b", "demote", "前 142 次 0 错 = 只说到\"目前为止\"的读数"),
];

/// `(expected, why)`；未登记的残余行返回 `UNCLASSIFIED`（不折算成 demote）。
fn a_expected(fact_id: &str) -> (&'static str, &'static str) {
    A_EXPECTATIONS
        .iter()
        .find(|(prefix, _, _)| fact_id.starts_with(*prefix))
        .map(|(_, expected, why)| (*expected, *why))
        .unwrap_or(("UNCLASSIFIED", "A 期望表未登记这一行"))
}

/// 一条 fact 行从审核载荷里找回它的草稿（fact/event 两族），供重渲染。
struct DraftLookup {
    /// (pass_id, family_slug, session_key, batch_turn_uids, owner_json, item_json, extra_json)
    family: String,
    session_key: String,
    turn_uids: Vec<String>,
    owner: Value,
    item: Value,
    extra: Option<Value>,
    pass_id: String,
}

#[test]
fn g1b_真模型价值轴靶向小样() {
    let Ok(dir) = std::env::var("RECOLLECT_G1B_MEASURE_DB") else {
        eprintln!("G1B_LIVE_SKIP: 未设 RECOLLECT_G1B_MEASURE_DB（临时副本目录），跳过真模型靶向小样。");
        return;
    };
    assert!(
        !dir.contains("node-data"),
        "测量只许在临时副本上跑，绝不指向现网目录（node-data）：{dir}"
    );
    let root = AgentRoot::open(std::path::Path::new(&dir)).unwrap();
    let m = root.memory();

    // 只读打开转录副本（目标轮正文与邻轮）。
    let t = rusqlite::Connection::open_with_flags(
        std::path::Path::new(&dir).join("transcript.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let turn_body = |uid: &str| -> Option<(String, String, i64, i64)> {
        t.query_row(
            "SELECT role, body, created_at_ms, ingested_at_ms FROM turns WHERE turn_uid=?1",
            [uid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .ok()
    };

    // 钟：取库里最新落卡时刻（与 g1a 同口径，冻结"截至最近一次落卡"的召回）。
    let now_ms: i64 = m
        .conn()
        .query_row(
            "SELECT COALESCE(max(created_at_ms), 0) FROM (SELECT created_at_ms FROM card UNION ALL SELECT created_at_ms FROM fact)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    rc_index::rebuild(m, now_ms).unwrap();

    let closed: HashSet<String> = CONSTRUCTION_PREDICATES.iter().map(|w| norm(w)).collect();
    let decision_words: HashSet<String> = DECISION_PREDICATES.iter().map(|w| norm(w)).collect();
    let exempt: HashSet<String> = EXEMPT_CATEGORIES.iter().map(|w| norm(w)).collect();

    // fact 行全表（只读）。
    struct FactRow {
        id: String,
        subject_anchor_id: String,
        predicate: String,
        value_text: Option<String>,
        category: String,
        turn_uid: String,
    }
    let facts: HashMap<String, FactRow> = {
        let mut st = m
            .conn()
            .prepare(
                "SELECT id, subject_anchor_id, predicate, value_text, category, evidence_turn_uid FROM fact",
            )
            .unwrap();
        let rows = st
            .query_map([], |r| {
                Ok(FactRow {
                    id: r.get(0)?,
                    subject_anchor_id: r.get(1)?,
                    predicate: r.get(2)?,
                    value_text: r.get(3)?,
                    category: r.get(4)?,
                    turn_uid: r.get(5)?,
                })
            })
            .unwrap();
        rows.map(|x| {
            let f = x.unwrap();
            (f.id.clone(), f)
        })
        .collect()
    };

    // card → 镜像 fact（取首条 fact 指针，与 g1a 同一口径）。
    let card_fact: HashMap<String, String> = {
        let mut st = m
            .conn()
            .prepare(
                "SELECT c.id, f.id FROM card c
                   JOIN json_each(c.pointers_json) p
                   JOIN fact f ON f.id = substr(p.value, 6)
                  WHERE p.value LIKE 'fact:%'",
            )
            .unwrap();
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).unwrap();
        let mut map = HashMap::new();
        for kv in rows {
            let (k, v) = kv.unwrap();
            map.entry(k).or_insert(v);
        }
        map
    };

    // 审核载荷索引：fact 用 (subject, predicate, value, turn)；event 用 (subject, action, value, turn)。
    let mut draft_index: HashMap<(String, String, String, String), DraftLookup> = HashMap::new();
    {
        let mut st = m
            .conn()
            .prepare("SELECT pass_id, family, session_key, payload_json FROM review_item WHERE pass_id IN ('fact','event')")
            .unwrap();
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .unwrap();
        for row in rows {
            let (pass_id, family, session_key, payload) = row.unwrap();
            let Ok(p) = serde_json::from_str::<Value>(&payload) else { continue };
            let Some(items) = p["items"].as_array() else { continue };
            let uids: Vec<String> = p["batch"]["turn_uids"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let owner = p["owner"].clone();
            for (idx, item) in items.iter().enumerate() {
                let subject = item["subject_anchor_id"].as_str().unwrap_or("").to_string();
                let turn = item["turn_uid"].as_str().unwrap_or("").to_string();
                let value = item["value"].as_str().unwrap_or("").to_string();
                let lookup = DraftLookup {
                    family: family.clone(),
                    session_key: session_key.clone(),
                    turn_uids: uids.clone(),
                    owner: owner.clone(),
                    item: item.clone(),
                    extra: p["extras"].as_array().and_then(|a| a.get(idx)).cloned(),
                    pass_id: pass_id.clone(),
                };
                if pass_id == "fact" {
                    let predicate = item["predicate"].as_str().unwrap_or("").to_string();
                    draft_index
                        .entry((subject, predicate, value, turn))
                        .or_insert(lookup);
                } else {
                    let action = p["extras"]
                        .as_array()
                        .and_then(|a| a.get(idx))
                        .and_then(|e| e["value_json"]["action"].as_str())
                        .unwrap_or("")
                        .to_string();
                    draft_index
                        .entry((subject, action, value, turn))
                        .or_insert(lookup);
                }
            }
        }
    }
    let lookup_of = |f: &FactRow| -> Option<&DraftLookup> {
        draft_index
            .get(&(
                f.subject_anchor_id.clone(),
                f.predicate.clone(),
                f.value_text.clone().unwrap_or_default(),
                f.turn_uid.clone(),
            ))
    };

    // ── A：q02/q05 主循环里的残余施工行 ────────────────────────────────────────
    let mut residual_facts: Vec<String> = Vec::new();
    for q in ["示例昵称是什么", "为什么砍掉调用配额"] {
        let req = RecallRequest::Query(q.to_string());
        let Ok(tj) = recall(m, &req, &RecallFilter::empty(), now_ms) else { continue };
        let v: Value = serde_json::from_str(&tj.text()).unwrap();
        for row in v["rows"].as_array().cloned().unwrap_or_default() {
            let Some(id) = row["id"].as_str() else { continue };
            let Some(card_id) = id.strip_prefix("card:") else { continue };
            let Some(fact_id) = card_fact.get(card_id) else { continue };
            let Some(fact) = facts.get(fact_id) else { continue };
            if closed.contains(&norm(&fact.predicate)) {
                residual_facts.push(fact_id.clone());
            }
        }
    }
    residual_facts.sort();
    residual_facts.dedup();

    // ── B：decision/promise 卡对应的 fact 草稿 ────────────────────────────────
    let mut keep_card_facts: Vec<String> = {
        let mut st = m
            .conn()
            .prepare("SELECT id FROM card WHERE kind IN ('decision','promise') ORDER BY created_at_ms DESC, id")
            .unwrap();
        let rows = st.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.filter_map(|x| x.ok())
            .filter_map(|card_id| card_fact.get(&card_id).cloned())
            .collect()
    };
    keep_card_facts.dedup();

    // ── C：普通决策/规则行（有载荷、未进 A/B）────────────────────────────────
    let mut control_keep: Vec<String> = facts
        .values()
        .filter(|f| {
            (exempt.contains(&norm(&f.category)) || decision_words.contains(&norm(&f.predicate)))
                && !residual_facts.contains(&f.id)
                && !keep_card_facts.contains(&f.id)
                && lookup_of(f).is_some()
        })
        .map(|f| f.id.clone())
        .collect();
    control_keep.sort();

    // ── D：明显流水行（施工谓词、非残余、有载荷）─────────────────────────────
    let mut control_demote: Vec<String> = facts
        .values()
        .filter(|f| {
            closed.contains(&norm(&f.predicate))
                && matches!(norm(&f.category).as_str(), "event" | "observation" | "state_change")
                && !residual_facts.contains(&f.id)
                && lookup_of(f).is_some()
        })
        .map(|f| f.id.clone())
        .collect();
    control_demote.sort();

    // ── 组装样本（B 取 6、C 取 3、D 取 3，A 全量且逐行给期望）────────────────
    let mut picked: Vec<(&'static str, &'static str, String)> = Vec::new();
    let mut a_unclassified = 0usize;
    for id in &residual_facts {
        let (expected, why) = a_expected(id);
        if expected == "UNCLASSIFIED" {
            a_unclassified += 1;
        }
        println!("EXPECT\tA_residual\t{}\t{expected}\t{why}", &id[..id.len().min(12)]);
        picked.push(("A_residual", expected, id.clone()));
    }
    let mut b = 0;
    for id in &keep_card_facts {
        if b >= 6 {
            break;
        }
        if lookup_of(facts.get(id).unwrap()).is_some() && !residual_facts.contains(id) {
            picked.push(("B_decision_card", "keep", id.clone()));
            b += 1;
        }
    }
    for id in control_keep.iter().take(3) {
        picked.push(("C_control_keep", "keep", id.clone()));
    }
    for id in control_demote.iter().take(3) {
        picked.push(("D_control_worklog", "demote", id.clone()));
    }

    // 构造每一条的单行审核输入。
    let mut samples: Vec<Sample> = Vec::new();
    let mut unrenderable: Vec<(String, String)> = Vec::new();
    for (group, expected, fact_id) in picked {
        let Some(fact) = facts.get(&fact_id) else {
            unrenderable.push((fact_id, "fact row vanished".into()));
            continue;
        };
        let Some(lookup) = lookup_of(fact) else {
            unrenderable.push((fact_id, "no review payload draft".into()));
            continue;
        };
        // 批：按载荷顺序取轮（正文/角色/钟来自转录副本）。
        let mut rows: Vec<TurnRow> = Vec::new();
        for uid in &lookup.turn_uids {
            if let Some((role, body, created, ingested)) = turn_body(uid) {
                rows.push(TurnRow {
                    turn_uid: uid.clone(),
                    role,
                    body,
                    created_at_ms: created,
                    ingested_at_ms: ingested,
                    speaker: None,
                    speaker_anchor_id: None,
                });
            }
        }
        if rows.is_empty() {
            unrenderable.push((fact_id, "target turns missing from transcript copy".into()));
            continue;
        }
        let _ = load_turn_speakers(m, &mut rows);
        let batch = Batch {
            session_key: lookup.session_key.clone(),
            device_id: lookup
                .owner
                .get("device_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            source_agent: rows_source_agent(&lookup.owner),
            project_hint: None,
            rows,
        };
        let owner = OwnerCtx {
            owner_principal_id: lookup.owner["owner_principal_id"].as_str().map(str::to_string),
            owner_agent_id: lookup.owner["owner_agent_id"].as_str().unwrap_or("").to_string(),
            visibility: parse_visibility(
                lookup.owner["visibility"].as_str().unwrap_or("private_agent"),
            ),
            retention: lookup.owner["retention"].as_str().unwrap_or("default").to_string(),
        };
        let scope = Scope {
            kind: ScopeKind::parse(lookup.owner["scope_kind"].as_str().unwrap_or("project"))
                .unwrap_or(ScopeKind::Project),
            key: lookup.owner["scope_key"].as_str().unwrap_or("").to_string(),
        };
        let ctx = match review_context(m, &lookup.session_key, &batch, &owner, &scope) {
            Ok(ctx) => ctx,
            Err(e) => {
                unrenderable.push((fact_id.clone(), format!("review_context: {e:?}")));
                continue;
            }
        };
        let item = &lookup.item;
        if lookup.pass_id == "fact" {
            let Some(family) = family_of(&lookup.family) else {
                unrenderable.push((fact_id, format!("unknown fact family `{}`", lookup.family)));
                continue;
            };
            let draft = FactDraft {
                reference: "f1".to_string(),
                turn_uid: item["turn_uid"].as_str().unwrap_or("").to_string(),
                subject_anchor_id: item["subject_anchor_id"].as_str().unwrap_or("").to_string(),
                object_anchor_id: item["object_anchor_id"].as_str().map(str::to_string),
                subject_canonical: item["subject_canonical"].as_str().unwrap_or("").to_string(),
                object_canonical: item["object_canonical"].as_str().map(str::to_string),
                predicate: item["predicate"].as_str().unwrap_or("").to_string(),
                value: item["value"].as_str().map(str::to_string),
                category: item["category"].as_str().unwrap_or("").to_string(),
                mode: item["mode"].as_str().unwrap_or("stated").to_string(),
                time_text: item["time_text"].as_str().unwrap_or("").to_string(),
                quote: item["quote"].as_str().unwrap_or("").to_string(),
                evidence_turn_uids: item["evidence"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|e| e["turn_uid"].as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                text: item["text"].as_str().unwrap_or("").to_string(),
            };
            samples.push(Sample {
                group,
                fact_id,
                draft_ref: "f1".into(),
                quote: draft.quote.clone(),
                expected,
                family: Some(family),
                system: fact_review_system(family),
                user: fact_review_user(family, std::slice::from_ref(&draft), &batch, &ctx),
                targets: vec![ReviewTarget::new("f1", draft.quote.clone())],
            });
        } else {
            let extra = lookup.extra.as_ref().cloned().unwrap_or(Value::Null);
            let value_json = extra.get("value_json").cloned().unwrap_or(Value::Null);
            let draft = EventDraft {
                reference: "e1".to_string(),
                turn_uid: item["turn_uid"].as_str().unwrap_or("").to_string(),
                category: item["category"].as_str().unwrap_or("").to_string(),
                actor_anchor_id: item["subject_anchor_id"].as_str().unwrap_or("").to_string(),
                object_anchor_id: item["object_anchor_id"].as_str().map(str::to_string),
                object_text: item["value"].as_str().map(str::to_string),
                participants_anchor_ids: extra["participants"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                place_anchor_id: extra["place_anchor_id"].as_str().map(str::to_string),
                trigger: value_json["trigger"].as_str().unwrap_or("").to_string(),
                action: value_json["action"].as_str().unwrap_or("").to_string(),
                result: value_json["result"].as_str().unwrap_or("unknown").to_string(),
                mode: item["mode"].as_str().unwrap_or("stated").to_string(),
                time_text: item["time_text"].as_str().unwrap_or("").to_string(),
                quote: item["quote"].as_str().unwrap_or("").to_string(),
                evidence_turn_uids: item["evidence"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|e| e["turn_uid"].as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                text: item["text"].as_str().unwrap_or("").to_string(),
                actor_canonical: item["actor_canonical"].as_str().unwrap_or("").to_string(),
                object_canonical: item["object_canonical"].as_str().map(str::to_string),
                participant_canonicals: item["participants_canonical"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                place_canonical: item["place_canonical"].as_str().map(str::to_string),
            };
            samples.push(Sample {
                group,
                fact_id,
                draft_ref: "e1".into(),
                quote: draft.quote.clone(),
                expected,
                family: None,
                system: event_review_system(),
                user: event_review_user(std::slice::from_ref(&draft), &batch, &ctx),
                targets: Vec::new(),
            });
        }
    }

    println!("=== G1B LIVE VALUE AXIS BEGIN ===");
    println!("db_dir: {dir}");
    println!("now_ms: {now_ms}");
    println!(
        "pool: facts={} residual={} residual_unclassified={} decision_card={} control_keep={} control_demote={} samples={} unrenderable={}",
        facts.len(),
        residual_facts.len(),
        a_unclassified,
        keep_card_facts.len(),
        control_keep.len(),
        control_demote.len(),
        samples.len(),
        unrenderable.len()
    );
    for (id, why) in &unrenderable {
        println!("UNRENDERABLE\t{}\t{}", &id[..id.len().min(12)], why);
    }

    let key = std::env::var("RECOLLECT_G1B_KEY").unwrap_or_default();
    let dry = std::env::var("RECOLLECT_G1B_DRY").ok().as_deref() == Some("1");
    if !dry && key.is_empty() {
        eprintln!("G1B_LIVE_BLOCKED: RECOLLECT_G1B_KEY 为空——不造读数，直接失败。");
        panic!("measurement key missing");
    }
    let endpoint = std::env::var("RECOLLECT_G1B_ENDPOINT").unwrap_or_default();
    let model = std::env::var("RECOLLECT_G1B_MODEL").unwrap_or_default();
    let protocol = std::env::var("RECOLLECT_G1B_PROTOCOL").unwrap_or_else(|_| "openai-chat".into());
    let temperature: f64 = std::env::var("RECOLLECT_G1B_TEMPERATURE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    let budget: u32 = std::env::var("RECOLLECT_G1B_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8192);
    let effort = std::env::var("RECOLLECT_G1B_REASONING_EFFORT")
        .ok()
        .filter(|s| !s.is_empty());
    if !dry {
        println!("seat: model={model} endpoint={endpoint} protocol={protocol} temp={temperature} budget={budget} effort={effort:?}");
    }

    let backend = if dry {
        None
    } else {
        let live = Live::new_custom(
            &endpoint,
            &protocol,
            &model,
            key,
            budget,
            Some(temperature),
            Box::new(rc_llm_http::transport::agent(300_000)),
        )
        .unwrap();
        Some(live.with_reasoning_effort(effort.clone()))
    };

    let mut hits = 0usize;
    let mut attempts = 0usize;
    let mut retries = 0usize;
    let mut failures: Vec<String> = Vec::new();
    let mut per_group: HashMap<&str, (usize, usize)> = HashMap::new();
    if std::env::var("RECOLLECT_G1B_SHOW_PROMPT").ok().as_deref() == Some("1") {
        if let Some(first) = samples.first() {
            println!("--- PROMPT DUMP ({} / {}) ---", first.group, &first.fact_id[..12]);
            println!("SYSTEM:\n{}", first.system);
            println!("USER:\n{}", first.user);
            println!("--- END PROMPT DUMP ---");
        }
    }
    for s in &samples {
        let prompt_sha = sha16(&s.system, &s.user);
        if dry {
            println!(
                "DRY\t{}\tfacts:{}\tref:{}\tquote:{}",
                s.group,
                &s.fact_id[..s.fact_id.len().min(12)],
                s.draft_ref,
                cut(&s.quote, 40)
            );
            continue;
        }
        let backend = backend.as_ref().unwrap();
        // 共享网关的并发闸（429）是运输面故障，不是读数：退避后重试，最多 3 次尝试。
        // 每一次成功读数仍来自**一行一次**调用；尝试次数逐行记进 attempts。
        let mut sends = 0usize;
        let mut started = std::time::Instant::now();
        let mut out = backend.complete(&Prompt {
            system: s.system.clone(),
            user: s.user.clone(),
            derive: false,
        });
        sends += 1;
        for backoff_ms in [5_000u64, 12_000] {
            if out.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
            started = std::time::Instant::now();
            out = backend.complete(&Prompt {
                system: s.system.clone(),
                user: s.user.clone(),
                derive: false,
            });
            sends += 1;
        }
        let ms = started.elapsed().as_millis() as i64;
        retries += sends.saturating_sub(1);
        attempts += 1;
        let entry = per_group.entry(s.group).or_insert((0, 0));
        match out {
            Ok(raw) => {
                // 原文留档（可选，写到临时目录；报告只用其中的判决，不改写、不落库）。
                if let Ok(rd) = std::env::var("RECOLLECT_G1B_RAW_DIR") {
                    let path = std::path::Path::new(&rd).join(format!(
                        "{}-{}.json",
                        s.group,
                        &s.fact_id[..s.fact_id.len().min(12)]
                    ));
                    let _ = std::fs::write(path, &raw);
                }
                let parsed: Result<(String, String), String> = if let Some(family) = s.family {
                    parse_fact_review(&raw, &s.targets, family)
                        .map_err(|e| format!("fact parse: {e:?}"))
                        .and_then(|report| {
                            match report.value_axis.iter().find(|(r, _)| r == &s.draft_ref) {
                                Some((_, axis)) => Ok((
                                    axis.as_str().to_string(),
                                    report
                                        .verdicts
                                        .iter()
                                        .find(|(r, _, _)| r == &s.draft_ref)
                                        .map(|(_, v, c)| format!("{v:?}/{c:?}"))
                                        .unwrap_or_default(),
                                )),
                                None => Err(format!("axis missing for {}", s.draft_ref)),
                            }
                        })
                } else {
                    parse_event_review_full(&raw, &["e1".to_string()])
                        .map_err(|e| format!("event parse: {e:?}"))
                        .and_then(|report| {
                            match report.value_axis.iter().find(|(r, _)| r == &s.draft_ref) {
                                Some((_, axis)) => Ok((
                                    axis.as_str().to_string(),
                                    report
                                        .verdicts
                                        .iter()
                                        .find(|(r, _, _)| r == &s.draft_ref)
                                        .map(|(_, v, c)| format!("{v:?}/{c:?}"))
                                        .unwrap_or_default(),
                                )),
                                None => Err(format!("axis missing for {}", s.draft_ref)),
                            }
                        })
                };
                match parsed {
                    Ok((axis, verdict)) => {
                        entry.0 += 1;
                        if axis == s.expected {
                            hits += 1;
                            entry.1 += 1;
                        }
                        println!(
                            "ROW\t{}\tfacts:{}\tref:{}\texpected:{}\tactual:{}\tverdict:{}\tmodel:{}\tsha:{}\tms:{}\tquote:{}",
                            s.group,
                            &s.fact_id[..s.fact_id.len().min(12)],
                            s.draft_ref,
                            s.expected,
                            axis,
                            verdict,
                            model,
                            prompt_sha,
                            ms,
                            cut(&s.quote, 40)
                        );
                    }
                    Err(why) => {
                        failures.push(format!(
                            "{}/{} {} parse failure: {why}",
                            s.group,
                            &s.fact_id[..s.fact_id.len().min(12)],
                            s.draft_ref
                        ));
                        entry.0 += 1;
                        println!(
                            "ROW\t{}\tfacts:{}\tref:{}\texpected:{}\tactual:PARSE_FAIL\tmodel:{}\tsha:{}\tms:{}\traw_len:{}\tquote:{}",
                            s.group,
                            &s.fact_id[..s.fact_id.len().min(12)],
                            s.draft_ref,
                            s.expected,
                            model,
                            prompt_sha,
                            ms,
                            raw.len(),
                            cut(&s.quote, 40)
                        );
                    }
                }
            }
            Err(fail) => {
                failures.push(format!(
                    "{}/{} {} llm failure {fail}",
                    s.group,
                    &s.fact_id[..s.fact_id.len().min(12)],
                    s.draft_ref
                ));
                println!(
                    "ROW\t{}\tfacts:{}\tref:{}\texpected:{}\tactual:LLM_FAIL({fail})\tmodel:{}\tsha:{}\tms:{}\tquote:{}",
                    s.group,
                    &s.fact_id[..s.fact_id.len().min(12)],
                    s.draft_ref,
                    s.expected,
                    model,
                    prompt_sha,
                    ms,
                    cut(&s.quote, 40)
                );
            }
        }
        // 共享网关并发紧：行间留一点空隙，减少 429 碰撞（不影响单行读数）。
        std::thread::sleep(std::time::Duration::from_millis(1_500));
    }

    println!("--- SUMMARY ---");
    let key_groups = ["A_residual", "B_decision_card", "C_control_keep", "D_control_worklog"];
    for g in key_groups {
        if let Some((n, h)) = per_group.get(g) {
            println!("GROUP\t{g}\trated={n}\thits={h}");
        }
    }
    let a = per_group.get("A_residual").copied().unwrap_or((0, 0));
    let b = per_group.get("B_decision_card").copied().unwrap_or((0, 0));
    println!(
        "HIT_RATES\tresidual_axis={}/{}\tdecision_keep={}/{}\toverall={}/{}\tattempts={attempts}\tretries={retries}\tfailures={}",
        a.1,
        a.0,
        b.1,
        b.0,
        hits,
        attempts,
        failures.len()
    );
    for f in &failures {
        println!("FAILED_ROW\t{f}");
    }
    println!("=== G1B LIVE VALUE AXIS END ===");
}

fn rows_source_agent(owner: &Value) -> String {
    owner["owner_agent_id"].as_str().unwrap_or("").to_string()
}
