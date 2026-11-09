//! v4 A1 写入口：锚 / 别名 / 轮级提及 / 问题。
//!
//! 可观察面五格：返回读数、四张表（anchor/alias/mention/provenance）的行、
//! attention 与 extraction_issue 的落点、以及重放后的行数不变。

use rc_memory::keys::{AnchorRole, Scope, ScopeKind};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_v4::{ensure_agent_anchor, write_a1, write_a1_with_owner, A1Anchor, A1Batch,
                            A1Issue, A1Mention, A1Unresolved};
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::{compute_mem_id, BatchCtx, MemError, MemTable, Provenance};
use rc_store::MemoryHandle;
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

fn uid(c: char) -> String {
    c.to_string().repeat(64)
}

fn scope() -> Scope {
    Scope { kind: ScopeKind::Person, key: "u1".into() }
}

fn ctx() -> BatchCtx {
    BatchCtx { source_agent: "rc-distill".into(), device_id: "pc-1".into() }
}

fn prov(c: char, role: AnchorRole) -> Provenance {
    Provenance { turn_uid: uid(c), anchor_role: role }
}

fn anchor(stable: bool) -> A1Anchor {
    A1Anchor {
        reference: "a1".into(),
        kind: "person".into(),
        canonical: "张三".into(),
        aliases: vec!["小张".into()],
        reality_status: "real".into(),
        evidence: vec![prov('a', AnchorRole::Human)],
        stable,
    }
}

/// 同一条 person 锚，但证据轮全是 agent 轮（W14d：这句人名是转述来的）。
fn anchor_with_agent_evidence() -> A1Anchor {
    let mut a = anchor(true);
    a.evidence = vec![prov('a', AnchorRole::Agent)];
    a
}

fn mention(anchor_ref: &str, speaker_ref: Option<&str>) -> A1Mention {
    A1Mention {
        anchor_ref: anchor_ref.into(),
        surface: "她".into(),
        turn_uid: uid('b'),
        speaker_ref: speaker_ref.map(str::to_string),
        discourse_role: "mentioned".into(),
        resolution: "resolved".into(),
        confidence: 0.9,
    }
}

fn batch(anchors: Vec<A1Anchor>, mentions: Vec<A1Mention>) -> A1Batch {
    A1Batch { anchors, mentions, issues: Vec::new(), unresolved: Vec::new() }
}

