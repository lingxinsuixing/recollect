//! A1 dev / holdout 夹具的形状闸。
//!
//! 只证明夹具能被契约解析、gold v2 逐轮覆盖、两套切分不串；不调模型，也不碰 holdout 的读数。

use rc_distill::a1_eval::{turns_from_exchange, GoldFile};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const DEV: &str = include_str!("../../../fixtures/a1/dev.json");
const DEV_GOLD: &str = include_str!("../../../fixtures/a1/dev_gold.json");
const HOLD: &str = include_str!("../../../fixtures/a1/holdout.json");
const HOLD_GOLD: &str = include_str!("../../../fixtures/a1/holdout_gold.json");

/// 两个 split 都允许复用的语域别名；它们靠实体 `sessions` 作用域消歧。
const ALLOWED_ALIAS_OVERLAP: [&str; 2] = ["NAS", "手册"];

fn accepted(body: &str) -> usize {
    let file = rc_contract::parse(body.as_bytes()).expect("exchange 契约");
    let report = rc_contract::normalize(file, 0);
    assert!(report.batch_reject.is_none(), "整批被拒：{:?}", report.batch_reject);
    report.accepted.len()
}

fn gold(body: &str) -> GoldFile {
    serde_json::from_str(body).expect("gold 形状")
}

fn exchange(body: &str) -> Value {
    serde_json::from_str(body).expect("exchange JSON")
}

