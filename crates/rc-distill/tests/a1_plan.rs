//! A1 双跑合并：stable 取交集，锚/提及/问题取并集后重映射到程序 ref。
//!
//! 这里只测"合并成什么形状"，不碰数据库；落库行为由 rc-memory 的 a1_writer 用例守。

use rc_distill::batch::{Batch, TurnRow};
use rc_distill::contract_v4::{plan_a1, AnchorDraft, A1Output, IssueDraft, MentionDraft,
                              UnresolvedSurface};
use rc_memory::writers_v4::A1Batch;
use rc_memory::AnchorRole;

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
            TurnRow { turn_uid: uid('1'), role: "human".into(), body: String::new(), created_at_ms: 1, ingested_at_ms: 1, speaker: None, speaker_anchor_id: None },
            TurnRow { turn_uid: uid('2'), role: "agent".into(), body: String::new(), created_at_ms: 2, ingested_at_ms: 2, speaker: None, speaker_anchor_id: None },
        ],
    }
}

fn anchor(reference: &str, kind: &str, canonical: &str, aliases: &[&str], evidence: char) -> AnchorDraft {
    AnchorDraft {
        reference: reference.into(),
        kind: kind.into(),
        canonical: canonical.into(),
        aliases: aliases.iter().map(|s| s.to_string()).collect(),
        reality_status: "real".into(),
        evidence: vec![uid(evidence)],
        reason: String::new(),
    }
}

fn mention(anchor_ref: &str, surface: &str, turn: char, speaker: Option<&str>) -> MentionDraft {
    MentionDraft {
        anchor_ref: anchor_ref.into(),
        surface: surface.into(),
        turn_uid: uid(turn),
        speaker_ref: speaker.map(str::to_string),
        discourse_role: "mentioned".into(),
        resolution: "resolved".into(),
        confidence: 0.8,
    }
}

fn output(anchors: Vec<AnchorDraft>, mentions: Vec<MentionDraft>) -> A1Output {
    A1Output { anchors, mentions, issues: Vec::new(), unresolved: Vec::new() }
}

fn by_canonical<'a>(plan: &'a A1Batch, canonical: &str) -> &'a rc_memory::writers_v4::A1Anchor {
    plan.anchors.iter().find(|a| a.canonical == canonical).unwrap()
}

#[test]
fn stable_is_the_intersection_and_union_is_written_as_provisional() {
    let run1 = output(
        vec![
            anchor("a1", "person", "张三", &["小张"], '1'),
            anchor("a2", "object", "老电脑", &[], '2'),
        ],
        vec![mention("a1", "她", '1', None)],
    );
    let run2 = output(
        vec![
            anchor("b1", "person", "  张三 ", &[], '1'),
            anchor("b2", "object", "打印机", &[], '2'),
        ],
        vec![mention("b1", "她", '1', None)],
    );

    let plan = plan_a1(&run1, &run2, &batch());
    assert_eq!(plan.anchors.len(), 3, "两次的锚取并集");
    assert!(by_canonical(&plan, "张三").stable, "两次都出现（归一后同名）⇒ stable");
    assert!(!by_canonical(&plan, "老电脑").stable, "只出现一次 ⇒ provisional");
    assert!(!by_canonical(&plan, "打印机").stable);
    let person_demo = by_canonical(&plan, "张三");
    assert_eq!(person_demo.aliases, vec!["小张"], "别名取并集而不是丢掉第二次的空表");
    assert_eq!(plan.mentions.len(), 1, "同一轮同一表面名同一锚 ⇒ 提及去重");
}

#[test]
fn refs_are_program_assigned_and_swap_invariant() {
    let run1 = output(
        vec![
            anchor("a1", "person", "张三", &[], '1'),
            anchor("a2", "object", "老电脑", &[], '2'),
        ],
        vec![mention("a2", "它", '2', Some("a1"))],
    );
    let run2 = output(
        vec![
            anchor("b1", "object", "老电脑", &[], '2'),
            anchor("b2", "person", "张三", &[], '1'),
        ],
        vec![mention("b1", "它", '2', Some("b2"))],
    );

    let a = plan_a1(&run1, &run2, &batch());
    let b = plan_a1(&run2, &run1, &batch());
    let refs = |p: &A1Batch| -> Vec<String> { p.anchors.iter().map(|x| x.reference.clone()).collect() };
    assert_eq!(refs(&a), vec!["a1", "a2"], "程序按稳定顺序发号，不沿用模型 ref");
    assert_eq!(refs(&a), refs(&b), "两次运行的顺序不同也得到同一份计划");
    let m = &a.mentions[0];
    assert_eq!(a.anchors.iter().find(|x| x.reference == *m.anchor_ref).unwrap().canonical, "老电脑");
    let speaker = m.speaker_ref.as_ref().expect("说话人要被重映射");
    assert_eq!(a.anchors.iter().find(|x| x.reference == *speaker).unwrap().canonical, "张三");
}