fn count(m: &MemoryHandle, table: &str) -> i64 {
    m.conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn a1_writes_anchor_alias_mention_and_provenance_with_program_columns() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    // 归属走合成默认 ⇒ 没有 principal。`person` 在身份未知时**不许自报 real**（P0-2）：
    // 模型写 `real` 也落成 `unknown`，读到这一格的人才知道"这条人名锚还没对上自然人"。
    let out = write_a1(
        m,
        &batch(vec![anchor(true)], vec![mention("a1", None)]),
        &ctx(),
        &scope(),
        "run-1",
        NOW,
    )
    .unwrap();

    assert_eq!((out.anchors, out.aliases, out.mentions), (1, 1, 1), "{out:?}");
    assert_eq!((out.stable, out.provisional), (1, 0), "{out:?}");
    assert_eq!(out.reality_downgrades, 1, "person + real + 身份未知 ⇒ 降一档：{out:?}");
    assert_eq!(
        (out.mentions_rebound, out.mentions_ambiguous, out.mentions_unmatched),
        (0, 0, 1),
        "「她」没有锚键 ⇒ 程序不命中，只计数，不改写模型的 resolution：{out:?}"
    );
    let anchor_id = out.by_ref["a1"].id.clone();

    let (canonical, reality, status, created, updated, norm, sk, skk): (
        String,
        String,
        String,
        i64,
        i64,
        i64,
        String,
        String,
    ) = m
        .conn()
        .query_row(
            "SELECT canonical, reality_status, status, created_at_ms, updated_at_ms,
                    norm_version, scope_kind, scope_key FROM anchor WHERE id=?1",
            [&anchor_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        (canonical.as_str(), reality.as_str(), status.as_str()),
        ("张三", "unknown", "active")
    );
    assert_eq!((created, updated), (NOW, NOW), "时间取程序传入的 now，不造导入钟之外的钟");
    assert_eq!(norm, rc_store::capability::NORM_VERSION as i64);
    assert_eq!((sk.as_str(), skk.as_str()), ("person", "u1"), "scope 由程序盖章");

    let alias: String = m
        .conn()
        .query_row("SELECT alias FROM alias WHERE anchor_id=?1", [&anchor_id], |r| r.get(0))
        .unwrap();
    assert_eq!(alias, "小张");

    let (turn, role): (String, String) = m
        .conn()
        .query_row(
            "SELECT turn_uid, anchor_role FROM provenance
             WHERE mem_table='anchor' AND mem_id=?1",
            [&anchor_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((turn.as_str(), role.as_str()), (uid('a').as_str(), "human"));

    let (surface, role, resolution, conf, speaker, agent, device, created): (
        String,
        String,
        String,
        f64,
        Option<String>,
        String,
        String,
        i64,
    ) = m
        .conn()
        .query_row(
            "SELECT surface, discourse_role, resolution, confidence, speaker_anchor_id,
                    source_agent, device_id, created_at_ms FROM mention WHERE anchor_id=?1",
            [&anchor_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            },
        )
        .unwrap();
    assert_eq!((surface.as_str(), role.as_str(), resolution.as_str()), ("她", "mentioned", "resolved"));
    assert_eq!(conf, 0.9);
    assert!(speaker.is_none(), "speaker_ref 省略 ⇒ speaker_anchor_id 为 NULL，不改写 resolution");
    assert_eq!((agent.as_str(), device.as_str()), ("rc-distill", "pc-1"));
    assert_eq!(created, NOW);
}

/// W14d 的新硬判据（用户裁定）：只有 agent 轮作证据的 person 锚照写——锚、别名、提及、
/// 出处、说话人一格不少——只把现实性落成未知，并留一行确定性审计（`from` 记模型自报值）。
#[test]
fn an_agent_reported_person_is_written_unknown_and_audited() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let mut a = anchor(true);
    a.evidence = vec![prov('a', AnchorRole::Agent)];

    let out = write_a1_with_owner(
        m,
        &batch(vec![a], vec![mention("a1", None)]),
        &ctx(),
        &session_owner(),
        &scope(),
        "run-1",
        NOW,
    )
    .unwrap();

    assert_eq!(out.person_agent_reported, 1, "只有 agent 轮作证据的 person 锚要计数：{out:?}");
    assert_eq!(out.reality_downgrades, 0, "身份已知：身份未知那一族不许再算一次：{out:?}");
    let (kind, reality): (String, String) = m
        .conn()
        .query_row(
            "SELECT kind, reality_status FROM anchor WHERE id=?1",
            [&out.by_ref["a1"].id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((kind.as_str(), reality.as_str()), ("person", "unknown"), "锚照写，现实性未知");
    assert_eq!(count(m, "anchor"), 1, "不丢锚");
    assert_eq!(count(m, "alias"), 1, "不丢别名");
    assert_eq!(count(m, "mention"), 1, "不丢提及");
    let role: String = m
        .conn()
        .query_row(
            "SELECT anchor_role FROM provenance WHERE mem_table='anchor' AND mem_id=?1",
            [&out.by_ref["a1"].id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(role, "agent", "出处照写，角色仍是 agent");

    let issue: (String, String) = m
        .conn()
        .query_row(
            "SELECT code, detail FROM extraction_issue",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(issue.0, "person_agent_reported", "只留这一族审计行");
    assert!(issue.1.contains("ref=a1"), "审计行要能指回批内短引用：{}", issue.1);
    assert!(issue.1.contains("evidence=agent_only"), "审计行要写明证据来源：{}", issue.1);
    assert!(issue.1.contains("from=real"), "from 记降级前的模型自报值：{}", issue.1);

    write_a1_with_owner(
        m,
        &batch(vec![anchor_with_agent_evidence()], vec![mention("a1", None)]),
        &ctx(),
        &session_owner(),
        &scope(),
        "run-1",
        NOW + 1,
    )
    .unwrap();
    assert_eq!(count(m, "extraction_issue"), 1, "同一 run 重放不追加审计行");
}

#[test]
fn a_person_named_by_a_human_keeps_real_and_is_not_counted() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    let out = write_a1_with_owner(
        m,
        &batch(vec![anchor(true)], vec![mention("a1", None)]),
        &ctx(),
        &session_owner(),
        &scope(),
        "run-1",
        NOW,
    )
    .unwrap();

    assert_eq!(out.person_agent_reported, 0, "有人开口作证据 ⇒ 不进转述那一族：{out:?}");
    assert_eq!(out.reality_downgrades, 0);
    let reality: String = m
        .conn()
        .query_row(
            "SELECT reality_status FROM anchor WHERE id=?1",
            [&out.by_ref["a1"].id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reality, "real", "人说的照旧");
    assert_eq!(count(m, "extraction_issue"), 0, "没有降级就没有账");
}

/// 身份未知与转述证据两族互斥：一个锚只算一次，转述那一族优先。
#[test]
fn agent_reported_and_unknown_principal_do_not_double_count() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    let out = write_a1(
        m,
        &batch(vec![anchor_with_agent_evidence()], Vec::new()),
        &ctx(),
        &scope(),
        "run-1",
        NOW,
    )
    .unwrap();

    assert_eq!(out.person_agent_reported, 1, "{out:?}");
    assert_eq!(out.reality_downgrades, 0, "同一个锚不许进两族：{out:?}");
    let codes: Vec<String> = m
        .conn()
        .prepare("SELECT code FROM extraction_issue ORDER BY code")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(codes, vec!["person_agent_reported"], "只留一条转述审计行");
}

#[test]
fn a1_replay_keeps_row_counts_and_timestamps_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let b = batch(vec![anchor(true)], vec![mention("a1", None)]);

    let first = write_a1(m, &b, &ctx(), &scope(), "run-1", NOW).unwrap();
    let (updated_before,): (i64,) = m
        .conn()
        .query_row("SELECT updated_at_ms FROM anchor WHERE id=?1", [&first.by_ref["a1"].id], |r| {
            Ok((r.get(0)?,))
        })
        .unwrap();
    let second = write_a1(m, &b, &ctx(), &scope(), "run-1", NOW + 60_000).unwrap();

    assert_eq!(second.anchors, 1);
    assert_eq!(second.aliases, 0, "读数按本次实际插入计，重放命中已有 ⇒ 0 新增");
    for table in ["anchor", "alias", "mention", "provenance"] {
        assert_eq!(count(m, table), 1, "{table} 重放长出了第二行");
    }
    let (updated_after,): (i64,) = m
        .conn()
        .query_row("SELECT updated_at_ms FROM anchor WHERE id=?1", [&first.by_ref["a1"].id], |r| {
            Ok((r.get(0)?,))
        })
        .unwrap();
    assert_eq!(updated_before, updated_after, "内容没变 ⇒ updated_at 不动");
}

#[test]
fn provisional_anchor_is_promoted_by_a_stable_run_and_never_demoted() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    let first = write_a1(
        m,
        &batch(vec![anchor(false)], Vec::new()),
        &ctx(),
        &scope(),
        "run-1",
        NOW,
    )
    .unwrap();
    let id = first.by_ref["a1"].id.clone();
    let status = |id: &str| -> String {
        m.conn()
            .query_row("SELECT status FROM anchor WHERE id=?1", [id], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(status(&id), "provisional", "只出现一次的锚入库但标 provisional");
    assert_eq!(first.provisional, 1);

    write_a1(m, &batch(vec![anchor(true)], Vec::new()), &ctx(), &scope(), "run-1", NOW + 1)
        .unwrap();
    assert_eq!(status(&id), "active", "双跑都出现 ⇒ 晋升为 active");

    write_a1(m, &batch(vec![anchor(false)], Vec::new()), &ctx(), &scope(), "run-1", NOW + 2)
        .unwrap();
    let (after, updated): (String, i64) = m
        .conn()
        .query_row("SELECT status, updated_at_ms FROM anchor WHERE id=?1", [&id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((after.as_str(), updated), ("active", NOW + 1), "已经 active 的锚不许被降级");
}

#[test]
fn alias_claimed_elsewhere_is_withheld_and_raises_alias_ambiguity_for_both() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let existing = put_entity(
        m,
        &EntityContent {
            kind: "person".into(),
            title: "张三的同事".into(),
            scope: scope(),
            aliases: vec!["小张".into()],
        },
        &[prov('c', AnchorRole::Human)],
        NOW,
    )
    .unwrap();

    let out = write_a1(
        m,
        &batch(vec![anchor(true)], Vec::new()),
        &ctx(),
        &scope(),
        "run-1",
        NOW,
    )
    .unwrap();
    let new_id = out.by_ref["a1"].id.clone();

    assert_eq!(out.aliases, 0, "冲突别名不落库");
    assert_eq!(out.aliases_withheld, 1, "{out:?}");
    let claimed: i64 = m
        .conn()
        .query_row(
            "SELECT count(*) FROM alias WHERE alias='小张' AND anchor_id=?1",
            [&new_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(claimed, 0, "歧义别名不硬挂到新锚");
    let flagged: Vec<String> = m
        .conn()
        .prepare(
            "SELECT mem_id FROM attention WHERE kind='alias_ambiguity' AND mem_table='anchor'
             AND resolved_at_ms IS NULL ORDER BY mem_id",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut want = vec![existing.id.clone(), new_id];
    want.sort();
    assert_eq!(flagged, want, "歧义要同时挂到两个认领方");
}

#[test]
fn issues_go_to_attention_or_extraction_issue_and_replay_does_not_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let mut b = batch(vec![anchor(true)], Vec::new());
    b.issues = vec![
        A1Issue { kind: "ambiguous_anchor".into(), refs: vec!["a1".into()], reason: "两个同名".into() },
        A1Issue { kind: "missing_subject".into(), refs: vec![], reason: "没有主语".into() },
    ];
    b.unresolved = vec![A1Unresolved {
        surface: "那台打印机".into(),
        turn_uid: uid('b'),
        reason: "信息不足".into(),
    }];

    let first = write_a1(m, &b, &ctx(), &scope(), "run-1", NOW).unwrap();
    assert_eq!(first.attention, 1, "{first:?}");
    assert_eq!(first.issues, 2, "每个 issue 都留一条审计行");
    assert_eq!(first.unresolved, 1);
    assert_eq!(first.reality_downgrades, 1, "person + real + 身份未知：降级也是一条账");

    let codes: Vec<String> = m
        .conn()
        .prepare("SELECT code FROM extraction_issue ORDER BY code")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        codes,
        vec![
            "ambiguous_anchor",
            "missing_subject",
            "person_reality_downgraded",
            "unresolved_surface",
        ]
    );
    let detail: String = m
        .conn()
        .query_row(
            "SELECT detail FROM extraction_issue WHERE code='unresolved_surface'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(detail.contains(&uid('b')), "审计行要能指回轮次：{detail}");

    write_a1(m, &b, &ctx(), &scope(), "run-1", NOW + 1).unwrap();
    assert_eq!(count(m, "extraction_issue"), 4, "同一 run 重放不追加审计行");
}

/// W14e（审计 P0-3）：六个形状类同批塞进悬空列表 ⇒ 真名桶 **0 行**，
/// 诊断桶各 1 行且 `shape=` 逐类正确，读数 `unresolved_shape_dropped == 7`。
/// 其中 `1234567` 是纯数字，标签必须是 number 而不是 hex（同为数字位的哈希才叫 hex）。
#[test]
fn shape_class_surfaces_go_to_the_diagnostic_bucket_never_the_name_bucket() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let shapes = [
        ("14d6e8ce", "hex"),
        ("D:\\recollect\\rust", "path"),
        ("https://example.invalid/build", "url"),
        ("123e4567-e89b-12d3-a456-426614174000", "uuid"),
        ("`cargo test`", "code"),
        ("1,234.5%", "number"),
        ("1234567", "number"),
    ];
    let mut b = batch(vec![anchor(true)], Vec::new());
    b.unresolved = shapes
        .iter()
        .map(|(surface, _)| A1Unresolved {
            surface: (*surface).into(),
            turn_uid: uid('b'),
            reason: "模型照旧条款塞了进来".into(),
        })
        .collect();

    let out = write_a1(m, &b, &ctx(), &scope(), "run-shape", NOW).unwrap();
    assert_eq!(out.unresolved, 0, "形状类不许进真名桶：{out:?}");
    assert_eq!(out.unresolved_shape_dropped, 7, "{out:?}");

    let codes: Vec<String> = m
        .conn()
        .prepare("SELECT code FROM extraction_issue ORDER BY code, detail")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        codes.iter().filter(|c| *c == "unresolved_surface").count(),
        0,
        "真名桶不许有形状类：{codes:?}"
    );
    assert_eq!(
        codes.iter().filter(|c| *c == "unresolved_surface_shape").count(),
        7,
        "{codes:?}"
    );

    let details: Vec<String> = m
        .conn()
        .prepare("SELECT detail FROM extraction_issue WHERE code='unresolved_surface_shape'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for (surface, shape) in shapes {
        assert!(
            details.iter().any(|d| d.contains(&format!("surface={surface};"))
                && d.ends_with(&format!("shape={shape}"))),
            "缺 {surface} ⇒ shape={shape} 的审计行：{details:?}"
        );
    }
}

/// W14e：真名不被形状闸误杀（宁可漏杀不可误杀），且 detail 尾巴带程序侧候选数。
/// `老张` 批内无锚 ⇒ `cand=0`；`测试组的李伟` 批内有同名锚 ⇒ `cand=1`；
/// `批次 M` 带数字但不是纯数字，仍留在真名桶。
#[test]
fn real_names_stay_in_the_name_bucket_and_carry_the_candidate_count() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let mut liwei = anchor(true);
    liwei.canonical = "测试组的李伟".into();
    liwei.aliases = vec!["李伟".into()];
    let mut b = batch(vec![liwei], Vec::new());
    b.unresolved = vec![
        A1Unresolved { surface: "老张".into(), turn_uid: uid('b'), reason: "批内没有这个落点".into() },
        A1Unresolved { surface: "测试组的李伟".into(), turn_uid: uid('c'), reason: "同名但没解析到".into() },
        A1Unresolved { surface: "批次 M".into(), turn_uid: uid('d'), reason: "泛称".into() },
    ];

    let out = write_a1(m, &b, &ctx(), &scope(), "run-name", NOW).unwrap();
    assert_eq!(out.unresolved, 3, "真名照旧落真名桶：{out:?}");
    assert_eq!(out.unresolved_shape_dropped, 0, "{out:?}");

    let details: Vec<String> = m
        .conn()
        .prepare("SELECT detail FROM extraction_issue WHERE code='unresolved_surface'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let row = |surface: &str| {
        details
            .iter()
            .find(|d| d.contains(&format!("surface={surface};")))
            .unwrap_or_else(|| panic!("缺 {surface} 的审计行：{details:?}"))
            .clone()
    };
    assert!(row("老张").ends_with("cand=0"), "{}", row("老张"));
    assert!(row("测试组的李伟").ends_with("cand=1"), "{}", row("测试组的李伟"));
    assert!(row("批次 M").ends_with("cand=0"), "{}", row("批次 M"));
}

#[test]
fn unknown_ref_rejects_the_whole_batch_without_partial_rows() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let b = batch(vec![anchor(true)], vec![mention("a9", None)]);

    let err = write_a1(m, &b, &ctx(), &scope(), "run-1", NOW).unwrap_err();
    assert!(matches!(err, MemError::UnknownAnchorRef), "{err}");
    for table in ["anchor", "alias", "mention", "provenance", "attention", "extraction_issue"] {
        assert_eq!(count(m, table), 0, "{table} 留下了半批内容");
    }
}

#[test]
fn pronoun_canonical_is_refused_by_the_alias_gate() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let mut a = anchor(true);
    a.canonical = "我".into();

    let err = write_a1(m, &batch(vec![a], Vec::new()), &ctx(), &scope(), "run-1", NOW).unwrap_err();
    assert!(matches!(err, MemError::AliasIsPronoun(_)), "{err}");
    assert_eq!(count(m, "anchor"), 0);
}

#[test]
fn speaker_ref_resolves_to_the_other_anchor_and_same_canonical_merges() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let a2 = A1Anchor {
        reference: "a2".into(),
        kind: "agent".into(),
        canonical: "Qoder".into(),
        aliases: Vec::new(),
        reality_status: "virtual".into(),
        evidence: vec![prov('b', AnchorRole::Agent)],
        stable: true,
    };
    let mut dup = anchor(true);
    dup.reference = "a3".into();
    dup.aliases = vec!["张三姐".into()];

    let out = write_a1(
        m,
        &batch(vec![anchor(true), a2, dup], vec![mention("a1", Some("a2"))]),
        &ctx(),
        &scope(),
        "run-1",
        NOW,
    )
    .unwrap();

    assert_eq!(out.anchors, 2, "两条同 kind+canonical 的草稿合成一个锚，另一个锚独立");
    assert_eq!(out.by_ref["a1"].id, out.by_ref["a3"].id);
    let speaker = out.by_ref["a2"].id.clone();
    let got: String = m
        .conn()
        .query_row(
            "SELECT speaker_anchor_id FROM mention WHERE anchor_id=?1",
            [&out.by_ref["a1"].id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(got, speaker);
    let aliases: Vec<String> = m
        .conn()
        .prepare("SELECT alias FROM alias WHERE anchor_id=?1 ORDER BY alias")
        .unwrap()
        .query_map([&out.by_ref["a1"].id], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(aliases, vec!["小张", "张三姐"], "合并后的别名取并集");
}

#[test]
fn intra_batch_alias_vs_canonical_conflict_is_order_independent() {
    let person_a = anchor(true);
    let mut other = anchor(true);
    other.reference = "a2".into();
    other.canonical = "小陈".into();
    other.aliases = vec!["张三".into()];
    other.evidence = vec![prov('b', AnchorRole::Human)];

    let run = |order: [A1Anchor; 2]| -> (usize, usize, i64) {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let m = root.memory();
        let out = write_a1(m, &batch(order.to_vec(), Vec::new()), &ctx(), &scope(), "run-1", NOW)
            .unwrap();
        let unclaimed: i64 = m
            .conn()
            .query_row(
                "SELECT count(*) FROM alias WHERE alias='张三' AND anchor_id<>(
                     SELECT id FROM anchor WHERE canonical='张三')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        (out.aliases_withheld, out.attention, unclaimed)
    };

    let forward = run([person_a.clone(), other.clone()]);
    let reverse = run([other, person_a]);
    assert_eq!(forward, reverse, "写入顺序不许改变歧义判定");
    assert_eq!((forward.0, forward.1, forward.2), (1, 2, 0), "{forward:?}");
}

#[test]
fn mentions_are_deduplicated_by_turn_anchor_surface() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let out = write_a1(
        m,
        &batch(
            vec![anchor(true)],
            vec![mention("a1", None), mention("a1", None)],
        ),
        &ctx(),
        &scope(),
        "run-1",
        NOW,
    )
    .unwrap();
    assert_eq!(out.mentions, 1);
    assert_eq!(count(m, "mention"), 1);
}

/// `resource_owner` 的一行（S2a-2 的断言面：锚行落库后紧跟的归属）。
struct OwnerSnapshot {
    owner_principal_id: Option<String>,
    owner_agent_id: String,
    created_by_agent: String,
    visibility: String,
    retention: String,
    scope_kind: String,
    scope_key: String,
    source_agent: Option<String>,
    device_id: Option<String>,
    created_at_ms: i64,
}

fn owner_of(m: &MemoryHandle, mem_id: &str) -> OwnerSnapshot {
    m.conn()
        .query_row(
            "SELECT owner_principal_id, owner_agent_id, created_by_agent, visibility, retention,
                    scope_kind, scope_key, source_agent, device_id, created_at_ms
             FROM resource_owner WHERE mem_table='anchor' AND mem_id=?1",
            [mem_id],
            |r| {
                Ok(OwnerSnapshot {
                    owner_principal_id: r.get(0)?,
                    owner_agent_id: r.get(1)?,
                    created_by_agent: r.get(2)?,
                    visibility: r.get(3)?,
                    retention: r.get(4)?,
                    scope_kind: r.get(5)?,
                    scope_key: r.get(6)?,
                    source_agent: r.get(7)?,
                    device_id: r.get(8)?,
                    created_at_ms: r.get(9)?,
                })
            },
        )
        .unwrap_or_else(|e| panic!("锚 {mem_id} 没有归属行：{e}"))
}

/// 旧入口（`write_a1`）的归属语义不变：合成默认 = owner 是署名 agent、private、default，
/// `created_by`/`source_agent`/`device_id` 取 `BatchCtx`，scope 取行自己的。
#[test]
fn write_a1_still_stamps_the_synthetic_owner_on_every_anchor() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    let out = write_a1(
        m,
        &batch(vec![anchor(true)], Vec::new()),
        &ctx(),
        &scope(),
        "run-1",
        NOW,
    )
    .unwrap();

    let row = owner_of(m, &out.by_ref["a1"].id);
    assert!(row.owner_principal_id.is_none(), "合成归属不登记自然人");
    assert_eq!(row.owner_agent_id, "rc-distill");
    assert_eq!(row.created_by_agent, "rc-distill");
    assert_eq!(row.visibility, "private_agent");
    assert_eq!(row.retention, "default");
    assert_eq!((row.scope_kind.as_str(), row.scope_key.as_str()), ("person", "u1"));
    assert_eq!(row.source_agent.as_deref(), Some("rc-distill"));
    assert_eq!(row.device_id.as_deref(), Some("pc-1"));
    assert_eq!(row.created_at_ms, NOW);
    assert_eq!(count(m, "resource_owner"), 1, "只给锚写归属，不为别名/提及各写一行");
}

/// 带 owner 的入口逐列落库，且 visibility 过行 scope 的天花板：
/// `project` 的会话声明落在 `person` 的行上 ⇒ 存 `person`（`owner::effective`）。
#[test]
fn write_a1_with_owner_writes_every_resource_owner_column() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let owner = OwnerCtx {
        owner_principal_id: Some("p1".into()),
        owner_agent_id: "alice".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    };

    let out = write_a1_with_owner(
        m,
        &batch(vec![anchor(true)], vec![mention("a1", None)]),
        &ctx(),
        &owner,
        &scope(),
        "run-1",
        NOW,
    )
    .unwrap();

    let row = owner_of(m, &out.by_ref["a1"].id);
    assert_eq!(row.owner_principal_id.as_deref(), Some("p1"));
    assert_eq!(row.owner_agent_id, "alice");
    assert_eq!(row.created_by_agent, "rc-distill", "created_by 是署名 agent，不是 owner");
    assert_eq!(row.visibility, "person", "min(project, ceiling(person)) = person");
    assert_eq!(row.retention, "keep");
    assert_eq!((row.scope_kind.as_str(), row.scope_key.as_str()), ("person", "u1"));
    assert_eq!(row.source_agent.as_deref(), Some("rc-distill"));
    assert_eq!(row.device_id.as_deref(), Some("pc-1"));
    assert_eq!(row.created_at_ms, NOW);
    assert_eq!(count(m, "resource_owner"), 1);
    // 反向保护（P0-2）：身份已知 ⇒ `person` 的 `real` 原样保留，程序不替模型降档。
    let (kind, reality): (String, String) = m
        .conn()
        .query_row(
            "SELECT kind, reality_status FROM anchor WHERE id=?1",
            [&out.by_ref["a1"].id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((kind.as_str(), reality.as_str()), ("person", "real"));
    assert_eq!(out.reality_downgrades, 0, "有身份时不该降：{out:?}");
}

/// 同 id 重写（重放 / 换归属再跑）不覆盖已有归属行：`owner::insert` 是 `INSERT OR IGNORE`。
/// 这一条是"先写者赢"的执法点——覆盖会把一条已有记忆的所属人与入库时刻静默改掉。
#[test]
fn rewriting_the_same_anchor_does_not_overwrite_its_owner() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let alice = OwnerCtx {
        owner_principal_id: Some("p1".into()),
        owner_agent_id: "alice".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    };
    let bob = OwnerCtx {
        owner_principal_id: Some("p2".into()),
        owner_agent_id: "bob".into(),
        visibility: Visibility::Public,
        retention: "days:7".into(),
    };
    let b = batch(vec![anchor(true)], Vec::new());

    let first = write_a1_with_owner(m, &b, &ctx(), &alice, &scope(), "run-1", NOW).unwrap();
    write_a1_with_owner(m, &b, &ctx(), &bob, &scope(), "run-1", NOW + 60_000).unwrap();

    assert_eq!(count(m, "resource_owner"), 1, "主键命中 ⇒ 不长第二行");
    let row = owner_of(m, &first.by_ref["a1"].id);
    assert_eq!(row.owner_principal_id.as_deref(), Some("p1"));
    assert_eq!(row.owner_agent_id, "alice");
    assert_eq!(row.visibility, "person");
    assert_eq!(row.retention, "keep");
    assert_eq!(row.created_at_ms, NOW, "已有归属的入库时刻不许被重写");
}

// ── S2e-7：会话 agent 自锚 ─────────────────────────────────────────────────────
//
// 锚是**程序产物**：id / scope / status / 时间都由程序盖章，模型只引用它的 `a*`。
// 这一组只断写入口那一层（行形状、归属、幂等、拒收），候选怎么接进 fact/event 在
// `rc-distill` 的用例里断。

/// 会话自报的主语与设备（`BatchCtx` 是 P 独占列，不来自模型）。
fn agent_ctx() -> BatchCtx {
    BatchCtx { source_agent: "qoder-cn".into(), device_id: "pc-9".into() }
}

fn project_scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-x".into() }
}

fn session_owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p1".into()),
        owner_agent_id: "alice".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}

/// 首次创建：id 是 `("agent", canonical, scope)` 的内容寻址；锚行是稳定锚形状
/// （active/real、profile 为空），归属行逐列取会话的声明。
#[test]
fn ensure_agent_anchor_writes_an_active_agent_anchor_and_its_owner() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    let key = ensure_agent_anchor(m, &agent_ctx(), &session_owner(), &project_scope(), NOW).unwrap();

    assert_eq!(key.table, MemTable::Anchor);
    assert_eq!(
        key.id,
        compute_mem_id(MemTable::Anchor, "agent", "qoder-cn", "", &project_scope()),
        "身份只有一条算法：同 canonical + scope 必落同一个 id"
    );

    let (kind, canonical, reality, status, profile, scope_kind, scope_key, created, updated, norm): (
        String,
        String,
        String,
        String,
        Option<String>,
        String,
        String,
        i64,
        i64,
        i64,
    ) = m
        .conn()
        .query_row(
            "SELECT kind, canonical, reality_status, status, profile_text, scope_kind, scope_key,
                    created_at_ms, updated_at_ms, norm_version FROM anchor WHERE id=?1",
            [&key.id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                ))
            },
        )
        .unwrap();
    assert_eq!((kind.as_str(), canonical.as_str()), ("agent", "qoder-cn"));
    assert_eq!((reality.as_str(), status.as_str()), ("real", "active"), "自锚是稳定锚");
    assert!(profile.is_none(), "自锚不写 profile");
    assert_eq!((scope_kind.as_str(), scope_key.as_str()), ("project", "proj-x"));
    assert_eq!((created, updated), (NOW, NOW));
    assert_eq!(norm, rc_store::capability::NORM_VERSION as i64);
    assert_eq!(count(m, "anchor"), 1);

    let row = owner_of(m, &key.id);
    assert_eq!(row.owner_principal_id.as_deref(), Some("p1"));
    assert_eq!(row.owner_agent_id, "alice", "owner 取会话的 effective 归属");
    assert_eq!(row.created_by_agent, "qoder-cn", "created_by 是会话的 source_agent");
    assert_eq!(row.visibility, "project", "min(project, ceiling(project)) = project");
    assert_eq!(row.retention, "keep");
    assert_eq!((row.scope_kind.as_str(), row.scope_key.as_str()), ("project", "proj-x"));
    assert_eq!(row.source_agent.as_deref(), Some("qoder-cn"));
    assert_eq!(row.device_id.as_deref(), Some("pc-9"));
    assert_eq!(row.created_at_ms, NOW);
}

