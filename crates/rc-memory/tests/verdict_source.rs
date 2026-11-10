//! W15a：`passed` 卡的**背书来源**。两条写路（audit / review_apply）写同一列
//! `card.verdict`，谁盖的那一问只由 `card_verdict_event` 回答；本文件钉住三件事：
//! ① review 晋升留 `review_apply` 且 CAS 0 行不追加；② 不带来源的写入口一条事件都不写
//! （否则来源退化成噪音）；③ 读侧按**最新**事件分桶，没有事件的 passed 卡落 `passed_unbacked`。

use rc_memory::{issues, promote_verdict, put_card, set_verdict, set_verdict_from, AnchorRole,
                FactContent, MemError, Provenance, Scope, ScopeKind, Verdict,
                VERDICT_SOURCE_AUDIT};
use rc_store::Root;

fn seed(m: &rc_store::MemoryHandle, title: &str) -> rc_memory::MemKey {
    put_card(
        m,
        &FactContent {
            kind: "decision".into(),
            title: title.into(),
            body: format!("verdict source fixture {title}"),
            scope: Scope { kind: ScopeKind::Project, key: "proj-w15a".into() },
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: "a".repeat(64), anchor_role: AnchorRole::Human }],
        1,
    )
    .unwrap()
}

fn events(m: &rc_store::MemoryHandle, card_id: &str) -> Vec<(String, String)> {
    let mut st = m
        .conn()
        .prepare("SELECT verdict, source FROM card_verdict_event WHERE card_id=?1 ORDER BY id")
        .unwrap();
    st.query_map([card_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
}

fn counts(m: &rc_store::MemoryHandle) -> (i64, i64, i64) {
    issues::verdict_source_counts(m).unwrap()
}

#[test]
fn review_promotion_records_one_review_apply_event_and_a_lost_cas_records_none() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let k = seed(m, "review 提升那一刀");
    assert!(events(m, &k.id).is_empty(), "put_card 的出厂状态不该带事件");

    assert_eq!(promote_verdict(m, &k, 11).unwrap(), 1, "unreviewed → passed 必须动一行");
    assert_eq!(events(m, &k.id), vec![("passed".to_string(), "review_apply".to_string())],
               "review 晋升必须留一条 review_apply 事件");
    assert_eq!(counts(m), (0, 1, 0), "晋升的卡只落 review_apply 一桶");

    // 第二次：CAS 已是 passed ⇒ 0 行。那一次**没有发生任何背书**，记账就是伪造。
    assert_eq!(promote_verdict(m, &k, 12).unwrap(), 0, "重复晋升是 0 行这个读数");
    assert_eq!(events(m, &k.id).len(), 1, "CAS 0 行不许追加事件（事件数必须不变）");
    assert_eq!(counts(m), (0, 1, 0));
}

#[test]
fn a_plain_write_records_no_event_and_lands_in_passed_unbacked() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let k = seed(m, "存量 36 张同形的那一张");

    // 不带来源的写入口与 W15a 之前逐字同形：列变了，账上没有新行。
    set_verdict(m, &k, Verdict::Passed, 2).unwrap();
    assert!(events(m, &k.id).is_empty(), "普通 set_verdict 不许写事件（来源会变成噪音）");
    assert_eq!(counts(m), (0, 0, 1), "没有事件的 passed 卡就是 passed_unbacked");

    // 补一条 audit 事件之后同一张卡换桶：1/0/0（不补写存量，只在真的有新落笔时记）。
    set_verdict_from(m, &k, Verdict::Passed, VERDICT_SOURCE_AUDIT, 3).unwrap();
    assert_eq!(events(m, &k.id), vec![("passed".to_string(), "audit".to_string())]);
    assert_eq!(counts(m), (1, 0, 0), "补上 audit 背书之后不再算 source unknown");
}

#[test]
fn the_latest_event_decides_the_bucket() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let k = seed(m, "先降档后晋升的那一张");

    // ① audit 先判 low（事件 1：low/audit）。
    set_verdict_from(m, &k, Verdict::Low, VERDICT_SOURCE_AUDIT, 4).unwrap();
    // ② 回到 unreviewed（不带来源，不记账），再由 review 晋升（事件 2：passed/review_apply）。
    set_verdict(m, &k, Verdict::Unreviewed, 5).unwrap();
    assert_eq!(promote_verdict(m, &k, 6).unwrap(), 1);
    assert_eq!(
        events(m, &k.id),
        vec![
            ("low".to_string(), "audit".to_string()),
            ("passed".to_string(), "review_apply".to_string()),
        ],
        "历史必须两条都在（追加型，不覆盖）"
    );
    // 读侧认最新一条（max(id)）：只算 review_apply 一桶。
    assert_eq!(counts(m), (0, 1, 0), "latest event 必须是 review_apply，不是先头的 audit");
}

#[test]
fn the_event_source_is_a_closed_set_enforced_by_the_ddl() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let k = seed(m, "闭集的执法点");
    // 绕开 API 直插：DDL 的 CHECK 必须自己拒收闭集外的来源（不是只靠写侧守规矩）。
    assert!(m
        .conn()
        .execute(
            "INSERT INTO card_verdict_event(card_id, verdict, source, at_ms, norm_version)
             VALUES(?1,'passed','bogus',7,1)",
            rusqlite::params![k.id],
        )
        .is_err(),
        "source 的闭集没生效：闭集外的值被收下了");
}

#[test]
fn an_anchor_target_is_still_not_a_verdict_bearing_shape() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let anchor = rc_memory::MemKey { table: rc_memory::MemTable::Anchor, id: "x".repeat(64) };
    let e = set_verdict_from(m, &anchor, Verdict::Passed, VERDICT_SOURCE_AUDIT, 8).unwrap_err();
    assert!(matches!(e, MemError::NotFound(_)), "anchor 没有 verdict 列：是 NotFound，不是 Sql");
    let n: i64 = m
        .conn()
        .query_row("SELECT count(*) FROM card_verdict_event", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "写失败的那一次不许留下半条账（改列与记账同一事务）");
}
