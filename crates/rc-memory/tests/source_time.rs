//! I3 写侧：`put_batch_at` 的逐格源时刻落进 `as_of_ms` / `created_at_ms`；
//! 抽取事件的三锚列是 `NULL/NULL/'unknown'`。`put_batch`（不带时刻）仍回落写入钟，
//! 所以这一条同时钉住"新入口改了、旧入口没被偷偷改语义"。

use rc_memory::keys::{MemTable, Scope, ScopeKind};
use rc_memory::writers_x::{put_batch, put_batch_at, BatchCtx, WriteItem};
use rc_memory::{AnchorRole, Provenance};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

fn ctx() -> BatchCtx {
    BatchCtx { source_agent: "agent-time".into(), device_id: "dev-time".into() }
}

fn item(table: MemTable, title: &str, uid: &str) -> WriteItem {
    WriteItem {
        table, kind: "lesson".into(), title: title.into(), body: "合成正文".into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-time".into() },
        valid_to_ms: None, aliases: vec![],
        prov: vec![Provenance { turn_uid: uid.into(), anchor_role: AnchorRole::Human }],
    }
}

#[test]
fn per_item_source_time_lands_in_as_of_and_created_at() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    // 旧三候选表都落 card：用三个不同标题区分，逐格断 as_of/created_at。
    let items = vec![item(MemTable::Card, "源时刻卡", &"a".repeat(64)),
                     item(MemTable::Card, "源时刻事件", &"b".repeat(64)),
                     item(MemTable::Card, "源时刻断言", &"c".repeat(64))];
    // 三格源时刻互不相同 ⇒ 逐格对齐错了会当场读出来（不是三条都写同一个数）。
    let times = [Some(1_000i64), Some(2_000), Some(3_000)];
    put_batch_at(m, &items, &ctx(), NOW, &times).unwrap();

    let get = |title: &str| -> (i64, i64) {
        m.conn().query_row(
            "SELECT as_of_ms, created_at_ms FROM card WHERE title=?1", [title],
            |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
    };
    assert_eq!(get("源时刻卡"), (1_000, 1_000), "第一行的两列都取源时刻 1000");
    assert_eq!(get("源时刻事件"), (2_000, 2_000), "第二行的两列都取源时刻 2000");
    assert_eq!(get("源时刻断言"), (3_000, 3_000), "第三行的两列都取源时刻 3000");

    let (anchor_ms, anchor_id, conf): (Option<i64>, Option<String>, Option<String>) =
        m.conn().query_row("SELECT time_anchor_ms, time_anchor_id, time_confidence
                            FROM card WHERE title='源时刻事件'", [],
                           |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!((anchor_ms, anchor_id), (None, None), "抽取事件没有可重算的锚");
    assert_eq!(conf.as_deref(), Some("unknown"), "无锚写 unknown，不拿 as_of 冒充锚");
}

#[test]
fn missing_item_time_falls_back_to_the_write_clock_only_for_the_legacy_entry() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    // 对齐切片里缺一格 ⇒ 那一格回落 now_ms；另一格仍用源时刻。
    put_batch_at(m, &[item(MemTable::Card, "有源时刻", &"d".repeat(64)),
                      item(MemTable::Card, "无源时刻", &"e".repeat(64))],
                 &ctx(), NOW, &[Some(42)]).unwrap();
    let at = |title: &str| -> i64 {
        m.conn().query_row("SELECT as_of_ms FROM card WHERE title=?1", [title], |r| r.get(0)).unwrap()
    };
    assert_eq!(at("有源时刻"), 42);
    assert_eq!(at("无源时刻"), NOW, "缺格回落写入钟（旧调用者语义）");
    // 旧入口 `put_batch` 的语义没变：没有源时刻 ⇒ 写入钟。
    put_batch(m, &[item(MemTable::Card, "旧入口", &"f".repeat(64))], &ctx(), NOW).unwrap();
    assert_eq!(at("旧入口"), NOW);
}