/// 二次调用幂等：同 id、锚的内容/时间不刷新、归属先写者赢（换一个 owner 也改不动）。
#[test]
fn ensure_agent_anchor_is_idempotent_and_the_first_owner_wins() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let first = ensure_agent_anchor(m, &agent_ctx(), &session_owner(), &project_scope(), NOW)
        .unwrap();

    let bob = OwnerCtx {
        owner_principal_id: Some("p2".into()),
        owner_agent_id: "bob".into(),
        visibility: Visibility::Public,
        retention: "days:7".into(),
    };
    let again = ensure_agent_anchor(m, &agent_ctx(), &bob, &project_scope(), NOW + 60_000).unwrap();

    assert_eq!(first.id, again.id, "同 canonical + scope ⇒ 同一个锚");
    assert_eq!(count(m, "anchor"), 1, "幂等：不长第二行");
    assert_eq!(count(m, "resource_owner"), 1, "归属也只有一行");
    let (created, updated): (i64, i64) = m
        .conn()
        .query_row("SELECT created_at_ms, updated_at_ms FROM anchor WHERE id=?1", [&first.id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((created, updated), (NOW, NOW), "既有锚的内容与时间都不被覆盖");

    let row = owner_of(m, &first.id);
    assert_eq!(row.owner_principal_id.as_deref(), Some("p1"));
    assert_eq!(row.owner_agent_id, "alice");
    assert_eq!(row.visibility, "project");
    assert_eq!(row.retention, "keep");
    assert_eq!(row.created_at_ms, NOW, "已有归属的入库时刻不许被重写");
}

/// scope 是身份的一半：同一 canonical 在两个 scope 里是两个锚、两行归属。
#[test]
fn the_agent_anchor_identity_is_keyed_by_canonical_and_scope() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    let person = ensure_agent_anchor(m, &agent_ctx(), &session_owner(), &scope(), NOW).unwrap();
    let project =
        ensure_agent_anchor(m, &agent_ctx(), &session_owner(), &project_scope(), NOW).unwrap();

    assert_ne!(person.id, project.id, "不同 scope ⇒ 不同 id");
    assert_eq!(person.id, compute_mem_id(MemTable::Anchor, "agent", "qoder-cn", "", &scope()));
    assert_eq!(count(m, "anchor"), 2);
    assert_eq!(count(m, "resource_owner"), 2);
    let row = owner_of(m, &person.id);
    assert_eq!((row.scope_kind.as_str(), row.scope_key.as_str()), ("person", "u1"));
    assert_eq!(row.visibility, "person", "min(project, ceiling(person)) = person");
}

