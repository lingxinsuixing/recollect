//! A1 评测器：把一份双跑计划对着 gold 算指标。
//!
//! 这些指标是 Hermes prompt loop 那套口径在 A1 上的映射：gold 命中/拆分/幻影/膨胀，
//! 加上 A1 特有的别名、kind 与 stable 覆盖。所有匹配都是确定性的，不调模型。

use rc_distill::a1_eval::{
    evaluate, evaluate_with_turns, GoldEntity, GoldFile, GoldMention, GoldTurn, PlanAnchor,
    PlanIssue, PlanMention, PlanUnresolved, PlanView, TurnCtx,
};
use rc_memory::writers_v4::{A1Anchor, A1Batch, A1Mention};
use rc_memory::{AnchorRole, Provenance};

fn anchor(reference: &str, kind: &str, canonical: &str, aliases: &[&str], stable: bool) -> PlanAnchor {
    PlanAnchor {
        session_key: String::new(),
        reference: reference.into(),
        kind: kind.into(),
        canonical: canonical.into(),
        aliases: aliases.iter().map(|s| s.to_string()).collect(),
        reality_status: "unknown".into(),
        evidence: vec!["a".repeat(64)],
        stable,
    }
}

fn gold(entities: Vec<GoldEntity>, forbidden: &[&str]) -> GoldFile {
    GoldFile {
        entities,
        forbidden: forbidden.iter().map(|s| s.to_string()).collect(),
        allowed_extra: Vec::new(),
        turns: Vec::new(),
    }
}

fn entity(id: &str, kind: &str, tokens: &[&str], aliases: &[&str], core: bool) -> GoldEntity {
    GoldEntity {
        id: id.into(),
        kind: kind.into(),
        tokens: tokens.iter().map(|s| s.to_string()).collect(),
        aliases: aliases.iter().map(|s| s.to_string()).collect(),
        core,
        mode: None,
        sessions: Vec::new(),
    }
}

fn view(anchors: Vec<PlanAnchor>) -> PlanView {
    PlanView {
        anchors,
        mentions: vec![PlanMention {
            session_key: String::new(),
            anchor_ref: "a1".into(),
            surface: "她".into(),
            turn_uid: "b".repeat(64),
            speaker_ref: None,
            discourse_role: "mentioned".into(),
            resolution: "resolved".into(),
            confidence: 0.9,
        }],
        issues: vec![PlanIssue {
            kind: "ambiguous_anchor".into(),
            refs: vec!["a1".into()],
            reason: "x".into(),
        }],
        unresolved: vec![PlanUnresolved {
            surface: "那台设备".into(),
            turn_uid: "c".repeat(64),
            reason: "信息不足".into(),
        }],
    }
}

#[test]
fn plan_view_round_trips_from_the_writer_batch() {
    let batch = A1Batch {
        anchors: vec![A1Anchor {
            reference: "a1".into(),
            kind: "person".into(),
            canonical: "张三".into(),
            aliases: vec!["小张".into()],
            reality_status: "real".into(),
            evidence: vec![Provenance {
                turn_uid: "a".repeat(64),
                anchor_role: AnchorRole::Human,
            }],
            stable: true,
        }],
        mentions: vec![A1Mention {
            anchor_ref: "a1".into(),
            surface: "她".into(),
            turn_uid: "b".repeat(64),
            speaker_ref: None,
            discourse_role: "mentioned".into(),
            resolution: "resolved".into(),
            confidence: 0.9,
        }],
        issues: Vec::new(),
        unresolved: Vec::new(),
    };
    let view = PlanView::from_batch(&batch);
    assert_eq!(view.anchors[0].evidence, vec!["a".repeat(64)]);
    assert!(view.anchors[0].stable);
    let text = serde_json::to_string(&view).unwrap();
    let back: PlanView = serde_json::from_str(&text).unwrap();
    assert_eq!(back, view, "计划要能落盘再由评测器读回");
}