#[test]
fn evidence_is_deduplicated_and_carries_the_turn_role() {
    let run1 = output(vec![anchor("a1", "person", "张三", &[], '1')], Vec::new());
    let mut run2 = output(vec![anchor("b1", "person", "张三", &[], '1')], Vec::new());
    run2.anchors[0].evidence.push(uid('2'));

    let plan = plan_a1(&run1, &run2, &batch());
    let ev: Vec<(&str, AnchorRole)> = plan.anchors[0]
        .evidence
        .iter()
        .map(|p| (p.turn_uid.as_str(), p.anchor_role))
        .collect();
    assert_eq!(
        ev,
        vec![(uid('1').as_str(), AnchorRole::Human), (uid('2').as_str(), AnchorRole::Agent)],
        "两次的 evidence 取并集，角色从批次行取，模型递不进"
    );
}

#[test]
fn naming_drift_across_runs_is_linked_by_alias_instead_of_leaving_two_anchors() {
    let run1 = output(
        vec![
            anchor("a1", "object", "老电脑", &[], '2'),
            anchor("a2", "object", "打印机", &[], '1'),
        ],
        Vec::new(),
    );
    let run2 = output(
        vec![
            anchor("b1", "object", "ThinkPad", &["老电脑"], '2'),
            anchor("b2", "object", "惠普 M126a", &["打印机"], '1'),
        ],
        Vec::new(),
    );

    let plan = plan_a1(&run1, &run2, &batch());
    assert_eq!(plan.anchors.len(), 2, "同一实体换了个 canonical 仍是一个锚");
    let computer = by_canonical(&plan, "ThinkPad");
    assert!(computer.stable, "两次都认出了这台电脑，只是名字不同");
    assert!(computer.aliases.contains(&"老电脑".to_string()));
    let printer = by_canonical(&plan, "惠普 M126a");
    assert!(printer.stable);
    assert!(printer.aliases.contains(&"打印机".to_string()));
}

#[test]
fn an_explicit_ambiguity_issue_blocks_the_alias_link() {
    let mut run1 = output(
        vec![
            anchor("a1", "person", "张三", &[], '1'),
            anchor("a2", "person", "张三的同事", &["张三"], '2'),
        ],
        Vec::new(),
    );
    run1.issues = vec![IssueDraft {
        kind: "ambiguous_anchor".into(),
        refs: vec!["a1".into(), "a2".into()],
        reason: "同名不同人".into(),
    }];
    let mut run2 = output(
        vec![
            anchor("b1", "person", "张三", &[], '1'),
            anchor("b2", "person", "张三的同事", &["张三"], '2'),
        ],
        Vec::new(),
    );
    run2.issues = run1.issues.clone();

    let plan = plan_a1(&run1, &run2, &batch());
    assert_eq!(plan.anchors.len(), 2, "模型明说同名不同人时，alias 链接不许把它们并成一个");
}

#[test]
fn issues_and_unresolved_are_remapped_and_deduplicated() {
    let mut run1 = output(vec![anchor("a1", "person", "张三", &[], '1')], Vec::new());
    run1.issues = vec![
        IssueDraft { kind: "ambiguous_anchor".into(), refs: vec!["a1".into()], reason: "同名".into() },
        IssueDraft { kind: "other".into(), refs: vec!["a1".into()], reason: "x".into() },
    ];
    run1.unresolved = vec![UnresolvedSurface {
        surface: "那台打印机".into(),
        turn_uid: uid('2'),
        reason: "信息不足".into(),
    }];

    let mut run2 = output(vec![anchor("b1", "person", "张三", &[], '1')], Vec::new());
    run2.issues = vec![
        IssueDraft { kind: "ambiguous_anchor".into(), refs: vec!["b1".into()], reason: "同名".into() },
        IssueDraft { kind: "other".into(), refs: vec!["b1".into()], reason: "x".into() },
    ];
    run2.unresolved = run1.unresolved.clone();

    let plan = plan_a1(&run1, &run2, &batch());
    assert_eq!(plan.issues.len(), 2, "ref 重映射后两次的同一条问题合成一条");
    assert_eq!(plan.unresolved.len(), 1);
    assert_eq!(plan.issues[0].refs, vec!["a1"], "issue 里的模型 ref 换成程序 ref");
}