/// canonical 为空（会话主语缺失）⇒ `BadA1Field("source_agent")`，一行都不落。
/// 不猜一个人名兜底：主语不知道就宁可没有锚。
#[test]
fn ensure_agent_anchor_refuses_an_empty_source_agent() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    for raw in ["", "   ", "\n\t"] {
        let ctx = BatchCtx { source_agent: raw.into(), device_id: "pc-9".into() };
        let err = ensure_agent_anchor(m, &ctx, &session_owner(), &project_scope(), NOW)
            .expect_err("空主语不许造锚");
        assert!(matches!(err, MemError::BadA1Field("source_agent")), "{raw:?}: {err}");
    }
    assert_eq!(count(m, "anchor"), 0, "拒收不落锚");
    assert_eq!(count(m, "resource_owner"), 0, "拒收不落归属");
}

// ── P3-5：mention 的 surface → 锚，由程序用同一把尺子再判一次 ──────────────────
//
// 判据与 `writers_x` 的 card↔card 字面锚是**同一份** `anchor_keys`（整串 + 2~3 词元窗口，
// 归一后比）：不新写第三把。模型的 `anchor_ref` 只是候选，程序唯一命中时以程序为准。

/// 一个 `kind`/canonical/aliases 都由调用方给的锚草稿（同 scope、human 证据、stable）。
fn anchor_named(reference: &str, kind: &str, canonical: &str, aliases: &[&str]) -> A1Anchor {
    A1Anchor {
        reference: reference.into(),
        kind: kind.into(),
        canonical: canonical.into(),
        aliases: aliases.iter().map(|a| a.to_string()).collect(),
        reality_status: "real".into(),
        evidence: vec![prov('a', AnchorRole::Human)],
        stable: true,
    }
}

