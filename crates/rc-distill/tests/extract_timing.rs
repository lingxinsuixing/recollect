//! I3：抽取写入的 `as_of_ms` / `created_at_ms` 取**源 turn 的 created_at_ms**，
//! 不取导入钟；事件没有可重算的锚 ⇒ 三锚列 `NULL/NULL/'unknown'`，不拿导入时间冒充。
//!
//! 夹具：两条 turn 的 created_at = 1000 / 1001，`run_extract` 的 `now_ms` = 9_999_999。
//! 断言读的是库里的行——只看返回值会让"写没写这一列"没有读者。

mod common;

use rc_distill::extract::{run_extract, select_batch, MAX_TURNS_PER_BATCH};
use rc_llm::mock::MockBackend;
use rc_store::Root;

#[test]
fn as_of_and_created_at_come_from_the_source_turn_not_the_import_clock() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = common::seed(&mut root, 2, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH)
        .unwrap().expect("两条 turn 必须选得出来");
    let payload = serde_json::json!({
        "entities": [{"kind":"concept","title":"源时刻实体","aliases":[],
                      "provenance":[uids[0]]}],
        "cards": [{"kind":"lesson","title":"源时刻卡","body":"合成正文一",
                   "valid_to_ms":null,"provenance":[uids[0]]}],
        "events": [{"kind":"milestone","title":"源时刻事件","body":"合成正文二",
                    "valid_to_ms":null,"provenance":[uids[1]]}],
        "assertions": [{"kind":"state","title":"取最晚源轮","body":"合成正文三",
                        "valid_to_ms":null,"provenance":[uids[0], uids[1]]}],
        "worthless_turn_uids": []
    }).to_string();
    let backend = MockBackend::scripted(vec![Ok(payload)]);
    // 导入钟比源轮晚 9_998_999 ms —— 旧形状会把三列都写成这个数。
    run_extract(root.memory(), root.ledger(), &plan, &backend, 9_999_999).unwrap();

    let card: (i64, i64) = root.memory().conn().query_row(
        "SELECT as_of_ms, created_at_ms FROM card WHERE title='源时刻卡'", [],
        |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(card, (1_000, 1_000), "card 的 as_of/created_at 必须取源轮 1000，不是导入钟");

    let event: (i64, i64, Option<i64>, Option<String>, Option<String>) = root.memory().conn()
        .query_row("SELECT as_of_ms, created_at_ms, time_anchor_ms, time_anchor_id,
                           time_confidence FROM card WHERE title='源时刻事件'", [],
                   |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).unwrap();
    assert_eq!((event.0, event.1), (1_001, 1_001),
        "event 的 as_of/created_at 取源轮 1001");
    assert_eq!((event.2, event.3), (None, None), "抽取事件没有可重算的锚");
    assert_eq!(event.4.as_deref(), Some("unknown"),
        "事件无锚必须显式写 unknown，而不是留 NULL 让读侧猜");

    let assertion: i64 = root.memory().conn().query_row(
        "SELECT as_of_ms FROM card WHERE title='取最晚源轮'", [],
        |r| r.get(0)).unwrap();
    assert_eq!(assertion, 1_001, "多源轮取最晚的那条（批准轮，不是提议轮）");

    let entity: i64 = root.memory().conn().query_row(
        "SELECT created_at_ms FROM anchor WHERE canonical='源时刻实体'", [],
        |r| r.get(0)).unwrap();
    assert_eq!(entity, 1_000, "实体唯一的 created_at 同样取源轮");
}