#[test]
fn planned_refs_are_the_same_refs_the_writer_resolves() {
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    let m = root.memory();
    let run1 = output(
        vec![
            anchor("a1", "person", "张三", &["小张"], '1'),
            anchor("a2", "object", "老电脑", &[], '2'),
        ],
        vec![mention("a1", "她", '1', None)],
    );
    let run2 = output(
        vec![
            anchor("b1", "person", "张三", &[], '1'),
            anchor("b2", "object", "打印机", &[], '2'),
        ],
        Vec::new(),
    );
    let plan = plan_a1(&run1, &run2, &batch());
    let out = rc_memory::write_a1(
        m,
        &plan,
        &rc_memory::BatchCtx { source_agent: "rc-distill".into(), device_id: "d".into() },
        &rc_memory::Scope { kind: rc_memory::ScopeKind::Person, key: "u1".into() },
        "run-1",
        1_700_000_000_000,
    )
    .unwrap();

    assert_eq!(out.by_ref.len(), 3, "每个程序 ref 都能解析到句柄");
    assert_eq!(out.mentions, 1);
    let status = |canonical: &str| -> String {
        m.conn()
            .query_row("SELECT status FROM anchor WHERE canonical=?1", [canonical], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(status("张三"), "active");
    assert_eq!(status("老电脑"), "provisional");
    assert_eq!(status("打印机"), "provisional");
}

#[test]
fn duplicate_unresolved_of_same_surface_and_turn_is_one_row() {
    let run1 = A1Output {
        anchors: Vec::new(),
        mentions: Vec::new(),
        issues: Vec::new(),
        unresolved: vec![UnresolvedSurface {
            surface: "那台设备".into(),
            turn_uid: uid('1'),
            reason: "信息不足".into(),
        }],
    };
    let run2 = A1Output {
        anchors: Vec::new(),
        mentions: Vec::new(),
        issues: Vec::new(),
        unresolved: vec![UnresolvedSurface {
            surface: "那台设备".into(),
            turn_uid: uid('1'),
            reason: "无法归属".into(),
        }],
    };
    let plan = plan_a1(&run1, &run2, &batch());
    assert_eq!(plan.unresolved.len(), 1, "同轮同 surface 的 unresolved 不许因 reason 不同变成两条");
}

#[test]
fn same_canonical_across_kinds_is_one_anchor() {
    let run1 = output(
        vec![
            anchor("a1", "event", "季度复盘会", &[], '1'),
            anchor("a2", "work", "季度复盘会", &[], '1'),
        ],
        Vec::new(),
    );
    let run2 = output(
        vec![
            anchor("b1", "event", "季度复盘会", &["复盘会"], '1'),
            anchor("b2", "work", "季度复盘会", &[], '1'),
        ],
        Vec::new(),
    );
    let plan = plan_a1(&run1, &run2, &batch());
    assert_eq!(plan.anchors.len(), 1, "同一个 canonical 跨 kind 不许拆成两条: {plan:#?}");
    assert_eq!(plan.anchors[0].canonical, "季度复盘会");
}

#[test]
fn nested_mentions_of_one_anchor_and_turn_keep_only_the_longest() {
    let run1 = output(
        vec![anchor("a1", "object", "老电脑", &["ThinkPad"], '1')],
        vec![
            mention("a1", "2019 年的 ThinkPad", '1', None),
            mention("a1", "ThinkPad", '1', None),
        ],
    );
    let run2 = output(
        vec![anchor("b1", "object", "老电脑", &["ThinkPad"], '1')],
        vec![
            mention("b1", "2019 年的 ThinkPad", '1', None),
            mention("b1", "ThinkPad", '1', None),
        ],
    );
    let plan = plan_a1(&run1, &run2, &batch());
    assert_eq!(plan.mentions.len(), 1, "嵌套 surface 只留最长: {plan:#?}");
    assert_eq!(plan.mentions[0].surface, "2019 年的 ThinkPad");
}

#[test]
fn pronoun_aliases_and_pronoun_canonicals_never_reach_the_plan() {
    let run1 = output(
        vec![
            anchor("a1", "person", "苏晚", &["小晚", "她"], '1'),
            anchor("a2", "person", "她", &[], '1'),
        ],
        Vec::new(),
    );
    let run2 = output(
        vec![
            anchor("b1", "person", "苏晚", &["她"], '1'),
            anchor("b2", "person", "她", &[], '1'),
        ],
        Vec::new(),
    );
    let plan = plan_a1(&run1, &run2, &batch());
    assert_eq!(plan.anchors.len(), 1, "代词 canonical 的锚必须被丢弃: {plan:#?}");
    assert_eq!(plan.anchors[0].canonical, "苏晚");
    assert!(!plan.anchors[0].aliases.contains(&"她".to_string()), "代词别名必须剔除");
}