/// 唯一命中 ⇒ 采纳程序结果：模型把「小张」配给了项目锚，程序认出它是张三的别名 ⇒
/// 这一条提及落到张三上，`resolution` 仍是模型给的值（程序只判锚，不替模型判指代）。
#[test]
fn mention_surface_rebinds_to_the_anchor_the_program_resolves() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    let person = anchor_named("a1", "person", "张三", &["小张"]);
    let project = anchor_named("a2", "org", "项目甲", &[]);
    let mut tagged = mention("a2", None); // 模型挂错了锚
    tagged.surface = "小张".into();

    let out = write_a1(
        m,
        &batch(vec![person, project], vec![tagged]),
        &ctx(),
        &scope(),
        "run-1",
        NOW,
    )
    .unwrap();

    assert_eq!((out.mentions_rebound, out.mentions_ambiguous), (1, 0), "{out:?}");
    let (anchor_id, resolution): (String, String) = m
        .conn()
        .query_row("SELECT anchor_id, resolution FROM mention", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(anchor_id, out.by_ref["a1"].id, "「小张」是张三的别名，不是项目锚的");
    assert_eq!(resolution, "resolved", "唯一命中 ⇒ 模型这一列照旧");
    let detail: String = m
        .conn()
        .query_row(
            "SELECT detail FROM extraction_issue WHERE code='mention_resolution'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(detail.contains("rebound=1"), "纠正要留读数：{detail}");
}

/// 多命中 ⇒ 强制 `unresolved`：同一串名字能对上两个锚（这里是一对同名不同 kind），
/// 程序不替模型选，`resolution` 也不许说已解析。行的 `anchor_id` 仍取模型给的那一格。
#[test]
fn an_ambiguous_surface_is_forced_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    let person = anchor_named("a1", "person", "张三", &[]);
    let same_name_agent = anchor_named("a2", "agent", "张三", &[]);
    let mut tagged = mention("a1", None);
    tagged.surface = "张三".into();

    let out = write_a1(
        m,
        &batch(vec![person, same_name_agent], vec![tagged]),
        &ctx(),
        &scope(),
        "run-1",
        NOW,
    )
    .unwrap();

    assert_eq!((out.mentions_rebound, out.mentions_ambiguous), (0, 1), "{out:?}");
    let (anchor_id, resolution): (String, String) = m
        .conn()
        .query_row("SELECT anchor_id, resolution FROM mention", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(anchor_id, out.by_ref["a1"].id, "两锚都合法 ⇒ 行的锚仍取模型那一格");
    assert_eq!(resolution, "unresolved", "多命中不许声称已解析");
}

// ── W15b1：跨 kind 的同名合流（审计 P1-1 的第一刀，只止血不回溯）─────────────────
//
// 判据：草案的归一正名在同 scope 里已经有**另一个 kind** 的活锚 ⇒ 草案改从那一格的
// kind 与正名（单向：旧行赢），不再建第二格。已存在的重复对不重键、不合并、不删。

/// 直接落一格锚：写侧入口会先做跨 kind 合流，要预置"旧库里的另一格身份"就得绕开它。
fn seed_anchor(
    m: &MemoryHandle,
    kind: &str,
    canonical: &str,
    status: &str,
    created_at_ms: i64,
) -> String {
    let id = compute_mem_id(MemTable::Anchor, kind, canonical, "", &scope());
    m.conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind, scope_key,
                                status, profile_text, created_at_ms, updated_at_ms, norm_version)
             VALUES(?1,?2,?3,'real',NULL,?4,?5,?6,NULL,?7,?7,?8)",
            rusqlite::params![
                id,
                kind,
                canonical,
                scope().kind.as_str(),
                scope().key,
                status,
                created_at_ms,
                rc_store::capability::NORM_VERSION as i64
            ],
        )
        .unwrap();
    id
}