#[test]
fn recall_split_phantom_forbidden_and_extras_are_all_reported() {
    let plan = view(vec![
        anchor("a1", "person", "张三", &["小张"], true),
        anchor("a2", "person", "张三姐", &["张三"], true),
        anchor("a3", "object", "ThinkPad", &["老电脑"], true),
        anchor("a4", "person", "某老板", &[], true),
        anchor("a5", "object", "纸箱", &[], true),
    ]);
    let gold = gold(
        vec![
            entity("person-demo", "person", &["张三"], &["小张"], true),
            entity("computer", "object", &["老电脑"], &["ThinkPad"], true),
        ],
        &["某老板"],
    );
    let source = "张三说老电脑要重装；纸箱在门口。";
    let report = evaluate(&plan, &gold, source);

    assert_eq!((report.core_recall.hit, report.core_recall.total), (2, 2));
    assert_eq!((report.core_stable.hit, report.core_stable.total), (2, 2));
    assert_eq!(report.splits.len(), 1, "张三被两个锚认领：{:?}", report.splits);
    assert_eq!(report.splits[0].id, "person-demo");
    assert_eq!(report.phantom, vec!["某老板"], "只有原文里完全不出现的名字才是幻影");
    assert_eq!(report.forbidden_hits, vec!["某老板"]);
    assert_eq!(report.extras, vec!["纸箱"], "没进 gold、也不是悬空锚的额外锚");
    assert_eq!(report.kind_accuracy.hit, 2, "两个命中锚的 kind 都对");
    assert_eq!(report.alias_recall.hit, report.alias_recall.total, "小张/ThinkPad 都被别名称呼");
    assert_eq!(report.mentions, 1);
    assert_eq!(report.unresolved, 1);
    assert_eq!(report.issues, 1);
}

#[test]
fn kind_mismatch_is_visible_in_the_ratio() {
    let plan = view(vec![anchor("a1", "concept", "老电脑", &[], true)]);
    let gold = gold(vec![entity("computer", "object", &["老电脑"], &[], true)], &[]);
    let report = evaluate(&plan, &gold, "老电脑重装完了。");
    assert_eq!((report.core_recall.hit, report.core_recall.total), (1, 1));
    assert_eq!((report.kind_accuracy.hit, report.kind_accuracy.total), (0, 1));
}

#[test]
fn all_mode_needs_every_token_so_two_liwei_do_not_collide() {
    let mut liwei_test = entity("liwei_test", "person", &["李伟", "测试组"], &[], true);
    liwei_test.mode = Some("all".into());
    let mut liwei_classmate = entity("liwei_classmate", "person", &["李伟", "大学同学"], &[], true);
    liwei_classmate.mode = Some("all".into());
    let gold = gold(vec![liwei_test, liwei_classmate], &[]);

    let plan = view(vec![
        anchor("a1", "person", "李伟（测试组）", &["测试组的李伟"], true),
        anchor("a2", "person", "李伟", &[], true),
    ]);
    let report = evaluate(&plan, &gold, "测试组的李伟说大学同学李伟要结婚。");
    assert_eq!((report.core_recall.hit, report.core_recall.total), (1, 2), "{report:#?}");
    assert_eq!(report.splits.len(), 0);
}

#[test]
fn an_invented_canonical_is_grounded_when_an_alias_is_in_the_source() {
    let plan = view(vec![anchor("a1", "person", "李伟（测试组）", &["测试组的李伟"], true)]);
    let report = evaluate(&plan, &gold(vec![], &[]), "测试组的李伟下周出差。");
    assert!(report.phantom.is_empty(), "别名的原文出现就够了：{:?}", report.phantom);
}

fn turn(session: &str, index: usize, uid: &str, body: &str) -> TurnCtx {
    TurnCtx {
        session_key: session.into(),
        turn_index: index,
        turn_uid: uid.into(),
        body: body.into(),
        role: "human".into(),
    }
}

fn gold_mention(surface: &str, entity: &str, speaker: Option<&str>, role: &str) -> GoldMention {
    GoldMention {
        surface: surface.into(),
        entity: entity.into(),
        speaker: speaker.map(str::to_string),
        role: role.into(),
    }
}

