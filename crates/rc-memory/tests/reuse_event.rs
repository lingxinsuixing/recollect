//! 波二刀 A（设计 `2026-11-08-g1-reuse-lesson-design.md` §波次二）：复用反馈写口/读口的钉。
//!
//! 钉四件事：
//! ① 闭集拒未知 outcome（**写口那一层**；工具面那一层在 `mcp/tests/reuse_feedback.rs`）；
//! ② append-only：同卡同 outcome 两次写 = 两行（绝不 upsert/覆盖）；
//! ③ 先认卡再写：悬空的卡一行都不写；
//! ④ 载体是 canonical 正路：开库即有表、`MEMORY_TABLES` 带它、幂等补表能自愈被 DROP 的表。
//!
//! 夹具全部合成（守卫 12）：通用占位名，没有语料专名、没有 CANARY、没有路径。

use rc_memory::reuse::{
    recent_events, record_reuse, reuse_counts, ReuseError, ReuseOutcome,
};
use rc_store::Root;

const NOW: i64 = 1_700_000_999_000;
const AGENT: &str = "agent-reuse";

fn open() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    (dir, root)
}

/// 铺一张卡（外键目标）。只写 reuse 需要的列——这里证的不是落卡链路。
fn seed_card(root: &Root, id: &str) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO card(id,kind,title,text,scope_kind,scope_key,as_of_ms,
                              source_agent,device_id,created_at_ms,norm_version)
             VALUES(?1,'observation','复用夹具','一句正文','project','proj-reuse',1,
                    'agent-reuse','dev-reuse',1,1)",
            rusqlite::params![id],
        )
        .unwrap();
}

fn event_rows(root: &Root) -> Vec<(i64, String, String, Option<String>, i64, String)> {
    let mut st = root
        .memory()
        .conn()
        .prepare(
            "SELECT id, card_id, outcome, note, at_ms, by_agent FROM card_reuse_event ORDER BY id",
        )
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

#[test]
fn 闭集词表与_slug_往返逐字对拍() {
    // 五档逐字（设计 §波次二第 1 条），且 slug/from_slug 是一对逆。
    assert_eq!(
        ReuseOutcome::ALL.map(|o| o.slug()),
        ["worked", "failed", "partial", "mismatch", "stale"]
    );
    for o in ReuseOutcome::ALL {
        assert_eq!(ReuseOutcome::from_slug(o.slug()), Some(o), "slug 往返漂了：{}", o.slug());
    }
    // 精确匹配：大小写、空格、近义词都不收（宽容读法会让 bad_outcome 测不到）。
    for bad in ["Worked", " worked", "worked ", "成功", "ok", "unknown", ""] {
        assert_eq!(ReuseOutcome::from_slug(bad), None, "闭集误收了 {bad:?}");
    }
}

#[test]
fn 未知_outcome_被写口拒掉且一行都不写() {
    let (_dir, root) = open();
    let m = root.memory();
    seed_card(&root, "c-1");
    match record_reuse(m, "c-1", "helped", Some("不在闭集里"), NOW, AGENT) {
        Err(ReuseError::UnknownOutcome) => {}
        other => panic!("未知 outcome 必须被拒为 UnknownOutcome，实际：{other:?}"),
    }
    assert!(event_rows(&root).is_empty(), "被拒的反馈在库里留下了行");
}

#[test]
fn 同卡同_outcome_两次写是两行() {
    let (_dir, root) = open();
    let m = root.memory();
    seed_card(&root, "c-1");
    let first = record_reuse(m, "c-1", "worked", None, NOW, AGENT).unwrap();
    let second = record_reuse(m, "c-1", "worked", None, NOW + 1, AGENT).unwrap();
    assert_ne!(first, second, "两次写必须拿到两个事件 id（append-only，不是 upsert）");
    let rows = event_rows(&root);
    assert_eq!(rows.len(), 2, "append-only：同卡同 outcome 两次 = 两行，实际 {rows:?}");
    assert_eq!(rows[0].2, "worked");
    assert_eq!(rows[1].2, "worked");
    assert_eq!(rows[1].4, NOW + 1, "后一条的钟必须是后一次写入的钟");
}

#[test]
fn 悬空的卡被拒且一行都不写() {
    let (_dir, root) = open();
    let m = root.memory();
    match record_reuse(m, "no-such-card", "worked", None, NOW, AGENT) {
        Err(ReuseError::Mem(rc_memory::MemError::NotFound(key))) => {
            assert_eq!(key.key_string(), "card:no-such-card", "拒法要指向被指认的那张卡");
        }
        other => panic!("悬空卡必须被拒为 NotFound，实际：{other:?}"),
    }
    assert!(event_rows(&root).is_empty(), "悬空反馈在库里留下了行");
}

#[test]
fn 读口按卡聚合_并给最近一条() {
    let (_dir, root) = open();
    let m = root.memory();
    seed_card(&root, "c-1");
    seed_card(&root, "c-2");
    record_reuse(m, "c-1", "worked", Some("第一次"), NOW, AGENT).unwrap();
    record_reuse(m, "c-1", "worked", None, NOW + 1, AGENT).unwrap();
    record_reuse(m, "c-1", "failed", Some("第三次"), NOW + 2, "agent-other").unwrap();
    record_reuse(m, "c-2", "stale", None, NOW + 3, AGENT).unwrap();

    let c1 = reuse_counts(m, "c-1").unwrap();
    assert_eq!(
        (c1.worked, c1.failed, c1.partial, c1.mismatch, c1.stale),
        (2, 1, 0, 0, 0),
        "c-1 的聚合读数漂了：{c1:?}"
    );
    assert_eq!(c1.total(), 3);
    assert_eq!(c1.of(ReuseOutcome::Worked), 2);
    let c2 = reuse_counts(m, "c-2").unwrap();
    assert_eq!(c2.of(ReuseOutcome::Stale), 1);
    // 没有反馈的卡是零读数，不是错误。
    assert_eq!(reuse_counts(m, "c-1-none").unwrap(), Default::default());

    let recent = recent_events(m, "c-1", 2).unwrap();
    assert_eq!(recent.len(), 2, "limit 生效：{recent:?}");
    assert_eq!(recent[0].outcome, "failed", "最近一条在前（id 降序）");
    assert_eq!(recent[1].outcome, "worked");
    assert_eq!(recent[1].note, None);
    assert_eq!(recent[0].by_agent, "agent-other", "谁报的要留得住");
}

#[test]
fn 表在开库即存在_且幂等补表能自愈被_drop_的表() {
    let (_dir, root) = open();
    let m = root.memory();
    // ① canonical 正路：`MEMORY_SQL` 重放列 ⇒ 开库即有。
    let exists: i64 = m
        .conn()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='card_reuse_event'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(exists, 1, "DDL 必须在 MEMORY_SQL 重放列里：开库即有 card_reuse_event");
    assert!(
        rc_memory::schema::MEMORY_TABLES.contains(&"card_reuse_event"),
        "card_reuse_event 必须进 canonical 清单（hub rows/restore/backup 才带着反馈走）"
    );
    // ② 写路径的幂等补表是双保险：表被 DROP 之后写一次就自愈（重放同一根 DDL 家）。
    seed_card(&root, "c-1");
    m.conn().execute_batch("DROP TABLE card_reuse_event").unwrap();
    let id = record_reuse(m, "c-1", "partial", None, NOW, AGENT).unwrap();
    assert!(id > 0);
    assert_eq!(event_rows(&root).len(), 1, "补表之后写入必须真的落库");
}