fn cross_kind_detail(m: &MemoryHandle) -> String {
    m.conn()
        .query_row(
            "SELECT detail FROM extraction_issue WHERE code='cross_kind_canonical'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

/// 用例 1：同一个正名先后被判成 work 与 agent ⇒ 只留 work 那一格；提及挂旧锚、
/// 出处累加到旧锚、审计行写明方向与候选数。
#[test]
fn a_cross_kind_duplicate_folds_into_the_anchor_that_already_exists() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    let first = write_a1(
        m,
        &batch(vec![anchor_named("s1", "work", "测试模型甲", &[])], Vec::new()),
        &ctx(),
        &scope(),
        "run-1",
        NOW,
    )
    .unwrap();
    let work_id = first.by_ref["s1"].id.clone();
    assert_eq!(first.cross_kind_coerced, 0, "库里还没有第二个 kind：{first:?}");

    let mut draft = anchor_named("s2", "agent", "测试模型甲", &[]);
    draft.evidence = vec![prov('c', AnchorRole::Agent)];
    let mut tagged = mention("s2", None);
    tagged.surface = "测试模型甲".into();

    let second = write_a1(m, &batch(vec![draft], vec![tagged]), &ctx(), &scope(), "run-2", NOW + 1)
        .unwrap();

    assert_eq!(second.cross_kind_coerced, 1, "{second:?}");
    assert_eq!(second.by_ref["s2"].id, work_id, "草案改从旧锚的 id");
    assert_eq!(count(m, "anchor"), 1, "不再建第二个锚");
    let (kind, canonical): (String, String) = m
        .conn()
        .query_row("SELECT kind, canonical FROM anchor", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!((kind.as_str(), canonical.as_str()), ("work", "测试模型甲"), "旧锚两列一个字不动");

    let mention_anchor: String = m
        .conn()
        .query_row("SELECT anchor_id FROM mention", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mention_anchor, work_id, "提及落在旧锚上");
    let provenance: i64 = m
        .conn()
        .query_row(
            "SELECT count(*) FROM provenance WHERE mem_table='anchor' AND mem_id=?1",
            [&work_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(provenance, 2, "两轮出处累加到同一格锚");
    assert_eq!(cross_kind_detail(m), "ref=s2; from=agent; to=work; candidates=1");
}

/// 用例 2：同一正名有多格活锚时，赢家是**较老**的那一格（状态打平 ⇒ 看 created_at_ms），
/// 候选数如实进审计行。草案的 kind 取第三种，才能同时对上两格候选。
#[test]
fn the_oldest_live_anchor_wins_when_several_kinds_share_the_name() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    let older = seed_anchor(m, "work", "测试模型乙", "active", NOW);
    let newer = seed_anchor(m, "agent", "测试模型乙", "active", NOW + 5_000);

    let out = write_a1(
        m,
        &batch(vec![anchor_named("s1", "object", "测试模型乙", &[])], Vec::new()),
        &ctx(),
        &scope(),
        "run-1",
        NOW + 10_000,
    )
    .unwrap();

    assert_eq!(out.cross_kind_coerced, 1, "{out:?}");
    assert_eq!(out.by_ref["s1"].id, older, "较早入库的那格赢（较新的是 {newer}）");
    assert_eq!(count(m, "anchor"), 2, "不建第三格");
    assert_eq!(cross_kind_detail(m), "ref=s1; from=object; to=work; candidates=2");
    let (kind, created): (String, i64) = m
        .conn()
        .query_row("SELECT kind, created_at_ms FROM anchor WHERE id=?1", [&newer], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((kind.as_str(), created), ("agent", NOW + 5_000), "落败的那格也不许被改");
}

/// 用例 3：同 canonical 同 kind ⇒ 走原路径，一行不多、合流读数为零。
#[test]
fn a_same_kind_repeat_does_not_go_through_the_fold() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    let first = write_a1(
        m,
        &batch(vec![anchor_named("s1", "work", "测试模型丙", &[])], Vec::new()),
        &ctx(),
        &scope(),
        "run-1",
        NOW,
    )
    .unwrap();
    let second = write_a1(
        m,
        &batch(vec![anchor_named("s1", "work", "测试模型丙", &[])], Vec::new()),
        &ctx(),
        &scope(),
        "run-2",
        NOW + 1,
    )
    .unwrap();

    assert_eq!(second.cross_kind_coerced, 0, "{second:?}");
    assert_eq!((first.anchors, second.anchors), (1, 1), "读数口径不变");
    assert_eq!(second.by_ref["s1"].id, first.by_ref["s1"].id);
    assert_eq!(count(m, "anchor"), 1, "同 kind 重放不长第二行");
    assert_eq!(count(m, "extraction_issue"), 0, "没有合流就没有审计行");
}

/// 用例 4：kind 不同但正名不同 ⇒ 两格照落（不许误并：同名才合流）。
#[test]
fn different_names_keep_their_own_anchors_across_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    write_a1(
        m,
        &batch(vec![anchor_named("s1", "work", "测试模型丁", &[])], Vec::new()),
        &ctx(),
        &scope(),
        "run-1",
        NOW,
    )
    .unwrap();
    let out = write_a1(
        m,
        &batch(vec![anchor_named("s1", "agent", "测试模型戊", &[])], Vec::new()),
        &ctx(),
        &scope(),
        "run-2",
        NOW + 1,
    )
    .unwrap();

    assert_eq!(out.cross_kind_coerced, 0, "{out:?}");
    assert_eq!(count(m, "anchor"), 2, "两个名字两格锚");
    assert_eq!(count(m, "extraction_issue"), 0, "没有合流就没有审计行");
}

/// 用例 5：旧锚已退场 ⇒ 不接新草案，照建新锚（退场的那格不许被复活或改写）。
#[test]
fn a_retired_anchor_does_not_absorb_a_new_draft() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    let first = write_a1(
        m,
        &batch(vec![anchor_named("s1", "work", "测试模型己", &[])], Vec::new()),
        &ctx(),
        &scope(),
        "run-1",
        NOW,
    )
    .unwrap();
    let old_id = first.by_ref["s1"].id.clone();
    rc_memory::retire(m, &first.by_ref["s1"], "手工退役", NOW + 1).unwrap();

    let out = write_a1(
        m,
        &batch(vec![anchor_named("s1", "agent", "测试模型己", &[])], Vec::new()),
        &ctx(),
        &scope(),
        "run-2",
        NOW + 2,
    )
    .unwrap();

    assert_eq!(out.cross_kind_coerced, 0, "退场的锚不接新草案：{out:?}");
    assert_eq!(count(m, "anchor"), 2, "草案照建新锚");
    let expected = compute_mem_id(MemTable::Anchor, "agent", "测试模型己", "", &scope());
    assert_eq!(out.by_ref["s1"].id, expected, "草案照建那一格 agent 锚");
    let (kind, status): (String, String) = m
        .conn()
        .query_row("SELECT kind, status FROM anchor WHERE id=?1", [&old_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((kind.as_str(), status.as_str()), ("work", "active"), "旧行一列不改（退场是另一本账）");
}