fn session_keys(doc: &Value) -> BTreeSet<String> {
    doc.get("sessions")
        .and_then(Value::as_array)
        .expect("sessions")
        .iter()
        .filter_map(|s| s.get("session_key").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

fn source(doc: &Value) -> String {
    doc.get("sessions")
        .and_then(Value::as_array)
        .expect("sessions")
        .iter()
        .flat_map(|s| s.get("turns").and_then(Value::as_array).expect("turns"))
        .filter_map(|t| t.get("body").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn dev_and_holdout_are_valid_and_separate() {
    let dev_turns = accepted(DEV);
    let hold_turns = accepted(HOLD);
    assert!((100..=150).contains(&dev_turns), "dev turns={dev_turns}");
    assert!((100..=150).contains(&hold_turns), "holdout turns={hold_turns}");

    let dev = gold(DEV_GOLD);
    let hold = gold(HOLD_GOLD);
    assert_eq!(dev.turns.len(), dev_turns, "dev gold 必须逐轮覆盖");
    assert_eq!(hold.turns.len(), hold_turns, "holdout gold 必须逐轮覆盖");
    assert_eq!(turns_from_exchange(&exchange(DEV)).len(), dev_turns);
    assert_eq!(turns_from_exchange(&exchange(HOLD)).len(), hold_turns);

    let dev_sessions = session_keys(&exchange(DEV));
    let hold_sessions = session_keys(&exchange(HOLD));
    assert_eq!(dev_sessions.len(), 12);
    assert_eq!(hold_sessions.len(), 12);
    assert!(dev_sessions.is_disjoint(&hold_sessions), "session_key 不许跨 split");

    let dev_ids: BTreeSet<&str> = dev.entities.iter().map(|e| e.id.as_str()).collect();
    let hold_ids: BTreeSet<&str> = hold.entities.iter().map(|e| e.id.as_str()).collect();
    assert!(dev_ids.is_disjoint(&hold_ids), "dev 与 holdout 的实体 id 不许重叠");

    let dev_canon: BTreeSet<&str> = dev
        .entities
        .iter()
        .filter_map(|e| e.tokens.first().map(String::as_str))
        .collect();
    let hold_canon: BTreeSet<&str> = hold
        .entities
        .iter()
        .filter_map(|e| e.tokens.first().map(String::as_str))
        .collect();
    assert!(dev_canon.is_disjoint(&hold_canon), "canonical token 不许跨 split");

    let dev_alias: BTreeSet<&str> = dev.entities.iter().flat_map(|e| e.aliases.iter().map(String::as_str)).collect();
    let hold_alias: BTreeSet<&str> = hold.entities.iter().flat_map(|e| e.aliases.iter().map(String::as_str)).collect();
    let alias_overlap: Vec<&str> = dev_alias.intersection(&hold_alias).copied().collect();
    assert!(
        alias_overlap.iter().all(|a| ALLOWED_ALIAS_OVERLAP.contains(a)),
        "新的跨 split alias: {alias_overlap:?}"
    );
}

#[test]
fn gold_covers_every_turn_and_names_are_grounded() {
    for (ex, gold_body, split) in [(DEV, DEV_GOLD, "dev"), (HOLD, HOLD_GOLD, "holdout")] {
        let doc = exchange(ex);
        let g = gold(gold_body);
        let ctx = turns_from_exchange(&doc);
        let session_set = session_keys(&doc);
        let body_by_turn: BTreeMap<(String, usize), String> = ctx
            .iter()
            .map(|t| ((t.session_key.clone(), t.turn_index), t.body.clone()))
            .collect();
        let uids: BTreeSet<&str> = ctx.iter().map(|t| t.turn_uid.as_str()).collect();
        assert_eq!(uids.len(), ctx.len(), "{split}: turn_uid 重复");

        let ids: BTreeSet<&str> = g.entities.iter().map(|e| e.id.as_str()).collect();
        let mut implied_names: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for e in &g.entities {
            let names = implied_names.entry(e.id.as_str()).or_default();
            for n in e.tokens.iter().chain(e.aliases.iter()) {
                names.insert(n.as_str());
                assert!(
                    source(&doc).contains(n.as_str()),
                    "{split}/{}: 名字不在原文 {n:?}",
                    e.id
                );
            }
            assert!(!e.sessions.is_empty(), "{split}/{}: 缺 sessions", e.id);
            for s in &e.sessions {
                assert!(session_set.contains(s), "{split}/{}: 未知 session {s}", e.id);
            }
        }

        let mut seen = BTreeSet::new();
        let mut mentions_by_entity: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for gt in &g.turns {
            let key = (gt.session_key.clone(), gt.turn_index);
            assert!(seen.insert(key.clone()), "{split}: gold turn 重复 {key:?}");
            let body = body_by_turn.get(&key).unwrap_or_else(|| panic!("{split}: gold turn 不在 exchange {key:?}"));
            for m in &gt.mentions {
                assert!(ids.contains(m.entity.as_str()), "{split}: mention 未知实体 {}", m.entity);
                assert!(body.contains(&m.surface), "{split}: surface 不在原文 {:?}", m.surface);
                mentions_by_entity
                    .entry(m.entity.as_str())
                    .or_default()
                    .push(gt.turn_index);
            }
            for s in gt.unresolved.iter().chain(gt.no_anchor.iter()) {
                assert!(body.contains(s.as_str()), "{split}: 边界串不在原文 {s:?}");
            }
        }
        assert_eq!(seen.len(), ctx.len(), "{split}: gold turn 覆盖不完整");

        for e in &g.entities {
            let turns = mentions_by_entity.get(e.id.as_str()).cloned().unwrap_or_default();
            if e.core && !e.sessions.iter().any(|s| s.ends_with("-01")) {
                assert!(turns.len() >= 2, "{split}/{}: core mention <2", e.id);
                let distinct: BTreeSet<usize> = turns.iter().copied().collect();
                assert!(distinct.len() >= 2, "{split}/{}: core 只跨 1 轮", e.id);
            }
            // alias 只需逐字出现在原文；嵌套表面只记最大短语，不强制 alias 单独作 mention。
        }

        let names: BTreeSet<&str> = g
            .entities
            .iter()
            .flat_map(|e| e.tokens.iter().chain(e.aliases.iter()).map(String::as_str))
            .collect();
        for f in &g.forbidden {
            assert!(source(&doc).contains(f.as_str()), "{split}: forbidden 不在原文 {f:?}");
            assert!(!names.contains(f.as_str()), "{split}: forbidden 是实体名 {f:?}");
        }
        for a in &g.allowed_extra {
            assert!(!names.contains(a.as_str()), "{split}: allowed_extra 是实体名 {a:?}");
        }
    }
}