#[test]
fn mention_and_unresolved_are_measured_against_turn_gold() {
    let turns = vec![
        turn("s1", 0, "u1", "张三说她下周搬家，那台设备先不搬。"),
        turn("s1", 1, "u2", "好。"),
    ];
    let mut plan = view(vec![anchor("a1", "person", "张三", &[], true)]);
    plan.mentions = vec![PlanMention {
        session_key: "s1".into(),
        anchor_ref: "a1".into(),
        surface: "张三".into(),
        turn_uid: "u1".into(),
        speaker_ref: Some("a1".into()),
        discourse_role: "speaker".into(),
        resolution: "resolved".into(),
        confidence: 0.9,
    }];
    plan.unresolved = vec![PlanUnresolved {
        surface: "那台设备".into(),
        turn_uid: "u1".into(),
        reason: "信息不足".into(),
    }];
    let mut gold = gold(vec![entity("person-demo", "person", &["张三"], &[], true)], &[]);
    gold.turns = vec![
        GoldTurn {
            session_key: "s1".into(),
            turn_index: 0,
            mentions: vec![gold_mention("张三", "person-demo", Some("person-demo"), "speaker")],
            unresolved: vec!["那台设备".into()],
            no_anchor: Vec::new(),
        },
        GoldTurn {
            session_key: "s1".into(),
            turn_index: 1,
            mentions: Vec::new(),
            unresolved: Vec::new(),
            no_anchor: Vec::new(),
        },
    ];
    let report = evaluate_with_turns(&plan, &gold, &turns);
    assert_eq!((report.mention_precision.hit, report.mention_precision.total), (1, 1));
    assert_eq!((report.mention_recall.hit, report.mention_recall.total), (1, 1));
    assert_eq!((report.unresolved_precision.hit, report.unresolved_precision.total), (1, 1));
    assert_eq!((report.unresolved_recall.hit, report.unresolved_recall.total), (1, 1));
    assert_eq!((report.speaker_accuracy.hit, report.speaker_accuracy.total), (1, 1));
    assert_eq!((report.role_accuracy.hit, report.role_accuracy.total), (1, 1));
    assert_eq!(report.gold_turns, 2);
}

#[test]
fn duplicate_mentions_count_as_false_positives() {
    let turns = vec![turn("s1", 0, "u1", "张三来了。")];
    let mut plan = view(vec![anchor("a1", "person", "张三", &[], true)]);
    let mention = PlanMention {
        session_key: "s1".into(),
        anchor_ref: "a1".into(),
        surface: "张三".into(),
        turn_uid: "u1".into(),
        speaker_ref: None,
        discourse_role: "mentioned".into(),
        resolution: "resolved".into(),
        confidence: 0.9,
    };
    plan.mentions = vec![mention.clone(), mention];
    let mut gold = gold(vec![entity("person-demo", "person", &["张三"], &[], true)], &[]);
    gold.turns = vec![GoldTurn {
        session_key: "s1".into(),
        turn_index: 0,
        mentions: vec![gold_mention("张三", "person-demo", None, "mentioned")],
        unresolved: Vec::new(),
        no_anchor: Vec::new(),
    }];
    let report = evaluate_with_turns(&plan, &gold, &turns);
    assert_eq!((report.mention_precision.hit, report.mention_precision.total), (1, 2));
    assert_eq!((report.mention_recall.hit, report.mention_recall.total), (1, 1));
}

#[test]
fn structure_gate_reports_boundaries_and_bad_evidence() {
    let turns = vec![
        turn("s1", 0, "u1", "10 月 29 日之前给她一张表。"),
        turn("s1", 1, "u2", "[[compact_summary]]\nKey Decisions: 赵敏负责搬家时间表"),
    ];
    let mut plan = view(vec![
        anchor("a1", "concept", "10 月 29 日", &["他"], true),
        PlanAnchor {
            session_key: String::new(),
            reference: "a2".into(),
            kind: "person".into(),
            canonical: "张三".into(),
            aliases: vec!["小张".into()],
            reality_status: "real".into(),
            evidence: vec!["u2".into(), "not-in-batch".into()],
            stable: true,
        },
        PlanAnchor {
            session_key: String::new(),
            reference: "a3".into(),
            kind: "object".into(),
            canonical: "纸箱".into(),
            aliases: Vec::new(),
            reality_status: "unknown".into(),
            evidence: Vec::new(),
            stable: false,
        },
    ]);
    plan.mentions = Vec::new();
    plan.unresolved = Vec::new();
    plan.anchors[0].evidence = vec!["u1".into()];
    let mut gold = gold(vec![entity("person-demo", "person", &["张三"], &[], true)], &[]);
    gold.turns = vec![
        GoldTurn {
            session_key: "s1".into(),
            turn_index: 0,
            mentions: Vec::new(),
            unresolved: Vec::new(),
            no_anchor: vec!["10 月 29 日".into()],
        },
        GoldTurn {
            session_key: "s1".into(),
            turn_index: 1,
            mentions: Vec::new(),
            unresolved: Vec::new(),
            no_anchor: Vec::new(),
        },
    ];
    let report = evaluate_with_turns(&plan, &gold, &turns);
    assert_eq!(report.boundary_hits, vec!["10 月 29 日"]);
    assert_eq!(report.pronoun_names, vec!["10 月 29 日"]);
    assert_eq!(report.evidence_out_of_batch, vec!["not-in-batch"]);
    assert_eq!(report.anchors_without_evidence, vec!["纸箱"]);
    assert_eq!(report.summary_evidence, vec!["u2"]);
    assert_eq!(report.alias_not_in_source, vec!["他", "小张"]);
}

