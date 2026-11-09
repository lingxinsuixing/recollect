use rc_memory::keys::AttentionKind;
use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_store::Root;

fn seed_card(m: &rc_store::MemoryHandle, title: &str, now_ms: i64) {
    let scope = Scope { kind: ScopeKind::Project, key: "proj-diag".into() };
    put_card(
        m,
        &FactContent {
            kind: "decision".into(),
            title: title.into(),
            body: format!("diag body {title}"),
            scope,
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: "a".repeat(64), anchor_role: AnchorRole::Human }],
        now_ms,
    )
    .unwrap();
}

#[test]
fn the_index_layer_and_the_content_layer_are_reconciled() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed_card(root.memory(), "one", 1);
    rc_index::rebuild(root.memory(), 2).unwrap();
    let rec = rc_diag::doctor::reconcile(root.memory()).expect("both sides are readable");
    assert_eq!(rec.rows_indexed, 1);
    assert_eq!(rec.facts, 1);
    assert_eq!(rec.delta(), 0);
    assert!(rec.agrees());
}

#[test]
fn a_mismatch_reports_the_delta() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed_card(root.memory(), "one", 1);
    rc_index::rebuild(root.memory(), 2).unwrap();
    // 重建之后再加一行：内容层多一行，索引层的 marker 还停在 1。
    seed_card(root.memory(), "two", 3);
    let rec = rc_diag::doctor::reconcile(root.memory()).expect("both sides are readable");
    assert_eq!(rec.rows_indexed, 1);
    assert_eq!(rec.facts, 2);
    assert_eq!(rec.delta(), -1);
    assert!(!rec.agrees());
}

#[test]
fn attention_open_by_kind_separates_the_three_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let conn = root.memory().conn();
    for sql in [
        "INSERT INTO attention(kind, mem_table, mem_id, created_at_ms) \
         VALUES('contradiction','cards','c1',1)",
        "INSERT INTO attention(kind, mem_table, mem_id, created_at_ms, resolved_at_ms) \
         VALUES('contradiction','cards','c2',1,9)",
        "INSERT INTO attention(kind, mem_table, mem_id, created_at_ms) \
         VALUES('alias_ambiguity','cards','a1',1)",
        "INSERT INTO attention(kind, mem_table, mem_id, created_at_ms) \
         VALUES('duplicate_title','cards','d1',1)",
    ] {
        conn.execute(sql, []).unwrap();
    }
    assert_eq!(
        rc_diag::doctor::attention_open_by_kind(root.memory(), AttentionKind::Contradiction),
        1,
        "两条 contradiction 里只有一条未解决"
    );
    assert_eq!(
        rc_diag::doctor::attention_open_by_kind(root.memory(), AttentionKind::AliasAmbiguity),
        1
    );
    assert_eq!(
        rc_diag::doctor::attention_open_by_kind(root.memory(), AttentionKind::DuplicateTitle),
        1
    );
}

/// 生命周期累计那一格（旧口径的唯一输入）。S3a 起它**不再**参与告警判定，
/// 留着它是为了把"88 累计 ≠ 触顶"这条阴性测出来。
fn usage(count: i64) -> rc_ledger::counters::UsageRow {
    rc_ledger::counters::UsageRow {
        subject_kind: "llm_job".into(),
        subject_id: "extract".into(),
        metric: rc_llm::ALERT_METRIC.into(),
        count,
        last_at_ms: 1,
    }
}

/// 最近一次运行是否触顶（hub 的三个 pass 收尾写）。
fn stopped(v: i64) -> rc_ledger::counters::UsageRow {
    rc_ledger::counters::UsageRow {
        subject_kind: "llm_job".into(),
        subject_id: "extract".into(),
        metric: rc_ledger::counters::QUOTA_STOPPED_METRIC.into(),
        count: v,
        last_at_ms: 2,
    }
}

#[test]
fn quota_exhaustion_produces_one_alert() {
    // 正面：累计只有 12（远低于 40），但最近一次运行写了 stopped=1 ⇒ 恰一条告警。
    let rows = [usage(12), stopped(1)];
    assert_eq!(
        rc_diag::doctor::quota_exhausted_alerts(&rows),
        vec!["quota_exhausted"],
        "stopped=1 必须恰好一条告警"
    );
}

#[test]
fn a_healthy_quota_produces_no_alert() {
    // 阴性：88 次累计 + stopped=0 ⇒ 不告警（旧口径在这里恒告警）。
    assert!(
        rc_diag::doctor::quota_exhausted_alerts(&[usage(88), stopped(0)]).is_empty(),
        "88 累计不许代言触顶"
    );
    // 阴性：一行运行记录都没有（null）也不告警。
    assert!(
        rc_diag::doctor::quota_exhausted_alerts(&[usage(39)]).is_empty(),
        "没有运行记录时这一格是 null，不是告警"
    );
}