#[test]
fn session_scoped_gold_keeps_same_alias_in_two_sessions_apart() {
    let mut a1 = anchor("a1", "object", "打印机", &[], true);
    a1.session_key = "s1".into();
    let mut a2 = anchor("a2", "object", "打印机", &[], true);
    a2.session_key = "s2".into();
    let plan = view(vec![a1, a2]);
    let mut p1 = entity("printer_s1", "object", &["打印机"], &[], true);
    p1.sessions = vec!["s1".into()];
    let mut p2 = entity("printer_s2", "object", &["打印机"], &[], true);
    p2.sessions = vec!["s2".into()];
    let report = evaluate(&plan, &gold(vec![p1, p2], &[]), "打印机在 s1 和 s2 各有一台。");
    assert_eq!((report.core_recall.hit, report.core_recall.total), (2, 2));
    assert!(report.splits.is_empty(), "跨 session 同名不是拆分: {:?}", report.splits);
}

#[test]
fn alias_verbatim_check_is_case_insensitive_like_the_source_normalizer() {
    let plan = view(vec![anchor("a1", "object", "群晖 DS220+", &["NAS"], true)]);
    let report = evaluate(&plan, &gold(vec![], &[]), "nas 连不上。");
    assert!(report.alias_not_in_source.is_empty(), "NAS/nas 是同一表面: {:?}", report.alias_not_in_source);
}

#[test]
fn unresolved_that_gold_marks_no_anchor_is_neutral_not_false_positive() {
    let turns = vec![turn("s1", 0, "u1", "第二场再改。")];
    let mut plan = view(vec![]);
    plan.unresolved = vec![PlanUnresolved {
        surface: "第二场".into(),
        turn_uid: "u1".into(),
        reason: "场次不是实体".into(),
    }];
    let mut gold = gold(vec![], &[]);
    gold.turns = vec![GoldTurn {
        session_key: "s1".into(),
        turn_index: 0,
        mentions: Vec::new(),
        unresolved: Vec::new(),
        no_anchor: vec!["第二场".into()],
    }];
    let report = evaluate_with_turns(&plan, &gold, &turns);
    assert_eq!(report.unresolved_scored, 0);
    assert_eq!(report.unresolved_precision.total, 0);
    assert_eq!(report.unresolved_precision.value(), 1.0);
}

#[test]
fn unresolved_for_a_gold_entity_is_a_real_false_positive() {
    let turns = vec![turn("s1", 0, "u1", "张三来了。")];
    let mut plan = view(vec![]);
    plan.unresolved = vec![PlanUnresolved {
        surface: "张三".into(),
        turn_uid: "u1".into(),
        reason: "不该 unresolved".into(),
    }];
    let mut gold = gold(vec![entity("person-demo", "person", &["张三"], &[], true)], &[]);
    gold.turns = vec![GoldTurn {
        session_key: "s1".into(),
        turn_index: 0,
        mentions: Vec::new(),
        unresolved: Vec::new(),
        no_anchor: Vec::new(),
    }];
    let report = evaluate_with_turns(&plan, &gold, &turns);
    assert_eq!(report.unresolved_scored, 1);
    assert_eq!((report.unresolved_precision.hit, report.unresolved_precision.total), (0, 1));
}
